//! The execute_plan method and associated helpers.

use crate::ast::*;
use crate::cancel::CancelCheck;
use crate::plan::*;
use crate::result::QueryError;
use powdb_storage::catalog::{Catalog, ExpressionIndexMeta};
use powdb_storage::stored_json_path::StoredJsonPathV1;
use powdb_storage::types::*;
use std::ops::ControlFlow;

use super::compiled::*;
use super::mem_budget;

/// Whether a `count` aggregate counts every row rather than the non-null
/// values of a target column. `count(T)` carries no argument and `count(*)`
/// carries the `*` sentinel field; anything else names a column, and
/// `count(T { .col })` must then skip nulls (the grouped path and SQL's
/// `COUNT(col)` both do).
pub(crate) fn counts_every_row(argument: Option<&Expr>) -> bool {
    argument.is_none_or(|argument| matches!(argument, Expr::Field(name) if name == "*"))
}

/// The bound a `Limit` node's count names, or `None` when it is not something a
/// fast path may act on.
///
/// A fast path that carries its own bound must **decline** on `None` and let the
/// generic `PlanNode::Limit` arm run: that arm is the single place that reports
/// `limit must be integer literal`, and negative counts are already refused up
/// front by [`validate_slice_counts`]. Substituting `usize::MAX` for a count the
/// fast path could not read is what made `F limit 1 + 1 { .id }` answer the whole
/// table while the same query without the projection was rejected — and, because
/// the top-N heap takes its capacity from the same number, it also lifted the
/// bound that keeps a rejected query from heaping every row in the table.
pub(super) fn literal_limit(count: &Expr) -> Option<usize> {
    match count {
        Expr::Literal(Literal::Int(value)) if *value >= 0 => Some(*value as usize),
        _ => None,
    }
}

/// Maximum number of elements sorted by the standard-library stable sort
/// without an intervening cancellation checkpoint. Larger inputs are sorted
/// as bounded stable runs and cooperatively merged below.
const CANCELLABLE_SORT_RUN: usize = 2_048;

// Test-only instrumentation: counts how often the O(N*M) `generic_rid_match`
// fallback runs, so a shape test can assert an index-driven mutation never
// degrades into it. Compiled out entirely in non-test builds.
#[cfg(test)]
thread_local! {
    static GENERIC_RID_MATCH_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) fn reset_generic_rid_match_calls() {
    GENERIC_RID_MATCH_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(super) fn generic_rid_match_calls() -> u64 {
    GENERIC_RID_MATCH_CALLS.with(std::cell::Cell::get)
}

/// Compare ORDER BY values with the engine-wide `NULLS LAST` contract.
///
/// `Value::Empty` represents both SQL/PowQL null and a missing JSON path.
/// Direction only reverses non-null values, so nulls remain last for both
/// ascending and descending order.
pub(super) fn compare_order_values(
    left: &Value,
    right: &Value,
    descending: bool,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    match (left, right) {
        (Value::Empty, Value::Empty) => Ordering::Equal,
        (Value::Empty, _) => Ordering::Greater,
        (_, Value::Empty) => Ordering::Less,
        _ if descending => left.cmp(right).reverse(),
        _ => left.cmp(right),
    }
}

/// Stable sort with cooperative cancellation throughout the sort itself.
///
/// We sort original positions rather than moving potentially large rows on
/// every merge pass. Each bounded run uses Rust's stable sort, with immediate
/// checks on both sides; bottom-up merges poll while emitting positions and
/// prefer the left run on equality, preserving global stability. The final
/// permutation is applied in place and also polls. Two `usize` arrays are the
/// only scratch allocation and are charged to the query memory budget.
pub(super) fn cooperative_stable_sort_by<T, F>(
    values: &mut [T],
    memory_limit: usize,
    compare: F,
) -> Result<(), QueryError>
where
    F: Fn(&T, &T) -> std::cmp::Ordering,
{
    crate::cancel::check()?;
    let len = values.len();
    if len < 2 {
        return Ok(());
    }

    let scratch_bytes = len
        .saturating_mul(std::mem::size_of::<usize>())
        .saturating_mul(2);
    mem_budget::charge(scratch_bytes, memory_limit)?;

    let mut order: Vec<usize> = (0..len).collect();
    let mut scratch = vec![0usize; len];

    for run in order.chunks_mut(CANCELLABLE_SORT_RUN) {
        crate::cancel::check()?;
        run.sort_by(|&a, &b| compare(&values[a], &values[b]));
        crate::cancel::check()?;
    }

    let mut cancel = CancelCheck::new();
    let mut width = CANCELLABLE_SORT_RUN;
    while width < len {
        let step = width.saturating_mul(2);
        let mut start = 0usize;
        while start < len {
            let mid = start.saturating_add(width).min(len);
            let end = start.saturating_add(step).min(len);
            let (mut left, mut right, mut out) = (start, mid, start);

            while left < mid && right < end {
                cancel.tick()?;
                if compare(&values[order[left]], &values[order[right]])
                    != std::cmp::Ordering::Greater
                {
                    scratch[out] = order[left];
                    left += 1;
                } else {
                    scratch[out] = order[right];
                    right += 1;
                }
                out += 1;
            }
            while left < mid {
                cancel.tick()?;
                scratch[out] = order[left];
                left += 1;
                out += 1;
            }
            while right < end {
                cancel.tick()?;
                scratch[out] = order[right];
                right += 1;
                out += 1;
            }
            start = start.saturating_add(step);
        }
        std::mem::swap(&mut order, &mut scratch);
        width = step;
    }

    // `order[new_position] = old_position`; invert it to a destination for
    // each item, then apply permutation cycles without cloning row payloads.
    for (new_position, &old_position) in order.iter().enumerate() {
        cancel.tick()?;
        scratch[old_position] = new_position;
    }
    drop(order);
    for position in 0..len {
        while scratch[position] != position {
            cancel.tick()?;
            let destination = scratch[position];
            values.swap(position, destination);
            scratch.swap(position, destination);
        }
    }
    Ok(())
}

/// Run a raw table scan with cooperative cancellation while preserving the
/// existing allocation-free callback shape used by hot read paths.
pub(super) fn for_each_row_raw_cancellable(
    catalog: &Catalog,
    table: &str,
    mut f: impl FnMut(RowId, &[u8]),
) -> Result<(), QueryError> {
    // Embedded/direct execution normally has no cancellation token. Preserve
    // the original zero-poll raw-scan hot path in that case instead of paying
    // a counter increment and mask branch for every row. The active-install
    // flag is a process-wide hint, so a true result still enters the
    // authoritative thread-local polling path below.
    if !crate::cancel::has_active_install() {
        return catalog
            .for_each_row_raw(table, f)
            .map_err(|err| QueryError::StorageError(err.to_string()));
    }

    let mut cancel = CancelCheck::new();
    let mut cancel_err: Option<QueryError> = None;
    catalog
        .try_for_each_row_raw(table, |rid, data| {
            if let Err(err) = cancel.tick() {
                cancel_err = Some(err);
                return ControlFlow::Break(());
            }
            f(rid, data);
            ControlFlow::Continue(())
        })
        .map_err(|err| QueryError::StorageError(err.to_string()))?;
    match cancel_err {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn resolve_expression_index(
    catalog: &Catalog,
    table: &str,
    path: &StoredJsonPathV1,
) -> Option<ExpressionIndexMeta> {
    catalog
        .expression_index_metadata(table)?
        .into_iter()
        .find(|metadata| metadata.canonical_version == 1 && metadata.json_path == *path)
}

fn expression_index_fallback(plan: &PlanNode) -> Option<PlanNode> {
    match plan {
        PlanNode::ExprIndexScan { table, path, key } => Some(PlanNode::Filter {
            input: Box::new(PlanNode::SeqScan {
                table: table.clone(),
            }),
            predicate: Expr::BinaryOp(
                Box::new(stored_json_path_expr(path)),
                BinOp::Eq,
                Box::new(key.clone()),
            ),
        }),
        PlanNode::ExprRangeScan {
            table,
            path,
            start,
            end,
        } => Some(PlanNode::Filter {
            input: Box::new(PlanNode::SeqScan {
                table: table.clone(),
            }),
            predicate: synthesize_expr_range_predicate(path, start, end),
        }),
        PlanNode::OrderedExprIndexScan {
            table,
            path,
            descending,
            limit,
            offset,
        } => {
            let sorted = PlanNode::Sort {
                input: Box::new(PlanNode::SeqScan {
                    table: table.clone(),
                }),
                keys: vec![SortKey {
                    expr: stored_json_path_expr(path),
                    descending: *descending,
                }],
            };
            let sliced = match offset {
                Some(count) => PlanNode::Offset {
                    input: Box::new(sorted),
                    count: count.clone(),
                },
                None => sorted,
            };
            Some(PlanNode::Limit {
                input: Box::new(sliced),
                count: limit.clone(),
            })
        }
        _ => None,
    }
}

#[derive(Debug)]
pub(crate) struct ProvenanceRows {
    pub(super) columns: Vec<String>,
    pub(super) rows: Vec<Vec<Value>>,
    source_aliases: Vec<String>,
    provenance: Vec<Vec<Option<RowId>>>,
}

impl ProvenanceRows {
    fn source_index(&self, alias: &str) -> Option<usize> {
        self.source_aliases
            .iter()
            .position(|source| source == alias)
    }
}

mod aggregate;
mod dispatch;
mod fast_paths;
mod join;
mod lowering;
mod mutation;
mod scan;
mod validate;

use lowering::{stored_json_path_expr, synthesize_expr_range_predicate};

pub(crate) use aggregate::{
    aggregate_rows, aggregate_rows_with_provenance, exec_group_by, exec_group_by_with_provenance,
    execute_window,
};
#[cfg(test)]
pub(crate) use aggregate::{compute_group_aggregate, GroupAggregateContext};
pub(crate) use dispatch::{nested_fields_have_via_link, scan_source_table};
#[cfg(test)]
pub(crate) use join::check_nested_loop_pair_limit;
pub(crate) use join::execute_materialized_join;
pub(crate) use lowering::{
    format_plan_tree, range_matches, synthesize_range_predicate, LoweredPlan,
};
pub(crate) use validate::{
    predicate_column_indices_json, validate_column_references, validate_json_path_types,
    validate_no_stray_aggregates, validate_slice_counts,
};
