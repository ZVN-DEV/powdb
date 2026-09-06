//! Typed-literal coercion: rewrite the literal side of a comparison against a
//! `datetime`, `uuid` or `bytes` column into that column's own `Value` before
//! the plan runs.
//!
//! Those three types have no literal spelling in PowQL, so every reference to
//! one is written as the string (or, for `datetime`, the integer) that `insert`
//! accepts. The insert path coerces it (`eval::coerce_value`); the comparison
//! path did not, and `Value`'s equality is strictly typed, so
//! `.u = "<its exact uuid>"` was false on every row, `!=` true on every row,
//! `>` true on every row, `update`/`delete` by uuid key touched nothing, and
//! the index was never probed because the raw string key could not address the
//! uuid byte lane. That reached every frontend at once: PowQL, SQL, `$1`
//! parameters, prepared statements and the wire.
//!
//! Coercing here rather than in the evaluator is what makes the rule general.
//! This pass runs once, inside [`super::LoweredPlan::of`], which is the single
//! boundary every plan crosses on its way into execution, and it rewrites the
//! literal in the plan itself: the scan, the compiled predicate, the index
//! probe and the mutation's discovery scan then all read the same typed value,
//! and a literal that cannot be coerced is a typed error naming the column and
//! the expected format instead of a silently empty result.

use super::*;
use crate::executor::eval::coerce_value;
use crate::result::QueryError;
use powdb_storage::catalog::Catalog;

use super::validate::{collect_rebound_names, collect_scan_columns, resolve_scan_type};

/// Rewrite every comparison literal that addresses a `datetime`/`uuid`/`bytes`
/// column into that column's `Value`. Returns the rewritten plan, or the typed
/// error the same literal would raise on `insert`.
pub(crate) fn coerce_typed_literals(
    catalog: &Catalog,
    plan: &PlanNode,
) -> Result<PlanNode, QueryError> {
    let mut scope: Vec<(String, TypeId)> = Vec::new();
    collect_scan_columns(catalog, plan, &mut scope);
    let mut rebound = std::collections::HashSet::new();
    let mut computed = std::collections::HashSet::new();
    collect_rebound_names(plan, &mut rebound, &mut computed);
    let ctx = TypedScope {
        catalog,
        scope,
        rebound,
    };
    coerce_plan(plan, &ctx)
}

/// The types whose PowQL literal spelling is a string (or, for `datetime`, an
/// integer): the ones a comparison has to coerce because `Value` equality is
/// strictly typed and no literal ever carries the right variant.
fn is_string_spelled(type_id: TypeId) -> bool {
    matches!(type_id, TypeId::DateTime | TypeId::Uuid | TypeId::Bytes)
}

/// Resolution context: the scan columns in scope and the names a projection or
/// aggregation rebinds (whose scan type no longer describes them).
struct TypedScope<'a> {
    catalog: &'a Catalog,
    scope: Vec<(String, TypeId)>,
    rebound: std::collections::HashSet<String>,
}

impl TypedScope<'_> {
    /// The declared type of `expr` when it is a bare column reference that
    /// resolves to exactly one string-spelled scan type.
    fn typed_column(&self, expr: &Expr) -> Option<(String, TypeId)> {
        let name = match expr {
            Expr::Field(name) => name.clone(),
            Expr::QualifiedField { qualifier, field } => format!("{qualifier}.{field}"),
            _ => return None,
        };
        if self.rebound.contains(&name) {
            return None;
        }
        let type_id = resolve_scan_type(&name, &self.scope)?;
        is_string_spelled(type_id).then_some((name, type_id))
    }
}

/// Coerce `literal` to `type_id` exactly as `insert` would, and wrap it as a
/// value literal the whole executor reads uniformly.
fn coerce_literal(name: &str, type_id: TypeId, literal: &Literal) -> Result<Expr, QueryError> {
    // A timestamp literal is the raw micros integer, and `Value::Ord` already
    // names the Int/DateTime pair, so an int against a datetime column compares
    // correctly on the scan and compiles to an int leaf. Leaving it alone keeps
    // that fast path; rewriting it to a real `DateTime` would also start
    // probing the datetime B-tree, whose keys were written from the `Int` the
    // insert path stores and therefore live in the Int lane. Only the spellings
    // that are silently false today are rewritten or refused.
    if type_id == TypeId::DateTime && matches!(literal, Literal::Int(_)) {
        return Ok(Expr::Literal(literal.clone()));
    }
    let column = ColumnDef {
        name: name.to_string(),
        type_id,
        required: false,
        position: 0,
    };
    let value = match literal {
        Literal::Int(v) => Value::Int(*v),
        Literal::Float(v) => Value::Float(*v),
        Literal::Bool(v) => Value::Bool(*v),
        Literal::String(s) => Value::Str(s.clone()),
    };
    coerce_value(value, &column)
        .map(Expr::ValueLit)
        .map_err(QueryError::Execution)
}

/// Coerce one `(column, literal)` operand pair of an index probe or range
/// bound, resolving the column type from the catalog rather than the plan
/// scope (these nodes name their own table and column).
fn coerce_probe_key(
    catalog: &Catalog,
    table: &str,
    column: &str,
    key: &Expr,
) -> Result<Expr, QueryError> {
    let Expr::Literal(literal) = key else {
        return Ok(key.clone());
    };
    let Some(type_id) = catalog
        .schema(table)
        .and_then(|schema| schema.find_column(column))
        .map(|column| column.type_id)
    else {
        return Ok(key.clone());
    };
    if !is_string_spelled(type_id) {
        return Ok(key.clone());
    }
    coerce_literal(column, type_id, literal)
}

fn coerce_bound(
    catalog: &Catalog,
    table: &str,
    column: &str,
    bound: &Option<(Expr, bool)>,
) -> Result<Option<(Expr, bool)>, QueryError> {
    match bound {
        None => Ok(None),
        Some((expr, inclusive)) => Ok(Some((
            coerce_probe_key(catalog, table, column, expr)?,
            *inclusive,
        ))),
    }
}

/// Comparison operators whose operands must agree in type. `like` is excluded:
/// its right operand is a pattern, not a value of the column's type.
fn is_typed_comparison(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Gt | BinOp::Lte | BinOp::Gte
    )
}

fn coerce_expr(expr: &Expr, ctx: &TypedScope) -> Result<Expr, QueryError> {
    match expr {
        Expr::BinaryOp(left, op, right) if is_typed_comparison(*op) => {
            if let (Some((name, type_id)), Expr::Literal(literal)) =
                (ctx.typed_column(left), right.as_ref())
            {
                return Ok(Expr::BinaryOp(
                    left.clone(),
                    *op,
                    Box::new(coerce_literal(&name, type_id, literal)?),
                ));
            }
            if let (Some((name, type_id)), Expr::Literal(literal)) =
                (ctx.typed_column(right), left.as_ref())
            {
                return Ok(Expr::BinaryOp(
                    Box::new(coerce_literal(&name, type_id, literal)?),
                    *op,
                    right.clone(),
                ));
            }
            Ok(Expr::BinaryOp(
                Box::new(coerce_expr(left, ctx)?),
                *op,
                Box::new(coerce_expr(right, ctx)?),
            ))
        }
        Expr::BinaryOp(left, op, right) => Ok(Expr::BinaryOp(
            Box::new(coerce_expr(left, ctx)?),
            *op,
            Box::new(coerce_expr(right, ctx)?),
        )),
        Expr::InList {
            expr: subject,
            list,
            negated,
        } => {
            let typed = ctx.typed_column(subject);
            let mut coerced = Vec::with_capacity(list.len());
            for item in list {
                match (&typed, item) {
                    (Some((name, type_id)), Expr::Literal(literal)) => {
                        coerced.push(coerce_literal(name, *type_id, literal)?);
                    }
                    _ => coerced.push(coerce_expr(item, ctx)?),
                }
            }
            Ok(Expr::InList {
                expr: Box::new(coerce_expr(subject, ctx)?),
                list: coerced,
                negated: *negated,
            })
        }
        Expr::UnaryOp(op, inner) => Ok(Expr::UnaryOp(*op, Box::new(coerce_expr(inner, ctx)?))),
        Expr::Coalesce(left, right) => Ok(Expr::Coalesce(
            Box::new(coerce_expr(left, ctx)?),
            Box::new(coerce_expr(right, ctx)?),
        )),
        Expr::Case { whens, else_expr } => {
            let mut coerced = Vec::with_capacity(whens.len());
            for (when, then) in whens {
                coerced.push((
                    Box::new(coerce_expr(when, ctx)?),
                    Box::new(coerce_expr(then, ctx)?),
                ));
            }
            Ok(Expr::Case {
                whens: coerced,
                else_expr: match else_expr {
                    Some(inner) => Some(Box::new(coerce_expr(inner, ctx)?)),
                    None => None,
                },
            })
        }
        other => Ok(other.clone()),
    }
}

fn coerce_plan(plan: &PlanNode, ctx: &TypedScope) -> Result<PlanNode, QueryError> {
    Ok(match plan {
        PlanNode::IndexScan { table, column, key } => PlanNode::IndexScan {
            table: table.clone(),
            column: column.clone(),
            key: coerce_probe_key(ctx.catalog, table, column, key)?,
        },
        PlanNode::RangeScan {
            table,
            column,
            start,
            end,
        } => PlanNode::RangeScan {
            table: table.clone(),
            column: column.clone(),
            start: coerce_bound(ctx.catalog, table, column, start)?,
            end: coerce_bound(ctx.catalog, table, column, end)?,
        },
        PlanNode::Filter { input, predicate } => PlanNode::Filter {
            input: Box::new(coerce_plan(input, ctx)?),
            predicate: coerce_expr(predicate, ctx)?,
        },
        PlanNode::Project { input, fields } => {
            let mut coerced = Vec::with_capacity(fields.len());
            for field in fields {
                coerced.push(ProjectField {
                    alias: field.alias.clone(),
                    expr: coerce_expr(&field.expr, ctx)?,
                });
            }
            PlanNode::Project {
                input: Box::new(coerce_plan(input, ctx)?),
                fields: coerced,
            }
        }
        PlanNode::Sort { input, keys } => {
            let mut coerced = Vec::with_capacity(keys.len());
            for key in keys {
                coerced.push(SortKey {
                    expr: coerce_expr(&key.expr, ctx)?,
                    descending: key.descending,
                });
            }
            PlanNode::Sort {
                input: Box::new(coerce_plan(input, ctx)?),
                keys: coerced,
            }
        }
        PlanNode::GroupBy {
            input,
            keys,
            aggregates,
            having,
        } => {
            let mut coerced_keys = Vec::with_capacity(keys.len());
            for key in keys {
                coerced_keys.push(GroupKey {
                    expr: coerce_expr(&key.expr, ctx)?,
                    output_name: key.output_name.clone(),
                });
            }
            let mut coerced_aggs = Vec::with_capacity(aggregates.len());
            for aggregate in aggregates {
                coerced_aggs.push(GroupAgg {
                    function: aggregate.function,
                    argument: coerce_expr(&aggregate.argument, ctx)?,
                    mode: aggregate.mode,
                    provenance_alias: aggregate.provenance_alias.clone(),
                    output_name: aggregate.output_name.clone(),
                });
            }
            PlanNode::GroupBy {
                input: Box::new(coerce_plan(input, ctx)?),
                keys: coerced_keys,
                aggregates: coerced_aggs,
                having: match having {
                    Some(having) => Some(coerce_expr(having, ctx)?),
                    None => None,
                },
            }
        }
        PlanNode::Aggregate {
            input,
            function,
            argument,
            mode,
            provenance_alias,
        } => PlanNode::Aggregate {
            input: Box::new(coerce_plan(input, ctx)?),
            function: *function,
            argument: match argument {
                Some(argument) => Some(coerce_expr(argument, ctx)?),
                None => None,
            },
            mode: *mode,
            provenance_alias: provenance_alias.clone(),
        },
        PlanNode::NestedLoopJoin {
            left,
            right,
            on,
            kind,
        } => PlanNode::NestedLoopJoin {
            left: Box::new(coerce_plan(left, ctx)?),
            right: Box::new(coerce_plan(right, ctx)?),
            on: match on {
                Some(on) => Some(coerce_expr(on, ctx)?),
                None => None,
            },
            kind: *kind,
        },
        // Each branch of a union resolves names against its own row, so each
        // gets its own scope rather than the merged one.
        PlanNode::Union { left, right, all } => PlanNode::Union {
            left: Box::new(coerce_typed_literals(ctx.catalog, left)?),
            right: Box::new(coerce_typed_literals(ctx.catalog, right)?),
            all: *all,
        },
        PlanNode::Limit { input, count } => PlanNode::Limit {
            input: Box::new(coerce_plan(input, ctx)?),
            count: count.clone(),
        },
        PlanNode::Offset { input, count } => PlanNode::Offset {
            input: Box::new(coerce_plan(input, ctx)?),
            count: count.clone(),
        },
        PlanNode::Distinct { input } => PlanNode::Distinct {
            input: Box::new(coerce_plan(input, ctx)?),
        },
        PlanNode::Window { input, windows } => PlanNode::Window {
            input: Box::new(coerce_plan(input, ctx)?),
            windows: windows.clone(),
        },
        PlanNode::Update {
            input,
            table,
            assignments,
            returning,
        } => PlanNode::Update {
            input: Box::new(coerce_plan(input, ctx)?),
            table: table.clone(),
            assignments: assignments.clone(),
            returning: *returning,
        },
        PlanNode::Delete {
            input,
            table,
            returning,
        } => PlanNode::Delete {
            input: Box::new(coerce_plan(input, ctx)?),
            table: table.clone(),
            returning: *returning,
        },
        PlanNode::Explain { input } => PlanNode::Explain {
            input: Box::new(coerce_plan(input, ctx)?),
        },
        // A nested projection resolves its child fields against the child
        // table's own scope, which this flat scope does not model; only the
        // parent pipeline is rewritten here.
        PlanNode::NestedProject { input, fields } => PlanNode::NestedProject {
            input: Box::new(coerce_plan(input, ctx)?),
            fields: fields.clone(),
        },
        other => other.clone(),
    })
}
