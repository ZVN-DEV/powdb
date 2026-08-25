//! The `execute_plan` dispatch match and materialized view operations.

use crate::cancel::CancelCheck;
use crate::result::{QueryError, QueryResult};
use powdb_storage::catalog::{LinkDef, LinkKind};
use powdb_storage::row::{decode_row, RowLayout};
use std::ops::ControlFlow;

use crate::executor::eval::*;
use crate::executor::row_body_base;
use crate::executor::{Engine, MAX_SORT_ROWS};
use powdb_storage::view::ViewDef;

use super::*;

impl Engine {
    /// Execute a plan on the mutable path.
    ///
    /// This is the one execution entry point that takes a bare [`PlanNode`],
    /// because embedders build plans themselves: `planner::plan` is public,
    /// `PlanNode` is public, and the `powdb` facade re-exports this method. So
    /// it lowers first, exactly like every path inside the executor does.
    ///
    /// Lowering is not an optimization. The planner is pure, so it emits index
    /// probes speculatively and leaves every literal as written; the pass is
    /// what decides whether those probes exist and what key bytes they address.
    /// Executing raw planner output here made a planned `.price < 3` answer
    /// `[]` through this entry point where the same text through
    /// [`Engine::execute_powql`] answered the rows, and `LoweredPlan` is
    /// crate-private, so an embedder had no way to lower for itself.
    ///
    /// Lowering is idempotent, so a caller that already has a lowered tree pays
    /// one pass and gets the same plan back.
    pub fn execute_plan(&mut self, plan: &PlanNode) -> Result<QueryResult, QueryError> {
        let lowered = self.lower(plan);
        self.execute_lowered(&lowered)
    }

    /// The write-path dispatch itself. Takes a bare `&PlanNode` because it is
    /// the recursion target: every child of a lowered plan is lowered, so a
    /// subtree needs no second pass. Mirrors [`Engine::dispatch_readonly`], and
    /// is private for the same reason: reaching it from outside an already
    /// lowered tree is what [`Engine::execute_plan`] above exists to prevent.
    pub(in crate::executor) fn dispatch_mut(
        &mut self,
        plan: &PlanNode,
    ) -> Result<QueryResult, QueryError> {
        // Refuse any plan whose evaluable expressions still carry an aggregate
        // FunctionCall the grouped-aggregate planner could not lower. Without
        // this, such an aggregate would reach eval_expr and silently evaluate
        // to Empty (a wrong answer). The outermost call validates the whole
        // tree before any row is produced.
        validate_no_stray_aggregates(plan)?;
        validate_json_path_types(&self.catalog, plan)?;
        validate_column_references(&self.catalog, plan)?;
        validate_slice_counts(plan)?;
        match plan {
            PlanNode::ExprIndexScan { .. }
            | PlanNode::ExprRangeScan { .. }
            | PlanNode::OrderedExprIndexScan { .. } => {
                if let Some(result) = self.execute_expression_index_plan(plan, None)? {
                    return Ok(result);
                }
                let fallback = expression_index_fallback(plan)
                    .expect("expression-index branch always has a fallback");
                self.dispatch_mut(&fallback)
            }
            PlanNode::SeqScan { table } => {
                // Auto-refresh dirty materialized views on read.
                if self.view_registry.is_dirty(table) {
                    self.refresh_view(table)?;
                }
                let schema = self
                    .catalog
                    .schema(table)
                    .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
                    .clone();
                let columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
                // Cooperative cancellation: a full-table scan of a huge table
                // must stay stoppable.
                let mut cancel = CancelCheck::new();
                let mut rows: Vec<Vec<Value>> = Vec::new();
                for item in self
                    .catalog
                    .scan(table)
                    .map_err(QueryError::from_storage_io)?
                {
                    let (_, row) = item.map_err(QueryError::from_storage_io)?;
                    cancel.tick()?;
                    rows.push(row);
                }
                Ok(QueryResult::Rows { columns, rows })
            }

            PlanNode::Filter { input, predicate } => {
                // Materialize any IN-subqueries in the predicate before the
                // scan loop — the closure can't call back into the engine.
                // Correlated subqueries are left in place for per-row eval.
                let materialized;
                let predicate = if contains_subquery(predicate) {
                    materialized = self.materialize_subqueries(predicate)?;
                    &materialized
                } else {
                    predicate
                };

                // Correlated subquery path: per-row materialisation.
                if contains_subquery(predicate) {
                    let result = self.dispatch_mut(input)?;
                    return match result {
                        QueryResult::Rows { columns, rows } => {
                            let mut filtered = Vec::new();
                            // Cooperative cancellation: a subquery runs per outer
                            // row, so a large outer scan must stay stoppable.
                            let mut cancel = CancelCheck::new();
                            for row in rows {
                                cancel.tick()?;
                                let row_pred =
                                    self.materialize_correlated_for_row(predicate, &row, &columns)?;
                                if eval_predicate(&row_pred, &row, &columns) {
                                    filtered.push(row);
                                }
                            }
                            Ok(QueryResult::Rows {
                                columns,
                                rows: filtered,
                            })
                        }
                        _ => Err("filter requires row input".into()),
                    };
                }

                // Lane A fast path: Filter over an equality-driven index scan.
                // The index narrows the candidate rids; the residual is
                // re-checked with a partial decode, full rows only for matches.
                if matches!(
                    input.as_ref(),
                    PlanNode::IndexScan { .. } | PlanNode::ExprIndexScan { .. }
                ) {
                    if let Some(result) = self.try_filter_index_residual_fast(input, predicate)? {
                        return Ok(result);
                    }
                }

                // Fast path: fuse Filter + SeqScan into a zero-copy streaming
                // loop. Uses decode_column() to evaluate the predicate on only
                // the columns it references, avoiding heap allocations for
                // String/Bytes columns that aren't part of the filter.
                // Overflow safety (P0-4/P1): v2-capable tables fall through to
                // the decoded general Filter path below — the raw fast path
                // rehydrates to v1 and drops/mis-reads >= 64KB spilled values.
                if let PlanNode::SeqScan { table } = input.as_ref() {
                    if !self.catalog.table_has_overflow(table)
                        && !self.generic_path_forced("filter-seqscan-raw")
                    {
                        // Auto-refresh dirty materialized views.
                        if self.view_registry.is_dirty(table) {
                            self.refresh_view(table)?;
                        }
                        let schema = self
                            .catalog
                            .schema(table)
                            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
                            .clone();
                        let columns: Vec<String> =
                            schema.columns.iter().map(|c| c.name.clone()).collect();
                        let fast = FastLayout::new(&schema);
                        let row_layout = RowLayout::new(&schema);
                        // Mission F: pre-size to skip the first 4 Vec doublings
                        // (4 → 8 → 16 → 32 → 64). On a 100K-row scan with 30%
                        // selectivity that's ~4 fewer reallocations + memcpys.
                        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(64);

                        // Try compiled predicate for the filter check (handles
                        // int leaves, string-eq leaves, and And conjunctions).
                        // Cooperative cancellation: a full-table compiled/
                        // selective predicate scan must stay stoppable, so use
                        // the early-terminating scan and break on cancel. The
                        // captured error is surfaced after the scan returns.
                        let mut cancel = CancelCheck::new();
                        let mut cancel_err: Option<QueryError> = None;
                        if let Some(compiled) = self.compile_predicate_unless_forced(
                            "filter-seqscan:predicate",
                            predicate,
                            &columns,
                            &fast,
                            &schema,
                        ) {
                            self.catalog
                                .try_for_each_row_raw(table, |_rid, data| {
                                    if let Err(e) = cancel.tick() {
                                        cancel_err = Some(e);
                                        return ControlFlow::Break(());
                                    }
                                    if compiled(data) {
                                        rows.push(decode_row(&schema, data));
                                    }
                                    ControlFlow::Continue(())
                                })
                                .map_err(QueryError::from_storage_io)?;
                        } else {
                            let pred_cols = predicate_column_indices_json(predicate, &columns);
                            self.catalog
                                .try_for_each_row_raw(table, |_rid, data| {
                                    if let Err(e) = cancel.tick() {
                                        cancel_err = Some(e);
                                        return ControlFlow::Break(());
                                    }
                                    let pred_row =
                                        decode_selective(&schema, &row_layout, data, &pred_cols);
                                    if eval_predicate(predicate, &pred_row, &columns) {
                                        rows.push(decode_row(&schema, data));
                                    }
                                    ControlFlow::Continue(())
                                })
                                .map_err(QueryError::from_storage_io)?;
                        }
                        if let Some(e) = cancel_err {
                            return Err(e);
                        }

                        return Ok(QueryResult::Rows { columns, rows });
                    }
                }

                // General path: materialise then filter.
                let result = self.dispatch_mut(input)?;
                match result {
                    QueryResult::Rows { columns, rows } => {
                        let mut cancel = CancelCheck::new();
                        let mut filtered: Vec<Vec<Value>> = Vec::new();
                        for row in rows {
                            cancel.tick()?;
                            if eval_predicate(predicate, &row, &columns) {
                                filtered.push(row);
                            }
                        }
                        Ok(QueryResult::Rows {
                            columns,
                            rows: filtered,
                        })
                    }
                    _ => Err("filter requires row input".into()),
                }
            }

            PlanNode::Project { input, fields } => {
                if matches!(
                    input.as_ref(),
                    PlanNode::ExprIndexScan { .. }
                        | PlanNode::ExprRangeScan { .. }
                        | PlanNode::OrderedExprIndexScan { .. }
                ) {
                    if let Some(result) = self.execute_expression_index_plan(input, Some(fields))? {
                        return Ok(result);
                    }
                }
                // Fast path: Project over IndexScan — decode only projected
                // columns from raw bytes instead of full decode_row.
                if let PlanNode::IndexScan { table, column, key } = input.as_ref() {
                    let schema = self
                        .catalog
                        .schema(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
                        .clone();
                    let all_columns: Vec<String> =
                        schema.columns.iter().map(|c| c.name.clone()).collect();
                    let key_value = literal_to_value(key)?;
                    let tbl = self
                        .catalog
                        .get_table(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;

                    let proj_columns: Vec<String> = fields
                        .iter()
                        .map(|f| {
                            f.alias.clone().unwrap_or_else(|| match &f.expr {
                                Expr::Field(name) => name.clone(),
                                _ => "?".into(),
                            })
                        })
                        .collect();

                    // Determine which column indices the projection needs
                    let proj_indices: Vec<usize> = fields
                        .iter()
                        .filter_map(|f| {
                            if let Expr::Field(name) = &f.expr {
                                all_columns.iter().position(|c| c == name)
                            } else {
                                None
                            }
                        })
                        .collect();

                    // Only serve plain-field projections here; a computed
                    // projection (e.g. `length(.v)`) must fall through to the
                    // generic expression-evaluating path — otherwise its column
                    // is silently dropped (proj_indices only collects Fields).
                    let all_plain_fields = fields.iter().all(|f| matches!(f.expr, Expr::Field(_)));
                    if tbl.has_index(column)
                        && all_plain_fields
                        && !self.generic_path_forced("project-over-index-scan")
                    {
                        let rids = tbl.index_lookup_all(column, &key_value);
                        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(rids.len());
                        let mut cancel = CancelCheck::new();
                        for rid in rids {
                            cancel.tick()?;
                            // Overflow safety (P0-3/P0-4): `tbl.get` reassembles
                            // spilled columns from their overflow chains. The old
                            // `heap.get` + `decode_column` read raw v2 bytes and
                            // returned Empty for a spilled column (or wrapped a
                            // >= 64KB value).
                            if let Some(full) = tbl.get(rid) {
                                let row: Vec<Value> =
                                    proj_indices.iter().map(|&ci| full[ci].clone()).collect();
                                rows.push(row);
                            }
                        }
                        return Ok(QueryResult::Rows {
                            columns: proj_columns,
                            rows,
                        });
                    }
                }

                // Fast path: Project(Limit(Sort(Filter(SeqScan)))) — bounded
                // top-N heap. Decodes only the sort key + projected columns,
                // keeps at most `limit` rows in a heap. Also handles the
                // Project(Limit(Sort(SeqScan))) variant (no filter).
                if let PlanNode::Limit {
                    input: inner,
                    count: limit_expr,
                } = input.as_ref()
                {
                    if let PlanNode::Sort {
                        input: sort_input,
                        keys,
                    } = inner.as_ref()
                    {
                        // Fast path only for single-key sorts, and only for a
                        // bound this path may act on, an unreadable count is
                        // the generic `Limit` arm's error to report.
                        if keys.len() == 1 {
                            if let (Expr::Field(sort_field), Some(limit)) =
                                (&keys[0].expr, literal_limit(limit_expr))
                            {
                                let descending = keys[0].descending;
                                let (table_opt, pred_opt): (Option<&str>, Option<&Expr>) =
                                    match sort_input.as_ref() {
                                        PlanNode::SeqScan { table } => (Some(table.as_str()), None),
                                        PlanNode::Filter {
                                            input: fi,
                                            predicate,
                                        } => {
                                            if let PlanNode::SeqScan { table } = fi.as_ref() {
                                                (Some(table.as_str()), Some(predicate))
                                            } else {
                                                (None, None)
                                            }
                                        }
                                        _ => (None, None),
                                    };
                                if let Some(table) = table_opt {
                                    if let Some(result) = self.project_filter_sort_limit_fast(
                                        table, fields, sort_field, descending, limit, pred_opt,
                                    )? {
                                        return Ok(result);
                                    }
                                }
                            }
                        }
                    }
                    // Fast path: Project(Limit(Filter(SeqScan))) — stream,
                    // decode only projected columns, stop at limit.
                    if let PlanNode::Filter {
                        input: fi,
                        predicate,
                    } = inner.as_ref()
                    {
                        if let (PlanNode::SeqScan { table }, Some(limit)) =
                            (fi.as_ref(), literal_limit(limit_expr))
                        {
                            if let Some(result) = self.project_filter_limit_fast(
                                table,
                                fields,
                                limit,
                                Some(predicate),
                            )? {
                                return Ok(result);
                            }
                        }
                    }
                    // Fast path: Project(Limit(SeqScan)) — stream, no filter.
                    if let (PlanNode::SeqScan { table }, Some(limit)) =
                        (inner.as_ref(), literal_limit(limit_expr))
                    {
                        if let Some(result) =
                            self.project_filter_limit_fast(table, fields, limit, None)?
                        {
                            return Ok(result);
                        }
                    }
                }

                // Mission D4: Project(Filter(SeqScan)) without Limit. Reuses
                // `project_filter_limit_fast` with limit = usize::MAX so the
                // hot loop decodes only projected columns and uses the
                // compiled predicate. Previously this fell through to the
                // generic Filter branch which materialised every column via
                // `decode_row` then re-projected — quadratic work.
                //
                // multi_col_and_filter (`U filter .age > 30 and .status =
                // "active" { .name, .age }`) was 6.18ms (0.7x SQLite) and
                // is the load-bearing workload for this fast path.
                if let PlanNode::Filter {
                    input: fi,
                    predicate,
                } = input.as_ref()
                {
                    if let PlanNode::SeqScan { table } = fi.as_ref() {
                        if let Some(result) = self.project_filter_limit_fast(
                            table,
                            fields,
                            usize::MAX,
                            Some(predicate),
                        )? {
                            return Ok(result);
                        }
                    }
                }

                // Mission D4: Project(SeqScan) without Filter or Limit.
                // Decode only projected columns; the previous fall-through
                // built full Vec<Value> rows then re-projected.
                if let PlanNode::SeqScan { table } = input.as_ref() {
                    if let Some(result) =
                        self.project_filter_limit_fast(table, fields, usize::MAX, None)?
                    {
                        return Ok(result);
                    }
                }

                let result = self.dispatch_mut(input)?;
                match result {
                    QueryResult::Rows { columns, rows } => {
                        let proj_columns: Vec<String> = fields
                            .iter()
                            .map(|f| {
                                f.alias.clone().unwrap_or_else(|| match &f.expr {
                                    Expr::Field(name) => name.clone(),
                                    // Mission E1.2: `{ u.name }` projects as the
                                    // qualified column name so callers can still
                                    // disambiguate across the join output.
                                    Expr::QualifiedField { qualifier, field } => {
                                        format!("{qualifier}.{field}")
                                    }
                                    _ => "?".into(),
                                })
                            })
                            .collect();
                        let mut cancel = CancelCheck::new();
                        let mut proj_rows: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
                        for row in &rows {
                            cancel.tick()?;
                            proj_rows.push(
                                fields
                                    .iter()
                                    .map(|f| eval_expr(&f.expr, row, &columns))
                                    .collect(),
                            );
                        }
                        Ok(QueryResult::Rows {
                            columns: proj_columns,
                            rows: proj_rows,
                        })
                    }
                    _ => Err("project requires row input".into()),
                }
            }

            PlanNode::Sort { input, keys } => {
                let result = self.dispatch_mut(input)?;
                match result {
                    QueryResult::Rows { columns, mut rows } => {
                        // WS2: row-count cap is a cheap secondary guard; the
                        // byte budget is the real OOM defense for the sort
                        // buffer (a few very large rows pass the row cap).
                        if rows.len() > MAX_SORT_ROWS {
                            return Err(QueryError::SortLimitExceeded);
                        }
                        self.charge_rows(&rows)?;
                        let key_specs: Vec<(Option<usize>, &Expr, bool)> = keys
                            .iter()
                            .map(|k| {
                                let stored_name = match &k.expr {
                                    Expr::Field(name) => Some(name.clone()),
                                    Expr::QualifiedField { qualifier, field } => {
                                        Some(format!("{qualifier}.{field}"))
                                    }
                                    _ => None,
                                };
                                // Same resolver the projections, filters and
                                // join keys use, so `order .amount` inside a
                                // join resolves the bare name against the
                                // `alias.field` scan columns instead of
                                // reporting a column the next clause projects
                                // as missing.
                                let index = stored_name
                                    .as_ref()
                                    .and_then(|name| resolve_column_index(name, &columns));
                                if let Some(name) = stored_name {
                                    if index.is_none() {
                                        return Err(QueryError::ColumnNotFound {
                                            table: String::new(),
                                            column: name,
                                        });
                                    }
                                }
                                Ok((index, &k.expr, k.descending))
                            })
                            .collect::<Result<_, QueryError>>()?;
                        cooperative_stable_sort_by(&mut rows, self.query_memory_limit, |a, b| {
                            for &(col_idx, expr, descending) in &key_specs {
                                let (left_value, right_value) = match col_idx {
                                    Some(index) => (&a[index], &b[index]),
                                    None => {
                                        let left = eval_expr(expr, a, &columns);
                                        let right = eval_expr(expr, b, &columns);
                                        let cmp = compare_order_values(&left, &right, descending);
                                        if cmp != std::cmp::Ordering::Equal {
                                            return cmp;
                                        }
                                        continue;
                                    }
                                };
                                let cmp = compare_order_values(left_value, right_value, descending);
                                if cmp != std::cmp::Ordering::Equal {
                                    return cmp;
                                }
                            }
                            std::cmp::Ordering::Equal
                        })?;
                        Ok(QueryResult::Rows { columns, rows })
                    }
                    _ => Err("sort requires row input".into()),
                }
            }

            PlanNode::Limit { input, count } => {
                let result = self.dispatch_mut(input)?;
                let n = match count {
                    Expr::Literal(Literal::Int(v)) => *v as usize,
                    _ => return Err("limit must be integer literal".into()),
                };
                match result {
                    QueryResult::Rows { columns, rows } => {
                        let mut cancel = CancelCheck::new();
                        let mut limited = Vec::with_capacity(n.min(rows.len()));
                        for row in rows.into_iter().take(n) {
                            cancel.tick()?;
                            limited.push(row);
                        }
                        Ok(QueryResult::Rows {
                            columns,
                            rows: limited,
                        })
                    }
                    _ => Err("limit requires row input".into()),
                }
            }

            PlanNode::Offset { input, count } => {
                let result = self.dispatch_mut(input)?;
                let n = match count {
                    Expr::Literal(Literal::Int(v)) => *v as usize,
                    _ => return Err("offset must be integer literal".into()),
                };
                match result {
                    QueryResult::Rows { columns, rows } => {
                        let mut cancel = CancelCheck::new();
                        let mut offset = Vec::with_capacity(rows.len().saturating_sub(n));
                        for (index, row) in rows.into_iter().enumerate() {
                            cancel.tick()?;
                            if index >= n {
                                offset.push(row);
                            }
                        }
                        Ok(QueryResult::Rows {
                            columns,
                            rows: offset,
                        })
                    }
                    _ => Err("offset requires row input".into()),
                }
            }

            PlanNode::Aggregate {
                input,
                function,
                argument,
                mode: _,
                provenance_alias,
            } => {
                if let Some(provenance_alias) = provenance_alias {
                    let input = self.materialize_rows_with_provenance(input)?;
                    self.charge_rows(&input.rows)?;
                    return aggregate_rows_with_provenance(
                        *function,
                        argument.as_ref(),
                        &input,
                        provenance_alias,
                        self.query_memory_limit(),
                    );
                }
                // Fast path: count() over SeqScan, counting rows without any decode.
                // Only a count with no target column counts rows: `count(T { .v })`
                // counts non-null `.v` and must reach the generic path below.
                // The forced-generic check gates the whole block, including the
                // count-over-filter path further down: one guard, so the inner
                // `compile_predicate_unless_forced` never records a decline
                // while the switch is on.
                if *function == AggFunc::Count
                    && counts_every_row(argument.as_ref())
                    && !self.generic_path_forced("count-fast-block")
                {
                    // Overflow safety (P0-4): the raw `for_each_row_raw` count
                    // drops any row too large to re-inline (>= 64KB) and would
                    // undercount; v2-capable tables use the decoded generic path.
                    if let PlanNode::SeqScan { table } = input.as_ref() {
                        if !self.catalog.table_has_overflow(table) {
                            // Auto-refresh a dirty materialized view before
                            // counting it — otherwise count(View) returns stale
                            // data after an underlying mutation (F3).
                            if self.view_registry.is_dirty(table) {
                                self.refresh_view(table)?;
                            }
                            let mut count: i64 = 0;
                            for_each_row_raw_cancellable(&self.catalog, table, |_rid, _data| {
                                count += 1;
                            })?;
                            return Ok(QueryResult::Scalar(Value::Int(count)));
                        }
                    }
                    // Fast path: count() over Filter(SeqScan) — try compiled
                    // predicate first, fall back to decode_column path.
                    // Skip a predicate carrying a subquery: the raw-bytes
                    // evaluators here don't materialise subqueries, so
                    // `count(T filter .x in (...))` would silently count 0
                    // (F1). Falling through routes it to the generic path
                    // that resolves the subquery correctly.
                    if let PlanNode::Filter {
                        input: inner,
                        predicate,
                    } = input.as_ref()
                    {
                        if let PlanNode::SeqScan { table } = inner.as_ref() {
                            if self.view_registry.is_dirty(table) {
                                self.refresh_view(table)?;
                            }
                        }
                        if let (PlanNode::SeqScan { table }, false) =
                            (inner.as_ref(), contains_subquery(predicate))
                        {
                            if !self.catalog.table_has_overflow(table) {
                                let schema = self
                                    .catalog
                                    .schema(table)
                                    .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
                                    .clone();
                                let columns: Vec<String> =
                                    schema.columns.iter().map(|c| c.name.clone()).collect();
                                let fast = FastLayout::new(&schema);
                                let row_layout = RowLayout::new(&schema);

                                // Try compiled predicate (zero-allocation hot path).
                                // Handles int leaves, string-eq leaves, AND conjunctions.
                                if let Some(compiled) = self.compile_predicate_unless_forced(
                                    "count-filter:predicate",
                                    predicate,
                                    &columns,
                                    &fast,
                                    &schema,
                                ) {
                                    let mut count: i64 = 0;
                                    for_each_row_raw_cancellable(
                                        &self.catalog,
                                        table,
                                        |_rid, data| {
                                            if compiled(data) {
                                                count += 1;
                                            }
                                        },
                                    )?;
                                    return Ok(QueryResult::Scalar(Value::Int(count)));
                                }

                                // Fallback: decode predicate columns
                                let pred_cols = predicate_column_indices_json(predicate, &columns);
                                let mut count: i64 = 0;
                                for_each_row_raw_cancellable(
                                    &self.catalog,
                                    table,
                                    |_rid, data| {
                                        let pred_row = decode_selective(
                                            &schema,
                                            &row_layout,
                                            data,
                                            &pred_cols,
                                        );
                                        if eval_predicate(predicate, &pred_row, &columns) {
                                            count += 1;
                                        }
                                    },
                                )?;

                                return Ok(QueryResult::Scalar(Value::Int(count)));
                            }
                        }
                    }
                }

                // Fast path: sum/avg/min/max over a single fixed-size int
                // column with an optional compiled filter predicate. Walks
                // raw row bytes, zero allocation per row.
                if matches!(
                    function,
                    AggFunc::Sum
                        | AggFunc::Avg
                        | AggFunc::Min
                        | AggFunc::Max
                        | AggFunc::CountDistinct
                ) {
                    if let Some(Expr::Field(col)) = argument.as_ref() {
                        // Shape: Aggregate(SeqScan) or Aggregate(Filter(SeqScan))
                        let (table_opt, pred_opt): (Option<&str>, Option<&Expr>) =
                            match input.as_ref() {
                                PlanNode::SeqScan { table } => (Some(table.as_str()), None),
                                PlanNode::Filter {
                                    input: inner,
                                    predicate,
                                } => {
                                    if let PlanNode::SeqScan { table } = inner.as_ref() {
                                        (Some(table.as_str()), Some(predicate))
                                    } else {
                                        (None, None)
                                    }
                                }
                                _ => (None, None),
                            };
                        if let Some(table) = table_opt {
                            if let Some(result) =
                                self.agg_single_col_fast(table, col, *function, pred_opt)?
                            {
                                return Ok(result);
                            }
                        }
                    }
                }

                // Fast path: Project(Limit(Filter(SeqScan))) — stream, decode
                // only projected columns, stop once we hit the limit.
                // (Handled in the Project branch; this branch only fires when
                // the aggregate is the outer node.)
                let result = self.dispatch_mut(input)?;
                match result {
                    QueryResult::Rows { columns, rows } => {
                        aggregate_rows(*function, argument.as_ref(), &columns, &rows)
                    }
                    _ => Err("aggregate requires row input".into()),
                }
            }

            PlanNode::Insert {
                table,
                rows,
                returning,
            } => {
                // Build + validate EVERY row before inserting any, so a bad
                // row (unknown/missing/uncoercible field) aborts the whole
                // statement without a partial write. The WAL fsync happens
                // once at statement end, so N rows = N appends + 1 fsync.
                let mut returning_columns: Vec<String> = Vec::new();
                let all_values: Vec<Vec<Value>> = {
                    let schema = self
                        .catalog
                        .schema(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                    if *returning {
                        returning_columns = schema.columns.iter().map(|c| c.name.clone()).collect();
                    }
                    let defaults = self.catalog.column_defaults(table).unwrap_or(&[]);
                    let auto = self.catalog.auto_columns(table).unwrap_or(&[]);
                    let mut all = Vec::with_capacity(rows.len());
                    for assignments in rows {
                        let mut values = vec![Value::Empty; schema.columns.len()];
                        for a in assignments {
                            let idx = schema.column_index(&a.field).ok_or_else(|| {
                                QueryError::ColumnNotFound {
                                    table: String::new(),
                                    column: a.field.clone(),
                                }
                            })?;
                            let raw = literal_to_value(&a.value)?;
                            values[idx] = coerce_value(raw, &schema.columns[idx])?;
                        }
                        // Fill any column left unset by this row from its
                        // declared default (applied before the required check,
                        // so a default satisfies a required column).
                        for (i, slot) in values.iter_mut().enumerate() {
                            if slot.is_empty() {
                                if let Some(Some(d)) = defaults.get(i) {
                                    *slot = d.clone();
                                }
                            }
                        }
                        for col in &schema.columns {
                            let pos = col.position as usize;
                            // Auto columns are exempt from the required check —
                            // they are filled from the sequence just below.
                            let is_auto = auto.get(pos).copied().unwrap_or(false);
                            if col.required && !is_auto && matches!(values[pos], Value::Empty) {
                                return Err(QueryError::Execution(format!(
                                    "column '{}' is required but no value was provided",
                                    col.name
                                )));
                            }
                        }
                        all.push(values);
                    }
                    all
                };
                // Assign auto-increment columns now that the immutable
                // schema/defaults/auto borrows are released. Done here (not in
                // the build loop) so the assigned ids land in `all_values` and
                // flow back through `returning`.
                let mut all_values = all_values;
                for values in all_values.iter_mut() {
                    self.catalog
                        .assign_auto_columns(table, values)
                        .map_err(QueryError::from_storage_io)?;
                }
                // Charge the materialized batch against the per-query memory
                // budget before inserting — keeps multi-row insert consistent
                // with every other full-materialization point (sort/join/group)
                // and bounds embedded callers (the server also caps the query
                // string at 1 MB, but embedded callers have no such limit).
                self.charge_rows(&all_values)?;
                let n = all_values.len() as u64;
                for values in &all_values {
                    self.catalog
                        .insert(table, values)
                        .map_err(QueryError::from_storage_io)?;
                }
                self.view_registry
                    .mark_dependents_dirty(table)
                    .map_err(QueryError::from_storage_io)?;
                if *returning {
                    Ok(QueryResult::Rows {
                        columns: returning_columns,
                        rows: all_values,
                    })
                } else {
                    Ok(QueryResult::Modified(n))
                }
            }

            PlanNode::Upsert {
                table,
                key_column,
                assignments,
                on_conflict,
            } => {
                let (mut values, key_idx) = {
                    let schema = self
                        .catalog
                        .schema(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                    let mut values = vec![Value::Empty; schema.columns.len()];
                    for a in assignments {
                        let idx = schema.column_index(&a.field).ok_or_else(|| {
                            QueryError::ColumnNotFound {
                                table: String::new(),
                                column: a.field.clone(),
                            }
                        })?;
                        let raw = literal_to_value(&a.value)?;
                        values[idx] = coerce_value(raw, &schema.columns[idx])?;
                    }
                    // Apply column defaults for the insert path, same as a plain
                    // insert (applied before the required-column check).
                    let defaults = self.catalog.column_defaults(table).unwrap_or(&[]);
                    for (i, slot) in values.iter_mut().enumerate() {
                        if slot.is_empty() {
                            if let Some(Some(d)) = defaults.get(i) {
                                *slot = d.clone();
                            }
                        }
                    }
                    let auto = self.catalog.auto_columns(table).unwrap_or(&[]);
                    for col in &schema.columns {
                        let pos = col.position as usize;
                        // Auto columns are exempt from the required check, same
                        // as the Insert arm: they are filled from the sequence
                        // on the insert branch below.
                        let is_auto = auto.get(pos).copied().unwrap_or(false);
                        if col.required && !is_auto && matches!(values[pos], Value::Empty) {
                            return Err(QueryError::Execution(format!(
                                "column '{}' is required but no value was provided",
                                col.name
                            )));
                        }
                    }
                    let key_idx = schema
                        .column_index(key_column)
                        .ok_or_else(|| format!("key column '{key_column}' not found"))?;
                    (values, key_idx)
                };

                // Upsert requires the `on` column to be unique — otherwise
                // there is no well-defined row to overwrite and a plain
                // insert could silently create duplicate keys.
                if self.catalog.is_index_unique(table, key_column) != Some(true) {
                    return Err(QueryError::Execution(format!(
                        "upsert on .{key_column} requires a unique column (declare it with \
                         `unique {key_column}: <type>` or `alter {table} add unique .{key_column}`)"
                    )));
                }

                let key_value = values[key_idx].clone();

                // Probe the unique index for a conflict.
                let existing = {
                    let tbl = self
                        .catalog
                        .get_table(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                    // The key column is guaranteed unique above, so this
                    // returns at most one matching row.
                    let rids = tbl.index_lookup_all(key_column, &key_value);
                    // Overflow safety (P0-3): reassemble via `tbl.get` so an
                    // upsert conflict row with a spilled column is read in full.
                    rids.into_iter()
                        .next()
                        .and_then(|rid| tbl.get(rid).map(|row| (rid, row)))
                };

                if let Some((rid, mut existing_row)) = existing {
                    // Conflict: apply on_conflict assignments (or all non-key if empty).
                    let update_assignments = if on_conflict.is_empty() {
                        assignments
                    } else {
                        on_conflict
                    };
                    let changed_cols: Vec<usize> = {
                        let schema = self
                            .catalog
                            .schema(table)
                            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                        let mut indices = Vec::new();
                        for a in update_assignments {
                            let idx = schema.column_index(&a.field).ok_or_else(|| {
                                QueryError::ColumnNotFound {
                                    table: String::new(),
                                    column: a.field.clone(),
                                }
                            })?;
                            if idx != key_idx {
                                // Coerce to the target column type, same as the
                                // UPDATE and INSERT paths — an int→float literal
                                // here would otherwise persist as raw i64 bits
                                // (#118 corruption on the upsert conflict path).
                                existing_row[idx] =
                                    coerce_value(literal_to_value(&a.value)?, &schema.columns[idx])
                                        .map_err(QueryError::TypeError)?;
                                indices.push(idx);
                            }
                        }
                        indices
                    };
                    self.catalog
                        .update_hinted(table, rid, &existing_row, Some(&changed_cols))
                        .map_err(QueryError::from_storage_io)?;
                    self.view_registry
                        .mark_dependents_dirty(table)
                        .map_err(QueryError::from_storage_io)?;
                    Ok(QueryResult::Modified(1))
                } else {
                    // No conflict: insert. This branch creates a row, so it
                    // owes that row the same `auto` ids a plain `insert` would
                    // give it. Skipping this wrote Value::Empty into the column
                    // the user declared `unique auto`, and because several
                    // NULLs coexist happily in a unique index nothing rejected
                    // it: repeated upserts silently accumulated rows with a
                    // NULL primary key.
                    //
                    // After the conflict probe, not before: the probe has to
                    // look up the key the caller supplied, and assigning first
                    // would hand a freshly minted id to a lookup that then
                    // matches nothing and inserts a duplicate. The conflict
                    // branch above deliberately does not assign — an existing
                    // row keeps the key it already has.
                    self.catalog
                        .assign_auto_columns(table, &mut values)
                        .map_err(QueryError::from_storage_io)?;
                    self.catalog
                        .insert(table, &values)
                        .map_err(QueryError::from_storage_io)?;
                    self.view_registry
                        .mark_dependents_dirty(table)
                        .map_err(QueryError::from_storage_io)?;
                    Ok(QueryResult::Modified(1))
                }
            }

            PlanNode::Update {
                input,
                table,
                assignments,
                returning,
            } => {
                // Mission C Phase 3: resolve assignments against a borrowed
                // schema, then drop the borrow before the mutation loop.
                // Try literal-only path first; fall back to per-row expression
                // evaluation if any assignment contains a non-literal expression
                // (e.g., `age := .age + 1`).
                let (col_indices, literal_vals, target_cols): (
                    Vec<usize>,
                    Option<Vec<Value>>,
                    Vec<ColumnDef>,
                ) = {
                    let schema_ref = self
                        .catalog
                        .schema(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                    let indices: Vec<usize> = assignments
                        .iter()
                        .map(|a| {
                            schema_ref.column_index(&a.field).ok_or_else(|| {
                                QueryError::ColumnNotFound {
                                    table: String::new(),
                                    column: a.field.clone(),
                                }
                            })
                        })
                        .collect::<Result<_, _>>()?;
                    // The target column defs (aligned with `assignments`), owned
                    // so the per-row expression path can coerce without holding a
                    // catalog borrow across the mutation loop.
                    let target_cols: Vec<ColumnDef> = indices
                        .iter()
                        .map(|&idx| schema_ref.columns[idx].clone())
                        .collect();
                    // Resolve each assignment to a literal value. If any is a
                    // non-literal expression, fall back (None) to the per-row
                    // expression-eval path below.
                    let raw_vals: Result<Vec<Value>, _> = assignments
                        .iter()
                        .map(|a| literal_to_value(&a.value))
                        .collect();
                    // Coerce each literal to its target column's declared type
                    // before it can reach the byte-patch fast path (the same
                    // coercion the INSERT path applies). Without this, an int
                    // assigned to a float column is written as raw i64 bits
                    // (#118 silent corruption) and a str assigned to a
                    // fixed-size column reaches `unreachable!` and aborts the
                    // whole server (#117 remote DoS). A genuine type mismatch
                    // is a hard error to the client, not an expr-path fallback.
                    let coerced = match raw_vals {
                        Ok(raws) => {
                            let mut out = Vec::with_capacity(raws.len());
                            for (raw, &idx) in raws.into_iter().zip(indices.iter()) {
                                out.push(
                                    coerce_value(raw, &schema_ref.columns[idx])
                                        .map_err(QueryError::TypeError)?,
                                );
                            }
                            Some(out)
                        }
                        Err(_) => None,
                    };
                    (indices, coerced, target_cols)
                };
                let resolved_assignments: Option<Vec<(usize, Value)>> =
                    literal_vals.map(|vals| col_indices.iter().copied().zip(vals).collect());

                // Mission C Phase 2: the hint Table::update_hinted needs to
                // decide whether to read the old row for index diff.
                let changed_cols: Vec<usize> = col_indices.clone();

                // ── RETURNING path ──────────────────────────────────────
                // `returning` materializes the post-update row image, so the
                // byte-patch / fused fast paths (which never decode a row)
                // can't serve it. Take the generic decode→mutate→collect
                // route. Opt-in only: when `returning` is false every path
                // below is byte-for-byte unchanged.
                if *returning {
                    let columns: Vec<String> = {
                        let schema_ref = self
                            .catalog
                            .schema(table)
                            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                        schema_ref.columns.iter().map(|c| c.name.clone()).collect()
                    };
                    let matching_rids = self.collect_rids_for_mutation(input, table)?;
                    let mut out_rows: Vec<Vec<Value>> = Vec::with_capacity(matching_rids.len());
                    // Cancellation is safe while collecting the target set, but
                    // once row writes start this executor has no statement-level
                    // savepoint. Check at the mutation boundary and then apply the
                    // full set without mid-loop cancellation; returning an error
                    // after a logged prefix would violate statement atomicity and
                    // is especially unsafe inside an explicit transaction.
                    crate::cancel::check()?;
                    for rid in matching_rids {
                        let mut row = match self.catalog.get(table, rid) {
                            Some(r) => r,
                            None => continue,
                        };
                        match &resolved_assignments {
                            // Literal path: apply the pre-coerced values.
                            Some(resolved) => {
                                for (idx, val) in resolved.iter() {
                                    row[*idx] = val.clone();
                                }
                            }
                            // Expression path: evaluate each RHS against the
                            // (progressively mutated) row, then coerce to the
                            // target column type before writing — same guard the
                            // literal path gets, matching the non-returning expr
                            // path exactly (#117/#118 on computed assignments).
                            None => {
                                for (i, asgn) in assignments.iter().enumerate() {
                                    let val = eval_expr(&asgn.value, &row, &columns);
                                    row[col_indices[i]] = coerce_value(val, &target_cols[i])
                                        .map_err(QueryError::TypeError)?;
                                }
                            }
                        }
                        self.catalog
                            .update_hinted(table, rid, &row, Some(&changed_cols))
                            .map_err(QueryError::from_storage_io)?;
                        out_rows.push(row);
                    }
                    self.view_registry
                        .mark_dependents_dirty(table)
                        .map_err(QueryError::from_storage_io)?;
                    return Ok(QueryResult::Rows {
                        columns,
                        rows: out_rows,
                    });
                }

                // ── Fused scan+update for Update(Filter(SeqScan)) ────────
                // Perf sprint: instead of the two-pass collect-RIDs-then-loop
                // pattern (which pays one ensure_hot per matched row on the
                // second pass), fuse the predicate evaluation and in-place
                // byte-level mutation into a single heap walk. Same idea as
                // the fused scan_delete_matching path for deletes.
                if let Some(ref resolved_assignments) = resolved_assignments {
                    if let PlanNode::Filter {
                        input: inner,
                        predicate,
                    } = input.as_ref()
                    {
                        if let PlanNode::SeqScan { table: t } = inner.as_ref() {
                            if t == table {
                                // The fused primitive mutates during its scan and
                                // cannot roll back a cancelled prefix. Honor an
                                // already-triggered token before entering it, then
                                // let the primitive finish atomically from the
                                // query layer's perspective.
                                crate::cancel::check()?;
                                let fused_result = self.try_fused_scan_update(
                                    table,
                                    predicate,
                                    resolved_assignments,
                                    &changed_cols,
                                );
                                if let Some(result) = fused_result {
                                    return result;
                                }
                            }
                        }
                    }
                }

                // Collect matching RowIds in a single pass.
                let matching_rids = self.collect_rids_for_mutation(input, table)?;
                // This is the last cancellable boundary before any row is
                // changed. Mutation loops below deliberately do not poll.
                crate::cancel::check()?;

                // ── Literal-only fast paths ─────────────────────────────
                if let Some(ref resolved_assignments) = resolved_assignments {
                    // Mission C Phase 4: in-place byte-patch fast path. If every
                    // assignment targets a fixed-size non-null column AND none of
                    // them is indexed, we can skip decode_row / Vec<Value> /
                    // encode_row_into entirely and patch the row's raw bytes on
                    // the hot page.
                    let fast_patch: Option<Vec<FastPatch>> = if self
                        .generic_path_forced("update-byte-patch")
                    {
                        None
                    } else {
                        let tbl = self
                            .catalog
                            .get_table(table)
                            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                        let schema = tbl.schema();
                        // Overflow safety (P0): byte-patching a v2 row with v1
                        // offsets corrupts it. Overflow tables take the generic
                        // reassembling `get` + `update_hinted` path below.
                        let all_fixed_nonnull = !tbl.has_overflow_rows()
                            && resolved_assignments.iter().all(|(idx, val)| {
                                is_fixed_size(schema.columns[*idx].type_id) && !val.is_empty()
                            });
                        let no_indexed = !resolved_assignments
                            .iter()
                            .any(|(idx, _)| tbl.has_indexed_col(*idx));

                        if all_fixed_nonnull && no_indexed {
                            let layout = RowLayout::new(schema);
                            let bitmap_size = layout.bitmap_size();
                            let patches: Vec<FastPatch> = resolved_assignments
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
                                        _ => unreachable!("all_fixed_nonnull guard lied"),
                                    };
                                    FastPatch {
                                        field_off,
                                        bitmap_byte_off: 2 + idx / 8,
                                        bit_mask: 1u8 << (idx % 8),
                                        bytes,
                                    }
                                })
                                .collect();
                            Some(patches)
                        } else {
                            None
                        }
                    };

                    if let Some(patches) = fast_patch {
                        let mut count = 0u64;
                        let mut fallback_rids: Vec<RowId> = Vec::new();
                        for rid in &matching_rids {
                            // Mission B2: WAL-log every patch so crash
                            // recovery replays the update. Same mutation
                            // closure as before — the wrapper just sandwiches
                            // it between a hot-page read and a WAL append.
                            //
                            // A false return means the byte-patch was refused
                            // (e.g. a v2/overflow row whose in-place layout the
                            // fast path cannot compute, reachable on a legacy
                            // heap where has_overflow_rows() under-reports). Do
                            // NOT drop the row: push it to `fallback_rids` and
                            // let the reassembling get + update_hinted path
                            // apply it, mirroring the var-column fast path
                            // below. The fast path is thus a pure optimization
                            // that can never silently lose an update.
                            let ok = self
                                .catalog
                                .update_row_bytes_logged(table, *rid, |row| {
                                    let base = row_body_base(row);
                                    for p in &patches {
                                        row[base + p.bitmap_byte_off] &= !p.bit_mask;
                                        let field_bytes = p.bytes.as_slice();
                                        row[base + p.field_off
                                            ..base + p.field_off + field_bytes.len()]
                                            .copy_from_slice(field_bytes);
                                    }
                                })
                                .map_err(QueryError::from_storage_io)?;
                            if ok {
                                count += 1;
                            } else {
                                fallback_rids.push(*rid);
                            }
                        }
                        for rid in fallback_rids {
                            let mut row = match self.catalog.get(table, rid) {
                                Some(r) => r,
                                None => continue,
                            };
                            for (idx, val) in resolved_assignments.iter() {
                                row[*idx] = val.clone();
                            }
                            self.catalog
                                .update_hinted(table, rid, &row, Some(&changed_cols))
                                .map_err(QueryError::from_storage_io)?;
                            count += 1;
                        }
                        self.view_registry
                            .mark_dependents_dirty(table)
                            .map_err(QueryError::from_storage_io)?;
                        return Ok(QueryResult::Modified(count));
                    }

                    // Mission C Phase 10: var-column in-place shrink fast path.
                    let var_fast: Option<(usize, Option<Vec<u8>>)> = if self
                        .generic_path_forced("update-var-shrink")
                    {
                        None
                    } else {
                        let tbl = self
                            .catalog
                            .get_table(table)
                            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                        let schema = tbl.schema();
                        // Overflow safety (P0/P0-2): the in-place var shrink
                        // patch computes v1 offsets — never on a v2-capable
                        // table. Falls through to the reassembling path.
                        let is_single = resolved_assignments.len() == 1 && !tbl.has_overflow_rows();
                        let is_var_col = is_single
                            && !is_fixed_size(schema.columns[resolved_assignments[0].0].type_id);
                        let no_indexed = !resolved_assignments
                            .iter()
                            .any(|(idx, _)| tbl.has_indexed_col(*idx));

                        if is_single && is_var_col && no_indexed {
                            let (idx, val) = &resolved_assignments[0];
                            let bytes_opt: Option<Vec<u8>> = match val {
                                Value::Str(s) => Some(s.as_bytes().to_vec()),
                                Value::Bytes(b) => Some(b.clone()),
                                // A json column stores its PJ1 bytes as the var
                                // payload (u32 length prefix + bytes, like Bytes),
                                // so the in-place patch writes them verbatim.
                                Value::Json(b) => Some(b.to_vec()),
                                Value::Empty => None,
                                _ => {
                                    return Err(QueryError::TypeError(format!(
                                        "cannot assign non-var value to var column '{}'",
                                        schema.columns[*idx].name
                                    )))
                                }
                            };
                            Some((*idx, bytes_opt))
                        } else {
                            None
                        }
                    };

                    if let Some((col_idx, new_bytes_opt)) = var_fast {
                        let new_bytes_ref: Option<&[u8]> = new_bytes_opt.as_deref();
                        let mut count = 0u64;
                        let mut fallback_rids: Vec<RowId> = Vec::new();
                        for rid in &matching_rids {
                            // Mission B2: logged variant so crash recovery
                            // replays the shrink. On a false return (row
                            // would have to grow), the rid is pushed to
                            // `fallback_rids` and the slower `update_hinted`
                            // path — which is already WAL-logged — picks it up.
                            let ok = self
                                .catalog
                                .patch_var_col_logged(table, *rid, col_idx, new_bytes_ref)
                                .map_err(QueryError::from_storage_io)?;
                            if ok {
                                count += 1;
                            } else {
                                fallback_rids.push(*rid);
                            }
                        }
                        for rid in fallback_rids {
                            let mut row = match self.catalog.get(table, rid) {
                                Some(r) => r,
                                None => continue,
                            };
                            for (idx, val) in resolved_assignments.iter() {
                                row[*idx] = val.clone();
                            }
                            self.catalog
                                .update_hinted(table, rid, &row, Some(&changed_cols))
                                .map_err(QueryError::from_storage_io)?;
                            count += 1;
                        }
                        self.view_registry
                            .mark_dependents_dirty(table)
                            .map_err(QueryError::from_storage_io)?;
                        return Ok(QueryResult::Modified(count));
                    }

                    // Generic literal path: decode row, apply literal values.
                    let mut count = 0u64;
                    for rid in matching_rids {
                        let mut row = match self.catalog.get(table, rid) {
                            Some(r) => r,
                            None => continue,
                        };
                        for (idx, val) in resolved_assignments.iter() {
                            row[*idx] = val.clone();
                        }
                        self.catalog
                            .update_hinted(table, rid, &row, Some(&changed_cols))
                            .map_err(QueryError::from_storage_io)?;
                        count += 1;
                    }
                    self.view_registry
                        .mark_dependents_dirty(table)
                        .map_err(QueryError::from_storage_io)?;
                    return Ok(QueryResult::Modified(count));
                } // end if let Some(resolved_assignments)

                // ── Expression-based update path ────────────────────────
                // At least one assignment contains a non-literal expression
                // (e.g., `age := .age + 1`). Evaluate per-row.
                let col_names: Vec<String> = {
                    let schema_ref = self
                        .catalog
                        .schema(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                    schema_ref.columns.iter().map(|c| c.name.clone()).collect()
                };
                let mut count = 0u64;
                for rid in matching_rids {
                    let mut row = match self.catalog.get(table, rid) {
                        Some(r) => r,
                        None => continue,
                    };
                    for (i, asgn) in assignments.iter().enumerate() {
                        let val = eval_expr(&asgn.value, &row, &col_names);
                        // Coerce to the target column type before writing, so a
                        // computed int→float assignment stores f64 (not raw i64
                        // bits, #118) and a str→fixed-col assignment returns a
                        // typed error instead of hitting the encoder's
                        // `unreachable!` and aborting the process (#117).
                        row[col_indices[i]] =
                            coerce_value(val, &target_cols[i]).map_err(QueryError::TypeError)?;
                    }
                    self.catalog
                        .update_hinted(table, rid, &row, Some(&changed_cols))
                        .map_err(QueryError::from_storage_io)?;
                    count += 1;
                }
                self.view_registry
                    .mark_dependents_dirty(table)
                    .map_err(QueryError::from_storage_io)?;
                Ok(QueryResult::Modified(count))
            }

            PlanNode::Delete {
                input,
                table,
                returning,
            } => {
                // ── RETURNING path ──────────────────────────────────────
                // `returning` needs the pre-delete row image, so read each
                // matched row before removing it. The fused single-pass
                // delete primitives below never decode rows, so they can't
                // serve this. Opt-in only: when `returning` is false the
                // fast paths below are byte-for-byte unchanged.
                if *returning {
                    let columns: Vec<String> = {
                        let schema_ref = self
                            .catalog
                            .schema(table)
                            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                        schema_ref.columns.iter().map(|c| c.name.clone()).collect()
                    };
                    let matching_rids = self.collect_rids_for_mutation(input, table)?;
                    let mut out_rows: Vec<Vec<Value>> = Vec::with_capacity(matching_rids.len());
                    // Cooperative cancellation of the pre-delete image read. The
                    // actual removal below is a single batched `delete_many`, so
                    // cancelling here happens before any row is deleted.
                    let mut cancel = CancelCheck::new();
                    for rid in &matching_rids {
                        cancel.tick()?;
                        if let Some(row) = self.catalog.get(table, *rid) {
                            out_rows.push(row);
                        }
                    }
                    crate::cancel::check()?;
                    self.catalog
                        .delete_many(table, &matching_rids)
                        .map_err(QueryError::from_storage_io)?;
                    self.view_registry
                        .mark_dependents_dirty(table)
                        .map_err(QueryError::from_storage_io)?;
                    return Ok(QueryResult::Rows {
                        columns,
                        rows: out_rows,
                    });
                }

                // Mission C Phase 3: no schema clone — collect_rids_for_mutation
                // looks up schema internally when it needs one, and the mutation
                // loop doesn't need the schema at all.
                //
                // Mission C Phase 12: route bulk deletes through
                // `Catalog::delete_many`, which batches the btree leaf
                // compaction and shares one `ensure_hot` per row between
                // the index-key extraction and the slot delete. On
                // `delete_by_filter` (100K fixture, ~20K matches) that
                // removes ~4ms of pure `Vec::remove` memmove from the btree
                // maintenance phase.
                //
                // Mission C Phase 16: for the common `delete where ...`
                // shape (Filter(SeqScan)) — and the rarer "delete
                // everything" shape (SeqScan) — skip the two-pass
                // `collect_rids_for_mutation` + `delete_many` flow entirely.
                // The fused `scan_delete_matching` primitive walks the
                // heap exactly once, paying one `ensure_hot` per page
                // instead of per-row. That closes the last major gap on
                // the bench's `delete_by_filter` workload.
                // Overflow safety (P1): a v2-capable table cannot take the fused
                // raw-byte delete — the compiled predicate mis-reads spilled
                // columns. Route it through the reassembling collect-rids path.
                let skip_fused_delete = self.catalog.table_has_overflow(table)
                    || self.generic_path_forced("delete-fused");
                if let PlanNode::Filter {
                    input: inner,
                    predicate,
                } = input.as_ref()
                {
                    if let PlanNode::SeqScan { table: t } = inner.as_ref() {
                        if t == table && !skip_fused_delete {
                            let schema = self
                                .catalog
                                .schema(table)
                                .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                            let columns: Vec<String> =
                                schema.columns.iter().map(|c| c.name.clone()).collect();
                            let fast = FastLayout::new(schema);
                            if let Some(compiled) = self.compile_predicate_unless_forced(
                                "delete-fused:predicate",
                                predicate,
                                &columns,
                                &fast,
                                schema,
                            ) {
                                // Mission B2: logged variant so every
                                // matched rid hits the WAL during the
                                // single-pass scan. Structure of the
                                // fused scan is unchanged — only the
                                // hook closure now also appends.
                                crate::cancel::check()?;
                                let count = self
                                    .catalog
                                    .scan_delete_matching_logged(table, |data| compiled(data))
                                    .map_err(QueryError::from_storage_io)?;
                                self.view_registry
                                    .mark_dependents_dirty(table)
                                    .map_err(QueryError::from_storage_io)?;
                                return Ok(QueryResult::Modified(count));
                            }
                        }
                    }
                } else if let PlanNode::SeqScan { table: t } = input.as_ref() {
                    if t == table && !skip_fused_delete {
                        // `delete from T` with no predicate — every live
                        // row matches. One pass is still the right shape.
                        // Mission B2: logged variant — see above.
                        crate::cancel::check()?;
                        let count = self
                            .catalog
                            .scan_delete_matching_logged(table, |_| true)
                            .map_err(QueryError::from_storage_io)?;
                        self.view_registry
                            .mark_dependents_dirty(table)
                            .map_err(QueryError::from_storage_io)?;
                        return Ok(QueryResult::Modified(count));
                    }
                }

                let matching_rids = self.collect_rids_for_mutation(input, table)?;
                crate::cancel::check()?;
                let count = self
                    .catalog
                    .delete_many(table, &matching_rids)
                    .map_err(QueryError::from_storage_io)?;
                self.view_registry
                    .mark_dependents_dirty(table)
                    .map_err(QueryError::from_storage_io)?;
                Ok(QueryResult::Modified(count))
            }

            PlanNode::NestedProject { input, fields } => {
                // Resolve link traversals against the persistent catalog before
                // anything else, so child tables are concrete for the
                // dirty-view refresh and the assembly below.
                let resolved;
                let fields: &[NestedProjectField] = if nested_fields_have_via_link(fields) {
                    let outer = scan_source_table(input).ok_or_else(|| {
                        QueryError::Execution(
                            "link traversal requires a plain aliased table scan as its parent"
                                .into(),
                        )
                    })?;
                    resolved = self.resolve_nested_via_links(fields, outer)?;
                    &resolved
                } else {
                    fields
                };
                // Auto-refresh dirty materialized views among the child
                // tables (at every nesting level) before the read-only
                // assembly runs.
                let mut child_tables = Vec::new();
                for field in fields {
                    if let NestedProjectField::Nested(nested) = field {
                        nested.visit_tables(&mut |table| child_tables.push(table.to_string()));
                    }
                }
                for table in child_tables {
                    if self.view_registry.is_dirty(&table) {
                        self.refresh_view(&table)?;
                    }
                }
                let parent = self.dispatch_mut(input)?;
                self.execute_nested_project(parent, fields)
            }

            PlanNode::AliasScan { table, alias } => {
                // Mission E1.2: scan `table` and rename every output column
                // to `alias.field`. Used as a join leaf so downstream
                // NestedLoopJoin + Filter + Project nodes can resolve
                // `Expr::QualifiedField` lookups by direct column-name match.
                //
                // We don't bother with a fused zero-copy loop here yet — the
                // whole join path is nested-loop and correctness-first
                // (Phase E1.3 will introduce hash join and at that point we
                // can revisit whether to specialise AliasScan).
                let schema = self
                    .catalog
                    .schema(table)
                    .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
                    .clone();
                let columns: Vec<String> = schema
                    .columns
                    .iter()
                    .map(|c| format!("{alias}.{}", c.name))
                    .collect();
                let mut cancel = CancelCheck::new();
                let mut rows: Vec<Vec<Value>> = Vec::new();
                for item in self
                    .catalog
                    .scan(table)
                    .map_err(QueryError::from_storage_io)?
                {
                    let (_, row) = item.map_err(QueryError::from_storage_io)?;
                    cancel.tick()?;
                    rows.push(row);
                }
                Ok(QueryResult::Rows { columns, rows })
            }

            PlanNode::NestedLoopJoin {
                left,
                right,
                on,
                kind,
            } => {
                // Materialise both sides. The executor ships two strategies:
                //   1. Hash join (E1.3) — when the `on` predicate is a
                //      simple equi-predicate `left_col = right_col`, build a
                //      FxHashMap<Value, Vec<row_idx>> over the right side
                //      and probe with the left side. O(L + R) instead of
                //      O(L × R). Handles Inner and LeftOuter.
                //   2. Nested loop (E1.2) — fallback for Cross, non-equi
                //      predicates, or `on` expressions that reference
                //      either side with something more complex than a
                //      QualifiedField.
                let left_result = self.dispatch_mut(left)?;
                let right_result = self.dispatch_mut(right)?;
                let (left_columns, left_rows) = match left_result {
                    QueryResult::Rows { columns, rows } => (columns, rows),
                    _ => return Err("join left side must produce rows".into()),
                };
                let (right_columns, right_rows) = match right_result {
                    QueryResult::Rows { columns, rows } => (columns, rows),
                    _ => return Err("join right side must produce rows".into()),
                };

                // WS2: byte-budget guard on the join build side. Charge both
                // materialized inputs before we build the hash table / probe;
                // the output is row-capped by check_join_limit below.
                self.charge_rows(&left_rows)?;
                self.charge_rows(&right_rows)?;

                execute_materialized_join(
                    left_columns,
                    left_rows,
                    right_columns,
                    right_rows,
                    on.as_ref(),
                    *kind,
                    self.nested_loop_pair_limit,
                )
            }

            PlanNode::Distinct { input } => {
                let result = self.dispatch_mut(input)?;
                match result {
                    QueryResult::Rows { columns, rows } => {
                        let mut seen = std::collections::HashSet::new();
                        let mut unique_rows = Vec::new();
                        let mut cancel = CancelCheck::new();
                        for row in rows {
                            cancel.tick()?;
                            if seen.insert(row.clone()) {
                                unique_rows.push(row);
                            }
                        }
                        Ok(QueryResult::Rows {
                            columns,
                            rows: unique_rows,
                        })
                    }
                    other => Ok(other),
                }
            }

            PlanNode::GroupBy {
                input,
                keys,
                aggregates,
                having,
            } => {
                if aggregates
                    .iter()
                    .any(|aggregate| aggregate.provenance_alias.is_some())
                {
                    let input = self.materialize_rows_with_provenance(input)?;
                    self.charge_rows(&input.rows)?;
                    return exec_group_by_with_provenance(
                        input,
                        keys,
                        aggregates,
                        having,
                        self.query_memory_limit(),
                    );
                }
                let result = self.dispatch_mut(input)?;
                match result {
                    QueryResult::Rows { columns, rows } => {
                        // WS2: byte-budget guard on the GROUP BY input buffer
                        // (the hash table is bounded by the input it groups).
                        self.charge_rows(&rows)?;
                        exec_group_by(columns, rows, keys, aggregates, having)
                    }
                    _ => Err("group by requires row input".into()),
                }
            }

            PlanNode::CreateTable {
                name,
                fields,
                if_not_exists,
            } => {
                // Idempotency: a re-declared type is a clean no-op under
                // `if not exists`, and otherwise a PowQL-flavored error that
                // names the type (not the storage layer's generic "table").
                if self.catalog.schema(name).is_some() {
                    if *if_not_exists {
                        return Ok(QueryResult::Executed {
                            message: format!("type '{name}' already exists (skipped)"),
                        });
                    }
                    // "cannot" prefix keeps this on the server's
                    // safe-to-forward allowlist (SAFE_ERROR_PREFIXES).
                    return Err(QueryError::Execution(format!(
                        "cannot create type '{name}': it already exists"
                    )));
                }
                let columns: Vec<ColumnDef> = fields
                    .iter()
                    .enumerate()
                    .map(|(i, f)| -> Result<ColumnDef, QueryError> {
                        Ok(ColumnDef {
                            name: f.name.clone(),
                            type_id: type_name_to_id(&f.type_name)
                                .map_err(QueryError::TypeError)?,
                            required: f.required,
                            position: i as u16,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                // Coerce each literal default to its column's type now, so a
                // type mismatch (`count: int default "x"`) is rejected at DDL
                // time and the stored default is ready to drop into inserts.
                let mut defaults: Vec<Option<Value>> = vec![None; columns.len()];
                let mut auto_cols: Vec<bool> = vec![false; columns.len()];
                for (i, f) in fields.iter().enumerate() {
                    if let Some(lit) = &f.default {
                        let raw = literal_value_from(lit);
                        defaults[i] = Some(coerce_value(raw, &columns[i])?);
                    }
                    if f.auto {
                        // Auto-increment only makes sense on an integer column,
                        // and combining it with a literal default is
                        // contradictory (both want to supply the value).
                        if columns[i].type_id != TypeId::Int {
                            return Err(QueryError::TypeError(format!(
                                "auto column '{}' must be of type int",
                                f.name
                            )));
                        }
                        if f.default.is_some() {
                            return Err(QueryError::TypeError(format!(
                                "auto column '{}' cannot also declare a default",
                                f.name
                            )));
                        }
                        auto_cols[i] = true;
                    }
                }
                let schema = Schema {
                    table_name: name.clone(),
                    columns,
                };
                self.catalog
                    .create_table_full(schema, defaults, auto_cols)
                    .map_err(QueryError::from_storage_io)?;
                // Declaring a field `unique` auto-creates a unique B+tree
                // index, which is where uniqueness is enforced on writes.
                for f in fields.iter().filter(|f| f.unique) {
                    self.catalog
                        .create_index_unique(name, &f.name, true)
                        .map_err(QueryError::from_storage_io)?;
                }
                Ok(QueryResult::Created(name.clone()))
            }

            PlanNode::CreateLink {
                owner,
                name,
                target,
                local_key,
                target_key,
            } => {
                self.create_link_from_parts(owner, name, target, local_key, target_key)?;
                Ok(QueryResult::Executed {
                    message: format!("link '{name}' added to '{owner}'"),
                })
            }

            PlanNode::AlterTable { table, action } => match action {
                AlterAction::AddColumn {
                    name,
                    type_name,
                    required,
                } => {
                    let position = self
                        .catalog
                        .schema(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
                        .columns
                        .len() as u16;
                    let col = ColumnDef {
                        name: name.clone(),
                        type_id: type_name_to_id(type_name).map_err(QueryError::TypeError)?,
                        required: *required,
                        position,
                    };
                    self.catalog
                        .alter_table_add_column(table, col)
                        .map_err(QueryError::from_storage_io)?;
                    Ok(QueryResult::Executed {
                        message: format!("column '{name}' added to '{table}'"),
                    })
                }
                AlterAction::DropColumn { name, if_exists } => {
                    // `if exists`: a missing column (or missing table) is a
                    // no-op instead of an error.
                    if *if_exists {
                        let present = self
                            .catalog
                            .schema(table)
                            .map(|s| s.column_index(name).is_some())
                            .unwrap_or(false);
                        if !present {
                            return Ok(QueryResult::Executed {
                                message: format!(
                                    "column '{name}' does not exist on '{table}' (skipped)"
                                ),
                            });
                        }
                    }
                    self.catalog
                        .alter_table_drop_column(table, name)
                        .map_err(QueryError::from_storage_io)?;
                    Ok(QueryResult::Executed {
                        message: format!("column '{name}' dropped from '{table}'"),
                    })
                }
                AlterAction::AddIndex {
                    target,
                    if_not_exists: _,
                } => {
                    let IndexTarget::Column(column) = target else {
                        let IndexTarget::JsonPath(path) = target else {
                            unreachable!("index target variants are exhaustive")
                        };
                        if let Some(existing) = resolve_expression_index(&self.catalog, table, path)
                        {
                            return Ok(QueryResult::Executed {
                                message: format!(
                                    "expression index {} on '{}' already exists (skipped)",
                                    existing.index_id, table
                                ),
                            });
                        }
                        crate::cancel::check()?;
                        let index_id = self
                            .catalog
                            .create_expression_index_metadata(
                                table,
                                1,
                                path.canonical_text(),
                                path.clone(),
                                false,
                            )
                            .map_err(QueryError::from_storage_io)?;
                        return Ok(QueryResult::Executed {
                            message: format!("expression index {index_id} on '{}' created", table),
                        });
                    };
                    // `add index` is already idempotent (no-op if the index
                    // exists), so `if not exists` is accepted for symmetry but
                    // does not change behavior.
                    crate::cancel::check()?;
                    self.catalog
                        .create_index(table, column)
                        .map_err(QueryError::from_storage_io)?;
                    Ok(QueryResult::Executed {
                        message: format!("index on '{table}.{column}' created"),
                    })
                }
                AlterAction::AddUnique {
                    target,
                    if_not_exists,
                } => {
                    let IndexTarget::Column(column) = target else {
                        let IndexTarget::JsonPath(path) = target else {
                            unreachable!("index target variants are exhaustive")
                        };
                        if let Some(existing) = resolve_expression_index(&self.catalog, table, path)
                        {
                            if *if_not_exists {
                                return Ok(QueryResult::Executed {
                                    message: format!(
                                        "expression index {} on '{}' already exists (skipped)",
                                        existing.index_id, table
                                    ),
                                });
                            }
                            return Err(QueryError::Execution(format!(
                                "cannot add unique expression index on {}: path already indexed",
                                table
                            )));
                        }
                        crate::cancel::check()?;
                        let index_id = self
                            .catalog
                            .create_expression_index_metadata(
                                table,
                                1,
                                path.canonical_text(),
                                path.clone(),
                                true,
                            )
                            .map_err(QueryError::from_storage_io)?;
                        return Ok(QueryResult::Executed {
                            message: format!(
                                "unique expression index {index_id} on '{}' created",
                                table
                            ),
                        });
                    };
                    // `if not exists`: an already-indexed column is a no-op
                    // rather than the (default) "already indexed" error.
                    if self.catalog.has_index(table, column) {
                        if *if_not_exists {
                            return Ok(QueryResult::Executed {
                                message: format!(
                                    "index on '{table}.{column}' already exists (skipped)"
                                ),
                            });
                        }
                        // Upgrading an existing non-unique index in place is
                        // intentionally rejected.
                        return Err(QueryError::Execution(format!(
                            "cannot add unique on {table}.{column}: column already indexed"
                        )));
                    }
                    // Scan existing rows for duplicate (non-null) values
                    // before creating the unique index.
                    {
                        let tbl = self
                            .catalog
                            .get_table(table)
                            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                        let col_idx = tbl.schema().column_index(column).ok_or_else(|| {
                            QueryError::ColumnNotFound {
                                table: table.to_string(),
                                column: column.clone(),
                            }
                        })?;
                        let mut seen = std::collections::HashSet::new();
                        let mut cancel = CancelCheck::new();
                        for item in tbl.scan() {
                            let (_, row) = item.map_err(QueryError::from_storage_io)?;
                            cancel.tick()?;
                            let v = &row[col_idx];
                            if v.is_empty() {
                                continue;
                            }
                            if !seen.insert(v.clone()) {
                                return Err(QueryError::Execution(format!(
                                    "cannot add unique on {table}.{column}: \
                                     duplicate value {v:?} exists"
                                )));
                            }
                        }
                    }
                    crate::cancel::check()?;
                    self.catalog
                        .create_index_unique(table, column, true)
                        .map_err(QueryError::from_storage_io)?;
                    Ok(QueryResult::Executed {
                        message: format!("unique index on '{table}.{column}' created"),
                    })
                }
                AlterAction::DropIndex { target, if_exists } => {
                    let IndexTarget::JsonPath(path) = target else {
                        return Err(QueryError::Execution(
                            "dropping stored-column indexes is not supported".to_string(),
                        ));
                    };
                    let Some(existing) = resolve_expression_index(&self.catalog, table, path)
                    else {
                        if *if_exists {
                            return Ok(QueryResult::Executed {
                                message: format!(
                                    "expression index on '{}' does not exist (skipped)",
                                    table
                                ),
                            });
                        }
                        return Err(QueryError::Execution(format!(
                            "expression index on '{}' does not exist",
                            table
                        )));
                    };
                    crate::cancel::check()?;
                    self.catalog
                        .drop_expression_index(table, existing.index_id)
                        .map_err(QueryError::from_storage_io)?;
                    Ok(QueryResult::Executed {
                        message: format!(
                            "expression index {} on '{}' dropped",
                            existing.index_id, table
                        ),
                    })
                }
                AlterAction::AddLink {
                    name,
                    target,
                    local_key,
                    target_key,
                } => {
                    self.create_link_from_parts(table, name, target, local_key, target_key)?;
                    Ok(QueryResult::Executed {
                        message: format!("link '{name}' added to '{table}'"),
                    })
                }
            },

            PlanNode::DropTable { name, if_exists } => {
                if *if_exists && self.catalog.schema(name).is_none() {
                    return Ok(QueryResult::Executed {
                        message: format!("type '{name}' does not exist (skipped)"),
                    });
                }
                self.catalog
                    .drop_table(name)
                    .map_err(QueryError::from_storage_io)?;
                // Dropping a table invalidates every view built over it just
                // as surely as mutating one does, and more permanently: the
                // rows a materialized view holds are now the only copy of data
                // whose source is gone. Without this, reading such a view kept
                // answering from that orphaned copy while `refresh` on the same
                // view already failed with "table not found" — the read and the
                // refresh disagreed about whether the view was still valid.
                // Marking dependents dirty makes the read take the refresh
                // path, so both now report the missing source instead of one
                // silently serving it.
                let views_affected = self
                    .view_registry
                    .mark_dependents_dirty(name)
                    .map(|()| self.view_registry.dependents_of(name))
                    .map_err(QueryError::from_storage_io)?;
                let message = if views_affected.is_empty() {
                    format!("table '{name}' dropped")
                } else {
                    let (subject, verb) = describe_view_list(&views_affected);
                    format!(
                        "table '{name}' dropped; {subject} {verb} no source and \
                         will fail until dropped or recreated"
                    )
                };
                Ok(QueryResult::Executed { message })
            }

            PlanNode::ListTypes => self.introspect_list_types(),

            PlanNode::Describe { table } => self.introspect_describe(table),

            PlanNode::ListLinks => self.introspect_list_links(),

            PlanNode::CreateView { name, query_text } => {
                self.create_view(name, query_text)?;
                Ok(QueryResult::Executed {
                    message: format!("materialized view '{name}' created"),
                })
            }

            PlanNode::RefreshView { name } => {
                self.refresh_view(name)?;
                Ok(QueryResult::Executed {
                    message: format!("materialized view '{name}' refreshed"),
                })
            }

            PlanNode::DropView { name, if_exists } => {
                if *if_exists && !self.view_registry.is_view(name) {
                    return Ok(QueryResult::Executed {
                        message: format!("view '{name}' does not exist (skipped)"),
                    });
                }
                self.drop_view(name)?;
                Ok(QueryResult::Executed {
                    message: format!("materialized view '{name}' dropped"),
                })
            }

            PlanNode::Window { input, windows } => {
                let result = self.dispatch_mut(input)?;
                execute_window(result, windows, self.query_memory_limit)
            }

            PlanNode::Union { left, right, all } => {
                let left_result = self.dispatch_mut(left)?;
                let right_result = self.dispatch_mut(right)?;
                let (left_cols, left_rows) = match left_result {
                    QueryResult::Rows { columns, rows } => (columns, rows),
                    _ => return Err("UNION requires query results on left side".into()),
                };
                let (_, right_rows) = match right_result {
                    QueryResult::Rows { columns, rows } => (columns, rows),
                    _ => return Err("UNION requires query results on right side".into()),
                };
                let mut combined = left_rows;
                let mut cancel = CancelCheck::new();
                if *all {
                    // UNION ALL — just concatenate.
                    for row in right_rows {
                        cancel.tick()?;
                        combined.push(row);
                    }
                } else {
                    // UNION — deduplicate using the same HashSet approach
                    // as DISTINCT. Value already implements Hash + Eq.
                    let mut seen = std::collections::HashSet::new();
                    for row in &combined {
                        cancel.tick()?;
                        seen.insert(row.clone());
                    }
                    for row in right_rows {
                        cancel.tick()?;
                        if seen.insert(row.clone()) {
                            combined.push(row);
                        }
                    }
                }
                Ok(QueryResult::Rows {
                    columns: left_cols,
                    rows: combined,
                })
            }

            PlanNode::Explain { input } => {
                // Every execute entry point runs lower_unindexed_scans before
                // dispatch and lowering recurses into Explain, so `input` is
                // already the plan that will actually run.
                let text = format_plan_tree(&self.catalog, input, 0);
                Ok(QueryResult::Rows {
                    columns: vec!["plan".to_string()],
                    rows: text
                        .lines()
                        .map(|line| vec![Value::Str(line.to_string())])
                        .collect(),
                })
            }

            PlanNode::Begin => {
                if self.in_transaction {
                    return Err(QueryError::Execution(
                        "already in a transaction (nested transactions not supported)".into(),
                    ));
                }
                self.catalog
                    .begin_transaction()
                    .map_err(QueryError::from_storage_io)?;
                self.in_transaction = true;
                Ok(QueryResult::Executed {
                    message: "transaction started".to_string(),
                })
            }

            PlanNode::Commit => {
                if !self.in_transaction {
                    return Err(QueryError::Execution(
                        "no active transaction to commit".into(),
                    ));
                }
                self.catalog
                    .commit_transaction()
                    .map_err(QueryError::from_storage_io)?;
                self.in_transaction = false;
                Ok(QueryResult::Executed {
                    message: "transaction committed".to_string(),
                })
            }

            PlanNode::Rollback => {
                if !self.in_transaction {
                    return Err(QueryError::Execution(
                        "no active transaction to roll back".into(),
                    ));
                }
                self.rollback_transaction_preserving_wal_archive()
            }

            PlanNode::IndexScan { table, column, key } => {
                let key_value = literal_to_value(key)?;
                let tbl = self
                    .catalog
                    .get_table(table)
                    .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                let columns: Vec<String> = tbl
                    .schema()
                    .columns
                    .iter()
                    .map(|c| c.name.clone())
                    .collect();

                // Fast path: the table has a B-tree on this column.
                // Uses index_lookup_all to return ALL matching rows for
                // both unique and non-unique indexes.
                if tbl.has_index(column) {
                    let rids = tbl.index_lookup_all(column, &key_value);
                    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(rids.len());
                    let mut cancel = CancelCheck::new();
                    for rid in rids {
                        cancel.tick()?;
                        // Overflow safety (P0-3/P0-4): `tbl.get` reassembles
                        // spilled columns; the old `heap.get` + `decode_row`
                        // returned Empty / wrapped a >= 64KB value.
                        if let Some(row) = tbl.get(rid) {
                            rows.push(row);
                        }
                    }
                    return Ok(QueryResult::Rows { columns, rows });
                }

                // Fallback: no index on this column. The planner emits IndexScan
                // eagerly (it has no visibility into which columns are indexed
                // at plan time), so here we must behave like SeqScan+Filter on
                // `.col = literal`: return *all* matching rows, not just the
                // first one. A non-indexed column isn't necessarily unique.
                // We compile the eq predicate once and stream without any
                // per-row decode for non-matching rows.
                let schema = tbl.schema();
                let fast = FastLayout::new(schema);
                let synth_pred = Expr::BinaryOp(
                    Box::new(Expr::Field(column.clone())),
                    BinOp::Eq,
                    Box::new(key.clone()),
                );
                // Overflow safety (P0-4/P1): the raw compiled scan drops/mis-reads
                // spilled columns; a v2-capable table uses the decoded scan below.
                if !tbl.has_overflow_rows() {
                    if let Some(compiled) = self.compile_predicate_unless_forced(
                        "index-scan-scan-fallback:predicate",
                        &synth_pred,
                        &columns,
                        &fast,
                        schema,
                    ) {
                        // Mission F: skip the first 4 Vec doublings.
                        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(64);
                        for_each_row_raw_cancellable(&self.catalog, table, |_rid, data| {
                            if compiled(data) {
                                rows.push(decode_row(schema, data));
                            }
                        })?;
                        return Ok(QueryResult::Rows { columns, rows });
                    }
                }

                // Last resort: slow eq-check on materialised rows.
                let col_idx =
                    schema
                        .column_index(column)
                        .ok_or_else(|| QueryError::ColumnNotFound {
                            table: String::new(),
                            column: column.clone(),
                        })?;
                let mut cancel = CancelCheck::new();
                let mut rows: Vec<Vec<Value>> = Vec::new();
                for item in tbl.scan() {
                    let (_, row) = item.map_err(QueryError::from_storage_io)?;
                    cancel.tick()?;
                    if row[col_idx] == key_value {
                        rows.push(row);
                    }
                }
                Ok(QueryResult::Rows { columns, rows })
            }

            PlanNode::RangeScan {
                table,
                column,
                start,
                end,
            } => {
                let tbl = self
                    .catalog
                    .get_table(table)
                    .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                let columns: Vec<String> = tbl
                    .schema()
                    .columns
                    .iter()
                    .map(|c| c.name.clone())
                    .collect();
                let schema = tbl.schema();

                let start_val = match start {
                    Some((expr, _)) => Some(literal_to_value(expr)?),
                    None => None,
                };
                let end_val = match end {
                    Some((expr, _)) => Some(literal_to_value(expr)?),
                    None => None,
                };
                let start_inclusive = start.as_ref().map(|(_, inc)| *inc).unwrap_or(true);
                let end_inclusive = end.as_ref().map(|(_, inc)| *inc).unwrap_or(true);

                // Non-unique index: walk the composite (value, rid) leaf
                // chain between prefix bounds, fetch each row from the heap,
                // and recheck. The recheck enforces exclusive bounds
                // (range_rids is inclusive) and defensively skips any decoded
                // null (nulls are never indexed, so they must not match).
                if tbl.is_index_unique(column) == Some(false) {
                    if let Some(btree) = tbl.index(column) {
                        if start_val.is_some() || end_val.is_some() {
                            let col_idx = schema.column_index(column).ok_or_else(|| {
                                QueryError::ColumnNotFound {
                                    table: String::new(),
                                    column: column.clone(),
                                }
                            })?;
                            let rids = btree.range_rids(start_val.as_ref(), end_val.as_ref());
                            let mut rows: Vec<Vec<Value>> = Vec::with_capacity(rids.len());
                            let mut cancel = CancelCheck::new();
                            for rid in rids {
                                cancel.tick()?;
                                // Overflow safety (P0-3): reassemble spilled cols.
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
                                        rows.push(row);
                                    }
                                }
                            }
                            return Ok(QueryResult::Rows { columns, rows });
                        }
                    }
                }

                // Range scans use the btree fast path for unique indexes,
                // walking raw column-value keys directly.
                if tbl.is_index_unique(column) == Some(true) {
                    if let Some(btree) = tbl.index(column) {
                        let hits: Vec<(Value, RowId)> = match (&start_val, &end_val) {
                            (Some(s), Some(e)) => btree.range(s, e).collect(),
                            (Some(s), None) => btree.range_from(s),
                            (None, Some(e)) => btree.range_to(e),
                            (None, None) => {
                                let mut cancel = CancelCheck::new();
                                let mut rows: Vec<Vec<Value>> = Vec::new();
                                for item in tbl.scan() {
                                    let (_, row) = item.map_err(QueryError::from_storage_io)?;
                                    cancel.tick()?;
                                    rows.push(row);
                                }
                                return Ok(QueryResult::Rows { columns, rows });
                            }
                        };
                        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(hits.len());
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
                            // Overflow safety (P0-3): reassemble spilled cols.
                            if let Some(row) = tbl.get(rid) {
                                rows.push(row);
                            }
                        }
                        return Ok(QueryResult::Rows { columns, rows });
                    }
                }

                // Fallback: no index — synthesize range predicate and scan.
                // Overflow safety (P0-4): v2-capable tables use the decoded
                // last-resort scan below.
                let fast = FastLayout::new(schema);
                let synth = synthesize_range_predicate(column, start, end);
                if !tbl.has_overflow_rows() {
                    if let Some(compiled) = self.compile_predicate_unless_forced(
                        "range-scan-scan-fallback:predicate",
                        &synth,
                        &columns,
                        &fast,
                        schema,
                    ) {
                        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(64);
                        for_each_row_raw_cancellable(&self.catalog, table, |_rid, data| {
                            if compiled(data) {
                                rows.push(decode_row(schema, data));
                            }
                        })?;
                        return Ok(QueryResult::Rows { columns, rows });
                    }
                }

                let col_idx =
                    schema
                        .column_index(column)
                        .ok_or_else(|| QueryError::ColumnNotFound {
                            table: String::new(),
                            column: column.clone(),
                        })?;
                let mut cancel = CancelCheck::new();
                let mut rows: Vec<Vec<Value>> = Vec::new();
                for item in tbl.scan() {
                    let (_, row) = item.map_err(QueryError::from_storage_io)?;
                    cancel.tick()?;
                    if range_matches(
                        &row[col_idx],
                        &start_val,
                        start_inclusive,
                        &end_val,
                        end_inclusive,
                    ) {
                        rows.push(row);
                    }
                }
                Ok(QueryResult::Rows { columns, rows })
            }
        }
    }

    // ─── Materialized view operations ──────────────────────────────────────
    //
    // See [`parse_stored_view_source`] below for why a stored source is parsed
    // before anything is computed from it.

    /// Create a materialized view: execute the source query, store results
    /// in a new backing table, and register the view.
    fn create_view(&mut self, name: &str, query_text: &str) -> Result<(), QueryError> {
        if self.view_registry.is_view(name) {
            return Err(QueryError::ViewError(format!(
                "materialized view '{name}' already exists"
            )));
        }
        // Execute the source query to get the result set.
        let result = self.execute_powql(query_text)?;
        let (columns, rows) = match result {
            QueryResult::Rows { columns, rows } => (columns, rows),
            _ => return Err("view source query must be a SELECT".into()),
        };
        // Derive a schema for the backing table from the query result columns.
        let schema = self.derive_view_schema(name, &columns, &rows)?;
        // Create the backing table and insert the result rows.
        crate::cancel::check()?;
        self.catalog
            .create_table(schema)
            .map_err(QueryError::from_storage_io)?;
        for row in &rows {
            self.catalog
                .insert(name, row)
                .map_err(QueryError::from_storage_io)?;
        }
        // Determine which base tables this view depends on by parsing the query.
        let depends_on = self.extract_view_deps(name, query_text)?;
        // Same ordering rule as `refresh_view`: registering the view CLEAN is a
        // durable claim that the rows just inserted are its current contents,
        // and `register` fsyncs `views.bin` while those rows are still only in
        // the WAL buffer. A crash in between would leave a registered, clean,
        // EMPTY view answering queries with zero rows and no error.
        if !self.in_transaction {
            self.catalog
                .commit_autocommit()
                .map_err(QueryError::from_storage_io)?;
        }
        self.view_registry
            .register(ViewDef {
                name: name.to_string(),
                query: query_text.to_string(),
                depends_on,
                dirty: false,
            })
            .map_err(QueryError::from_storage_io)?;
        Ok(())
    }

    /// Refresh a materialized view: re-execute its source query and replace
    /// the backing table's contents.
    pub(in crate::executor) fn refresh_view(&mut self, name: &str) -> Result<(), QueryError> {
        let def = self
            .view_registry
            .get(name)
            .ok_or_else(|| format!("materialized view '{name}' not found"))?;
        let query_text = def.query.clone();
        // The stored source has to be readable before anything is recomputed
        // from it. Re-executing it blind is what made a view with an
        // unparseable source silently keep serving its old rows.
        parse_stored_view_source(name, &query_text)?;
        // Execute the source query.
        let result = self.execute_powql(&query_text)?;
        let (_columns, rows) = match result {
            QueryResult::Rows { columns, rows } => (columns, rows),
            _ => return Err("view source query must be a SELECT".into()),
        };
        // The backing table's schema was frozen at create time, and the
        // encoder trusts it unconditionally. A projection is typed per row,
        // so fresh rows can legitimately come back with a different type
        // (the base table changed, a `??` arm flipped). That has to be a
        // typed error HERE, before the old contents are destroyed, not an
        // abort or bit-reinterpreted garbage inside the insert loop below.
        {
            let schema = self.catalog.schema(name).ok_or_else(|| {
                QueryError::ViewError(format!("materialized view '{name}' has no backing table"))
            })?;
            for row in &rows {
                if row.len() != schema.columns.len() {
                    return Err(QueryError::ViewError(format!(
                        "refresh of materialized view '{name}' produced rows with {} \
                         columns but the view stores {}; drop and recreate the view",
                        row.len(),
                        schema.columns.len()
                    )));
                }
                for (val, col) in row.iter().zip(&schema.columns) {
                    let t = val.type_id();
                    if t != powdb_storage::types::TypeId::Empty && t != col.type_id {
                        return Err(QueryError::ViewError(format!(
                            "refresh of materialized view '{name}' produced a {t:?} \
                             in column '{}' but the view stores {:?}; drop and \
                             recreate the view to change its column types",
                            col.name, col.type_id
                        )));
                    }
                }
            }
        }
        // Clear old data and insert fresh results. Mission B2: logged
        // variant — view refreshes are a mutation and crash recovery
        // must see them.
        crate::cancel::check()?;
        self.catalog
            .scan_delete_matching_logged(name, |_| true)
            .map_err(QueryError::from_storage_io)?;
        for row in &rows {
            self.catalog
                .insert(name, row)
                .map_err(QueryError::from_storage_io)?;
        }
        // The clean flag is durable, so it must not get to disk ahead of the
        // rows it vouches for: `mark_clean` fsyncs `views.bin`, and the writes
        // just above are still only in the WAL buffer at this point. A crash
        // between the two would leave a CLEAN flag over the pre-refresh rows,
        // which is the wrong-answer direction of the same staleness bug. So
        // commit first, then record. Inside an explicit transaction there is
        // nothing to commit yet, so the flag is cleared in memory only and the
        // on-disk flag stays dirty: at worst one redundant refresh after the
        // next open, never a stale answer.
        if self.in_transaction {
            self.view_registry.mark_clean_in_memory(name);
        } else {
            self.catalog
                .commit_autocommit()
                .map_err(QueryError::from_storage_io)?;
            self.view_registry
                .mark_clean(name)
                .map_err(QueryError::from_storage_io)?;
        }
        Ok(())
    }

    /// Drop a materialized view: remove the backing table and unregister.
    fn drop_view(&mut self, name: &str) -> Result<(), QueryError> {
        if !self.view_registry.is_view(name) {
            return Err(QueryError::ViewError(format!(
                "materialized view '{name}' not found"
            )));
        }
        self.view_registry
            .unregister(name)
            .map_err(QueryError::from_storage_io)?;
        self.catalog
            .drop_table(name)
            .map_err(QueryError::from_storage_io)?;
        Ok(())
    }

    /// Derive a storage `Schema` for a view's backing table from query
    /// result column names and the types of ALL rows.
    ///
    /// A projection is typed per row (`.tags ?? 0` is json where `tags` is
    /// set and int where it is not), while the backing table's encoder
    /// trusts the schema unconditionally: a value whose class contradicts
    /// its column either aborts (variable column, fixed value) or is bit-
    /// reinterpreted on decode (int bits read as a float). So the type must
    /// be unified over every row, with null never constraining it, and a
    /// column that genuinely mixes types is a typed error here, before any
    /// backing table exists.
    fn derive_view_schema(
        &self,
        name: &str,
        columns: &[String],
        rows: &[Vec<Value>],
    ) -> Result<Schema, QueryError> {
        use powdb_storage::types::{ColumnDef, TypeId};
        let mut types: Vec<Option<TypeId>> = vec![None; columns.len()];
        for row in rows {
            for (i, val) in row.iter().enumerate().take(columns.len()) {
                let t = val.type_id();
                if t == TypeId::Empty {
                    continue;
                }
                match types[i] {
                    None => types[i] = Some(t),
                    Some(prev) if prev == t => {}
                    Some(prev) => {
                        return Err(QueryError::ViewError(format!(
                            "materialized view '{name}' column '{}' mixes value types \
                             across rows ({prev:?} and {t:?}); make the projection \
                             produce one type per column",
                            columns[i]
                        )));
                    }
                }
            }
        }
        let cols: Vec<ColumnDef> = columns
            .iter()
            .enumerate()
            .map(|(i, col_name)| ColumnDef {
                name: col_name.clone(),
                // A column with no non-null value anywhere (or no rows at
                // all) stores as str: it encodes every null and keeps the
                // table readable.
                type_id: types[i].unwrap_or(TypeId::Str),
                required: false,
                position: i as u16,
            })
            .collect();
        Ok(Schema {
            table_name: name.to_string(),
            columns: cols,
        })
    }

    /// Extract base table dependencies from a view's source query by
    /// parsing it and collecting the source table names.
    ///
    /// A parse failure is an error rather than "no dependencies". An empty
    /// dependency list means nothing ever marks the view dirty, so it is never
    /// refreshed and every read serves whatever the backing table happens to
    /// hold, permanently and without any error: the exact silent-wrong-answer
    /// shape the rest of the engine refuses.
    fn extract_view_deps(&self, name: &str, query_text: &str) -> Result<Vec<String>, QueryError> {
        fn collect(statement: &Statement, deps: &mut Vec<String>) {
            match statement {
                Statement::Query(q) => {
                    deps.push(q.source.clone());
                    for join in &q.joins {
                        deps.push(join.source.clone());
                    }
                }
                // Both halves of a union are read by the view, so both have to
                // be able to dirty it. Without this arm a `union` view was
                // registered with no dependencies at all and never refreshed.
                Statement::Union(u) => {
                    collect(&u.left, deps);
                    collect(&u.right, deps);
                }
                _ => {}
            }
        }
        let statement = parse_stored_view_source(name, query_text)?;
        let mut deps = Vec::new();
        collect(&statement, &mut deps);
        Ok(deps)
    }

    /// Route a parsed link declaration to the persistent catalog's
    /// `create_link`, which validates the tables/columns and derives the
    /// cardinality from the target key's uniqueness. The `on <local> =
    /// <target>` clause means "the owner's `local_key` equals the target's
    /// `target_key`". A caller-supplied `kind` is ignored by the catalog, so
    /// we pass a placeholder.
    fn create_link_from_parts(
        &mut self,
        owner: &str,
        name: &str,
        target: &str,
        local_key: &str,
        target_key: &str,
    ) -> Result<(), QueryError> {
        self.catalog
            .create_link(LinkDef {
                owner_type: owner.to_string(),
                name: name.to_string(),
                target_type: target.to_string(),
                local_key: local_key.to_string(),
                target_key: target_key.to_string(),
                // Placeholder: the catalog derives the real cardinality.
                kind: LinkKind::ToMany,
            })
            .map_err(QueryError::from_storage_io)
    }

    /// Resolve every unresolved link traversal among these nested fields
    /// against the persistent catalog, returning fields whose nested
    /// projections carry a concrete child table and correlation columns and
    /// whose scalar link paths carry a resolved hop chain. Runs at execution
    /// time (the pure planner cannot see the catalog), in the same spirit as
    /// `RangeScan` late lowering. `outer_table` is the declaring type of the
    /// parent scan.
    pub(crate) fn resolve_nested_via_links(
        &self,
        fields: &[NestedProjectField],
        outer_table: &str,
    ) -> Result<Vec<NestedProjectField>, QueryError> {
        fields
            .iter()
            .map(|field| match field {
                NestedProjectField::Nested(nested) => Ok(NestedProjectField::Nested(Box::new(
                    self.resolve_via_link(nested, outer_table, true)?,
                ))),
                NestedProjectField::Plain(_) => Ok(field.clone()),
                NestedProjectField::Link(link) => Ok(NestedProjectField::Link(Box::new(
                    self.resolve_scalar_link_field(link, outer_table)?,
                ))),
            })
            .collect()
    }

    /// Resolve one nested projection level (and its deeper levels): if it is a
    /// block link traversal, look the link up under `(outer_table, link_name)`
    /// and fill in the child table and correlation columns so execution
    /// proceeds exactly as for the explicit correlated spelling. A block
    /// traversal is only valid through a `ToMany` link; a `ToOne` link is a
    /// kind-mismatch error. Cardinality is derived from the catalog at
    /// execution time, so it tracks index DDL that ran after the link was
    /// declared. `qualify_parent` mirrors the planner: the
    /// top level correlates against an `AliasScan`'s `alias.col` columns,
    /// deeper levels against the enclosing child's bare schema columns.
    fn resolve_via_link(
        &self,
        nested: &NestedProjection,
        outer_table: &str,
        qualify_parent: bool,
    ) -> Result<NestedProjection, QueryError> {
        let mut out = nested.clone();
        if let Some(via) = &nested.via_link {
            let link = self.catalog.link(outer_table, &via.link_name).cloned();
            let link = link.ok_or_else(|| {
                QueryError::Execution(format!(
                    "unknown link `{}` on type `{}`; declare it with \
                     `link {}.{} -> <Target> on <local> = <target>`",
                    via.link_name, outer_table, outer_table, via.link_name
                ))
            })?;
            // GATE B1. Cardinality is derived from index uniqueness here and
            // nowhere else. `LinkDef::kind` is an advisory byte that is never
            // refreshed (see `Catalog::derive_link_kind`); reading it would
            // make `alter <Target> add unique .<key>` after the link silently
            // keep this hop to-many forever.
            let kind = self
                .catalog
                .derive_link_kind(&link.target_type, &link.target_key);
            if kind != LinkKind::ToMany {
                return Err(QueryError::Execution(format!(
                    "link `{}` on type `{}` is a to-one link (its target key \
                     `{}.{}` is unique, so a hop matches at most one row); \
                     traverse it as a path (`{}.{}.<column>`), not a block",
                    via.link_name,
                    outer_table,
                    link.target_type,
                    link.target_key,
                    nested.parent_alias,
                    via.link_name
                )));
            }
            // owner.local_key = target.target_key: the child (target) side of
            // the correlation is `target_key`, the parent (owner) side is
            // `local_key`.
            out.table = link.target_type.clone();
            out.child_key = link.target_key.clone();
            out.parent_key = if qualify_parent {
                format!("{}.{}", nested.parent_alias, link.local_key)
            } else {
                link.local_key.clone()
            };
            out.via_link = None;
        }
        // Deeper levels correlate against THIS child table (now concrete) on a
        // bare parent key.
        out.fields = nested
            .fields
            .iter()
            .map(|field| match field {
                NestedField::Nested(inner) => Ok(NestedField::Nested(Box::new(
                    self.resolve_via_link(inner, &out.table, false)?,
                ))),
                NestedField::Scalar { .. } => Ok(field.clone()),
            })
            .collect::<Result<Vec<_>, QueryError>>()?;
        Ok(out)
    }

    /// Resolve a scalar link path (`o.user.company.name`) against the
    /// persistent catalog: each path segment must name a declared `ToOne`
    /// link on the type reached so far. Produces one [`ScalarLinkHop`] per
    /// segment; the first hop's FK column is qualified with the outer alias to
    /// match the parent `AliasScan`'s column names. A `ToMany` link in the
    /// chain (a non-unique target key) is a kind-mismatch error, never a silent
    /// fan-out. Each hop's cardinality is derived from the catalog at execution
    /// time, so a target key made unique after the link was declared is a
    /// to-one hop from that moment on, with no re-declaration.
    fn resolve_scalar_link_field(
        &self,
        field: &ScalarLinkField,
        outer_table: &str,
    ) -> Result<ScalarLinkField, QueryError> {
        let mut out = field.clone();
        if out.resolved.is_some() {
            return Ok(out);
        }
        let mut chain: Vec<LinkDef> = Vec::with_capacity(field.links.len());
        let mut current = outer_table.to_string();
        for link_name in &field.links {
            let link = self.catalog.link(&current, link_name).cloned();
            let link = link.ok_or_else(|| {
                QueryError::Execution(format!(
                    "unknown link `{link_name}` on type `{current}`; declare it with \
                     `link {current}.{link_name} -> <Target> on <local> = <target>`"
                ))
            })?;
            // GATE B2, per hop: every link in the chain is checked against the
            // catalog as it stands now, not as it stood when the link was
            // declared. Same rule as B1: never read `LinkDef::kind`.
            let kind = self
                .catalog
                .derive_link_kind(&link.target_type, &link.target_key);
            if kind != LinkKind::ToOne {
                // Lead with the remedy that keeps the query as written. The
                // block form is the alternative, not the default: it turns a
                // foreign-key lookup into a one-element array the caller has to
                // unwrap forever. Only offer `add unique` when it would
                // actually be accepted: a target key that already carries a
                // plain index cannot be upgraded in place, and pointing at a
                // statement that errors is how the old message misled.
                //
                // Each branch supplies a whole sentence rather than a fragment
                // spliced into a shared frame: the plain-index case has no
                // imperative to give, and forcing it into "To read one value
                // per row, <fragment>" produced a sentence that did not parse.
                let remedy = if self
                    .catalog
                    .is_index_unique(&link.target_type, &link.target_key)
                    == Some(false)
                {
                    format!(
                        "There is no way to read one value per row here: `{}.{}` \
                         already carries a non-unique index and an index cannot \
                         be upgraded in place, so this link stays to-many.",
                        link.target_type, link.target_key
                    )
                } else {
                    format!(
                        "To read one value per row, make the target key unique \
                         with `alter {} add unique .{}`.",
                        link.target_type, link.target_key
                    )
                };
                return Err(QueryError::Execution(format!(
                    "link `{link_name}` on type `{current}` is a to-many link: \
                     its target key `{}.{}` is not unique, so a hop can match \
                     many rows. {remedy} To read every match, traverse it with a \
                     block (`{link_name}: {}.{link_name} {{ ... }}`)",
                    link.target_type, link.target_key, field.outer_alias
                )));
            }
            current = link.target_type.clone();
            chain.push(link);
        }
        // owner.local_key = target.target_key: the FK on the many side is
        // `local_key`, the key on the one side is `target_key`. The parser only
        // builds a link path with at least one hop, but this runs on any plan
        // an executor is handed, and an empty chain must be a typed error and
        // never a slice-index panic (panic = abort makes that a remote DoS).
        let Some(first) = chain.first() else {
            return Err(QueryError::Execution(format!(
                "scalar link path for column `{}` names no link to traverse; \
                 write it as `<alias>.<link>.<column>`",
                field.column
            )));
        };
        let first_fk = format!("{}.{}", field.outer_alias, first.local_key);
        let hops = chain
            .iter()
            .enumerate()
            .map(|(i, link)| ScalarLinkHop {
                table: link.target_type.clone(),
                key_col: link.target_key.clone(),
                out_col: match chain.get(i + 1) {
                    Some(next) => next.local_key.clone(),
                    None => field.column.clone(),
                },
            })
            .collect();
        out.resolved = Some(ScalarLinkResolved { first_fk, hops });
        Ok(out)
    }

    /// Build one lookup map per hop of a resolved scalar link path: key column
    /// value -> out column value over the hop's target table. A duplicate key
    /// value is an error, not a silent pick: a scalar hop through a non-unique
    /// key is the to-one assumption failing (a `ToOne` link whose unique index
    /// was later dropped), which in SQL would silently fan the join out.
    /// `fk_keys` is the set of distinct non-NULL FK values the outer scan
    /// actually selects. A to-one hop's target key is unique, so a selective
    /// outer query only needs a point probe per key it references instead of a
    /// full target-table scan. Each hop restricts to the keys the previous
    /// hop's map can actually reach, so a selective outer query stays selective
    /// through a multi-hop path.
    fn build_scalar_link_maps(
        &self,
        link: &ScalarLinkField,
        resolved: &ScalarLinkResolved,
        fk_keys: &rustc_hash::FxHashSet<Value>,
    ) -> Result<Vec<rustc_hash::FxHashMap<Value, Value>>, QueryError> {
        use rustc_hash::{FxHashMap, FxHashSet};
        let mut cancel = CancelCheck::new();
        let mut maps = Vec::with_capacity(resolved.hops.len());
        // Keys the executor will look up at this hop: the outer FK values for
        // the first hop, then the non-NULL outputs the previous map produced
        // for those keys. The executor only ever consults `map.get(v)` for
        // `v` in this set, so a map restricted to it is byte-identical for
        // every lookup that actually happens.
        let mut needed_keys: FxHashSet<Value> = fk_keys.clone();
        for hop in &resolved.hops {
            let schema = self
                .catalog
                .schema(&hop.table)
                .ok_or_else(|| QueryError::TableNotFound(hop.table.clone()))?
                .clone();
            let column_index = |name: &str| {
                schema
                    .columns
                    .iter()
                    .position(|c| c.name == name)
                    .ok_or_else(|| QueryError::ColumnNotFound {
                        table: hop.table.clone(),
                        column: name.to_string(),
                    })
            };
            let key_idx = column_index(&hop.key_col)?;
            let out_idx = column_index(&hop.out_col)?;
            // Probe only when the key column has a UNIQUE index: uniqueness
            // makes the full-scan duplicate check moot (at most one row per
            // key), so a per-key point probe is byte-identical to the scan.
            // A non-unique or absent index falls through to the full scan,
            // which still raises the hard duplicate-key error even for keys no
            // parent references (the "to-one link whose unique index was
            // dropped" corruption case).
            let use_probes = self.catalog.is_index_unique(&hop.table, &hop.key_col) == Some(true)
                && self.child_index_probe_pays_off(&hop.table, &hop.key_col, needed_keys.len());
            let mut map: FxHashMap<Value, Value> = FxHashMap::default();
            if use_probes {
                let tbl = self
                    .catalog
                    .get_table(&hop.table)
                    .ok_or_else(|| QueryError::TableNotFound(hop.table.clone()))?;
                // Strict-type gate mirrors the scan-built map: its keys are all
                // the column's own type, and Value equality is typed, so a
                // cross-type FK never matches under either strategy.
                let col_type = schema.columns[key_idx].type_id;
                let mut narrowed: Vec<Vec<Value>> = Vec::with_capacity(needed_keys.len());
                for key in &needed_keys {
                    cancel.tick()?;
                    if key.type_id() != col_type {
                        continue;
                    }
                    if let Some((_, row)) = tbl.index_lookup(&hop.key_col, key) {
                        // A NULL key never matches any FK value.
                        if row[key_idx] == Value::Empty {
                            continue;
                        }
                        narrowed.push(vec![row[key_idx].clone(), row[out_idx].clone()]);
                    }
                }
                self.charge_rows(&narrowed)?;
                for mut pair in narrowed {
                    cancel.tick()?;
                    let value = pair.pop().expect("two columns per narrowed row");
                    let key = pair.pop().expect("two columns per narrowed row");
                    map.insert(key, value);
                }
            } else {
                // Materialize the two needed columns and charge them against
                // the query budget like a join build side.
                let mut narrowed: Vec<Vec<Value>> = Vec::new();
                for item in self
                    .catalog
                    .scan(&hop.table)
                    .map_err(QueryError::from_storage_io)?
                {
                    let (_, row) = item.map_err(QueryError::from_storage_io)?;
                    cancel.tick()?;
                    // A NULL key never matches any FK value.
                    if row[key_idx] == Value::Empty {
                        continue;
                    }
                    narrowed.push(vec![row[key_idx].clone(), row[out_idx].clone()]);
                }
                self.charge_rows(&narrowed)?;
                map.reserve(narrowed.len());
                for mut pair in narrowed {
                    cancel.tick()?;
                    let value = pair.pop().expect("two columns per narrowed row");
                    let key = pair.pop().expect("two columns per narrowed row");
                    if map.insert(key.clone(), value).is_some() {
                        return Err(QueryError::Execution(format!(
                            "scalar link `{}`: key column `{}.{}` is not unique \
                             (duplicate value {key:?}); a scalar link requires a \
                             unique target key",
                            link.name, hop.table, hop.key_col
                        )));
                    }
                }
            }
            // The next hop only needs arrays for the non-NULL values this hop
            // produces for the keys we care about; anything else the executor
            // will never consult.
            needed_keys = needed_keys
                .iter()
                .filter_map(|k| map.get(k))
                .filter(|v| **v != Value::Empty)
                .cloned()
                .collect();
            maps.push(map);
        }
        Ok(maps)
    }

    /// Execute the projection layer of a `NestedProject`: plain fields
    /// evaluate against the parent rows like `Project`; each nested field is
    /// assembled bottom-up by [`Engine::assemble_nested_arrays`], one hash
    /// build pass per child table keyed by its correlation column. Shared by
    /// the mutable and read-only dispatches (assembly only reads).
    pub(crate) fn execute_nested_project(
        &self,
        parent: QueryResult,
        fields: &[NestedProjectField],
    ) -> Result<QueryResult, QueryError> {
        use rustc_hash::FxHashMap;
        let QueryResult::Rows {
            columns: parent_columns,
            rows: parent_rows,
        } = parent
        else {
            return Err("nested projection requires row input".into());
        };
        // Per non-plain field: the parent-side key column index and the
        // assembled build side (JSON array map for a nested block, one
        // key -> value map per hop for a scalar link path).
        enum FieldBuild {
            Nested(usize, FxHashMap<Value, String>),
            Link(usize, Vec<FxHashMap<Value, Value>>),
        }
        let mut builds: Vec<FieldBuild> = Vec::new();
        for field in fields {
            match field {
                NestedProjectField::Plain(_) => {}
                NestedProjectField::Nested(nested) => {
                    let parent_idx = parent_columns
                        .iter()
                        .position(|c| c == &nested.parent_key)
                        .ok_or_else(|| {
                            QueryError::Execution(format!(
                                "nested projection `{}` outer column `{}` not found",
                                nested.name, nested.parent_key
                            ))
                        })?;
                    // Distinct non-NULL correlation values actually present on
                    // the parent side. Assembly only ever needs child rows for
                    // these keys, which is what lets a selective parent avoid
                    // paying for the whole child table.
                    let parent_keys = distinct_non_null(&parent_rows, parent_idx);
                    builds.push(FieldBuild::Nested(
                        parent_idx,
                        self.assemble_nested_arrays(nested, &parent_keys)?,
                    ));
                }
                NestedProjectField::Link(link) => {
                    let resolved = link.resolved.as_ref().ok_or_else(|| {
                        QueryError::Execution(format!(
                            "scalar link path `{}` was not resolved before execution",
                            link.name
                        ))
                    })?;
                    let parent_idx = parent_columns
                        .iter()
                        .position(|c| c == &resolved.first_fk)
                        .ok_or_else(|| {
                            QueryError::Execution(format!(
                                "scalar link `{}` FK column `{}` not found on the outer scan",
                                link.name, resolved.first_fk
                            ))
                        })?;
                    // Distinct non-NULL FK values the outer scan actually
                    // selects: a to-one hop's target key is unique, so a
                    // selective outer query only needs point probes for these
                    // keys instead of scanning the whole target table.
                    let fk_keys = distinct_non_null(&parent_rows, parent_idx);
                    builds.push(FieldBuild::Link(
                        parent_idx,
                        self.build_scalar_link_maps(link, resolved, &fk_keys)?,
                    ));
                }
            }
        }

        let columns: Vec<String> = fields
            .iter()
            .map(|field| match field {
                NestedProjectField::Plain(f) => f
                    .alias
                    .clone()
                    .unwrap_or_else(|| expression_output_name(&f.expr)),
                NestedProjectField::Nested(nested) => nested.name.clone(),
                NestedProjectField::Link(link) => link.name.clone(),
            })
            .collect();
        let mut cancel = CancelCheck::new();
        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(parent_rows.len());
        for parent_row in &parent_rows {
            cancel.tick()?;
            let mut out = Vec::with_capacity(fields.len());
            let mut build_iter = builds.iter();
            for field in fields {
                match field {
                    NestedProjectField::Plain(f) => {
                        out.push(eval_expr(&f.expr, parent_row, &parent_columns));
                    }
                    NestedProjectField::Nested(nested) => {
                        let Some(FieldBuild::Nested(parent_idx, build)) = build_iter.next() else {
                            unreachable!("one build side per non-plain field, in order");
                        };
                        let array = build
                            .get(&parent_row[*parent_idx])
                            .map(String::as_str)
                            .unwrap_or("[]");
                        // Round-tripping through the text parser yields
                        // canonical PJ1 (sorted object keys) for free.
                        let doc = powdb_storage::pj1::parse_json_text(array).map_err(|e| {
                            QueryError::Execution(format!(
                                "nested projection `{}` produced invalid JSON: {e}",
                                nested.name
                            ))
                        })?;
                        out.push(Value::Json(doc.into()));
                    }
                    NestedProjectField::Link(_) => {
                        let Some(FieldBuild::Link(parent_idx, maps)) = build_iter.next() else {
                            unreachable!("one build side per non-plain field, in order");
                        };
                        // Walk the hop maps: a NULL or dangling FK at any hop
                        // yields an empty value (LEFT JOIN semantics); the
                        // parent row is never dropped.
                        let mut value = parent_row[*parent_idx].clone();
                        for map in maps {
                            if value == Value::Empty {
                                break;
                            }
                            value = map.get(&value).cloned().unwrap_or(Value::Empty);
                        }
                        out.push(value);
                    }
                }
            }
            rows.push(out);
        }
        Ok(QueryResult::Rows { columns, rows })
    }

    /// Assemble one nested projection level bottom-up: gather this level's
    /// child rows (full scan, or per-parent-key index probes when the parent
    /// side is selective and the correlation column is indexed), apply the
    /// residual filter, recursively assemble deeper levels restricted to the
    /// correlation values actually gathered, group rows by correlation
    /// value, order and truncate each parent's bucket, and serialize each
    /// bucket to JSON array text. Recursion depth is bounded by the parser's
    /// nesting guard.
    ///
    /// `parent_keys` is the set of distinct non-NULL correlation values the
    /// enclosing level will look up: the assembled map never needs any other
    /// key, so a small set with an index on `child_key` skips the child
    /// table scan entirely.
    fn assemble_nested_arrays(
        &self,
        nested: &NestedProjection,
        parent_keys: &rustc_hash::FxHashSet<Value>,
    ) -> Result<rustc_hash::FxHashMap<Value, String>, QueryError> {
        use rustc_hash::FxHashMap;
        let schema = self
            .catalog
            .schema(&nested.table)
            .ok_or_else(|| QueryError::TableNotFound(nested.table.clone()))?
            .clone();
        let column_index = |name: &str| {
            schema
                .columns
                .iter()
                .position(|c| c.name == name)
                .ok_or_else(|| QueryError::ColumnNotFound {
                    table: nested.table.clone(),
                    column: name.to_string(),
                })
        };
        let key_idx = column_index(&nested.child_key)?;
        // One value source per output field: a scalar column, or the
        // correlation column of a deeper level (whose arrays are assembled
        // after this level's rows are gathered, so the recursion can be
        // restricted to the keys those rows actually reference).
        let mut field_specs: Vec<(&str, usize, Option<&NestedProjection>)> =
            Vec::with_capacity(nested.fields.len());
        for field in &nested.fields {
            match field {
                NestedField::Scalar { key, column } => {
                    field_specs.push((key.as_str(), column_index(column)?, None));
                }
                NestedField::Nested(inner) => {
                    field_specs.push((
                        inner.name.as_str(),
                        column_index(&inner.parent_key)?,
                        Some(inner),
                    ));
                }
            }
        }
        let order_idxs = nested
            .order
            .iter()
            .map(|(column, descending)| Ok((column_index(column)?, *descending)))
            .collect::<Result<Vec<_>, QueryError>>()?;
        let bound = |expr: &Option<Expr>, what: &str| -> Result<Option<usize>, QueryError> {
            match expr {
                None => Ok(None),
                Some(Expr::Literal(Literal::Int(v))) if *v >= 0 => Ok(Some(*v as usize)),
                Some(_) => Err(QueryError::Execution(format!(
                    "nested projection `{}` {what} must be a non-negative integer literal",
                    nested.name
                ))),
            }
        };
        let limit = bound(&nested.limit, "limit")?;
        let offset = bound(&nested.offset, "offset")?;
        // No parent will consult the map: skip the data work, but only
        // after the validation above, and still validate deeper levels so
        // schema errors do not appear and disappear with the data.
        if parent_keys.is_empty() {
            for (_, _, inner) in &field_specs {
                if let Some(inner) = inner {
                    self.assemble_nested_arrays(inner, parent_keys)?;
                }
            }
            return Ok(FxHashMap::default());
        }
        // Residual conditions reference bare child columns (rewritten by
        // the planner), so they evaluate against the full schema row.
        let schema_cols: Vec<String> = if nested.residual.is_some() {
            schema.columns.iter().map(|c| c.name.clone()).collect()
        } else {
            Vec::new()
        };
        // Materialize only the needed child columns (key first), charge
        // them against the query budget like a join build side, then fold
        // into per-parent buckets.
        //
        // Row gathering has two strategies:
        //   1. Index probes: when the parent side is selective and
        //      `child_key` is indexed, probe the btree once per parent key
        //      and fetch only matching rows. Probe results come back in rid
        //      order per key, which is exactly the heap scan order the
        //      unordered-array contract promises.
        //   2. Full scan: the fleet-shaped default. When the parent key set
        //      is small in absolute terms, non-matching correlation values
        //      are skipped before narrowing so unrelated buckets are never
        //      materialized or serialized.
        let use_index_probes =
            self.child_index_probe_pays_off(&nested.table, &nested.child_key, parent_keys.len());
        // Membership pre-filter for the scan strategy: cheap insurance for
        // selective parents without an index, skipped for large parent sets
        // (fleet shape) where nearly every child row matches anyway.
        const SCAN_KEY_FILTER_MAX_KEYS: usize = 1024;
        let scan_key_filter = !use_index_probes && parent_keys.len() <= SCAN_KEY_FILTER_MAX_KEYS;
        let mut cancel = CancelCheck::new();
        let mut child_rows: Vec<Vec<Value>> = Vec::new();
        let narrow_into =
            |row: &[Value], child_rows: &mut Vec<Vec<Value>>| -> Result<(), QueryError> {
                // A NULL correlation value never matches any parent.
                if row[key_idx] == Value::Empty {
                    return Ok(());
                }
                if scan_key_filter && !parent_keys.contains(&row[key_idx]) {
                    return Ok(());
                }
                if let Some(residual) = &nested.residual {
                    if !eval_predicate(residual, row, &schema_cols) {
                        return Ok(());
                    }
                }
                let mut narrowed = Vec::with_capacity(1 + field_specs.len() + order_idxs.len());
                narrowed.push(row[key_idx].clone());
                for (_, idx, _) in &field_specs {
                    narrowed.push(row[*idx].clone());
                }
                for (idx, _) in &order_idxs {
                    narrowed.push(row[*idx].clone());
                }
                child_rows.push(narrowed);
                Ok(())
            };
        if use_index_probes {
            let tbl = self
                .catalog
                .get_table(&nested.table)
                .ok_or_else(|| QueryError::TableNotFound(nested.table.clone()))?;
            // Strict-type gate: the hash build this path replaces uses
            // strictly-typed Value equality (Int(4) never equals Float(4.0)),
            // but the btree's Ord is cross-type numeric. Only probe with
            // keys of the column's own type; any other key can never match
            // and correctly falls through to the [] default.
            let col_type = schema.columns[key_idx].type_id;
            for key in parent_keys {
                cancel.tick()?;
                if key.type_id() != col_type {
                    continue;
                }
                for rid in tbl.index_lookup_all(&nested.child_key, key) {
                    cancel.tick()?;
                    // `tbl.get` reassembles spilled/overflow columns and
                    // tolerates a stale rid (None) like the IndexScan path.
                    if let Some(row) = tbl.get(rid) {
                        narrow_into(&row, &mut child_rows)?;
                    }
                }
            }
        } else {
            for item in self
                .catalog
                .scan(&nested.table)
                .map_err(QueryError::from_storage_io)?
            {
                let (_, row) = item.map_err(QueryError::from_storage_io)?;
                cancel.tick()?;
                narrow_into(&row, &mut child_rows)?;
            }
        }
        self.charge_rows(&child_rows)?;
        // Deeper levels only need arrays for correlation values that
        // actually appear in the gathered rows; collecting them here is what
        // lets a selective parent stay selective all the way down.
        enum FieldSource {
            Column,
            Arrays(FxHashMap<Value, String>),
        }
        let mut sources: Vec<(&str, FieldSource)> = Vec::with_capacity(field_specs.len());
        for (i, (name, _, inner)) in field_specs.iter().enumerate() {
            match inner {
                None => sources.push((name, FieldSource::Column)),
                Some(inner) => {
                    let mut inner_keys: rustc_hash::FxHashSet<Value> =
                        rustc_hash::FxHashSet::default();
                    for child in &child_rows {
                        let value = &child[1 + i];
                        if *value != Value::Empty {
                            inner_keys.insert(value.clone());
                        }
                    }
                    sources.push((
                        name,
                        FieldSource::Arrays(self.assemble_nested_arrays(inner, &inner_keys)?),
                    ));
                }
            }
        }
        // Bucket entries keep their per-parent sort key values (the
        // narrowed tail) until ordering and truncation are applied.
        let mut buckets: FxHashMap<Value, Vec<(Vec<Value>, String)>> =
            FxHashMap::with_capacity_and_hasher(child_rows.len(), Default::default());
        let sort_tail = 1 + sources.len();
        for mut child in child_rows {
            cancel.tick()?;
            let sort_values = child.split_off(sort_tail);
            let mut object = String::from("{");
            for (i, ((name, source), value)) in sources.iter().zip(&child[1..]).enumerate() {
                if i > 0 {
                    object.push(',');
                }
                push_json_string(&mut object, name);
                object.push(':');
                match source {
                    FieldSource::Column => push_json_value(&mut object, value),
                    FieldSource::Arrays(arrays) => {
                        object.push_str(arrays.get(value).map(String::as_str).unwrap_or("[]"));
                    }
                }
            }
            object.push('}');
            let key = child.swap_remove(0);
            buckets.entry(key).or_default().push((sort_values, object));
        }
        let mut build: FxHashMap<Value, String> =
            FxHashMap::with_capacity_and_hasher(buckets.len(), Default::default());
        for (key, mut bucket) in buckets {
            cancel.tick()?;
            if !order_idxs.is_empty() {
                // Stable sort: ties keep child scan order.
                bucket.sort_by(|(a, _), (b, _)| {
                    for (pos, (_, descending)) in order_idxs.iter().enumerate() {
                        let cmp = compare_order_values(&a[pos], &b[pos], *descending);
                        if cmp != std::cmp::Ordering::Equal {
                            return cmp;
                        }
                    }
                    std::cmp::Ordering::Equal
                });
            }
            let kept = bucket
                .iter()
                .skip(offset.unwrap_or(0))
                .take(limit.unwrap_or(usize::MAX));
            let mut array =
                String::with_capacity(2 + kept.clone().map(|(_, o)| o.len() + 1).sum::<usize>());
            array.push('[');
            for (i, (_, object)) in kept.enumerate() {
                if i > 0 {
                    array.push(',');
                }
                array.push_str(object);
            }
            array.push(']');
            build.insert(key, array);
        }
        Ok(build)
    }

    /// Whether per-parent-key index probes beat a full child-table scan for
    /// one nested projection level. Mirrors the range chooser's use of live
    /// `catalog.index_stats`: estimate the fetched row count as
    /// `parent keys * average bucket size` and require it to undercut the
    /// scan by 4x, pricing in the btree probe plus the random-access
    /// `tbl.get` per rid versus the sequential mmap scan. A fleet-shaped
    /// read (every parent selected) estimates at ~total entries and stays
    /// on the scan; a selective parent estimates tiny and probes.
    fn child_index_probe_pays_off(&self, table: &str, column: &str, n_keys: usize) -> bool {
        if !self.catalog.has_index(table, column) {
            return false;
        }
        let Some(stats) = self.catalog.index_stats(table, column) else {
            return false;
        };
        if stats.distinct_keys == 0 {
            // Empty index: every probe is a no-op and the scan has nothing
            // indexable either (Empty keys never correlate).
            return true;
        }
        let avg_bucket = stats.total_entries.div_ceil(stats.distinct_keys);
        let estimated_fetch = (n_keys as u64).saturating_mul(avg_bucket);
        estimated_fetch.saturating_mul(4) <= stats.total_entries
    }
}

/// True when any nested field (at any depth) is an unresolved link traversal
/// (a block `via_link` or an unresolved scalar link path) and therefore needs
/// catalog resolution before assembly.
/// Render a view-name list for the `drop` message, with the verb that agrees
/// with it: ("view 'V'", "has") for one, ("views 'A', 'B'", "have") for several.
/// Callers only reach this with a non-empty list.
fn describe_view_list(names: &[String]) -> (String, &'static str) {
    let quoted: Vec<String> = names.iter().map(|n| format!("'{n}'")).collect();
    if quoted.len() == 1 {
        (format!("view {}", quoted[0]), "has")
    } else {
        (format!("views {}", quoted.join(", ")), "have")
    }
}

pub(crate) fn nested_fields_have_via_link(fields: &[NestedProjectField]) -> bool {
    fn nested_has(nested: &NestedProjection) -> bool {
        nested.via_link.is_some()
            || nested.fields.iter().any(|field| match field {
                NestedField::Nested(inner) => nested_has(inner),
                NestedField::Scalar { .. } => false,
            })
    }
    fields.iter().any(|field| match field {
        NestedProjectField::Nested(nested) => nested_has(nested),
        NestedProjectField::Plain(_) => false,
        NestedProjectField::Link(link) => link.resolved.is_none(),
    })
}

/// The base table a read plan scans, following the single-input pipeline down
/// to its `AliasScan`/`SeqScan` leaf. Used to name the declaring type when
/// resolving a top-level link traversal.
pub(crate) fn scan_source_table(plan: &PlanNode) -> Option<&str> {
    match plan {
        PlanNode::AliasScan { table, .. } | PlanNode::SeqScan { table } => Some(table),
        PlanNode::Filter { input, .. }
        | PlanNode::Sort { input, .. }
        | PlanNode::Limit { input, .. }
        | PlanNode::Offset { input, .. } => scan_source_table(input),
        _ => None,
    }
}

/// Distinct non-NULL values at column `idx` across `rows`. This is the set of
/// correlation / FK keys a nested block or scalar link will ever look up, so
/// threading it into the build side lets a selective parent skip child rows no
/// parent references.
fn distinct_non_null(rows: &[Vec<Value>], idx: usize) -> rustc_hash::FxHashSet<Value> {
    let mut keys: rustc_hash::FxHashSet<Value> = rustc_hash::FxHashSet::default();
    for row in rows {
        let key = &row[idx];
        if *key != Value::Empty {
            keys.insert(key.clone());
        }
    }
    keys
}

/// Append `s` to `out` as a JSON string literal with the required escapes.
fn push_json_string(out: &mut String, s: &str) {
    use std::fmt::Write;
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c <= '\u{1f}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Append a child column value to `out` as a JSON value. Scalars map
/// naturally (int/float -> number, str -> string, bool -> bool, empty ->
/// null); JSON columns embed as sub-documents; the remaining types
/// (datetime, uuid, bytes) fall back to their wire text as a JSON string
/// (slice scope).
fn push_json_value(out: &mut String, value: &Value) {
    use std::fmt::Write;
    match value {
        Value::Empty => out.push_str("null"),
        Value::Int(v) => {
            let _ = write!(out, "{v}");
        }
        Value::Float(v) if v.is_finite() => {
            // Rust's shortest Display renders 3.0 as "3", which the
            // canonicalizing PJ1 re-parse would store as an int. Use the
            // shared renderer that guarantees a fractional/exponent marker.
            out.push_str(&powdb_storage::pj1::render_float(*v));
        }
        // NaN/infinity have no JSON representation.
        Value::Float(_) => out.push_str("null"),
        Value::Bool(v) => out.push_str(if *v { "true" } else { "false" }),
        Value::Str(s) => push_json_string(out, s),
        Value::Json(doc) => {
            out.push_str(&powdb_storage::pj1::pj1_to_text(doc).unwrap_or_else(|_| "null".into()))
        }
        other => push_json_string(out, &other.to_wire_string()),
    }
}

/// Parse a materialized view's STORED source text, or fail with a typed error
/// naming the view.
///
/// A view's source text outlives the process and outlives the release that
/// wrote it. Releases up to 0.21.0 reconstructed that text in a way that could
/// lose string escapes and backtick-quoted identifiers, so a database written
/// by one of them can hold a source that no longer parses at all. Both places
/// that read one back treated a parse failure as "nothing to do":
/// `extract_view_deps` returned no dependencies, so the view was never marked
/// dirty and therefore never refreshed, and every read of it then served
/// whatever rows the backing table happened to hold, forever, with no error
/// anywhere. A read returned `[]` where the view's own query returned `[1]`.
///
/// Fixing the reconstruction is not retroactive: nothing rewrites a source that
/// is already on disk. So the read side refuses instead, which turns a silent
/// wrong answer into an error the operator can act on.
///
/// The relex round-trip check that guards `materialize` is deliberately NOT
/// applied here. A refresh executes the stored text directly rather than
/// re-rendering it, so a source that parses but is not a fixed point of the
/// current reconstruction still computes exactly what it says; rejecting it
/// would fail live views over a difference with no runtime consequence.
pub(super) fn parse_stored_view_source(name: &str, source: &str) -> Result<Statement, QueryError> {
    crate::parser::parse(source).map_err(|err| {
        QueryError::ViewError(format!(
            "materialized view '{name}' has a stored source query that no longer parses \
             ({err}). It was written by an older release whose source-text reconstruction \
             was lossy, so the view cannot be refreshed and its rows cannot be trusted. \
             Re-create it: `drop view {name}`, then `materialize {name} as \
             <the original query>`."
        ))
    })
}
