use crate::ast::*;
use crate::parser::{parse, ParseError};
use crate::plan::*;
use powdb_storage::stored_json_path::StoredJsonPathV1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RangeTarget {
    Column(String),
    JsonPath(StoredJsonPathV1),
}

/// (target, lower_bound, upper_bound) — used by range-index extraction.
pub(crate) type RangeBound = (RangeTarget, Option<(Expr, bool)>, Option<(Expr, bool)>);

/// Plan-phase error — wraps ParseError for the full lex→parse→plan chain.
#[derive(Debug)]
pub enum PlanError {
    /// Error originated in the parser (or lexer, via ParseError::Lex).
    Parse(ParseError),
    /// The parsed query is structurally valid but cannot be planned safely.
    Semantic(String),
}

impl PlanError {
    /// Convenience: human-readable message for any variant.
    pub fn message(&self) -> String {
        self.to_string()
    }
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "{e}"),
            Self::Semantic(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for PlanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(e) => Some(e),
            Self::Semantic(_) => None,
        }
    }
}

impl From<ParseError> for PlanError {
    fn from(e: ParseError) -> Self {
        PlanError::Parse(e)
    }
}

pub fn plan(input: &str) -> Result<PlanNode, PlanError> {
    let stmt = parse(input)?;
    plan_statement(stmt)
}

pub fn plan_statement(stmt: Statement) -> Result<PlanNode, PlanError> {
    match stmt {
        Statement::Query(q) => plan_query(q),
        Statement::Insert(ins) => plan_insert(ins),
        Statement::UpdateQuery(upd) => plan_update(upd),
        Statement::DeleteQuery(del) => plan_delete(del),
        Statement::CreateType(ct) => plan_create_type(ct),
        Statement::AlterTable(at) => Ok(PlanNode::AlterTable {
            table: at.table,
            action: at.action,
        }),
        Statement::DropTable(dt) => Ok(PlanNode::DropTable {
            name: dt.table,
            if_exists: dt.if_exists,
        }),
        Statement::CreateView(cv) => Ok(PlanNode::CreateView {
            name: cv.name,
            query_text: cv.query_text,
        }),
        Statement::RefreshView(rv) => Ok(PlanNode::RefreshView { name: rv.name }),
        Statement::DropView(dv) => Ok(PlanNode::DropView {
            name: dv.name,
            if_exists: dv.if_exists,
        }),
        Statement::ListTypes => Ok(PlanNode::ListTypes),
        Statement::Describe(table) => Ok(PlanNode::Describe { table }),
        Statement::Union(u) => {
            let left = plan_statement(*u.left)?;
            let right = plan_statement(*u.right)?;
            Ok(PlanNode::Union {
                left: Box::new(left),
                right: Box::new(right),
                all: u.all,
            })
        }
        Statement::Upsert(ups) => plan_upsert(ups),
        Statement::Begin => Ok(PlanNode::Begin),
        Statement::Commit => Ok(PlanNode::Commit),
        Statement::Rollback => Ok(PlanNode::Rollback),
        Statement::Explain(inner) => {
            let inner_plan = plan_statement(*inner)?;
            Ok(PlanNode::Explain {
                input: Box::new(inner_plan),
            })
        }
    }
}

fn plan_query(mut q: QueryExpr) -> Result<PlanNode, PlanError> {
    // Language-lab slice: projections carrying a nested sub-query field take
    // a dedicated path so every other query shape plans exactly as before.
    if q.projection.as_ref().is_some_and(|proj| {
        proj.iter()
            .any(|pf| matches!(pf.expr, Expr::NestedQuery(_)))
    }) {
        return plan_nested_query(q);
    }
    // Mission E1.2: if the query has joins, build a left-deep nested-loop
    // plan. Correctness first — hash-join optimization is E1.3. We also
    // don't try to fold an IndexScan under a joined query yet (the
    // leaf-level fast paths all match on `PlanNode::SeqScan { .. }`
    // literally, so mixing them into a join plan would silently break).
    if !q.joins.is_empty() {
        return plan_joined_query(q);
    }
    let source_aliases = std::collections::HashSet::from([q.source.clone()]);
    // Try to fold `filter .col = literal` into an IndexScan. The executor
    // decides at run time whether the column actually has an index — if not,
    // it transparently falls back to a sequential scan with the same predicate,
    // so this rewrite is always safe.
    //
    // We only rewrite the *simple* eq case here: `filter .col = literal`.
    // A conjunction like `filter .col = 1 and .other > 5` stays as
    // SeqScan + Filter in the planner; runtime lowering
    // (`lower_unindexed_scans`) then picks an indexed conjunct to drive the
    // scan and re-checks the rest as a residual filter, using real catalog
    // knowledge the pure planner does not have.
    let ordered_expr_scan = try_extract_ordered_expr_index_scan(&q);
    let (source, filter) = if let Some(scan) = ordered_expr_scan {
        // The ordered expression node owns these clauses and executes them in
        // index order. Clear them so the generic pipeline does not wrap a
        // second Sort/Offset/Limit around the speculative node.
        q.order = None;
        q.limit = None;
        q.offset = None;
        (scan, None)
    } else {
        match q.filter {
            Some(pred) => match try_extract_eq_index_key(&q.source, &pred) {
                Some(index_scan) => (index_scan, None),
                None => match try_extract_range_index_keys(&q.source, &pred) {
                    Some(range_scan) => (range_scan, None),
                    None => (
                        PlanNode::SeqScan {
                            table: q.source.clone(),
                        },
                        Some(pred),
                    ),
                },
            },
            None => (
                PlanNode::SeqScan {
                    table: q.source.clone(),
                },
                None,
            ),
        }
    };
    let mut node = source;

    if let Some(pred) = filter {
        node = PlanNode::Filter {
            input: Box::new(node),
            predicate: pred,
        };
    }

    // Mission E2b: GROUP BY path — insert GroupBy + Project before
    // order/limit/offset/distinct.
    if let Some(group) = q.group_by {
        let mut grouped_order = q.order;
        let mut proj_fields: Vec<ProjectField> = q
            .projection
            .map(|proj| {
                proj.into_iter()
                    .map(|pf| ProjectField {
                        alias: pf.alias,
                        expr: pf.expr,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut having = group.having;
        let aggregates = extract_aggregates(&mut proj_fields, &mut having, &source_aliases)?;
        rewrite_group_order_keys(grouped_order.as_mut(), &proj_fields, &group.keys);
        rewrite_group_key_references(&mut proj_fields, &mut having, &group.keys);

        node = PlanNode::GroupBy {
            input: Box::new(node),
            keys: group.keys,
            aggregates,
            having,
        };

        if !proj_fields.is_empty() {
            node = PlanNode::Project {
                input: Box::new(node),
                fields: proj_fields,
            };
        }

        if let Some(order) = grouped_order {
            node = PlanNode::Sort {
                input: Box::new(node),
                keys: order
                    .keys
                    .into_iter()
                    .map(|k| SortKey {
                        expr: k.expr,
                        descending: k.descending,
                    })
                    .collect(),
            };
        }
        // Offset must be applied *before* Limit: skip M rows, then take N.
        // Plan shape is Limit(Offset(...)), so Offset is built first (inner)
        // and Limit wraps it (outer).
        if let Some(off) = q.offset {
            node = PlanNode::Offset {
                input: Box::new(node),
                count: off,
            };
        }
        if let Some(lim) = q.limit {
            node = PlanNode::Limit {
                input: Box::new(node),
                count: lim,
            };
        }
        if q.distinct {
            node = PlanNode::Distinct {
                input: Box::new(node),
            };
        }
        return Ok(node);
    }

    if let Some(order) = q.order {
        node = PlanNode::Sort {
            input: Box::new(node),
            keys: order
                .keys
                .into_iter()
                .map(|k| SortKey {
                    expr: k.expr,
                    descending: k.descending,
                })
                .collect(),
        };
    }

    // Offset must be applied *before* Limit: skip M rows, then take N.
    // Plan shape is Limit(Offset(...)), so Offset is built first (inner)
    // and Limit wraps it (outer).
    if let Some(off) = q.offset {
        node = PlanNode::Offset {
            input: Box::new(node),
            count: off,
        };
    }

    if let Some(lim) = q.limit {
        node = PlanNode::Limit {
            input: Box::new(node),
            count: lim,
        };
    }

    if let Some(proj) = q.projection {
        let mut fields: Vec<ProjectField> = proj
            .into_iter()
            .map(|pf| ProjectField {
                alias: pf.alias,
                expr: pf.expr,
            })
            .collect();
        let windows = extract_windows(&mut fields);
        if !windows.is_empty() {
            node = PlanNode::Window {
                input: Box::new(node),
                windows,
            };
        }
        node = PlanNode::Project {
            input: Box::new(node),
            fields,
        };
    }

    if q.distinct {
        node = PlanNode::Distinct {
            input: Box::new(node),
        };
    }

    if let Some(agg) = q.aggregation {
        let provenance_alias = symmetric_provenance_alias(
            agg.function,
            agg.argument.as_ref(),
            agg.mode,
            &source_aliases,
        )?;
        node = PlanNode::Aggregate {
            input: Box::new(node),
            function: agg.function,
            argument: agg.argument,
            mode: agg.mode,
            provenance_alias,
        };
    }

    Ok(node)
}

/// Build a `NestedProject` plan for a query whose projection carries nested
/// sub-query fields (language-lab slice). The parent pipeline is an
/// `AliasScan` (so `alias.field` references resolve by column name) plus the
/// usual filter/order/offset/limit stack; the projection itself becomes the
/// `NestedProject` layer. Emitted speculatively like `RangeScan`: the planner
/// stays catalog-pure and the executor resolves tables/columns at run time.
fn plan_nested_query(q: QueryExpr) -> Result<PlanNode, PlanError> {
    if !q.joins.is_empty() || q.group_by.is_some() || q.aggregation.is_some() || q.distinct {
        return Err(PlanError::Semantic(
            "nested projections require a plain aliased table scan (no joins, \
             group, distinct, or aggregation)"
                .into(),
        ));
    }
    let parent_alias = q.alias.unwrap_or_else(|| q.source.clone());
    let mut node = PlanNode::AliasScan {
        table: q.source,
        alias: parent_alias.clone(),
    };
    if let Some(pred) = q.filter {
        node = PlanNode::Filter {
            input: Box::new(node),
            predicate: pred,
        };
    }
    if let Some(order) = q.order {
        node = PlanNode::Sort {
            input: Box::new(node),
            keys: order
                .keys
                .into_iter()
                .map(|k| SortKey {
                    expr: k.expr,
                    descending: k.descending,
                })
                .collect(),
        };
    }
    if let Some(off) = q.offset {
        node = PlanNode::Offset {
            input: Box::new(node),
            count: off,
        };
    }
    if let Some(lim) = q.limit {
        node = PlanNode::Limit {
            input: Box::new(node),
            count: lim,
        };
    }
    let fields = q
        .projection
        .expect("plan_nested_query is only called with a projection")
        .into_iter()
        .map(|pf| match pf.expr {
            Expr::NestedQuery(nested) => {
                let name = pf.alias.ok_or_else(|| {
                    PlanError::Semantic("nested projection field requires a name".into())
                })?;
                resolve_nested_projection(name, *nested, &parent_alias)
                    .map(NestedProjectField::Nested)
            }
            expr => Ok(NestedProjectField::Plain(ProjectField {
                alias: pf.alias,
                expr,
            })),
        })
        .collect::<Result<Vec<_>, PlanError>>()?;
    Ok(PlanNode::NestedProject {
        input: Box::new(node),
        fields,
    })
}

/// Split a parsed `NestedQuery` into the resolved `NestedProjection` form.
/// The filter's AND chain must contain exactly one equi-correlation
/// predicate `child.col = outer.col` (either side order, any position);
/// the remaining conjuncts become the residual filter, rewritten to bare
/// child columns and evaluated per child row by the executor.
fn resolve_nested_projection(
    name: String,
    nested: NestedQuery,
    parent_alias: &str,
) -> Result<NestedProjection, PlanError> {
    resolve_nested_projection_inner(name, nested, parent_alias, true)
}

/// `qualify_parent_key`: at the top level the parent pipeline is an
/// `AliasScan` whose columns are `alias.field`-qualified; deeper levels
/// correlate against the enclosing child table's bare schema columns.
fn resolve_nested_projection_inner(
    name: String,
    nested: NestedQuery,
    parent_alias: &str,
    qualify_parent_key: bool,
) -> Result<NestedProjection, PlanError> {
    let mut conjuncts = Vec::new();
    split_and_chain(nested.filter, &mut conjuncts);

    // A correlation conjunct is `child.col = parent.col` (either side order).
    let correlation_of = |expr: &Expr| -> Option<(String, String)> {
        let Expr::BinaryOp(left, BinOp::Eq, right) = expr else {
            return None;
        };
        let side = |expr: &Expr| match expr {
            Expr::QualifiedField { qualifier, field } => Some((qualifier.clone(), field.clone())),
            _ => None,
        };
        let ((lq, lf), (rq, rf)) = (side(left)?, side(right)?);
        if lq == nested.alias && rq == parent_alias {
            Some((lf, rf))
        } else if rq == nested.alias && lq == parent_alias {
            Some((rf, lf))
        } else {
            None
        }
    };
    let mut correlation: Option<(String, String)> = None;
    let mut residual: Option<Expr> = None;
    for conjunct in conjuncts {
        match correlation_of(&conjunct) {
            Some(keys) if correlation.is_none() => correlation = Some(keys),
            Some(_) => {
                return Err(PlanError::Semantic(format!(
                    "nested projection `{name}` links `{child}` to `{parent}` more than \
                     once; exactly one correlation predicate \
                     ({child}.<col> = {parent}.<col>) is supported",
                    child = nested.alias,
                    parent = parent_alias,
                )))
            }
            None => {
                let rewritten =
                    rewrite_residual_condition(conjunct, &name, &nested.alias, parent_alias)?;
                residual = Some(match residual {
                    Some(existing) => {
                        Expr::BinaryOp(Box::new(existing), BinOp::And, Box::new(rewritten))
                    }
                    None => rewritten,
                });
            }
        }
    }
    let Some((child_key, parent_key)) = correlation else {
        return Err(PlanError::Semantic(format!(
            "nested projection `{name}` requires an equi-correlation predicate linking \
             `{child}` to the outer query ({child}.<col> = {parent}.<col>) somewhere in \
             its filter",
            child = nested.alias,
            parent = parent_alias,
        )));
    };
    let order = nested
        .order
        .map(|clause| {
            clause
                .keys
                .into_iter()
                .map(|key| {
                    let column = match &key.expr {
                        Expr::Field(field) => field.clone(),
                        Expr::QualifiedField { qualifier, field } if *qualifier == nested.alias => {
                            field.clone()
                        }
                        _ => {
                            return Err(PlanError::Semantic(format!(
                                "nested projection `{name}` order keys must be plain \
                                 columns of `{child}` (`{child}.<col>` or `.<col>`)",
                                child = nested.alias,
                            )))
                        }
                    };
                    Ok((column, key.descending))
                })
                .collect::<Result<Vec<_>, PlanError>>()
        })
        .transpose()?
        .unwrap_or_default();
    let fields = nested
        .fields
        .into_iter()
        .map(|pf| {
            if let Expr::NestedQuery(inner) = pf.expr {
                let inner_name = pf.alias.ok_or_else(|| {
                    PlanError::Semantic(
                        "nested projection field requires a name \
                         (`<name>: <Table> as <alias> ...`)"
                            .into(),
                    )
                })?;
                // The enclosing child scan exposes bare schema columns, so
                // deeper levels correlate on an unqualified parent key.
                return resolve_nested_projection_inner(inner_name, *inner, &nested.alias, false)
                    .map(NestedField::Nested);
            }
            let column = match &pf.expr {
                Expr::Field(field) => field.clone(),
                Expr::QualifiedField { qualifier, field } if *qualifier == nested.alias => {
                    field.clone()
                }
                _ => {
                    return Err(PlanError::Semantic(format!(
                        "nested projection `{name}` fields must be plain columns of `{}` \
                         (`{}.<col>` or `.<col>`) or a deeper nested projection",
                        nested.alias, nested.alias
                    )))
                }
            };
            let key = pf.alias.unwrap_or_else(|| column.clone());
            Ok(NestedField::Scalar { key, column })
        })
        .collect::<Result<Vec<_>, PlanError>>()?;
    Ok(NestedProjection {
        name,
        table: nested.source,
        alias: nested.alias,
        parent_alias: parent_alias.to_string(),
        child_key,
        parent_key: if qualify_parent_key {
            format!("{parent_alias}.{parent_key}")
        } else {
            parent_key
        },
        residual,
        order,
        limit: nested.limit,
        offset: nested.offset,
        offset_before_limit: nested.offset_before_limit,
        fields,
    })
}

/// Flatten a left-associative AND chain into its conjuncts, in source order.
fn split_and_chain(expr: Expr, out: &mut Vec<Expr>) {
    match expr {
        Expr::BinaryOp(left, BinOp::And, right) => {
            split_and_chain(*left, out);
            split_and_chain(*right, out);
        }
        other => out.push(other),
    }
}

/// Rewrite one residual conjunct of a nested projection filter so it
/// evaluates against the bare child schema: `child.col` becomes `col`.
/// Rejects references to the outer alias (only the correlation predicate
/// may cross scopes) and constructs the executor cannot evaluate per child
/// row (subqueries, aggregates, window functions, further nesting).
fn rewrite_residual_condition(
    expr: Expr,
    name: &str,
    child_alias: &str,
    parent_alias: &str,
) -> Result<Expr, PlanError> {
    let rewrite = |inner: Box<Expr>| -> Result<Box<Expr>, PlanError> {
        Ok(Box::new(rewrite_residual_condition(
            *inner,
            name,
            child_alias,
            parent_alias,
        )?))
    };
    match expr {
        Expr::QualifiedField { qualifier, field } => {
            if qualifier == child_alias {
                Ok(Expr::Field(field))
            } else if qualifier == parent_alias {
                Err(PlanError::Semantic(format!(
                    "nested projection `{name}` filter references outer alias \
                     `{parent_alias}` (`{parent_alias}.{field}`) outside the correlation \
                     predicate; move that condition to the outer query's filter"
                )))
            } else {
                Err(PlanError::Semantic(format!(
                    "nested projection `{name}` filter references unknown alias \
                     `{qualifier}`; only columns of `{child_alias}` may be used"
                )))
            }
        }
        Expr::Field(_) | Expr::Literal(_) | Expr::Param(_) | Expr::ValueLit(_) | Expr::Null => {
            Ok(expr)
        }
        Expr::BinaryOp(left, op, right) => Ok(Expr::BinaryOp(rewrite(left)?, op, rewrite(right)?)),
        Expr::UnaryOp(op, inner) => Ok(Expr::UnaryOp(op, rewrite(inner)?)),
        Expr::Coalesce(left, right) => Ok(Expr::Coalesce(rewrite(left)?, rewrite(right)?)),
        Expr::Cast(inner, ty) => Ok(Expr::Cast(rewrite(inner)?, ty)),
        Expr::ScalarFunc(func, args) => Ok(Expr::ScalarFunc(
            func,
            args.into_iter()
                .map(|arg| rewrite_residual_condition(arg, name, child_alias, parent_alias))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        Expr::InList {
            expr,
            list,
            negated,
        } => Ok(Expr::InList {
            expr: rewrite(expr)?,
            list: list
                .into_iter()
                .map(|item| rewrite_residual_condition(item, name, child_alias, parent_alias))
                .collect::<Result<Vec<_>, _>>()?,
            negated,
        }),
        Expr::Case { whens, else_expr } => Ok(Expr::Case {
            whens: whens
                .into_iter()
                .map(|(cond, result)| Ok((rewrite(cond)?, rewrite(result)?)))
                .collect::<Result<Vec<_>, PlanError>>()?,
            else_expr: else_expr.map(rewrite).transpose()?,
        }),
        Expr::JsonPath { base, segments } => Ok(Expr::JsonPath {
            base: rewrite(base)?,
            segments,
        }),
        Expr::InSubquery { .. } | Expr::ExistsSubquery { .. } => Err(PlanError::Semantic(format!(
            "nested projection `{name}` filter cannot contain a subquery; \
             filter the outer query or the child columns directly"
        ))),
        Expr::FunctionCall(..) => Err(PlanError::Semantic(format!(
            "nested projection `{name}` filter cannot contain an aggregate function"
        ))),
        Expr::Window { .. } => Err(PlanError::Semantic(format!(
            "nested projection `{name}` filter cannot contain a window function"
        ))),
        Expr::NestedQuery(_) => Err(PlanError::Semantic(format!(
            "nested projection `{name}` filter cannot contain another nested projection"
        ))),
    }
}

/// Build a left-deep nested-loop join plan for a query with 1+ join clauses.
///
/// The plan shape for `T1 as a [inner|left|cross] join T2 as b on <pred> ...` is:
///
///   Project? (optional, from q.projection)
///   └─ Offset? / Limit? / Sort?
///      └─ Filter? (the top-level q.filter, using qualified columns)
///         └─ NestedLoopJoin { kind, on }
///            ├─ AliasScan { T1, a }
///            └─ AliasScan { T2, b }
///
/// Multi-join chains extend left-deep: a third join adds a second
/// `NestedLoopJoin` on top, with the first join's output as its `left`.
///
/// Aliases default to the source table name when the query didn't write
/// `as <name>` explicitly — that way users can always write `T.field`
/// without being forced to alias every source.
///
/// RightOuter is rewritten into LeftOuter with inputs swapped — the two
/// differ only in which side survives non-matching rows, and swapping
/// inputs lets the executor ship a single LeftOuter path.
fn plan_joined_query(mut q: QueryExpr) -> Result<PlanNode, PlanError> {
    let primary_alias = q.alias.clone().unwrap_or_else(|| q.source.clone());
    let mut aliases = std::collections::HashSet::new();
    aliases.insert(primary_alias.clone());
    let mut node = PlanNode::AliasScan {
        table: q.source.clone(),
        alias: primary_alias,
    };

    for join in q.joins {
        let right_alias = join.alias.unwrap_or_else(|| join.source.clone());
        if !aliases.insert(right_alias.clone()) {
            return Err(ParseError::Syntax {
                message: format!(
                    "duplicate source alias `{right_alias}` in join; every joined source needs a unique alias"
                ),
            }
            .into());
        }
        let right = PlanNode::AliasScan {
            table: join.source,
            alias: right_alias,
        };
        match join.kind {
            JoinKind::Inner | JoinKind::LeftOuter | JoinKind::Cross => {
                node = PlanNode::NestedLoopJoin {
                    left: Box::new(node),
                    right: Box::new(right),
                    on: join.on,
                    kind: join.kind,
                };
            }
            JoinKind::RightOuter => {
                // `a RIGHT OUTER JOIN b ON <p>` ≡ `b LEFT OUTER JOIN a ON <p>`.
                node = PlanNode::NestedLoopJoin {
                    left: Box::new(right),
                    right: Box::new(node),
                    on: join.on,
                    kind: JoinKind::LeftOuter,
                };
            }
        }
    }

    if let Some(pred) = q.filter {
        node = PlanNode::Filter {
            input: Box::new(node),
            predicate: pred,
        };
    }

    if q.group_by.is_none() {
        if let Some(order) = q.order.take() {
            node = PlanNode::Sort {
                input: Box::new(node),
                keys: order
                    .keys
                    .into_iter()
                    .map(|k| SortKey {
                        expr: k.expr,
                        descending: k.descending,
                    })
                    .collect(),
            };
        }
    }

    // Mission E2b: GROUP BY path for joined queries.
    if let Some(group) = q.group_by {
        let mut grouped_order = q.order;
        let mut proj_fields: Vec<ProjectField> = q
            .projection
            .map(|proj| {
                proj.into_iter()
                    .map(|pf| ProjectField {
                        alias: pf.alias,
                        expr: pf.expr,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut having = group.having;
        let aggregates = extract_aggregates(&mut proj_fields, &mut having, &aliases)?;
        rewrite_group_order_keys(grouped_order.as_mut(), &proj_fields, &group.keys);
        rewrite_group_key_references(&mut proj_fields, &mut having, &group.keys);

        node = PlanNode::GroupBy {
            input: Box::new(node),
            keys: group.keys,
            aggregates,
            having,
        };

        if !proj_fields.is_empty() {
            node = PlanNode::Project {
                input: Box::new(node),
                fields: proj_fields,
            };
        }
        if let Some(order) = grouped_order {
            node = PlanNode::Sort {
                input: Box::new(node),
                keys: order
                    .keys
                    .into_iter()
                    .map(|key| SortKey {
                        expr: key.expr,
                        descending: key.descending,
                    })
                    .collect(),
            };
        }
        // LIMIT/OFFSET operate on grouped result rows, never on the joined
        // input. Applying either before GroupBy truncates source rows and can
        // silently change aggregate values. Offset remains inside Limit so
        // execution skips M grouped rows before taking N.
        if let Some(off) = q.offset {
            node = PlanNode::Offset {
                input: Box::new(node),
                count: off,
            };
        }
        if let Some(lim) = q.limit {
            node = PlanNode::Limit {
                input: Box::new(node),
                count: lim,
            };
        }
        if q.distinct {
            node = PlanNode::Distinct {
                input: Box::new(node),
            };
        }
        return Ok(node);
    }

    // Offset must be applied *before* Limit: skip M rows, then take N.
    // Plan shape is Limit(Offset(...)), so Offset is built first (inner)
    // and Limit wraps it (outer).
    if let Some(off) = q.offset {
        node = PlanNode::Offset {
            input: Box::new(node),
            count: off,
        };
    }

    if let Some(lim) = q.limit {
        node = PlanNode::Limit {
            input: Box::new(node),
            count: lim,
        };
    }

    if let Some(proj) = q.projection {
        let mut fields: Vec<ProjectField> = proj
            .into_iter()
            .map(|pf| ProjectField {
                alias: pf.alias,
                expr: pf.expr,
            })
            .collect();
        let windows = extract_windows(&mut fields);
        if !windows.is_empty() {
            node = PlanNode::Window {
                input: Box::new(node),
                windows,
            };
        }
        node = PlanNode::Project {
            input: Box::new(node),
            fields,
        };
    }

    if q.distinct {
        node = PlanNode::Distinct {
            input: Box::new(node),
        };
    }

    if let Some(agg) = q.aggregation {
        let provenance_alias =
            symmetric_provenance_alias(agg.function, agg.argument.as_ref(), agg.mode, &aliases)?;
        node = PlanNode::Aggregate {
            input: Box::new(node),
            function: agg.function,
            argument: agg.argument,
            mode: agg.mode,
            provenance_alias,
        };
    }

    Ok(node)
}

fn plan_insert(ins: InsertExpr) -> Result<PlanNode, PlanError> {
    Ok(PlanNode::Insert {
        table: ins.target,
        rows: ins.rows,
        returning: ins.returning,
    })
}

fn plan_update(upd: UpdateExpr) -> Result<PlanNode, PlanError> {
    // Mirror the read-side IndexScan fold: when the update filter is a simple
    // `.col = literal`, emit `Update(IndexScan)` so the executor's index-lookup
    // mutation fast path fires. The executor falls back to a scan if the
    // column happens to lack an index, so this is always safe.
    let source = match upd.filter {
        Some(pred) => match try_extract_eq_index_key(&upd.source, &pred) {
            Some(index_scan) => index_scan,
            None => match try_extract_range_index_keys(&upd.source, &pred) {
                Some(range_scan) => range_scan,
                None => PlanNode::Filter {
                    input: Box::new(PlanNode::SeqScan {
                        table: upd.source.clone(),
                    }),
                    predicate: pred,
                },
            },
        },
        None => PlanNode::SeqScan {
            table: upd.source.clone(),
        },
    };
    Ok(PlanNode::Update {
        input: Box::new(source),
        table: upd.source,
        assignments: upd.assignments,
        returning: upd.returning,
    })
}

fn plan_delete(del: DeleteExpr) -> Result<PlanNode, PlanError> {
    let source = match del.filter {
        Some(pred) => match try_extract_eq_index_key(&del.source, &pred) {
            Some(index_scan) => index_scan,
            None => match try_extract_range_index_keys(&del.source, &pred) {
                Some(range_scan) => range_scan,
                None => PlanNode::Filter {
                    input: Box::new(PlanNode::SeqScan {
                        table: del.source.clone(),
                    }),
                    predicate: pred,
                },
            },
        },
        None => PlanNode::SeqScan {
            table: del.source.clone(),
        },
    };
    Ok(PlanNode::Delete {
        input: Box::new(source),
        table: del.source,
        returning: del.returning,
    })
}

fn plan_upsert(ups: UpsertExpr) -> Result<PlanNode, PlanError> {
    Ok(PlanNode::Upsert {
        table: ups.target,
        key_column: ups.key_column,
        assignments: ups.assignments,
        on_conflict: ups.on_conflict,
    })
}

fn plan_create_type(ct: CreateTypeExpr) -> Result<PlanNode, PlanError> {
    let fields = ct
        .fields
        .into_iter()
        .map(|f| crate::plan::CreateField {
            name: f.name,
            type_name: f.type_name,
            required: f.required,
            unique: f.unique,
            default: f.default,
            auto: f.auto,
        })
        .collect();
    Ok(PlanNode::CreateTable {
        name: ct.name,
        fields,
        if_not_exists: ct.if_not_exists,
    })
}

/// If the predicate is a simple `.field = literal` (or `literal = .field`),
/// return a corresponding IndexScan plan node. Otherwise return None so the
/// caller can fall through to SeqScan + Filter.
///
/// The executor decides at run time whether the named column actually has a
/// B-tree index — if not, IndexScan transparently falls back to a scan +
/// equality filter on that column. That means this rewrite is always safe
/// regardless of schema/index state; it just unlocks the fast path when an
/// index happens to exist.
pub(crate) fn try_extract_eq_index_key(table: &str, pred: &Expr) -> Option<PlanNode> {
    let (lhs, op, rhs) = match pred {
        Expr::BinaryOp(lhs, op, rhs) => (lhs.as_ref(), *op, rhs.as_ref()),
        _ => return None,
    };
    if op != BinOp::Eq {
        return None;
    }
    match (lhs, rhs) {
        (path @ Expr::JsonPath { .. }, Expr::Literal(_)) => Some(PlanNode::ExprIndexScan {
            table: table.to_string(),
            path: stored_json_path(path)?,
            key: rhs.clone(),
        }),
        (Expr::Literal(_), path @ Expr::JsonPath { .. }) => Some(PlanNode::ExprIndexScan {
            table: table.to_string(),
            path: stored_json_path(path)?,
            key: lhs.clone(),
        }),
        (Expr::Field(name), Expr::Literal(_)) => Some(PlanNode::IndexScan {
            table: table.to_string(),
            column: name.clone(),
            key: rhs.clone(),
        }),
        (Expr::Literal(_), Expr::Field(name)) => Some(PlanNode::IndexScan {
            table: table.to_string(),
            column: name.clone(),
            key: lhs.clone(),
        }),
        _ => None,
    }
}

fn stored_json_path(expr: &Expr) -> Option<StoredJsonPathV1> {
    JsonPathIdentityV1::from_expr(expr)?.bind_table_local(None)
}

/// Extract a single range bound from a simple inequality predicate.
/// Returns `(column, lower_bound, upper_bound)` where at most one bound is set.
pub(crate) fn extract_single_bound(pred: &Expr) -> Option<RangeBound> {
    let (lhs, op, rhs) = match pred {
        Expr::BinaryOp(lhs, op, rhs) => (lhs.as_ref(), *op, rhs.as_ref()),
        _ => return None,
    };
    match op {
        // .col > literal  →  lower=(literal, exclusive)
        BinOp::Gt => match (lhs, rhs) {
            (Expr::Field(name), Expr::Literal(_)) => Some((
                RangeTarget::Column(name.clone()),
                Some((rhs.clone(), false)),
                None,
            )),
            (Expr::Literal(_), Expr::Field(name)) => {
                // literal > .col  →  col < literal  →  upper=(literal, exclusive)
                Some((
                    RangeTarget::Column(name.clone()),
                    None,
                    Some((lhs.clone(), false)),
                ))
            }
            (path @ Expr::JsonPath { .. }, Expr::Literal(_)) => Some((
                RangeTarget::JsonPath(stored_json_path(path)?),
                Some((rhs.clone(), false)),
                None,
            )),
            (Expr::Literal(_), path @ Expr::JsonPath { .. }) => Some((
                RangeTarget::JsonPath(stored_json_path(path)?),
                None,
                Some((lhs.clone(), false)),
            )),
            _ => None,
        },
        // .col >= literal  →  lower=(literal, inclusive)
        BinOp::Gte => match (lhs, rhs) {
            (Expr::Field(name), Expr::Literal(_)) => Some((
                RangeTarget::Column(name.clone()),
                Some((rhs.clone(), true)),
                None,
            )),
            (Expr::Literal(_), Expr::Field(name)) => Some((
                RangeTarget::Column(name.clone()),
                None,
                Some((lhs.clone(), true)),
            )),
            (path @ Expr::JsonPath { .. }, Expr::Literal(_)) => Some((
                RangeTarget::JsonPath(stored_json_path(path)?),
                Some((rhs.clone(), true)),
                None,
            )),
            (Expr::Literal(_), path @ Expr::JsonPath { .. }) => Some((
                RangeTarget::JsonPath(stored_json_path(path)?),
                None,
                Some((lhs.clone(), true)),
            )),
            _ => None,
        },
        // .col < literal  →  upper=(literal, exclusive)
        BinOp::Lt => match (lhs, rhs) {
            (Expr::Field(name), Expr::Literal(_)) => Some((
                RangeTarget::Column(name.clone()),
                None,
                Some((rhs.clone(), false)),
            )),
            (Expr::Literal(_), Expr::Field(name)) => Some((
                RangeTarget::Column(name.clone()),
                Some((lhs.clone(), false)),
                None,
            )),
            (path @ Expr::JsonPath { .. }, Expr::Literal(_)) => Some((
                RangeTarget::JsonPath(stored_json_path(path)?),
                None,
                Some((rhs.clone(), false)),
            )),
            (Expr::Literal(_), path @ Expr::JsonPath { .. }) => Some((
                RangeTarget::JsonPath(stored_json_path(path)?),
                Some((lhs.clone(), false)),
                None,
            )),
            _ => None,
        },
        // .col <= literal  →  upper=(literal, inclusive)
        BinOp::Lte => match (lhs, rhs) {
            (Expr::Field(name), Expr::Literal(_)) => Some((
                RangeTarget::Column(name.clone()),
                None,
                Some((rhs.clone(), true)),
            )),
            (Expr::Literal(_), Expr::Field(name)) => Some((
                RangeTarget::Column(name.clone()),
                Some((lhs.clone(), true)),
                None,
            )),
            (path @ Expr::JsonPath { .. }, Expr::Literal(_)) => Some((
                RangeTarget::JsonPath(stored_json_path(path)?),
                None,
                Some((rhs.clone(), true)),
            )),
            (Expr::Literal(_), path @ Expr::JsonPath { .. }) => Some((
                RangeTarget::JsonPath(stored_json_path(path)?),
                Some((lhs.clone(), true)),
                None,
            )),
            _ => None,
        },
        _ => None,
    }
}

/// If the predicate is an inequality or a conjunction of two inequalities
/// on the same indexed column, return a RangeScan plan node.
/// Handles: `.col > lit`, `.col >= lit`, `.col < lit`, `.col <= lit`,
/// and AND-conjunctions like `.col >= low AND .col <= high` (BETWEEN pattern).
fn try_extract_range_index_keys(table: &str, pred: &Expr) -> Option<PlanNode> {
    // Case 1: AND conjunction — try to merge two bounds on the same column.
    if let Expr::BinaryOp(lhs, BinOp::And, rhs) = pred {
        if let (Some((col1, s1, e1)), Some((col2, s2, e2))) =
            (extract_single_bound(lhs), extract_single_bound(rhs))
        {
            if col1 == col2 {
                let start = s1.or(s2);
                let end = e1.or(e2);
                if start.is_some() || end.is_some() {
                    return Some(range_scan_for_target(table, col1, start, end));
                }
            }
        }
    }

    // Case 2: single inequality.
    if let Some((col, start, end)) = extract_single_bound(pred) {
        return Some(range_scan_for_target(table, col, start, end));
    }

    None
}

pub(crate) fn range_scan_for_target(
    table: &str,
    target: RangeTarget,
    start: Option<(Expr, bool)>,
    end: Option<(Expr, bool)>,
) -> PlanNode {
    match target {
        RangeTarget::Column(column) => PlanNode::RangeScan {
            table: table.to_string(),
            column,
            start,
            end,
        },
        RangeTarget::JsonPath(path) => PlanNode::ExprRangeScan {
            table: table.to_string(),
            path,
            start,
            end,
        },
    }
}

/// Fold only the exact, semantics-preserving single-table shape that can stream
/// directly from one expression index. Anything involving filters, joins,
/// grouping, distinct, aggregation, windows, multiple sort keys, or non-integer
/// slice expressions retains the generic Sort pipeline.
fn try_extract_ordered_expr_index_scan(query: &QueryExpr) -> Option<PlanNode> {
    if query.alias.is_some()
        || !query.joins.is_empty()
        || query.filter.is_some()
        || query.group_by.is_some()
        || query.distinct
        || query.aggregation.is_some()
        || query.projection.as_ref().is_some_and(|fields| {
            fields
                .iter()
                .any(|field| matches!(field.expr, Expr::Window { .. }))
        })
    {
        return None;
    }
    let order = query.order.as_ref()?;
    let [key] = order.keys.as_slice() else {
        return None;
    };
    let path = stored_json_path(&key.expr)?;
    let limit = query.limit.as_ref()?;
    if !matches!(limit, Expr::Literal(Literal::Int(value)) if *value >= 0) {
        return None;
    }
    if !query
        .offset
        .as_ref()
        .is_none_or(|offset| matches!(offset, Expr::Literal(Literal::Int(value)) if *value >= 0))
    {
        return None;
    }
    Some(PlanNode::OrderedExprIndexScan {
        table: query.source.clone(),
        path,
        descending: key.descending,
        limit: limit.clone(),
        offset: query.offset.clone(),
    })
}

/// Walk projection fields, replacing every `Expr::Window { .. }` with
/// `Expr::Field("__win_N")` and collecting the corresponding `WindowDef`
/// descriptors. Returns the list of window definitions to insert as a
/// `PlanNode::Window` before the `Project` node.
fn extract_windows(proj_fields: &mut [ProjectField]) -> Vec<WindowDef> {
    let mut defs = Vec::new();
    let mut counter = 0usize;
    for f in proj_fields.iter_mut() {
        if let Expr::Window {
            function,
            args,
            mode,
            partition_by,
            order_by,
        } = &f.expr
        {
            let output_name = format!("__win_{counter}");
            defs.push(WindowDef {
                function: *function,
                args: args.clone(),
                mode: *mode,
                partition_by: partition_by.clone(),
                order_by: order_by
                    .iter()
                    .map(|k| SortKey {
                        expr: k.expr.clone(),
                        descending: k.descending,
                    })
                    .collect(),
                output_name: output_name.clone(),
            });
            f.expr = Expr::Field(output_name);
            counter += 1;
        }
    }
    defs
}

/// Walk projection fields and HAVING expression, replacing every
/// `Expr::FunctionCall(func, Field(col))` with `Expr::Field("__agg_N")`
/// and collecting the corresponding `GroupAgg` descriptors. Deduplicates:
/// if the same (func, field) pair appears in both projection and HAVING,
/// they share a single `GroupAgg` entry.
fn extract_aggregates(
    proj_fields: &mut [ProjectField],
    having: &mut Option<Expr>,
    source_aliases: &std::collections::HashSet<String>,
) -> Result<Vec<GroupAgg>, PlanError> {
    let mut aggs: Vec<GroupAgg> = Vec::new();
    let mut counter = 0usize;
    for f in proj_fields.iter_mut() {
        rewrite_agg_expr(&mut f.expr, &mut aggs, &mut counter, source_aliases)?;
    }
    if let Some(h) = having {
        rewrite_agg_expr(h, &mut aggs, &mut counter, source_aliases)?;
    }
    Ok(aggs)
}

fn rewrite_group_key_references(
    fields: &mut [ProjectField],
    having: &mut Option<Expr>,
    keys: &[GroupKey],
) {
    for field in fields {
        rewrite_group_key_expr(&mut field.expr, keys);
    }
    if let Some(having) = having {
        rewrite_group_key_expr(having, keys);
    }
}

fn rewrite_group_order_keys(
    order: Option<&mut OrderClause>,
    projection: &[ProjectField],
    keys: &[GroupKey],
) {
    let Some(order) = order else {
        return;
    };
    for order_key in &mut order.keys {
        let Some(group_key) = keys.iter().find(|key| key.expr == order_key.expr) else {
            continue;
        };
        let projected_name = projection
            .iter()
            .find(|field| field.expr == group_key.expr)
            .and_then(|field| field.alias.clone())
            .unwrap_or_else(|| group_key.output_name());
        order_key.expr = Expr::Field(projected_name);
    }
}

fn rewrite_group_key_expr(expr: &mut Expr, keys: &[GroupKey]) {
    if let Some(key) = keys.iter().find(|key| key.expr == *expr) {
        *expr = Expr::Field(key.output_name());
        return;
    }
    match expr {
        // Aggregate arguments run against input rows and have already been
        // extracted before this pass, so a survivor must not be rebound to a
        // grouped output column.
        Expr::FunctionCall(..) => {}
        Expr::BinaryOp(left, _, right) | Expr::Coalesce(left, right) => {
            rewrite_group_key_expr(left, keys);
            rewrite_group_key_expr(right, keys);
        }
        Expr::UnaryOp(_, inner) | Expr::Cast(inner, _) => rewrite_group_key_expr(inner, keys),
        Expr::ScalarFunc(_, args) => {
            for arg in args {
                rewrite_group_key_expr(arg, keys);
            }
        }
        Expr::InList { expr, list, .. } => {
            rewrite_group_key_expr(expr, keys);
            for item in list {
                rewrite_group_key_expr(item, keys);
            }
        }
        Expr::Case { whens, else_expr } => {
            for (condition, result) in whens {
                rewrite_group_key_expr(condition, keys);
                rewrite_group_key_expr(result, keys);
            }
            if let Some(expr) = else_expr {
                rewrite_group_key_expr(expr, keys);
            }
        }
        _ => {}
    }
}

fn rewrite_agg_expr(
    expr: &mut Expr,
    aggs: &mut Vec<GroupAgg>,
    counter: &mut usize,
    source_aliases: &std::collections::HashSet<String>,
) -> Result<(), PlanError> {
    match expr {
        Expr::FunctionCall(func, inner, mode) => {
            let output = find_or_insert_agg(aggs, *func, inner, *mode, counter, source_aliases)?;
            *expr = Expr::Field(output);
        }
        Expr::BinaryOp(l, _, r) => {
            rewrite_agg_expr(l, aggs, counter, source_aliases)?;
            rewrite_agg_expr(r, aggs, counter, source_aliases)?;
        }
        Expr::UnaryOp(_, inner) => rewrite_agg_expr(inner, aggs, counter, source_aliases)?,
        Expr::Coalesce(l, r) => {
            rewrite_agg_expr(l, aggs, counter, source_aliases)?;
            rewrite_agg_expr(r, aggs, counter, source_aliases)?;
        }
        Expr::InList { expr: e, list, .. } => {
            rewrite_agg_expr(e, aggs, counter, source_aliases)?;
            for item in list {
                rewrite_agg_expr(item, aggs, counter, source_aliases)?;
            }
        }
        Expr::InSubquery { expr: e, .. } => {
            rewrite_agg_expr(e, aggs, counter, source_aliases)?;
        }
        _ => {}
    }
    Ok(())
}

fn find_or_insert_agg(
    aggs: &mut Vec<GroupAgg>,
    func: AggFunc,
    argument: &Expr,
    mode: AggregateMode,
    counter: &mut usize,
    source_aliases: &std::collections::HashSet<String>,
) -> Result<String, PlanError> {
    for existing in aggs.iter() {
        if existing.function == func && existing.argument == *argument && existing.mode == mode {
            return Ok(existing.output_name.clone());
        }
    }
    let provenance_alias = symmetric_provenance_alias(func, Some(argument), mode, source_aliases)?;
    let output_name = format!("__agg_{counter}");
    aggs.push(GroupAgg {
        function: func,
        argument: argument.clone(),
        mode,
        provenance_alias,
        output_name: output_name.clone(),
    });
    *counter += 1;
    Ok(output_name)
}

fn symmetric_provenance_alias(
    function: AggFunc,
    argument: Option<&Expr>,
    mode: AggregateMode,
    source_aliases: &std::collections::HashSet<String>,
) -> Result<Option<String>, PlanError> {
    if mode == AggregateMode::Raw
        || source_aliases.len() < 2
        || !matches!(function, AggFunc::Sum | AggFunc::Avg | AggFunc::Count)
        || (function == AggFunc::Count
            && argument.is_none_or(|argument| matches!(argument, Expr::Field(name) if name == "*")))
    {
        return Ok(None);
    }
    let Some(argument) = argument else {
        return Err(symmetric_aggregate_error(
            function,
            "does not reference a source row",
        ));
    };

    let mut qualified = std::collections::HashSet::new();
    let mut has_unqualified = false;
    collect_expression_sources(argument, &mut qualified, &mut has_unqualified);

    for alias in &qualified {
        if !source_aliases.contains(alias) {
            return Err(symmetric_aggregate_error(
                function,
                &format!("references unknown source alias '{alias}'"),
            ));
        }
    }
    if has_unqualified {
        if source_aliases.len() != 1 {
            return Err(symmetric_aggregate_error(
                function,
                "contains an ambiguous unqualified field",
            ));
        }
        qualified.extend(source_aliases.iter().cloned());
    }
    match qualified.len() {
        1 => Ok(qualified.into_iter().next()),
        0 => Err(symmetric_aggregate_error(
            function,
            "does not reference a source row",
        )),
        _ => Err(symmetric_aggregate_error(
            function,
            "references multiple source aliases",
        )),
    }
}

fn symmetric_aggregate_error(function: AggFunc, reason: &str) -> PlanError {
    let name = format!("{function:?}").to_lowercase();
    PlanError::Semantic(format!(
        "symmetric {name} expression {reason}; reference exactly one source alias or use {name}(raw ...)"
    ))
}

fn collect_expression_sources(
    expr: &Expr,
    qualified: &mut std::collections::HashSet<String>,
    has_unqualified: &mut bool,
) {
    match expr {
        Expr::Field(name) if name != "*" => *has_unqualified = true,
        Expr::QualifiedField { qualifier, .. } => {
            qualified.insert(qualifier.clone());
        }
        Expr::BinaryOp(left, _, right) | Expr::Coalesce(left, right) => {
            collect_expression_sources(left, qualified, has_unqualified);
            collect_expression_sources(right, qualified, has_unqualified);
        }
        Expr::UnaryOp(_, inner) | Expr::Cast(inner, _) | Expr::JsonPath { base: inner, .. } => {
            collect_expression_sources(inner, qualified, has_unqualified);
        }
        Expr::ScalarFunc(_, args) => {
            for argument in args {
                collect_expression_sources(argument, qualified, has_unqualified);
            }
        }
        Expr::InList { expr, list, .. } => {
            collect_expression_sources(expr, qualified, has_unqualified);
            for item in list {
                collect_expression_sources(item, qualified, has_unqualified);
            }
        }
        Expr::InSubquery { expr, .. } => {
            collect_expression_sources(expr, qualified, has_unqualified);
        }
        Expr::Case { whens, else_expr } => {
            for (condition, result) in whens {
                collect_expression_sources(condition, qualified, has_unqualified);
                collect_expression_sources(result, qualified, has_unqualified);
            }
            if let Some(expr) = else_expr {
                collect_expression_sources(expr, qualified, has_unqualified);
            }
        }
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for expr in args.iter().chain(partition_by) {
                collect_expression_sources(expr, qualified, has_unqualified);
            }
            for key in order_by {
                collect_expression_sources(&key.expr, qualified, has_unqualified);
            }
        }
        Expr::FunctionCall(_, inner, _) => {
            collect_expression_sources(inner, qualified, has_unqualified);
        }
        Expr::ExistsSubquery { .. }
        | Expr::Field(_)
        | Expr::Literal(_)
        | Expr::Param(_)
        | Expr::ValueLit(_)
        | Expr::Null
        | Expr::NestedQuery(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlanNode;

    #[test]
    fn test_plan_simple_scan() {
        let plan = plan("User").unwrap();
        assert!(matches!(plan, PlanNode::SeqScan { table } if table == "User"));
    }

    #[test]
    fn test_plan_filter() {
        let plan = plan("User filter .age > 30").unwrap();
        assert!(matches!(plan, PlanNode::RangeScan { .. }));
    }

    #[test]
    fn test_plan_filter_with_projection() {
        let plan = plan("User filter .age > 30 { name, email }").unwrap();
        assert!(matches!(plan, PlanNode::Project { .. }));
    }

    #[test]
    fn test_plan_insert() {
        let plan = plan(r#"insert User { name := "Alice", age := 30 }"#).unwrap();
        assert!(matches!(plan, PlanNode::Insert { .. }));
    }

    #[test]
    fn test_plan_order_limit() {
        let plan = plan("User order .name limit 10").unwrap();
        match plan {
            PlanNode::Limit { input, .. } => {
                assert!(matches!(*input, PlanNode::Sort { .. }));
            }
            _ => panic!("expected Limit(Sort(SeqScan))"),
        }
    }

    #[test]
    fn test_plan_count() {
        let plan = plan("count(User)").unwrap();
        assert!(matches!(plan, PlanNode::Aggregate { .. }));
    }

    #[test]
    fn single_source_aggregates_do_not_request_provenance() {
        for query in [
            "sum(User { .amount })",
            "avg(User { .amount })",
            "count(User { .amount })",
        ] {
            match plan(query).unwrap() {
                PlanNode::Aggregate {
                    provenance_alias, ..
                } => assert!(
                    provenance_alias.is_none(),
                    "unexpected provenance for {query}"
                ),
                other => panic!("expected Aggregate for {query}, got {other:?}"),
            }
        }

        match plan("User group .dept { total: sum(.amount) }").unwrap() {
            PlanNode::Project { input, .. } => match *input {
                PlanNode::GroupBy { aggregates, .. } => {
                    assert!(aggregates[0].provenance_alias.is_none());
                }
                other => panic!("expected GroupBy, got {other:?}"),
            },
            other => panic!("expected Project(GroupBy), got {other:?}"),
        }
    }

    #[test]
    fn join_provenance_is_limited_to_fanout_sensitive_aggregates() {
        let base = "Account as a join Entry as e on a.id = e.account_id group a.dept";
        for (function, expects_provenance) in [
            ("sum(a.balance)", true),
            ("avg(a.balance)", true),
            ("count(a.balance)", true),
            ("min(a.balance)", false),
            ("max(a.balance)", false),
            ("count(distinct a.balance)", false),
            ("count(*)", false),
        ] {
            let query = format!("{base} {{ value: {function} }}");
            match plan(&query).unwrap() {
                PlanNode::Project { input, .. } => match *input {
                    PlanNode::GroupBy { aggregates, .. } => assert_eq!(
                        aggregates[0].provenance_alias.as_deref(),
                        expects_provenance.then_some("a"),
                        "unexpected provenance selection for {function}"
                    ),
                    other => panic!("expected GroupBy for {function}, got {other:?}"),
                },
                other => panic!("expected Project(GroupBy) for {function}, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_plan_eq_becomes_index_scan() {
        // `filter .col = literal` should fold into an IndexScan — the executor
        // falls back to a scan if the column happens to lack an index.
        let plan = plan("User filter .id = 42").unwrap();
        match plan {
            PlanNode::IndexScan { table, column, key } => {
                assert_eq!(table, "User");
                assert_eq!(column, "id");
                assert!(matches!(key, Expr::Literal(Literal::Int(42))));
            }
            other => panic!("expected IndexScan, got {other:?}"),
        }
    }

    #[test]
    fn test_plan_eq_reversed_becomes_index_scan() {
        // Literal-on-the-left form should fold the same way.
        let plan = plan(r#"User filter "NYC" = .city"#).unwrap();
        assert!(matches!(plan, PlanNode::IndexScan { .. }));
    }

    #[test]
    fn json_path_equality_and_reversed_equality_are_speculative_expression_scans() {
        for query in ["Post filter .data->age = 21", "Post filter 21 = .data->age"] {
            match plan(query).unwrap() {
                PlanNode::ExprIndexScan { table, path, key } => {
                    assert_eq!(table, "Post");
                    assert_eq!(path.canonical_text(), "v1:.data->\"age\"");
                    assert!(matches!(key, Expr::Literal(Literal::Int(21))));
                }
                other => panic!("expected ExprIndexScan for `{query}`, got {other:?}"),
            }
        }
    }

    #[test]
    fn json_path_range_and_same_path_compound_bounds_are_speculative_scans() {
        for query in ["Post filter .data->age > 18", "Post filter 18 < .data->age"] {
            match plan(query).unwrap() {
                PlanNode::ExprRangeScan {
                    path, start, end, ..
                } => {
                    assert_eq!(path.canonical_text(), "v1:.data->\"age\"");
                    assert!(start.is_some());
                    assert!(end.is_none());
                }
                other => panic!("expected ExprRangeScan for `{query}`, got {other:?}"),
            }
        }

        match plan("Post filter .data->age >= 18 and .data->age < 65").unwrap() {
            PlanNode::ExprRangeScan {
                path, start, end, ..
            } => {
                assert_eq!(path.canonical_text(), "v1:.data->\"age\"");
                assert_eq!(start, Some((Expr::Literal(Literal::Int(18)), true)));
                assert_eq!(end, Some((Expr::Literal(Literal::Int(65)), false)));
            }
            other => panic!("expected bounded ExprRangeScan, got {other:?}"),
        }

        assert!(matches!(
            plan("Post filter .data->age >= 18 and .data->score < 65").unwrap(),
            PlanNode::Filter { .. }
        ));
    }

    #[test]
    fn exact_single_path_order_limit_uses_ordered_expression_scan() {
        match plan("Post order .data->age desc limit 10 offset 2 { .id }").unwrap() {
            PlanNode::Project { input, .. } => match *input {
                PlanNode::OrderedExprIndexScan {
                    table,
                    path,
                    descending,
                    limit,
                    offset,
                } => {
                    assert_eq!(table, "Post");
                    assert_eq!(path.canonical_text(), "v1:.data->\"age\"");
                    assert!(descending);
                    assert_eq!(limit, Expr::Literal(Literal::Int(10)));
                    assert_eq!(offset, Some(Expr::Literal(Literal::Int(2))));
                }
                other => panic!("expected OrderedExprIndexScan, got {other:?}"),
            },
            other => panic!("expected Project(OrderedExprIndexScan), got {other:?}"),
        }
    }

    #[test]
    fn incompatible_path_order_shapes_keep_generic_sort() {
        for query in [
            "Post order .data->age",
            "Post order .data->age, .id limit 10",
            "Post filter .data->active = true order .data->age limit 10",
            "Post order .data->age limit .id",
        ] {
            let planned = plan(query).unwrap();
            assert!(
                !plan_contains_ordered_expr_scan(&planned),
                "`{query}` must remain on the generic pipeline: {planned:?}"
            );
        }
    }

    fn plan_contains_ordered_expr_scan(plan: &PlanNode) -> bool {
        match plan {
            PlanNode::OrderedExprIndexScan { .. } => true,
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
            | PlanNode::Explain { input } => plan_contains_ordered_expr_scan(input),
            PlanNode::NestedLoopJoin { left, right, .. } | PlanNode::Union { left, right, .. } => {
                plan_contains_ordered_expr_scan(left) || plan_contains_ordered_expr_scan(right)
            }
            _ => false,
        }
    }

    #[test]
    fn test_plan_non_eq_stays_filter() {
        // `>` now emits a RangeScan instead of SeqScan+Filter.
        let plan = plan("User filter .age > 30").unwrap();
        match plan {
            PlanNode::RangeScan {
                column, start, end, ..
            } => {
                assert_eq!(column, "age");
                assert!(start.is_some(), "expected lower bound");
                assert!(end.is_none(), "expected no upper bound");
                let (_, inclusive) = start.unwrap();
                assert!(!inclusive, "expected exclusive lower bound for >");
            }
            other => panic!("expected RangeScan, got {other:?}"),
        }
    }

    #[test]
    fn test_plan_index_scan_with_projection() {
        // Projection on top of an IndexScan should layer correctly.
        let plan = plan("User filter .id = 1 { .name }").unwrap();
        match plan {
            PlanNode::Project { input, .. } => {
                assert!(matches!(*input, PlanNode::IndexScan { .. }));
            }
            other => panic!("expected Project(IndexScan), got {other:?}"),
        }
    }

    #[test]
    fn test_plan_update_by_pk_becomes_index_scan() {
        // `.id = literal` update should fold to Update(IndexScan), not
        // Update(Filter(SeqScan)).
        let plan = plan("User filter .id = 42 update { age := 31 }").unwrap();
        match plan {
            PlanNode::Update { input, .. } => {
                assert!(
                    matches!(*input, PlanNode::IndexScan { .. }),
                    "expected Update(IndexScan), got {input:?}"
                );
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn test_plan_update_range_stays_range_scan() {
        let plan = plan("User filter .age > 30 update { age := 31 }").unwrap();
        match plan {
            PlanNode::Update { input, .. } => {
                assert!(
                    matches!(*input, PlanNode::RangeScan { .. }),
                    "expected Update(RangeScan), got {input:?}"
                );
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn test_plan_delete_by_pk_becomes_index_scan() {
        let plan = plan("User filter .id = 7 delete").unwrap();
        match plan {
            PlanNode::Delete { input, .. } => {
                assert!(matches!(*input, PlanNode::IndexScan { .. }));
            }
            other => panic!("expected Delete, got {other:?}"),
        }
    }

    #[test]
    fn test_plan_inner_join_builds_nested_loop() {
        // Mission E1.2: a join query should plan to NestedLoopJoin with
        // AliasScan leaves on both sides.
        let plan = plan("User as u join Order as o on u.id = o.user_id").unwrap();
        match plan {
            PlanNode::NestedLoopJoin {
                left,
                right,
                on,
                kind,
            } => {
                assert_eq!(kind, JoinKind::Inner);
                assert!(on.is_some());
                assert!(matches!(*left, PlanNode::AliasScan { .. }));
                assert!(matches!(*right, PlanNode::AliasScan { .. }));
            }
            other => panic!("expected NestedLoopJoin, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_join_aliases_are_rejected_before_execution() {
        let err = plan("A as x join A as x on x.id = x.id").unwrap_err();
        assert!(
            err.to_string().contains("duplicate source alias `x`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_plan_right_join_rewritten_as_left_with_swapped_inputs() {
        let plan = plan("User as u right join Order as o on u.id = o.user_id").unwrap();
        match plan {
            PlanNode::NestedLoopJoin {
                left, right, kind, ..
            } => {
                assert_eq!(kind, JoinKind::LeftOuter);
                // Swapped: Order is now on the left, User on the right.
                match *left {
                    PlanNode::AliasScan { table, .. } => assert_eq!(table, "Order"),
                    other => panic!("expected AliasScan(Order), got {other:?}"),
                }
                match *right {
                    PlanNode::AliasScan { table, .. } => assert_eq!(table, "User"),
                    other => panic!("expected AliasScan(User), got {other:?}"),
                }
            }
            other => panic!("expected NestedLoopJoin, got {other:?}"),
        }
    }

    #[test]
    fn test_plan_multi_join_is_left_deep() {
        // Three sources → two NestedLoopJoins, left-deep.
        let plan = plan(
            "User as u join Order as o on u.id = o.user_id \
             join Product as p on o.product_id = p.id",
        )
        .unwrap();
        match plan {
            PlanNode::NestedLoopJoin { left, right, .. } => {
                // Outer (Product) join: right is AliasScan(Product)
                match *right {
                    PlanNode::AliasScan { table, .. } => assert_eq!(table, "Product"),
                    other => panic!("expected AliasScan(Product), got {other:?}"),
                }
                // Outer.left is inner (Order) NestedLoopJoin
                assert!(matches!(*left, PlanNode::NestedLoopJoin { .. }));
            }
            other => panic!("expected NestedLoopJoin, got {other:?}"),
        }
    }

    #[test]
    fn test_plan_join_with_filter_tail_wraps_filter_on_top() {
        let plan =
            plan("User as u join Order as o on u.id = o.user_id filter o.total > 100").unwrap();
        match plan {
            PlanNode::Filter { input, .. } => {
                assert!(matches!(*input, PlanNode::NestedLoopJoin { .. }));
            }
            other => panic!("expected Filter(NestedLoopJoin), got {other:?}"),
        }
    }

    #[test]
    fn test_plan_group_by_builds_groupby_node() {
        let plan = plan("User group .status { .status, n: count(.name) }").unwrap();
        // Should be Project(GroupBy(SeqScan)).
        match plan {
            PlanNode::Project { input, fields } => {
                assert_eq!(fields.len(), 2);
                match *input {
                    PlanNode::GroupBy {
                        input: inner,
                        keys,
                        aggregates,
                        having,
                    } => {
                        assert!(matches!(*inner, PlanNode::SeqScan { .. }));
                        assert_eq!(
                            keys,
                            vec![GroupKey {
                                expr: Expr::Field("status".into()),
                                output_name: "status".into(),
                            }]
                        );
                        assert_eq!(aggregates.len(), 1);
                        assert_eq!(aggregates[0].function, AggFunc::Count);
                        assert_eq!(aggregates[0].argument, Expr::Field("name".into()));
                        assert!(having.is_none());
                    }
                    other => panic!("expected GroupBy, got {other:?}"),
                }
            }
            other => panic!("expected Project, got {other:?}"),
        }
    }

    #[test]
    fn test_plan_joined_group_applies_order_offset_limit_after_grouping() {
        let plan = plan(
            "User as u join Order as o on u.id = o.user_id \
             group u.status { u.status, n: count(*) } order n desc offset 1 limit 2",
        )
        .unwrap();

        let PlanNode::Limit { input, .. } = plan else {
            panic!("expected Limit at the grouped-result boundary");
        };
        let PlanNode::Offset { input, .. } = *input else {
            panic!("expected Offset below Limit");
        };
        let PlanNode::Sort { input, .. } = *input else {
            panic!("expected Sort below Offset");
        };
        let PlanNode::Project { input, .. } = *input else {
            panic!("expected Project below Sort");
        };
        let PlanNode::GroupBy { input, .. } = *input else {
            panic!("expected GroupBy below Project");
        };
        assert!(
            matches!(*input, PlanNode::NestedLoopJoin { .. }),
            "joined rows must flow into GroupBy before result limiting"
        );
    }

    #[test]
    fn test_plan_group_by_having_rewrites_agg_in_having() {
        let plan = plan("User group .status having count(.name) > 1 { .status }").unwrap();
        match plan {
            PlanNode::Project { input, .. } => {
                match *input {
                    PlanNode::GroupBy {
                        having, aggregates, ..
                    } => {
                        // The planner should have extracted count(.name) into
                        // aggregates and rewritten the HAVING to reference __agg_0.
                        assert_eq!(aggregates.len(), 1);
                        assert_eq!(aggregates[0].output_name, "__agg_0");
                        let h = having.expect("having should be Some");
                        match h {
                            Expr::BinaryOp(l, BinOp::Gt, _) => {
                                assert!(
                                    matches!(*l, Expr::Field(ref name) if name == "__agg_0"),
                                    "expected Field(__agg_0), got {l:?}"
                                );
                            }
                            other => panic!("expected BinaryOp, got {other:?}"),
                        }
                    }
                    other => panic!("expected GroupBy, got {other:?}"),
                }
            }
            other => panic!("expected Project, got {other:?}"),
        }
    }

    #[test]
    fn test_plan_window_inserts_window_node_before_project() {
        let plan = plan("User { .name, rn: row_number() over (order .age) }").unwrap();
        // Expected shape: Project(Window(SeqScan))
        match plan {
            PlanNode::Project { input, fields } => {
                assert_eq!(fields.len(), 2);
                // The window expr should have been replaced with Field("__win_0")
                assert!(
                    matches!(&fields[1].expr, Expr::Field(name) if name == "__win_0"),
                    "expected Field(__win_0), got {:?}",
                    fields[1].expr
                );
                match *input {
                    PlanNode::Window {
                        input: inner,
                        windows,
                    } => {
                        assert_eq!(windows.len(), 1);
                        assert_eq!(windows[0].output_name, "__win_0");
                        assert!(matches!(*inner, PlanNode::SeqScan { .. }));
                    }
                    other => panic!("expected Window, got {other:?}"),
                }
            }
            other => panic!("expected Project, got {other:?}"),
        }
    }

    #[test]
    fn test_plan_multiple_windows() {
        let plan = plan(
            "User { .name, rn: row_number() over (order .age), s: sum(.salary) over (partition .dept order .salary) }"
        ).unwrap();
        match plan {
            PlanNode::Project { input, fields } => {
                assert_eq!(fields.len(), 3);
                assert!(matches!(&fields[1].expr, Expr::Field(name) if name == "__win_0"));
                assert!(matches!(&fields[2].expr, Expr::Field(name) if name == "__win_1"));
                match *input {
                    PlanNode::Window { windows, .. } => {
                        assert_eq!(windows.len(), 2);
                        assert_eq!(windows[0].output_name, "__win_0");
                        assert_eq!(windows[1].output_name, "__win_1");
                    }
                    other => panic!("expected Window, got {other:?}"),
                }
            }
            other => panic!("expected Project, got {other:?}"),
        }
    }

    #[test]
    fn test_plan_no_window_without_over() {
        // Plain aggregate in projection should not create a Window node.
        let plan = plan("User group .dept { .dept, total: sum(.salary) }").unwrap();
        match plan {
            PlanNode::Project { input, .. } => {
                // Input should be GroupBy, not Window.
                assert!(
                    matches!(*input, PlanNode::GroupBy { .. }),
                    "expected GroupBy under Project, got {:?}",
                    input
                );
            }
            other => panic!("expected Project, got {other:?}"),
        }
    }

    #[test]
    fn test_plan_explain_wraps_inner() {
        let plan = plan("explain User filter .age > 30").unwrap();
        match plan {
            PlanNode::Explain { input } => {
                assert!(
                    matches!(*input, PlanNode::RangeScan { .. }),
                    "expected Explain(RangeScan), got {:?}",
                    input
                );
            }
            other => panic!("expected Explain, got {other:?}"),
        }
    }

    #[test]
    fn test_plan_explain_simple_scan() {
        let plan = plan("explain User").unwrap();
        match plan {
            PlanNode::Explain { input } => {
                assert!(matches!(*input, PlanNode::SeqScan { .. }));
            }
            other => panic!("expected Explain(SeqScan), got {other:?}"),
        }
    }

    #[test]
    fn test_plan_explain_join() {
        let plan = plan("explain User as u join Order as o on u.id = o.user_id").unwrap();
        match plan {
            PlanNode::Explain { input } => {
                assert!(matches!(*input, PlanNode::NestedLoopJoin { .. }));
            }
            other => panic!("expected Explain(NestedLoopJoin), got {other:?}"),
        }
    }
}
