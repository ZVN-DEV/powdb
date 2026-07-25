//! Plan-wide validation: JSON path typing and stray-aggregate rejection.

use crate::result::QueryError;
use powdb_storage::catalog::Catalog;
use powdb_storage::types::*;

use crate::executor::compiled::*;

use super::*;

/// Reject any aggregate `FunctionCall` that survives planning into an
/// evaluable position (a projection field, a filter predicate, or a HAVING
/// clause). The grouped-aggregate planner rewrites every supported aggregate
/// into a `Field` reference to a computed column, so a surviving
/// `FunctionCall` means the aggregate sits somewhere the engine cannot
/// evaluate it. `eval_expr` would otherwise silently produce `Empty` there (a
/// wrong answer); this turns that into a typed error before any row is
/// evaluated. Walks the whole plan so fused fast paths cannot bypass it.
/// Column indices a predicate reads, INCLUDING the json columns that JSON `->`
/// path bases decode from. The compiled walker `predicate_column_indices`
/// (`collect_field_indices`) does not descend into `Expr::JsonPath`, so on its
/// own it would leave the json column undecoded and every path evaluate to the
/// empty set. This augments it with each path's base column so `decode_selective`
/// materializes the value the path walks.
pub(crate) fn predicate_column_indices_json(expr: &Expr, columns: &[String]) -> Vec<usize> {
    let mut indices = predicate_column_indices(expr, columns);
    collect_json_path_base_indices(expr, columns, &mut indices);
    indices.sort_unstable();
    indices.dedup();
    indices
}

/// Add the column index of every `JsonPath` base reachable from `expr`.
fn collect_json_path_base_indices(expr: &Expr, columns: &[String], out: &mut Vec<usize>) {
    match expr {
        Expr::JsonPath { base, .. } => {
            let name = match base.as_ref() {
                Expr::Field(n) => n.clone(),
                Expr::QualifiedField { qualifier, field } => format!("{qualifier}.{field}"),
                other => {
                    collect_json_path_base_indices(other, columns, out);
                    return;
                }
            };
            if let Some(idx) = columns.iter().position(|c| *c == name) {
                out.push(idx);
            }
        }
        Expr::BinaryOp(l, _, r) | Expr::Coalesce(l, r) => {
            collect_json_path_base_indices(l, columns, out);
            collect_json_path_base_indices(r, columns, out);
        }
        Expr::UnaryOp(_, i) | Expr::FunctionCall(_, i, _) | Expr::Cast(i, _) => {
            collect_json_path_base_indices(i, columns, out);
        }
        Expr::ScalarFunc(_, args) => {
            for a in args {
                collect_json_path_base_indices(a, columns, out);
            }
        }
        Expr::InList { expr, list, .. } => {
            collect_json_path_base_indices(expr, columns, out);
            for item in list {
                collect_json_path_base_indices(item, columns, out);
            }
        }
        Expr::InSubquery { expr, .. } => collect_json_path_base_indices(expr, columns, out),
        Expr::Case { whens, else_expr } => {
            for (c, r) in whens {
                collect_json_path_base_indices(c, columns, out);
                collect_json_path_base_indices(r, columns, out);
            }
            if let Some(e) = else_expr {
                collect_json_path_base_indices(e, columns, out);
            }
        }
        _ => {}
    }
}

/// Reject a JSON `->` path whose base column is not of type `json` (e.g.
/// `.age->x` on an int column) before any row is produced, so a mistyped path
/// never silently evaluates to the empty set on every row (the "database must
/// not have silent-wrong-answer paths" rule).
///
/// Resolution is deliberately conservative to never reject a VALID query: a
/// base name a `Project` node could redefine (shadowing the scan column), a
/// base that resolves to more than one type across joined tables, or a base
/// that resolves to no scan column at all, is skipped. Such paths fall through
/// to the generic evaluator, which safely yields `Empty` for a non-JSON base.
pub(crate) fn validate_json_path_types(
    catalog: &Catalog,
    plan: &PlanNode,
) -> Result<(), QueryError> {
    let mut scope: Vec<(String, TypeId)> = Vec::new();
    collect_scan_columns(catalog, plan, &mut scope);
    let mut shadowed: std::collections::HashSet<String> = std::collections::HashSet::new();
    collect_projected_names(plan, &mut shadowed);
    check_plan_json_paths(plan, &scope, &shadowed)
}

/// Gather the output column names and types of every scan leaf reachable from
/// `plan`. `SeqScan`/`IndexScan`/`RangeScan` contribute bare column names;
/// `AliasScan` contributes `alias.field` names (the join output shape).
fn collect_scan_columns(catalog: &Catalog, plan: &PlanNode, out: &mut Vec<(String, TypeId)>) {
    match plan {
        PlanNode::SeqScan { table }
        | PlanNode::IndexScan { table, .. }
        | PlanNode::RangeScan { table, .. } => {
            if let Some(schema) = catalog.schema(table) {
                for c in &schema.columns {
                    out.push((c.name.clone(), c.type_id));
                }
            }
        }
        PlanNode::AliasScan { table, alias } => {
            if let Some(schema) = catalog.schema(table) {
                for c in &schema.columns {
                    out.push((format!("{alias}.{}", c.name), c.type_id));
                }
            }
        }
        PlanNode::Filter { input, .. }
        | PlanNode::Project { input, .. }
        | PlanNode::Sort { input, .. }
        | PlanNode::Limit { input, .. }
        | PlanNode::Offset { input, .. }
        | PlanNode::Aggregate { input, .. }
        | PlanNode::Distinct { input }
        | PlanNode::GroupBy { input, .. }
        | PlanNode::Window { input, .. }
        | PlanNode::Update { input, .. }
        | PlanNode::Delete { input, .. }
        | PlanNode::Explain { input } => collect_scan_columns(catalog, input, out),
        PlanNode::NestedLoopJoin { left, right, .. } | PlanNode::Union { left, right, .. } => {
            collect_scan_columns(catalog, left, out);
            collect_scan_columns(catalog, right, out);
        }
        _ => {}
    }
}

/// Collect the output names produced by every `Project` node, so a base name a
/// projection could rebind to a different type is left unvalidated.
fn collect_projected_names(plan: &PlanNode, out: &mut std::collections::HashSet<String>) {
    if let PlanNode::Project { fields, .. } = plan {
        for f in fields {
            if let Some(a) = &f.alias {
                out.insert(a.clone());
            } else {
                match &f.expr {
                    Expr::Field(n) => {
                        out.insert(n.clone());
                    }
                    Expr::QualifiedField { qualifier, field } => {
                        out.insert(format!("{qualifier}.{field}"));
                    }
                    _ => {}
                }
            }
        }
    }
    match plan {
        PlanNode::Filter { input, .. }
        | PlanNode::Project { input, .. }
        | PlanNode::Sort { input, .. }
        | PlanNode::Limit { input, .. }
        | PlanNode::Offset { input, .. }
        | PlanNode::Aggregate { input, .. }
        | PlanNode::Distinct { input }
        | PlanNode::GroupBy { input, .. }
        | PlanNode::Window { input, .. }
        | PlanNode::Update { input, .. }
        | PlanNode::Delete { input, .. }
        | PlanNode::Explain { input } => collect_projected_names(input, out),
        PlanNode::NestedLoopJoin { left, right, .. } | PlanNode::Union { left, right, .. } => {
            collect_projected_names(left, out);
            collect_projected_names(right, out);
        }
        _ => {}
    }
}

/// Resolve `name` in `scope` to a single column type. Returns `None` when the
/// name is absent or resolves to more than one distinct type (ambiguous across
/// joined tables) — both cases are skipped by the caller.
fn resolve_scan_type(name: &str, scope: &[(String, TypeId)]) -> Option<TypeId> {
    let mut found: Option<TypeId> = None;
    for (n, t) in scope {
        if n == name {
            match found {
                None => found = Some(*t),
                Some(prev) if prev == *t => {}
                Some(_) => return None, // ambiguous
            }
        }
    }
    found
}

/// If `base` (the base of a `JsonPath`) resolves to a non-`json` scan column,
/// return a typed error message; otherwise `None`.
fn json_path_base_error(
    base: &Expr,
    scope: &[(String, TypeId)],
    shadowed: &std::collections::HashSet<String>,
) -> Option<String> {
    let name = match base {
        Expr::Field(n) => n.clone(),
        Expr::QualifiedField { qualifier, field } => format!("{qualifier}.{field}"),
        // The parser flattens nested paths, so a JsonPath base is always a
        // Field/QualifiedField; anything else is left to the generic evaluator.
        _ => return None,
    };
    if shadowed.contains(&name) {
        return None;
    }
    match resolve_scan_type(&name, scope) {
        Some(TypeId::Json) | None => None,
        Some(other) => Some(format!(
            "'{}' is a {} column, not json: the '->' path operator requires a json column",
            name,
            type_id_to_name(other)
        )),
    }
}

/// Walk `expr`, validating the base of every `JsonPath` it contains.
fn check_expr_json_paths(
    expr: &Expr,
    scope: &[(String, TypeId)],
    shadowed: &std::collections::HashSet<String>,
) -> Result<(), QueryError> {
    match expr {
        Expr::JsonPath { base, .. } => {
            if let Some(msg) = json_path_base_error(base, scope, shadowed) {
                return Err(QueryError::TypeError(msg));
            }
            check_expr_json_paths(base, scope, shadowed)
        }
        Expr::BinaryOp(l, _, r) | Expr::Coalesce(l, r) => {
            check_expr_json_paths(l, scope, shadowed)?;
            check_expr_json_paths(r, scope, shadowed)
        }
        Expr::UnaryOp(_, inner) | Expr::FunctionCall(_, inner, _) | Expr::Cast(inner, _) => {
            check_expr_json_paths(inner, scope, shadowed)
        }
        Expr::ScalarFunc(_, args) => {
            for a in args {
                check_expr_json_paths(a, scope, shadowed)?;
            }
            Ok(())
        }
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for expr in args.iter().chain(partition_by) {
                check_expr_json_paths(expr, scope, shadowed)?;
            }
            for key in order_by {
                check_expr_json_paths(&key.expr, scope, shadowed)?;
            }
            Ok(())
        }
        Expr::InList { expr, list, .. } => {
            check_expr_json_paths(expr, scope, shadowed)?;
            for item in list {
                check_expr_json_paths(item, scope, shadowed)?;
            }
            Ok(())
        }
        Expr::Case { whens, else_expr } => {
            for (c, r) in whens {
                check_expr_json_paths(c, scope, shadowed)?;
                check_expr_json_paths(r, scope, shadowed)?;
            }
            if let Some(e) = else_expr {
                check_expr_json_paths(e, scope, shadowed)?;
            }
            Ok(())
        }
        // Subquery operands validate their own paths on their own plan; only
        // the outer operand is on this plan's scope.
        Expr::InSubquery { expr, .. } => check_expr_json_paths(expr, scope, shadowed),
        _ => Ok(()),
    }
}

/// Recurse `plan`, validating JSON paths in every expression-bearing field.
fn check_plan_json_paths(
    plan: &PlanNode,
    scope: &[(String, TypeId)],
    shadowed: &std::collections::HashSet<String>,
) -> Result<(), QueryError> {
    match plan {
        PlanNode::Filter { input, predicate } => {
            check_expr_json_paths(predicate, scope, shadowed)?;
            check_plan_json_paths(input, scope, shadowed)
        }
        PlanNode::Project { input, fields } => {
            for f in fields {
                check_expr_json_paths(&f.expr, scope, shadowed)?;
            }
            check_plan_json_paths(input, scope, shadowed)
        }
        PlanNode::GroupBy {
            input,
            keys,
            aggregates,
            having,
        } => {
            for key in keys {
                check_expr_json_paths(&key.expr, scope, shadowed)?;
            }
            for aggregate in aggregates {
                check_expr_json_paths(&aggregate.argument, scope, shadowed)?;
            }
            if let Some(h) = having {
                check_expr_json_paths(h, scope, shadowed)?;
            }
            check_plan_json_paths(input, scope, shadowed)
        }
        PlanNode::NestedLoopJoin {
            left, right, on, ..
        } => {
            if let Some(on) = on {
                check_expr_json_paths(on, scope, shadowed)?;
            }
            check_plan_json_paths(left, scope, shadowed)?;
            check_plan_json_paths(right, scope, shadowed)
        }
        PlanNode::Union { left, right, .. } => {
            check_plan_json_paths(left, scope, shadowed)?;
            check_plan_json_paths(right, scope, shadowed)
        }
        PlanNode::Sort { input, keys } => {
            for key in keys {
                check_expr_json_paths(&key.expr, scope, shadowed)?;
            }
            check_plan_json_paths(input, scope, shadowed)
        }
        PlanNode::Aggregate {
            input, argument, ..
        } => {
            if let Some(argument) = argument {
                check_expr_json_paths(argument, scope, shadowed)?;
            }
            check_plan_json_paths(input, scope, shadowed)
        }
        PlanNode::Window { input, windows } => {
            for window in windows {
                for expr in window.args.iter().chain(&window.partition_by) {
                    check_expr_json_paths(expr, scope, shadowed)?;
                }
                for key in &window.order_by {
                    check_expr_json_paths(&key.expr, scope, shadowed)?;
                }
            }
            check_plan_json_paths(input, scope, shadowed)
        }
        PlanNode::Limit { input, .. }
        | PlanNode::Offset { input, .. }
        | PlanNode::Distinct { input }
        | PlanNode::Update { input, .. }
        | PlanNode::Delete { input, .. }
        | PlanNode::Explain { input } => check_plan_json_paths(input, scope, shadowed),
        _ => Ok(()),
    }
}

/// Reject a reference to a column that exists nowhere in the plan, and a
/// comparison between a column and a literal of an incompatible type.
///
/// `group`, `order` and `insert` already refused unknown columns; `filter` and
/// projections did not, so `User filter .agee > 25` returned the empty set,
/// `User { .agee }` returned a column of NULLs, and `User filter .agee = null
/// delete` matched (and would have deleted) every row. Likewise `.name > 25` on
/// a `str` column compared across types and returned every row.
///
/// Resolution is deliberately conservative so a VALID query is never rejected:
/// a plan with no resolvable scan is skipped entirely, `count(*)`'s `*`
/// sentinel is skipped, and a name a projection or aggregation REBINDS is
/// excluded from the type check (its scan type no longer describes it).
pub(crate) fn validate_column_references(
    catalog: &Catalog,
    plan: &PlanNode,
) -> Result<(), QueryError> {
    let mut scope: Vec<(String, TypeId)> = Vec::new();
    collect_scan_columns(catalog, plan, &mut scope);
    if scope.is_empty() {
        // No scan resolved (DDL, a values-only insert, a view whose source is
        // not in this tree): there is nothing to resolve names against.
        return Ok(());
    }
    // Only aliases and synthetic aggregation outputs ADD a name; an unaliased
    // `.col` in a projection just passes a scan column through, so it must not
    // vouch for itself (that is exactly how `User { .agee }` used to slip past
    // and return a column of NULLs).
    let mut rebound: std::collections::HashSet<String> = std::collections::HashSet::new();
    collect_rebound_names(plan, &mut rebound);
    let mut known: std::collections::HashSet<String> =
        scope.iter().map(|(name, _)| name.clone()).collect();
    // A join's scan columns are named `alias.field`, and an unqualified
    // reference inside a join is resolved by suffix match at runtime, so the
    // bare field name is legitimately known too.
    for (name, _) in &scope {
        if let Some((_, field)) = name.split_once('.') {
            known.insert(field.to_string());
        }
    }
    known.extend(rebound.iter().cloned());
    let ctx = ColumnScope {
        known,
        rebound,
        scope,
    };
    check_plan_columns(plan, &ctx)
}

/// Resolution context for [`validate_column_references`].
struct ColumnScope {
    /// Every name the plan can produce: scan columns, projection outputs, and
    /// synthetic group/aggregate/window output names.
    known: std::collections::HashSet<String>,
    /// Names bound to a computed expression rather than passed through from a
    /// scan, so their scan type must not drive the comparison type check.
    rebound: std::collections::HashSet<String>,
    /// Scan columns with their types.
    scope: Vec<(String, TypeId)>,
}

/// Collect names bound to a computed expression: projection aliases and the
/// synthetic outputs of grouping and window functions. Unlike
/// [`collect_projected_names`] this deliberately skips an unaliased `.col`,
/// which passes the scan column through unchanged and therefore keeps its type.
fn collect_rebound_names(plan: &PlanNode, out: &mut std::collections::HashSet<String>) {
    if let PlanNode::Project { fields, .. } = plan {
        for field in fields {
            if let Some(alias) = &field.alias {
                out.insert(alias.clone());
            }
        }
    }
    match plan {
        PlanNode::GroupBy {
            keys, aggregates, ..
        } => {
            for key in keys {
                out.insert(key.output_name.clone());
            }
            for aggregate in aggregates {
                out.insert(aggregate.output_name.clone());
            }
        }
        PlanNode::Window { windows, .. } => {
            for window in windows {
                out.insert(window.output_name.clone());
            }
        }
        _ => {}
    }
    match plan {
        PlanNode::Filter { input, .. }
        | PlanNode::Project { input, .. }
        | PlanNode::NestedProject { input, .. }
        | PlanNode::Sort { input, .. }
        | PlanNode::Limit { input, .. }
        | PlanNode::Offset { input, .. }
        | PlanNode::Aggregate { input, .. }
        | PlanNode::Distinct { input }
        | PlanNode::GroupBy { input, .. }
        | PlanNode::Window { input, .. }
        | PlanNode::Update { input, .. }
        | PlanNode::Delete { input, .. }
        | PlanNode::Explain { input } => collect_rebound_names(input, out),
        PlanNode::NestedLoopJoin { left, right, .. } | PlanNode::Union { left, right, .. } => {
            collect_rebound_names(left, out);
            collect_rebound_names(right, out);
        }
        _ => {}
    }
}

/// Whether `name` is resolvable in this plan.
fn column_is_known(name: &str, ctx: &ColumnScope) -> bool {
    // `count(*)` carries `*` as a sentinel field name, not a column.
    name == "*" || ctx.known.contains(name)
}

/// Comparability class of a column type. Types whose literal spelling is a
/// string (datetime, uuid, bytes, json) are `Other` and never type-checked
/// here: coercion for those is per-value and lives in the evaluator.
#[derive(PartialEq, Clone, Copy)]
enum TypeClass {
    Numeric,
    Text,
    Bool,
    Other,
}

fn column_class(type_id: TypeId) -> TypeClass {
    match type_id {
        TypeId::Int | TypeId::Float => TypeClass::Numeric,
        TypeId::Str => TypeClass::Text,
        TypeId::Bool => TypeClass::Bool,
        _ => TypeClass::Other,
    }
}

fn literal_class(literal: &Literal) -> TypeClass {
    match literal {
        Literal::Int(_) | Literal::Float(_) => TypeClass::Numeric,
        Literal::String(_) => TypeClass::Text,
        Literal::Bool(_) => TypeClass::Bool,
    }
}

fn literal_type_name(literal: &Literal) -> &'static str {
    match literal {
        Literal::Int(_) => "int",
        Literal::Float(_) => "float",
        Literal::String(_) => "str",
        Literal::Bool(_) => "bool",
    }
}

/// If `expr` is a bare column reference that resolves to exactly one scan type
/// and is not rebound by a projection, return its name and type.
fn comparable_column(expr: &Expr, ctx: &ColumnScope) -> Option<(String, TypeId)> {
    let name = match expr {
        Expr::Field(name) => name.clone(),
        Expr::QualifiedField { qualifier, field } => format!("{qualifier}.{field}"),
        _ => return None,
    };
    if ctx.rebound.contains(&name) {
        return None;
    }
    let type_id = resolve_scan_type(&name, &ctx.scope)?;
    Some((name, type_id))
}

/// Reject `column <cmp> literal` (either orientation) when the two sides
/// cannot compare, e.g. `.name > 25` on a `str` column, which used to return
/// every row. Mirrors the message `insert` already produces for the same class
/// of mistake.
fn comparison_type_error(left: &Expr, right: &Expr, ctx: &ColumnScope) -> Option<String> {
    let (column, literal) = match (left, right) {
        (column, Expr::Literal(literal)) => (column, literal),
        (Expr::Literal(literal), column) => (column, literal),
        _ => return None,
    };
    let (name, type_id) = comparable_column(column, ctx)?;
    let column_class = column_class(type_id);
    let literal_class = literal_class(literal);
    if column_class == TypeClass::Other || column_class == literal_class {
        return None;
    }
    Some(format!(
        "type mismatch for column '{}': expected {:?}, got {}",
        name,
        type_id,
        literal_type_name(literal)
    ))
}

fn check_expr_columns(expr: &Expr, ctx: &ColumnScope) -> Result<(), QueryError> {
    match expr {
        Expr::Field(name) => {
            if !column_is_known(name, ctx) {
                return Err(QueryError::ColumnNotFound {
                    table: String::new(),
                    column: name.clone(),
                });
            }
            Ok(())
        }
        Expr::QualifiedField { qualifier, field } => {
            if !column_is_known(&format!("{qualifier}.{field}"), ctx) {
                return Err(QueryError::ColumnNotFound {
                    table: qualifier.clone(),
                    column: field.clone(),
                });
            }
            Ok(())
        }
        Expr::BinaryOp(left, op, right) => {
            if matches!(
                op,
                BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Gt | BinOp::Lte | BinOp::Gte
            ) {
                if let Some(message) = comparison_type_error(left, right, ctx) {
                    return Err(QueryError::Execution(message));
                }
            }
            check_expr_columns(left, ctx)?;
            check_expr_columns(right, ctx)
        }
        Expr::Coalesce(left, right) => {
            check_expr_columns(left, ctx)?;
            check_expr_columns(right, ctx)
        }
        Expr::UnaryOp(_, inner) | Expr::FunctionCall(_, inner, _) | Expr::Cast(inner, _) => {
            check_expr_columns(inner, ctx)
        }
        Expr::JsonPath { base, .. } => check_expr_columns(base, ctx),
        Expr::ScalarFunc(_, args) => {
            for arg in args {
                check_expr_columns(arg, ctx)?;
            }
            Ok(())
        }
        Expr::InList { expr, list, .. } => {
            check_expr_columns(expr, ctx)?;
            for item in list {
                check_expr_columns(item, ctx)?;
            }
            Ok(())
        }
        Expr::Case { whens, else_expr } => {
            for (when, then) in whens {
                check_expr_columns(when, ctx)?;
                check_expr_columns(then, ctx)?;
            }
            if let Some(else_expr) = else_expr {
                check_expr_columns(else_expr, ctx)?;
            }
            Ok(())
        }
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for expr in args.iter().chain(partition_by) {
                check_expr_columns(expr, ctx)?;
            }
            for key in order_by {
                check_expr_columns(&key.expr, ctx)?;
            }
            Ok(())
        }
        // A subquery resolves against its own plan, which is validated when it
        // executes; only the outer operand belongs to this scope.
        Expr::InSubquery { expr, .. } => check_expr_columns(expr, ctx),
        _ => Ok(()),
    }
}

fn check_plan_columns(plan: &PlanNode, ctx: &ColumnScope) -> Result<(), QueryError> {
    match plan {
        PlanNode::Filter { input, predicate } => {
            check_expr_columns(predicate, ctx)?;
            check_plan_columns(input, ctx)
        }
        PlanNode::Project { input, fields } => {
            for field in fields {
                check_expr_columns(&field.expr, ctx)?;
            }
            check_plan_columns(input, ctx)
        }
        PlanNode::GroupBy {
            input,
            keys,
            aggregates,
            having,
        } => {
            for key in keys {
                check_expr_columns(&key.expr, ctx)?;
            }
            for aggregate in aggregates {
                check_expr_columns(&aggregate.argument, ctx)?;
            }
            if let Some(having) = having {
                check_expr_columns(having, ctx)?;
            }
            check_plan_columns(input, ctx)
        }
        PlanNode::Sort { input, keys } => {
            for key in keys {
                check_expr_columns(&key.expr, ctx)?;
            }
            check_plan_columns(input, ctx)
        }
        PlanNode::Aggregate {
            input, argument, ..
        } => {
            if let Some(argument) = argument {
                check_expr_columns(argument, ctx)?;
            }
            check_plan_columns(input, ctx)
        }
        PlanNode::NestedLoopJoin {
            left, right, on, ..
        } => {
            if let Some(on) = on {
                check_expr_columns(on, ctx)?;
            }
            check_plan_columns(left, ctx)?;
            check_plan_columns(right, ctx)
        }
        PlanNode::Union { left, right, .. } => {
            check_plan_columns(left, ctx)?;
            check_plan_columns(right, ctx)
        }
        // A nested projection resolves child fields against the child table's
        // own scope, which this flat scope does not model; the executor
        // resolves and reports those itself.
        PlanNode::NestedProject { input, .. } => check_plan_columns(input, ctx),
        PlanNode::Limit { input, .. }
        | PlanNode::Offset { input, .. }
        | PlanNode::Distinct { input }
        | PlanNode::Window { input, .. }
        | PlanNode::Update { input, .. }
        | PlanNode::Delete { input, .. }
        | PlanNode::Explain { input } => check_plan_columns(input, ctx),
        _ => Ok(()),
    }
}

/// Reject a negative `limit` / `offset`. Both are cast with `as usize` at
/// execution, so `limit -1` wrapped to `usize::MAX` and silently returned every
/// row instead of erroring.
pub(crate) fn validate_slice_counts(plan: &PlanNode) -> Result<(), QueryError> {
    match plan {
        PlanNode::Limit { input, count } => {
            check_non_negative(count, "limit")?;
            validate_slice_counts(input)
        }
        PlanNode::Offset { input, count } => {
            check_non_negative(count, "offset")?;
            validate_slice_counts(input)
        }
        PlanNode::OrderedExprIndexScan { limit, offset, .. } => {
            check_non_negative(limit, "limit")?;
            if let Some(offset) = offset {
                check_non_negative(offset, "offset")?;
            }
            Ok(())
        }
        PlanNode::Filter { input, .. }
        | PlanNode::Project { input, .. }
        | PlanNode::NestedProject { input, .. }
        | PlanNode::Sort { input, .. }
        | PlanNode::Aggregate { input, .. }
        | PlanNode::Distinct { input }
        | PlanNode::GroupBy { input, .. }
        | PlanNode::Window { input, .. }
        | PlanNode::Update { input, .. }
        | PlanNode::Delete { input, .. }
        | PlanNode::Explain { input } => validate_slice_counts(input),
        PlanNode::NestedLoopJoin { left, right, .. } | PlanNode::Union { left, right, .. } => {
            validate_slice_counts(left)?;
            validate_slice_counts(right)
        }
        _ => Ok(()),
    }
}

fn check_non_negative(count: &Expr, what: &str) -> Result<(), QueryError> {
    match count {
        Expr::Literal(Literal::Int(value)) if *value < 0 => Err(QueryError::Execution(format!(
            "{what} must not be negative, got {value}"
        ))),
        _ => Ok(()),
    }
}

pub(crate) fn validate_no_stray_aggregates(plan: &PlanNode) -> Result<(), QueryError> {
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
        PlanNode::GroupBy {
            input,
            keys,
            aggregates,
            having,
        } => {
            for key in keys {
                check_expr_no_aggregate(&key.expr)?;
            }
            for aggregate in aggregates {
                check_expr_no_aggregate(&aggregate.argument)?;
            }
            if let Some(h) = having {
                check_expr_no_aggregate(h)?;
            }
            validate_no_stray_aggregates(input)?;
        }
        PlanNode::NestedLoopJoin {
            left, right, on, ..
        } => {
            if let Some(on) = on {
                check_expr_no_aggregate(on)?;
            }
            validate_no_stray_aggregates(left)?;
            validate_no_stray_aggregates(right)?;
        }
        PlanNode::Union { left, right, .. } => {
            validate_no_stray_aggregates(left)?;
            validate_no_stray_aggregates(right)?;
        }
        PlanNode::Sort { input, keys } => {
            for key in keys {
                check_expr_no_aggregate(&key.expr)?;
            }
            validate_no_stray_aggregates(input)?;
        }
        PlanNode::Aggregate {
            input, argument, ..
        } => {
            if let Some(argument) = argument {
                check_expr_no_aggregate(argument)?;
            }
            validate_no_stray_aggregates(input)?;
        }
        PlanNode::Window { input, windows } => {
            for window in windows {
                for expr in window.args.iter().chain(&window.partition_by) {
                    check_expr_no_aggregate(expr)?;
                }
                for key in &window.order_by {
                    check_expr_no_aggregate(&key.expr)?;
                }
            }
            validate_no_stray_aggregates(input)?;
        }
        PlanNode::Limit { input, .. }
        | PlanNode::Offset { input, .. }
        | PlanNode::Distinct { input }
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
        Expr::FunctionCall(..) => Err(QueryError::Execution(
            "invalid query: aggregate function in an unsupported position".to_string(),
        )),
        Expr::BinaryOp(l, _, r) | Expr::Coalesce(l, r) => {
            check_expr_no_aggregate(l)?;
            check_expr_no_aggregate(r)
        }
        Expr::UnaryOp(_, inner) | Expr::Cast(inner, _) | Expr::JsonPath { base: inner, .. } => {
            check_expr_no_aggregate(inner)
        }
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
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for expr in args.iter().chain(partition_by) {
                check_expr_no_aggregate(expr)?;
            }
            for key in order_by {
                check_expr_no_aggregate(&key.expr)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
