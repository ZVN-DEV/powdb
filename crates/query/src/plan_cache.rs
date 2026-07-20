//! Plan cache — Mission D9.
//!
//! Two queries that differ only in literal values share the same parsed +
//! planned tree. Re-running the lexer + parser + planner on every call is
//! pure overhead — easily 1-3μs per query, which is the entire budget on
//! sub-microsecond workloads like `update_by_pk`. SQLite gets around this
//! with `prepare_cached`; we get around it with this cache.
//!
//! ## How it works
//!
//! 1. [`crate::canonicalize::canonicalize`] lexes the input and produces
//!    `(canonical_hash, literals)`. The hash collapses literal *values*
//!    into placeholders, so `User filter .id = 1` and `User filter .id = 2`
//!    have the same hash.
//! 2. On the first call, [`PlanCache::insert`] stores the planned tree
//!    keyed by the canonical hash. The plan still has the *first call's*
//!    literal values baked into its `Expr::Literal` nodes — that's fine,
//!    we'll overwrite them on subsequent hits.
//! 3. On a subsequent call, [`PlanCache::get_with_substitution`] clones
//!    the cached plan and walks it depth-first, replacing each
//!    `Expr::Literal` it finds (in source order) with the corresponding
//!    literal from the new query.
//!
//! The walk order is **deterministic and matches the source order of the
//! literal list collected by `canonicalize`** — see the per-PlanNode
//! comments below for the exact traversal contract.

use crate::ast::{Assignment, Expr, Literal};
use crate::plan::PlanNode;
use rustc_hash::FxHashMap;

/// LRU-ish plan cache keyed by canonical query hash.
///
/// Mission F: uses FxHashMap. The keys are u64 hashes (already pre-hashed
/// by `canonicalize`), so SipHash is pure overhead — Fx is much cheaper for
/// the integer-keyed lookup.
pub struct PlanCache {
    cache: FxHashMap<u64, PlanNode>,
    capacity: usize,
    pub hits: u64,
    pub misses: u64,
}

impl PlanCache {
    pub fn new(capacity: usize) -> Self {
        PlanCache {
            cache: FxHashMap::default(),
            capacity,
            hits: 0,
            misses: 0,
        }
    }

    /// Store a planned query under its canonical hash. The plan can have
    /// any literal values inside it — those will be overwritten on hit.
    ///
    /// `source_literal_count` is the number of literals
    /// [`crate::canonicalize::canonicalize`] collected from the query text.
    /// The cache only works because, on a hit, every collected literal maps
    /// 1:1 to a substitutable slot the `substitute_plan` walk can reach.
    /// If those counts disagree, the plan has literals the walk cannot
    /// re-bind — the one case today is a subquery (`in (<subquery>)` /
    /// `exists (...)`), whose inner literals `canonicalize` collects at the
    /// token level but which live in an un-walked `QueryExpr` AST inside the
    /// predicate. Caching such a plan would serve the *first* call's inner
    /// literal to every later same-shape call: silent wrong rows in release,
    /// a substitution-count assert in debug (issue #137). We refuse to cache
    /// it; the engine then plans from source on every call, which is always
    /// correct. This check runs only on the populating miss, so the hot
    /// hit-path pays nothing.
    pub fn insert(&mut self, hash: u64, plan: PlanNode, source_literal_count: usize) {
        // GROUP BY planning lifts aggregate arguments out of their source
        // projection positions, and the parser accepts HAVING both before and
        // after that projection. The physical plan does not retain which
        // clause order the source used, so a literal-slot walk cannot safely
        // reconstruct source order for grouped HAVING queries. Refuse to
        // cache that shape until plans carry explicit source-slot ordinals.
        // Replanning is cheaper than ever serving silently rebound literals.
        if contains_grouped_having(&plan) {
            return;
        }
        // Nested-projection plans are cacheable: their residual/limit/offset
        // literal slots are walked in source order by
        // `substitute_nested_projection`. The one form that walk cannot
        // rebind is a nested block that wrote `offset` before `limit`
        // (source literal order [offset, limit], walk order [limit, offset]);
        // refuse it and replan from source on every call instead.
        if nested_projection_defeats_cache(&plan) {
            return;
        }
        if count_literal_slots(&plan) != source_literal_count {
            return;
        }
        if self.cache.len() >= self.capacity && !self.cache.contains_key(&hash) {
            // Crude eviction: when full, drop everything. Plan cache is
            // small (capacity ~256) and bench loops only ever fill a
            // handful of slots, so this is acceptable for now. A real LRU
            // would matter once we have hundreds of distinct query shapes.
            self.cache.clear();
        }
        self.cache.insert(hash, plan);
    }

    /// Look up a plan by canonical hash and return a clone with the new
    /// literals substituted into every `Expr::Literal` slot in source
    /// order.
    ///
    /// Returns `Some(plan)` on a hit and bumps `self.hits`. Returns `None`
    /// on a miss and bumps `self.misses`. Returning `None` instead of
    /// reaching for the planner here keeps this module dependency-free
    /// from `planner` — the engine handles the miss path.
    ///
    /// The substitution is done on a **clone** of the cached plan, not the
    /// stored copy. The cached plan stays pristine for the next call.
    pub fn get_with_substitution(&mut self, hash: u64, literals: &[Literal]) -> Option<PlanNode> {
        match self.cache.get(&hash) {
            Some(template) => {
                self.hits += 1;
                let mut plan = template.clone();
                let mut idx = 0usize;
                substitute_plan(&mut plan, literals, &mut idx);
                debug_assert_eq!(
                    idx,
                    literals.len(),
                    "plan substitution consumed {idx} literals but query had {}",
                    literals.len(),
                );
                Some(plan)
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    pub fn clear(&mut self) {
        self.cache.clear();
    }
}

/// True when a nested projection anywhere in the plan wrote `offset` before
/// `limit` in its source block. The literal-slot walk visits limit before
/// offset, so that form's source literal order cannot be safely rebound;
/// both the cache (see `insert`) and `Engine::prepare` refuse it.
pub(crate) fn nested_projection_defeats_cache(plan: &PlanNode) -> bool {
    fn nested_defeats(nested: &crate::plan::NestedProjection) -> bool {
        nested.offset_before_limit
            || nested.fields.iter().any(|field| match field {
                crate::plan::NestedField::Nested(inner) => nested_defeats(inner),
                crate::plan::NestedField::Scalar { .. } => false,
            })
    }
    fn walk(plan: &PlanNode) -> bool {
        match plan {
            PlanNode::NestedProject { input, fields } => {
                walk(input)
                    || fields.iter().any(|field| match field {
                        crate::plan::NestedProjectField::Nested(nested) => nested_defeats(nested),
                        crate::plan::NestedProjectField::Plain(_) => false,
                    })
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
            | PlanNode::Explain { input } => walk(input),
            PlanNode::NestedLoopJoin { left, right, .. } | PlanNode::Union { left, right, .. } => {
                walk(left) || walk(right)
            }
            _ => false,
        }
    }
    walk(plan)
}

/// Walk one nested projection's literal slots in source order: residual
/// filter (the correlation predicate and order keys carry no literals),
/// then limit, then offset. `limit`-then-`offset` matches source order
/// because plans whose source wrote `offset` before `limit` are refused at
/// insert (`offset_before_limit`). Shared shape with
/// [`count_nested_projection`]; both must stay in lockstep.
fn substitute_nested_projection(
    nested: &mut crate::plan::NestedProjection,
    literals: &[Literal],
    idx: &mut usize,
) {
    if let Some(residual) = &mut nested.residual {
        substitute_expr(residual, literals, idx);
    }
    if let Some(limit) = &mut nested.limit {
        substitute_expr(limit, literals, idx);
    }
    if let Some(offset) = &mut nested.offset {
        substitute_expr(offset, literals, idx);
    }
    // The `{ ... }` block comes last in source; deeper nested blocks
    // contribute their slots in field order.
    for field in &mut nested.fields {
        if let crate::plan::NestedField::Nested(inner) = field {
            substitute_nested_projection(inner, literals, idx);
        }
    }
}

/// Count one nested projection's literal slots; mirrors
/// [`substitute_nested_projection`].
fn count_nested_projection(nested: &crate::plan::NestedProjection, n: &mut usize) {
    if let Some(residual) = &nested.residual {
        count_expr(residual, n);
    }
    if let Some(limit) = &nested.limit {
        count_expr(limit, n);
    }
    if let Some(offset) = &nested.offset {
        count_expr(offset, n);
    }
    for field in &nested.fields {
        if let crate::plan::NestedField::Nested(inner) = field {
            count_nested_projection(inner, n);
        }
    }
}

fn contains_grouped_having(plan: &PlanNode) -> bool {
    match plan {
        PlanNode::GroupBy {
            having: Some(_), ..
        } => true,
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
        | PlanNode::NestedProject { input, .. }
        | PlanNode::Explain { input } => contains_grouped_having(input),
        PlanNode::NestedLoopJoin { left, right, .. } | PlanNode::Union { left, right, .. } => {
            contains_grouped_having(left) || contains_grouped_having(right)
        }
        PlanNode::SeqScan { .. }
        | PlanNode::AliasScan { .. }
        | PlanNode::IndexScan { .. }
        | PlanNode::RangeScan { .. }
        | PlanNode::ExprIndexScan { .. }
        | PlanNode::ExprRangeScan { .. }
        | PlanNode::OrderedExprIndexScan { .. }
        | PlanNode::AlterTable { .. }
        | PlanNode::DropTable { .. }
        | PlanNode::Insert { .. }
        | PlanNode::Upsert { .. }
        | PlanNode::CreateTable { .. }
        | PlanNode::ListTypes
        | PlanNode::Describe { .. }
        | PlanNode::CreateView { .. }
        | PlanNode::RefreshView { .. }
        | PlanNode::DropView { .. }
        | PlanNode::Begin
        | PlanNode::Commit
        | PlanNode::Rollback => false,
    }
}

/// Walk a plan tree depth-first, replacing every `Expr::Literal` with the
/// next literal from `literals` (consumed by index). The traversal order
/// is deterministic and matches the source order produced by
/// [`crate::canonicalize::canonicalize`].
///
/// **Walk contract** — the cache only works because both ends agree on
/// this order:
///   - Children of recursive nodes (`Filter`, `Project`, `Sort`, `Limit`,
///     `Offset`, `Aggregate`, `Update`, `Delete`) are visited *before*
///     the local expressions, because the source `User filter ... { ... }`
///     reads the table → predicate → projection in that order, and the
///     planner wraps `SeqScan → Filter → Project` accordingly.
///   - For `Update`, the input plan is visited first (which holds the
///     filter literal), then assignments in declaration order — same
///     order as the source `User filter .id = 42 update { age := 31 }`.
///
/// `pub(crate)` so the executor's prepared-statement API can reuse the
/// exact same walk — same order as canonicalise, same as the cache.
pub(crate) fn substitute_plan(plan: &mut PlanNode, literals: &[Literal], idx: &mut usize) {
    match plan {
        PlanNode::SeqScan { .. } => {}
        PlanNode::AliasScan { .. } => {}
        PlanNode::IndexScan { key, .. } => {
            substitute_expr(key, literals, idx);
        }
        PlanNode::RangeScan { start, end, .. } => {
            if let Some((expr, _)) = start {
                substitute_expr(expr, literals, idx);
            }
            if let Some((expr, _)) = end {
                substitute_expr(expr, literals, idx);
            }
        }
        PlanNode::ExprIndexScan { key, .. } => substitute_expr(key, literals, idx),
        PlanNode::ExprRangeScan { start, end, .. } => {
            if let Some((expr, _)) = start {
                substitute_expr(expr, literals, idx);
            }
            if let Some((expr, _)) = end {
                substitute_expr(expr, literals, idx);
            }
        }
        PlanNode::OrderedExprIndexScan { limit, offset, .. } => {
            substitute_expr(limit, literals, idx);
            if let Some(offset) = offset {
                substitute_expr(offset, literals, idx);
            }
        }
        PlanNode::Filter { input, predicate } => {
            substitute_plan(input, literals, idx);
            substitute_expr(predicate, literals, idx);
        }
        PlanNode::Project { input, fields } => {
            if let PlanNode::GroupBy {
                input: group_input,
                keys,
                aggregates,
                having: None,
            } = input.as_mut()
            {
                // GROUP BY lifts aggregate arguments out of their projection
                // positions. Walk the grouped input/keys first, then replay
                // each lifted argument at the exact projection position where
                // it appeared in source, preserving literal ordinals.
                substitute_plan(group_input, literals, idx);
                for key in keys {
                    substitute_expr(&mut key.expr, literals, idx);
                }
                let mut visited = std::collections::HashSet::new();
                for field in fields {
                    substitute_group_projection_expr(
                        &mut field.expr,
                        aggregates,
                        &mut visited,
                        literals,
                        idx,
                    );
                }
            } else {
                substitute_plan(input, literals, idx);
                for f in fields {
                    substitute_expr(&mut f.expr, literals, idx);
                }
            }
        }
        PlanNode::NestedProject { input, fields } => {
            // Source order: parent pipeline first, then projection fields
            // left to right. Within a nested field the block reads
            // `filter <correlation + residuals> { ... }`; the correlation
            // predicate carries no literals, so walking the residual covers
            // the block's literal slots in source order.
            substitute_plan(input, literals, idx);
            for field in fields {
                match field {
                    crate::plan::NestedProjectField::Plain(f) => {
                        substitute_expr(&mut f.expr, literals, idx);
                    }
                    crate::plan::NestedProjectField::Nested(nested) => {
                        substitute_nested_projection(nested, literals, idx);
                    }
                }
            }
        }
        PlanNode::Sort { input, keys } => {
            substitute_plan(input, literals, idx);
            for key in keys {
                substitute_expr(&mut key.expr, literals, idx);
            }
        }
        PlanNode::AlterTable { .. } => {}
        PlanNode::DropTable { .. } => {}
        PlanNode::Limit { input, count } => {
            // Source order for `filter ... limit N offset M` is
            // [filter literals, N, M]. The planner now builds
            // Limit(Offset(...)) so that execution skips M rows *before*
            // taking N. Naively walking "input then count" would yield
            // [filter, M, N] — wrong. Special-case `Limit(Offset(...))`
            // to descend into Offset's own input (which holds the filter
            // literals), then visit Limit.count, then Offset.count, so
            // the literal stream stays in source order.
            if let PlanNode::Offset {
                input: inner,
                count: off_count,
            } = input.as_mut()
            {
                substitute_plan(inner, literals, idx);
                substitute_expr(count, literals, idx);
                substitute_expr(off_count, literals, idx);
            } else {
                substitute_plan(input, literals, idx);
                substitute_expr(count, literals, idx);
            }
        }
        PlanNode::Offset { input, count } => {
            // Bare Offset (no wrapping Limit) — source order is
            // [..., offset literal] so descend first then visit count.
            substitute_plan(input, literals, idx);
            substitute_expr(count, literals, idx);
        }
        PlanNode::Aggregate {
            input, argument, ..
        } => {
            substitute_plan(input, literals, idx);
            if let Some(argument) = argument {
                substitute_expr(argument, literals, idx);
            }
        }
        PlanNode::NestedLoopJoin {
            left, right, on, ..
        } => {
            // Walk order: left subtree → right subtree → on predicate.
            // Matches canonicalise's source-order literal collection for
            // joined queries: left source tokens come first, then right
            // source tokens, then the `on` expression's literals (if any).
            substitute_plan(left, literals, idx);
            substitute_plan(right, literals, idx);
            if let Some(pred) = on {
                substitute_expr(pred, literals, idx);
            }
        }
        PlanNode::Distinct { input } => {
            substitute_plan(input, literals, idx);
        }
        PlanNode::GroupBy {
            input,
            keys,
            aggregates,
            having,
        } => {
            substitute_plan(input, literals, idx);
            for key in keys {
                substitute_expr(&mut key.expr, literals, idx);
            }
            for aggregate in aggregates {
                substitute_expr(&mut aggregate.argument, literals, idx);
            }
            if let Some(pred) = having {
                substitute_expr(pred, literals, idx);
            }
        }
        PlanNode::Insert { rows, .. } => {
            for assignments in rows {
                substitute_assignments(assignments, literals, idx);
            }
        }
        PlanNode::Upsert {
            assignments,
            on_conflict,
            ..
        } => {
            substitute_assignments(assignments, literals, idx);
            substitute_assignments(on_conflict, literals, idx);
        }
        PlanNode::Update {
            input, assignments, ..
        } => {
            substitute_plan(input, literals, idx);
            substitute_assignments(assignments, literals, idx);
        }
        PlanNode::Delete { input, .. } => {
            substitute_plan(input, literals, idx);
        }
        PlanNode::CreateTable { .. } => {}
        PlanNode::CreateView { .. } => {}
        PlanNode::RefreshView { .. } => {}
        PlanNode::DropView { .. } => {}
        PlanNode::Window { input, windows } => {
            substitute_plan(input, literals, idx);
            for w in windows {
                for arg in &mut w.args {
                    substitute_expr(arg, literals, idx);
                }
                for expr in &mut w.partition_by {
                    substitute_expr(expr, literals, idx);
                }
                for key in &mut w.order_by {
                    substitute_expr(&mut key.expr, literals, idx);
                }
            }
        }
        PlanNode::Union { left, right, .. } => {
            substitute_plan(left, literals, idx);
            substitute_plan(right, literals, idx);
        }
        PlanNode::Explain { input } => {
            substitute_plan(input, literals, idx);
        }
        PlanNode::ListTypes | PlanNode::Describe { .. } => {}
        PlanNode::Begin | PlanNode::Commit | PlanNode::Rollback => {}
    }
}

fn substitute_assignments(assignments: &mut [Assignment], literals: &[Literal], idx: &mut usize) {
    for a in assignments {
        substitute_expr(&mut a.value, literals, idx);
    }
}

fn substitute_group_projection_expr(
    expr: &mut Expr,
    aggregates: &mut [crate::plan::GroupAgg],
    visited: &mut std::collections::HashSet<String>,
    literals: &[Literal],
    idx: &mut usize,
) {
    if let Expr::Field(name) = expr {
        if visited.insert(name.clone()) {
            if let Some(aggregate) = aggregates.iter_mut().find(|agg| agg.output_name == *name) {
                substitute_expr(&mut aggregate.argument, literals, idx);
                return;
            }
        }
    }
    match expr {
        Expr::BinaryOp(left, _, right) | Expr::Coalesce(left, right) => {
            substitute_group_projection_expr(left, aggregates, visited, literals, idx);
            substitute_group_projection_expr(right, aggregates, visited, literals, idx);
        }
        Expr::UnaryOp(_, inner) | Expr::Cast(inner, _) => {
            substitute_group_projection_expr(inner, aggregates, visited, literals, idx);
        }
        Expr::ScalarFunc(_, args) => {
            for arg in args {
                substitute_group_projection_expr(arg, aggregates, visited, literals, idx);
            }
        }
        Expr::InList { expr, list, .. } => {
            substitute_group_projection_expr(expr, aggregates, visited, literals, idx);
            for item in list {
                substitute_group_projection_expr(item, aggregates, visited, literals, idx);
            }
        }
        Expr::Case { whens, else_expr } => {
            for (condition, result) in whens {
                substitute_group_projection_expr(condition, aggregates, visited, literals, idx);
                substitute_group_projection_expr(result, aggregates, visited, literals, idx);
            }
            if let Some(expr) = else_expr {
                substitute_group_projection_expr(expr, aggregates, visited, literals, idx);
            }
        }
        _ => substitute_expr(expr, literals, idx),
    }
}

/// Count every `Expr::Literal` slot reachable from `plan` using the same
/// walk order as [`substitute_plan`]. Used by `Engine::prepare` to validate
/// that calls to `execute_prepared` pass the right number of literals, and
/// to fail early if a caller prepares a query with zero literals (which
/// would be a no-op for the prepared API — better to catch that up front).
pub(crate) fn count_literal_slots(plan: &PlanNode) -> usize {
    let mut n = 0usize;
    count_plan(plan, &mut n);
    n
}

fn count_plan(plan: &PlanNode, n: &mut usize) {
    match plan {
        PlanNode::SeqScan { .. } => {}
        PlanNode::AliasScan { .. } => {}
        PlanNode::IndexScan { key, .. } => count_expr(key, n),
        PlanNode::RangeScan { start, end, .. } => {
            if let Some((expr, _)) = start {
                count_expr(expr, n);
            }
            if let Some((expr, _)) = end {
                count_expr(expr, n);
            }
        }
        PlanNode::ExprIndexScan { key, .. } => count_expr(key, n),
        PlanNode::ExprRangeScan { start, end, .. } => {
            if let Some((expr, _)) = start {
                count_expr(expr, n);
            }
            if let Some((expr, _)) = end {
                count_expr(expr, n);
            }
        }
        PlanNode::OrderedExprIndexScan { limit, offset, .. } => {
            count_expr(limit, n);
            if let Some(offset) = offset {
                count_expr(offset, n);
            }
        }
        PlanNode::Filter { input, predicate } => {
            count_plan(input, n);
            count_expr(predicate, n);
        }
        PlanNode::Project { input, fields } => {
            if let PlanNode::GroupBy {
                input: group_input,
                keys,
                aggregates,
                having: None,
            } = input.as_ref()
            {
                count_plan(group_input, n);
                for key in keys {
                    count_expr(&key.expr, n);
                }
                let mut visited = std::collections::HashSet::new();
                for field in fields {
                    count_group_projection_expr(&field.expr, aggregates, &mut visited, n);
                }
            } else {
                count_plan(input, n);
                for f in fields {
                    count_expr(&f.expr, n);
                }
            }
        }
        PlanNode::NestedProject { input, fields } => {
            // Mirrors the substitute walk: parent pipeline, then fields left
            // to right, with nested blocks contributing their residual slots.
            count_plan(input, n);
            for field in fields {
                match field {
                    crate::plan::NestedProjectField::Plain(f) => count_expr(&f.expr, n),
                    crate::plan::NestedProjectField::Nested(nested) => {
                        count_nested_projection(nested, n);
                    }
                }
            }
        }
        PlanNode::Sort { input, keys } => {
            count_plan(input, n);
            for key in keys {
                count_expr(&key.expr, n);
            }
        }
        PlanNode::Limit { input, count } => {
            // Mirror the substitute walk: `Limit(Offset(...))` descends
            // into the offset's child first, then counts Limit.count,
            // then Offset.count. Source order is
            // [..., limit literal, offset literal].
            if let PlanNode::Offset {
                input: inner,
                count: off_count,
            } = input.as_ref()
            {
                count_plan(inner, n);
                count_expr(count, n);
                count_expr(off_count, n);
            } else {
                count_plan(input, n);
                count_expr(count, n);
            }
        }
        PlanNode::Offset { input, count } => {
            count_plan(input, n);
            count_expr(count, n);
        }
        PlanNode::Aggregate {
            input, argument, ..
        } => {
            count_plan(input, n);
            if let Some(argument) = argument {
                count_expr(argument, n);
            }
        }
        PlanNode::NestedLoopJoin {
            left, right, on, ..
        } => {
            count_plan(left, n);
            count_plan(right, n);
            if let Some(pred) = on {
                count_expr(pred, n);
            }
        }
        PlanNode::Distinct { input } => count_plan(input, n),
        PlanNode::GroupBy {
            input,
            keys,
            aggregates,
            having,
        } => {
            count_plan(input, n);
            for key in keys {
                count_expr(&key.expr, n);
            }
            for aggregate in aggregates {
                count_expr(&aggregate.argument, n);
            }
            if let Some(pred) = having {
                count_expr(pred, n);
            }
        }
        PlanNode::Insert { rows, .. } => {
            for assignments in rows {
                for a in assignments {
                    count_expr(&a.value, n);
                }
            }
        }
        PlanNode::Upsert {
            assignments,
            on_conflict,
            ..
        } => {
            for a in assignments {
                count_expr(&a.value, n);
            }
            for a in on_conflict {
                count_expr(&a.value, n);
            }
        }
        PlanNode::Update {
            input, assignments, ..
        } => {
            count_plan(input, n);
            for a in assignments {
                count_expr(&a.value, n);
            }
        }
        PlanNode::Delete { input, .. } => count_plan(input, n),
        PlanNode::CreateTable { .. } => {}
        PlanNode::AlterTable { .. } => {}
        PlanNode::DropTable { .. } => {}
        PlanNode::CreateView { .. } => {}
        PlanNode::RefreshView { .. } => {}
        PlanNode::DropView { .. } => {}
        PlanNode::Window { input, windows } => {
            count_plan(input, n);
            for w in windows {
                for arg in &w.args {
                    count_expr(arg, n);
                }
                for expr in &w.partition_by {
                    count_expr(expr, n);
                }
                for key in &w.order_by {
                    count_expr(&key.expr, n);
                }
            }
        }
        PlanNode::Union { left, right, .. } => {
            count_plan(left, n);
            count_plan(right, n);
        }
        PlanNode::Explain { input } => {
            count_plan(input, n);
        }
        PlanNode::ListTypes | PlanNode::Describe { .. } => {}
        PlanNode::Begin | PlanNode::Commit | PlanNode::Rollback => {}
    }
}

fn count_group_projection_expr(
    expr: &Expr,
    aggregates: &[crate::plan::GroupAgg],
    visited: &mut std::collections::HashSet<String>,
    n: &mut usize,
) {
    if let Expr::Field(name) = expr {
        if visited.insert(name.clone()) {
            if let Some(aggregate) = aggregates.iter().find(|agg| agg.output_name == *name) {
                count_expr(&aggregate.argument, n);
                return;
            }
        }
    }
    match expr {
        Expr::BinaryOp(left, _, right) | Expr::Coalesce(left, right) => {
            count_group_projection_expr(left, aggregates, visited, n);
            count_group_projection_expr(right, aggregates, visited, n);
        }
        Expr::UnaryOp(_, inner) | Expr::Cast(inner, _) => {
            count_group_projection_expr(inner, aggregates, visited, n);
        }
        Expr::ScalarFunc(_, args) => {
            for arg in args {
                count_group_projection_expr(arg, aggregates, visited, n);
            }
        }
        Expr::InList { expr, list, .. } => {
            count_group_projection_expr(expr, aggregates, visited, n);
            for item in list {
                count_group_projection_expr(item, aggregates, visited, n);
            }
        }
        Expr::Case { whens, else_expr } => {
            for (condition, result) in whens {
                count_group_projection_expr(condition, aggregates, visited, n);
                count_group_projection_expr(result, aggregates, visited, n);
            }
            if let Some(expr) = else_expr {
                count_group_projection_expr(expr, aggregates, visited, n);
            }
        }
        _ => count_expr(expr, n),
    }
}

fn count_expr(expr: &Expr, n: &mut usize) {
    match expr {
        Expr::Literal(_) => *n += 1,
        Expr::Field(_) | Expr::QualifiedField { .. } | Expr::Param(_) => {}
        Expr::BinaryOp(l, _, r) => {
            count_expr(l, n);
            count_expr(r, n);
        }
        Expr::UnaryOp(_, inner) => count_expr(inner, n),
        Expr::FunctionCall(_, inner, _) => count_expr(inner, n),
        Expr::Coalesce(l, r) => {
            count_expr(l, n);
            count_expr(r, n);
        }
        Expr::InList { expr, list, .. } => {
            count_expr(expr, n);
            for item in list {
                count_expr(item, n);
            }
        }
        Expr::ScalarFunc(_, args) => {
            for a in args {
                count_expr(a, n);
            }
        }
        Expr::Cast(inner, _) => count_expr(inner, n),
        Expr::Case { whens, else_expr } => {
            for (cond, result) in whens {
                count_expr(cond, n);
                count_expr(result, n);
            }
            if let Some(e) = else_expr {
                count_expr(e, n);
            }
        }
        Expr::InSubquery { expr, .. } => {
            count_expr(expr, n);
            // Subquery literals are not counted — the subquery is
            // re-planned/executed separately.
        }
        Expr::ExistsSubquery { .. } => {
            // Subquery literals are not counted — the subquery is
            // re-planned/executed separately.
        }
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                count_expr(a, n);
            }
            for expr in partition_by {
                count_expr(expr, n);
            }
            for key in order_by {
                count_expr(&key.expr, n);
            }
        }
        // JSON path segments are STRUCTURAL, never literal slots (#137): only
        // the base can carry literals (it can't today — it is a Field — but
        // recursing keeps this correct if the base grammar ever widens).
        Expr::JsonPath { base, .. } => count_expr(base, n),
        // Runtime-only literal (correlated/subquery substitution); never
        // reaches the plan cache, and it occupies no source literal slot.
        Expr::ValueLit(_) => {}
        Expr::Null => {}
        // Plans carrying nested projections are never inserted into the
        // cache (see `plan_contains_nested_project`), so this node's inner
        // literal slots are never walked.
        Expr::NestedQuery(_) => {}
    }
}

fn substitute_expr(expr: &mut Expr, literals: &[Literal], idx: &mut usize) {
    match expr {
        Expr::Literal(_) => {
            // The cached plan held the *first* call's literal at this
            // slot; replace with the new call's value at the matching
            // source position.
            *expr = Expr::Literal(literals[*idx].clone());
            *idx += 1;
        }
        Expr::Field(_) | Expr::QualifiedField { .. } | Expr::Param(_) => {}
        Expr::BinaryOp(l, _, r) => {
            substitute_expr(l, literals, idx);
            substitute_expr(r, literals, idx);
        }
        Expr::UnaryOp(_, inner) => {
            substitute_expr(inner, literals, idx);
        }
        Expr::FunctionCall(_, inner, _) => {
            substitute_expr(inner, literals, idx);
        }
        Expr::Coalesce(l, r) => {
            substitute_expr(l, literals, idx);
            substitute_expr(r, literals, idx);
        }
        Expr::InList { expr, list, .. } => {
            substitute_expr(expr, literals, idx);
            for item in list {
                substitute_expr(item, literals, idx);
            }
        }
        Expr::ScalarFunc(_, args) => {
            for a in args {
                substitute_expr(a, literals, idx);
            }
        }
        Expr::Cast(inner, _) => substitute_expr(inner, literals, idx),
        Expr::Case { whens, else_expr } => {
            for (cond, result) in whens {
                substitute_expr(cond, literals, idx);
                substitute_expr(result, literals, idx);
            }
            if let Some(e) = else_expr {
                substitute_expr(e, literals, idx);
            }
        }
        Expr::InSubquery { expr, .. } => {
            substitute_expr(expr, literals, idx);
        }
        Expr::ExistsSubquery { .. } => {
            // Subquery has its own literal list; nothing to substitute
            // at this level.
        }
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                substitute_expr(a, literals, idx);
            }
            for expr in partition_by {
                substitute_expr(expr, literals, idx);
            }
            for key in order_by {
                substitute_expr(&mut key.expr, literals, idx);
            }
        }
        // JSON path segments are STRUCTURAL (#137): substitution only recurses
        // into the base, mirroring `count_expr`, so the slot walk stays aligned.
        Expr::JsonPath { base, .. } => substitute_expr(base, literals, idx),
        // Runtime-only literal (correlated/subquery substitution); never
        // reaches the plan cache, so there is nothing to substitute.
        Expr::ValueLit(_) => {}
        Expr::Null => {}
        // Never cached (see `plan_contains_nested_project`); nothing to
        // substitute.
        Expr::NestedQuery(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonicalize::canonicalize;
    use crate::planner;

    #[test]
    fn test_cache_hit_substitutes_literal() {
        let mut cache = PlanCache::new(100);

        // First call: "User filter .id = 42" — miss, plan + insert.
        let q1 = "User filter .id = 42";
        let (h1, lits1) = canonicalize(q1).unwrap();
        let p1 = planner::plan(q1).unwrap();
        cache.insert(h1, p1, lits1.len());

        // Second call with a different literal — should hit and produce
        // a plan with the new literal substituted in.
        let q2 = "User filter .id = 99";
        let (h2, lits2) = canonicalize(q2).unwrap();
        assert_eq!(h1, h2, "different literals must hash the same");

        let plan = cache.get_with_substitution(h2, &lits2).expect("hit");

        // The plan should be `IndexScan { key: Literal::Int(99) }`.
        match plan {
            PlanNode::IndexScan { key, .. } => {
                assert_eq!(key, Expr::Literal(Literal::Int(99)));
            }
            other => panic!("expected IndexScan, got {other:?}"),
        }

        // First call's literal vector still holds 42, untouched — proves
        // we substituted on a clone, not the cached template.
        assert_eq!(lits1, vec![Literal::Int(42)]);
        assert_eq!(cache.hits, 1);
        assert_eq!(cache.misses, 0);
    }

    #[test]
    fn test_subquery_plan_not_cached() {
        // #137: `canonicalize` collects the inner `100` literal at the token
        // level, but it lives in an un-walked subquery AST that
        // `substitute_plan` can't reach (`count_literal_slots` returns 0).
        // The counts disagree, so the cache must refuse to store the plan —
        // otherwise a later same-shape call with a different inner literal
        // would be served this plan's stale `100`.
        let mut cache = PlanCache::new(100);
        let q = "User filter .id in (Ord filter .total > 100 { .user_id })";
        let (h, lits) = canonicalize(q).unwrap();
        assert_eq!(lits.len(), 1, "canonicalize collects the inner literal");
        let plan = planner::plan(q).unwrap();
        assert_eq!(
            count_literal_slots(&plan),
            0,
            "the subquery literal is not a reachable substitution slot"
        );
        cache.insert(h, plan, lits.len());
        assert!(cache.is_empty(), "subquery plans must not be cached (#137)");
        assert!(cache.get_with_substitution(h, &lits).is_none());
    }

    #[test]
    fn test_cache_miss_returns_none_and_bumps_counter() {
        let mut cache = PlanCache::new(100);
        assert!(cache.get_with_substitution(99999, &[]).is_none());
        assert_eq!(cache.misses, 1);
        assert_eq!(cache.hits, 0);
    }

    #[test]
    fn test_multi_literal_filter_substitution() {
        let mut cache = PlanCache::new(100);
        let q1 = r#"User filter .age > 30 and .status = "active" { .name }"#;
        let (h1, lits1) = canonicalize(q1).unwrap();
        cache.insert(h1, planner::plan(q1).unwrap(), lits1.len());

        let q2 = r#"User filter .age > 50 and .status = "pending" { .name }"#;
        let (h2, lits2) = canonicalize(q2).unwrap();
        let plan = cache.get_with_substitution(h2, &lits2).expect("hit");

        // Walk the plan and pull out every literal — should be [50, "pending"].
        let mut found = Vec::new();
        collect_literals_for_test(&plan, &mut found);
        assert_eq!(
            found,
            vec![Literal::Int(50), Literal::String("pending".into()),]
        );
    }

    #[test]
    fn test_grouped_join_having_is_not_cached_without_source_slot_ordinals() {
        // Qualified grouped joins have the same ambiguity as single-table
        // grouped HAVING: planning no longer retains whether HAVING appeared
        // before or after the projection, so matching slot counts alone do not
        // make literal replay safe.
        let mut cache = PlanCache::new(100);
        let q1 = "User as u join Order as o on u.id = o.user_id \
                  group u.status having count(o.total) > 1 { u.status, n: count(o.total) }";
        let (h1, lits1) = canonicalize(q1).unwrap();
        let p1 = planner::plan(q1).unwrap();

        // Only the HAVING `1` is a substitutable literal; the slot count still
        // matches, proving the grouped-HAVING shape guard is what protects the
        // cache rather than an incidental count mismatch.
        assert_eq!(lits1.len(), 1, "only the HAVING literal is collected");
        assert_eq!(
            count_literal_slots(&p1),
            lits1.len(),
            "group keys/args are structural, so slots == literals (#137)"
        );
        cache.insert(h1, p1, lits1.len());
        assert!(cache.is_empty(), "grouped HAVING plans must not cache");

        let q2 = "User as u join Order as o on u.id = o.user_id \
                  group u.status having count(o.total) > 5 { u.status, n: count(o.total) }";
        let (h2, lits2) = canonicalize(q2).unwrap();
        assert_eq!(h1, h2, "different HAVING literal must hash the same");

        assert!(cache.get_with_substitution(h2, &lits2).is_none());
        assert_eq!(cache.hits, 0);
        assert_eq!(cache.misses, 1);
    }

    #[test]
    fn json_path_slot_count_invariant() {
        // #137 property: over path-bearing queries, count_literal_slots(plan)
        // must equal the source literal count from canonicalize. Path segments
        // (keys and array indexes) are structural and must not inflate either
        // side, so the plan is cacheable.
        for q in [
            r#"Post filter .data->author->name = "x""#,
            r#"Post filter .data->tags->0 = "rust""#,
            r#"Post filter .data->age > 21 and .data->year = 2026"#,
            r#"Post filter .data->"weird key" = 1 { .id }"#,
            r#"Post { author: .data->author, first_tag: .data->tags->0 }"#,
        ] {
            let (_, lits) = canonicalize(q).unwrap();
            let plan = planner::plan(q).unwrap();
            assert_eq!(
                count_literal_slots(&plan),
                lits.len(),
                "slot count must equal source literal count for `{q}`"
            );
        }
    }

    #[test]
    fn json_path_plan_round_trips_cache() {
        // A path-bearing query caches on first call and, on a second call with
        // a DIFFERENT comparison literal but the SAME path, hits the cache and
        // substitutes the new literal (no panic, no stale value).
        let mut cache = PlanCache::new(100);
        let q1 = r#"Post filter .data->age > 21"#;
        let (h1, lits1) = canonicalize(q1).unwrap();
        let p1 = planner::plan(q1).unwrap();
        assert_eq!(count_literal_slots(&p1), lits1.len());
        cache.insert(h1, p1, lits1.len());
        assert_eq!(cache.len(), 1, "path plan must cache");

        let q2 = r#"Post filter .data->age > 65"#;
        let (h2, lits2) = canonicalize(q2).unwrap();
        assert_eq!(h1, h2, "same path, different literal → same hash");
        let plan = cache.get_with_substitution(h2, &lits2).expect("hit");

        let mut found = Vec::new();
        collect_literals_for_test(&plan, &mut found);
        assert_eq!(found, vec![Literal::Int(65)], "new literal substituted");
        assert_eq!(cache.hits, 1);
    }

    #[test]
    fn json_path_different_path_is_a_cache_miss() {
        let mut cache = PlanCache::new(100);
        let q1 = r#"Post filter .data->age > 21"#;
        let (h1, lits1) = canonicalize(q1).unwrap();
        cache.insert(h1, planner::plan(q1).unwrap(), lits1.len());

        // Different key → different shape → miss (would otherwise serve a plan
        // that walks the wrong path).
        let q2 = r#"Post filter .data->year > 21"#;
        let (h2, lits2) = canonicalize(q2).unwrap();
        assert!(
            cache.get_with_substitution(h2, &lits2).is_none(),
            "a different path must not hit the cached plan"
        );
    }

    #[test]
    fn expression_aggregate_literal_substitutes_on_cache_hit() {
        let mut cache = PlanCache::new(8);
        let q1 = "Post group .data->kind { total: sum(.data->age + 1) }";
        let (h1, literals1) = canonicalize(q1).unwrap();
        let plan1 = planner::plan(q1).unwrap();
        assert_eq!(count_literal_slots(&plan1), literals1.len());
        cache.insert(h1, plan1, literals1.len());

        let q2 = "Post group .data->kind { total: sum(.data->age + 7) }";
        let (h2, literals2) = canonicalize(q2).unwrap();
        assert_eq!(h1, h2);
        let plan = cache.get_with_substitution(h2, &literals2).expect("hit");
        let mut found = Vec::new();
        collect_literals_for_test(&plan, &mut found);
        assert_eq!(found, vec![Literal::Int(7)]);
    }

    #[test]
    fn expression_index_equality_and_range_bounds_substitute_on_cache_hits() {
        let mut cache = PlanCache::new(8);

        let q1 = "Post filter .data->age = 21";
        let (h1, literals1) = canonicalize(q1).unwrap();
        let plan1 = planner::plan(q1).unwrap();
        assert!(matches!(plan1, PlanNode::ExprIndexScan { .. }));
        assert_eq!(count_literal_slots(&plan1), literals1.len());
        cache.insert(h1, plan1, literals1.len());

        let q2 = "Post filter .data->age = 65";
        let (h2, literals2) = canonicalize(q2).unwrap();
        assert_eq!(h1, h2);
        let plan = cache.get_with_substitution(h2, &literals2).expect("hit");
        let mut found = Vec::new();
        collect_literals_for_test(&plan, &mut found);
        assert_eq!(found, vec![Literal::Int(65)]);

        let q3 = "Post filter .data->age >= 18 and .data->age < 65";
        let (h3, literals3) = canonicalize(q3).unwrap();
        let plan3 = planner::plan(q3).unwrap();
        assert!(matches!(plan3, PlanNode::ExprRangeScan { .. }));
        assert_eq!(count_literal_slots(&plan3), literals3.len());
        cache.insert(h3, plan3, literals3.len());

        let q4 = "Post filter .data->age >= 25 and .data->age < 80";
        let (h4, literals4) = canonicalize(q4).unwrap();
        assert_eq!(h3, h4);
        let plan = cache.get_with_substitution(h4, &literals4).expect("hit");
        let mut found = Vec::new();
        collect_literals_for_test(&plan, &mut found);
        assert_eq!(found, vec![Literal::Int(25), Literal::Int(80)]);
    }

    #[test]
    fn ordered_expression_scan_limit_offset_substitute_in_canonical_order() {
        let mut cache = PlanCache::new(8);
        let q1 = "Post order .data->age desc limit 10 offset 2";
        let (h1, literals1) = canonicalize(q1).unwrap();
        let plan1 = planner::plan(q1).unwrap();
        assert!(matches!(plan1, PlanNode::OrderedExprIndexScan { .. }));
        assert_eq!(count_literal_slots(&plan1), literals1.len());
        cache.insert(h1, plan1, literals1.len());

        let q2 = "Post order .data->age desc limit 20 offset 3";
        let (h2, literals2) = canonicalize(q2).unwrap();
        assert_eq!(h1, h2);
        let plan = cache.get_with_substitution(h2, &literals2).expect("hit");
        let mut found = Vec::new();
        collect_literals_for_test(&plan, &mut found);
        assert_eq!(found, vec![Literal::Int(20), Literal::Int(3)]);
    }

    #[test]
    fn test_update_by_pk_substitution() {
        let mut cache = PlanCache::new(100);
        let q1 = "User filter .id = 1 update { age := 100 }";
        let (h1, lits1) = canonicalize(q1).unwrap();
        cache.insert(h1, planner::plan(q1).unwrap(), lits1.len());

        let q2 = "User filter .id = 7 update { age := 200 }";
        let (h2, lits2) = canonicalize(q2).unwrap();
        let plan = cache.get_with_substitution(h2, &lits2).expect("hit");

        let mut found = Vec::new();
        collect_literals_for_test(&plan, &mut found);
        assert_eq!(found, vec![Literal::Int(7), Literal::Int(200)]);
    }

    #[test]
    fn test_insert_substitution() {
        let mut cache = PlanCache::new(100);
        let q1 = r#"insert User { id := 1, name := "Alice", age := 20 }"#;
        let (h1, lits1) = canonicalize(q1).unwrap();
        cache.insert(h1, planner::plan(q1).unwrap(), lits1.len());

        let q2 = r#"insert User { id := 2, name := "Bob", age := 30 }"#;
        let (h2, lits2) = canonicalize(q2).unwrap();
        let plan = cache.get_with_substitution(h2, &lits2).expect("hit");

        let mut found = Vec::new();
        collect_literals_for_test(&plan, &mut found);
        assert_eq!(
            found,
            vec![
                Literal::Int(2),
                Literal::String("Bob".into()),
                Literal::Int(30),
            ]
        );
    }

    /// #151/#140-class: `uuid("…")` const-fold sugar plans as
    /// `Cast(Literal::String, Uuid)` — one substitutable slot. It must cache
    /// AND rebind the inner string on a same-shape second call (a bulk load).
    #[test]
    fn test_insert_uuid_sugar_cacheable_and_substitutes() {
        let mut cache = PlanCache::new(100);
        let q1 = r#"insert User { id := uuid("00000000-0000-0000-0000-000000000001") }"#;
        let (h1, lits1) = canonicalize(q1).unwrap();
        assert_eq!(lits1.len(), 1, "the inner string is the only literal");
        let plan = planner::plan(q1).unwrap();
        assert_eq!(
            count_literal_slots(&plan),
            1,
            "Cast wrapping a Literal is a reachable substitution slot"
        );
        cache.insert(h1, plan, lits1.len());
        assert!(!cache.is_empty(), "uuid() insert must be cacheable");

        let q2 = r#"insert User { id := uuid("00000000-0000-0000-0000-000000000002") }"#;
        let (h2, lits2) = canonicalize(q2).unwrap();
        assert_eq!(h1, h2, "same shape hashes identically");
        let subst = cache.get_with_substitution(h2, &lits2).expect("hit");

        let mut found = Vec::new();
        collect_literals_for_test(&subst, &mut found);
        assert_eq!(
            found,
            vec![Literal::String(
                "00000000-0000-0000-0000-000000000002".into()
            )],
            "the second call's uuid must be substituted in, not the cached one"
        );
    }

    /// Two-arg `cast(.x, "uuid")` bakes the target into the AST but leaves the
    /// `"uuid"` string as a collected literal with no matching plan slot, so
    /// the count-mismatch guard refuses to cache it (pre-existing behavior,
    /// now confirmed for the uuid target).
    #[test]
    fn test_two_arg_cast_uuid_not_cached() {
        let mut cache = PlanCache::new(100);
        let q = r#"User filter .id = cast(.other, "uuid")"#;
        let (h, lits) = canonicalize(q).unwrap();
        assert_eq!(
            lits.len(),
            1,
            "canonicalize collects the cast-target string"
        );
        let plan = planner::plan(q).unwrap();
        assert_eq!(
            count_literal_slots(&plan),
            0,
            "the cast target is baked into the AST, not a slot"
        );
        cache.insert(h, plan, lits.len());
        assert!(cache.is_empty(), "cast(x, \"uuid\") must not be cached");
    }

    #[test]
    fn test_eviction_on_capacity() {
        let mut cache = PlanCache::new(2);
        let q1 = "User";
        let q2 = "User filter .age > 1";
        let _q3 = "User filter .age > 2";
        // q3 has same canonical as q2 — won't trigger eviction.
        // Use a different shape to force eviction.
        let q3_distinct = "User filter .id = 5";

        let (h1, lits1) = canonicalize(q1).unwrap();
        let (h2, lits2) = canonicalize(q2).unwrap();
        let (h3, lits3) = canonicalize(q3_distinct).unwrap();
        cache.insert(h1, planner::plan(q1).unwrap(), lits1.len());
        cache.insert(h2, planner::plan(q2).unwrap(), lits2.len());
        // Cache full → inserting a third *new* shape should clear.
        cache.insert(h3, planner::plan(q3_distinct).unwrap(), lits3.len());
        assert!(cache.cache.contains_key(&h3));
        assert_eq!(cache.cache.len(), 1);
    }

    /// Test helper — depth-first walk that pulls out every Literal in the
    /// same order `substitute_plan` would visit them. Used to verify
    /// substitution actually wrote to the right slots.
    fn collect_literals_for_test(plan: &PlanNode, out: &mut Vec<Literal>) {
        match plan {
            PlanNode::SeqScan { .. } => {}
            PlanNode::AliasScan { .. } => {}
            PlanNode::IndexScan { key, .. } => collect_expr_literals(key, out),
            PlanNode::RangeScan { start, end, .. } => {
                if let Some((expr, _)) = start {
                    collect_expr_literals(expr, out);
                }
                if let Some((expr, _)) = end {
                    collect_expr_literals(expr, out);
                }
            }
            PlanNode::ExprIndexScan { key, .. } => collect_expr_literals(key, out),
            PlanNode::ExprRangeScan { start, end, .. } => {
                if let Some((expr, _)) = start {
                    collect_expr_literals(expr, out);
                }
                if let Some((expr, _)) = end {
                    collect_expr_literals(expr, out);
                }
            }
            PlanNode::OrderedExprIndexScan { limit, offset, .. } => {
                collect_expr_literals(limit, out);
                if let Some(offset) = offset {
                    collect_expr_literals(offset, out);
                }
            }
            PlanNode::Filter { input, predicate } => {
                collect_literals_for_test(input, out);
                collect_expr_literals(predicate, out);
            }
            PlanNode::Project { input, fields } => {
                collect_literals_for_test(input, out);
                for f in fields {
                    collect_expr_literals(&f.expr, out);
                }
            }
            PlanNode::NestedProject { input, fields } => {
                fn collect_nested(nested: &crate::plan::NestedProjection, out: &mut Vec<Literal>) {
                    if let Some(residual) = &nested.residual {
                        collect_expr_literals(residual, out);
                    }
                    if let Some(limit) = &nested.limit {
                        collect_expr_literals(limit, out);
                    }
                    if let Some(offset) = &nested.offset {
                        collect_expr_literals(offset, out);
                    }
                    for field in &nested.fields {
                        if let crate::plan::NestedField::Nested(inner) = field {
                            collect_nested(inner, out);
                        }
                    }
                }
                collect_literals_for_test(input, out);
                for field in fields {
                    match field {
                        crate::plan::NestedProjectField::Plain(f) => {
                            collect_expr_literals(&f.expr, out);
                        }
                        crate::plan::NestedProjectField::Nested(nested) => {
                            collect_nested(nested, out);
                        }
                    }
                }
            }
            PlanNode::Sort { input, keys } => {
                collect_literals_for_test(input, out);
                for key in keys {
                    collect_expr_literals(&key.expr, out);
                }
            }
            PlanNode::Limit { input, count } => {
                collect_literals_for_test(input, out);
                collect_expr_literals(count, out);
            }
            PlanNode::Offset { input, count } => {
                collect_literals_for_test(input, out);
                collect_expr_literals(count, out);
            }
            PlanNode::Aggregate {
                input, argument, ..
            } => {
                collect_literals_for_test(input, out);
                if let Some(argument) = argument {
                    collect_expr_literals(argument, out);
                }
            }
            PlanNode::NestedLoopJoin {
                left, right, on, ..
            } => {
                collect_literals_for_test(left, out);
                collect_literals_for_test(right, out);
                if let Some(pred) = on {
                    collect_expr_literals(pred, out);
                }
            }
            PlanNode::Insert { rows, .. } => {
                for assignments in rows {
                    for a in assignments {
                        collect_expr_literals(&a.value, out);
                    }
                }
            }
            PlanNode::Upsert {
                assignments,
                on_conflict,
                ..
            } => {
                for a in assignments {
                    collect_expr_literals(&a.value, out);
                }
                for a in on_conflict {
                    collect_expr_literals(&a.value, out);
                }
            }
            PlanNode::Update {
                input, assignments, ..
            } => {
                collect_literals_for_test(input, out);
                for a in assignments {
                    collect_expr_literals(&a.value, out);
                }
            }
            PlanNode::Distinct { input } => collect_literals_for_test(input, out),
            PlanNode::GroupBy {
                input,
                keys,
                aggregates,
                having,
            } => {
                collect_literals_for_test(input, out);
                for key in keys {
                    collect_expr_literals(&key.expr, out);
                }
                for aggregate in aggregates {
                    collect_expr_literals(&aggregate.argument, out);
                }
                if let Some(pred) = having {
                    collect_expr_literals(pred, out);
                }
            }
            PlanNode::Delete { input, .. } => collect_literals_for_test(input, out),
            PlanNode::CreateTable { .. } => {}
            PlanNode::AlterTable { .. } => {}
            PlanNode::DropTable { .. } => {}
            PlanNode::CreateView { .. } => {}
            PlanNode::RefreshView { .. } => {}
            PlanNode::DropView { .. } => {}
            PlanNode::Window { input, windows } => {
                collect_literals_for_test(input, out);
                for w in windows {
                    for arg in &w.args {
                        collect_expr_literals(arg, out);
                    }
                    for expr in &w.partition_by {
                        collect_expr_literals(expr, out);
                    }
                    for key in &w.order_by {
                        collect_expr_literals(&key.expr, out);
                    }
                }
            }
            PlanNode::Union { left, right, .. } => {
                collect_literals_for_test(left, out);
                collect_literals_for_test(right, out);
            }
            PlanNode::Explain { input } => {
                collect_literals_for_test(input, out);
            }
            PlanNode::ListTypes | PlanNode::Describe { .. } => {}
            PlanNode::Begin | PlanNode::Commit | PlanNode::Rollback => {}
        }
    }

    fn collect_expr_literals(expr: &Expr, out: &mut Vec<Literal>) {
        match expr {
            Expr::Literal(l) => out.push(l.clone()),
            Expr::Field(_) | Expr::QualifiedField { .. } | Expr::Param(_) => {}
            Expr::BinaryOp(l, _, r) => {
                collect_expr_literals(l, out);
                collect_expr_literals(r, out);
            }
            Expr::UnaryOp(_, inner) => collect_expr_literals(inner, out),
            Expr::FunctionCall(_, inner, _) => collect_expr_literals(inner, out),
            Expr::Coalesce(l, r) => {
                collect_expr_literals(l, out);
                collect_expr_literals(r, out);
            }
            Expr::InList { expr, list, .. } => {
                collect_expr_literals(expr, out);
                for item in list {
                    collect_expr_literals(item, out);
                }
            }
            Expr::ScalarFunc(_, args) => {
                for a in args {
                    collect_expr_literals(a, out);
                }
            }
            Expr::Cast(inner, _) => collect_expr_literals(inner, out),
            Expr::Case { whens, else_expr } => {
                for (cond, result) in whens {
                    collect_expr_literals(cond, out);
                    collect_expr_literals(result, out);
                }
                if let Some(e) = else_expr {
                    collect_expr_literals(e, out);
                }
            }
            Expr::InSubquery { expr, .. } => {
                collect_expr_literals(expr, out);
            }
            Expr::ExistsSubquery { .. } => {}
            Expr::Window {
                args,
                partition_by,
                order_by,
                ..
            } => {
                for a in args {
                    collect_expr_literals(a, out);
                }
                for expr in partition_by {
                    collect_expr_literals(expr, out);
                }
                for key in order_by {
                    collect_expr_literals(&key.expr, out);
                }
            }
            // JSON path segments are structural, never literals — mirror
            // count_expr/substitute_expr and recurse into the base only.
            Expr::JsonPath { base, .. } => collect_expr_literals(base, out),
            Expr::ValueLit(_) => {}
            Expr::Null => {}
            // Never cached; mirrors count_expr/substitute_expr.
            Expr::NestedQuery(_) => {}
        }
    }
}
