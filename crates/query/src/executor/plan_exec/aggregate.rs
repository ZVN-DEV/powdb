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
        let mut running_total = NumericAgg::new();
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
                    let final_v = whole_partition_final_value(
                        wdef.function,
                        &running_total,
                        &win_values[indices[sorted_pos - 1]],
                    )?;
                    for ri in partition_row_indices.drain(..) {
                        cancel.tick()?;
                        win_values[ri] = final_v.clone();
                    }
                }
                partition_start = sorted_pos;
                running_count = 0;
                running_total = NumericAgg::new();
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
                        running_total.push("sum", &value)?;
                    }
                    if whole_partition_frame {
                        // Every intermediate is overwritten by the back-fill
                        // below, so the i64 conversion is deferred to the
                        // partition total that is actually emitted. Converting
                        // per row would fail a partition like
                        // [5e18, 5e18, -5e18] whose total fits.
                        Value::Empty
                    } else {
                        // A running frame emits this prefix, so a prefix that
                        // leaves i64 is a real overflow of a reported value.
                        running_total.sum("sum")?
                    }
                }
                WindowFunc::Avg => {
                    if let Some(value) = current_arg() {
                        running_total.push("avg", &value)?;
                    }
                    running_total.avg()
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
            let final_v = whole_partition_final_value(
                wdef.function,
                &running_total,
                &win_values[indices[n - 1]],
            )?;
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

/// The value a whole-partition aggregate frame (`over ()` with no `order`)
/// emits for every row of the partition that just ended.
///
/// Every function except `sum` stores its complete value on the partition's
/// last row, so that stored value is the answer. `sum` stores a placeholder
/// instead and finalizes here, because the running total is folded at full
/// i128 width: a prefix may leave `i64` on a partition whose emitted total
/// does not (integer addition is associative, the `i64` conversion is not).
/// A partition total that genuinely leaves `i64` still errors, here.
fn whole_partition_final_value(
    function: WindowFunc,
    running_total: &NumericAgg,
    last_running_value: &Value,
) -> Result<Value, QueryError> {
    if matches!(function, WindowFunc::Sum) {
        running_total.sum("sum")
    } else {
        Ok(last_running_value.clone())
    }
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

/// Running total shared by the generic scalar path, the grouped path, the
/// symmetric (join) path and the window path, so which of those happened to
/// fire cannot change the answer. Two rules it enforces that the per-path
/// accumulators did not:
///   * a non-null value that is neither `Int` nor `Float` is a typed error,
///     not a skipped row. `sum` over a str column used to answer `0` and
///     `avg` used to answer null;
///   * an integer total that leaves `i64` is a typed error, not a wrapped
///     (generic paths) or clamped (fast path) number.
///
/// The compiled fast path in `fast_paths.rs` accumulates separately (it reads
/// column bytes without materializing `Value`s) and only shares
/// [`agg_overflow_error`]. It agrees on every input that contributes a value,
/// but not on one that contributes none: it types the zero from the column
/// (`sum` over an all-null float column answers `Float(0.0)`) while this
/// accumulator types it from the values it saw (`Int(0)`), because an
/// expression argument such as a JSON path has no declared column type to
/// read. Unifying the two is a deliberate semantic decision, see
/// `generic_sum_over_only_nulls_is_int_zero` below.
pub(super) struct NumericAgg {
    int_sum: i128,
    float_sum: f64,
    saw_float: bool,
    count: u64,
}

impl NumericAgg {
    pub(super) fn new() -> Self {
        Self {
            int_sum: 0,
            float_sum: 0.0,
            saw_float: false,
            count: 0,
        }
    }

    /// Fold one value in. `Empty` is PowQL's null and contributes nothing,
    /// exactly as it does for `count`/`min`/`max`.
    pub(super) fn push(&mut self, label: &str, value: &Value) -> Result<(), QueryError> {
        match value {
            Value::Int(v) => {
                self.int_sum = self
                    .int_sum
                    .checked_add(i128::from(*v))
                    .ok_or_else(|| agg_overflow_error(label))?;
            }
            Value::Float(v) => {
                self.float_sum += *v;
                self.saw_float = true;
            }
            Value::Empty => return Ok(()),
            other => return Err(non_numeric_agg_error(label, other)),
        }
        self.count += 1;
        Ok(())
    }

    /// Final `sum`: `Int` unless a `Float` contributed. The `i64` conversion
    /// is where an overflowing integer total surfaces.
    pub(super) fn sum(&self, label: &str) -> Result<Value, QueryError> {
        if self.saw_float {
            return Ok(Value::Float(self.float_sum + self.int_sum as f64));
        }
        i64::try_from(self.int_sum)
            .map(Value::Int)
            .map_err(|_| agg_overflow_error(label))
    }

    /// Final `avg`: always a `Float`, and `Empty` when no numeric row
    /// contributed. The i128 integer total means an input whose sum does not
    /// fit in `i64` still averages correctly instead of erroring.
    pub(super) fn avg(&self) -> Value {
        if self.count == 0 {
            Value::Empty
        } else {
            Value::Float((self.float_sum + self.int_sum as f64) / self.count as f64)
        }
    }
}

/// `sum`/`avg` reached a value it cannot add.
fn non_numeric_agg_error(label: &str, value: &Value) -> QueryError {
    let found = format!("{:?}", value.type_id()).to_lowercase();
    QueryError::TypeError(format!(
        "{label} requires a numeric argument, but a {found} value was aggregated"
    ))
}

/// An integer `sum` total that no longer fits in `i64`. Shared with the
/// compiled fast path so every path reports overflow identically.
///
/// Deliberately NOT [`QueryError::TypeError`]: every argument was a well typed
/// `int`, and only their total left the range, so the `type mismatch: ` prefix
/// that variant's Display adds told the caller something false. The refusal
/// leads with `cannot` so the server's egress allowlist (`SAFE_ERROR_PREFIXES`
/// in crates/server/src/handler/classify.rs) still forwards it verbatim, and it
/// classifies as the wire class `execution` (2) exactly as `TypeError` did, so
/// the stable class byte is unchanged.
pub(super) fn agg_overflow_error(label: &str) -> QueryError {
    QueryError::Execution(format!(
        "cannot compute {label}: the integer total overflows int64"
    ))
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
            let mut total = NumericAgg::new();
            for value in &values {
                total.push("avg", value)?;
            }
            total.avg()
        }
        AggFunc::Sum => {
            let mut total = NumericAgg::new();
            for value in &values {
                total.push("sum", value)?;
            }
            total.sum("sum")?
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
    let label = format!("{func:?}").to_lowercase();
    let mut seen = HashSet::new();
    let mut total = NumericAgg::new();
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
            AggFunc::Sum | AggFunc::Avg => total.push(&label, &value)?,
            AggFunc::CountDistinct | AggFunc::Min | AggFunc::Max => unreachable!(),
        }
    }
    let value = match func {
        AggFunc::Count => Value::Int(count as i64),
        AggFunc::Sum => total.sum(&label)?,
        AggFunc::Avg => total.avg(),
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
        AggFunc::Sum | AggFunc::Avg => {
            // Shares `NumericAgg` with the scalar, symmetric and window
            // paths, so a grouped `sum`/`avg` promotes Float, rejects
            // non-numeric values and reports overflow exactly as they do.
            let label = if func == AggFunc::Sum { "sum" } else { "avg" };
            let mut total = NumericAgg::new();
            for &ri in row_indices {
                cancel.tick()?;
                let value = value_at(ri);
                if value.is_empty()
                    || !accept_symmetric_contribution(ri, source_index, provenance, &mut seen_rids)?
                {
                    continue;
                }
                total.push(label, &value)?;
            }
            if func == AggFunc::Sum {
                total.sum(label)
            } else {
                Ok(total.avg())
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

#[cfg(test)]
mod tests {
    use super::*;

    const HUGE: i64 = 4_000_000_000_000_000_000;

    fn str_rows() -> Vec<Vec<Value>> {
        vec![vec![Value::Str("north".to_string())]]
    }

    fn overflow_rows() -> Vec<Vec<Value>> {
        vec![
            vec![Value::Int(HUGE)],
            vec![Value::Int(HUGE)],
            vec![Value::Int(HUGE)],
        ]
    }

    fn value_columns() -> Vec<String> {
        vec!["v".to_string()]
    }

    fn value_field() -> Expr {
        Expr::Field("v".to_string())
    }

    fn scalar_agg(func: AggFunc, rows: &[Vec<Value>]) -> Result<QueryResult, QueryError> {
        aggregate_rows(func, Some(&value_field()), &value_columns(), rows)
    }

    fn grouped_agg(func: AggFunc, rows: Vec<Vec<Value>>) -> Result<QueryResult, QueryError> {
        exec_group_by(
            value_columns(),
            rows,
            &[],
            &[GroupAgg {
                function: func,
                argument: value_field(),
                mode: AggregateMode::Raw,
                provenance_alias: None,
                output_name: "total".to_string(),
            }],
            &None,
        )
    }

    fn symmetric_agg(func: AggFunc, rows: Vec<Vec<Value>>) -> Result<QueryResult, QueryError> {
        let provenance = (0..rows.len())
            .map(|slot| {
                vec![Some(RowId {
                    page_id: 1,
                    slot_index: slot as u16,
                })]
            })
            .collect();
        let input = ProvenanceRows {
            columns: vec!["a.v".to_string()],
            rows,
            source_aliases: vec!["a".to_string()],
            provenance,
        };
        aggregate_rows_with_provenance(
            func,
            Some(&Expr::QualifiedField {
                qualifier: "a".to_string(),
                field: "v".to_string(),
            }),
            &input,
            "a",
            usize::MAX,
        )
    }

    fn window_over(
        func: WindowFunc,
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
        partition_by: Vec<Expr>,
        order_by: Vec<SortKey>,
    ) -> Result<QueryResult, QueryError> {
        execute_window(
            QueryResult::Rows { columns, rows },
            &[WindowDef {
                function: func,
                args: vec![value_field()],
                mode: AggregateMode::Raw,
                partition_by,
                order_by,
                output_name: "w".to_string(),
            }],
            usize::MAX,
        )
    }

    fn window_agg(func: WindowFunc, rows: Vec<Vec<Value>>) -> Result<QueryResult, QueryError> {
        window_over(func, value_columns(), rows, Vec::new(), Vec::new())
    }

    /// The appended window column, in the input's original row order.
    fn window_column(result: Result<QueryResult, QueryError>) -> Vec<Value> {
        match result {
            Ok(QueryResult::Rows { rows, .. }) => rows
                .into_iter()
                .map(|row| row.last().cloned().expect("window column is appended"))
                .collect(),
            other => panic!("expected window rows, got {other:?}"),
        }
    }

    fn assert_non_numeric(result: Result<QueryResult, QueryError>) {
        match result {
            Err(QueryError::TypeError(message)) => {
                assert!(message.contains("numeric"), "unexpected message: {message}");
            }
            other => panic!("expected a non-numeric type error, got {other:?}"),
        }
    }

    fn assert_overflow(result: Result<QueryResult, QueryError>) {
        match result {
            Err(QueryError::Execution(message)) => {
                assert!(
                    message.contains("overflow"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected an overflow error, got {other:?}"),
        }
    }

    // ── B1: sum/avg over a non-numeric value is an error on every path ──
    //
    // The catch-all match arms used to skip str/bool/datetime/uuid/bytes/json
    // silently, so `sum` answered 0 and `avg` answered null over a column that
    // cannot be summed at all.

    #[test]
    fn scalar_sum_over_str_is_a_type_error() {
        assert_non_numeric(scalar_agg(AggFunc::Sum, &str_rows()));
    }

    #[test]
    fn scalar_avg_over_str_is_a_type_error() {
        assert_non_numeric(scalar_agg(AggFunc::Avg, &str_rows()));
    }

    #[test]
    fn grouped_sum_over_str_is_a_type_error() {
        assert_non_numeric(grouped_agg(AggFunc::Sum, str_rows()));
    }

    #[test]
    fn grouped_avg_over_str_is_a_type_error() {
        assert_non_numeric(grouped_agg(AggFunc::Avg, str_rows()));
    }

    #[test]
    fn symmetric_sum_over_str_is_a_type_error() {
        assert_non_numeric(symmetric_agg(AggFunc::Sum, str_rows()));
    }

    #[test]
    fn symmetric_avg_over_str_is_a_type_error() {
        assert_non_numeric(symmetric_agg(AggFunc::Avg, str_rows()));
    }

    #[test]
    fn window_sum_over_str_is_a_type_error() {
        assert_non_numeric(window_agg(WindowFunc::Sum, str_rows()));
    }

    #[test]
    fn window_avg_over_str_is_a_type_error() {
        assert_non_numeric(window_agg(WindowFunc::Avg, str_rows()));
    }

    #[test]
    fn non_numeric_error_names_the_offending_type() {
        for (value, type_name) in [
            (Value::Bool(true), "bool"),
            (Value::DateTime(0), "datetime"),
            (Value::Uuid([0; 16]), "uuid"),
            (Value::Bytes(vec![1]), "bytes"),
        ] {
            let error = scalar_agg(AggFunc::Sum, &[vec![value]]).unwrap_err();
            assert!(
                error.to_string().contains(type_name),
                "expected {type_name} in {error}"
            );
        }
    }

    #[test]
    fn nulls_are_still_skipped_rather_than_rejected() {
        let rows = vec![vec![Value::Int(2)], vec![Value::Empty], vec![Value::Int(3)]];
        assert!(matches!(
            scalar_agg(AggFunc::Sum, &rows).unwrap(),
            QueryResult::Scalar(Value::Int(5))
        ));
        assert!(matches!(
            window_agg(WindowFunc::Sum, rows).unwrap(),
            QueryResult::Rows { .. }
        ));
    }

    // ── B2: an i64 sum total that overflows is an error, not a clamp ──

    #[test]
    fn scalar_sum_overflow_is_an_error() {
        assert_overflow(scalar_agg(AggFunc::Sum, &overflow_rows()));
    }

    #[test]
    fn grouped_sum_overflow_is_an_error() {
        assert_overflow(grouped_agg(AggFunc::Sum, overflow_rows()));
    }

    #[test]
    fn symmetric_sum_overflow_is_an_error() {
        assert_overflow(symmetric_agg(AggFunc::Sum, overflow_rows()));
    }

    #[test]
    fn window_sum_overflow_is_an_error() {
        assert_overflow(window_agg(WindowFunc::Sum, overflow_rows()));
    }

    // ── B3: a transient running total must not fail a value that fits ──
    //
    // `over ()` with no `order` frames the WHOLE partition, so every running
    // value is discarded and back-filled with the partition total. Converting
    // each running value to i64 turned an input whose total fits into a
    // query-killing overflow error, and made the answer depend on which path
    // ran: the scalar `sum` of the same rows succeeds.

    /// Total 5e18 (fits in i64); the running prefix reaches 1e19 (does not).
    fn transient_overflow_rows() -> Vec<Vec<Value>> {
        vec![
            vec![Value::Int(5_000_000_000_000_000_000)],
            vec![Value::Int(5_000_000_000_000_000_000)],
            vec![Value::Int(-5_000_000_000_000_000_000)],
        ]
    }

    #[test]
    fn window_sum_survives_a_transient_running_overflow() {
        let expected = Value::Int(5_000_000_000_000_000_000);
        assert_eq!(
            window_column(window_agg(WindowFunc::Sum, transient_overflow_rows())),
            vec![expected.clone(), expected.clone(), expected]
        );
    }

    #[test]
    fn window_and_scalar_sum_agree_on_a_transient_overflow() {
        let scalar = match scalar_agg(AggFunc::Sum, &transient_overflow_rows()).unwrap() {
            QueryResult::Scalar(value) => value,
            other => panic!("expected a scalar, got {other:?}"),
        };
        for value in window_column(window_agg(WindowFunc::Sum, transient_overflow_rows())) {
            assert_eq!(value, scalar);
        }
    }

    #[test]
    fn window_sum_finalizes_each_partition_independently() {
        // Two partitions, each transiently overflowing, flushed by different
        // code paths: the first at the partition boundary, the second at the
        // end of the scan.
        let mut rows = Vec::new();
        for key in ["a", "b"] {
            for row in transient_overflow_rows() {
                rows.push(vec![Value::Str(key.to_string()), row[0].clone()]);
            }
        }
        let result = window_over(
            WindowFunc::Sum,
            vec!["k".to_string(), "v".to_string()],
            rows,
            vec![Expr::Field("k".to_string())],
            Vec::new(),
        );
        assert_eq!(
            window_column(result),
            vec![Value::Int(5_000_000_000_000_000_000); 6]
        );
    }

    #[test]
    fn window_running_sum_still_errors_when_an_emitted_prefix_overflows() {
        // With an `order` clause the running prefix IS the emitted value, so
        // a prefix of 1e19 is a real overflow of a reported number.
        assert_overflow(window_over(
            WindowFunc::Sum,
            value_columns(),
            transient_overflow_rows(),
            Vec::new(),
            vec![SortKey {
                expr: value_field(),
                descending: true,
            }],
        ));
    }

    #[test]
    fn whole_partition_frame_still_back_fills_the_other_aggregates() {
        // Only `sum` defers its finalization; the rest keep back-filling the
        // partition's complete running value onto every row.
        let rows = vec![vec![Value::Int(2)], vec![Value::Empty], vec![Value::Int(8)]];
        for (func, expected) in [
            (WindowFunc::Avg, Value::Float(5.0)),
            (WindowFunc::Min, Value::Int(2)),
            (WindowFunc::Max, Value::Int(8)),
            (WindowFunc::Count, Value::Int(2)),
        ] {
            assert_eq!(
                window_column(window_agg(func, rows.clone())),
                vec![expected.clone(); 3],
                "{func:?}"
            );
        }
    }

    // ── B4: the aggregate error messages are wire-visible ──
    //
    // `sum`/`avg` errors reach clients through the server's egress
    // allowlist ("type mismatch" and "cannot" are both safe prefixes), and
    // their `QueryError` variant is the wire error class a driver switches on.
    // Both are pinned byte-exact here and, end to end through the engine, in
    // crates/query/tests/error_display.rs.

    #[test]
    fn overflow_error_does_not_claim_a_type_mismatch() {
        let error = scalar_agg(AggFunc::Sum, &overflow_rows()).unwrap_err();
        assert!(
            !matches!(error, QueryError::TypeError(_)),
            "every summed value was a well typed int; only the total overflowed, \
             so the `type mismatch: ` prefix would be false. got {error:?}"
        );
        assert_eq!(
            error.to_string(),
            "cannot compute sum: the integer total overflows int64"
        );
    }

    #[test]
    fn non_numeric_error_message_is_byte_exact() {
        let error = scalar_agg(AggFunc::Sum, &str_rows()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "type mismatch: sum requires a numeric argument, but a str value was aggregated"
        );
    }

    // ── B5: the type of a zero total, when nothing contributed ──

    #[test]
    fn generic_sum_over_only_nulls_is_int_zero() {
        // Pinned because it is the one input on which the generic paths and
        // the compiled fast path disagree: over an all-null FLOAT column the
        // fast path answers `Float(0.0)` (it types the zero from the column)
        // and every generic path answers `Int(0)` (it types the zero from the
        // values it summed, because an expression argument has no column type
        // to read).
        //
        // That is user-visible, and the plan shape alone decides it. On
        // `type F { required id: int, v: float }` holding two rows with a null
        // `v`, `sum(F { .v })` answers `Float(0.0)` while
        // `sum(F filter .id in (1,2) { .v })` and `F group .id { s: sum(.v) }`
        // answer `Int(0)`.
        //
        // Closing it needs BOTH sides changed in one step, because either half
        // on its own widens the split (making the generic zero `Empty` would
        // newly disagree with the fast path's `Int(0)` on an int column). The
        // fast-path half lives in the `TypeId::Float => AggFunc::Sum` arm of
        // `fast_paths.rs::agg_single_col_fast`; see the note on `NumericAgg`.
        let nulls = vec![vec![Value::Empty], vec![Value::Empty]];
        assert!(matches!(
            scalar_agg(AggFunc::Sum, &nulls).unwrap(),
            QueryResult::Scalar(Value::Int(0))
        ));
        assert_eq!(
            window_column(window_agg(WindowFunc::Sum, nulls)),
            vec![Value::Int(0); 2]
        );
    }

    #[test]
    fn avg_of_large_ints_stays_exact_instead_of_overflowing() {
        // `avg` accumulates in i128 and only ever reports a Float, so a total
        // that cannot fit in i64 is still a correct answer, not an error.
        let expected = HUGE as f64;
        for result in [
            scalar_agg(AggFunc::Avg, &overflow_rows()).unwrap(),
            symmetric_agg(AggFunc::Avg, overflow_rows()).unwrap(),
        ] {
            match result {
                QueryResult::Scalar(Value::Float(avg)) => {
                    assert!((avg - expected).abs() < 1.0, "got {avg}");
                }
                other => panic!("expected a float average, got {other:?}"),
            }
        }
    }
}
