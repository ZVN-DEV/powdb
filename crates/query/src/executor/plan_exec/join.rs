//! Join execution: hash and nested-loop joins, materialized and
//! provenance-preserving variants.

use crate::cancel::CancelCheck;
use crate::result::{QueryError, QueryResult};
use powdb_storage::types::*;

use crate::executor::check_join_limit;
use crate::executor::eval::*;

use super::*;

struct HashJoinSpec<'a> {
    left_key_idx: usize,
    right_key_idx: usize,
    residuals: Vec<&'a Expr>,
}

struct MaterializedJoinInputs {
    left_columns: Vec<String>,
    left_rows: Vec<Vec<Value>>,
    right_columns: Vec<String>,
    right_rows: Vec<Vec<Value>>,
}

pub(super) fn flatten_conjunctions<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    match expr {
        Expr::BinaryOp(left, BinOp::And, right) => {
            flatten_conjunctions(left, out);
            flatten_conjunctions(right, out);
        }
        _ => out.push(expr),
    }
}

/// Extract one cross-side equality from an arbitrary AND conjunction. The
/// chosen equality becomes the hash key and every other conjunct remains a
/// residual predicate evaluated only inside the matching hash bucket.
fn try_extract_hash_join<'a>(
    pred: &'a Expr,
    left_columns: &[String],
    right_columns: &[String],
) -> Option<HashJoinSpec<'a>> {
    let mut conjuncts = Vec::new();
    flatten_conjunctions(pred, &mut conjuncts);
    for (key_position, conjunct) in conjuncts.iter().enumerate() {
        let Some((left_key_idx, right_key_idx)) =
            try_extract_equi_join_keys(conjunct, left_columns, right_columns)
        else {
            continue;
        };
        let residuals = conjuncts
            .iter()
            .enumerate()
            .filter_map(|(position, residual)| (position != key_position).then_some(*residual))
            .collect();
        return Some(HashJoinSpec {
            left_key_idx,
            right_key_idx,
            residuals,
        });
    }
    None
}

/// Resolve a single cross-side equality, accepting either operand orientation.
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
        // An unqualified `on .user_id = ...` inside a join resolves the same way
        // the evaluator does, so the hash-join key extraction sees the column
        // instead of falling back to the nested loop.
        Expr::Field(name) => resolve_column_index(name, columns),
        _ => None,
    }
}

/// O(L + R + matching bucket candidates) hash join. Residual predicates are
/// evaluated only after the equi-key probe has found a candidate. For
/// `JoinKind::LeftOuter`, a left row is padded with `Value::Empty` when there
/// is no key bucket or when every candidate in its bucket fails a residual.
///
/// The right side is always the build side. That choice is forced for
/// LeftOuter (the left side must stream so we can detect orphans), and
/// for Inner it's a reasonable default — left-deep plans tend to grow the
/// left side with each join, so the un-joined right leaf is often the
/// smaller of the two at each level.
fn hash_join(
    inputs: MaterializedJoinInputs,
    left_key_idx: usize,
    right_key_idx: usize,
    kind: JoinKind,
    residuals: &[&Expr],
) -> Result<QueryResult, QueryError> {
    use rustc_hash::FxHashMap;

    let MaterializedJoinInputs {
        left_columns,
        left_rows,
        right_columns,
        right_rows,
    } = inputs;

    let n_left = left_columns.len();
    let n_right = right_columns.len();
    let mut columns = Vec::with_capacity(n_left + n_right);
    columns.extend(left_columns);
    columns.extend(right_columns);

    // Cooperative cancellation: build and probe both walk the full input, so
    // poll the deadline in each so a huge-input join can be timed out / freed.
    let mut cancel = CancelCheck::new();

    // Build: right_key -> list of right-row indices. Pre-size to the row
    // count so the map doesn't rehash mid-build.
    let mut build: FxHashMap<Value, Vec<usize>> =
        FxHashMap::with_capacity_and_hasher(right_rows.len(), Default::default());
    for (i, row) in right_rows.iter().enumerate() {
        cancel.tick()?;
        // PowQL equality is direct Value equality, including Empty = Empty.
        // Hash joins must preserve the same semantics as the nested-loop
        // evaluator rather than silently dropping nullable-key matches.
        build.entry(row[right_key_idx].clone()).or_default().push(i);
    }

    // Reasonable starting capacity — inner joins produce ≥ left_rows.len()
    // rows in the common 1:1 case, left-outer always emits ≥ left_rows.len().
    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(left_rows.len());

    crate::cancel::check()?;
    for left_row in &left_rows {
        cancel.tick()?;
        let key = &left_row[left_key_idx];
        let candidates = build.get(key);
        let mut matched = false;
        match candidates {
            Some(matches) if !matches.is_empty() => {
                for &ri in matches {
                    cancel.tick()?;
                    let right_row = &right_rows[ri];
                    let mut combined = Vec::with_capacity(n_left + n_right);
                    combined.extend_from_slice(left_row);
                    combined.extend_from_slice(right_row);
                    if residuals
                        .iter()
                        .all(|residual| eval_join_predicate(residual, &combined, &columns))
                    {
                        rows.push(combined);
                        check_join_limit(rows.len())?;
                        matched = true;
                    }
                }
            }
            _ => {}
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

#[inline]
pub(crate) fn check_nested_loop_pair_limit(
    left_rows: usize,
    right_rows: usize,
    pair_limit: usize,
) -> Result<usize, QueryError> {
    let candidate_pairs =
        left_rows
            .checked_mul(right_rows)
            .ok_or(QueryError::NestedLoopPairLimitExceeded {
                left_rows,
                right_rows,
                limit: pair_limit,
            })?;
    if candidate_pairs > pair_limit {
        return Err(QueryError::NestedLoopPairLimitExceeded {
            left_rows,
            right_rows,
            limit: pair_limit,
        });
    }
    Ok(candidate_pairs)
}

/// Execute a join over already materialized inputs. Runtime column resolution
/// decides whether a cross-side equality is usable as a hash key; otherwise the
/// checked and cancellation-aware nested loop remains the compatibility path.
pub(crate) fn execute_materialized_join(
    left_columns: Vec<String>,
    left_rows: Vec<Vec<Value>>,
    right_columns: Vec<String>,
    right_rows: Vec<Vec<Value>>,
    on: Option<&Expr>,
    kind: JoinKind,
    pair_limit: usize,
) -> Result<QueryResult, QueryError> {
    crate::cancel::check()?;
    if !matches!(kind, JoinKind::Cross) {
        if let Some(pred) = on {
            if let Some(spec) = try_extract_hash_join(pred, &left_columns, &right_columns) {
                return hash_join(
                    MaterializedJoinInputs {
                        left_columns,
                        left_rows,
                        right_columns,
                        right_rows,
                    },
                    spec.left_key_idx,
                    spec.right_key_idx,
                    kind,
                    &spec.residuals,
                );
            }
        }
    }

    check_nested_loop_pair_limit(left_rows.len(), right_rows.len(), pair_limit)?;
    let n_left = left_columns.len();
    let n_right = right_columns.len();
    let mut columns = Vec::with_capacity(n_left + n_right);
    columns.extend(left_columns);
    columns.extend(right_columns);

    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(left_rows.len());
    let mut combined: Vec<Value> = Vec::with_capacity(n_left + n_right);
    let mut cancel = CancelCheck::new();
    for left_row in &left_rows {
        let mut matched = false;
        for right_row in &right_rows {
            cancel.tick()?;
            combined.clear();
            combined.extend_from_slice(left_row);
            combined.extend_from_slice(right_row);
            let keep = match kind {
                JoinKind::Cross => true,
                JoinKind::Inner | JoinKind::LeftOuter => {
                    on.is_none_or(|pred| eval_join_predicate(pred, &combined, &columns))
                }
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

pub(super) fn execute_provenance_join(
    left: ProvenanceRows,
    right: ProvenanceRows,
    on: Option<&Expr>,
    kind: JoinKind,
    pair_limit: usize,
) -> Result<ProvenanceRows, QueryError> {
    let left_width = left.columns.len();
    let right_width = right.columns.len();
    let right_source_count = right.source_aliases.len();
    let mut columns = left.columns.clone();
    columns.extend(right.columns.clone());
    let mut source_aliases = left.source_aliases.clone();
    source_aliases.extend(right.source_aliases.clone());
    let mut rows = Vec::new();
    let mut provenance = Vec::new();
    let mut cancel = CancelCheck::new();

    if !matches!(kind, JoinKind::Cross) {
        if let Some(predicate) = on {
            if let Some(spec) = try_extract_hash_join(predicate, &left.columns, &right.columns) {
                let mut build: rustc_hash::FxHashMap<Value, Vec<usize>> =
                    rustc_hash::FxHashMap::default();
                for (index, row) in right.rows.iter().enumerate() {
                    cancel.tick()?;
                    let key = &row[spec.right_key_idx];
                    build.entry(key.clone()).or_default().push(index);
                }
                for (left_index, left_row) in left.rows.iter().enumerate() {
                    cancel.tick()?;
                    let key = &left_row[spec.left_key_idx];
                    let candidates = build.get(key);
                    let mut matched = false;
                    if let Some(candidates) = candidates {
                        for &right_index in candidates {
                            cancel.tick()?;
                            let mut row = Vec::with_capacity(left_width + right_width);
                            row.extend_from_slice(left_row);
                            row.extend_from_slice(&right.rows[right_index]);
                            if spec
                                .residuals
                                .iter()
                                .all(|residual| eval_join_predicate(residual, &row, &columns))
                            {
                                let mut row_provenance = left.provenance[left_index].clone();
                                row_provenance.extend_from_slice(&right.provenance[right_index]);
                                rows.push(row);
                                provenance.push(row_provenance);
                                check_join_limit(rows.len())?;
                                matched = true;
                            }
                        }
                    }
                    if !matched && matches!(kind, JoinKind::LeftOuter) {
                        let mut row = left_row.clone();
                        row.resize(left_width + right_width, Value::Empty);
                        let mut row_provenance = left.provenance[left_index].clone();
                        row_provenance.extend(std::iter::repeat_n(None, right_source_count));
                        rows.push(row);
                        provenance.push(row_provenance);
                        check_join_limit(rows.len())?;
                    }
                }
                return Ok(ProvenanceRows {
                    columns,
                    rows,
                    source_aliases,
                    provenance,
                });
            }
        }
    }

    check_nested_loop_pair_limit(left.rows.len(), right.rows.len(), pair_limit)?;
    for (left_index, left_row) in left.rows.iter().enumerate() {
        let mut matched = false;
        for (right_index, right_row) in right.rows.iter().enumerate() {
            cancel.tick()?;
            let mut row = Vec::with_capacity(left_width + right_width);
            row.extend_from_slice(left_row);
            row.extend_from_slice(right_row);
            let keep = match kind {
                JoinKind::Cross => true,
                JoinKind::Inner | JoinKind::LeftOuter => {
                    on.is_none_or(|predicate| eval_join_predicate(predicate, &row, &columns))
                }
                JoinKind::RightOuter => {
                    unreachable!("planner rewrites RightOuter to LeftOuter")
                }
            };
            if keep {
                let mut row_provenance = left.provenance[left_index].clone();
                row_provenance.extend_from_slice(&right.provenance[right_index]);
                rows.push(row);
                provenance.push(row_provenance);
                check_join_limit(rows.len())?;
                matched = true;
            }
        }
        if !matched && matches!(kind, JoinKind::LeftOuter) {
            let mut row = left_row.clone();
            row.resize(left_width + right_width, Value::Empty);
            let mut row_provenance = left.provenance[left_index].clone();
            row_provenance.extend(std::iter::repeat_n(None, right_source_count));
            rows.push(row);
            provenance.push(row_provenance);
            check_join_limit(rows.len())?;
        }
    }
    Ok(ProvenanceRows {
        columns,
        rows,
        source_aliases,
        provenance,
    })
}
