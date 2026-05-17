//! PreparedQuery struct and related Engine methods.

use crate::ast::*;
use crate::plan::*;
use crate::planner;
use crate::result::{QueryError, QueryResult};
use powdb_storage::catalog::Catalog;
use powdb_storage::types::*;

use super::compiled::*;
use super::eval::*;
use super::Engine;

pub struct PreparedQuery {
    plan_template: PlanNode,
    /// Total number of `Expr::Literal` slots reachable from the plan.
    /// Callers must supply exactly this many literals per execution.
    pub param_count: usize,
    /// Fast-path metadata for `PlanNode::Insert`. `Some` when:
    ///   * the template is an Insert, and
    ///   * every assignment RHS is `Expr::Literal(_)` (no computed exprs),
    ///     which means param_count == assignments.len() and the caller's
    ///     literal slice maps 1:1 to schema column indices.
    ///
    /// Mission C Phase 15: upgraded from a bare `Vec<usize>` to a
    /// dedicated [`InsertFast`] struct so the execute path can skip the
    /// second `catalog.schema(table)` HashMap lookup just to read
    /// `n_cols`, and can dispatch through `get_table_mut` + `tbl.insert`
    /// instead of going via the generic `catalog.insert` wrapper.
    insert_fast: Option<InsertFast>,
    /// Mission C Phase 14: fast-path metadata for point updates by primary
    /// key — `T filter .pk = <lit> update { col := <lit> }` where `pk` is
    /// an indexed column and `col` is fixed-size and not indexed. At
    /// execute time we skip plan clone, substitute walk, schema re-lookup,
    /// `resolved_assignments` + `FastPatch` + `matching_rids` Vec allocs,
    /// and the whole `PlanNode::Update` arm. Just a btree lookup and a
    /// byte patch.
    update_pk_fast: Option<UpdatePkFast>,
}

/// Mission C Phase 15: precomputed insert fast-path metadata. Built once
/// in [`Engine::prepare`] from a `PlanNode::Insert` template whose every
/// assignment RHS is a raw literal. The execute path reads `n_cols` and
/// `col_indices` directly — no catalog schema lookup needed.
#[derive(Clone)]
struct InsertFast {
    /// Mission C Phase 18: cached slot index into `Catalog::tables`.
    /// Resolved once at `prepare` time and stable for the lifetime of
    /// the catalog (PowDB has no DROP TABLE). Lets the hot path dispatch
    /// through `catalog.table_by_slot_mut(slot)` — a pure Vec index,
    /// no hash, no bucket walk, no string compare.
    table_slot: usize,
    /// Schema column index for each positional literal, in the order the
    /// caller passes them.
    col_indices: Vec<usize>,
    /// Total number of schema columns — the size `insert_values_scratch`
    /// must be resized to before filling positions via `col_indices`.
    /// Cached here so the hot loop skips `catalog.schema(table)` entirely.
    n_cols: usize,
}

/// Mission C Phase 14: precomputed fast-path for `update_by_pk` shaped
/// prepared queries. Built once in [`Engine::prepare`] and reused on every
/// `execute_prepared` call.
#[derive(Clone)]
struct UpdatePkFast {
    /// Mission C Phase 18: cached slot index into `Catalog::tables`.
    /// Resolved once at `prepare` time and stable for the lifetime of
    /// the catalog. At a 52ns total budget the swap from FxHashMap
    /// probe to a Vec index is measurable.
    table_slot: usize,
    /// Name of the key column (the `.id = ?` side). We look this up in
    /// the owning table's `indexed_cols` at execute time rather than
    /// caching a raw `&BTree` — the engine owns the catalog and can't
    /// hand out long-lived borrows anyway, and the n≤5 linear scan is
    /// a handful of ns.
    key_col: String,
    /// Byte offset of the target fixed column in the row encoding:
    /// `2 + bitmap_size + layout.fixed_offsets[target_col]`.
    field_off: usize,
    /// Byte offset of the bitmap byte containing the target column's null
    /// bit (`2 + target_col / 8`).
    bitmap_byte_off: usize,
    /// Bit mask for the target column's null bit.
    bit_mask: u8,
    /// Type of the target fixed column — drives the literal-to-bytes
    /// encoding at execute time.
    target_type: TypeId,
    /// Index into the caller's `literals` slice that holds the filter key.
    /// Always 0 today (filter literal is visited before the assignment
    /// RHS), but stored explicitly so the contract is obvious.
    key_literal_idx: usize,
    /// Index into the caller's `literals` slice that holds the new value.
    value_literal_idx: usize,
}

impl Engine {
    pub fn prepare(&mut self, query: &str) -> Result<PreparedQuery, QueryError> {
        let plan = planner::plan(query).map_err(|e| QueryError::Parse(e.to_string()))?;
        let param_count = crate::plan_cache::count_literal_slots(&plan);

        // Insert fast path: if the template is Insert and every assignment
        // RHS is a literal, resolve column indices once here and store
        // them. execute_prepared will skip the plan-clone + substitute
        // walk on this path.
        //
        // Mission C Phase 15: also cache `n_cols` and the target table
        // name so execute_prepared doesn't need a second HashMap lookup
        // on `self.catalog.schema(table)` just to size the scratch Vec.
        let insert_fast = match &plan {
            PlanNode::Insert { table, assignments }
                if assignments
                    .iter()
                    .all(|a| matches!(a.value, Expr::Literal(_)))
                    && param_count == assignments.len() =>
            {
                let table_slot = self
                    .catalog
                    .table_slot(table)
                    .ok_or_else(|| QueryError::TableNotFound(table.clone()))?;
                let schema = &self.catalog.table_by_slot(table_slot).schema;
                let n_cols = schema.columns.len();
                let indices: Result<Vec<usize>, QueryError> = assignments
                    .iter()
                    .map(|a| {
                        schema
                            .column_index(&a.field)
                            .ok_or_else(|| QueryError::ColumnNotFound {
                                table: table.clone(),
                                column: a.field.clone(),
                            })
                    })
                    .collect();
                Some(InsertFast {
                    table_slot,
                    col_indices: indices?,
                    n_cols,
                })
            }
            _ => None,
        };

        // Mission C Phase 14: update-by-pk fast path. Match on the shape
        // planner::plan_update builds for `T filter .pk = ? update
        // { col := ? }` — `Update { input: IndexScan(pk), assignments:
        // [{col, Literal}] }` — and only if every precondition holds:
        //   * `pk` is an indexed column (so the executor would take the
        //     btree.lookup path at run time regardless)
        //   * there's exactly one assignment
        //   * the assigned column is fixed-size and *not* indexed (so we
        //     don't have to maintain any secondary index on write)
        //   * both literal slots are already `Expr::Literal` (no computed
        //     expressions)
        // If any of these fail we fall through to the standard substitute
        // + execute path.
        let update_pk_fast = Self::try_build_update_pk_fast(&self.catalog, &plan);

        Ok(PreparedQuery {
            plan_template: plan,
            param_count,
            insert_fast,
            update_pk_fast,
        })
    }

    /// Mission C Phase 14: inspect a planned tree and, if it matches the
    /// `update_by_pk` fast-path shape, return the precomputed byte-patch
    /// metadata. Returns `None` on any mismatch — the caller falls through
    /// to the substitute-and-execute path, which is always correct.
    fn try_build_update_pk_fast(catalog: &Catalog, plan: &PlanNode) -> Option<UpdatePkFast> {
        // Top level must be `Update { input: IndexScan(...), ... }`.
        let (table, input, assignments) = match plan {
            PlanNode::Update {
                table,
                input,
                assignments,
            } => (table, input.as_ref(), assignments),
            _ => return None,
        };
        // Exactly one assignment — the bench hot path and the only case
        // where a single byte-patch covers the whole mutation.
        if assignments.len() != 1 {
            return None;
        }
        let assn = &assignments[0];
        // Assignment RHS must be a raw literal, not a computed expr.
        if !matches!(assn.value, Expr::Literal(_)) {
            return None;
        }
        // Input must be an IndexScan on the same table with a literal key.
        let (key_col, key_table) = match input {
            PlanNode::IndexScan {
                table: t,
                column,
                key: Expr::Literal(_),
            } => (column.clone(), t.clone()),
            _ => return None,
        };
        if &key_table != table {
            return None;
        }

        // Look up schema + index state from the live catalog, caching
        // the slot so the execute path skips the name probe.
        let table_slot = catalog.table_slot(table)?;
        let tbl = catalog.table_by_slot(table_slot);
        let schema = &tbl.schema;

        // Key column must have an index (the btree.lookup path is what
        // makes the fast path worth building).
        if !tbl.has_index(&key_col) {
            return None;
        }

        // Target column must exist, be fixed-size, and NOT be indexed (so
        // we don't have to maintain any secondary index here).
        let target_col_idx = schema.column_index(&assn.field)?;
        let target_type = schema.columns[target_col_idx].type_id;
        if !is_fixed_size(target_type) {
            return None;
        }
        if tbl.has_indexed_col(target_col_idx) {
            return None;
        }

        // Precompute byte offsets from the cached row layout.
        let layout = tbl.row_layout();
        let fixed_off = layout.fixed_offset(target_col_idx)?;
        let bitmap_size = layout.bitmap_size();
        let field_off = 2 + bitmap_size + fixed_off;
        let bitmap_byte_off = 2 + target_col_idx / 8;
        let bit_mask = 1u8 << (target_col_idx % 8);

        // Literal walk order for `Update { IndexScan(key), [{value}] }`
        // (see `plan_cache::substitute_plan` — input first, then the
        // assignments). The filter key is literal 0, the assignment RHS
        // is literal 1.
        Some(UpdatePkFast {
            table_slot,
            key_col,
            field_off,
            bitmap_byte_off,
            bit_mask,
            target_type,
            key_literal_idx: 0,
            value_literal_idx: 1,
        })
    }

    /// Execute a [`PreparedQuery`] with the given literal values.
    ///
    /// The literals are substituted into a clone of the template plan in
    /// the same deterministic walk order that [`crate::canonicalize`]
    /// produces (filter predicate first, then projection, then assignment
    /// RHS, and so on). Substitution errors here mean the caller passed
    /// the wrong number of literals for this query shape.
    pub fn execute_prepared(
        &mut self,
        prep: &PreparedQuery,
        literals: &[Literal],
    ) -> Result<QueryResult, QueryError> {
        if literals.len() != prep.param_count {
            return Err(QueryError::Execution(format!(
                "prepared query expects {} literal(s), got {}",
                prep.param_count,
                literals.len(),
            )));
        }

        // Mission C Phase 14: update-by-pk fast path. Skip plan clone,
        // substitute walk, resolved_assignments, FastPatch, Vec<RowId>,
        // RowLayout::new — straight to btree.lookup_int + byte patch.
        // On rare mismatches (wrong literal type, index dropped after
        // prepare) the helper returns `Ok(None)` and we fall through to
        // the generic substitute-and-execute path below.
        if let Some(fast) = &prep.update_pk_fast {
            if let Some(result) = self.try_execute_update_pk_fast(fast, literals)? {
                // Mark dependent views dirty for prepared update fast path.
                if let PlanNode::Update { table, .. } = &prep.plan_template {
                    self.view_registry.mark_dependents_dirty(table);
                }
                // Mission B (post-review): statement-boundary WAL group
                // commit. The fast path appended an Update record but did
                // not flush — flush it now so the executor's contract is
                // "WAL is on disk before this returns".
                self.catalog
                    .sync_wal()
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                return Ok(result);
            }
        }

        // Insert fast path: skip plan-clone + substitute walk + PlanNode::Insert
        // arm's column-index resolution. Build the Row directly from the
        // caller's literal slice using indices we resolved at prepare time.
        // Saves ~300-500ns per insert on the bench.
        //
        // Mission C Phase 13: the scratch `Vec<Value>` is reused across
        // calls — no fresh allocation per insert. We split the borrow
        // between `self.catalog` and `self.insert_values_scratch` by
        // moving the scratch into a local, filling it, passing to the
        // catalog, and putting it back.
        //
        // Mission C Phase 15: the cached `InsertFast` carries `n_cols`
        // and the table name, so the hot path makes exactly one catalog
        // HashMap lookup (`get_table_mut`) and dispatches straight into
        // `tbl.insert` — no intermediate schema lookup, no generic
        // `Catalog::insert` wrapper.
        if let Some(fast) = &prep.insert_fast {
            let mut values = std::mem::take(&mut self.insert_values_scratch);
            values.clear();
            values.resize(fast.n_cols, Value::Empty);
            for (pos, lit) in literals.iter().enumerate() {
                values[fast.col_indices[pos]] = literal_value_from(lit);
            }
            // Mission C Phase 18: direct O(1) slot index — no
            // catalog hash probe. Slot was resolved at prepare time.
            let tbl = self.catalog.table_by_slot_mut(fast.table_slot);
            let res = tbl.insert(&values).map_err(|e| e.to_string());
            // Clear strings before returning the scratch — don't keep
            // dangling allocations from the previous row alive across
            // calls. `clear()` drops the Value::Str entries.
            values.clear();
            self.insert_values_scratch = values;
            res?;
            // Mark dependent views dirty for prepared insert fast path.
            if let PlanNode::Insert { table, .. } = &prep.plan_template {
                self.view_registry.mark_dependents_dirty(table);
            }
            // Mission B (post-review): statement-boundary WAL group commit.
            self.catalog
                .sync_wal()
                .map_err(|e| QueryError::StorageError(e.to_string()))?;
            return Ok(QueryResult::Modified(1));
        }

        let mut plan = prep.plan_template.clone();
        let mut idx = 0usize;
        crate::plan_cache::substitute_plan(&mut plan, literals, &mut idx);
        debug_assert_eq!(idx, literals.len());
        let result = self.execute_plan(&plan);
        // Mission B (post-review): statement-boundary WAL group commit.
        // No-op when nothing was buffered (read-only plans).
        self.catalog
            .sync_wal()
            .map_err(|e| QueryError::StorageError(e.to_string()))?;
        result
    }

    /// Mission C Phase 14: point-update fast path for prepared
    /// `T filter .pk = ? update { col := ? }` queries. The caller has
    /// already verified this is an int-indexed pk with a fixed-size,
    /// non-indexed target column; all we do here is pluck the two
    /// literals out of the caller's slice, run one `btree.lookup_int`,
    /// and patch 1–8 bytes of the row. No plan clone, no allocations.
    ///
    /// Returns:
    ///   * `Ok(Some(result))` — fast path took the mutation.
    ///   * `Ok(None)` — can't take the fast path this call (wrong
    ///     literal type, index dropped since prepare, etc.). Caller
    ///     falls through to the generic substitute-and-execute path.
    ///   * `Err(_)` — real error (table gone, I/O, etc.).
    #[inline]
    fn try_execute_update_pk_fast(
        &mut self,
        fast: &UpdatePkFast,
        literals: &[Literal],
    ) -> Result<Option<QueryResult>, QueryError> {
        // 1) Extract the key literal. The fast path is only built for
        //    int key columns; any other literal type means the caller
        //    is violating the prepared-query contract or the schema
        //    changed — either way, fall back.
        let key_int = match &literals[fast.key_literal_idx] {
            Literal::Int(v) => *v,
            _ => return Ok(None),
        };

        // 2) Encode the new value as little-endian bytes matching the
        //    target column's fixed encoding.
        let bytes: FixedBytes = match (fast.target_type, &literals[fast.value_literal_idx]) {
            (TypeId::Int, Literal::Int(v)) => FixedBytes::I64(v.to_le_bytes()),
            (TypeId::DateTime, Literal::Int(v)) => FixedBytes::I64(v.to_le_bytes()),
            (TypeId::Float, Literal::Float(v)) => FixedBytes::F64(v.to_le_bytes()),
            (TypeId::Bool, Literal::Bool(v)) => FixedBytes::Bool(if *v { 1 } else { 0 }),
            // Type mismatch — fall back to the generic path for a
            // consistent error shape.
            _ => return Ok(None),
        };

        // 3) Look up the table + btree, do the int lookup, patch the row
        //    in place. Phase 18: table dispatch is a direct slot index;
        //    the btree lookup is the linear scan over `indexed_cols`.
        //    Single btree.lookup_int + one `with_row_bytes_mut` call.
        //    No Vec allocations at all.
        //
        // Mission B2: route the in-place patch through the catalog's
        // WAL-logged wrapper so crash recovery sees the update. The
        // extra cost is one WAL append + fsync per query — the hot
        // loop structure is unchanged.
        let tbl = self.catalog.table_by_slot_mut(fast.table_slot);
        let Some(btree) = tbl.index(&fast.key_col) else {
            // Index dropped since prepare — bail to the generic path.
            return Ok(None);
        };
        let Some(rid) = btree.lookup_int(key_int) else {
            return Ok(Some(QueryResult::Modified(0)));
        };

        let fast_table_slot = fast.table_slot;
        let bitmap_byte_off = fast.bitmap_byte_off;
        let bit_mask = fast.bit_mask;
        let field_off = fast.field_off;
        let ok = self
            .catalog
            .update_row_bytes_logged_by_slot(fast_table_slot, rid, |row| {
                // Idempotent null-bit clear — safe even when the column was
                // already non-null (the overwhelmingly common case).
                row[bitmap_byte_off] &= !bit_mask;
                let field_bytes = bytes.as_slice();
                row[field_off..field_off + field_bytes.len()].copy_from_slice(field_bytes);
            })
            .map_err(|e| QueryError::StorageError(e.to_string()))?;

        Ok(Some(QueryResult::Modified(if ok { 1 } else { 0 })))
    }

    /// Mission C Phase 13: moving variant of [`Engine::execute_prepared`]
    /// for the insert fast path. Takes `literals` by mutable reference
    /// so that each `Literal::String` can be consumed via `mem::take`
    /// instead of cloned into a `Value::Str`. On `insert_batch_1k` that
    /// removes three per-row heap allocations (name, status, email),
    /// bringing the workload over the line vs SQLite's amortized
    /// prepare+execute loop.
    ///
    /// The caller's `Literal::String` entries are replaced with empty
    /// strings on successful inserts — the `literals` slice is *not*
    /// left in a valid-for-reuse state except for `Int`/`Float`/`Bool`
    /// values. Non-insert templates fall through to the standard
    /// substitute-and-execute path.
    pub fn execute_prepared_take(
        &mut self,
        prep: &PreparedQuery,
        literals: &mut [Literal],
    ) -> Result<QueryResult, QueryError> {
        if literals.len() != prep.param_count {
            return Err(QueryError::Execution(format!(
                "prepared query expects {} literal(s), got {}",
                prep.param_count,
                literals.len(),
            )));
        }

        if let Some(fast) = &prep.insert_fast {
            let mut values = std::mem::take(&mut self.insert_values_scratch);
            values.clear();
            values.resize(fast.n_cols, Value::Empty);
            for (pos, lit) in literals.iter_mut().enumerate() {
                values[fast.col_indices[pos]] = literal_value_take(lit);
            }
            // Mission C Phase 18: direct O(1) slot index — see
            // `execute_prepared` for rationale. This is the hot path
            // for `insert_batch_1k`.
            let tbl = self.catalog.table_by_slot_mut(fast.table_slot);
            let res = tbl.insert(&values).map_err(|e| e.to_string());
            values.clear();
            self.insert_values_scratch = values;
            res?;
            // Mission B (post-review): statement-boundary WAL group commit.
            self.catalog
                .sync_wal()
                .map_err(|e| QueryError::StorageError(e.to_string()))?;
            return Ok(QueryResult::Modified(1));
        }

        // Non-insert templates — fall back to the standard path. We
        // can't usefully move the literals because `substitute_plan`
        // still expects an immutable slice, and the non-insert hot
        // paths are dominated by plan walks anyway.
        self.execute_prepared(prep, literals)
    }

    /// Walk an expression tree and replace every `InSubquery` node with
    /// an `InList` by executing the subquery and collecting its first
    /// column as literal values. This must be called before entering
    /// the row-by-row scan loop because the scan closure can't call back
    /// into the engine.
    pub(super) fn materialize_subqueries(&mut self, expr: &Expr) -> Result<Expr, QueryError> {
        match expr {
            Expr::InSubquery {
                expr: inner,
                subquery,
                negated,
            } => {
                if is_correlated_subquery(subquery, &self.catalog) {
                    let inner = self.materialize_subqueries(inner)?;
                    return Ok(Expr::InSubquery {
                        expr: Box::new(inner),
                        subquery: subquery.clone(),
                        negated: *negated,
                    });
                }
                let inner = self.materialize_subqueries(inner)?;
                // Plan and execute the subquery.
                let sub_plan = crate::planner::plan_statement(Statement::Query(*subquery.clone()))
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                let result = self.execute_plan(&sub_plan)?;
                let values = match result {
                    QueryResult::Rows { rows, .. } => rows
                        .into_iter()
                        .filter_map(|mut row| {
                            if row.is_empty() {
                                None
                            } else {
                                Some(value_to_expr(row.swap_remove(0)))
                            }
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                Ok(Expr::InList {
                    expr: Box::new(inner),
                    list: values,
                    negated: *negated,
                })
            }
            Expr::ExistsSubquery { subquery, negated } => {
                if is_correlated_subquery(subquery, &self.catalog) {
                    return Ok(expr.clone());
                }
                // Uncorrelated EXISTS: run the subquery once and collapse
                // into a Bool literal.
                let sub_plan = crate::planner::plan_statement(Statement::Query(*subquery.clone()))
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                let result = self.execute_plan(&sub_plan)?;
                let has_rows = match result {
                    QueryResult::Rows { rows, .. } => !rows.is_empty(),
                    _ => false,
                };
                let truth = if *negated { !has_rows } else { has_rows };
                Ok(Expr::Literal(Literal::Bool(truth)))
            }
            Expr::BinaryOp(l, op, r) => {
                let l = self.materialize_subqueries(l)?;
                let r = self.materialize_subqueries(r)?;
                Ok(Expr::BinaryOp(Box::new(l), *op, Box::new(r)))
            }
            Expr::UnaryOp(op, inner) => {
                let inner = self.materialize_subqueries(inner)?;
                Ok(Expr::UnaryOp(*op, Box::new(inner)))
            }
            Expr::Case { whens, else_expr } => {
                let whens = whens
                    .iter()
                    .map(|(c, r)| {
                        let c = self.materialize_subqueries(c)?;
                        let r = self.materialize_subqueries(r)?;
                        Ok((Box::new(c), Box::new(r)))
                    })
                    .collect::<Result<Vec<_>, QueryError>>()?;
                let else_expr = match else_expr {
                    Some(e) => Some(Box::new(self.materialize_subqueries(e)?)),
                    None => None,
                };
                Ok(Expr::Case { whens, else_expr })
            }
            // Leaf nodes: no subqueries possible.
            other => Ok(other.clone()),
        }
    }

    /// Write-path per-row materialisation of correlated subqueries.
    pub(super) fn materialize_correlated_for_row(
        &mut self,
        expr: &Expr,
        outer_row: &[Value],
        outer_columns: &[String],
    ) -> Result<Expr, QueryError> {
        match expr {
            Expr::InSubquery {
                expr: inner,
                subquery,
                negated,
            } => {
                let inner = self.materialize_correlated_for_row(inner, outer_row, outer_columns)?;
                let mut sub = *subquery.clone();
                if let Some(ref filter) = sub.filter {
                    sub.filter = Some(substitute_outer_refs(
                        filter,
                        &sub.source,
                        &self.catalog,
                        outer_row,
                        outer_columns,
                    ));
                }
                let sub_plan = crate::planner::plan_statement(Statement::Query(sub))
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                let result = self.execute_plan(&sub_plan)?;
                let values = match result {
                    QueryResult::Rows { rows, .. } => rows
                        .into_iter()
                        .filter_map(|mut row| {
                            if row.is_empty() {
                                None
                            } else {
                                Some(value_to_expr(row.swap_remove(0)))
                            }
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                Ok(Expr::InList {
                    expr: Box::new(inner),
                    list: values,
                    negated: *negated,
                })
            }
            Expr::ExistsSubquery { subquery, negated } => {
                let mut sub = *subquery.clone();
                if let Some(ref filter) = sub.filter {
                    sub.filter = Some(substitute_outer_refs(
                        filter,
                        &sub.source,
                        &self.catalog,
                        outer_row,
                        outer_columns,
                    ));
                }
                let sub_plan = crate::planner::plan_statement(Statement::Query(sub))
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                let result = self.execute_plan(&sub_plan)?;
                let has_rows = match result {
                    QueryResult::Rows { rows, .. } => !rows.is_empty(),
                    _ => false,
                };
                let truth = if *negated { !has_rows } else { has_rows };
                Ok(Expr::Literal(Literal::Bool(truth)))
            }
            Expr::BinaryOp(l, op, r) => {
                let l = self.materialize_correlated_for_row(l, outer_row, outer_columns)?;
                let r = self.materialize_correlated_for_row(r, outer_row, outer_columns)?;
                Ok(Expr::BinaryOp(Box::new(l), *op, Box::new(r)))
            }
            Expr::UnaryOp(op, inner) => {
                let inner = self.materialize_correlated_for_row(inner, outer_row, outer_columns)?;
                Ok(Expr::UnaryOp(*op, Box::new(inner)))
            }
            other => Ok(other.clone()),
        }
    }
}
