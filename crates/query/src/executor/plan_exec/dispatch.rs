//! The `execute_plan` dispatch match and materialized view operations.

use crate::cancel::CancelCheck;
use crate::result::{QueryError, QueryResult};
use powdb_storage::row::{decode_row, RowLayout};
use powdb_storage::types::*;
use std::ops::ControlFlow;

use crate::executor::compiled::*;
use crate::executor::eval::*;
use crate::executor::row_body_base;
use crate::executor::{Engine, MAX_SORT_ROWS};
use powdb_storage::view::ViewDef;

use super::*;

impl Engine {
    pub fn execute_plan(&mut self, plan: &PlanNode) -> Result<QueryResult, QueryError> {
        // Refuse any plan whose evaluable expressions still carry an aggregate
        // FunctionCall the grouped-aggregate planner could not lower. Without
        // this, such an aggregate would reach eval_expr and silently evaluate
        // to Empty (a wrong answer). The outermost call validates the whole
        // tree before any row is produced.
        validate_no_stray_aggregates(plan)?;
        validate_json_path_types(&self.catalog, plan)?;
        match plan {
            PlanNode::ExprIndexScan { .. }
            | PlanNode::ExprRangeScan { .. }
            | PlanNode::OrderedExprIndexScan { .. } => {
                if let Some(result) = self.execute_expression_index_plan(plan, None)? {
                    return Ok(result);
                }
                let fallback = expression_index_fallback(plan)
                    .expect("expression-index branch always has a fallback");
                self.execute_plan(&fallback)
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
                for (_, row) in self
                    .catalog
                    .scan(table)
                    .map_err(|e| QueryError::StorageError(e.to_string()))?
                {
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
                    let result = self.execute_plan(input)?;
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
                    if !self.catalog.table_has_overflow(table) {
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
                        if let Some(compiled) =
                            compile_predicate(predicate, &columns, &fast, &schema)
                        {
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
                                .map_err(|e| QueryError::StorageError(e.to_string()))?;
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
                                .map_err(|e| QueryError::StorageError(e.to_string()))?;
                        }
                        if let Some(e) = cancel_err {
                            return Err(e);
                        }

                        return Ok(QueryResult::Rows { columns, rows });
                    }
                }

                // General path: materialise then filter.
                let result = self.execute_plan(input)?;
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
                    if tbl.has_index(column) && all_plain_fields {
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
                        // Fast path only for single-key sorts
                        if keys.len() == 1 {
                            if let Expr::Field(sort_field) = &keys[0].expr {
                                let descending = keys[0].descending;
                                let limit = match limit_expr {
                                    Expr::Literal(Literal::Int(v)) if *v >= 0 => *v as usize,
                                    _ => usize::MAX,
                                };
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
                        if let PlanNode::SeqScan { table } = fi.as_ref() {
                            let limit = match limit_expr {
                                Expr::Literal(Literal::Int(v)) if *v >= 0 => *v as usize,
                                _ => usize::MAX,
                            };
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
                    if let PlanNode::SeqScan { table } = inner.as_ref() {
                        let limit = match limit_expr {
                            Expr::Literal(Literal::Int(v)) if *v >= 0 => *v as usize,
                            _ => usize::MAX,
                        };
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

                let result = self.execute_plan(input)?;
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
                let result = self.execute_plan(input)?;
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
                                let index = stored_name
                                    .as_ref()
                                    .and_then(|name| columns.iter().position(|c| c == name));
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
                let result = self.execute_plan(input)?;
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
                let result = self.execute_plan(input)?;
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
                // Fast path: count() over SeqScan — count rows without any decode
                if *function == AggFunc::Count {
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
                                if let Some(compiled) =
                                    compile_predicate(predicate, &columns, &fast, &schema)
                                {
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
                let result = self.execute_plan(input)?;
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
                    self.catalog.assign_auto_columns(table, values);
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
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                }
                self.view_registry.mark_dependents_dirty(table);
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
                let (values, key_idx) = {
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
                    for col in &schema.columns {
                        if col.required && matches!(values[col.position as usize], Value::Empty) {
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
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    self.view_registry.mark_dependents_dirty(table);
                    Ok(QueryResult::Modified(1))
                } else {
                    // No conflict: insert.
                    self.catalog
                        .insert(table, &values)
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    self.view_registry.mark_dependents_dirty(table);
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
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                        out_rows.push(row);
                    }
                    self.view_registry.mark_dependents_dirty(table);
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
                    let fast_patch: Option<Vec<FastPatch>> = {
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
                                .map_err(|e| QueryError::StorageError(e.to_string()))?;
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
                                .map_err(|e| QueryError::StorageError(e.to_string()))?;
                            count += 1;
                        }
                        self.view_registry.mark_dependents_dirty(table);
                        return Ok(QueryResult::Modified(count));
                    }

                    // Mission C Phase 10: var-column in-place shrink fast path.
                    let var_fast: Option<(usize, Option<Vec<u8>>)> = {
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
                                .map_err(|e| QueryError::StorageError(e.to_string()))?;
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
                                .map_err(|e| QueryError::StorageError(e.to_string()))?;
                            count += 1;
                        }
                        self.view_registry.mark_dependents_dirty(table);
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
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                        count += 1;
                    }
                    self.view_registry.mark_dependents_dirty(table);
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
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    count += 1;
                }
                self.view_registry.mark_dependents_dirty(table);
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
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    self.view_registry.mark_dependents_dirty(table);
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
                let delete_overflow = self.catalog.table_has_overflow(table);
                if let PlanNode::Filter {
                    input: inner,
                    predicate,
                } = input.as_ref()
                {
                    if let PlanNode::SeqScan { table: t } = inner.as_ref() {
                        if t == table && !delete_overflow {
                            let schema = self
                                .catalog
                                .schema(table)
                                .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                            let columns: Vec<String> =
                                schema.columns.iter().map(|c| c.name.clone()).collect();
                            let fast = FastLayout::new(schema);
                            if let Some(compiled) =
                                compile_predicate(predicate, &columns, &fast, schema)
                            {
                                // Mission B2: logged variant so every
                                // matched rid hits the WAL during the
                                // single-pass scan. Structure of the
                                // fused scan is unchanged — only the
                                // hook closure now also appends.
                                crate::cancel::check()?;
                                let count = self
                                    .catalog
                                    .scan_delete_matching_logged(table, |data| compiled(data))
                                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                                self.view_registry.mark_dependents_dirty(table);
                                return Ok(QueryResult::Modified(count));
                            }
                        }
                    }
                } else if let PlanNode::SeqScan { table: t } = input.as_ref() {
                    if t == table && !delete_overflow {
                        // `delete from T` with no predicate — every live
                        // row matches. One pass is still the right shape.
                        // Mission B2: logged variant — see above.
                        crate::cancel::check()?;
                        let count = self
                            .catalog
                            .scan_delete_matching_logged(table, |_| true)
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                        self.view_registry.mark_dependents_dirty(table);
                        return Ok(QueryResult::Modified(count));
                    }
                }

                let matching_rids = self.collect_rids_for_mutation(input, table)?;
                crate::cancel::check()?;
                let count = self
                    .catalog
                    .delete_many(table, &matching_rids)
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                self.view_registry.mark_dependents_dirty(table);
                Ok(QueryResult::Modified(count))
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
                for (_, row) in self
                    .catalog
                    .scan(table)
                    .map_err(|e| QueryError::StorageError(e.to_string()))?
                {
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
                let left_result = self.execute_plan(left)?;
                let right_result = self.execute_plan(right)?;
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
                let result = self.execute_plan(input)?;
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
                let result = self.execute_plan(input)?;
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
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                // Declaring a field `unique` auto-creates a unique B+tree
                // index, which is where uniqueness is enforced on writes.
                for f in fields.iter().filter(|f| f.unique) {
                    self.catalog
                        .create_index_unique(name, &f.name, true)
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                }
                Ok(QueryResult::Created(name.clone()))
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
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
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
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
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
                            .map_err(|error| QueryError::StorageError(error.to_string()))?;
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
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
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
                            .map_err(|error| QueryError::StorageError(error.to_string()))?;
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
                        for (_, row) in tbl.scan() {
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
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
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
                        .map_err(|error| QueryError::StorageError(error.to_string()))?;
                    Ok(QueryResult::Executed {
                        message: format!(
                            "expression index {} on '{}' dropped",
                            existing.index_id, table
                        ),
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
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                Ok(QueryResult::Executed {
                    message: format!("table '{name}' dropped"),
                })
            }

            PlanNode::ListTypes => self.introspect_list_types(),

            PlanNode::Describe { table } => self.introspect_describe(table),

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
                let result = self.execute_plan(input)?;
                execute_window(result, windows, self.query_memory_limit)
            }

            PlanNode::Union { left, right, all } => {
                let left_result = self.execute_plan(left)?;
                let right_result = self.execute_plan(right)?;
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
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
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
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
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
                    if let Some(compiled) = compile_predicate(&synth_pred, &columns, &fast, schema)
                    {
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
                for (_, row) in tbl.scan() {
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
                                for (_, row) in tbl.scan() {
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
                    if let Some(compiled) = compile_predicate(&synth, &columns, &fast, schema) {
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
                for (_, row) in tbl.scan() {
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
        let schema = self.derive_view_schema(name, &columns, &rows);
        // Create the backing table and insert the result rows.
        crate::cancel::check()?;
        self.catalog
            .create_table(schema)
            .map_err(|e| QueryError::StorageError(e.to_string()))?;
        for row in &rows {
            self.catalog
                .insert(name, row)
                .map_err(|e| QueryError::StorageError(e.to_string()))?;
        }
        // Determine which base tables this view depends on by parsing the query.
        let depends_on = self.extract_view_deps(query_text);
        self.view_registry
            .register(ViewDef {
                name: name.to_string(),
                query: query_text.to_string(),
                depends_on,
                dirty: false,
            })
            .map_err(|e| QueryError::StorageError(e.to_string()))?;
        Ok(())
    }

    /// Refresh a materialized view: re-execute its source query and replace
    /// the backing table's contents.
    fn refresh_view(&mut self, name: &str) -> Result<(), QueryError> {
        let def = self
            .view_registry
            .get(name)
            .ok_or_else(|| format!("materialized view '{name}' not found"))?;
        let query_text = def.query.clone();
        // Execute the source query.
        let result = self.execute_powql(&query_text)?;
        let (_columns, rows) = match result {
            QueryResult::Rows { columns, rows } => (columns, rows),
            _ => return Err("view source query must be a SELECT".into()),
        };
        // Clear old data and insert fresh results. Mission B2: logged
        // variant — view refreshes are a mutation and crash recovery
        // must see them.
        crate::cancel::check()?;
        self.catalog
            .scan_delete_matching_logged(name, |_| true)
            .map_err(|e| QueryError::StorageError(e.to_string()))?;
        for row in &rows {
            self.catalog
                .insert(name, row)
                .map_err(|e| QueryError::StorageError(e.to_string()))?;
        }
        self.view_registry.mark_clean(name);
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
            .map_err(|e| QueryError::StorageError(e.to_string()))?;
        self.catalog
            .drop_table(name)
            .map_err(|e| QueryError::StorageError(e.to_string()))?;
        Ok(())
    }

    /// Derive a storage `Schema` for a view's backing table from query
    /// result column names and the first row's types.
    fn derive_view_schema(&self, name: &str, columns: &[String], rows: &[Vec<Value>]) -> Schema {
        use powdb_storage::types::{ColumnDef, TypeId};
        let cols: Vec<ColumnDef> = columns
            .iter()
            .enumerate()
            .map(|(i, col_name)| {
                let type_id = rows
                    .first()
                    .and_then(|row| row.get(i))
                    .map(|v| v.type_id())
                    .unwrap_or(TypeId::Str);
                ColumnDef {
                    name: col_name.clone(),
                    type_id,
                    required: false,
                    position: i as u16,
                }
            })
            .collect();
        Schema {
            table_name: name.to_string(),
            columns: cols,
        }
    }

    /// Extract base table dependencies from a view's source query by
    /// parsing it and collecting the source table name.
    fn extract_view_deps(&self, query_text: &str) -> Vec<String> {
        use crate::parser::parse;
        match parse(query_text) {
            Ok(Statement::Query(q)) => {
                let mut deps = vec![q.source.clone()];
                for j in &q.joins {
                    deps.push(j.source.clone());
                }
                deps
            }
            _ => Vec::new(),
        }
    }
}
