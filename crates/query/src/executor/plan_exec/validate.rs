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
