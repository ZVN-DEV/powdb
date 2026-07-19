//! Mutation support: the fused scan+update walk and RowId collection for
//! update/delete shapes.

use crate::cancel::CancelCheck;
use crate::result::{QueryError, QueryResult};
use powdb_storage::row::{patch_var_column_in_place, RowLayout};
use powdb_storage::types::*;
use std::ops::ControlFlow;

use crate::executor::compiled::*;
use crate::executor::eval::*;
use crate::executor::row_body_base;
use crate::executor::Engine;

use super::lowering::{scan_table, stored_json_path_expr};
use super::*;

impl Engine {
    /// Gather the RowIds that a mutation should operate on, without
    /// materialising the full row set. Handles the shapes the planner emits
    /// for update/delete: SeqScan, IndexScan, and Filter(SeqScan). Other
    /// shapes fall back to `generic_rid_match`.
    ///
    /// Perf sprint: try to fuse the predicate evaluation and in-place
    /// byte-level mutation into a single heap walk. Returns `Some(result)`
    /// if the fused path fired, `None` to fall through to the generic
    /// two-pass code.
    ///
    /// Covers two shapes:
    /// 1. Fixed-width non-null literal assignments on non-indexed columns
    ///    → byte-patch every matched row in place (row length unchanged).
    /// 2. Single var-col literal assignment on a non-indexed column
    ///    → `patch_var_column_in_place` on every matched row (may shrink);
    ///    rows that can't be patched in place are collected for fallback.
    pub(super) fn try_fused_scan_update(
        &mut self,
        table: &str,
        predicate: &Expr,
        resolved: &[(usize, Value)],
        changed_cols: &[usize],
    ) -> Option<Result<QueryResult, QueryError>> {
        // Overflow safety (P0/P1): a table that may hold v2 rows can never take
        // the byte-patch fast paths — patching computes v1 offsets and would
        // corrupt a spilled row, and the compiled predicate over raw bytes
        // mis-evaluates a spilled column. Fall through to the reassembling
        // collect-rids + get/update_hinted path.
        if self.catalog.table_has_overflow(table) {
            return None;
        }
        // Build compiled predicate. Requires a schema borrow that must be
        // dropped before we call scan_patch_matching_logged.
        let compiled = {
            let schema = self.catalog.schema(table)?;
            let columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
            let fast = FastLayout::new(schema);
            compile_predicate(predicate, &columns, &fast, schema)?
        };

        // ── Path 1: fixed-width fast patch ──────────────────────────
        let fixed_patches: Option<Vec<FastPatch>> = {
            let tbl = self.catalog.get_table(table)?;
            let schema = tbl.schema();
            let all_fixed_nonnull = resolved
                .iter()
                .all(|(idx, val)| is_fixed_size(schema.columns[*idx].type_id) && !val.is_empty());
            let no_indexed = !resolved.iter().any(|(idx, _)| tbl.has_indexed_col(*idx));
            if all_fixed_nonnull && no_indexed {
                let layout = RowLayout::new(schema);
                let bitmap_size = layout.bitmap_size();
                Some(
                    resolved
                        .iter()
                        .map(|(idx, val)| {
                            let fixed_off = layout
                                .fixed_offset(*idx)
                                .expect("is_fixed_size already checked");
                            let field_off = 2 + bitmap_size + fixed_off;
                            let bytes: FixedBytes = match val {
                                Value::Int(v) => FixedBytes::I64(v.to_le_bytes()),
                                Value::Float(v) => FixedBytes::F64(v.to_le_bytes()),
                                Value::Bool(v) => FixedBytes::Bool(if *v { 1 } else { 0 }),
                                Value::DateTime(v) => FixedBytes::I64(v.to_le_bytes()),
                                Value::Uuid(v) => FixedBytes::Uuid(*v),
                                _ => unreachable!("all_fixed_nonnull guard"),
                            };
                            FastPatch {
                                field_off,
                                bitmap_byte_off: 2 + idx / 8,
                                bit_mask: 1u8 << (idx % 8),
                                bytes,
                            }
                        })
                        .collect(),
                )
            } else {
                None
            }
        };
        if let Some(patches) = fixed_patches {
            let result = self
                .catalog
                .scan_patch_matching_logged(table, compiled, |row| {
                    let base = row_body_base(row);
                    for p in &patches {
                        row[base + p.bitmap_byte_off] &= !p.bit_mask;
                        let field_bytes = p.bytes.as_slice();
                        row[base + p.field_off..base + p.field_off + field_bytes.len()]
                            .copy_from_slice(field_bytes);
                    }
                    Some(row.len() as u16)
                })
                .map_err(|e| e.to_string());
            match result {
                Ok((count, _)) => {
                    self.view_registry.mark_dependents_dirty(table);
                    return Some(Ok(QueryResult::Modified(count)));
                }
                Err(e) => return Some(Err(QueryError::Execution(e))),
            }
        }

        // ── Path 2: single var-col shrink fast patch ────────────────
        let var_patch: Option<(usize, Option<Vec<u8>>)> = {
            let tbl = self.catalog.get_table(table)?;
            let schema = tbl.schema();
            let is_single = resolved.len() == 1;
            let is_var = is_single && !is_fixed_size(schema.columns[resolved[0].0].type_id);
            let no_indexed = !resolved.iter().any(|(idx, _)| tbl.has_indexed_col(*idx));
            if is_single && is_var && no_indexed {
                let (idx, val) = &resolved[0];
                let bytes_opt = match val {
                    Value::Str(s) => Some(s.as_bytes().to_vec()),
                    Value::Bytes(b) => Some(b.clone()),
                    Value::Empty => None,
                    _ => return None, // type mismatch, fall through
                };
                Some((*idx, bytes_opt))
            } else {
                None
            }
        };
        if let Some((col_idx, ref new_bytes_opt)) = var_patch {
            // Build a fresh RowLayout before the mutable borrow.
            let layout = {
                let schema = self.catalog.schema(table)?;
                RowLayout::new(schema)
            };
            let new_bytes_ref: Option<&[u8]> = new_bytes_opt.as_deref();
            let result = self
                .catalog
                .scan_patch_matching_logged(table, compiled, |row| {
                    patch_var_column_in_place(row, &layout, col_idx, new_bytes_ref)
                })
                .map_err(|e| e.to_string());
            match result {
                Ok((mut count, fallback_rids)) => {
                    // Handle rows where in-place patch failed (new > old).
                    for rid in fallback_rids {
                        let mut row = match self.catalog.get(table, rid) {
                            Some(r) => r,
                            None => continue,
                        };
                        for (idx, val) in resolved.iter() {
                            row[*idx] = val.clone();
                        }
                        if let Err(e) =
                            self.catalog
                                .update_hinted(table, rid, &row, Some(changed_cols))
                        {
                            return Some(Err(QueryError::StorageError(e.to_string())));
                        }
                        count += 1;
                    }
                    self.view_registry.mark_dependents_dirty(table);
                    return Some(Ok(QueryResult::Modified(count)));
                }
                Err(e) => return Some(Err(QueryError::Execution(e))),
            }
        }

        None // no fused path applicable — fall through
    }

    /// Collect the RowIds a lowered index-scan node yields, applying the same
    /// exclusive-bound and null-skip rechecks the SELECT executor uses, or
    /// `None` when `scan` is not an index-scan shape the mutation path can
    /// drive from (the caller then falls back to the generic matcher). This is
    /// what keeps an index-driven conjunction update/delete off the O(N*M)
    /// value-rematch path. Every heap fetch goes through `Table::get`, which
    /// reassembles spilled columns, so it is overflow-safe.
    fn index_scan_rids(&self, scan: &PlanNode) -> Result<Option<Vec<RowId>>, QueryError> {
        match scan {
            PlanNode::IndexScan { table, column, key } => {
                let Some(tbl) = self.catalog.get_table(table) else {
                    return Ok(None);
                };
                if !tbl.has_index(column) {
                    return Ok(None);
                }
                let key_value = literal_to_value(key)?;
                Ok(Some(tbl.index_lookup_all(column, &key_value)))
            }
            PlanNode::ExprIndexScan { table, path, key } => {
                let Some(index) = resolve_expression_index(&self.catalog, table, path) else {
                    return Ok(None);
                };
                let key_value = literal_to_value(key)?;
                let rids = if key_value.is_empty() {
                    self.catalog
                        .expression_index_btree(table, index.index_id)
                        .ok_or_else(|| {
                            QueryError::Execution("expression index disappeared".to_string())
                        })?
                        .empty_rids()
                        .to_vec()
                } else {
                    self.catalog
                        .expression_index_lookup_all(table, index.index_id, &key_value)
                        .map_err(|error| QueryError::StorageError(error.to_string()))?
                };
                Ok(Some(rids))
            }
            PlanNode::RangeScan {
                table,
                column,
                start,
                end,
            } => {
                let Some(tbl) = self.catalog.get_table(table) else {
                    return Ok(None);
                };
                let start_val = start
                    .as_ref()
                    .map(|(expr, _)| literal_to_value(expr))
                    .transpose()?;
                let end_val = end
                    .as_ref()
                    .map(|(expr, _)| literal_to_value(expr))
                    .transpose()?;
                let start_inclusive = start.as_ref().map(|(_, inc)| *inc).unwrap_or(true);
                let end_inclusive = end.as_ref().map(|(_, inc)| *inc).unwrap_or(true);
                // Unique and non-unique indexes store keys differently, so their
                // range walks differ, so mirror the SELECT `RangeScan` executor.
                match tbl.is_index_unique(column) {
                    Some(false) => {
                        let col_idx = tbl.schema().column_index(column).ok_or_else(|| {
                            QueryError::ColumnNotFound {
                                table: String::new(),
                                column: column.clone(),
                            }
                        })?;
                        let Some(btree) = tbl.index(column) else {
                            return Ok(None);
                        };
                        // `range_rids` is inclusive over the composite prefix;
                        // recheck enforces exclusive bounds and skips nulls
                        // (never indexed).
                        let candidates = btree.range_rids(start_val.as_ref(), end_val.as_ref());
                        let mut rids = Vec::with_capacity(candidates.len());
                        let mut cancel = CancelCheck::new();
                        for rid in candidates {
                            cancel.tick()?;
                            if let Some(row) = tbl.get(rid) {
                                if !row[col_idx].is_empty()
                                    && range_matches(
                                        &row[col_idx],
                                        &start_val,
                                        start_inclusive,
                                        &end_val,
                                        end_inclusive,
                                    )
                                {
                                    rids.push(rid);
                                }
                            }
                        }
                        Ok(Some(rids))
                    }
                    Some(true) => {
                        let Some(btree) = tbl.index(column) else {
                            return Ok(None);
                        };
                        // Unique index: raw column-value keys. An unbounded scan
                        // is not a range shape the planner emits here, so defer it
                        // to the generic path rather than a full index walk.
                        let hits: Vec<(Value, RowId)> = match (&start_val, &end_val) {
                            (Some(s), Some(e)) => btree.range(s, e).collect(),
                            (Some(s), None) => btree.range_from(s),
                            (None, Some(e)) => btree.range_to(e),
                            (None, None) => return Ok(None),
                        };
                        let mut rids = Vec::with_capacity(hits.len());
                        let mut cancel = CancelCheck::new();
                        for (key, rid) in hits {
                            cancel.tick()?;
                            if !start_inclusive {
                                if let Some(ref s) = start_val {
                                    if &key == s {
                                        continue;
                                    }
                                }
                            }
                            if !end_inclusive {
                                if let Some(ref e) = end_val {
                                    if &key == e {
                                        continue;
                                    }
                                }
                            }
                            rids.push(rid);
                        }
                        Ok(Some(rids))
                    }
                    None => Ok(None),
                }
            }
            PlanNode::ExprRangeScan {
                table,
                path,
                start,
                end,
            } => {
                let Some(index) = resolve_expression_index(&self.catalog, table, path) else {
                    return Ok(None);
                };
                let start_val = start
                    .as_ref()
                    .map(|(expr, _)| literal_to_value(expr))
                    .transpose()?;
                let end_val = end
                    .as_ref()
                    .map(|(expr, _)| literal_to_value(expr))
                    .transpose()?;
                let start_inclusive = start.as_ref().map(|(_, inc)| *inc).unwrap_or(true);
                let end_inclusive = end.as_ref().map(|(_, inc)| *inc).unwrap_or(true);
                let candidates = self
                    .catalog
                    .expression_index_range_rids(
                        table,
                        index.index_id,
                        start_val.as_ref(),
                        end_val.as_ref(),
                    )
                    .map_err(|error| QueryError::StorageError(error.to_string()))?;
                let schema = self
                    .catalog
                    .schema(table)
                    .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                let all_columns: Vec<String> =
                    schema.columns.iter().map(|c| c.name.clone()).collect();
                let path_expr = stored_json_path_expr(path);
                let mut rids = Vec::with_capacity(candidates.len());
                let mut cancel = CancelCheck::new();
                for rid in candidates {
                    cancel.tick()?;
                    let Some(row) = self.catalog.get(table, rid) else {
                        continue;
                    };
                    let value = eval_expr(&path_expr, &row, &all_columns);
                    if value.is_empty()
                        || !range_matches(
                            &value,
                            &start_val,
                            start_inclusive,
                            &end_val,
                            end_inclusive,
                        )
                    {
                        continue;
                    }
                    rids.push(rid);
                }
                Ok(Some(rids))
            }
            _ => Ok(None),
        }
    }

    /// Rid collection for `Filter(<index scan>)` mutation discovery: narrow to
    /// the index scan's candidate rids, then recheck the residual predicate
    /// while decoding only the columns it references (`get_projected`), exactly
    /// as [`Self::try_filter_index_residual_fast`] does for reads. Returns
    /// `None` when the inner scan is not an index shape over `table`, or when
    /// the residual carries a subquery (which cannot be rechecked row-at-a-time
    /// here), so the caller keeps the correct generic path.
    fn collect_rids_via_index_residual(
        &self,
        inner: &PlanNode,
        predicate: &Expr,
        table: &str,
    ) -> Result<Option<Vec<RowId>>, QueryError> {
        if contains_subquery(predicate) || scan_table(inner) != Some(table) {
            return Ok(None);
        }
        let Some(candidates) = self.index_scan_rids(inner)? else {
            return Ok(None);
        };
        let schema = self
            .catalog
            .schema(table)
            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
        let all_columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
        let residual_indices = predicate_column_indices_json(predicate, &all_columns);
        let residual_names: Vec<String> = residual_indices
            .iter()
            .map(|&index| all_columns[index].clone())
            .collect();
        let mut rids = Vec::new();
        let mut cancel = CancelCheck::new();
        for rid in candidates {
            cancel.tick()?;
            let Some(sparse) = self
                .catalog
                .get_projected(table, rid, &residual_indices)
                .map_err(|error| QueryError::StorageError(error.to_string()))?
            else {
                continue;
            };
            if eval_predicate(predicate, &sparse, &residual_names) {
                rids.push(rid);
            }
        }
        Ok(Some(rids))
    }

    /// Mission C Phase 3: schema is looked up via `self.catalog.schema(table)`
    /// inside the branches that actually need it. Previously the caller had
    /// to clone the full Schema (6+ String allocs) before every mutation just
    /// so this function could borrow it — a cost the update/delete hot path
    /// did not need.
    pub(super) fn collect_rids_for_mutation(
        &mut self,
        input: &PlanNode,
        table: &str,
    ) -> Result<Vec<RowId>, QueryError> {
        // Overflow safety (P1/P0-4): the raw-byte fast paths below stream
        // through `for_each_row_raw`, which rehydrates v2 rows to v1 and SKIPS
        // any row carrying a value too large to re-inline (>= 64KB). For a
        // v2-capable table, evaluate the predicate over fully decoded rows
        // instead so no matching row is missed or mis-judged on a spilled
        // column. Exact index lookups (value-size independent) still fall
        // through to the normal path.
        if self.catalog.table_has_overflow(table) {
            if let Some(rids) = self.collect_rids_decoded(input, table)? {
                return Ok(rids);
            }
        }
        match input {
            PlanNode::SeqScan { table: t } if t == table => {
                // "Update/delete everything" — rare but legal.
                let mut cancel = CancelCheck::new();
                let mut rids: Vec<RowId> = Vec::new();
                for (rid, _) in self
                    .catalog
                    .scan(table)
                    .map_err(|e| QueryError::StorageError(e.to_string()))?
                {
                    cancel.tick()?;
                    rids.push(rid);
                }
                Ok(rids)
            }
            PlanNode::IndexScan {
                table: t,
                column,
                key,
            } if t == table => {
                let key_value = literal_to_value(key)?;

                // Indexed case: single lookup, 0 or 1 rows.
                // Mission D7: int-specialized fast path on int-keyed indexes
                // (primary keys, created_at, etc.) — the common case for
                // `update_by_pk` / `delete where id = ?`.
                //
                // Scope the `tbl` borrow so it's released before we fall
                // through to the scan-based paths below (which reborrow
                // `self.catalog`).
                {
                    let tbl = self
                        .catalog
                        .get_table(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                    if tbl.has_index(column) {
                        let rids = tbl.index_lookup_all(column, &key_value);
                        return Ok(rids);
                    }
                }

                // No index: the planner folds `.col = literal` to IndexScan
                // regardless of whether the column is actually unique. When
                // there's no index we must behave like Filter(SeqScan) and
                // return *all* matching RIDs — not just the first one.
                let schema = self
                    .catalog
                    .schema(table)
                    .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                let columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
                let fast = FastLayout::new(schema);
                let synth = Expr::BinaryOp(
                    Box::new(Expr::Field(column.clone())),
                    BinOp::Eq,
                    Box::new(key.clone()),
                );
                if let Some(compiled) = compile_predicate(&synth, &columns, &fast, schema) {
                    // Mission F: skip the first 4 Vec doublings.
                    let mut rids: Vec<RowId> = Vec::with_capacity(64);
                    let mut cancel = CancelCheck::new();
                    let mut cancel_err: Option<QueryError> = None;
                    self.catalog
                        .try_for_each_row_raw(table, |rid, data| {
                            if let Err(e) = cancel.tick() {
                                cancel_err = Some(e);
                                return ControlFlow::Break(());
                            }
                            if compiled(data) {
                                rids.push(rid);
                            }
                            ControlFlow::Continue(())
                        })
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    if let Some(e) = cancel_err {
                        return Err(e);
                    }
                    return Ok(rids);
                }

                // Fallback: decode each row, compare values.
                let col_idx =
                    schema
                        .column_index(column)
                        .ok_or_else(|| QueryError::ColumnNotFound {
                            table: String::new(),
                            column: column.clone(),
                        })?;
                let mut cancel = CancelCheck::new();
                let mut rids: Vec<RowId> = Vec::new();
                for (rid, row) in self
                    .catalog
                    .scan(table)
                    .map_err(|e| QueryError::StorageError(e.to_string()))?
                {
                    cancel.tick()?;
                    if row[col_idx] == key_value {
                        rids.push(rid);
                    }
                }
                Ok(rids)
            }
            PlanNode::RangeScan { table: t, .. }
            | PlanNode::ExprIndexScan { table: t, .. }
            | PlanNode::ExprRangeScan { table: t, .. }
                if t == table =>
            {
                // A conjunction whose residual was fully consumed lowers to a
                // bare index scan (no Filter). Collect its rids from the index
                // directly instead of the generic value rematch.
                match self.index_scan_rids(input)? {
                    Some(rids) => Ok(rids),
                    None => self.generic_rid_match(input, table),
                }
            }
            PlanNode::Filter {
                input: inner,
                predicate,
            } => {
                if let PlanNode::SeqScan { table: t } = inner.as_ref() {
                    if t != table {
                        return self.generic_rid_match(input, table);
                    }
                    let schema = self
                        .catalog
                        .schema(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                    let columns: Vec<String> =
                        schema.columns.iter().map(|c| c.name.clone()).collect();
                    let fast = FastLayout::new(schema);
                    let row_layout = RowLayout::new(schema);

                    // Cooperative cancellation: the rid-collection scan that
                    // backs `update/delete filter <unindexed pred>` walks the
                    // whole table, so it must stay stoppable.
                    let mut cancel = CancelCheck::new();
                    let mut cancel_err: Option<QueryError> = None;
                    // Try compiled predicate first.
                    if let Some(compiled) = compile_predicate(predicate, &columns, &fast, schema) {
                        // Mission F: skip the first 4 Vec doublings.
                        let mut rids: Vec<RowId> = Vec::with_capacity(64);
                        self.catalog
                            .try_for_each_row_raw(table, |rid, data| {
                                if let Err(e) = cancel.tick() {
                                    cancel_err = Some(e);
                                    return ControlFlow::Break(());
                                }
                                if compiled(data) {
                                    rids.push(rid);
                                }
                                ControlFlow::Continue(())
                            })
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                        if let Some(e) = cancel_err {
                            return Err(e);
                        }
                        return Ok(rids);
                    }

                    // Fallback: selective decode + eval.
                    let pred_cols = predicate_column_indices_json(predicate, &columns);
                    let mut rids: Vec<RowId> = Vec::with_capacity(64);
                    self.catalog
                        .try_for_each_row_raw(table, |rid, data| {
                            if let Err(e) = cancel.tick() {
                                cancel_err = Some(e);
                                return ControlFlow::Break(());
                            }
                            let pred_row = decode_selective(schema, &row_layout, data, &pred_cols);
                            if eval_predicate(predicate, &pred_row, &columns) {
                                rids.push(rid);
                            }
                            ControlFlow::Continue(())
                        })
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    if let Some(e) = cancel_err {
                        return Err(e);
                    }
                    return Ok(rids);
                }
                // Lane A mutation fast path: a conjunction update/delete whose
                // discovery scan lowered to `Filter(<index scan>)` collects
                // candidate rids from the index and rechecks the residual per
                // rid, instead of the O(N*M) generic value rematch.
                if let Some(rids) = self.collect_rids_via_index_residual(inner, predicate, table)? {
                    return Ok(rids);
                }
                self.generic_rid_match(input, table)
            }
            _ => self.generic_rid_match(input, table),
        }
    }

    /// Decode-based rid collection for v2-capable tables (see the guard in
    /// [`Self::collect_rids_for_mutation`]). Scans fully reassembled rows via
    /// `Catalog::scan` (`decode_row_v2`, chain fetch, correct for any value
    /// size) and evaluates the predicate on decoded `Value`s. Returns `None`
    /// for shapes it does not special-case (indexed `IndexScan`, or anything
    /// exotic) so the caller falls through to the normal path.
    fn collect_rids_decoded(
        &mut self,
        input: &PlanNode,
        table: &str,
    ) -> Result<Option<Vec<RowId>>, QueryError> {
        // Determine the per-row predicate (None = match every row).
        let pred: Option<Expr> = match input {
            PlanNode::SeqScan { table: t } if t == table => None,
            PlanNode::Filter {
                input: inner,
                predicate,
            } => match inner.as_ref() {
                PlanNode::SeqScan { table: t } if t == table => Some(predicate.clone()),
                _ => return Ok(None),
            },
            PlanNode::IndexScan {
                table: t,
                column,
                key,
            } if t == table => {
                // A real index makes the lookup exact and value-size
                // independent — let the normal IndexScan path handle it.
                let indexed = self
                    .catalog
                    .get_table(table)
                    .map(|tb| tb.has_index(column))
                    .unwrap_or(false);
                if indexed {
                    return Ok(None);
                }
                Some(Expr::BinaryOp(
                    Box::new(Expr::Field(column.clone())),
                    BinOp::Eq,
                    Box::new(key.clone()),
                ))
            }
            _ => return Ok(None),
        };

        let columns: Vec<String> = {
            let schema = self
                .catalog
                .schema(table)
                .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
            schema.columns.iter().map(|c| c.name.clone()).collect()
        };
        let mut rids: Vec<RowId> = Vec::new();
        let mut cancel = CancelCheck::new();
        for (rid, row) in self
            .catalog
            .scan(table)
            .map_err(|e| QueryError::StorageError(e.to_string()))?
        {
            cancel.tick()?;
            let keep = match &pred {
                None => true,
                Some(p) => eval_predicate(p, &row, &columns),
            };
            if keep {
                rids.push(rid);
            }
        }
        Ok(Some(rids))
    }

    /// Last-ditch generic match: execute the plan, collect matching rows,
    /// then find corresponding RowIds by value equality. This is the old
    /// O(N*M) code path; only used when the plan shape is something exotic.
    fn generic_rid_match(
        &mut self,
        input: &PlanNode,
        table: &str,
    ) -> Result<Vec<RowId>, QueryError> {
        #[cfg(test)]
        GENERIC_RID_MATCH_CALLS.with(|calls| calls.set(calls.get() + 1));
        let result = self.execute_plan(input)?;
        let rows = match result {
            QueryResult::Rows { rows, .. } => rows,
            _ => return Err("mutation source must be rows".into()),
        };
        let mut matching: Vec<RowId> = Vec::new();
        let mut cancel = CancelCheck::new();
        for (rid, row) in self
            .catalog
            .scan(table)
            .map_err(|e| QueryError::StorageError(e.to_string()))?
        {
            cancel.tick()?;
            let mut matched = false;
            for candidate in &rows {
                cancel.tick()?;
                if candidate == &row {
                    matched = true;
                    break;
                }
            }
            if matched {
                matching.push(rid);
            }
        }
        Ok(matching)
    }
}
