//! The execute_plan method and associated helpers.

use crate::ast::*;
use crate::plan::*;
use crate::result::{QueryError, QueryResult};
use powdb_storage::catalog::Catalog;
use powdb_storage::row::{decode_column, decode_row, patch_var_column_in_place, RowLayout};
use powdb_storage::types::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

use super::compiled::*;
use super::eval::*;
use super::row_body_base;
use super::{check_join_limit, Engine, MAX_SORT_ROWS};
use powdb_storage::view::ViewDef;

impl Engine {
    /// `schema` — one result row per type: name + column count. Read-only;
    /// reads live catalog state, so a cached plan can never serve a stale list.
    pub(super) fn introspect_list_types(&self) -> Result<QueryResult, QueryError> {
        let rows: Vec<Vec<Value>> = self
            .catalog
            .list_tables()
            .iter()
            .map(|name| {
                let cols = self
                    .catalog
                    .schema(name)
                    .map(|s| s.columns.len())
                    .unwrap_or(0) as i64;
                vec![Value::Str((*name).to_string()), Value::Int(cols)]
            })
            .collect();
        Ok(QueryResult::Rows {
            columns: vec!["name".to_string(), "columns".to_string()],
            rows,
        })
    }

    /// `describe <Type>` — one result row per column: name, type, nullability,
    /// and index kind (`unique` / `index` / empty). Read-only.
    pub(super) fn introspect_describe(&self, table: &str) -> Result<QueryResult, QueryError> {
        let schema = self
            .catalog
            .schema(table)
            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
        let rows: Vec<Vec<Value>> = schema
            .columns
            .iter()
            .map(|c| {
                let index = if self.catalog.has_index(table, &c.name) {
                    match self.catalog.is_index_unique(table, &c.name) {
                        Some(true) => "unique",
                        _ => "index",
                    }
                } else {
                    ""
                };
                vec![
                    Value::Str(c.name.clone()),
                    Value::Str(type_id_to_name(c.type_id).to_string()),
                    Value::Bool(!c.required),
                    Value::Str(index.to_string()),
                ]
            })
            .collect();
        Ok(QueryResult::Rows {
            columns: vec![
                "column".to_string(),
                "type".to_string(),
                "nullable".to_string(),
                "index".to_string(),
            ],
            rows,
        })
    }

    pub fn execute_plan(&mut self, plan: &PlanNode) -> Result<QueryResult, QueryError> {
        // Refuse any plan whose evaluable expressions still carry an aggregate
        // FunctionCall the grouped-aggregate planner could not lower. Without
        // this, such an aggregate would reach eval_expr and silently evaluate
        // to Empty (a wrong answer). The outermost call validates the whole
        // tree before any row is produced.
        validate_no_stray_aggregates(plan)?;
        match plan {
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
                let rows: Vec<Vec<Value>> = self
                    .catalog
                    .scan(table)
                    .map_err(|e| QueryError::StorageError(e.to_string()))?
                    .map(|(_, row)| row)
                    .collect();
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
                            for row in rows {
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

                // Fast path: fuse Filter + SeqScan into a zero-copy streaming
                // loop. Uses decode_column() to evaluate the predicate on only
                // the columns it references, avoiding heap allocations for
                // String/Bytes columns that aren't part of the filter.
                if let PlanNode::SeqScan { table } = input.as_ref() {
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
                    if let Some(compiled) = compile_predicate(predicate, &columns, &fast, &schema) {
                        self.catalog
                            .for_each_row_raw(table, |_rid, data| {
                                if compiled(data) {
                                    rows.push(decode_row(&schema, data));
                                }
                            })
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    } else {
                        let pred_cols = predicate_column_indices(predicate, &columns);
                        self.catalog
                            .for_each_row_raw(table, |_rid, data| {
                                let pred_row =
                                    decode_selective(&schema, &row_layout, data, &pred_cols);
                                if eval_predicate(predicate, &pred_row, &columns) {
                                    rows.push(decode_row(&schema, data));
                                }
                            })
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    }

                    return Ok(QueryResult::Rows { columns, rows });
                }

                // General path: materialise then filter.
                let result = self.execute_plan(input)?;
                match result {
                    QueryResult::Rows { columns, rows } => {
                        let filtered: Vec<Vec<Value>> = rows
                            .into_iter()
                            .filter(|row| eval_predicate(predicate, row, &columns))
                            .collect();
                        Ok(QueryResult::Rows {
                            columns,
                            rows: filtered,
                        })
                    }
                    _ => Err("filter requires row input".into()),
                }
            }

            PlanNode::Project { input, fields } => {
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

                    if tbl.has_index(column) {
                        let layout = RowLayout::new(&schema);
                        let rids = tbl.index_lookup_all(column, &key_value);
                        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(rids.len());
                        for rid in rids {
                            if let Some(data) = tbl.heap.get(rid) {
                                let row: Vec<Value> = proj_indices
                                    .iter()
                                    .map(|&ci| decode_column(&schema, &layout, &data, ci))
                                    .collect();
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
                            let sort_field = &keys[0].field;
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
                        let proj_rows: Vec<Vec<Value>> = rows
                            .iter()
                            .map(|row| {
                                fields
                                    .iter()
                                    .map(|f| eval_expr(&f.expr, row, &columns))
                                    .collect()
                            })
                            .collect();
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
                        let key_indices: Vec<(usize, bool)> = keys
                            .iter()
                            .map(|k| {
                                columns
                                    .iter()
                                    .position(|c| c == &k.field)
                                    .map(|idx| (idx, k.descending))
                                    .ok_or_else(|| QueryError::ColumnNotFound {
                                        table: String::new(),
                                        column: k.field.clone(),
                                    })
                            })
                            .collect::<Result<_, QueryError>>()?;
                        rows.sort_by(|a, b| {
                            for &(col_idx, descending) in &key_indices {
                                let cmp = a[col_idx].cmp(&b[col_idx]);
                                let cmp = if descending { cmp.reverse() } else { cmp };
                                if cmp != std::cmp::Ordering::Equal {
                                    return cmp;
                                }
                            }
                            std::cmp::Ordering::Equal
                        });
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
                    QueryResult::Rows { columns, rows } => Ok(QueryResult::Rows {
                        columns,
                        rows: rows.into_iter().take(n).collect(),
                    }),
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
                    QueryResult::Rows { columns, rows } => Ok(QueryResult::Rows {
                        columns,
                        rows: rows.into_iter().skip(n).collect(),
                    }),
                    _ => Err("offset requires row input".into()),
                }
            }

            PlanNode::Aggregate {
                input,
                function,
                field,
            } => {
                // Fast path: count() over SeqScan — count rows without any decode
                if *function == AggFunc::Count {
                    if let PlanNode::SeqScan { table } = input.as_ref() {
                        // Auto-refresh a dirty materialized view before
                        // counting it — otherwise count(View) returns stale
                        // data after an underlying mutation (F3).
                        if self.view_registry.is_dirty(table) {
                            self.refresh_view(table)?;
                        }
                        let mut count: i64 = 0;
                        self.catalog
                            .for_each_row_raw(table, |_rid, _data| {
                                count += 1;
                            })
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                        return Ok(QueryResult::Scalar(Value::Int(count)));
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
                                self.catalog
                                    .for_each_row_raw(table, |_rid, data| {
                                        if compiled(data) {
                                            count += 1;
                                        }
                                    })
                                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                                return Ok(QueryResult::Scalar(Value::Int(count)));
                            }

                            // Fallback: decode predicate columns
                            let pred_cols = predicate_column_indices(predicate, &columns);
                            let mut count: i64 = 0;
                            self.catalog
                                .for_each_row_raw(table, |_rid, data| {
                                    let pred_row =
                                        decode_selective(&schema, &row_layout, data, &pred_cols);
                                    if eval_predicate(predicate, &pred_row, &columns) {
                                        count += 1;
                                    }
                                })
                                .map_err(|e| QueryError::StorageError(e.to_string()))?;

                            return Ok(QueryResult::Scalar(Value::Int(count)));
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
                    if let Some(col) = field.as_ref() {
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
                        match function {
                            AggFunc::Count => {
                                Ok(QueryResult::Scalar(Value::Int(rows.len() as i64)))
                            }
                            AggFunc::CountDistinct => {
                                let col = field.as_ref().ok_or("count distinct requires field")?;
                                let idx = columns
                                    .iter()
                                    .position(|c| c == col)
                                    .ok_or("col not found")?;
                                let mut seen = std::collections::HashSet::new();
                                for row in &rows {
                                    let v = &row[idx];
                                    if !v.is_empty() {
                                        seen.insert(v.clone());
                                    }
                                }
                                Ok(QueryResult::Scalar(Value::Int(seen.len() as i64)))
                            }
                            AggFunc::Avg => {
                                let col = field.as_ref().ok_or("avg requires field")?;
                                let idx = columns
                                    .iter()
                                    .position(|c| c == col)
                                    .ok_or("col not found")?;
                                let mut count: u64 = 0;
                                let sum: f64 = rows
                                    .iter()
                                    .filter_map(|r| match &r[idx] {
                                        Value::Int(v) => Some(*v as f64),
                                        Value::Float(v) => Some(*v),
                                        _ => None,
                                    })
                                    .inspect(|_| count += 1)
                                    .sum();
                                if count == 0 {
                                    Ok(QueryResult::Scalar(Value::Empty))
                                } else {
                                    Ok(QueryResult::Scalar(Value::Float(sum / count as f64)))
                                }
                            }
                            AggFunc::Sum => {
                                let col = field.as_ref().ok_or("sum requires field")?;
                                let idx = columns
                                    .iter()
                                    .position(|c| c == col)
                                    .ok_or("col not found")?;
                                // Track int and float contributions separately so
                                // Float columns (and mixed Int/Float rows) don't get
                                // silently dropped as they did in the Int-only
                                // version. If any Float is present, the whole sum
                                // promotes to Float — matching Avg's semantics.
                                let mut int_sum: i64 = 0;
                                let mut float_sum: f64 = 0.0;
                                let mut saw_float = false;
                                for r in &rows {
                                    match &r[idx] {
                                        Value::Int(v) => int_sum += *v,
                                        Value::Float(v) => {
                                            float_sum += *v;
                                            saw_float = true;
                                        }
                                        _ => {}
                                    }
                                }
                                let result = if saw_float {
                                    Value::Float(float_sum + int_sum as f64)
                                } else {
                                    Value::Int(int_sum)
                                };
                                Ok(QueryResult::Scalar(result))
                            }
                            AggFunc::Min | AggFunc::Max => {
                                let col = field.as_ref().ok_or("min/max requires field")?;
                                let idx = columns
                                    .iter()
                                    .position(|c| c == col)
                                    .ok_or("col not found")?;
                                let vals: Vec<&Value> = rows.iter().map(|r| &r[idx]).collect();
                                let result = if *function == AggFunc::Min {
                                    vals.into_iter().min().cloned()
                                } else {
                                    vals.into_iter().max().cloned()
                                };
                                Ok(QueryResult::Scalar(result.unwrap_or(Value::Empty)))
                            }
                        }
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
                    rids.into_iter().next().and_then(|rid| {
                        tbl.heap
                            .get(rid)
                            .map(|data| (rid, decode_row(&tbl.schema, &data)))
                    })
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
                        let schema = &tbl.schema;
                        let all_fixed_nonnull = resolved_assignments.iter().all(|(idx, val)| {
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
                        for rid in matching_rids {
                            // Mission B2: WAL-log every patch so crash
                            // recovery replays the update. Same mutation
                            // closure as before — the wrapper just sandwiches
                            // it between a hot-page read and a WAL append.
                            let ok = self
                                .catalog
                                .update_row_bytes_logged(table, rid, |row| {
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
                            }
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
                        let schema = &tbl.schema;
                        let is_single = resolved_assignments.len() == 1;
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
                    for rid in &matching_rids {
                        if let Some(row) = self.catalog.get(table, *rid) {
                            out_rows.push(row);
                        }
                    }
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
                if let PlanNode::Filter {
                    input: inner,
                    predicate,
                } = input.as_ref()
                {
                    if let PlanNode::SeqScan { table: t } = inner.as_ref() {
                        if t == table {
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
                    if t == table {
                        // `delete from T` with no predicate — every live
                        // row matches. One pass is still the right shape.
                        // Mission B2: logged variant — see above.
                        let count = self
                            .catalog
                            .scan_delete_matching_logged(table, |_| true)
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                        self.view_registry.mark_dependents_dirty(table);
                        return Ok(QueryResult::Modified(count));
                    }
                }

                let matching_rids = self.collect_rids_for_mutation(input, table)?;
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
                let rows: Vec<Vec<Value>> = self
                    .catalog
                    .scan(table)
                    .map_err(|e| QueryError::StorageError(e.to_string()))?
                    .map(|(_, row)| row)
                    .collect();
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

                // Hash-join fast path.
                if !matches!(kind, JoinKind::Cross) {
                    if let Some(pred) = on {
                        if let Some((l_idx, r_idx)) =
                            try_extract_equi_join_keys(pred, &left_columns, &right_columns)
                        {
                            let result = hash_join(
                                left_columns,
                                left_rows,
                                right_columns,
                                right_rows,
                                l_idx,
                                r_idx,
                                *kind,
                            );
                            if let QueryResult::Rows { ref rows, .. } = result {
                                check_join_limit(rows.len())?;
                            }
                            return Ok(result);
                        }
                    }
                }

                // Nested-loop fallback.
                let n_left = left_columns.len();
                let n_right = right_columns.len();
                let mut columns = Vec::with_capacity(n_left + n_right);
                columns.extend(left_columns);
                columns.extend(right_columns);

                let mut rows: Vec<Vec<Value>> = Vec::with_capacity(left_rows.len());
                let mut combined: Vec<Value> = Vec::with_capacity(n_left + n_right);

                for left_row in &left_rows {
                    let mut matched = false;
                    for right_row in &right_rows {
                        combined.clear();
                        combined.extend_from_slice(left_row);
                        combined.extend_from_slice(right_row);
                        let keep = match kind {
                            JoinKind::Cross => true,
                            JoinKind::Inner | JoinKind::LeftOuter => match on {
                                Some(pred) => eval_predicate(pred, &combined, &columns),
                                // Missing `on` for non-cross joins is a
                                // parser error, but if it slips through we
                                // treat it as "match everything".
                                None => true,
                            },
                            // RightOuter is rewritten to LeftOuter by the
                            // planner, so we never see it here.
                            JoinKind::RightOuter => {
                                unreachable!("planner rewrites RightOuter to LeftOuter")
                            }
                        };
                        if keep {
                            rows.push(combined.clone());
                            check_join_limit(rows.len())?;
                            matched = true;
                        }
                    }
                    if !matched && matches!(kind, JoinKind::LeftOuter) {
                        let mut row = Vec::with_capacity(n_left + n_right);
                        row.extend_from_slice(left_row);
                        row.resize(n_left + n_right, Value::Empty);
                        rows.push(row);
                        check_join_limit(rows.len())?;
                    }
                }

                Ok(QueryResult::Rows { columns, rows })
            }

            PlanNode::Distinct { input } => {
                let result = self.execute_plan(input)?;
                match result {
                    QueryResult::Rows { columns, rows } => {
                        let mut seen = std::collections::HashSet::new();
                        let mut unique_rows = Vec::new();
                        for row in rows {
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
                    column,
                    if_not_exists: _,
                } => {
                    // `add index` is already idempotent (no-op if the index
                    // exists), so `if not exists` is accepted for symmetry but
                    // does not change behavior.
                    self.catalog
                        .create_index(table, column)
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    Ok(QueryResult::Executed {
                        message: format!("index on '{table}.{column}' created"),
                    })
                }
                AlterAction::AddUnique {
                    column,
                    if_not_exists,
                } => {
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
                        // No DropIndex exists, so we cannot upgrade an existing
                        // non-unique index in place — reject it cleanly.
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
                        let col_idx = tbl.schema.column_index(column).ok_or_else(|| {
                            QueryError::ColumnNotFound {
                                table: table.to_string(),
                                column: column.clone(),
                            }
                        })?;
                        let mut seen = std::collections::HashSet::new();
                        for (_, row) in tbl.scan() {
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
                    self.catalog
                        .create_index_unique(table, column, true)
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    Ok(QueryResult::Executed {
                        message: format!("unique index on '{table}.{column}' created"),
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
                execute_window(result, windows)
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
                if *all {
                    // UNION ALL — just concatenate.
                    combined.extend(right_rows);
                } else {
                    // UNION — deduplicate using the same HashSet approach
                    // as DISTINCT. Value already implements Hash + Eq.
                    let mut seen = std::collections::HashSet::new();
                    for row in &combined {
                        seen.insert(row.clone());
                    }
                    for row in right_rows {
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
                let text = format_plan_tree(input, 0);
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
                let columns: Vec<String> =
                    tbl.schema.columns.iter().map(|c| c.name.clone()).collect();

                // Fast path: the table has a B-tree on this column.
                // Uses index_lookup_all to return ALL matching rows for
                // both unique and non-unique indexes.
                if tbl.has_index(column) {
                    let rids = tbl.index_lookup_all(column, &key_value);
                    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(rids.len());
                    for rid in rids {
                        if let Some(data) = tbl.heap.get(rid) {
                            rows.push(decode_row(&tbl.schema, &data));
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
                let schema = &tbl.schema;
                let fast = FastLayout::new(schema);
                let synth_pred = Expr::BinaryOp(
                    Box::new(Expr::Field(column.clone())),
                    BinOp::Eq,
                    Box::new(key.clone()),
                );
                if let Some(compiled) = compile_predicate(&synth_pred, &columns, &fast, schema) {
                    // Mission F: skip the first 4 Vec doublings.
                    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(64);
                    self.catalog
                        .for_each_row_raw(table, |_rid, data| {
                            if compiled(data) {
                                rows.push(decode_row(schema, data));
                            }
                        })
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    return Ok(QueryResult::Rows { columns, rows });
                }

                // Last resort: slow eq-check on materialised rows.
                let col_idx =
                    schema
                        .column_index(column)
                        .ok_or_else(|| QueryError::ColumnNotFound {
                            table: String::new(),
                            column: column.clone(),
                        })?;
                let rows: Vec<Vec<Value>> = tbl
                    .scan()
                    .filter_map(|(_, row)| {
                        if row[col_idx] == key_value {
                            Some(row)
                        } else {
                            None
                        }
                    })
                    .collect();
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
                let columns: Vec<String> =
                    tbl.schema.columns.iter().map(|c| c.name.clone()).collect();
                let schema = &tbl.schema;

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
                            for rid in rids {
                                if let Some(data) = tbl.heap.get(rid) {
                                    let row = decode_row(schema, &data);
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
                                let rows: Vec<Vec<Value>> =
                                    tbl.scan().map(|(_, row)| row).collect();
                                return Ok(QueryResult::Rows { columns, rows });
                            }
                        };
                        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(hits.len());
                        for (key, rid) in hits {
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
                            if let Some(data) = tbl.heap.get(rid) {
                                rows.push(decode_row(schema, &data));
                            }
                        }
                        return Ok(QueryResult::Rows { columns, rows });
                    }
                }

                // Fallback: no index — synthesize range predicate and scan.
                let fast = FastLayout::new(schema);
                let synth = synthesize_range_predicate(column, start, end);
                if let Some(compiled) = compile_predicate(&synth, &columns, &fast, schema) {
                    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(64);
                    self.catalog
                        .for_each_row_raw(table, |_rid, data| {
                            if compiled(data) {
                                rows.push(decode_row(schema, data));
                            }
                        })
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    return Ok(QueryResult::Rows { columns, rows });
                }

                let col_idx =
                    schema
                        .column_index(column)
                        .ok_or_else(|| QueryError::ColumnNotFound {
                            table: String::new(),
                            column: column.clone(),
                        })?;
                let rows: Vec<Vec<Value>> = tbl
                    .scan()
                    .filter(|(_, row)| {
                        range_matches(
                            &row[col_idx],
                            &start_val,
                            start_inclusive,
                            &end_val,
                            end_inclusive,
                        )
                    })
                    .map(|(_, row)| row)
                    .collect();
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

    // ─── Specialized fast paths ─────────────────────────────────────────────
    //
    // These methods are helpers for the `execute_plan` match arms above.
    // Each returns `Ok(Some(result))` when the fast path fires, `Ok(None)`
    // when the shape isn't supported (caller falls back to generic code).

    /// Aggregate sum/avg/min/max over a single fixed-size i64 column, with
    /// an optional compiled filter predicate. Walks raw row bytes — zero
    /// per-row allocation. Uses i128 accumulator for sum/avg overflow safety.
    pub(super) fn agg_single_col_fast(
        &self,
        table: &str,
        col: &str,
        function: AggFunc,
        predicate: Option<&Expr>,
    ) -> Result<Option<QueryResult>, QueryError> {
        let schema = self
            .catalog
            .schema(table)
            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
            .clone();
        let columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
        let col_idx = match schema.column_index(col) {
            Some(i) => i,
            None => return Ok(None),
        };
        // Only fast-path fixed-size numeric columns (Int/Float) for
        // sum/avg/min/max/count. Mission D10: Float parity — prior version
        // bailed on Float columns, forcing them through the generic row-
        // decoding path that allocated a Vec<Value> per row and dispatched
        // on Value::cmp for every compare. f64 decode is structurally the
        // same as i64 (load 8 bytes, cast), so the fast path handles both.
        let col_type = schema.columns[col_idx].type_id;
        if col_type != TypeId::Int && col_type != TypeId::Float {
            return Ok(None);
        }

        let fast = FastLayout::new(&schema);
        // Mission C Phase 20b: inline the numeric-column reader instead of
        // building a `Box<dyn Fn>`. Eliminates 100K vtable dispatches per
        // 100K-row agg scan — every reader call folds directly into the
        // hot loop below.
        let byte_offset = match fast.fixed_offsets[col_idx] {
            Some(o) => o,
            None => return Ok(None),
        };
        let bitmap_byte = col_idx / 8;
        let bitmap_bit = (col_idx % 8) as u32;
        let body_data_offset = 2 + fast.bitmap_size + byte_offset;

        // Optional compiled filter.
        let compiled_pred: Option<CompiledPredicate> = match predicate {
            Some(pred) => match compile_predicate(pred, &columns, &fast, &schema) {
                Some(c) => Some(c),
                None => return Ok(None), // let generic path handle it
            },
            None => None,
        };

        // Mission C Phase 20b: specialize the inner loop per aggregate
        // function. The previous version ran a `match function { ... }`
        // *inside* the closure, which kept LLVM from producing optimal
        // scalar code for each variant (agg_max regressed ~23% vs the
        // baseline Box<dyn Fn> version even though per-row vtable cost
        // should have been strictly lower). Pushing the match out of the
        // hot loop lets each specialized body fold cleanly into
        // `for_each_row_raw` and removes a captured `AggFunc` + match
        // dispatch per row.
        //
        // Mission D10: same specialisation applies to the Float branch.
        // For Min/Max we use `f64::total_cmp` so the result matches
        // `Value::Ord` — this is the same ordering ORDER BY and the
        // top-N sort fast path use, keeping semantics consistent across
        // read paths (NaN compares as greatest, -0.0 < +0.0 for
        // deterministic tie-breaking).
        //
        // Mission D11 Phase 1: each inner loop now splits on presence of
        // a predicate (`if let Some(pred) = &compiled_pred`) so the hot
        // body never re-tests `Option` per row, and reads column bytes
        // via `read_i64_unchecked` / `read_f64_unchecked` helpers that
        // drop two bounds checks per row (null bitmap byte + value
        // slice). Safety is carried by the `FastLayout` invariant that
        // `data_offset + 8 <= row_len` for any fixed-size column; see
        // the helper doc comments. Hot loops are macro-generated so the
        // with-pred / no-pred split can't drift between variants.
        let result = match col_type {
            TypeId::Int => match function {
                AggFunc::Sum | AggFunc::Avg => {
                    let mut sum_i128: i128 = 0;
                    let mut count: i64 = 0;
                    agg_int_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: i64| {
                            count += 1;
                            sum_i128 += v as i128;
                        }
                    );
                    if matches!(function, AggFunc::Sum) {
                        let clamped = sum_i128.clamp(i64::MIN as i128, i64::MAX as i128) as i64;
                        QueryResult::Scalar(Value::Int(clamped))
                    } else if count == 0 {
                        QueryResult::Scalar(Value::Empty)
                    } else {
                        let avg = (sum_i128 as f64) / (count as f64);
                        QueryResult::Scalar(Value::Float(avg))
                    }
                }
                AggFunc::Min => {
                    let mut min_v: Option<i64> = None;
                    agg_int_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: i64| {
                            min_v = Some(match min_v {
                                Some(m) => m.min(v),
                                None => v,
                            });
                        }
                    );
                    QueryResult::Scalar(min_v.map(Value::Int).unwrap_or(Value::Empty))
                }
                AggFunc::Max => {
                    let mut max_v: Option<i64> = None;
                    agg_int_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: i64| {
                            max_v = Some(match max_v {
                                Some(m) => m.max(v),
                                None => v,
                            });
                        }
                    );
                    QueryResult::Scalar(max_v.map(Value::Int).unwrap_or(Value::Empty))
                }
                AggFunc::Count => {
                    let mut count: i64 = 0;
                    agg_int_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |_v: i64| {
                            count += 1;
                        }
                    );
                    QueryResult::Scalar(Value::Int(count))
                }
                AggFunc::CountDistinct => {
                    let mut seen = rustc_hash::FxHashSet::default();
                    agg_int_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: i64| {
                            seen.insert(v);
                        }
                    );
                    QueryResult::Scalar(Value::Int(seen.len() as i64))
                }
            },
            TypeId::Float => match function {
                AggFunc::Sum => {
                    // Use a single f64 accumulator. Naive summation is
                    // sufficient for MVP parity; if precision becomes an
                    // issue on long scans we can upgrade to Kahan–Neumaier
                    // compensated sum (~2x scalar cost, zero error growth).
                    let mut sum: f64 = 0.0;
                    agg_float_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: f64| {
                            sum += v;
                        }
                    );
                    QueryResult::Scalar(Value::Float(sum))
                }
                AggFunc::Avg => {
                    let mut sum: f64 = 0.0;
                    let mut count: i64 = 0;
                    agg_float_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: f64| {
                            sum += v;
                            count += 1;
                        }
                    );
                    if count == 0 {
                        QueryResult::Scalar(Value::Empty)
                    } else {
                        QueryResult::Scalar(Value::Float(sum / count as f64))
                    }
                }
                AggFunc::Min => {
                    // `total_cmp` for deterministic NaN handling (matches
                    // Value::Ord). NaN compares greatest, so Min will
                    // correctly ignore it in favour of any finite value.
                    let mut min_v: Option<f64> = None;
                    agg_float_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: f64| {
                            min_v = Some(match min_v {
                                Some(m) => {
                                    if v.total_cmp(&m).is_lt() {
                                        v
                                    } else {
                                        m
                                    }
                                }
                                None => v,
                            });
                        }
                    );
                    QueryResult::Scalar(min_v.map(Value::Float).unwrap_or(Value::Empty))
                }
                AggFunc::Max => {
                    let mut max_v: Option<f64> = None;
                    agg_float_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: f64| {
                            max_v = Some(match max_v {
                                Some(m) => {
                                    if v.total_cmp(&m).is_gt() {
                                        v
                                    } else {
                                        m
                                    }
                                }
                                None => v,
                            });
                        }
                    );
                    QueryResult::Scalar(max_v.map(Value::Float).unwrap_or(Value::Empty))
                }
                AggFunc::Count => {
                    let mut count: i64 = 0;
                    agg_float_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |_v: f64| {
                            count += 1;
                        }
                    );
                    QueryResult::Scalar(Value::Int(count))
                }
                AggFunc::CountDistinct => {
                    // Hash on `f64::to_bits` — matches `Value::Hash`, so
                    // distinct NaN bit patterns count as distinct and
                    // -0.0/+0.0 count as distinct. Consistent with how
                    // Float values are hashed in every other DISTINCT /
                    // GROUP BY path.
                    let mut seen = rustc_hash::FxHashSet::default();
                    agg_float_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: f64| {
                            seen.insert(v.to_bits());
                        }
                    );
                    QueryResult::Scalar(Value::Int(seen.len() as i64))
                }
            },
            _ => unreachable!("type guard above restricts to Int/Float"),
        };
        Ok(Some(result))
    }

    /// `Project(Limit(Filter(SeqScan)))` and `Project(Limit(SeqScan))`.
    /// Streams rows, decodes only projected columns, stops at the limit.
    pub(super) fn project_filter_limit_fast(
        &self,
        table: &str,
        fields: &[ProjectField],
        limit: usize,
        predicate: Option<&Expr>,
    ) -> Result<Option<QueryResult>, QueryError> {
        let schema = self
            .catalog
            .schema(table)
            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
            .clone();
        let all_columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();

        // Each projection field must be a simple `.field` reference for this
        // fast path. Aliased or computed fields fall through.
        let mut proj_indices: Vec<usize> = Vec::with_capacity(fields.len());
        let mut proj_columns: Vec<String> = Vec::with_capacity(fields.len());
        for f in fields {
            let name = match &f.expr {
                Expr::Field(n) => n.clone(),
                _ => return Ok(None),
            };
            let idx = match all_columns.iter().position(|c| c == &name) {
                Some(i) => i,
                None => return Ok(None),
            };
            proj_indices.push(idx);
            proj_columns.push(f.alias.clone().unwrap_or(name));
        }

        let fast = FastLayout::new(&schema);
        let row_layout = RowLayout::new(&schema);

        let compiled_pred: Option<CompiledPredicate> = match predicate {
            Some(pred) => match compile_predicate(pred, &all_columns, &fast, &schema) {
                Some(c) => Some(c),
                None => return Ok(None),
            },
            None => None,
        };

        let mut out: Vec<Vec<Value>> = Vec::with_capacity(limit.min(1024));
        // Mission D2: use try_for_each_row_raw to actually stop iterating
        // once the limit is reached. The previous `done` flag only short-
        // circuited the closure body, so a `limit 100` over 100K rows still
        // walked all 100K slots — burning ~30x SQLite on scan_filter_project_top100.
        self.catalog
            .try_for_each_row_raw(table, |_rid, data| {
                use std::ops::ControlFlow;
                if let Some(ref pred) = compiled_pred {
                    if !pred(data) {
                        return ControlFlow::Continue(());
                    }
                }
                let row: Vec<Value> = proj_indices
                    .iter()
                    .map(|&ci| decode_column(&schema, &row_layout, data, ci))
                    .collect();
                out.push(row);
                if out.len() >= limit {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .map_err(|e| QueryError::StorageError(e.to_string()))?;

        Ok(Some(QueryResult::Rows {
            columns: proj_columns,
            rows: out,
        }))
    }

    /// `Project(Limit(Sort(Filter(SeqScan))))` and `Project(Limit(Sort(SeqScan)))`.
    /// Bounded top-N heap over the sort key. Only the sort key needs to be
    /// read per row; projected columns are decoded only for the final
    /// winning rows when the heap drains.
    pub(super) fn project_filter_sort_limit_fast(
        &self,
        table: &str,
        fields: &[ProjectField],
        sort_field: &str,
        descending: bool,
        limit: usize,
        predicate: Option<&Expr>,
    ) -> Result<Option<QueryResult>, QueryError> {
        if limit == 0 {
            // Degenerate case — empty result. Let the generic path handle it
            // for proper column naming.
            return Ok(None);
        }
        // The top-N heaps never hold more than `limit` rows, but `limit` is an
        // attacker-supplied literal (`order .x limit 99999999999`). Reserving
        // that capacity up front would allocate gigabytes and abort the
        // process before a single row is read. Cap the pre-allocation; the
        // heaps still grow on demand up to the true `limit`.
        const TOPN_PREALLOC_CAP: usize = 4096;
        let prealloc = limit.min(TOPN_PREALLOC_CAP);
        let schema = self
            .catalog
            .schema(table)
            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
            .clone();
        let all_columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();

        // Sort key must be a fixed-size numeric column (Int or Float).
        // Mission D10: extended from Int-only. Float sort keys use a
        // sortable-u64 transform (see `f64_to_sortable_u64`) so the heap
        // path stays keyed on `u64` and the whole branch shape is
        // identical to the Int case — no new heap types, no `total_cmp`
        // closures in the hot loop.
        let sort_idx = match schema.column_index(sort_field) {
            Some(i) => i,
            None => return Ok(None),
        };
        let sort_col_type = schema.columns[sort_idx].type_id;
        if sort_col_type != TypeId::Int && sort_col_type != TypeId::Float {
            return Ok(None);
        }

        // Each projection field must be a simple `.field`.
        let mut proj_indices: Vec<usize> = Vec::with_capacity(fields.len());
        let mut proj_columns: Vec<String> = Vec::with_capacity(fields.len());
        for f in fields {
            let name = match &f.expr {
                Expr::Field(n) => n.clone(),
                _ => return Ok(None),
            };
            let idx = match all_columns.iter().position(|c| c == &name) {
                Some(i) => i,
                None => return Ok(None),
            };
            proj_indices.push(idx);
            proj_columns.push(f.alias.clone().unwrap_or(name));
        }

        let fast = FastLayout::new(&schema);
        let row_layout = RowLayout::new(&schema);
        // Mission C Phase 20b: inline numeric-column reader (no Box<dyn Fn>).
        let sort_byte_offset = match fast.fixed_offsets[sort_idx] {
            Some(o) => o,
            None => return Ok(None),
        };
        let sort_bitmap_byte = sort_idx / 8;
        let sort_bitmap_bit = (sort_idx % 8) as u32;
        let sort_body_data_offset = 2 + fast.bitmap_size + sort_byte_offset;

        let compiled_pred: Option<CompiledPredicate> = match predicate {
            Some(pred) => match compile_predicate(pred, &all_columns, &fast, &schema) {
                Some(c) => Some(c),
                None => return Ok(None),
            },
            None => None,
        };

        // Bounded top-N heap. For `order .x desc limit N`, we want the N
        // largest values — use a min-heap so the smallest is at the top and
        // can be popped when a better candidate arrives. For ascending, use
        // a max-heap. We tie-break with a monotonic `seq` counter so the
        // result is deterministic and stable.
        //
        // To keep this simple we maintain two typed heaps and pick by
        // direction.
        let drained: Vec<Vec<u8>> = match sort_col_type {
            TypeId::Int => {
                let mut seq: u64 = 0;
                let mut heap_desc: BinaryHeap<Reverse<(i64, u64, Vec<u8>)>> =
                    BinaryHeap::with_capacity(prealloc);
                let mut heap_asc: BinaryHeap<(i64, u64, Vec<u8>)> =
                    BinaryHeap::with_capacity(prealloc);

                self.catalog
                    .for_each_row_raw(table, |_rid, data| {
                        if let Some(ref pred) = compiled_pred {
                            if !pred(data) {
                                return;
                            }
                        }
                        // Inlined int-column reader: null check + i64 decode.
                        let base = row_body_base(data);
                        let sort_data_offset = base + sort_body_data_offset;
                        if data.len() < sort_data_offset + 8
                            || data.len() <= base + 2 + sort_bitmap_byte
                        {
                            return;
                        }
                        let is_null =
                            (data[base + 2 + sort_bitmap_byte] >> sort_bitmap_bit) & 1 == 1;
                        if is_null {
                            return;
                        }
                        let key = i64::from_le_bytes(
                            data[sort_data_offset..sort_data_offset + 8]
                                .try_into()
                                .unwrap_or_else(|_| unreachable!()),
                        );
                        let id = seq;
                        seq += 1;

                        if descending {
                            if heap_desc.len() < limit {
                                heap_desc.push(Reverse((key, id, data.to_vec())));
                            } else if let Some(Reverse((top_key, _, _))) = heap_desc.peek() {
                                if key > *top_key {
                                    heap_desc.pop();
                                    heap_desc.push(Reverse((key, id, data.to_vec())));
                                }
                            }
                        } else if heap_asc.len() < limit {
                            heap_asc.push((key, id, data.to_vec()));
                        } else if let Some((top_key, _, _)) = heap_asc.peek() {
                            if key < *top_key {
                                heap_asc.pop();
                                heap_asc.push((key, id, data.to_vec()));
                            }
                        }
                    })
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;

                let mut drained: Vec<(i64, u64, Vec<u8>)> = if descending {
                    heap_desc.into_iter().map(|Reverse(t)| t).collect()
                } else {
                    heap_asc.into_iter().collect()
                };
                if descending {
                    drained.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
                } else {
                    drained.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
                }
                drained.into_iter().map(|(_, _, d)| d).collect()
            }
            TypeId::Float => {
                // Novel angle: rather than introducing a `TotalF64` newtype
                // with `Ord via total_cmp`, transform the f64 bit pattern
                // into a sortable `u64` so `BinaryHeap<u64>` orders exactly
                // like `f64::total_cmp` would. Classic trick: flip the sign
                // bit on positives, flip all bits on negatives. Result:
                // - NaN (sign=0) stays greatest, matching total_cmp
                // - -0.0 sorts before +0.0, matching total_cmp
                // - Hot loop is branch-cheap (one compare + one xor)
                let mut seq: u64 = 0;
                let mut heap_desc: BinaryHeap<Reverse<(u64, u64, Vec<u8>)>> =
                    BinaryHeap::with_capacity(prealloc);
                let mut heap_asc: BinaryHeap<(u64, u64, Vec<u8>)> =
                    BinaryHeap::with_capacity(prealloc);

                self.catalog
                    .for_each_row_raw(table, |_rid, data| {
                        if let Some(ref pred) = compiled_pred {
                            if !pred(data) {
                                return;
                            }
                        }
                        let base = row_body_base(data);
                        let sort_data_offset = base + sort_body_data_offset;
                        if data.len() < sort_data_offset + 8
                            || data.len() <= base + 2 + sort_bitmap_byte
                        {
                            return;
                        }
                        let is_null =
                            (data[base + 2 + sort_bitmap_byte] >> sort_bitmap_bit) & 1 == 1;
                        if is_null {
                            return;
                        }
                        let bits = u64::from_le_bytes(
                            data[sort_data_offset..sort_data_offset + 8]
                                .try_into()
                                .unwrap_or_else(|_| unreachable!()),
                        );
                        let key = f64_bits_to_sortable_u64(bits);
                        let id = seq;
                        seq += 1;

                        if descending {
                            if heap_desc.len() < limit {
                                heap_desc.push(Reverse((key, id, data.to_vec())));
                            } else if let Some(Reverse((top_key, _, _))) = heap_desc.peek() {
                                if key > *top_key {
                                    heap_desc.pop();
                                    heap_desc.push(Reverse((key, id, data.to_vec())));
                                }
                            }
                        } else if heap_asc.len() < limit {
                            heap_asc.push((key, id, data.to_vec()));
                        } else if let Some((top_key, _, _)) = heap_asc.peek() {
                            if key < *top_key {
                                heap_asc.pop();
                                heap_asc.push((key, id, data.to_vec()));
                            }
                        }
                    })
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;

                let mut drained: Vec<(u64, u64, Vec<u8>)> = if descending {
                    heap_desc.into_iter().map(|Reverse(t)| t).collect()
                } else {
                    heap_asc.into_iter().collect()
                };
                if descending {
                    drained.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
                } else {
                    drained.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
                }
                drained.into_iter().map(|(_, _, d)| d).collect()
            }
            _ => unreachable!("type guard above restricts to Int/Float"),
        };

        let rows: Vec<Vec<Value>> = drained
            .into_iter()
            .map(|data| {
                proj_indices
                    .iter()
                    .map(|&ci| decode_column(&schema, &row_layout, &data, ci))
                    .collect()
            })
            .collect();

        Ok(Some(QueryResult::Rows {
            columns: proj_columns,
            rows,
        }))
    }

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
    fn try_fused_scan_update(
        &mut self,
        table: &str,
        predicate: &Expr,
        resolved: &[(usize, Value)],
        changed_cols: &[usize],
    ) -> Option<Result<QueryResult, QueryError>> {
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
            let schema = &tbl.schema;
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
            let schema = &tbl.schema;
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

    /// Mission C Phase 3: schema is looked up via `self.catalog.schema(table)`
    /// inside the branches that actually need it. Previously the caller had
    /// to clone the full Schema (6+ String allocs) before every mutation just
    /// so this function could borrow it — a cost the update/delete hot path
    /// did not need.
    fn collect_rids_for_mutation(
        &mut self,
        input: &PlanNode,
        table: &str,
    ) -> Result<Vec<RowId>, QueryError> {
        match input {
            PlanNode::SeqScan { table: t } if t == table => {
                // "Update/delete everything" — rare but legal.
                let rids: Vec<RowId> = self
                    .catalog
                    .scan(table)
                    .map_err(|e| QueryError::StorageError(e.to_string()))?
                    .map(|(rid, _)| rid)
                    .collect();
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
                    self.catalog
                        .for_each_row_raw(table, |rid, data| {
                            if compiled(data) {
                                rids.push(rid);
                            }
                        })
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
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
                let rids: Vec<RowId> = self
                    .catalog
                    .scan(table)
                    .map_err(|e| QueryError::StorageError(e.to_string()))?
                    .filter_map(|(rid, row)| {
                        if row[col_idx] == key_value {
                            Some(rid)
                        } else {
                            None
                        }
                    })
                    .collect();
                Ok(rids)
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

                    // Try compiled predicate first.
                    if let Some(compiled) = compile_predicate(predicate, &columns, &fast, schema) {
                        // Mission F: skip the first 4 Vec doublings.
                        let mut rids: Vec<RowId> = Vec::with_capacity(64);
                        self.catalog
                            .for_each_row_raw(table, |rid, data| {
                                if compiled(data) {
                                    rids.push(rid);
                                }
                            })
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                        return Ok(rids);
                    }

                    // Fallback: selective decode + eval.
                    let pred_cols = predicate_column_indices(predicate, &columns);
                    let mut rids: Vec<RowId> = Vec::with_capacity(64);
                    self.catalog
                        .for_each_row_raw(table, |rid, data| {
                            let pred_row = decode_selective(schema, &row_layout, data, &pred_cols);
                            if eval_predicate(predicate, &pred_row, &columns) {
                                rids.push(rid);
                            }
                        })
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    return Ok(rids);
                }
                self.generic_rid_match(input, table)
            }
            _ => self.generic_rid_match(input, table),
        }
    }

    /// Last-ditch generic match: execute the plan, collect matching rows,
    /// then find corresponding RowIds by value equality. This is the old
    /// O(N*M) code path; only used when the plan shape is something exotic.
    fn generic_rid_match(
        &mut self,
        input: &PlanNode,
        table: &str,
    ) -> Result<Vec<RowId>, QueryError> {
        let result = self.execute_plan(input)?;
        let rows = match result {
            QueryResult::Rows { rows, .. } => rows,
            _ => return Err("mutation source must be rows".into()),
        };
        let matching: Vec<RowId> = self
            .catalog
            .scan(table)
            .map_err(|e| QueryError::StorageError(e.to_string()))?
            .filter(|(_, row)| rows.iter().any(|r| r == row))
            .map(|(rid, _)| rid)
            .collect();
        Ok(matching)
    }
}

pub(super) fn execute_window(
    result: QueryResult,
    windows: &[WindowDef],
) -> Result<QueryResult, QueryError> {
    let (mut columns, mut rows) = match result {
        QueryResult::Rows { columns, rows } => (columns, rows),
        _ => return Err("window function requires row input".into()),
    };

    for wdef in windows {
        // Resolve partition/order column indices against current columns.
        let part_indices: Vec<usize> = wdef
            .partition_by
            .iter()
            .map(|name| {
                columns
                    .iter()
                    .position(|c| c == name)
                    .ok_or_else(|| format!("window partition column '{name}' not found"))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let ord_indices: Vec<(usize, bool)> = wdef
            .order_by
            .iter()
            .map(|sk| {
                columns
                    .iter()
                    .position(|c| c == &sk.field)
                    .map(|i| (i, sk.descending))
                    .ok_or_else(|| format!("window order column '{}' not found", sk.field))
            })
            .collect::<Result<Vec<_>, _>>()?;

        // Resolve the argument column index (for aggregate windows).
        let arg_col_idx: Option<usize> = if let Some(arg) = wdef.args.first() {
            match arg {
                Expr::Field(name) => {
                    if name == "*" {
                        None // count(*) style — no specific column
                    } else {
                        Some(
                            columns
                                .iter()
                                .position(|c| c == name)
                                .ok_or_else(|| format!("window arg column '{name}' not found"))?,
                        )
                    }
                }
                _ => None,
            }
        } else {
            None
        };

        // Build a sort-index to sort rows by partition_by then order_by
        // without actually reordering the original Vec (we need original
        // order to write results back).
        let n = rows.len();
        let mut indices: Vec<usize> = (0..n).collect();
        indices.sort_by(|&a, &b| {
            // Compare partition keys first.
            for &pi in &part_indices {
                let cmp = rows[a][pi].cmp(&rows[b][pi]);
                if cmp != std::cmp::Ordering::Equal {
                    return cmp;
                }
            }
            // Then order keys.
            for &(oi, desc) in &ord_indices {
                let cmp = rows[a][oi].cmp(&rows[b][oi]);
                if cmp != std::cmp::Ordering::Equal {
                    return if desc { cmp.reverse() } else { cmp };
                }
            }
            std::cmp::Ordering::Equal
        });

        // SQL window-frame semantics: with no `order` clause the frame for an
        // aggregate window is the ENTIRE partition, not the running prefix.
        // The loop below computes running values; for the no-order case we
        // back-fill every row of a partition with the partition's final
        // (i.e. complete) aggregate once its boundary is reached. Ranking
        // functions are untouched — row_number/rank/dense_rank are inherently
        // positional.
        let whole_partition_frame = wdef.order_by.is_empty()
            && matches!(
                wdef.function,
                WindowFunc::Sum
                    | WindowFunc::Avg
                    | WindowFunc::Count
                    | WindowFunc::Min
                    | WindowFunc::Max
            );
        // Original row indices of the partition currently being scanned
        // (only tracked when back-filling is needed).
        let mut partition_row_indices: Vec<usize> = Vec::new();

        // Compute window values in sorted order, tracking partition boundaries.
        let mut win_values: Vec<Value> = vec![Value::Empty; n];
        let mut partition_start = 0usize;
        // Running state for aggregate windows:
        let mut running_count: i64 = 0;
        let mut running_int_sum: i64 = 0;
        let mut running_float_sum: f64 = 0.0;
        let mut running_saw_float = false;
        let mut running_min: Option<Value> = None;
        let mut running_max: Option<Value> = None;
        let mut rank_counter: i64 = 0;
        let mut dense_rank_counter: i64 = 0;
        let mut prev_order_key: Option<Vec<Value>> = None;
        let mut same_rank_count: i64 = 0;

        for sorted_pos in 0..n {
            let row_idx = indices[sorted_pos];

            // Detect partition boundary.
            let new_partition = if sorted_pos == 0 {
                true
            } else {
                let prev_row_idx = indices[sorted_pos - 1];
                part_indices
                    .iter()
                    .any(|&pi| rows[row_idx][pi] != rows[prev_row_idx][pi])
            };

            if new_partition {
                // No-order aggregate frame: the partition that just ended is
                // complete, so its final running value IS the whole-partition
                // aggregate. Back-fill it onto every row of that partition.
                if whole_partition_frame && sorted_pos > 0 {
                    let final_v = win_values[indices[sorted_pos - 1]].clone();
                    for ri in partition_row_indices.drain(..) {
                        win_values[ri] = final_v.clone();
                    }
                }
                partition_start = sorted_pos;
                running_count = 0;
                running_int_sum = 0;
                running_float_sum = 0.0;
                running_saw_float = false;
                running_min = None;
                running_max = None;
                rank_counter = 0;
                dense_rank_counter = 0;
                prev_order_key = None;
                same_rank_count = 0;
            }

            // Extract current order key for rank tracking.
            let current_order_key: Vec<Value> = ord_indices
                .iter()
                .map(|&(oi, _)| rows[row_idx][oi].clone())
                .collect();
            let same_as_prev = prev_order_key.as_ref() == Some(&current_order_key);

            let value = match wdef.function {
                WindowFunc::RowNumber => Value::Int((sorted_pos - partition_start + 1) as i64),
                WindowFunc::Rank => {
                    if same_as_prev {
                        same_rank_count += 1;
                    } else {
                        rank_counter += same_rank_count + 1;
                        same_rank_count = 0;
                        if rank_counter == 0 {
                            rank_counter = 1;
                        }
                    }
                    Value::Int(rank_counter)
                }
                WindowFunc::DenseRank => {
                    if !same_as_prev {
                        dense_rank_counter += 1;
                    }
                    Value::Int(dense_rank_counter)
                }
                WindowFunc::Sum => {
                    if let Some(ci) = arg_col_idx {
                        match &rows[row_idx][ci] {
                            Value::Int(v) => running_int_sum += v,
                            Value::Float(v) => {
                                running_float_sum += v;
                                running_saw_float = true;
                            }
                            _ => {}
                        }
                    }
                    if running_saw_float {
                        Value::Float(running_float_sum + running_int_sum as f64)
                    } else {
                        Value::Int(running_int_sum)
                    }
                }
                WindowFunc::Avg => {
                    if let Some(ci) = arg_col_idx {
                        match &rows[row_idx][ci] {
                            Value::Int(v) => {
                                running_float_sum += *v as f64;
                                running_count += 1;
                            }
                            Value::Float(v) => {
                                running_float_sum += v;
                                running_count += 1;
                            }
                            _ => {}
                        }
                    }
                    if running_count == 0 {
                        Value::Empty
                    } else {
                        Value::Float(running_float_sum / running_count as f64)
                    }
                }
                WindowFunc::Count => {
                    if let Some(ci) = arg_col_idx {
                        if !rows[row_idx][ci].is_empty() {
                            running_count += 1;
                        }
                    } else {
                        // count(*) — count all rows
                        running_count += 1;
                    }
                    Value::Int(running_count)
                }
                WindowFunc::Min => {
                    if let Some(ci) = arg_col_idx {
                        let v = &rows[row_idx][ci];
                        if !v.is_empty() {
                            running_min = Some(match &running_min {
                                None => v.clone(),
                                Some(cur) => {
                                    if v < cur {
                                        v.clone()
                                    } else {
                                        cur.clone()
                                    }
                                }
                            });
                        }
                    }
                    running_min.clone().unwrap_or(Value::Empty)
                }
                WindowFunc::Max => {
                    if let Some(ci) = arg_col_idx {
                        let v = &rows[row_idx][ci];
                        if !v.is_empty() {
                            running_max = Some(match &running_max {
                                None => v.clone(),
                                Some(cur) => {
                                    if v > cur {
                                        v.clone()
                                    } else {
                                        cur.clone()
                                    }
                                }
                            });
                        }
                    }
                    running_max.clone().unwrap_or(Value::Empty)
                }
            };

            prev_order_key = Some(current_order_key);
            win_values[row_idx] = value;
            if whole_partition_frame {
                partition_row_indices.push(row_idx);
            }
        }

        // Back-fill the final partition (the loop only flushes at boundaries).
        if whole_partition_frame && n > 0 {
            let final_v = win_values[indices[n - 1]].clone();
            for ri in partition_row_indices.drain(..) {
                win_values[ri] = final_v.clone();
            }
        }

        // Append the computed window column to each row.
        for (ri, row) in rows.iter_mut().enumerate() {
            row.push(win_values[ri].clone());
        }
        columns.push(wdef.output_name.clone());
    }

    Ok(QueryResult::Rows { columns, rows })
}

/// Resolve a group-by key or aggregate argument name against the input
/// columns of a `GroupBy` node.
///
/// Single-table inputs have bare column names (`status`); join inputs have
/// `alias.field` names. Resolution rules:
///   1. Exact match first. Single-table keys and fully qualified
///      `alias.field` references hit here, preserving existing behavior.
///   2. A qualified reference (one containing `.`) only ever matches exactly;
///      if the exact column is absent it is genuinely missing.
///   3. An unqualified name falls back to a unique `.field` suffix match over
///      the join output columns. Zero matches is a column-not-found error;
///      more than one is an ambiguity error naming the candidates.
pub(super) fn resolve_group_column(
    name: &str,
    columns: &[String],
) -> Result<usize, QueryError> {
    if let Some(i) = columns.iter().position(|c| c == name) {
        return Ok(i);
    }
    if name.contains('.') {
        return Err(QueryError::ColumnNotFound {
            table: String::new(),
            column: name.to_string(),
        });
    }
    let suffix = format!(".{name}");
    let mut matches = columns
        .iter()
        .enumerate()
        .filter(|(_, c)| c.ends_with(&suffix));
    match matches.next() {
        None => Err(QueryError::ColumnNotFound {
            table: String::new(),
            column: name.to_string(),
        }),
        Some((first_idx, _)) => {
            let rest: Vec<&str> = matches.map(|(_, c)| c.as_str()).collect();
            if rest.is_empty() {
                Ok(first_idx)
            } else {
                // Rebuild the full candidate list (the consumed first match
                // plus the rest) so the message names every ambiguous column.
                let candidates: Vec<&str> = columns
                    .iter()
                    .filter(|c| c.ends_with(&suffix))
                    .map(|c| c.as_str())
                    .collect();
                Err(QueryError::Execution(format!(
                    "cannot group by ambiguous column '{name}'; candidates: {}",
                    candidates.join(", ")
                )))
            }
        }
    }
}

/// Mission E2b: execute a `GroupBy` plan node over already-materialized input
/// rows. Shared by the mutable (`execute_plan`) and read-only
/// (`execute_plan_readonly`) executors so key/argument resolution and the
/// output-column naming stay identical on both paths.
pub(super) fn exec_group_by(
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    keys: &[GroupKey],
    aggregates: &[GroupAgg],
    having: &Option<Expr>,
) -> Result<QueryResult, QueryError> {
    // Resolve key column indices. Qualified keys resolve exactly to
    // `alias.field`; unqualified keys resolve by exact-then-suffix match.
    let key_indices: Vec<usize> = keys
        .iter()
        .map(|k| resolve_group_column(&k.output_name(), &columns))
        .collect::<Result<Vec<_>, _>>()?;

    // Resolve aggregate field indices. count(*) uses the usize::MAX sentinel;
    // every other argument gets the same resolution as keys.
    let agg_field_indices: Vec<usize> = aggregates
        .iter()
        .map(|a| {
            if a.field == "*" {
                Ok(usize::MAX)
            } else {
                resolve_group_column(&a.field, &columns)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Group rows by key values (preserving insertion order).
    let mut group_map: rustc_hash::FxHashMap<Vec<Value>, usize> =
        rustc_hash::FxHashMap::default();
    let mut groups: Vec<(Vec<Value>, Vec<usize>)> = Vec::new();
    for (ri, row) in rows.iter().enumerate() {
        let key: Vec<Value> = key_indices.iter().map(|&i| row[i].clone()).collect();
        match group_map.get(&key) {
            Some(&idx) => groups[idx].1.push(ri),
            None => {
                let idx = groups.len();
                group_map.insert(key.clone(), idx);
                groups.push((key, vec![ri]));
            }
        }
    }

    // Output columns: key display names ++ aggregate output names. Qualified
    // keys are emitted as `alias.field` so a qualified HAVING reference and
    // downstream projections resolve against them.
    let mut out_columns: Vec<String> = keys.iter().map(|k| k.output_name()).collect();
    for agg in aggregates.iter() {
        out_columns.push(agg.output_name.clone());
    }

    // Compute aggregates per group.
    let mut out_rows: Vec<Vec<Value>> = Vec::with_capacity(groups.len());
    for (key_vals, row_indices) in &groups {
        let mut row = key_vals.clone();
        for (ai, agg) in aggregates.iter().enumerate() {
            let col_idx = agg_field_indices[ai];
            let val = compute_group_aggregate(agg.function, &rows, row_indices, col_idx);
            row.push(val);
        }
        out_rows.push(row);
    }

    // Apply HAVING filter.
    if let Some(having_expr) = having {
        out_rows.retain(|row| eval_predicate(having_expr, row, &out_columns));
    }

    Ok(QueryResult::Rows {
        columns: out_columns,
        rows: out_rows,
    })
}

/// Reject any aggregate `FunctionCall` that survives planning into an
/// evaluable position (a projection field, a filter predicate, or a HAVING
/// clause). The grouped-aggregate planner rewrites every supported aggregate
/// into a `Field` reference to a computed column, so a surviving
/// `FunctionCall` means the aggregate sits somewhere the engine cannot
/// evaluate it. `eval_expr` would otherwise silently produce `Empty` there (a
/// wrong answer); this turns that into a typed error before any row is
/// evaluated. Walks the whole plan so fused fast paths cannot bypass it.
pub(super) fn validate_no_stray_aggregates(plan: &PlanNode) -> Result<(), QueryError> {
    match plan {
        PlanNode::Project { input, fields } => {
            for f in fields {
                check_expr_no_aggregate(&f.expr)?;
            }
            validate_no_stray_aggregates(input)?;
        }
        PlanNode::Filter { input, predicate } => {
            check_expr_no_aggregate(predicate)?;
            validate_no_stray_aggregates(input)?;
        }
        PlanNode::GroupBy { input, having, .. } => {
            if let Some(h) = having {
                check_expr_no_aggregate(h)?;
            }
            validate_no_stray_aggregates(input)?;
        }
        PlanNode::NestedLoopJoin { left, right, .. } => {
            validate_no_stray_aggregates(left)?;
            validate_no_stray_aggregates(right)?;
        }
        PlanNode::Union { left, right, .. } => {
            validate_no_stray_aggregates(left)?;
            validate_no_stray_aggregates(right)?;
        }
        PlanNode::Sort { input, .. }
        | PlanNode::Limit { input, .. }
        | PlanNode::Offset { input, .. }
        | PlanNode::Distinct { input }
        | PlanNode::Aggregate { input, .. }
        | PlanNode::Window { input, .. }
        | PlanNode::Update { input, .. }
        | PlanNode::Delete { input, .. }
        | PlanNode::Explain { input } => {
            validate_no_stray_aggregates(input)?;
        }
        _ => {}
    }
    Ok(())
}

/// Recurse an expression tree, rejecting any aggregate `FunctionCall`. Does
/// not descend into subquery `QueryExpr`s (they are materialized and
/// evaluated on their own path), only their outer operand expression.
fn check_expr_no_aggregate(expr: &Expr) -> Result<(), QueryError> {
    match expr {
        Expr::FunctionCall(_, _) => Err(QueryError::Execution(
            "invalid query: aggregate function in an unsupported position".to_string(),
        )),
        Expr::BinaryOp(l, _, r) | Expr::Coalesce(l, r) => {
            check_expr_no_aggregate(l)?;
            check_expr_no_aggregate(r)
        }
        Expr::UnaryOp(_, inner) | Expr::Cast(inner, _) => check_expr_no_aggregate(inner),
        Expr::ScalarFunc(_, args) => {
            for a in args {
                check_expr_no_aggregate(a)?;
            }
            Ok(())
        }
        Expr::InList { expr: e, list, .. } => {
            check_expr_no_aggregate(e)?;
            for item in list {
                check_expr_no_aggregate(item)?;
            }
            Ok(())
        }
        Expr::InSubquery { expr: e, .. } => check_expr_no_aggregate(e),
        Expr::Case { whens, else_expr } => {
            for (c, r) in whens {
                check_expr_no_aggregate(c)?;
                check_expr_no_aggregate(r)?;
            }
            if let Some(e) = else_expr {
                check_expr_no_aggregate(e)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Mission E2b: compute one aggregate over a set of rows in a group.
pub(super) fn compute_group_aggregate(
    func: AggFunc,
    all_rows: &[Vec<Value>],
    row_indices: &[usize],
    col_idx: usize,
) -> Value {
    match func {
        AggFunc::Count => {
            if col_idx == usize::MAX {
                // count(*) — count all rows in the group.
                return Value::Int(row_indices.len() as i64);
            }
            let count = row_indices
                .iter()
                .filter(|&&ri| !all_rows[ri][col_idx].is_empty())
                .count();
            Value::Int(count as i64)
        }
        AggFunc::CountDistinct => {
            let mut seen = std::collections::HashSet::new();
            for &ri in row_indices {
                let v = &all_rows[ri][col_idx];
                if !v.is_empty() {
                    seen.insert(v.clone());
                }
            }
            Value::Int(seen.len() as i64)
        }
        AggFunc::Sum => {
            // Mirror the scalar Sum path: accumulate int and float
            // contributions separately and promote the final result to
            // Float if any Float row was observed. Prevents silent
            // drop of Float columns in GROUP BY aggregates.
            let mut int_sum: i64 = 0;
            let mut float_sum: f64 = 0.0;
            let mut saw_float = false;
            for &ri in row_indices {
                match &all_rows[ri][col_idx] {
                    Value::Int(v) => int_sum += v,
                    Value::Float(v) => {
                        float_sum += *v;
                        saw_float = true;
                    }
                    _ => {}
                }
            }
            if saw_float {
                Value::Float(float_sum + int_sum as f64)
            } else {
                Value::Int(int_sum)
            }
        }
        AggFunc::Avg => {
            let mut sum = 0.0f64;
            let mut count = 0usize;
            for &ri in row_indices {
                match &all_rows[ri][col_idx] {
                    Value::Int(v) => {
                        sum += *v as f64;
                        count += 1;
                    }
                    Value::Float(v) => {
                        sum += *v;
                        count += 1;
                    }
                    _ => {}
                }
            }
            if count == 0 {
                Value::Empty
            } else {
                Value::Float(sum / count as f64)
            }
        }
        AggFunc::Min => row_indices
            .iter()
            .map(|&ri| &all_rows[ri][col_idx])
            .filter(|v| !v.is_empty())
            .min()
            .cloned()
            .unwrap_or(Value::Empty),
        AggFunc::Max => row_indices
            .iter()
            .map(|&ri| &all_rows[ri][col_idx])
            .filter(|v| !v.is_empty())
            .max()
            .cloned()
            .unwrap_or(Value::Empty),
    }
}

/// Mission E1.3: try to extract equi-join key indices from a join `on`
/// predicate. Returns `Some((left_col_idx, right_col_idx))` when the
/// predicate is exactly `L = R` (or `R = L`) and both sides resolve
/// cleanly — `L` to the left subtree's column list and `R` to the right
/// subtree's column list.
///
/// This is deliberately narrow. We only recognise the two shapes:
///   * `QualifiedField = QualifiedField`  (`u.id = o.user_id`)
///   * `Field = Field`                    (`.id = .user_id`, unqualified)
///
/// Anything else — conjunctions, constants, function calls, or predicates
/// that touch the same side on both halves — falls through to the
/// nested-loop path unchanged.
pub(super) fn try_extract_equi_join_keys(
    pred: &Expr,
    left_columns: &[String],
    right_columns: &[String],
) -> Option<(usize, usize)> {
    let (lhs, op, rhs) = match pred {
        Expr::BinaryOp(l, op, r) => (l.as_ref(), *op, r.as_ref()),
        _ => return None,
    };
    if op != BinOp::Eq {
        return None;
    }
    // Normal orientation: lhs in left, rhs in right.
    if let (Some(li), Some(ri)) = (
        resolve_side_column(lhs, left_columns),
        resolve_side_column(rhs, right_columns),
    ) {
        return Some((li, ri));
    }
    // Swapped: rhs in left, lhs in right. Both sides of `=` are
    // commutative so this is safe.
    if let (Some(li), Some(ri)) = (
        resolve_side_column(rhs, left_columns),
        resolve_side_column(lhs, right_columns),
    ) {
        return Some((li, ri));
    }
    None
}

fn resolve_side_column(expr: &Expr, columns: &[String]) -> Option<usize> {
    match expr {
        Expr::QualifiedField { qualifier, field } => {
            // Byte-level match so we don't allocate a fresh `format!` on
            // every call — this runs once per plan, so allocation would be
            // cheap, but the match is trivial enough to keep inline with
            // the eval_expr version.
            let q = qualifier.as_bytes();
            let f = field.as_bytes();
            columns.iter().position(|c| {
                let b = c.as_bytes();
                b.len() == q.len() + 1 + f.len()
                    && b[..q.len()] == *q
                    && b[q.len()] == b'.'
                    && b[q.len() + 1..] == *f
            })
        }
        Expr::Field(name) => columns.iter().position(|c| c == name),
        _ => None,
    }
}

/// Mission E1.3: O(L + R) hash join. Builds a `FxHashMap<Value, Vec<usize>>`
/// over the right (inner) side's join keys, then streams the left (outer)
/// side and for each probe row emits every combined row whose right-side
/// key matches. For `JoinKind::LeftOuter`, unmatched left rows are emitted
/// padded with `Value::Empty` on the right side.
///
/// The right side is always the build side. That choice is forced for
/// LeftOuter (the left side must stream so we can detect orphans), and
/// for Inner it's a reasonable default — left-deep plans tend to grow the
/// left side with each join, so the un-joined right leaf is often the
/// smaller of the two at each level.
pub(super) fn hash_join(
    left_columns: Vec<String>,
    left_rows: Vec<Vec<Value>>,
    right_columns: Vec<String>,
    right_rows: Vec<Vec<Value>>,
    left_key_idx: usize,
    right_key_idx: usize,
    kind: JoinKind,
) -> QueryResult {
    use rustc_hash::FxHashMap;

    let n_left = left_columns.len();
    let n_right = right_columns.len();
    let mut columns = Vec::with_capacity(n_left + n_right);
    columns.extend(left_columns);
    columns.extend(right_columns);

    // Build: right_key -> list of right-row indices. Pre-size to the row
    // count so the map doesn't rehash mid-build.
    let mut build: FxHashMap<Value, Vec<usize>> =
        FxHashMap::with_capacity_and_hasher(right_rows.len(), Default::default());
    for (i, row) in right_rows.iter().enumerate() {
        // Skip Empty keys on the build side — they can never match under
        // SQL semantics (NULL ≠ NULL) and would collapse all nullables to
        // one bucket.
        if matches!(row[right_key_idx], Value::Empty) {
            continue;
        }
        build.entry(row[right_key_idx].clone()).or_default().push(i);
    }

    // Reasonable starting capacity — inner joins produce ≥ left_rows.len()
    // rows in the common 1:1 case, left-outer always emits ≥ left_rows.len().
    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(left_rows.len());

    for left_row in &left_rows {
        let key = &left_row[left_key_idx];
        let matched = if matches!(key, Value::Empty) {
            None
        } else {
            build.get(key)
        };
        match matched {
            Some(matches) if !matches.is_empty() => {
                for &ri in matches {
                    let right_row = &right_rows[ri];
                    let mut combined = Vec::with_capacity(n_left + n_right);
                    combined.extend_from_slice(left_row);
                    combined.extend_from_slice(right_row);
                    rows.push(combined);
                }
            }
            _ => {
                if matches!(kind, JoinKind::LeftOuter) {
                    let mut row = Vec::with_capacity(n_left + n_right);
                    row.extend_from_slice(left_row);
                    row.resize(n_left + n_right, Value::Empty);
                    rows.push(row);
                }
            }
        }
    }

    QueryResult::Rows { columns, rows }
}

/// Lower unindexed `RangeScan` and `IndexScan` nodes to `Filter(SeqScan)`
/// so that all downstream fast paths (count, project+limit, sort+limit,
/// agg, update, delete) continue to fire.
///
/// The planner emits `RangeScan` (for `.age > 30`) and `IndexScan` (for
/// `.email = lit`) speculatively because it has no catalog access. When
/// the column has a B-tree index, those plans are correct. When it
/// doesn't, the executor's fallbacks materialise every matching row with
/// full `decode_row` — bypassing the compiled-predicate fast paths that
/// `Filter(SeqScan)` would trigger. Lowering both speculative leaf kinds
/// also keeps EXPLAIN honest: it prints the plan that actually runs.
///
/// This pass runs once per query, before execution.
pub(super) fn lower_unindexed_scans(catalog: &Catalog, plan: &PlanNode) -> PlanNode {
    match plan {
        PlanNode::RangeScan {
            table,
            column,
            start,
            end,
        } => {
            if let Some(tbl) = catalog.get_table(table) {
                // Keep RangeScan whenever ANY index exists on the column:
                // unique indexes store raw column values, non-unique indexes
                // store composite (value, rid) keys that the executor walks
                // natively via BTree::range_rids. Only lower to Filter(SeqScan)
                // when the column is unindexed.
                if tbl.has_index(column) {
                    return plan.clone();
                }
            }
            let pred = synthesize_range_predicate(column, start, end);
            PlanNode::Filter {
                input: Box::new(PlanNode::SeqScan {
                    table: table.clone(),
                }),
                predicate: pred,
            }
        }
        PlanNode::Filter { input, predicate } => PlanNode::Filter {
            input: Box::new(lower_unindexed_scans(catalog, input)),
            predicate: predicate.clone(),
        },
        PlanNode::Project { input, fields } => PlanNode::Project {
            input: Box::new(lower_unindexed_scans(catalog, input)),
            fields: fields.clone(),
        },
        PlanNode::Sort { input, keys } => PlanNode::Sort {
            input: Box::new(lower_unindexed_scans(catalog, input)),
            keys: keys.clone(),
        },
        PlanNode::Limit { input, count } => PlanNode::Limit {
            input: Box::new(lower_unindexed_scans(catalog, input)),
            count: count.clone(),
        },
        PlanNode::Offset { input, count } => PlanNode::Offset {
            input: Box::new(lower_unindexed_scans(catalog, input)),
            count: count.clone(),
        },
        PlanNode::Aggregate {
            input,
            function,
            field,
        } => PlanNode::Aggregate {
            input: Box::new(lower_unindexed_scans(catalog, input)),
            function: *function,
            field: field.clone(),
        },
        PlanNode::Distinct { input } => PlanNode::Distinct {
            input: Box::new(lower_unindexed_scans(catalog, input)),
        },
        PlanNode::GroupBy {
            input,
            keys,
            aggregates,
            having,
        } => PlanNode::GroupBy {
            input: Box::new(lower_unindexed_scans(catalog, input)),
            keys: keys.clone(),
            aggregates: aggregates.clone(),
            having: having.clone(),
        },
        PlanNode::Update {
            input,
            table,
            assignments,
            returning,
        } => PlanNode::Update {
            input: Box::new(lower_unindexed_scans(catalog, input)),
            table: table.clone(),
            assignments: assignments.clone(),
            returning: *returning,
        },
        PlanNode::Delete {
            input,
            table,
            returning,
        } => PlanNode::Delete {
            input: Box::new(lower_unindexed_scans(catalog, input)),
            table: table.clone(),
            returning: *returning,
        },
        PlanNode::Window { input, windows } => PlanNode::Window {
            input: Box::new(lower_unindexed_scans(catalog, input)),
            windows: windows.clone(),
        },
        PlanNode::Union { left, right, all } => PlanNode::Union {
            left: Box::new(lower_unindexed_scans(catalog, left)),
            right: Box::new(lower_unindexed_scans(catalog, right)),
            all: *all,
        },
        PlanNode::Explain { input } => PlanNode::Explain {
            input: Box::new(lower_unindexed_scans(catalog, input)),
        },
        PlanNode::NestedLoopJoin {
            left,
            right,
            on,
            kind,
        } => PlanNode::NestedLoopJoin {
            left: Box::new(lower_unindexed_scans(catalog, left)),
            right: Box::new(lower_unindexed_scans(catalog, right)),
            on: on.clone(),
            kind: *kind,
        },
        PlanNode::IndexScan { table, column, key } => {
            if let Some(tbl) = catalog.get_table(table) {
                if tbl.has_index(column) {
                    return plan.clone();
                }
            }
            PlanNode::Filter {
                input: Box::new(PlanNode::SeqScan {
                    table: table.clone(),
                }),
                predicate: Expr::BinaryOp(
                    Box::new(Expr::Field(column.clone())),
                    BinOp::Eq,
                    Box::new(key.clone()),
                ),
            }
        }
        // Leaf nodes: no children to recurse into.
        _ => plan.clone(),
    }
}

/// Synthesize a range predicate from RangeScan bounds for the fallback path.
pub(super) fn synthesize_range_predicate(
    column: &str,
    start: &Option<(Expr, bool)>,
    end: &Option<(Expr, bool)>,
) -> Expr {
    let lower = start.as_ref().map(|(expr, inclusive)| {
        let op = if *inclusive { BinOp::Gte } else { BinOp::Gt };
        Expr::BinaryOp(
            Box::new(Expr::Field(column.to_string())),
            op,
            Box::new(expr.clone()),
        )
    });
    let upper = end.as_ref().map(|(expr, inclusive)| {
        let op = if *inclusive { BinOp::Lte } else { BinOp::Lt };
        Expr::BinaryOp(
            Box::new(Expr::Field(column.to_string())),
            op,
            Box::new(expr.clone()),
        )
    });
    match (lower, upper) {
        (Some(l), Some(u)) => Expr::BinaryOp(Box::new(l), BinOp::And, Box::new(u)),
        (Some(l), None) => l,
        (None, Some(u)) => u,
        (None, None) => Expr::Literal(Literal::Bool(true)),
    }
}

/// Check if a value falls within a range (used in last-resort decoded-row eval).
pub(super) fn range_matches(
    val: &Value,
    start: &Option<Value>,
    start_inc: bool,
    end: &Option<Value>,
    end_inc: bool,
) -> bool {
    if let Some(ref s) = start {
        if start_inc {
            if val < s {
                return false;
            }
        } else if val <= s {
            return false;
        }
    }
    if let Some(ref e) = end {
        if end_inc {
            if val > e {
                return false;
            }
        } else if val >= e {
            return false;
        }
    }
    true
}

/// Format a `PlanNode` tree as a human-readable, indented text
/// representation. Used by the `EXPLAIN` command.
pub(super) fn format_plan_tree(plan: &PlanNode, depth: usize) -> String {
    let indent = "  ".repeat(depth);
    match plan {
        PlanNode::SeqScan { table } => format!("{indent}SeqScan table={table}"),
        PlanNode::AliasScan { table, alias } => {
            format!("{indent}AliasScan table={table} alias={alias}")
        }
        PlanNode::IndexScan { table, column, key } => {
            format!("{indent}IndexScan table={table} column={column} key={key:?}")
        }
        PlanNode::RangeScan {
            table,
            column,
            start,
            end,
        } => {
            let s = match start {
                Some((expr, inc)) => {
                    let op = if *inc { ">=" } else { ">" };
                    format!("{op}{expr:?}")
                }
                None => "unbounded".to_string(),
            };
            let e = match end {
                Some((expr, inc)) => {
                    let op = if *inc { "<=" } else { "<" };
                    format!("{op}{expr:?}")
                }
                None => "unbounded".to_string(),
            };
            format!("{indent}RangeScan table={table} column={column} [{s}, {e}]")
        }
        PlanNode::Filter { input, predicate } => {
            let child = format_plan_tree(input, depth + 1);
            format!("{indent}Filter predicate={predicate:?}\n{child}")
        }
        PlanNode::Project { input, fields } => {
            let names: Vec<String> = fields
                .iter()
                .map(|f| match &f.alias {
                    Some(a) => format!("{a}: {:?}", f.expr),
                    None => format!("{:?}", f.expr),
                })
                .collect();
            let child = format_plan_tree(input, depth + 1);
            format!("{indent}Project fields=[{}]\n{child}", names.join(", "))
        }
        PlanNode::Sort { input, keys } => {
            let ks: Vec<String> = keys
                .iter()
                .map(|k| {
                    if k.descending {
                        format!("{} desc", k.field)
                    } else {
                        k.field.clone()
                    }
                })
                .collect();
            let child = format_plan_tree(input, depth + 1);
            format!("{indent}Sort keys=[{}]\n{child}", ks.join(", "))
        }
        PlanNode::Limit { input, count } => {
            let child = format_plan_tree(input, depth + 1);
            format!("{indent}Limit count={count:?}\n{child}")
        }
        PlanNode::Offset { input, count } => {
            let child = format_plan_tree(input, depth + 1);
            format!("{indent}Offset count={count:?}\n{child}")
        }
        PlanNode::Aggregate {
            input,
            function,
            field,
        } => {
            let f = field.as_deref().unwrap_or("*");
            let child = format_plan_tree(input, depth + 1);
            format!("{indent}Aggregate fn={function:?} field={f}\n{child}")
        }
        PlanNode::NestedLoopJoin {
            left,
            right,
            on,
            kind,
        } => {
            let left_child = format_plan_tree(left, depth + 1);
            let right_child = format_plan_tree(right, depth + 1);
            let on_str = match on {
                Some(pred) => format!("{pred:?}"),
                None => "none".to_string(),
            };
            format!("{indent}NestedLoopJoin kind={kind:?} on={on_str}\n{left_child}\n{right_child}")
        }
        PlanNode::Distinct { input } => {
            let child = format_plan_tree(input, depth + 1);
            format!("{indent}Distinct\n{child}")
        }
        PlanNode::GroupBy {
            input,
            keys,
            aggregates,
            having,
        } => {
            let agg_strs: Vec<String> = aggregates
                .iter()
                .map(|a| format!("{:?}({}) as {}", a.function, a.field, a.output_name))
                .collect();
            let having_str = match having {
                Some(h) => format!(" having={h:?}"),
                None => String::new(),
            };
            let key_strs: Vec<String> = keys.iter().map(|k| k.output_name()).collect();
            let child = format_plan_tree(input, depth + 1);
            format!(
                "{indent}GroupBy keys=[{}] aggs=[{}]{having_str}\n{child}",
                key_strs.join(", "),
                agg_strs.join(", "),
            )
        }
        PlanNode::Insert { table, rows, .. } => {
            let cols: Vec<&str> = rows
                .first()
                .map(|r| r.iter().map(|a| a.field.as_str()).collect())
                .unwrap_or_default();
            format!(
                "{indent}Insert table={table} rows={} cols=[{}]",
                rows.len(),
                cols.join(", ")
            )
        }
        PlanNode::Upsert {
            table,
            key_column,
            assignments,
            on_conflict,
        } => {
            let cols: Vec<&str> = assignments.iter().map(|a| a.field.as_str()).collect();
            let conflict_cols: Vec<&str> = on_conflict.iter().map(|a| a.field.as_str()).collect();
            if conflict_cols.is_empty() {
                format!(
                    "{indent}Upsert table={table} key={key_column} cols=[{}]",
                    cols.join(", ")
                )
            } else {
                format!(
                    "{indent}Upsert table={table} key={key_column} cols=[{}] on_conflict=[{}]",
                    cols.join(", "),
                    conflict_cols.join(", ")
                )
            }
        }
        PlanNode::Update {
            input,
            table,
            assignments,
            returning,
        } => {
            let cols: Vec<&str> = assignments.iter().map(|a| a.field.as_str()).collect();
            let child = format_plan_tree(input, depth + 1);
            let ret = if *returning { " returning" } else { "" };
            format!(
                "{indent}Update table={table} set=[{}]{ret}\n{child}",
                cols.join(", ")
            )
        }
        PlanNode::Delete {
            input,
            table,
            returning,
        } => {
            let child = format_plan_tree(input, depth + 1);
            let ret = if *returning { " returning" } else { "" };
            format!("{indent}Delete table={table}{ret}\n{child}")
        }
        PlanNode::CreateTable { name, fields, .. } => {
            let fs: Vec<String> = fields
                .iter()
                .map(|f| {
                    let mut mods = String::new();
                    if f.required {
                        mods.push_str(" required");
                    }
                    if f.unique {
                        mods.push_str(" unique");
                    }
                    format!("{}: {}{mods}", f.name, f.type_name)
                })
                .collect();
            format!("{indent}CreateTable name={name} fields=[{}]", fs.join(", "))
        }
        PlanNode::AlterTable { table, action } => {
            format!("{indent}AlterTable table={table} action={action:?}")
        }
        PlanNode::DropTable { name, .. } => format!("{indent}DropTable name={name}"),
        PlanNode::CreateView { name, .. } => format!("{indent}CreateView name={name}"),
        PlanNode::RefreshView { name } => format!("{indent}RefreshView name={name}"),
        PlanNode::DropView { name, .. } => format!("{indent}DropView name={name}"),
        PlanNode::ListTypes => format!("{indent}ListTypes"),
        PlanNode::Describe { table } => format!("{indent}Describe table={table}"),
        PlanNode::Window { input, windows } => {
            let ws: Vec<String> = windows
                .iter()
                .map(|w| format!("{:?} as {}", w.function, w.output_name))
                .collect();
            let child = format_plan_tree(input, depth + 1);
            format!("{indent}Window fns=[{}]\n{child}", ws.join(", "))
        }
        PlanNode::Union { left, right, all } => {
            let kind = if *all { "UNION ALL" } else { "UNION" };
            let left_child = format_plan_tree(left, depth + 1);
            let right_child = format_plan_tree(right, depth + 1);
            format!("{indent}{kind}\n{left_child}\n{right_child}")
        }
        PlanNode::Explain { input } => {
            let child = format_plan_tree(input, depth + 1);
            format!("{indent}Explain\n{child}")
        }
        PlanNode::Begin => format!("{indent}Begin"),
        PlanNode::Commit => format!("{indent}Commit"),
        PlanNode::Rollback => format!("{indent}Rollback"),
    }
}
