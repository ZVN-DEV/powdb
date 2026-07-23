//! Runtime plan lowering (unindexed scan fallbacks, conjunction index
//! choice) and EXPLAIN plan-tree formatting.

use crate::ast::*;
use crate::planner::{
    extract_single_bound, range_scan_for_target, try_extract_eq_index_key, RangeBound, RangeTarget,
};
use powdb_storage::btree::IndexStats;
use powdb_storage::catalog::Catalog;
use powdb_storage::types::*;
use std::collections::HashSet;

use crate::executor::eval::*;

use super::join::flatten_conjunctions;
use super::*;

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
/// Flatten a top-level `and` chain into its individual conjuncts. A predicate
/// that is not an `and` yields a single-element list.
fn flatten_and<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    match expr {
        Expr::BinaryOp(lhs, BinOp::And, rhs) => {
            flatten_and(lhs, out);
            flatten_and(rhs, out);
        }
        other => out.push(other),
    }
}

/// Selectivity tier of an equality index-scan candidate, or `None` when the
/// index does not resolve in the catalog. Lower is better:
/// 0 = unique-index equality, 1 = non-unique-index equality.
fn eq_candidate_tier(catalog: &Catalog, scan: &PlanNode) -> Option<u8> {
    match scan {
        PlanNode::IndexScan { table, column, .. } => match catalog.is_index_unique(table, column) {
            Some(true) => Some(0),
            Some(false) => Some(1),
            None => None,
        },
        PlanNode::ExprIndexScan { table, path, .. } => {
            resolve_expression_index(catalog, table, path).map(|meta| u8::from(!meta.unique))
        }
        _ => None,
    }
}

/// Whether a range candidate's index exists in the catalog.
fn range_candidate_resolves(catalog: &Catalog, scan: &PlanNode) -> bool {
    match scan {
        PlanNode::RangeScan { table, column, .. } => catalog.has_index(table, column),
        PlanNode::ExprRangeScan { table, path, .. } => {
            resolve_expression_index(catalog, table, path).is_some()
        }
        _ => false,
    }
}

/// Estimate returned when an index resolved for tiering but its stats did not
/// (should not happen once a candidate's tier resolved; kept defensive). It is
/// the maximum, so tier and build order decide, matching v0.14 behavior.
const UNKNOWN_EST: u64 = u64::MAX;

/// Fraction-of-total skew guard. An equality driver whose probe returns more
/// than `total_entries / HOT_DIVISOR` rows is not selective: one sequential pass
/// (a compiled `Filter(SeqScan)`) beats reading that many rows by random rid and
/// re-checking the residual per row. `2` (half the table) is deliberately
/// conservative -- it only rejects a driver that is provably worse than a full
/// scan, so a rare / selective equality is never pushed off its index. This
/// replaces the old skew-BLIND uniform average (`total_entries / distinct_keys`),
/// which estimated a hot Zipfian literal at the rare-key average and drove the
/// wrong conjunct; the guard counts the actual literal instead.
const HOT_DIVISOR: u64 = 2;

/// Rows above which an equality probe is treated as "hot" (not selective).
fn hot_threshold(total_entries: u64) -> u64 {
    total_entries / HOT_DIVISOR
}

/// Counting cap for a skew probe: one past the hot threshold, so a hot literal
/// saturates exactly when we can already conclude it is hot. Bounds each leaf
/// walk to `O(total_entries / HOT_DIVISOR)`.
fn probe_cap(total_entries: u64) -> usize {
    hot_threshold(total_entries).saturating_add(1) as usize
}

/// Skew-aware rows an equality probe of `key` returns against the plain-column
/// index `(table, column)`. Unique -> 1; the empty/missing sentinel -> its exact
/// side-list length; a concrete non-empty literal -> the EXACT index count
/// capped at `probe_cap` (a hot literal saturates at the cap). Falls back to the
/// uniform average only when `key` is not a countable literal (e.g. an
/// unsubstituted parameter), preserving prior behavior there. Single skew-aware
/// source shared by the conjunction chooser and the `explain` annotation so the
/// ranking and the printed value never disagree.
fn column_eq_est(catalog: &Catalog, table: &str, column: &str, key: &Expr, unique: bool) -> u64 {
    let Some(stats) = catalog.index_stats(table, column) else {
        return UNKNOWN_EST;
    };
    if unique {
        return 1;
    }
    match literal_to_value(key) {
        Ok(Value::Empty) => stats.empty_count,
        Ok(value) => catalog
            .index_key_count_capped(table, column, &value, probe_cap(stats.total_entries))
            .map_or_else(|| eq_est_rows(&stats, false, false), |count| count as u64),
        Err(_) => eq_est_rows(&stats, false, false),
    }
}

/// Whether a lone plain-column equality `column = key` is "hot": its literal
/// matches more than half the indexed rows, so a compiled sequential scan beats
/// the index scan. False for unique indexes, the empty/missing sentinel, a
/// non-literal key, or an unindexed / statless column -- all of which keep the
/// index unchanged. Bounded `O(threshold)` index walk.
fn hot_lone_equality(catalog: &Catalog, table: &str, column: &str, key: &Expr) -> bool {
    if catalog.is_index_unique(table, column) != Some(false) {
        return false; // unique, or column not resolvable as an index
    }
    if probes_empty_sentinel(key) {
        return false; // `= null` keeps its existing empty-list semantics
    }
    let Some(stats) = catalog.index_stats(table, column) else {
        return false;
    };
    let Ok(value) = literal_to_value(key) else {
        return false; // non-literal (e.g. parameter) probe: leave unchanged
    };
    match catalog.index_key_count_capped(table, column, &value, probe_cap(stats.total_entries)) {
        Some(count) => count as u64 > hot_threshold(stats.total_entries),
        None => false,
    }
}

/// Skew-aware equality estimate for an expression (JSON-path) index, mirroring
/// `column_eq_est`.
fn expr_eq_est(catalog: &Catalog, table: &str, index_id: u64, unique: bool, key: &Expr) -> u64 {
    let Some(stats) = catalog.expression_index_stats(table, index_id) else {
        return UNKNOWN_EST;
    };
    if unique {
        return 1;
    }
    match literal_to_value(key) {
        Ok(Value::Empty) => stats.empty_count,
        Ok(value) => catalog
            .expression_index_key_count_capped(
                table,
                index_id,
                &value,
                probe_cap(stats.total_entries),
            )
            .map_or_else(|| eq_est_rows(&stats, false, false), |count| count as u64),
        Err(_) => eq_est_rows(&stats, false, false),
    }
}

/// Whether an index probe literal targets the empty / missing / JSON-null
/// sentinel (`Value::Empty`), whose rows live in the tree's separate empty list.
fn probes_empty_sentinel(key: &Expr) -> bool {
    matches!(literal_to_value(key), Ok(Value::Empty))
}

/// Estimated rows an equality probe against `stats` returns. A unique index
/// returns at most one row; a non-unique probe of the empty/missing sentinel
/// returns the empty-list length; any other non-unique probe returns the average
/// rows per key. O(1) over the already-loaded counters. Single source of the
/// `est_rows` formula, shared by the conjunction chooser and both `explain`
/// index-scan annotations so the ranking and the printed value never disagree.
fn eq_est_rows(stats: &IndexStats, unique: bool, empty_probe: bool) -> u64 {
    if unique {
        1
    } else if empty_probe {
        stats.empty_count
    } else {
        stats.total_entries / stats.distinct_keys.max(1)
    }
}

/// Estimated rows an equality candidate's index probe returns, used to rank
/// conjunction drivers by selectivity. `tier == 0` marks a unique index (the
/// uniqueness source shared with `explain`). Skew-aware: a non-unique probe
/// counts the actual literal (capped) instead of the old uniform average.
fn eq_candidate_est(catalog: &Catalog, scan: &PlanNode, tier: u8) -> u64 {
    match scan {
        PlanNode::IndexScan { table, column, key } => {
            column_eq_est(catalog, table, column, key, tier == 0)
        }
        PlanNode::ExprIndexScan { table, path, key } => {
            match resolve_expression_index(catalog, table, path) {
                Some(meta) => expr_eq_est(catalog, table, meta.index_id, meta.unique, key),
                None => UNKNOWN_EST,
            }
        }
        _ => UNKNOWN_EST,
    }
}

/// Estimated rows a range candidate scans: its index's total entries (range
/// selectivity estimation is out of scope). Any equality candidate,
/// whose estimate is reduced by distinct keys, therefore ranks ahead, which
/// preserves the v0.14 tier ordering.
fn range_candidate_est(catalog: &Catalog, scan: &PlanNode) -> u64 {
    let stats = match scan {
        PlanNode::RangeScan { table, column, .. } => catalog.index_stats(table, column),
        PlanNode::ExprRangeScan { table, path, .. } => {
            resolve_expression_index(catalog, table, path)
                .and_then(|meta| catalog.expression_index_stats(table, meta.index_id))
        }
        _ => None,
    };
    stats.map_or(UNKNOWN_EST, |stats| stats.total_entries)
}

/// Declared type of `column` in `table`, if both resolve.
fn column_type(catalog: &Catalog, table: &str, column: &str) -> Option<TypeId> {
    catalog
        .schema(table)?
        .find_column(column)
        .map(|col| col.type_id)
}

/// Rewrite a plain-column index-key literal into the value the index actually
/// stores for `col_type`, or return `None` when no rewrite makes the indexed
/// lookup equivalent to the reference `Filter(SeqScan)`.
///
/// The reference scan compiles `.col <op> literal` per the column's declared
/// type: a float column promotes an int literal to `f64` (so `.f = 1` matches a
/// stored `1.0`), while a non-float column never matches a float literal under
/// the strict `Value` equality the eval fallback uses. A plain-column B-tree
/// stores keys under the column's type behind a type tag, so a raw `Int(1)` key
/// would miss every `Float(1.0)` row. Coercing the literal here keeps the
/// index-driven path exactly in step with the scan; anything we cannot rewrite
/// without changing the result set is rejected so the caller falls back to the
/// always-correct scan.
fn coerce_column_index_key(col_type: TypeId, key: &Expr) -> Option<Expr> {
    match (key, col_type) {
        // Same-typed literal: the index key already matches the stored key.
        // A datetime column stores an int-literal timestamp as a raw `Int`
        // (see `coerce_value`), so an int key is correct there too.
        (Expr::Literal(Literal::Int(_)), TypeId::Int | TypeId::DateTime) => Some(key.clone()),
        (Expr::Literal(Literal::Float(_)), TypeId::Float) => Some(key.clone()),
        (Expr::Literal(Literal::String(_)), TypeId::Str) => Some(key.clone()),
        (Expr::Literal(Literal::Bool(_)), TypeId::Bool) => Some(key.clone()),
        // Int literal into a float column: widen to `f64`, exactly as the
        // compiled float leaf does, so the float-typed index key matches.
        (Expr::Literal(Literal::Int(v)), TypeId::Float) => {
            Some(Expr::Literal(Literal::Float(*v as f64)))
        }
        // Any other pairing either never matches under the reference semantics
        // or would need a lossy coercion that changes which rows match, so reject.
        _ => None,
    }
}

/// Coerce one optional range bound to `col_type`. The outer `Option` is the
/// keep/reject signal for the whole candidate; the inner `Option` preserves
/// "no bound on this side".
fn coerce_column_index_bound(
    col_type: TypeId,
    bound: Option<(Expr, bool)>,
) -> Option<Option<(Expr, bool)>> {
    match bound {
        None => Some(None),
        Some((expr, inclusive)) => {
            coerce_column_index_key(col_type, &expr).map(|expr| Some((expr, inclusive)))
        }
    }
}

/// Coerce the literal key(s) of a freshly-extracted candidate scan to the
/// driving column's declared type, or return `None` to drop the candidate (the
/// caller then keeps the correct `Filter(SeqScan)`). Expression-index
/// (json-path) candidates pass through unchanged: they look scalars up by raw
/// `Value` (`BTree::lookup_all` / `raw_range_rids`), so they already agree with
/// the sequential scan and need no type-tag coercion.
fn coerce_candidate_keys(catalog: &Catalog, scan: PlanNode) -> Option<PlanNode> {
    match scan {
        PlanNode::IndexScan { table, column, key } => {
            let col_type = column_type(catalog, &table, &column)?;
            let key = coerce_column_index_key(col_type, &key)?;
            Some(PlanNode::IndexScan { table, column, key })
        }
        PlanNode::RangeScan {
            table,
            column,
            start,
            end,
        } => {
            let col_type = column_type(catalog, &table, &column)?;
            let start = coerce_column_index_bound(col_type, start)?;
            let end = coerce_column_index_bound(col_type, end)?;
            Some(PlanNode::RangeScan {
                table,
                column,
                start,
                end,
            })
        }
        other => Some(other),
    }
}

/// A conjunct chosen to drive an indexed scan, plus the conjunct indices it
/// consumes (the rest become the residual Filter).
struct ConjunctionCandidate {
    plan: PlanNode,
    consumed: Vec<usize>,
    /// Estimated rows the driving probe returns (lower is more selective).
    est: u64,
    tier: u8,
}

/// Lane A: rewrite a `Filter(SeqScan)` whose predicate is a top-level `and`
/// chain into `Filter(residual)(index scan)` driven by the most selective
/// indexed conjunct. Returns `None` when the predicate is not a conjunction or
/// no conjunct resolves to an existing index, so the caller keeps today's
/// `Filter(SeqScan)` byte-identical.
///
/// Selection ranks candidates by `(tier, estimated rows, build order)`: a
/// unique equality estimates 1, a non-unique equality estimates the EXACT count
/// of its literal (capped via a bounded `O(threshold)` index walk, so a hot
/// Zipfian value no longer hides behind the uniform average), and a range
/// estimates its index's full size so an equality still wins. Ranking is
/// tier-first (equality before range, unique before non-unique) then estimate
/// then conjunct order. A wrong pick is only ever slower, never wrong: the
/// residual re-checks the full conjunction on each fetched row.
fn lower_conjunction_scan(catalog: &Catalog, table: &str, predicate: &Expr) -> Option<PlanNode> {
    let mut conjuncts: Vec<&Expr> = Vec::new();
    flatten_and(predicate, &mut conjuncts);
    if conjuncts.len() < 2 {
        return None;
    }

    let mut candidates: Vec<ConjunctionCandidate> = Vec::new();

    // Equality candidates, in conjunct order so ties resolve to the first.
    for (i, conjunct) in conjuncts.iter().enumerate() {
        if let Some(scan) = try_extract_eq_index_key(table, conjunct) {
            // Coerce the driving literal to the column's type before probing
            // the index (a raw int key would miss a float-typed index); an
            // uncoercible key drops the candidate to the correct scan.
            if let Some(scan) = coerce_candidate_keys(catalog, scan) {
                if let Some(tier) = eq_candidate_tier(catalog, &scan) {
                    let est = eq_candidate_est(catalog, &scan, tier);
                    candidates.push(ConjunctionCandidate {
                        plan: scan,
                        consumed: vec![i],
                        est,
                        tier,
                    });
                }
            }
        }
    }

    // Range candidates: merge same-column bounds into one BETWEEN scan. Only
    // the first lower and first upper bound on a target are folded in; any
    // extra bound on that target stays a residual conjunct so the recheck
    // preserves exact semantics.
    let bounds: Vec<(usize, RangeBound)> = conjuncts
        .iter()
        .enumerate()
        .filter_map(|(i, conjunct)| extract_single_bound(conjunct).map(|bound| (i, bound)))
        .collect();
    let mut seen_targets: Vec<RangeTarget> = Vec::new();
    for (_, (target, _, _)) in &bounds {
        if !seen_targets.contains(target) {
            seen_targets.push(target.clone());
        }
    }
    for target in seen_targets {
        let mut lower: Option<(Expr, bool)> = None;
        let mut lower_idx: Option<usize> = None;
        let mut upper: Option<(Expr, bool)> = None;
        let mut upper_idx: Option<usize> = None;
        for (i, (candidate_target, start, end)) in &bounds {
            if *candidate_target != target {
                continue;
            }
            if lower.is_none() {
                if let Some(bound) = start.clone() {
                    lower = Some(bound);
                    lower_idx = Some(*i);
                }
            }
            if upper.is_none() {
                if let Some(bound) = end.clone() {
                    upper = Some(bound);
                    upper_idx = Some(*i);
                }
            }
        }
        if lower.is_none() && upper.is_none() {
            continue;
        }
        let scan = range_scan_for_target(table, target, lower, upper);
        // Coerce int bounds to a float column's type (a raw int bound would
        // miss the float-typed range index); an uncoercible bound drops the
        // candidate to the correct scan.
        let Some(scan) = coerce_candidate_keys(catalog, scan) else {
            continue;
        };
        if !range_candidate_resolves(catalog, &scan) {
            continue;
        }
        let mut consumed: Vec<usize> = Vec::new();
        if let Some(i) = lower_idx {
            consumed.push(i);
        }
        if let Some(i) = upper_idx {
            if !consumed.contains(&i) {
                consumed.push(i);
            }
        }
        let est = range_candidate_est(catalog, &scan);
        candidates.push(ConjunctionCandidate {
            plan: scan,
            consumed,
            est,
            tier: 2,
        });
    }

    // Rank by (tier, estimated rows, build order): a unique equality (tier 0)
    // beats any non-unique probe, a non-unique equality (tier 1) beats a range
    // (tier 2), and within a tier the lower skew-aware estimate wins. Tier leads
    // so that a non-unique literal that happens to match zero rows never
    // leapfrogs a guaranteed-<=1-row unique index. `min_by_key` keeps the first
    // element on a full tie (earliest-built: equalities in conjunct order, then
    // ranges). A wrong pick is only ever slower, never wrong: the residual
    // re-checks the full conjunction on each fetched row.
    let winner = candidates
        .into_iter()
        .enumerate()
        .min_by_key(|(build_order, candidate)| (candidate.tier, candidate.est, *build_order))?
        .1;

    let mut residual: Vec<Expr> = Vec::new();
    for (i, conjunct) in conjuncts.iter().enumerate() {
        if !winner.consumed.contains(&i) {
            residual.push((*conjunct).clone());
        }
    }
    if residual.is_empty() {
        return Some(winner.plan);
    }
    let residual_expr = residual
        .into_iter()
        .reduce(|acc, next| Expr::BinaryOp(Box::new(acc), BinOp::And, Box::new(next)))
        .expect("residual is non-empty");
    Some(PlanNode::Filter {
        input: Box::new(winner.plan),
        predicate: residual_expr,
    })
}

/// This pass runs once per query, before execution.
pub(crate) fn lower_unindexed_scans(catalog: &Catalog, plan: &PlanNode) -> PlanNode {
    match plan {
        PlanNode::ExprIndexScan { table, path, .. }
        | PlanNode::ExprRangeScan { table, path, .. }
        | PlanNode::OrderedExprIndexScan { table, path, .. } => {
            if resolve_expression_index(catalog, table, path).is_some() {
                plan.clone()
            } else {
                expression_index_fallback(plan)
                    .expect("expression-index branch always has a fallback")
            }
        }
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
        PlanNode::Filter { input, predicate } => {
            // Lane A: a `Filter(SeqScan)` whose predicate is a top-level `and`
            // chain can be driven by an indexed conjunct, re-checking the rest
            // as a residual. The planner emits this shape because it is pure;
            // lowering makes the choice with real catalog knowledge.
            if let PlanNode::SeqScan { table } = input.as_ref() {
                if let Some(lowered) = lower_conjunction_scan(catalog, table, predicate) {
                    return lowered;
                }
            }
            PlanNode::Filter {
                input: Box::new(lower_unindexed_scans(catalog, input)),
                predicate: predicate.clone(),
            }
        }
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
            argument,
            mode,
            provenance_alias,
        } => PlanNode::Aggregate {
            input: Box::new(lower_unindexed_scans(catalog, input)),
            function: *function,
            argument: argument.clone(),
            mode: *mode,
            provenance_alias: provenance_alias.clone(),
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
                    // Skew guard: a lone equality on a HOT literal (one that
                    // matches more than half the table) runs faster as a
                    // compiled `Filter(SeqScan)` -- one sequential pass with the
                    // compiled predicate -- than as an index scan that reads most
                    // rows by random rid. Rare / selective literals (<= half) keep
                    // the index. Unique indexes (<=1 row) and the empty/missing
                    // sentinel (`= null`, its own side list) are never hot, so
                    // they are left exactly as before.
                    if !hot_lone_equality(catalog, table, column, key) {
                        return plan.clone();
                    }
                    return PlanNode::Filter {
                        input: Box::new(PlanNode::SeqScan {
                            table: table.clone(),
                        }),
                        predicate: Expr::BinaryOp(
                            Box::new(Expr::Field(column.clone())),
                            BinOp::Eq,
                            Box::new(key.clone()),
                        ),
                    };
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

pub(super) fn stored_json_path_expr(
    path: &powdb_storage::stored_json_path::StoredJsonPathV1,
) -> Expr {
    use powdb_storage::stored_json_path::StoredJsonPathSegmentV1;

    Expr::JsonPath {
        base: Box::new(Expr::Field(path.column.clone())),
        segments: path
            .segments
            .iter()
            .map(|segment| match segment {
                StoredJsonPathSegmentV1::Key(key) => PathSeg::Key(key.clone()),
                StoredJsonPathSegmentV1::Index(index) => PathSeg::Index(*index),
            })
            .collect(),
    }
}

pub(super) fn synthesize_expr_range_predicate(
    path: &powdb_storage::stored_json_path::StoredJsonPathV1,
    start: &Option<(Expr, bool)>,
    end: &Option<(Expr, bool)>,
) -> Expr {
    let lower = start.as_ref().map(|(expr, inclusive)| {
        Expr::BinaryOp(
            Box::new(stored_json_path_expr(path)),
            if *inclusive { BinOp::Gte } else { BinOp::Gt },
            Box::new(expr.clone()),
        )
    });
    let upper = end.as_ref().map(|(expr, inclusive)| {
        Expr::BinaryOp(
            Box::new(stored_json_path_expr(path)),
            if *inclusive { BinOp::Lte } else { BinOp::Lt },
            Box::new(expr.clone()),
        )
    });
    match (lower, upper) {
        (Some(lower), Some(upper)) => Expr::BinaryOp(Box::new(lower), BinOp::And, Box::new(upper)),
        (Some(lower), None) => lower,
        (None, Some(upper)) => upper,
        (None, None) => Expr::Literal(Literal::Bool(true)),
    }
}

/// Synthesize a range predicate from RangeScan bounds for the fallback path.
pub(crate) fn synthesize_range_predicate(
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
/// The table a single index-scan node reads, if it is one of the index-scan
/// shapes. Used to confirm a lowered discovery scan targets the mutation's own
/// table before its rids are reused.
pub(super) fn scan_table(scan: &PlanNode) -> Option<&str> {
    match scan {
        PlanNode::IndexScan { table, .. }
        | PlanNode::RangeScan { table, .. }
        | PlanNode::ExprIndexScan { table, .. }
        | PlanNode::ExprRangeScan { table, .. } => Some(table),
        _ => None,
    }
}

pub(crate) fn range_matches(
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

fn collect_plan_qualifiers(plan: &PlanNode, qualifiers: &mut HashSet<String>) {
    match plan {
        PlanNode::SeqScan { table }
        | PlanNode::IndexScan { table, .. }
        | PlanNode::RangeScan { table, .. }
        | PlanNode::ExprIndexScan { table, .. }
        | PlanNode::ExprRangeScan { table, .. }
        | PlanNode::OrderedExprIndexScan { table, .. } => {
            qualifiers.insert(table.clone());
        }
        PlanNode::AliasScan { alias, .. } => {
            qualifiers.insert(alias.clone());
        }
        PlanNode::Filter { input, .. }
        | PlanNode::Project { input, .. }
        | PlanNode::Sort { input, .. }
        | PlanNode::Limit { input, .. }
        | PlanNode::Offset { input, .. }
        | PlanNode::Aggregate { input, .. }
        | PlanNode::Distinct { input }
        | PlanNode::GroupBy { input, .. }
        | PlanNode::Update { input, .. }
        | PlanNode::Delete { input, .. }
        | PlanNode::Window { input, .. }
        | PlanNode::Explain { input } => collect_plan_qualifiers(input, qualifiers),
        PlanNode::NestedLoopJoin { left, right, .. } | PlanNode::Union { left, right, .. } => {
            collect_plan_qualifiers(left, qualifiers);
            collect_plan_qualifiers(right, qualifiers);
        }
        _ => {}
    }
}

fn qualified_ref(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::QualifiedField { qualifier, .. } => Some(qualifier),
        _ => None,
    }
}

fn explain_join_strategy(
    left: &PlanNode,
    right: &PlanNode,
    on: Option<&Expr>,
    kind: JoinKind,
) -> &'static str {
    if matches!(kind, JoinKind::Cross) {
        return "nested-loop-bounded";
    }
    let Some(predicate) = on else {
        return "nested-loop-bounded";
    };
    let mut conjunctions = Vec::new();
    flatten_conjunctions(predicate, &mut conjunctions);
    let mut left_qualifiers = HashSet::new();
    let mut right_qualifiers = HashSet::new();
    collect_plan_qualifiers(left, &mut left_qualifiers);
    collect_plan_qualifiers(right, &mut right_qualifiers);

    let has_cross_side_equi = conjunctions.iter().any(|expr| {
        let Expr::BinaryOp(lhs, BinOp::Eq, rhs) = expr else {
            return false;
        };
        let (Some(lhs_q), Some(rhs_q)) = (qualified_ref(lhs), qualified_ref(rhs)) else {
            return false;
        };
        (left_qualifiers.contains(lhs_q) && right_qualifiers.contains(rhs_q))
            || (left_qualifiers.contains(rhs_q) && right_qualifiers.contains(lhs_q))
    });
    if has_cross_side_equi {
        if conjunctions.len() > 1 {
            "hash+residual"
        } else {
            "hash"
        }
    } else {
        "nested-loop-bounded"
    }
}

/// Format a `PlanNode` tree as a human-readable, indented text
/// representation. Used by the `EXPLAIN` command.
/// Append one nested projection's EXPLAIN line (and, recursively, its
/// deeper levels) to `out`, indented under the `NestedProject` node.
fn format_nested_projection(nested: &NestedProjection, depth: usize, out: &mut String) {
    use std::fmt::Write;
    let indent = "  ".repeat(depth);
    // A block link traversal has placeholder correlation columns until the
    // executor resolves the link from the catalog (the planner never touches
    // the catalog), so show what IS known at plan time: the declared path.
    if let Some(via) = &nested.via_link {
        let _ = writeln!(
            out,
            "{indent}nested {}: to-many link {}.{} (child table + correlation \
             resolved from catalog at execution)",
            nested.name, via.outer_alias, via.link_name
        );
        for field in &nested.fields {
            if let NestedField::Nested(inner) = field {
                format_nested_projection(inner, depth + 1, out);
            }
        }
        return;
    }
    let parent = if nested.parent_key.contains('.') {
        nested.parent_key.clone()
    } else {
        format!("{}.{}", nested.parent_alias, nested.parent_key)
    };
    let _ = write!(
        out,
        "{indent}nested {}: {} as {} on {}.{} = {}",
        nested.name, nested.table, nested.alias, nested.alias, nested.child_key, parent
    );
    if let Some(residual) = &nested.residual {
        let _ = write!(out, " residual={residual:?}");
    }
    if !nested.order.is_empty() {
        let keys: Vec<String> = nested
            .order
            .iter()
            .map(|(column, descending)| {
                format!("{column} {}", if *descending { "desc" } else { "asc" })
            })
            .collect();
        let _ = write!(out, " order [{}]", keys.join(", "));
    }
    let bound = |expr: &Expr| match expr {
        Expr::Literal(crate::ast::Literal::Int(v)) => v.to_string(),
        other => format!("{other:?}"),
    };
    if let Some(limit) = &nested.limit {
        let _ = write!(out, " limit {}", bound(limit));
    }
    if let Some(offset) = &nested.offset {
        let _ = write!(out, " offset {}", bound(offset));
    }
    out.push('\n');
    for field in &nested.fields {
        if let NestedField::Nested(inner) = field {
            format_nested_projection(inner, depth + 1, out);
        }
    }
}

pub(crate) fn format_plan_tree(catalog: &Catalog, plan: &PlanNode, depth: usize) -> String {
    let indent = "  ".repeat(depth);
    match plan {
        PlanNode::SeqScan { table } => format!("{indent}SeqScan table={table}"),
        PlanNode::AliasScan { table, alias } => {
            format!("{indent}AliasScan table={table} alias={alias}")
        }
        PlanNode::IndexScan { table, column, key } => {
            let base = format!("{indent}IndexScan table={table} column={column} key={key:?}");
            match catalog.index_stats(table, column) {
                Some(stats) => {
                    let unique = catalog.is_index_unique(table, column) == Some(true);
                    let est = column_eq_est(catalog, table, column, key, unique);
                    format!(
                        "{base} est_rows={est} entries={} distinct={}",
                        stats.total_entries, stats.distinct_keys
                    )
                }
                None => base,
            }
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
        PlanNode::ExprIndexScan { table, path, key } => {
            let meta = resolve_expression_index(catalog, table, path);
            let index_id = meta
                .as_ref()
                .map(|metadata| metadata.index_id.to_string())
                .unwrap_or_else(|| "unresolved".to_string());
            let base = format!(
                "{indent}ExprIndexScan table={table} path={} index_id={index_id} key={key:?}",
                path.canonical_text()
            );
            match meta.and_then(|m| {
                catalog
                    .expression_index_stats(table, m.index_id)
                    .map(|stats| (m.index_id, m.unique, stats))
            }) {
                Some((index_id, unique, stats)) => {
                    let est = expr_eq_est(catalog, table, index_id, unique, key);
                    format!(
                        "{base} est_rows={est} entries={} distinct={}",
                        stats.total_entries, stats.distinct_keys
                    )
                }
                None => base,
            }
        }
        PlanNode::ExprRangeScan {
            table,
            path,
            start,
            end,
        } => {
            let index_id = resolve_expression_index(catalog, table, path)
                .map(|metadata| metadata.index_id.to_string())
                .unwrap_or_else(|| "unresolved".to_string());
            format!(
                "{indent}ExprRangeScan table={table} path={} index_id={index_id} start={start:?} end={end:?}",
                path.canonical_text()
            )
        }
        PlanNode::OrderedExprIndexScan {
            table,
            path,
            descending,
            limit,
            offset,
        } => {
            let index_id = resolve_expression_index(catalog, table, path)
                .map(|metadata| metadata.index_id.to_string())
                .unwrap_or_else(|| "unresolved".to_string());
            format!(
                "{indent}OrderedExprIndexScan table={table} path={} index_id={index_id} descending={descending} limit={limit:?} offset={offset:?}",
                path.canonical_text()
            )
        }
        PlanNode::Filter { input, predicate } => {
            let child = format_plan_tree(catalog, input, depth + 1);
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
            let child = format_plan_tree(catalog, input, depth + 1);
            format!("{indent}Project fields=[{}]\n{child}", names.join(", "))
        }
        PlanNode::NestedProject { input, fields } => {
            let names: Vec<String> = fields
                .iter()
                .map(|f| match f {
                    NestedProjectField::Plain(field) => match &field.alias {
                        Some(a) => format!("{a}: {:?}", field.expr),
                        None => format!("{:?}", field.expr),
                    },
                    NestedProjectField::Nested(nested) => nested.name.clone(),
                    NestedProjectField::Link(link) => link.name.clone(),
                })
                .collect();
            let mut out = format!("{indent}NestedProject fields=[{}]\n", names.join(", "));
            for f in fields {
                match f {
                    NestedProjectField::Nested(nested) => {
                        format_nested_projection(nested, depth + 1, &mut out);
                    }
                    NestedProjectField::Link(link) => {
                        // Links resolve at execution time (the planner never
                        // touches the catalog), so EXPLAIN shows the declared
                        // hop path rather than faking a resolved target.
                        let pad = "  ".repeat(depth + 1);
                        out.push_str(&format!(
                            "{pad}link {}: scalar to-one path {}.{}.{} \
                             (hops [{}] -> column {}; targets resolved from \
                             catalog at execution)\n",
                            link.name,
                            link.outer_alias,
                            link.links.join("."),
                            link.column,
                            link.links.join(", "),
                            link.column
                        ));
                    }
                    NestedProjectField::Plain(_) => {}
                }
            }
            out.push_str(&format_plan_tree(catalog, input, depth + 1));
            out
        }
        PlanNode::Sort { input, keys } => {
            let ks: Vec<String> = keys
                .iter()
                .map(|k| {
                    let expr = expression_output_name(&k.expr);
                    if k.descending {
                        format!("{expr} desc")
                    } else {
                        expr
                    }
                })
                .collect();
            let child = format_plan_tree(catalog, input, depth + 1);
            format!("{indent}Sort keys=[{}]\n{child}", ks.join(", "))
        }
        PlanNode::Limit { input, count } => {
            let child = format_plan_tree(catalog, input, depth + 1);
            format!("{indent}Limit count={count:?}\n{child}")
        }
        PlanNode::Offset { input, count } => {
            let child = format_plan_tree(catalog, input, depth + 1);
            format!("{indent}Offset count={count:?}\n{child}")
        }
        PlanNode::Aggregate {
            input,
            function,
            argument,
            mode,
            provenance_alias: _,
        } => {
            let argument = argument
                .as_ref()
                .map(expression_output_name)
                .unwrap_or_else(|| "*".to_string());
            let child = format_plan_tree(catalog, input, depth + 1);
            format!("{indent}Aggregate fn={function:?} mode={mode:?} argument={argument}\n{child}")
        }
        PlanNode::NestedLoopJoin {
            left,
            right,
            on,
            kind,
        } => {
            let left_child = format_plan_tree(catalog, left, depth + 1);
            let right_child = format_plan_tree(catalog, right, depth + 1);
            let on_str = match on {
                Some(pred) => format!("{pred:?}"),
                None => "none".to_string(),
            };
            let strategy = explain_join_strategy(left, right, on.as_ref(), *kind);
            format!(
                "{indent}NestedLoopJoin kind={kind:?} strategy={strategy} on={on_str}\n{left_child}\n{right_child}"
            )
        }
        PlanNode::Distinct { input } => {
            let child = format_plan_tree(catalog, input, depth + 1);
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
                .map(|a| {
                    format!(
                        "{:?}({}) mode={:?} as {}",
                        a.function,
                        expression_output_name(&a.argument),
                        a.mode,
                        a.output_name
                    )
                })
                .collect();
            let having_str = match having {
                Some(h) => format!(" having={h:?}"),
                None => String::new(),
            };
            let key_strs: Vec<String> = keys.iter().map(|k| k.output_name()).collect();
            let child = format_plan_tree(catalog, input, depth + 1);
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
            let child = format_plan_tree(catalog, input, depth + 1);
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
            let child = format_plan_tree(catalog, input, depth + 1);
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
        PlanNode::CreateLink {
            owner,
            name,
            target,
            local_key,
            target_key,
        } => {
            format!("{indent}CreateLink {owner}.{name} -> {target} on {local_key} = {target_key}")
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
            let child = format_plan_tree(catalog, input, depth + 1);
            format!("{indent}Window fns=[{}]\n{child}", ws.join(", "))
        }
        PlanNode::Union { left, right, all } => {
            let kind = if *all { "UNION ALL" } else { "UNION" };
            let left_child = format_plan_tree(catalog, left, depth + 1);
            let right_child = format_plan_tree(catalog, right, depth + 1);
            format!("{indent}{kind}\n{left_child}\n{right_child}")
        }
        PlanNode::Explain { input } => {
            let child = format_plan_tree(catalog, input, depth + 1);
            format!("{indent}Explain\n{child}")
        }
        PlanNode::Begin => format!("{indent}Begin"),
        PlanNode::Commit => format!("{indent}Commit"),
        PlanNode::Rollback => format!("{indent}Rollback"),
    }
}
