//! Window functions, grouped aggregation, and scalar aggregate evaluation.

use crate::cancel::CancelCheck;
use crate::result::{QueryError, QueryResult};
use powdb_storage::types::*;
use std::collections::HashSet;

use crate::executor::eval::*;
use crate::executor::mem_budget;

use super::*;

pub(crate) fn execute_window(
    result: QueryResult,
    windows: &[WindowDef],
    memory_limit: usize,
) -> Result<QueryResult, QueryError> {
    let (mut columns, mut rows) = match result {
        QueryResult::Rows { columns, rows } => (columns, rows),
        _ => return Err("window function requires row input".into()),
    };

    let mut cancel = CancelCheck::new();
    for wdef in windows {
        cancel.tick()?;
        // Stored fields resolve once; expression-valued window keys use the
        // common evaluator without changing the original row order.
        let part_indices: Vec<Option<usize>> = wdef
            .partition_by
            .iter()
            .map(|expr| resolve_direct_group_expr(expr, &columns))
            .collect::<Result<Vec<_>, _>>()?;

        let ord_indices: Vec<(Option<usize>, &Expr, bool)> = wdef
            .order_by
            .iter()
            .map(|sk| {
                resolve_direct_group_expr(&sk.expr, &columns)
                    .map(|index| (index, &sk.expr, sk.descending))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let arg_expr = wdef.args.first();
        let arg_col_idx = arg_expr
            .map(|expr| resolve_direct_group_expr(expr, &columns))
            .transpose()?
            .flatten();

        // Build a sort-index to sort rows by partition_by then order_by
        // without actually reordering the original Vec (we need original
        // order to write results back).
        let n = rows.len();
        let mut indices: Vec<usize> = (0..n).collect();
        cooperative_stable_sort_by(&mut indices, memory_limit, |&a, &b| {
            // Compare partition keys first.
            for (expr, index) in wdef.partition_by.iter().zip(&part_indices) {
                let av = index
                    .map(|i| rows[a][i].clone())
                    .unwrap_or_else(|| eval_expr(expr, &rows[a], &columns));
                let bv = index
                    .map(|i| rows[b][i].clone())
                    .unwrap_or_else(|| eval_expr(expr, &rows[b], &columns));
                let cmp = av.cmp(&bv);
                if cmp != std::cmp::Ordering::Equal {
                    return cmp;
                }
            }
            // Then order keys.
            for &(index, expr, desc) in &ord_indices {
                let av = index
                    .map(|i| rows[a][i].clone())
                    .unwrap_or_else(|| eval_expr(expr, &rows[a], &columns));
                let bv = index
                    .map(|i| rows[b][i].clone())
                    .unwrap_or_else(|| eval_expr(expr, &rows[b], &columns));
                let cmp = compare_order_values(&av, &bv, desc);
                if cmp != std::cmp::Ordering::Equal {
                    return cmp;
                }
            }
            std::cmp::Ordering::Equal
        })?;

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
            cancel.tick()?;
            let row_idx = indices[sorted_pos];

            // Detect partition boundary.
            let new_partition = if sorted_pos == 0 {
                true
            } else {
                let prev_row_idx = indices[sorted_pos - 1];
                wdef.partition_by
                    .iter()
                    .zip(&part_indices)
                    .any(|(expr, index)| {
                        let current = index
                            .map(|i| rows[row_idx][i].clone())
                            .unwrap_or_else(|| eval_expr(expr, &rows[row_idx], &columns));
                        let previous = index
                            .map(|i| rows[prev_row_idx][i].clone())
                            .unwrap_or_else(|| eval_expr(expr, &rows[prev_row_idx], &columns));
                        current != previous
                    })
            };

            if new_partition {
                // No-order aggregate frame: the partition that just ended is
                // complete, so its final running value IS the whole-partition
                // aggregate. Back-fill it onto every row of that partition.
                if whole_partition_frame && sorted_pos > 0 {
                    let final_v = win_values[indices[sorted_pos - 1]].clone();
                    for ri in partition_row_indices.drain(..) {
                        cancel.tick()?;
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
                .map(|&(index, expr, _)| {
                    index
                        .map(|i| rows[row_idx][i].clone())
                        .unwrap_or_else(|| eval_expr(expr, &rows[row_idx], &columns))
                })
                .collect();
            let same_as_prev = prev_order_key.as_ref() == Some(&current_order_key);
            let current_arg = || {
                arg_expr.map(|expr| {
                    arg_col_idx
                        .map(|index| rows[row_idx][index].clone())
                        .unwrap_or_else(|| eval_expr(expr, &rows[row_idx], &columns))
                })
            };
            let count_all =
                arg_expr.is_none() || matches!(arg_expr, Some(Expr::Field(name)) if name == "*");

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
                    if let Some(value) = current_arg() {
                        match value {
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
                    if let Some(value) = current_arg() {
                        match value {
                            Value::Int(v) => {
                                running_float_sum += v as f64;
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
                    if count_all {
                        running_count += 1;
                    } else if let Some(value) = current_arg() {
                        if !value.is_empty() {
                            running_count += 1;
                        }
                    }
                    Value::Int(running_count)
                }
                WindowFunc::Min => {
                    if let Some(v) = current_arg() {
                        if !v.is_empty() {
                            running_min = Some(match &running_min {
                                None => v,
                                Some(cur) => {
                                    if v < *cur {
                                        v
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
                    if let Some(v) = current_arg() {
                        if !v.is_empty() {
                            running_max = Some(match &running_max {
                                None => v,
                                Some(cur) => {
                                    if v > *cur {
                                        v
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
                cancel.tick()?;
                win_values[ri] = final_v.clone();
            }
        }

        // Append the computed window column to each row.
        for (ri, row) in rows.iter_mut().enumerate() {
            cancel.tick()?;
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
pub(super) fn resolve_group_column(name: &str, columns: &[String]) -> Result<usize, QueryError> {
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
pub(crate) fn exec_group_by(
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    keys: &[GroupKey],
    aggregates: &[GroupAgg],
    having: &Option<Expr>,
) -> Result<QueryResult, QueryError> {
    exec_group_by_internal(columns, rows, None, keys, aggregates, having)
}

pub(crate) fn exec_group_by_with_provenance(
    input: ProvenanceRows,
    keys: &[GroupKey],
    aggregates: &[GroupAgg],
    having: &Option<Expr>,
    memory_limit: usize,
) -> Result<QueryResult, QueryError> {
    let ProvenanceRows {
        columns,
        rows,
        source_aliases,
        provenance,
    } = input;
    exec_group_by_internal(
        columns,
        rows,
        Some(GroupProvenance {
            source_aliases,
            rows: provenance,
            memory_limit,
        }),
        keys,
        aggregates,
        having,
    )
}

struct GroupProvenance {
    source_aliases: Vec<String>,
    rows: Vec<Vec<Option<RowId>>>,
    memory_limit: usize,
}

fn exec_group_by_internal(
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    provenance: Option<GroupProvenance>,
    keys: &[GroupKey],
    aggregates: &[GroupAgg],
    having: &Option<Expr>,
) -> Result<QueryResult, QueryError> {
    // Stored fields resolve once and read directly. Expression-valued keys
    // (including JSON paths) use the common expression evaluator per row.
    let key_indices: Vec<Option<usize>> = keys
        .iter()
        .map(|k| resolve_direct_group_expr(&k.expr, &columns))
        .collect::<Result<Vec<_>, _>>()?;

    let agg_field_indices: Vec<Option<usize>> = aggregates
        .iter()
        .map(|a| resolve_direct_group_expr(&a.argument, &columns))
        .collect::<Result<Vec<_>, _>>()?;
    let agg_source_indices: Vec<Option<usize>> = aggregates
        .iter()
        .map(|aggregate| {
            aggregate
                .provenance_alias
                .as_ref()
                .map(|alias| {
                    provenance
                        .as_ref()
                        .and_then(|provenance| {
                            provenance
                                .source_aliases
                                .iter()
                                .position(|source| source == alias)
                        })
                        .ok_or_else(|| {
                            QueryError::Execution(format!(
                                "symmetric aggregate source alias '{alias}' is not present in its input"
                            ))
                        })
                })
                .transpose()
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Group rows by key values (preserving insertion order).
    let mut group_map: rustc_hash::FxHashMap<Vec<Value>, usize> = rustc_hash::FxHashMap::default();
    let mut groups: Vec<(Vec<Value>, Vec<usize>)> = Vec::new();
    let mut cancel = CancelCheck::new();
    for (ri, row) in rows.iter().enumerate() {
        cancel.tick()?;
        let key: Vec<Value> = keys
            .iter()
            .zip(&key_indices)
            .map(|(key, index)| match index {
                Some(index) => row[*index].clone(),
                None => eval_expr(&key.expr, row, &columns),
            })
            .collect();
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
        cancel.tick()?;
        let mut row = key_vals.clone();
        for (ai, agg) in aggregates.iter().enumerate() {
            let val = compute_group_aggregate(
                agg.function,
                &agg.argument,
                agg_field_indices[ai],
                GroupAggregateContext {
                    columns: &columns,
                    all_rows: &rows,
                    row_indices,
                    source_index: agg_source_indices[ai],
                    provenance: provenance
                        .as_ref()
                        .map(|provenance| (provenance.rows.as_slice(), provenance.memory_limit)),
                },
            )?;
            row.push(val);
        }
        out_rows.push(row);
    }

    // Apply HAVING filter.
    if let Some(having_expr) = having {
        let mut filtered = Vec::with_capacity(out_rows.len());
        for row in out_rows {
            cancel.tick()?;
            if eval_predicate(having_expr, &row, &out_columns) {
                filtered.push(row);
            }
        }
        out_rows = filtered;
    }

    Ok(QueryResult::Rows {
        columns: out_columns,
        rows: out_rows,
    })
}

fn resolve_direct_group_expr(expr: &Expr, columns: &[String]) -> Result<Option<usize>, QueryError> {
    match expr {
        Expr::Field(name) if name == "*" => Ok(None),
        Expr::Field(name) => resolve_group_column(name, columns).map(Some),
        Expr::QualifiedField { qualifier, field } => {
            resolve_group_column(&format!("{qualifier}.{field}"), columns).map(Some)
        }
        _ => Ok(None),
    }
}

/// Evaluate a scalar aggregate over already materialized rows. Stored-field
/// aggregates retain their raw-column fast path in the caller; this generic
/// path is also able to aggregate arbitrary expressions such as JSON paths.
pub(crate) fn aggregate_rows(
    func: AggFunc,
    argument: Option<&Expr>,
    columns: &[String],
    rows: &[Vec<Value>],
) -> Result<QueryResult, QueryError> {
    let mut cancel = CancelCheck::new();
    if func == AggFunc::Count && counts_every_row(argument) {
        return Ok(QueryResult::Scalar(Value::Int(rows.len() as i64)));
    }
    let argument = argument.ok_or_else(|| {
        QueryError::Execution(format!(
            "{} requires an argument",
            format!("{func:?}").to_lowercase()
        ))
    })?;

    let mut values = Vec::with_capacity(rows.len());
    for row in rows {
        cancel.tick()?;
        values.push(eval_expr(argument, row, columns));
    }

    let value = match func {
        AggFunc::Count => Value::Int(values.iter().filter(|v| !v.is_empty()).count() as i64),
        AggFunc::CountDistinct => {
            let seen: std::collections::HashSet<Value> =
                values.into_iter().filter(|v| !v.is_empty()).collect();
            Value::Int(seen.len() as i64)
        }
        AggFunc::Avg => {
            let mut sum = 0.0;
            let mut count = 0_u64;
            for value in values {
                match value {
                    Value::Int(v) => {
                        sum += v as f64;
                        count += 1;
                    }
                    Value::Float(v) => {
                        sum += v;
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
        AggFunc::Sum => {
            let mut int_sum = 0_i64;
            let mut float_sum = 0.0;
            let mut saw_float = false;
            for value in values {
                match value {
                    Value::Int(v) => int_sum += v,
                    Value::Float(v) => {
                        float_sum += v;
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
        AggFunc::Min | AggFunc::Max => {
            let mut result: Option<Value> = None;
            for value in values.into_iter().filter(|v| !v.is_empty()) {
                let replace = match &result {
                    None => true,
                    Some(current) if func == AggFunc::Min => value < *current,
                    Some(current) => value > *current,
                };
                if replace {
                    result = Some(value);
                }
            }
            result.unwrap_or(Value::Empty)
        }
    };
    Ok(QueryResult::Scalar(value))
}

const SYMMETRIC_RID_SET_ENTRY_BYTES: usize =
    std::mem::size_of::<RowId>() + 2 * std::mem::size_of::<usize>();

pub(crate) fn aggregate_rows_with_provenance(
    func: AggFunc,
    argument: Option<&Expr>,
    input: &ProvenanceRows,
    provenance_alias: &str,
    memory_limit: usize,
) -> Result<QueryResult, QueryError> {
    if matches!(func, AggFunc::Min | AggFunc::Max | AggFunc::CountDistinct) {
        return aggregate_rows(func, argument, &input.columns, &input.rows);
    }
    let argument = argument.ok_or_else(|| {
        QueryError::Execution(
            "symmetric aggregate requires a source-valued argument; use raw".to_string(),
        )
    })?;
    let source_index = input.source_index(provenance_alias).ok_or_else(|| {
        QueryError::Execution(format!(
            "symmetric aggregate source alias '{provenance_alias}' is not present in its input"
        ))
    })?;
    let mut seen = HashSet::new();
    let mut int_sum = 0_i64;
    let mut float_sum = 0.0_f64;
    let mut saw_float = false;
    let mut count = 0_u64;
    let mut cancel = CancelCheck::new();
    for (row, row_provenance) in input.rows.iter().zip(&input.provenance) {
        cancel.tick()?;
        let value = eval_expr(argument, row, &input.columns);
        if value.is_empty() {
            continue;
        }
        let Some(rid) = row_provenance[source_index] else {
            continue;
        };
        if !seen.insert(rid) {
            continue;
        }
        mem_budget::charge(SYMMETRIC_RID_SET_ENTRY_BYTES, memory_limit)?;
        match func {
            AggFunc::Count => count += 1,
            AggFunc::Sum | AggFunc::Avg => match value {
                Value::Int(value) => {
                    int_sum += value;
                    count += 1;
                }
                Value::Float(value) => {
                    float_sum += value;
                    saw_float = true;
                    count += 1;
                }
                _ => {}
            },
            AggFunc::CountDistinct | AggFunc::Min | AggFunc::Max => unreachable!(),
        }
    }
    let value = match func {
        AggFunc::Count => Value::Int(count as i64),
        AggFunc::Sum if saw_float => Value::Float(float_sum + int_sum as f64),
        AggFunc::Sum => Value::Int(int_sum),
        AggFunc::Avg if count == 0 => Value::Empty,
        AggFunc::Avg => Value::Float((float_sum + int_sum as f64) / count as f64),
        AggFunc::CountDistinct | AggFunc::Min | AggFunc::Max => unreachable!(),
    };
    Ok(QueryResult::Scalar(value))
}

/// Mission E2b: compute one aggregate over a set of rows in a group.
pub(crate) struct GroupAggregateContext<'a> {
    pub(crate) columns: &'a [String],
    pub(crate) all_rows: &'a [Vec<Value>],
    pub(crate) row_indices: &'a [usize],
    pub(crate) source_index: Option<usize>,
    pub(crate) provenance: Option<(&'a [Vec<Option<RowId>>], usize)>,
}

pub(crate) fn compute_group_aggregate(
    func: AggFunc,
    argument: &Expr,
    direct_index: Option<usize>,
    context: GroupAggregateContext<'_>,
) -> Result<Value, QueryError> {
    let GroupAggregateContext {
        columns,
        all_rows,
        row_indices,
        source_index,
        provenance,
    } = context;
    let count_all = matches!(argument, Expr::Field(name) if name == "*");
    let value_at = |ri: usize| match direct_index {
        Some(index) => all_rows[ri][index].clone(),
        None => eval_expr(argument, &all_rows[ri], columns),
    };
    let mut cancel = CancelCheck::new();
    let mut seen_rids = HashSet::new();
    match func {
        AggFunc::Count => {
            if count_all {
                // count(*) — count all rows in the group.
                return Ok(Value::Int(row_indices.len() as i64));
            }
            let mut count = 0usize;
            for &ri in row_indices {
                cancel.tick()?;
                let value = value_at(ri);
                if !value.is_empty()
                    && accept_symmetric_contribution(ri, source_index, provenance, &mut seen_rids)?
                {
                    count += 1;
                }
            }
            Ok(Value::Int(count as i64))
        }
        AggFunc::CountDistinct => {
            let mut seen = std::collections::HashSet::new();
            for &ri in row_indices {
                cancel.tick()?;
                let v = value_at(ri);
                if !v.is_empty() {
                    seen.insert(v);
                }
            }
            Ok(Value::Int(seen.len() as i64))
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
                cancel.tick()?;
                let value = value_at(ri);
                if value.is_empty()
                    || !accept_symmetric_contribution(ri, source_index, provenance, &mut seen_rids)?
                {
                    continue;
                }
                match value {
                    Value::Int(v) => int_sum += v,
                    Value::Float(v) => {
                        float_sum += v;
                        saw_float = true;
                    }
                    _ => {}
                }
            }
            if saw_float {
                Ok(Value::Float(float_sum + int_sum as f64))
            } else {
                Ok(Value::Int(int_sum))
            }
        }
        AggFunc::Avg => {
            let mut sum = 0.0f64;
            let mut count = 0usize;
            for &ri in row_indices {
                cancel.tick()?;
                let value = value_at(ri);
                if value.is_empty()
                    || !accept_symmetric_contribution(ri, source_index, provenance, &mut seen_rids)?
                {
                    continue;
                }
                match value {
                    Value::Int(v) => {
                        sum += v as f64;
                        count += 1;
                    }
                    Value::Float(v) => {
                        sum += v;
                        count += 1;
                    }
                    _ => {}
                }
            }
            if count == 0 {
                Ok(Value::Empty)
            } else {
                Ok(Value::Float(sum / count as f64))
            }
        }
        AggFunc::Min | AggFunc::Max => {
            let mut result: Option<Value> = None;
            for &ri in row_indices {
                cancel.tick()?;
                let value = value_at(ri);
                if value.is_empty() {
                    continue;
                }
                let replace = match &result {
                    None => true,
                    Some(current) if func == AggFunc::Min => value < *current,
                    Some(current) => value > *current,
                };
                if replace {
                    result = Some(value);
                }
            }
            Ok(result.unwrap_or(Value::Empty))
        }
    }
}

fn accept_symmetric_contribution(
    row_index: usize,
    source_index: Option<usize>,
    provenance: Option<(&[Vec<Option<RowId>>], usize)>,
    seen: &mut HashSet<RowId>,
) -> Result<bool, QueryError> {
    let Some(source_index) = source_index else {
        return Ok(true);
    };
    let Some((provenance, memory_limit)) = provenance else {
        return Err(QueryError::Execution(
            "symmetric aggregate provenance is unavailable; use raw".to_string(),
        ));
    };
    let Some(rid) = provenance[row_index][source_index] else {
        return Ok(false);
    };
    if !seen.insert(rid) {
        return Ok(false);
    }
    mem_budget::charge(SYMMETRIC_RID_SET_ENTRY_BYTES, memory_limit)?;
    Ok(true)
}
