//! Scan-family execution: expression-index scans, the Lane A index-residual
//! fast path, provenance-preserving materialization, and introspection.

use crate::cancel::CancelCheck;
use crate::result::{QueryError, QueryResult};
use powdb_storage::catalog::{IndexOrderDirection, LinkDef, LinkKind};
use std::collections::HashSet;

use crate::executor::eval::*;
use crate::executor::{mem_budget, Engine, MAX_SORT_ROWS};

use super::join::execute_provenance_join;
use super::lowering::stored_json_path_expr;
use super::*;

impl Engine {
    pub(crate) fn execute_expression_index_plan(
        &self,
        plan: &PlanNode,
        projected_fields: Option<&[ProjectField]>,
    ) -> Result<Option<QueryResult>, QueryError> {
        let (table, path) = match plan {
            PlanNode::ExprIndexScan { table, path, .. }
            | PlanNode::ExprRangeScan { table, path, .. }
            | PlanNode::OrderedExprIndexScan { table, path, .. } => (table, path),
            _ => return Ok(None),
        };
        let Some(index) = resolve_expression_index(&self.catalog, table, path) else {
            return Ok(None);
        };
        let schema = self
            .catalog
            .schema(table)
            .ok_or_else(|| QueryError::TableNotFound(table.clone()))?
            .clone();
        let all_columns: Vec<String> = schema
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect();

        let projection = match projected_fields {
            Some(fields) => {
                if !fields
                    .iter()
                    .all(|field| matches!(field.expr, Expr::Field(_)))
                {
                    return Ok(None);
                }
                let mut indices = Vec::with_capacity(fields.len());
                let mut columns = Vec::with_capacity(fields.len());
                for field in fields {
                    let Expr::Field(name) = &field.expr else {
                        unreachable!("plain-field projection checked above")
                    };
                    let index =
                        schema
                            .column_index(name)
                            .ok_or_else(|| QueryError::ColumnNotFound {
                                table: table.clone(),
                                column: name.clone(),
                            })?;
                    indices.push(index);
                    columns.push(field.alias.clone().unwrap_or_else(|| name.clone()));
                }
                Some((indices, columns))
            }
            None => None,
        };

        let (rids, range) = match plan {
            PlanNode::ExprIndexScan { key, .. } => {
                let key = literal_to_value(key)?;
                let rids = if key.is_empty() {
                    self.catalog
                        .expression_index_btree(table, index.index_id)
                        .ok_or_else(|| {
                            QueryError::Execution("expression index disappeared".to_string())
                        })?
                        .empty_rids()
                        .to_vec()
                } else {
                    self.catalog
                        .expression_index_lookup_all(table, index.index_id, &key)
                        .map_err(QueryError::from_storage_io)?
                };
                (rids, None)
            }
            PlanNode::ExprRangeScan { start, end, .. } => {
                let start_value = start
                    .as_ref()
                    .map(|(expr, _)| literal_to_value(expr))
                    .transpose()?;
                let end_value = end
                    .as_ref()
                    .map(|(expr, _)| literal_to_value(expr))
                    .transpose()?;
                let rids = self
                    .catalog
                    .expression_index_range_rids(
                        table,
                        index.index_id,
                        start_value.as_ref(),
                        end_value.as_ref(),
                    )
                    .map_err(QueryError::from_storage_io)?;
                (
                    rids,
                    Some((
                        start_value,
                        start.as_ref().is_none_or(|(_, inclusive)| *inclusive),
                        end_value,
                        end.as_ref().is_none_or(|(_, inclusive)| *inclusive),
                    )),
                )
            }
            PlanNode::OrderedExprIndexScan {
                descending,
                limit,
                offset,
                ..
            } => {
                let Expr::Literal(Literal::Int(limit)) = limit else {
                    return Err(QueryError::Execution(
                        "expression-index limit must be a non-negative integer".to_string(),
                    ));
                };
                let offset = match offset {
                    Some(Expr::Literal(Literal::Int(offset))) if *offset >= 0 => *offset as usize,
                    None => 0,
                    _ => {
                        return Err(QueryError::Execution(
                            "expression-index offset must be a non-negative integer".to_string(),
                        ));
                    }
                };
                if *limit < 0 {
                    return Err(QueryError::Execution(
                        "expression-index limit must be a non-negative integer".to_string(),
                    ));
                }
                let rids = self
                    .catalog
                    .expression_index_ordered_rids_bounded(
                        table,
                        index.index_id,
                        if *descending {
                            IndexOrderDirection::Desc
                        } else {
                            IndexOrderDirection::Asc
                        },
                        offset,
                        *limit as usize,
                    )
                    .map_err(QueryError::from_storage_io)?;
                (rids, None)
            }
            _ => unreachable!("expression-index plan checked above"),
        };

        let root_index =
            schema
                .column_index(&path.column)
                .ok_or_else(|| QueryError::ColumnNotFound {
                    table: table.clone(),
                    column: path.column.clone(),
                })?;
        let path_expr = stored_json_path_expr(path);
        let mut rows = Vec::with_capacity(rids.len());
        let mut cancel = CancelCheck::new();
        for rid in rids {
            cancel.tick()?;
            match &projection {
                Some((projected_indices, _)) => {
                    let mut fetch_indices = projected_indices.clone();
                    let root_position = fetch_indices.iter().position(|index| *index == root_index);
                    let root_position = match root_position {
                        Some(position) => position,
                        None => {
                            fetch_indices.push(root_index);
                            fetch_indices.len() - 1
                        }
                    };
                    let Some(mut fetched) = self
                        .catalog
                        .get_projected(table, rid, &fetch_indices)
                        .map_err(QueryError::from_storage_io)?
                    else {
                        continue;
                    };
                    if let Some((start, start_inclusive, end, end_inclusive)) = &range {
                        let value = eval_expr(
                            &path_expr,
                            std::slice::from_ref(&fetched[root_position]),
                            std::slice::from_ref(&path.column),
                        );
                        if value.is_empty()
                            || !range_matches(&value, start, *start_inclusive, end, *end_inclusive)
                        {
                            continue;
                        }
                    }
                    fetched.truncate(projected_indices.len());
                    rows.push(fetched);
                }
                None => {
                    let Some(row) = self.catalog.get(table, rid) else {
                        continue;
                    };
                    if let Some((start, start_inclusive, end, end_inclusive)) = &range {
                        let value = eval_expr(&path_expr, &row, &all_columns);
                        if value.is_empty()
                            || !range_matches(&value, start, *start_inclusive, end, *end_inclusive)
                        {
                            continue;
                        }
                    }
                    rows.push(row);
                }
            }
        }

        let columns = projection
            .map(|(_, columns)| columns)
            .unwrap_or(all_columns);
        Ok(Some(QueryResult::Rows { columns, rows }))
    }

    /// Lane A residual-recheck fast path for `Filter(<equality index scan>)`.
    ///
    /// The index narrows the candidate rids; the residual predicate is then
    /// re-checked while decoding only the columns it references
    /// (`get_projected`), and only the rows that pass are fully materialized.
    /// A non-matching candidate never pays a full-row decode, the win that
    /// turns a driven conjunction into single-digit milliseconds.
    ///
    /// Returns `None` for any shape it does not accelerate (range-driven scans,
    /// unresolved indexes, subquery predicates), deferring to the general
    /// Filter path, which stays correct in every case. `get_projected`
    /// reassembles spilled columns, so this path is overflow-safe and needs no
    /// v2 gating. Output rows and their order match the general path exactly.
    pub(crate) fn try_filter_index_residual_fast(
        &self,
        input: &PlanNode,
        predicate: &Expr,
    ) -> Result<Option<QueryResult>, QueryError> {
        if self.generic_path_forced("filter-index-residual") {
            return Ok(None);
        }
        // A subquery residual cannot be evaluated row-at-a-time here; let the
        // general path materialize it.
        if contains_subquery(predicate) {
            return Ok(None);
        }
        let (table, rids) = match input {
            PlanNode::IndexScan { table, column, key } => {
                let Some(tbl) = self.catalog.get_table(table) else {
                    return Ok(None);
                };
                if !tbl.has_index(column) {
                    return Ok(None);
                }
                let key_value = literal_to_value(key)?;
                (table.as_str(), tbl.index_lookup_all(column, &key_value))
            }
            PlanNode::ExprIndexScan { table, path, key } => {
                let Some(index) = resolve_expression_index(&self.catalog, table, path) else {
                    return Ok(None);
                };
                let key_value = literal_to_value(key)?;
                let rids = if key_value.is_empty() {
                    self.catalog
                        .expression_index_btree(table, index.index_id)
                        .ok_or_else(|| {
                            QueryError::Execution("expression index disappeared".to_string())
                        })?
                        .empty_rids()
                        .to_vec()
                } else {
                    self.catalog
                        .expression_index_lookup_all(table, index.index_id, &key_value)
                        .map_err(QueryError::from_storage_io)?
                };
                (table.as_str(), rids)
            }
            _ => return Ok(None),
        };

        let schema = self
            .catalog
            .schema(table)
            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
            .clone();
        let all_columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
        // Decode only the columns the residual touches when rechecking; the
        // matching rows are materialized in full afterwards.
        let residual_indices = predicate_column_indices_json(predicate, &all_columns);
        let residual_names: Vec<String> = residual_indices
            .iter()
            .map(|&index| all_columns[index].clone())
            .collect();

        let mut rows: Vec<Vec<Value>> = Vec::new();
        // Cooperative cancellation: a driving key with many matching rids can
        // fetch a large candidate set, so this loop must stay stoppable.
        let mut cancel = CancelCheck::new();
        for rid in rids {
            cancel.tick()?;
            let Some(sparse) = self
                .catalog
                .get_projected(table, rid, &residual_indices)
                .map_err(QueryError::from_storage_io)?
            else {
                continue;
            };
            if eval_predicate(predicate, &sparse, &residual_names) {
                if let Some(full) = self.catalog.get(table, rid) {
                    rows.push(full);
                }
            }
        }
        Ok(Some(QueryResult::Rows {
            columns: all_columns,
            rows,
        }))
    }

    fn charge_provenance(&self, rows: &ProvenanceRows) -> Result<(), QueryError> {
        let aliases =
            rows.source_aliases
                .iter()
                .fold(std::mem::size_of::<Vec<String>>(), |total, alias| {
                    total
                        .saturating_add(std::mem::size_of::<String>())
                        .saturating_add(alias.capacity())
                });
        let per_row = std::mem::size_of::<Vec<Option<RowId>>>().saturating_add(
            rows.source_aliases
                .len()
                .saturating_mul(std::mem::size_of::<Option<RowId>>()),
        );
        mem_budget::charge(
            aliases.saturating_add(rows.provenance.len().saturating_mul(per_row)),
            self.query_memory_limit(),
        )
    }

    fn provenance_scan(
        &self,
        table: &str,
        alias: &str,
        qualify_columns: bool,
    ) -> Result<ProvenanceRows, QueryError> {
        let schema = self
            .catalog
            .schema(table)
            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
            .clone();
        let columns = schema
            .columns
            .iter()
            .map(|column| {
                if qualify_columns {
                    format!("{alias}.{}", column.name)
                } else {
                    column.name.clone()
                }
            })
            .collect();
        let mut rows = Vec::new();
        let mut provenance = Vec::new();
        let mut cancel = CancelCheck::new();
        for (rid, row) in self
            .catalog
            .scan(table)
            .map_err(QueryError::from_storage_io)?
        {
            cancel.tick()?;
            rows.push(row);
            provenance.push(vec![Some(rid)]);
        }
        let result = ProvenanceRows {
            columns,
            rows,
            source_aliases: vec![alias.to_string()],
            provenance,
        };
        Ok(result)
    }

    pub(crate) fn materialize_rows_with_provenance(
        &self,
        plan: &PlanNode,
    ) -> Result<ProvenanceRows, QueryError> {
        let result = match plan {
            PlanNode::SeqScan { table } => self.provenance_scan(table, table, false)?,
            PlanNode::AliasScan { table, alias } => self.provenance_scan(table, alias, true)?,
            PlanNode::IndexScan { table, column, key } => {
                let fallback = PlanNode::Filter {
                    input: Box::new(PlanNode::SeqScan {
                        table: table.clone(),
                    }),
                    predicate: Expr::BinaryOp(
                        Box::new(Expr::Field(column.clone())),
                        BinOp::Eq,
                        Box::new(key.clone()),
                    ),
                };
                self.materialize_rows_with_provenance(&fallback)?
            }
            PlanNode::RangeScan {
                table,
                column,
                start,
                end,
            } => {
                let fallback = PlanNode::Filter {
                    input: Box::new(PlanNode::SeqScan {
                        table: table.clone(),
                    }),
                    predicate: synthesize_range_predicate(column, start, end),
                };
                self.materialize_rows_with_provenance(&fallback)?
            }
            PlanNode::ExprIndexScan { .. }
            | PlanNode::ExprRangeScan { .. }
            | PlanNode::OrderedExprIndexScan { .. } => {
                let fallback = expression_index_fallback(plan)
                    .expect("expression-index branch always has a fallback");
                self.materialize_rows_with_provenance(&fallback)?
            }
            PlanNode::Filter { input, predicate } => {
                if contains_subquery(predicate) {
                    return Err(QueryError::Execution(
                        "symmetric aggregation over a subquery filter is not supported; use raw"
                            .to_string(),
                    ));
                }
                let input = self.materialize_rows_with_provenance(input)?;
                let mut rows = Vec::new();
                let mut provenance = Vec::new();
                let mut cancel = CancelCheck::new();
                for (row, row_provenance) in input.rows.into_iter().zip(input.provenance) {
                    cancel.tick()?;
                    if eval_predicate(predicate, &row, &input.columns) {
                        rows.push(row);
                        provenance.push(row_provenance);
                    }
                }
                ProvenanceRows {
                    columns: input.columns,
                    rows,
                    source_aliases: input.source_aliases,
                    provenance,
                }
            }
            PlanNode::Project { input, fields } => {
                let input = self.materialize_rows_with_provenance(input)?;
                let columns = fields
                    .iter()
                    .map(|field| {
                        field.alias.clone().unwrap_or_else(|| match &field.expr {
                            Expr::Field(name) => name.clone(),
                            Expr::QualifiedField { qualifier, field } => {
                                format!("{qualifier}.{field}")
                            }
                            _ => expression_output_name(&field.expr),
                        })
                    })
                    .collect();
                let mut rows = Vec::with_capacity(input.rows.len());
                let mut cancel = CancelCheck::new();
                for row in &input.rows {
                    cancel.tick()?;
                    rows.push(
                        fields
                            .iter()
                            .map(|field| eval_expr(&field.expr, row, &input.columns))
                            .collect(),
                    );
                }
                ProvenanceRows {
                    columns,
                    rows,
                    source_aliases: input.source_aliases,
                    provenance: input.provenance,
                }
            }
            PlanNode::Sort { input, keys } => {
                let input = self.materialize_rows_with_provenance(input)?;
                if input.rows.len() > MAX_SORT_ROWS {
                    return Err(QueryError::SortLimitExceeded);
                }
                self.charge_rows(&input.rows)?;
                let mut paired: Vec<_> = input.rows.into_iter().zip(input.provenance).collect();
                cooperative_stable_sort_by(
                    &mut paired,
                    self.query_memory_limit(),
                    |(left, _), (right, _)| {
                        for key in keys {
                            let left_value = eval_expr(&key.expr, left, &input.columns);
                            let right_value = eval_expr(&key.expr, right, &input.columns);
                            let comparison =
                                compare_order_values(&left_value, &right_value, key.descending);
                            if comparison != std::cmp::Ordering::Equal {
                                return comparison;
                            }
                        }
                        std::cmp::Ordering::Equal
                    },
                )?;
                let (rows, provenance) = paired.into_iter().unzip();
                ProvenanceRows {
                    columns: input.columns,
                    rows,
                    source_aliases: input.source_aliases,
                    provenance,
                }
            }
            PlanNode::Limit { input, count } | PlanNode::Offset { input, count } => {
                let input_rows = self.materialize_rows_with_provenance(input)?;
                let Expr::Literal(Literal::Int(count)) = count else {
                    return Err(QueryError::Execution(
                        "limit/offset must be an integer literal".to_string(),
                    ));
                };
                let count = *count as usize;
                let is_limit = matches!(plan, PlanNode::Limit { .. });
                let iterator = input_rows.rows.into_iter().zip(input_rows.provenance);
                let (rows, provenance) = if is_limit {
                    iterator.take(count).unzip()
                } else {
                    iterator.skip(count).unzip()
                };
                ProvenanceRows {
                    columns: input_rows.columns,
                    rows,
                    source_aliases: input_rows.source_aliases,
                    provenance,
                }
            }
            PlanNode::Distinct { input } => {
                let input = self.materialize_rows_with_provenance(input)?;
                let mut seen = HashSet::new();
                let mut rows = Vec::new();
                let mut provenance = Vec::new();
                let mut cancel = CancelCheck::new();
                for (row, row_provenance) in input.rows.into_iter().zip(input.provenance) {
                    cancel.tick()?;
                    if seen.insert(row.clone()) {
                        rows.push(row);
                        provenance.push(row_provenance);
                    }
                }
                ProvenanceRows {
                    columns: input.columns,
                    rows,
                    source_aliases: input.source_aliases,
                    provenance,
                }
            }
            PlanNode::Union { left, right, all } => {
                let mut left_rows = self.materialize_rows_with_provenance(left)?;
                let right_rows = self.materialize_rows_with_provenance(right)?;
                if left_rows.columns.len() != right_rows.columns.len() {
                    return Err(QueryError::Execution(
                        "union sides must have the same number of columns".to_string(),
                    ));
                }
                if left_rows.source_aliases != right_rows.source_aliases {
                    return Err(QueryError::Execution(
                        "symmetric aggregation over union requires matching source aliases; use raw"
                            .to_string(),
                    ));
                }
                left_rows.rows.extend(right_rows.rows);
                left_rows.provenance.extend(right_rows.provenance);
                if !all {
                    let mut seen = HashSet::new();
                    let mut rows = Vec::new();
                    let mut provenance = Vec::new();
                    for (row, row_provenance) in
                        left_rows.rows.into_iter().zip(left_rows.provenance)
                    {
                        if seen.insert(row.clone()) {
                            rows.push(row);
                            provenance.push(row_provenance);
                        }
                    }
                    left_rows.rows = rows;
                    left_rows.provenance = provenance;
                }
                left_rows
            }
            PlanNode::NestedLoopJoin {
                left,
                right,
                on,
                kind,
            } => {
                let left = self.materialize_rows_with_provenance(left)?;
                let right = self.materialize_rows_with_provenance(right)?;
                execute_provenance_join(
                    left,
                    right,
                    on.as_ref(),
                    *kind,
                    self.nested_loop_pair_limit,
                )?
            }
            _ => {
                return Err(QueryError::Execution(
                    "symmetric aggregation input shape is not supported; use raw".to_string(),
                ));
            }
        };
        self.charge_provenance(&result)?;
        Ok(result)
    }

    /// `schema` — one result row per type: name + column count. Read-only;
    /// reads live catalog state, so a cached plan can never serve a stale list.
    pub(crate) fn introspect_list_types(&self) -> Result<QueryResult, QueryError> {
        let rows: Vec<Vec<Value>> = self
            .catalog
            .list_tables()
            .iter()
            .map(|name| {
                let cols = self
                    .catalog
                    .schema(name)
                    .map(|s| s.columns.len())
                    .unwrap_or(0) as i64;
                vec![Value::Str((*name).to_string()), Value::Int(cols)]
            })
            .collect();
        Ok(QueryResult::Rows {
            columns: vec!["name".to_string(), "columns".to_string()],
            rows,
        })
    }

    /// `describe <Type>` — one result row per column: name, type, nullability,
    /// and index kind (`unique` / `index` / empty). Entity links touching the
    /// type are appended after the column rows with `type = "link"`: outgoing
    /// links first (named bare, detail `-> Target (...)`), then links from
    /// other types targeting it (named `Owner.name`, detail `<- Owner (...)`).
    /// Column rows are byte-identical to a link-free catalog. Read-only.
    pub(crate) fn introspect_describe(&self, table: &str) -> Result<QueryResult, QueryError> {
        let schema = self
            .catalog
            .schema(table)
            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
        let mut rows: Vec<Vec<Value>> = schema
            .columns
            .iter()
            .map(|c| {
                let index = if self.catalog.has_index(table, &c.name) {
                    match self.catalog.is_index_unique(table, &c.name) {
                        Some(true) => "unique",
                        _ => "index",
                    }
                } else {
                    ""
                };
                vec![
                    Value::Str(c.name.clone()),
                    Value::Str(type_id_to_name(c.type_id).to_string()),
                    Value::Bool(!c.required),
                    Value::Str(index.to_string()),
                ]
            })
            .collect();
        let mut outgoing: Vec<&LinkDef> = self
            .catalog
            .links()
            .filter(|l| l.owner_type == table)
            .collect();
        outgoing.sort_by(|a, b| a.name.cmp(&b.name));
        for link in outgoing {
            rows.push(vec![
                Value::Str(link.name.clone()),
                Value::Str("link".to_string()),
                Value::Empty,
                Value::Str(format!(
                    "-> {} ({}, {} -> {})",
                    link.target_type,
                    // GATE B3: derived live, never `link.kind` (advisory, stale
                    // by design). See `Catalog::derive_link_kind`.
                    link_cardinality(
                        self.catalog
                            .derive_link_kind(&link.target_type, &link.target_key)
                    ),
                    link.local_key,
                    link.target_key
                )),
            ]);
        }
        let mut incoming: Vec<&LinkDef> = self
            .catalog
            .links()
            .filter(|l| l.target_type == table && l.owner_type != table)
            .collect();
        incoming.sort_by(|a, b| (&a.owner_type, &a.name).cmp(&(&b.owner_type, &b.name)));
        for link in incoming {
            rows.push(vec![
                Value::Str(format!("{}.{}", link.owner_type, link.name)),
                Value::Str("link".to_string()),
                Value::Empty,
                Value::Str(format!(
                    "<- {} ({}, {} -> {})",
                    link.owner_type,
                    // GATE B4: the incoming-link loop is a separate site from
                    // the outgoing one above and derives live for the same
                    // reason.
                    link_cardinality(
                        self.catalog
                            .derive_link_kind(&link.target_type, &link.target_key)
                    ),
                    link.local_key,
                    link.target_key
                )),
            ]);
        }
        Ok(QueryResult::Rows {
            columns: vec![
                "column".to_string(),
                "type".to_string(),
                "nullable".to_string(),
                "index".to_string(),
            ],
            rows,
        })
    }

    /// `schema links`: one result row per declared entity link, ordered by
    /// owner then link name so output is stable across runs and restarts.
    /// Read-only; reads live catalog state, so a cached plan is never stale.
    pub(crate) fn introspect_list_links(&self) -> Result<QueryResult, QueryError> {
        let mut links: Vec<&LinkDef> = self.catalog.links().collect();
        links.sort_by(|a, b| (&a.owner_type, &a.name).cmp(&(&b.owner_type, &b.name)));
        let rows: Vec<Vec<Value>> = links
            .iter()
            .map(|l| {
                vec![
                    Value::Str(l.owner_type.clone()),
                    Value::Str(l.name.clone()),
                    Value::Str(l.target_type.clone()),
                    Value::Str(l.local_key.clone()),
                    Value::Str(l.target_key.clone()),
                    // GATE B5: `schema links` derives live too.
                    Value::Str(
                        link_cardinality(
                            self.catalog.derive_link_kind(&l.target_type, &l.target_key),
                        )
                        .to_string(),
                    ),
                ]
            })
            .collect();
        Ok(QueryResult::Rows {
            columns: vec![
                "owner".to_string(),
                "name".to_string(),
                "target".to_string(),
                "local_key".to_string(),
                "target_key".to_string(),
                "cardinality".to_string(),
            ],
            rows,
        })
    }
}

/// Human-readable cardinality label used by both link-introspection surfaces.
fn link_cardinality(kind: LinkKind) -> &'static str {
    match kind {
        LinkKind::ToOne => "to-one",
        LinkKind::ToMany => "to-many",
    }
}
