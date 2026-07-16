//! The execute_plan method and associated helpers.

use crate::ast::*;
use crate::cancel::CancelCheck;
use crate::plan::*;
use crate::planner::{
    extract_single_bound, range_scan_for_target, try_extract_eq_index_key, RangeBound, RangeTarget,
};
use crate::result::{QueryError, QueryResult};
use powdb_storage::btree::IndexStats;
use powdb_storage::catalog::{Catalog, ExpressionIndexMeta, IndexOrderDirection};
use powdb_storage::row::{decode_column, decode_row, patch_var_column_in_place, RowLayout};
use powdb_storage::stored_json_path::StoredJsonPathV1;
use powdb_storage::types::*;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::ops::ControlFlow;

use super::compiled::*;
use super::eval::*;
use super::row_body_base;
use super::{check_join_limit, mem_budget, Engine, MAX_SORT_ROWS};
use powdb_storage::view::ViewDef;

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
pub(super) struct ProvenanceRows {
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

impl Engine {
    pub(super) fn execute_expression_index_plan(
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
                        .map_err(|error| QueryError::StorageError(error.to_string()))?
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
                    .map_err(|error| QueryError::StorageError(error.to_string()))?;
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
                    .map_err(|error| QueryError::StorageError(error.to_string()))?;
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
                        .map_err(|error| QueryError::StorageError(error.to_string()))?
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
    pub(super) fn try_filter_index_residual_fast(
        &self,
        input: &PlanNode,
        predicate: &Expr,
    ) -> Result<Option<QueryResult>, QueryError> {
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
                        .map_err(|error| QueryError::StorageError(error.to_string()))?
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
                .map_err(|error| QueryError::StorageError(error.to_string()))?
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
            .map_err(|error| QueryError::StorageError(error.to_string()))?
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

    pub(super) fn materialize_rows_with_provenance(
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
    pub(super) fn introspect_list_types(&self) -> Result<QueryResult, QueryError> {
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
    /// and index kind (`unique` / `index` / empty). Read-only.
    pub(super) fn introspect_describe(&self, table: &str) -> Result<QueryResult, QueryError> {
        let schema = self
            .catalog
            .schema(table)
            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
        let rows: Vec<Vec<Value>> = schema
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

    pub fn execute_plan(&mut self, plan: &PlanNode) -> Result<QueryResult, QueryError> {
        // Refuse any plan whose evaluable expressions still carry an aggregate
        // FunctionCall the grouped-aggregate planner could not lower. Without
        // this, such an aggregate would reach eval_expr and silently evaluate
        // to Empty (a wrong answer). The outermost call validates the whole
        // tree before any row is produced.
        validate_no_stray_aggregates(plan)?;
        validate_json_path_types(&self.catalog, plan)?;
        match plan {
            PlanNode::ExprIndexScan { .. }
            | PlanNode::ExprRangeScan { .. }
            | PlanNode::OrderedExprIndexScan { .. } => {
                if let Some(result) = self.execute_expression_index_plan(plan, None)? {
                    return Ok(result);
                }
                let fallback = expression_index_fallback(plan)
                    .expect("expression-index branch always has a fallback");
                self.execute_plan(&fallback)
            }
            PlanNode::SeqScan { table } => {
                // Auto-refresh dirty materialized views on read.
                if self.view_registry.is_dirty(table) {
                    self.refresh_view(table)?;
                }
                let schema = self
                    .catalog
                    .schema(table)
                    .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
                    .clone();
                let columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
                // Cooperative cancellation: a full-table scan of a huge table
                // must stay stoppable.
                let mut cancel = CancelCheck::new();
                let mut rows: Vec<Vec<Value>> = Vec::new();
                for (_, row) in self
                    .catalog
                    .scan(table)
                    .map_err(|e| QueryError::StorageError(e.to_string()))?
                {
                    cancel.tick()?;
                    rows.push(row);
                }
                Ok(QueryResult::Rows { columns, rows })
            }

            PlanNode::Filter { input, predicate } => {
                // Materialize any IN-subqueries in the predicate before the
                // scan loop — the closure can't call back into the engine.
                // Correlated subqueries are left in place for per-row eval.
                let materialized;
                let predicate = if contains_subquery(predicate) {
                    materialized = self.materialize_subqueries(predicate)?;
                    &materialized
                } else {
                    predicate
                };

                // Correlated subquery path: per-row materialisation.
                if contains_subquery(predicate) {
                    let result = self.execute_plan(input)?;
                    return match result {
                        QueryResult::Rows { columns, rows } => {
                            let mut filtered = Vec::new();
                            // Cooperative cancellation: a subquery runs per outer
                            // row, so a large outer scan must stay stoppable.
                            let mut cancel = CancelCheck::new();
                            for row in rows {
                                cancel.tick()?;
                                let row_pred =
                                    self.materialize_correlated_for_row(predicate, &row, &columns)?;
                                if eval_predicate(&row_pred, &row, &columns) {
                                    filtered.push(row);
                                }
                            }
                            Ok(QueryResult::Rows {
                                columns,
                                rows: filtered,
                            })
                        }
                        _ => Err("filter requires row input".into()),
                    };
                }

                // Lane A fast path: Filter over an equality-driven index scan.
                // The index narrows the candidate rids; the residual is
                // re-checked with a partial decode, full rows only for matches.
                if matches!(
                    input.as_ref(),
                    PlanNode::IndexScan { .. } | PlanNode::ExprIndexScan { .. }
                ) {
                    if let Some(result) = self.try_filter_index_residual_fast(input, predicate)? {
                        return Ok(result);
                    }
                }

                // Fast path: fuse Filter + SeqScan into a zero-copy streaming
                // loop. Uses decode_column() to evaluate the predicate on only
                // the columns it references, avoiding heap allocations for
                // String/Bytes columns that aren't part of the filter.
                // Overflow safety (P0-4/P1): v2-capable tables fall through to
                // the decoded general Filter path below — the raw fast path
                // rehydrates to v1 and drops/mis-reads >= 64KB spilled values.
                if let PlanNode::SeqScan { table } = input.as_ref() {
                    if !self.catalog.table_has_overflow(table) {
                        // Auto-refresh dirty materialized views.
                        if self.view_registry.is_dirty(table) {
                            self.refresh_view(table)?;
                        }
                        let schema = self
                            .catalog
                            .schema(table)
                            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
                            .clone();
                        let columns: Vec<String> =
                            schema.columns.iter().map(|c| c.name.clone()).collect();
                        let fast = FastLayout::new(&schema);
                        let row_layout = RowLayout::new(&schema);
                        // Mission F: pre-size to skip the first 4 Vec doublings
                        // (4 → 8 → 16 → 32 → 64). On a 100K-row scan with 30%
                        // selectivity that's ~4 fewer reallocations + memcpys.
                        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(64);

                        // Try compiled predicate for the filter check (handles
                        // int leaves, string-eq leaves, and And conjunctions).
                        // Cooperative cancellation: a full-table compiled/
                        // selective predicate scan must stay stoppable, so use
                        // the early-terminating scan and break on cancel. The
                        // captured error is surfaced after the scan returns.
                        let mut cancel = CancelCheck::new();
                        let mut cancel_err: Option<QueryError> = None;
                        if let Some(compiled) =
                            compile_predicate(predicate, &columns, &fast, &schema)
                        {
                            self.catalog
                                .try_for_each_row_raw(table, |_rid, data| {
                                    if let Err(e) = cancel.tick() {
                                        cancel_err = Some(e);
                                        return ControlFlow::Break(());
                                    }
                                    if compiled(data) {
                                        rows.push(decode_row(&schema, data));
                                    }
                                    ControlFlow::Continue(())
                                })
                                .map_err(|e| QueryError::StorageError(e.to_string()))?;
                        } else {
                            let pred_cols = predicate_column_indices_json(predicate, &columns);
                            self.catalog
                                .try_for_each_row_raw(table, |_rid, data| {
                                    if let Err(e) = cancel.tick() {
                                        cancel_err = Some(e);
                                        return ControlFlow::Break(());
                                    }
                                    let pred_row =
                                        decode_selective(&schema, &row_layout, data, &pred_cols);
                                    if eval_predicate(predicate, &pred_row, &columns) {
                                        rows.push(decode_row(&schema, data));
                                    }
                                    ControlFlow::Continue(())
                                })
                                .map_err(|e| QueryError::StorageError(e.to_string()))?;
                        }
                        if let Some(e) = cancel_err {
                            return Err(e);
                        }

                        return Ok(QueryResult::Rows { columns, rows });
                    }
                }

                // General path: materialise then filter.
                let result = self.execute_plan(input)?;
                match result {
                    QueryResult::Rows { columns, rows } => {
                        let mut cancel = CancelCheck::new();
                        let mut filtered: Vec<Vec<Value>> = Vec::new();
                        for row in rows {
                            cancel.tick()?;
                            if eval_predicate(predicate, &row, &columns) {
                                filtered.push(row);
                            }
                        }
                        Ok(QueryResult::Rows {
                            columns,
                            rows: filtered,
                        })
                    }
                    _ => Err("filter requires row input".into()),
                }
            }

            PlanNode::Project { input, fields } => {
                if matches!(
                    input.as_ref(),
                    PlanNode::ExprIndexScan { .. }
                        | PlanNode::ExprRangeScan { .. }
                        | PlanNode::OrderedExprIndexScan { .. }
                ) {
                    if let Some(result) = self.execute_expression_index_plan(input, Some(fields))? {
                        return Ok(result);
                    }
                }
                // Fast path: Project over IndexScan — decode only projected
                // columns from raw bytes instead of full decode_row.
                if let PlanNode::IndexScan { table, column, key } = input.as_ref() {
                    let schema = self
                        .catalog
                        .schema(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
                        .clone();
                    let all_columns: Vec<String> =
                        schema.columns.iter().map(|c| c.name.clone()).collect();
                    let key_value = literal_to_value(key)?;
                    let tbl = self
                        .catalog
                        .get_table(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;

                    let proj_columns: Vec<String> = fields
                        .iter()
                        .map(|f| {
                            f.alias.clone().unwrap_or_else(|| match &f.expr {
                                Expr::Field(name) => name.clone(),
                                _ => "?".into(),
                            })
                        })
                        .collect();

                    // Determine which column indices the projection needs
                    let proj_indices: Vec<usize> = fields
                        .iter()
                        .filter_map(|f| {
                            if let Expr::Field(name) = &f.expr {
                                all_columns.iter().position(|c| c == name)
                            } else {
                                None
                            }
                        })
                        .collect();

                    // Only serve plain-field projections here; a computed
                    // projection (e.g. `length(.v)`) must fall through to the
                    // generic expression-evaluating path — otherwise its column
                    // is silently dropped (proj_indices only collects Fields).
                    let all_plain_fields = fields.iter().all(|f| matches!(f.expr, Expr::Field(_)));
                    if tbl.has_index(column) && all_plain_fields {
                        let rids = tbl.index_lookup_all(column, &key_value);
                        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(rids.len());
                        let mut cancel = CancelCheck::new();
                        for rid in rids {
                            cancel.tick()?;
                            // Overflow safety (P0-3/P0-4): `tbl.get` reassembles
                            // spilled columns from their overflow chains. The old
                            // `heap.get` + `decode_column` read raw v2 bytes and
                            // returned Empty for a spilled column (or wrapped a
                            // >= 64KB value).
                            if let Some(full) = tbl.get(rid) {
                                let row: Vec<Value> =
                                    proj_indices.iter().map(|&ci| full[ci].clone()).collect();
                                rows.push(row);
                            }
                        }
                        return Ok(QueryResult::Rows {
                            columns: proj_columns,
                            rows,
                        });
                    }
                }

                // Fast path: Project(Limit(Sort(Filter(SeqScan)))) — bounded
                // top-N heap. Decodes only the sort key + projected columns,
                // keeps at most `limit` rows in a heap. Also handles the
                // Project(Limit(Sort(SeqScan))) variant (no filter).
                if let PlanNode::Limit {
                    input: inner,
                    count: limit_expr,
                } = input.as_ref()
                {
                    if let PlanNode::Sort {
                        input: sort_input,
                        keys,
                    } = inner.as_ref()
                    {
                        // Fast path only for single-key sorts
                        if keys.len() == 1 {
                            if let Expr::Field(sort_field) = &keys[0].expr {
                                let descending = keys[0].descending;
                                let limit = match limit_expr {
                                    Expr::Literal(Literal::Int(v)) if *v >= 0 => *v as usize,
                                    _ => usize::MAX,
                                };
                                let (table_opt, pred_opt): (Option<&str>, Option<&Expr>) =
                                    match sort_input.as_ref() {
                                        PlanNode::SeqScan { table } => (Some(table.as_str()), None),
                                        PlanNode::Filter {
                                            input: fi,
                                            predicate,
                                        } => {
                                            if let PlanNode::SeqScan { table } = fi.as_ref() {
                                                (Some(table.as_str()), Some(predicate))
                                            } else {
                                                (None, None)
                                            }
                                        }
                                        _ => (None, None),
                                    };
                                if let Some(table) = table_opt {
                                    if let Some(result) = self.project_filter_sort_limit_fast(
                                        table, fields, sort_field, descending, limit, pred_opt,
                                    )? {
                                        return Ok(result);
                                    }
                                }
                            }
                        }
                    }
                    // Fast path: Project(Limit(Filter(SeqScan))) — stream,
                    // decode only projected columns, stop at limit.
                    if let PlanNode::Filter {
                        input: fi,
                        predicate,
                    } = inner.as_ref()
                    {
                        if let PlanNode::SeqScan { table } = fi.as_ref() {
                            let limit = match limit_expr {
                                Expr::Literal(Literal::Int(v)) if *v >= 0 => *v as usize,
                                _ => usize::MAX,
                            };
                            if let Some(result) = self.project_filter_limit_fast(
                                table,
                                fields,
                                limit,
                                Some(predicate),
                            )? {
                                return Ok(result);
                            }
                        }
                    }
                    // Fast path: Project(Limit(SeqScan)) — stream, no filter.
                    if let PlanNode::SeqScan { table } = inner.as_ref() {
                        let limit = match limit_expr {
                            Expr::Literal(Literal::Int(v)) if *v >= 0 => *v as usize,
                            _ => usize::MAX,
                        };
                        if let Some(result) =
                            self.project_filter_limit_fast(table, fields, limit, None)?
                        {
                            return Ok(result);
                        }
                    }
                }

                // Mission D4: Project(Filter(SeqScan)) without Limit. Reuses
                // `project_filter_limit_fast` with limit = usize::MAX so the
                // hot loop decodes only projected columns and uses the
                // compiled predicate. Previously this fell through to the
                // generic Filter branch which materialised every column via
                // `decode_row` then re-projected — quadratic work.
                //
                // multi_col_and_filter (`U filter .age > 30 and .status =
                // "active" { .name, .age }`) was 6.18ms (0.7x SQLite) and
                // is the load-bearing workload for this fast path.
                if let PlanNode::Filter {
                    input: fi,
                    predicate,
                } = input.as_ref()
                {
                    if let PlanNode::SeqScan { table } = fi.as_ref() {
                        if let Some(result) = self.project_filter_limit_fast(
                            table,
                            fields,
                            usize::MAX,
                            Some(predicate),
                        )? {
                            return Ok(result);
                        }
                    }
                }

                // Mission D4: Project(SeqScan) without Filter or Limit.
                // Decode only projected columns; the previous fall-through
                // built full Vec<Value> rows then re-projected.
                if let PlanNode::SeqScan { table } = input.as_ref() {
                    if let Some(result) =
                        self.project_filter_limit_fast(table, fields, usize::MAX, None)?
                    {
                        return Ok(result);
                    }
                }

                let result = self.execute_plan(input)?;
                match result {
                    QueryResult::Rows { columns, rows } => {
                        let proj_columns: Vec<String> = fields
                            .iter()
                            .map(|f| {
                                f.alias.clone().unwrap_or_else(|| match &f.expr {
                                    Expr::Field(name) => name.clone(),
                                    // Mission E1.2: `{ u.name }` projects as the
                                    // qualified column name so callers can still
                                    // disambiguate across the join output.
                                    Expr::QualifiedField { qualifier, field } => {
                                        format!("{qualifier}.{field}")
                                    }
                                    _ => "?".into(),
                                })
                            })
                            .collect();
                        let mut cancel = CancelCheck::new();
                        let mut proj_rows: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
                        for row in &rows {
                            cancel.tick()?;
                            proj_rows.push(
                                fields
                                    .iter()
                                    .map(|f| eval_expr(&f.expr, row, &columns))
                                    .collect(),
                            );
                        }
                        Ok(QueryResult::Rows {
                            columns: proj_columns,
                            rows: proj_rows,
                        })
                    }
                    _ => Err("project requires row input".into()),
                }
            }

            PlanNode::Sort { input, keys } => {
                let result = self.execute_plan(input)?;
                match result {
                    QueryResult::Rows { columns, mut rows } => {
                        // WS2: row-count cap is a cheap secondary guard; the
                        // byte budget is the real OOM defense for the sort
                        // buffer (a few very large rows pass the row cap).
                        if rows.len() > MAX_SORT_ROWS {
                            return Err(QueryError::SortLimitExceeded);
                        }
                        self.charge_rows(&rows)?;
                        let key_specs: Vec<(Option<usize>, &Expr, bool)> = keys
                            .iter()
                            .map(|k| {
                                let stored_name = match &k.expr {
                                    Expr::Field(name) => Some(name.clone()),
                                    Expr::QualifiedField { qualifier, field } => {
                                        Some(format!("{qualifier}.{field}"))
                                    }
                                    _ => None,
                                };
                                let index = stored_name
                                    .as_ref()
                                    .and_then(|name| columns.iter().position(|c| c == name));
                                if let Some(name) = stored_name {
                                    if index.is_none() {
                                        return Err(QueryError::ColumnNotFound {
                                            table: String::new(),
                                            column: name,
                                        });
                                    }
                                }
                                Ok((index, &k.expr, k.descending))
                            })
                            .collect::<Result<_, QueryError>>()?;
                        cooperative_stable_sort_by(&mut rows, self.query_memory_limit, |a, b| {
                            for &(col_idx, expr, descending) in &key_specs {
                                let (left_value, right_value) = match col_idx {
                                    Some(index) => (&a[index], &b[index]),
                                    None => {
                                        let left = eval_expr(expr, a, &columns);
                                        let right = eval_expr(expr, b, &columns);
                                        let cmp = compare_order_values(&left, &right, descending);
                                        if cmp != std::cmp::Ordering::Equal {
                                            return cmp;
                                        }
                                        continue;
                                    }
                                };
                                let cmp = compare_order_values(left_value, right_value, descending);
                                if cmp != std::cmp::Ordering::Equal {
                                    return cmp;
                                }
                            }
                            std::cmp::Ordering::Equal
                        })?;
                        Ok(QueryResult::Rows { columns, rows })
                    }
                    _ => Err("sort requires row input".into()),
                }
            }

            PlanNode::Limit { input, count } => {
                let result = self.execute_plan(input)?;
                let n = match count {
                    Expr::Literal(Literal::Int(v)) => *v as usize,
                    _ => return Err("limit must be integer literal".into()),
                };
                match result {
                    QueryResult::Rows { columns, rows } => {
                        let mut cancel = CancelCheck::new();
                        let mut limited = Vec::with_capacity(n.min(rows.len()));
                        for row in rows.into_iter().take(n) {
                            cancel.tick()?;
                            limited.push(row);
                        }
                        Ok(QueryResult::Rows {
                            columns,
                            rows: limited,
                        })
                    }
                    _ => Err("limit requires row input".into()),
                }
            }

            PlanNode::Offset { input, count } => {
                let result = self.execute_plan(input)?;
                let n = match count {
                    Expr::Literal(Literal::Int(v)) => *v as usize,
                    _ => return Err("offset must be integer literal".into()),
                };
                match result {
                    QueryResult::Rows { columns, rows } => {
                        let mut cancel = CancelCheck::new();
                        let mut offset = Vec::with_capacity(rows.len().saturating_sub(n));
                        for (index, row) in rows.into_iter().enumerate() {
                            cancel.tick()?;
                            if index >= n {
                                offset.push(row);
                            }
                        }
                        Ok(QueryResult::Rows {
                            columns,
                            rows: offset,
                        })
                    }
                    _ => Err("offset requires row input".into()),
                }
            }

            PlanNode::Aggregate {
                input,
                function,
                argument,
                mode: _,
                provenance_alias,
            } => {
                if let Some(provenance_alias) = provenance_alias {
                    let input = self.materialize_rows_with_provenance(input)?;
                    self.charge_rows(&input.rows)?;
                    return aggregate_rows_with_provenance(
                        *function,
                        argument.as_ref(),
                        &input,
                        provenance_alias,
                        self.query_memory_limit(),
                    );
                }
                // Fast path: count() over SeqScan — count rows without any decode
                if *function == AggFunc::Count {
                    // Overflow safety (P0-4): the raw `for_each_row_raw` count
                    // drops any row too large to re-inline (>= 64KB) and would
                    // undercount; v2-capable tables use the decoded generic path.
                    if let PlanNode::SeqScan { table } = input.as_ref() {
                        if !self.catalog.table_has_overflow(table) {
                            // Auto-refresh a dirty materialized view before
                            // counting it — otherwise count(View) returns stale
                            // data after an underlying mutation (F3).
                            if self.view_registry.is_dirty(table) {
                                self.refresh_view(table)?;
                            }
                            let mut count: i64 = 0;
                            for_each_row_raw_cancellable(&self.catalog, table, |_rid, _data| {
                                count += 1;
                            })?;
                            return Ok(QueryResult::Scalar(Value::Int(count)));
                        }
                    }
                    // Fast path: count() over Filter(SeqScan) — try compiled
                    // predicate first, fall back to decode_column path.
                    // Skip a predicate carrying a subquery: the raw-bytes
                    // evaluators here don't materialise subqueries, so
                    // `count(T filter .x in (...))` would silently count 0
                    // (F1). Falling through routes it to the generic path
                    // that resolves the subquery correctly.
                    if let PlanNode::Filter {
                        input: inner,
                        predicate,
                    } = input.as_ref()
                    {
                        if let PlanNode::SeqScan { table } = inner.as_ref() {
                            if self.view_registry.is_dirty(table) {
                                self.refresh_view(table)?;
                            }
                        }
                        if let (PlanNode::SeqScan { table }, false) =
                            (inner.as_ref(), contains_subquery(predicate))
                        {
                            if !self.catalog.table_has_overflow(table) {
                                let schema = self
                                    .catalog
                                    .schema(table)
                                    .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
                                    .clone();
                                let columns: Vec<String> =
                                    schema.columns.iter().map(|c| c.name.clone()).collect();
                                let fast = FastLayout::new(&schema);
                                let row_layout = RowLayout::new(&schema);

                                // Try compiled predicate (zero-allocation hot path).
                                // Handles int leaves, string-eq leaves, AND conjunctions.
                                if let Some(compiled) =
                                    compile_predicate(predicate, &columns, &fast, &schema)
                                {
                                    let mut count: i64 = 0;
                                    for_each_row_raw_cancellable(
                                        &self.catalog,
                                        table,
                                        |_rid, data| {
                                            if compiled(data) {
                                                count += 1;
                                            }
                                        },
                                    )?;
                                    return Ok(QueryResult::Scalar(Value::Int(count)));
                                }

                                // Fallback: decode predicate columns
                                let pred_cols = predicate_column_indices_json(predicate, &columns);
                                let mut count: i64 = 0;
                                for_each_row_raw_cancellable(
                                    &self.catalog,
                                    table,
                                    |_rid, data| {
                                        let pred_row = decode_selective(
                                            &schema,
                                            &row_layout,
                                            data,
                                            &pred_cols,
                                        );
                                        if eval_predicate(predicate, &pred_row, &columns) {
                                            count += 1;
                                        }
                                    },
                                )?;

                                return Ok(QueryResult::Scalar(Value::Int(count)));
                            }
                        }
                    }
                }

                // Fast path: sum/avg/min/max over a single fixed-size int
                // column with an optional compiled filter predicate. Walks
                // raw row bytes, zero allocation per row.
                if matches!(
                    function,
                    AggFunc::Sum
                        | AggFunc::Avg
                        | AggFunc::Min
                        | AggFunc::Max
                        | AggFunc::CountDistinct
                ) {
                    if let Some(Expr::Field(col)) = argument.as_ref() {
                        // Shape: Aggregate(SeqScan) or Aggregate(Filter(SeqScan))
                        let (table_opt, pred_opt): (Option<&str>, Option<&Expr>) =
                            match input.as_ref() {
                                PlanNode::SeqScan { table } => (Some(table.as_str()), None),
                                PlanNode::Filter {
                                    input: inner,
                                    predicate,
                                } => {
                                    if let PlanNode::SeqScan { table } = inner.as_ref() {
                                        (Some(table.as_str()), Some(predicate))
                                    } else {
                                        (None, None)
                                    }
                                }
                                _ => (None, None),
                            };
                        if let Some(table) = table_opt {
                            if let Some(result) =
                                self.agg_single_col_fast(table, col, *function, pred_opt)?
                            {
                                return Ok(result);
                            }
                        }
                    }
                }

                // Fast path: Project(Limit(Filter(SeqScan))) — stream, decode
                // only projected columns, stop once we hit the limit.
                // (Handled in the Project branch; this branch only fires when
                // the aggregate is the outer node.)
                let result = self.execute_plan(input)?;
                match result {
                    QueryResult::Rows { columns, rows } => {
                        aggregate_rows(*function, argument.as_ref(), &columns, &rows)
                    }
                    _ => Err("aggregate requires row input".into()),
                }
            }

            PlanNode::Insert {
                table,
                rows,
                returning,
            } => {
                // Build + validate EVERY row before inserting any, so a bad
                // row (unknown/missing/uncoercible field) aborts the whole
                // statement without a partial write. The WAL fsync happens
                // once at statement end, so N rows = N appends + 1 fsync.
                let mut returning_columns: Vec<String> = Vec::new();
                let all_values: Vec<Vec<Value>> = {
                    let schema = self
                        .catalog
                        .schema(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                    if *returning {
                        returning_columns = schema.columns.iter().map(|c| c.name.clone()).collect();
                    }
                    let defaults = self.catalog.column_defaults(table).unwrap_or(&[]);
                    let auto = self.catalog.auto_columns(table).unwrap_or(&[]);
                    let mut all = Vec::with_capacity(rows.len());
                    for assignments in rows {
                        let mut values = vec![Value::Empty; schema.columns.len()];
                        for a in assignments {
                            let idx = schema.column_index(&a.field).ok_or_else(|| {
                                QueryError::ColumnNotFound {
                                    table: String::new(),
                                    column: a.field.clone(),
                                }
                            })?;
                            let raw = literal_to_value(&a.value)?;
                            values[idx] = coerce_value(raw, &schema.columns[idx])?;
                        }
                        // Fill any column left unset by this row from its
                        // declared default (applied before the required check,
                        // so a default satisfies a required column).
                        for (i, slot) in values.iter_mut().enumerate() {
                            if slot.is_empty() {
                                if let Some(Some(d)) = defaults.get(i) {
                                    *slot = d.clone();
                                }
                            }
                        }
                        for col in &schema.columns {
                            let pos = col.position as usize;
                            // Auto columns are exempt from the required check —
                            // they are filled from the sequence just below.
                            let is_auto = auto.get(pos).copied().unwrap_or(false);
                            if col.required && !is_auto && matches!(values[pos], Value::Empty) {
                                return Err(QueryError::Execution(format!(
                                    "column '{}' is required but no value was provided",
                                    col.name
                                )));
                            }
                        }
                        all.push(values);
                    }
                    all
                };
                // Assign auto-increment columns now that the immutable
                // schema/defaults/auto borrows are released. Done here (not in
                // the build loop) so the assigned ids land in `all_values` and
                // flow back through `returning`.
                let mut all_values = all_values;
                for values in all_values.iter_mut() {
                    self.catalog.assign_auto_columns(table, values);
                }
                // Charge the materialized batch against the per-query memory
                // budget before inserting — keeps multi-row insert consistent
                // with every other full-materialization point (sort/join/group)
                // and bounds embedded callers (the server also caps the query
                // string at 1 MB, but embedded callers have no such limit).
                self.charge_rows(&all_values)?;
                let n = all_values.len() as u64;
                for values in &all_values {
                    self.catalog
                        .insert(table, values)
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                }
                self.view_registry.mark_dependents_dirty(table);
                if *returning {
                    Ok(QueryResult::Rows {
                        columns: returning_columns,
                        rows: all_values,
                    })
                } else {
                    Ok(QueryResult::Modified(n))
                }
            }

            PlanNode::Upsert {
                table,
                key_column,
                assignments,
                on_conflict,
            } => {
                let (values, key_idx) = {
                    let schema = self
                        .catalog
                        .schema(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                    let mut values = vec![Value::Empty; schema.columns.len()];
                    for a in assignments {
                        let idx = schema.column_index(&a.field).ok_or_else(|| {
                            QueryError::ColumnNotFound {
                                table: String::new(),
                                column: a.field.clone(),
                            }
                        })?;
                        let raw = literal_to_value(&a.value)?;
                        values[idx] = coerce_value(raw, &schema.columns[idx])?;
                    }
                    // Apply column defaults for the insert path, same as a plain
                    // insert (applied before the required-column check).
                    let defaults = self.catalog.column_defaults(table).unwrap_or(&[]);
                    for (i, slot) in values.iter_mut().enumerate() {
                        if slot.is_empty() {
                            if let Some(Some(d)) = defaults.get(i) {
                                *slot = d.clone();
                            }
                        }
                    }
                    for col in &schema.columns {
                        if col.required && matches!(values[col.position as usize], Value::Empty) {
                            return Err(QueryError::Execution(format!(
                                "column '{}' is required but no value was provided",
                                col.name
                            )));
                        }
                    }
                    let key_idx = schema
                        .column_index(key_column)
                        .ok_or_else(|| format!("key column '{key_column}' not found"))?;
                    (values, key_idx)
                };

                // Upsert requires the `on` column to be unique — otherwise
                // there is no well-defined row to overwrite and a plain
                // insert could silently create duplicate keys.
                if self.catalog.is_index_unique(table, key_column) != Some(true) {
                    return Err(QueryError::Execution(format!(
                        "upsert on .{key_column} requires a unique column (declare it with \
                         `unique {key_column}: <type>` or `alter {table} add unique .{key_column}`)"
                    )));
                }

                let key_value = values[key_idx].clone();

                // Probe the unique index for a conflict.
                let existing = {
                    let tbl = self
                        .catalog
                        .get_table(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                    // The key column is guaranteed unique above, so this
                    // returns at most one matching row.
                    let rids = tbl.index_lookup_all(key_column, &key_value);
                    // Overflow safety (P0-3): reassemble via `tbl.get` so an
                    // upsert conflict row with a spilled column is read in full.
                    rids.into_iter()
                        .next()
                        .and_then(|rid| tbl.get(rid).map(|row| (rid, row)))
                };

                if let Some((rid, mut existing_row)) = existing {
                    // Conflict: apply on_conflict assignments (or all non-key if empty).
                    let update_assignments = if on_conflict.is_empty() {
                        assignments
                    } else {
                        on_conflict
                    };
                    let changed_cols: Vec<usize> = {
                        let schema = self
                            .catalog
                            .schema(table)
                            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                        let mut indices = Vec::new();
                        for a in update_assignments {
                            let idx = schema.column_index(&a.field).ok_or_else(|| {
                                QueryError::ColumnNotFound {
                                    table: String::new(),
                                    column: a.field.clone(),
                                }
                            })?;
                            if idx != key_idx {
                                // Coerce to the target column type, same as the
                                // UPDATE and INSERT paths — an int→float literal
                                // here would otherwise persist as raw i64 bits
                                // (#118 corruption on the upsert conflict path).
                                existing_row[idx] =
                                    coerce_value(literal_to_value(&a.value)?, &schema.columns[idx])
                                        .map_err(QueryError::TypeError)?;
                                indices.push(idx);
                            }
                        }
                        indices
                    };
                    self.catalog
                        .update_hinted(table, rid, &existing_row, Some(&changed_cols))
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    self.view_registry.mark_dependents_dirty(table);
                    Ok(QueryResult::Modified(1))
                } else {
                    // No conflict: insert.
                    self.catalog
                        .insert(table, &values)
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    self.view_registry.mark_dependents_dirty(table);
                    Ok(QueryResult::Modified(1))
                }
            }

            PlanNode::Update {
                input,
                table,
                assignments,
                returning,
            } => {
                // Mission C Phase 3: resolve assignments against a borrowed
                // schema, then drop the borrow before the mutation loop.
                // Try literal-only path first; fall back to per-row expression
                // evaluation if any assignment contains a non-literal expression
                // (e.g., `age := .age + 1`).
                let (col_indices, literal_vals, target_cols): (
                    Vec<usize>,
                    Option<Vec<Value>>,
                    Vec<ColumnDef>,
                ) = {
                    let schema_ref = self
                        .catalog
                        .schema(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                    let indices: Vec<usize> = assignments
                        .iter()
                        .map(|a| {
                            schema_ref.column_index(&a.field).ok_or_else(|| {
                                QueryError::ColumnNotFound {
                                    table: String::new(),
                                    column: a.field.clone(),
                                }
                            })
                        })
                        .collect::<Result<_, _>>()?;
                    // The target column defs (aligned with `assignments`), owned
                    // so the per-row expression path can coerce without holding a
                    // catalog borrow across the mutation loop.
                    let target_cols: Vec<ColumnDef> = indices
                        .iter()
                        .map(|&idx| schema_ref.columns[idx].clone())
                        .collect();
                    // Resolve each assignment to a literal value. If any is a
                    // non-literal expression, fall back (None) to the per-row
                    // expression-eval path below.
                    let raw_vals: Result<Vec<Value>, _> = assignments
                        .iter()
                        .map(|a| literal_to_value(&a.value))
                        .collect();
                    // Coerce each literal to its target column's declared type
                    // before it can reach the byte-patch fast path (the same
                    // coercion the INSERT path applies). Without this, an int
                    // assigned to a float column is written as raw i64 bits
                    // (#118 silent corruption) and a str assigned to a
                    // fixed-size column reaches `unreachable!` and aborts the
                    // whole server (#117 remote DoS). A genuine type mismatch
                    // is a hard error to the client, not an expr-path fallback.
                    let coerced = match raw_vals {
                        Ok(raws) => {
                            let mut out = Vec::with_capacity(raws.len());
                            for (raw, &idx) in raws.into_iter().zip(indices.iter()) {
                                out.push(
                                    coerce_value(raw, &schema_ref.columns[idx])
                                        .map_err(QueryError::TypeError)?,
                                );
                            }
                            Some(out)
                        }
                        Err(_) => None,
                    };
                    (indices, coerced, target_cols)
                };
                let resolved_assignments: Option<Vec<(usize, Value)>> =
                    literal_vals.map(|vals| col_indices.iter().copied().zip(vals).collect());

                // Mission C Phase 2: the hint Table::update_hinted needs to
                // decide whether to read the old row for index diff.
                let changed_cols: Vec<usize> = col_indices.clone();

                // ── RETURNING path ──────────────────────────────────────
                // `returning` materializes the post-update row image, so the
                // byte-patch / fused fast paths (which never decode a row)
                // can't serve it. Take the generic decode→mutate→collect
                // route. Opt-in only: when `returning` is false every path
                // below is byte-for-byte unchanged.
                if *returning {
                    let columns: Vec<String> = {
                        let schema_ref = self
                            .catalog
                            .schema(table)
                            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                        schema_ref.columns.iter().map(|c| c.name.clone()).collect()
                    };
                    let matching_rids = self.collect_rids_for_mutation(input, table)?;
                    let mut out_rows: Vec<Vec<Value>> = Vec::with_capacity(matching_rids.len());
                    // Cancellation is safe while collecting the target set, but
                    // once row writes start this executor has no statement-level
                    // savepoint. Check at the mutation boundary and then apply the
                    // full set without mid-loop cancellation; returning an error
                    // after a logged prefix would violate statement atomicity and
                    // is especially unsafe inside an explicit transaction.
                    crate::cancel::check()?;
                    for rid in matching_rids {
                        let mut row = match self.catalog.get(table, rid) {
                            Some(r) => r,
                            None => continue,
                        };
                        match &resolved_assignments {
                            // Literal path: apply the pre-coerced values.
                            Some(resolved) => {
                                for (idx, val) in resolved.iter() {
                                    row[*idx] = val.clone();
                                }
                            }
                            // Expression path: evaluate each RHS against the
                            // (progressively mutated) row, then coerce to the
                            // target column type before writing — same guard the
                            // literal path gets, matching the non-returning expr
                            // path exactly (#117/#118 on computed assignments).
                            None => {
                                for (i, asgn) in assignments.iter().enumerate() {
                                    let val = eval_expr(&asgn.value, &row, &columns);
                                    row[col_indices[i]] = coerce_value(val, &target_cols[i])
                                        .map_err(QueryError::TypeError)?;
                                }
                            }
                        }
                        self.catalog
                            .update_hinted(table, rid, &row, Some(&changed_cols))
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                        out_rows.push(row);
                    }
                    self.view_registry.mark_dependents_dirty(table);
                    return Ok(QueryResult::Rows {
                        columns,
                        rows: out_rows,
                    });
                }

                // ── Fused scan+update for Update(Filter(SeqScan)) ────────
                // Perf sprint: instead of the two-pass collect-RIDs-then-loop
                // pattern (which pays one ensure_hot per matched row on the
                // second pass), fuse the predicate evaluation and in-place
                // byte-level mutation into a single heap walk. Same idea as
                // the fused scan_delete_matching path for deletes.
                if let Some(ref resolved_assignments) = resolved_assignments {
                    if let PlanNode::Filter {
                        input: inner,
                        predicate,
                    } = input.as_ref()
                    {
                        if let PlanNode::SeqScan { table: t } = inner.as_ref() {
                            if t == table {
                                // The fused primitive mutates during its scan and
                                // cannot roll back a cancelled prefix. Honor an
                                // already-triggered token before entering it, then
                                // let the primitive finish atomically from the
                                // query layer's perspective.
                                crate::cancel::check()?;
                                let fused_result = self.try_fused_scan_update(
                                    table,
                                    predicate,
                                    resolved_assignments,
                                    &changed_cols,
                                );
                                if let Some(result) = fused_result {
                                    return result;
                                }
                            }
                        }
                    }
                }

                // Collect matching RowIds in a single pass.
                let matching_rids = self.collect_rids_for_mutation(input, table)?;
                // This is the last cancellable boundary before any row is
                // changed. Mutation loops below deliberately do not poll.
                crate::cancel::check()?;

                // ── Literal-only fast paths ─────────────────────────────
                if let Some(ref resolved_assignments) = resolved_assignments {
                    // Mission C Phase 4: in-place byte-patch fast path. If every
                    // assignment targets a fixed-size non-null column AND none of
                    // them is indexed, we can skip decode_row / Vec<Value> /
                    // encode_row_into entirely and patch the row's raw bytes on
                    // the hot page.
                    let fast_patch: Option<Vec<FastPatch>> = {
                        let tbl = self
                            .catalog
                            .get_table(table)
                            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                        let schema = tbl.schema();
                        // Overflow safety (P0): byte-patching a v2 row with v1
                        // offsets corrupts it. Overflow tables take the generic
                        // reassembling `get` + `update_hinted` path below.
                        let all_fixed_nonnull = !tbl.has_overflow_rows()
                            && resolved_assignments.iter().all(|(idx, val)| {
                                is_fixed_size(schema.columns[*idx].type_id) && !val.is_empty()
                            });
                        let no_indexed = !resolved_assignments
                            .iter()
                            .any(|(idx, _)| tbl.has_indexed_col(*idx));

                        if all_fixed_nonnull && no_indexed {
                            let layout = RowLayout::new(schema);
                            let bitmap_size = layout.bitmap_size();
                            let patches: Vec<FastPatch> = resolved_assignments
                                .iter()
                                .map(|(idx, val)| {
                                    let fixed_off = layout
                                        .fixed_offset(*idx)
                                        .expect("is_fixed_size already checked");
                                    let field_off = 2 + bitmap_size + fixed_off;
                                    let bytes: FixedBytes = match val {
                                        Value::Int(v) => FixedBytes::I64(v.to_le_bytes()),
                                        Value::Float(v) => FixedBytes::F64(v.to_le_bytes()),
                                        Value::Bool(v) => FixedBytes::Bool(if *v { 1 } else { 0 }),
                                        Value::DateTime(v) => FixedBytes::I64(v.to_le_bytes()),
                                        Value::Uuid(v) => FixedBytes::Uuid(*v),
                                        _ => unreachable!("all_fixed_nonnull guard lied"),
                                    };
                                    FastPatch {
                                        field_off,
                                        bitmap_byte_off: 2 + idx / 8,
                                        bit_mask: 1u8 << (idx % 8),
                                        bytes,
                                    }
                                })
                                .collect();
                            Some(patches)
                        } else {
                            None
                        }
                    };

                    if let Some(patches) = fast_patch {
                        let mut count = 0u64;
                        let mut fallback_rids: Vec<RowId> = Vec::new();
                        for rid in &matching_rids {
                            // Mission B2: WAL-log every patch so crash
                            // recovery replays the update. Same mutation
                            // closure as before — the wrapper just sandwiches
                            // it between a hot-page read and a WAL append.
                            //
                            // A false return means the byte-patch was refused
                            // (e.g. a v2/overflow row whose in-place layout the
                            // fast path cannot compute, reachable on a legacy
                            // heap where has_overflow_rows() under-reports). Do
                            // NOT drop the row: push it to `fallback_rids` and
                            // let the reassembling get + update_hinted path
                            // apply it, mirroring the var-column fast path
                            // below. The fast path is thus a pure optimization
                            // that can never silently lose an update.
                            let ok = self
                                .catalog
                                .update_row_bytes_logged(table, *rid, |row| {
                                    let base = row_body_base(row);
                                    for p in &patches {
                                        row[base + p.bitmap_byte_off] &= !p.bit_mask;
                                        let field_bytes = p.bytes.as_slice();
                                        row[base + p.field_off
                                            ..base + p.field_off + field_bytes.len()]
                                            .copy_from_slice(field_bytes);
                                    }
                                })
                                .map_err(|e| QueryError::StorageError(e.to_string()))?;
                            if ok {
                                count += 1;
                            } else {
                                fallback_rids.push(*rid);
                            }
                        }
                        for rid in fallback_rids {
                            let mut row = match self.catalog.get(table, rid) {
                                Some(r) => r,
                                None => continue,
                            };
                            for (idx, val) in resolved_assignments.iter() {
                                row[*idx] = val.clone();
                            }
                            self.catalog
                                .update_hinted(table, rid, &row, Some(&changed_cols))
                                .map_err(|e| QueryError::StorageError(e.to_string()))?;
                            count += 1;
                        }
                        self.view_registry.mark_dependents_dirty(table);
                        return Ok(QueryResult::Modified(count));
                    }

                    // Mission C Phase 10: var-column in-place shrink fast path.
                    let var_fast: Option<(usize, Option<Vec<u8>>)> = {
                        let tbl = self
                            .catalog
                            .get_table(table)
                            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                        let schema = tbl.schema();
                        // Overflow safety (P0/P0-2): the in-place var shrink
                        // patch computes v1 offsets — never on a v2-capable
                        // table. Falls through to the reassembling path.
                        let is_single = resolved_assignments.len() == 1 && !tbl.has_overflow_rows();
                        let is_var_col = is_single
                            && !is_fixed_size(schema.columns[resolved_assignments[0].0].type_id);
                        let no_indexed = !resolved_assignments
                            .iter()
                            .any(|(idx, _)| tbl.has_indexed_col(*idx));

                        if is_single && is_var_col && no_indexed {
                            let (idx, val) = &resolved_assignments[0];
                            let bytes_opt: Option<Vec<u8>> = match val {
                                Value::Str(s) => Some(s.as_bytes().to_vec()),
                                Value::Bytes(b) => Some(b.clone()),
                                // A json column stores its PJ1 bytes as the var
                                // payload (u32 length prefix + bytes, like Bytes),
                                // so the in-place patch writes them verbatim.
                                Value::Json(b) => Some(b.to_vec()),
                                Value::Empty => None,
                                _ => {
                                    return Err(QueryError::TypeError(format!(
                                        "cannot assign non-var value to var column '{}'",
                                        schema.columns[*idx].name
                                    )))
                                }
                            };
                            Some((*idx, bytes_opt))
                        } else {
                            None
                        }
                    };

                    if let Some((col_idx, new_bytes_opt)) = var_fast {
                        let new_bytes_ref: Option<&[u8]> = new_bytes_opt.as_deref();
                        let mut count = 0u64;
                        let mut fallback_rids: Vec<RowId> = Vec::new();
                        for rid in &matching_rids {
                            // Mission B2: logged variant so crash recovery
                            // replays the shrink. On a false return (row
                            // would have to grow), the rid is pushed to
                            // `fallback_rids` and the slower `update_hinted`
                            // path — which is already WAL-logged — picks it up.
                            let ok = self
                                .catalog
                                .patch_var_col_logged(table, *rid, col_idx, new_bytes_ref)
                                .map_err(|e| QueryError::StorageError(e.to_string()))?;
                            if ok {
                                count += 1;
                            } else {
                                fallback_rids.push(*rid);
                            }
                        }
                        for rid in fallback_rids {
                            let mut row = match self.catalog.get(table, rid) {
                                Some(r) => r,
                                None => continue,
                            };
                            for (idx, val) in resolved_assignments.iter() {
                                row[*idx] = val.clone();
                            }
                            self.catalog
                                .update_hinted(table, rid, &row, Some(&changed_cols))
                                .map_err(|e| QueryError::StorageError(e.to_string()))?;
                            count += 1;
                        }
                        self.view_registry.mark_dependents_dirty(table);
                        return Ok(QueryResult::Modified(count));
                    }

                    // Generic literal path: decode row, apply literal values.
                    let mut count = 0u64;
                    for rid in matching_rids {
                        let mut row = match self.catalog.get(table, rid) {
                            Some(r) => r,
                            None => continue,
                        };
                        for (idx, val) in resolved_assignments.iter() {
                            row[*idx] = val.clone();
                        }
                        self.catalog
                            .update_hinted(table, rid, &row, Some(&changed_cols))
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                        count += 1;
                    }
                    self.view_registry.mark_dependents_dirty(table);
                    return Ok(QueryResult::Modified(count));
                } // end if let Some(resolved_assignments)

                // ── Expression-based update path ────────────────────────
                // At least one assignment contains a non-literal expression
                // (e.g., `age := .age + 1`). Evaluate per-row.
                let col_names: Vec<String> = {
                    let schema_ref = self
                        .catalog
                        .schema(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                    schema_ref.columns.iter().map(|c| c.name.clone()).collect()
                };
                let mut count = 0u64;
                for rid in matching_rids {
                    let mut row = match self.catalog.get(table, rid) {
                        Some(r) => r,
                        None => continue,
                    };
                    for (i, asgn) in assignments.iter().enumerate() {
                        let val = eval_expr(&asgn.value, &row, &col_names);
                        // Coerce to the target column type before writing, so a
                        // computed int→float assignment stores f64 (not raw i64
                        // bits, #118) and a str→fixed-col assignment returns a
                        // typed error instead of hitting the encoder's
                        // `unreachable!` and aborting the process (#117).
                        row[col_indices[i]] =
                            coerce_value(val, &target_cols[i]).map_err(QueryError::TypeError)?;
                    }
                    self.catalog
                        .update_hinted(table, rid, &row, Some(&changed_cols))
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    count += 1;
                }
                self.view_registry.mark_dependents_dirty(table);
                Ok(QueryResult::Modified(count))
            }

            PlanNode::Delete {
                input,
                table,
                returning,
            } => {
                // ── RETURNING path ──────────────────────────────────────
                // `returning` needs the pre-delete row image, so read each
                // matched row before removing it. The fused single-pass
                // delete primitives below never decode rows, so they can't
                // serve this. Opt-in only: when `returning` is false the
                // fast paths below are byte-for-byte unchanged.
                if *returning {
                    let columns: Vec<String> = {
                        let schema_ref = self
                            .catalog
                            .schema(table)
                            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                        schema_ref.columns.iter().map(|c| c.name.clone()).collect()
                    };
                    let matching_rids = self.collect_rids_for_mutation(input, table)?;
                    let mut out_rows: Vec<Vec<Value>> = Vec::with_capacity(matching_rids.len());
                    // Cooperative cancellation of the pre-delete image read. The
                    // actual removal below is a single batched `delete_many`, so
                    // cancelling here happens before any row is deleted.
                    let mut cancel = CancelCheck::new();
                    for rid in &matching_rids {
                        cancel.tick()?;
                        if let Some(row) = self.catalog.get(table, *rid) {
                            out_rows.push(row);
                        }
                    }
                    crate::cancel::check()?;
                    self.catalog
                        .delete_many(table, &matching_rids)
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    self.view_registry.mark_dependents_dirty(table);
                    return Ok(QueryResult::Rows {
                        columns,
                        rows: out_rows,
                    });
                }

                // Mission C Phase 3: no schema clone — collect_rids_for_mutation
                // looks up schema internally when it needs one, and the mutation
                // loop doesn't need the schema at all.
                //
                // Mission C Phase 12: route bulk deletes through
                // `Catalog::delete_many`, which batches the btree leaf
                // compaction and shares one `ensure_hot` per row between
                // the index-key extraction and the slot delete. On
                // `delete_by_filter` (100K fixture, ~20K matches) that
                // removes ~4ms of pure `Vec::remove` memmove from the btree
                // maintenance phase.
                //
                // Mission C Phase 16: for the common `delete where ...`
                // shape (Filter(SeqScan)) — and the rarer "delete
                // everything" shape (SeqScan) — skip the two-pass
                // `collect_rids_for_mutation` + `delete_many` flow entirely.
                // The fused `scan_delete_matching` primitive walks the
                // heap exactly once, paying one `ensure_hot` per page
                // instead of per-row. That closes the last major gap on
                // the bench's `delete_by_filter` workload.
                // Overflow safety (P1): a v2-capable table cannot take the fused
                // raw-byte delete — the compiled predicate mis-reads spilled
                // columns. Route it through the reassembling collect-rids path.
                let delete_overflow = self.catalog.table_has_overflow(table);
                if let PlanNode::Filter {
                    input: inner,
                    predicate,
                } = input.as_ref()
                {
                    if let PlanNode::SeqScan { table: t } = inner.as_ref() {
                        if t == table && !delete_overflow {
                            let schema = self
                                .catalog
                                .schema(table)
                                .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                            let columns: Vec<String> =
                                schema.columns.iter().map(|c| c.name.clone()).collect();
                            let fast = FastLayout::new(schema);
                            if let Some(compiled) =
                                compile_predicate(predicate, &columns, &fast, schema)
                            {
                                // Mission B2: logged variant so every
                                // matched rid hits the WAL during the
                                // single-pass scan. Structure of the
                                // fused scan is unchanged — only the
                                // hook closure now also appends.
                                crate::cancel::check()?;
                                let count = self
                                    .catalog
                                    .scan_delete_matching_logged(table, |data| compiled(data))
                                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                                self.view_registry.mark_dependents_dirty(table);
                                return Ok(QueryResult::Modified(count));
                            }
                        }
                    }
                } else if let PlanNode::SeqScan { table: t } = input.as_ref() {
                    if t == table && !delete_overflow {
                        // `delete from T` with no predicate — every live
                        // row matches. One pass is still the right shape.
                        // Mission B2: logged variant — see above.
                        crate::cancel::check()?;
                        let count = self
                            .catalog
                            .scan_delete_matching_logged(table, |_| true)
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                        self.view_registry.mark_dependents_dirty(table);
                        return Ok(QueryResult::Modified(count));
                    }
                }

                let matching_rids = self.collect_rids_for_mutation(input, table)?;
                crate::cancel::check()?;
                let count = self
                    .catalog
                    .delete_many(table, &matching_rids)
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                self.view_registry.mark_dependents_dirty(table);
                Ok(QueryResult::Modified(count))
            }

            PlanNode::AliasScan { table, alias } => {
                // Mission E1.2: scan `table` and rename every output column
                // to `alias.field`. Used as a join leaf so downstream
                // NestedLoopJoin + Filter + Project nodes can resolve
                // `Expr::QualifiedField` lookups by direct column-name match.
                //
                // We don't bother with a fused zero-copy loop here yet — the
                // whole join path is nested-loop and correctness-first
                // (Phase E1.3 will introduce hash join and at that point we
                // can revisit whether to specialise AliasScan).
                let schema = self
                    .catalog
                    .schema(table)
                    .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
                    .clone();
                let columns: Vec<String> = schema
                    .columns
                    .iter()
                    .map(|c| format!("{alias}.{}", c.name))
                    .collect();
                let mut cancel = CancelCheck::new();
                let mut rows: Vec<Vec<Value>> = Vec::new();
                for (_, row) in self
                    .catalog
                    .scan(table)
                    .map_err(|e| QueryError::StorageError(e.to_string()))?
                {
                    cancel.tick()?;
                    rows.push(row);
                }
                Ok(QueryResult::Rows { columns, rows })
            }

            PlanNode::NestedLoopJoin {
                left,
                right,
                on,
                kind,
            } => {
                // Materialise both sides. The executor ships two strategies:
                //   1. Hash join (E1.3) — when the `on` predicate is a
                //      simple equi-predicate `left_col = right_col`, build a
                //      FxHashMap<Value, Vec<row_idx>> over the right side
                //      and probe with the left side. O(L + R) instead of
                //      O(L × R). Handles Inner and LeftOuter.
                //   2. Nested loop (E1.2) — fallback for Cross, non-equi
                //      predicates, or `on` expressions that reference
                //      either side with something more complex than a
                //      QualifiedField.
                let left_result = self.execute_plan(left)?;
                let right_result = self.execute_plan(right)?;
                let (left_columns, left_rows) = match left_result {
                    QueryResult::Rows { columns, rows } => (columns, rows),
                    _ => return Err("join left side must produce rows".into()),
                };
                let (right_columns, right_rows) = match right_result {
                    QueryResult::Rows { columns, rows } => (columns, rows),
                    _ => return Err("join right side must produce rows".into()),
                };

                // WS2: byte-budget guard on the join build side. Charge both
                // materialized inputs before we build the hash table / probe;
                // the output is row-capped by check_join_limit below.
                self.charge_rows(&left_rows)?;
                self.charge_rows(&right_rows)?;

                execute_materialized_join(
                    left_columns,
                    left_rows,
                    right_columns,
                    right_rows,
                    on.as_ref(),
                    *kind,
                    self.nested_loop_pair_limit,
                )
            }

            PlanNode::Distinct { input } => {
                let result = self.execute_plan(input)?;
                match result {
                    QueryResult::Rows { columns, rows } => {
                        let mut seen = std::collections::HashSet::new();
                        let mut unique_rows = Vec::new();
                        let mut cancel = CancelCheck::new();
                        for row in rows {
                            cancel.tick()?;
                            if seen.insert(row.clone()) {
                                unique_rows.push(row);
                            }
                        }
                        Ok(QueryResult::Rows {
                            columns,
                            rows: unique_rows,
                        })
                    }
                    other => Ok(other),
                }
            }

            PlanNode::GroupBy {
                input,
                keys,
                aggregates,
                having,
            } => {
                if aggregates
                    .iter()
                    .any(|aggregate| aggregate.provenance_alias.is_some())
                {
                    let input = self.materialize_rows_with_provenance(input)?;
                    self.charge_rows(&input.rows)?;
                    return exec_group_by_with_provenance(
                        input,
                        keys,
                        aggregates,
                        having,
                        self.query_memory_limit(),
                    );
                }
                let result = self.execute_plan(input)?;
                match result {
                    QueryResult::Rows { columns, rows } => {
                        // WS2: byte-budget guard on the GROUP BY input buffer
                        // (the hash table is bounded by the input it groups).
                        self.charge_rows(&rows)?;
                        exec_group_by(columns, rows, keys, aggregates, having)
                    }
                    _ => Err("group by requires row input".into()),
                }
            }

            PlanNode::CreateTable {
                name,
                fields,
                if_not_exists,
            } => {
                // Idempotency: a re-declared type is a clean no-op under
                // `if not exists`, and otherwise a PowQL-flavored error that
                // names the type (not the storage layer's generic "table").
                if self.catalog.schema(name).is_some() {
                    if *if_not_exists {
                        return Ok(QueryResult::Executed {
                            message: format!("type '{name}' already exists (skipped)"),
                        });
                    }
                    // "cannot" prefix keeps this on the server's
                    // safe-to-forward allowlist (SAFE_ERROR_PREFIXES).
                    return Err(QueryError::Execution(format!(
                        "cannot create type '{name}': it already exists"
                    )));
                }
                let columns: Vec<ColumnDef> = fields
                    .iter()
                    .enumerate()
                    .map(|(i, f)| -> Result<ColumnDef, QueryError> {
                        Ok(ColumnDef {
                            name: f.name.clone(),
                            type_id: type_name_to_id(&f.type_name)
                                .map_err(QueryError::TypeError)?,
                            required: f.required,
                            position: i as u16,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                // Coerce each literal default to its column's type now, so a
                // type mismatch (`count: int default "x"`) is rejected at DDL
                // time and the stored default is ready to drop into inserts.
                let mut defaults: Vec<Option<Value>> = vec![None; columns.len()];
                let mut auto_cols: Vec<bool> = vec![false; columns.len()];
                for (i, f) in fields.iter().enumerate() {
                    if let Some(lit) = &f.default {
                        let raw = literal_value_from(lit);
                        defaults[i] = Some(coerce_value(raw, &columns[i])?);
                    }
                    if f.auto {
                        // Auto-increment only makes sense on an integer column,
                        // and combining it with a literal default is
                        // contradictory (both want to supply the value).
                        if columns[i].type_id != TypeId::Int {
                            return Err(QueryError::TypeError(format!(
                                "auto column '{}' must be of type int",
                                f.name
                            )));
                        }
                        if f.default.is_some() {
                            return Err(QueryError::TypeError(format!(
                                "auto column '{}' cannot also declare a default",
                                f.name
                            )));
                        }
                        auto_cols[i] = true;
                    }
                }
                let schema = Schema {
                    table_name: name.clone(),
                    columns,
                };
                self.catalog
                    .create_table_full(schema, defaults, auto_cols)
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                // Declaring a field `unique` auto-creates a unique B+tree
                // index, which is where uniqueness is enforced on writes.
                for f in fields.iter().filter(|f| f.unique) {
                    self.catalog
                        .create_index_unique(name, &f.name, true)
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                }
                Ok(QueryResult::Created(name.clone()))
            }

            PlanNode::AlterTable { table, action } => match action {
                AlterAction::AddColumn {
                    name,
                    type_name,
                    required,
                } => {
                    let position = self
                        .catalog
                        .schema(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
                        .columns
                        .len() as u16;
                    let col = ColumnDef {
                        name: name.clone(),
                        type_id: type_name_to_id(type_name).map_err(QueryError::TypeError)?,
                        required: *required,
                        position,
                    };
                    self.catalog
                        .alter_table_add_column(table, col)
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    Ok(QueryResult::Executed {
                        message: format!("column '{name}' added to '{table}'"),
                    })
                }
                AlterAction::DropColumn { name, if_exists } => {
                    // `if exists`: a missing column (or missing table) is a
                    // no-op instead of an error.
                    if *if_exists {
                        let present = self
                            .catalog
                            .schema(table)
                            .map(|s| s.column_index(name).is_some())
                            .unwrap_or(false);
                        if !present {
                            return Ok(QueryResult::Executed {
                                message: format!(
                                    "column '{name}' does not exist on '{table}' (skipped)"
                                ),
                            });
                        }
                    }
                    self.catalog
                        .alter_table_drop_column(table, name)
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    Ok(QueryResult::Executed {
                        message: format!("column '{name}' dropped from '{table}'"),
                    })
                }
                AlterAction::AddIndex {
                    target,
                    if_not_exists: _,
                } => {
                    let IndexTarget::Column(column) = target else {
                        let IndexTarget::JsonPath(path) = target else {
                            unreachable!("index target variants are exhaustive")
                        };
                        if let Some(existing) = resolve_expression_index(&self.catalog, table, path)
                        {
                            return Ok(QueryResult::Executed {
                                message: format!(
                                    "expression index {} on '{}' already exists (skipped)",
                                    existing.index_id, table
                                ),
                            });
                        }
                        crate::cancel::check()?;
                        let index_id = self
                            .catalog
                            .create_expression_index_metadata(
                                table,
                                1,
                                path.canonical_text(),
                                path.clone(),
                                false,
                            )
                            .map_err(|error| QueryError::StorageError(error.to_string()))?;
                        return Ok(QueryResult::Executed {
                            message: format!("expression index {index_id} on '{}' created", table),
                        });
                    };
                    // `add index` is already idempotent (no-op if the index
                    // exists), so `if not exists` is accepted for symmetry but
                    // does not change behavior.
                    crate::cancel::check()?;
                    self.catalog
                        .create_index(table, column)
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    Ok(QueryResult::Executed {
                        message: format!("index on '{table}.{column}' created"),
                    })
                }
                AlterAction::AddUnique {
                    target,
                    if_not_exists,
                } => {
                    let IndexTarget::Column(column) = target else {
                        let IndexTarget::JsonPath(path) = target else {
                            unreachable!("index target variants are exhaustive")
                        };
                        if let Some(existing) = resolve_expression_index(&self.catalog, table, path)
                        {
                            if *if_not_exists {
                                return Ok(QueryResult::Executed {
                                    message: format!(
                                        "expression index {} on '{}' already exists (skipped)",
                                        existing.index_id, table
                                    ),
                                });
                            }
                            return Err(QueryError::Execution(format!(
                                "cannot add unique expression index on {}: path already indexed",
                                table
                            )));
                        }
                        crate::cancel::check()?;
                        let index_id = self
                            .catalog
                            .create_expression_index_metadata(
                                table,
                                1,
                                path.canonical_text(),
                                path.clone(),
                                true,
                            )
                            .map_err(|error| QueryError::StorageError(error.to_string()))?;
                        return Ok(QueryResult::Executed {
                            message: format!(
                                "unique expression index {index_id} on '{}' created",
                                table
                            ),
                        });
                    };
                    // `if not exists`: an already-indexed column is a no-op
                    // rather than the (default) "already indexed" error.
                    if self.catalog.has_index(table, column) {
                        if *if_not_exists {
                            return Ok(QueryResult::Executed {
                                message: format!(
                                    "index on '{table}.{column}' already exists (skipped)"
                                ),
                            });
                        }
                        // Upgrading an existing non-unique index in place is
                        // intentionally rejected.
                        return Err(QueryError::Execution(format!(
                            "cannot add unique on {table}.{column}: column already indexed"
                        )));
                    }
                    // Scan existing rows for duplicate (non-null) values
                    // before creating the unique index.
                    {
                        let tbl = self
                            .catalog
                            .get_table(table)
                            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                        let col_idx = tbl.schema().column_index(column).ok_or_else(|| {
                            QueryError::ColumnNotFound {
                                table: table.to_string(),
                                column: column.clone(),
                            }
                        })?;
                        let mut seen = std::collections::HashSet::new();
                        let mut cancel = CancelCheck::new();
                        for (_, row) in tbl.scan() {
                            cancel.tick()?;
                            let v = &row[col_idx];
                            if v.is_empty() {
                                continue;
                            }
                            if !seen.insert(v.clone()) {
                                return Err(QueryError::Execution(format!(
                                    "cannot add unique on {table}.{column}: \
                                     duplicate value {v:?} exists"
                                )));
                            }
                        }
                    }
                    crate::cancel::check()?;
                    self.catalog
                        .create_index_unique(table, column, true)
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    Ok(QueryResult::Executed {
                        message: format!("unique index on '{table}.{column}' created"),
                    })
                }
                AlterAction::DropIndex { target, if_exists } => {
                    let IndexTarget::JsonPath(path) = target else {
                        return Err(QueryError::Execution(
                            "dropping stored-column indexes is not supported".to_string(),
                        ));
                    };
                    let Some(existing) = resolve_expression_index(&self.catalog, table, path)
                    else {
                        if *if_exists {
                            return Ok(QueryResult::Executed {
                                message: format!(
                                    "expression index on '{}' does not exist (skipped)",
                                    table
                                ),
                            });
                        }
                        return Err(QueryError::Execution(format!(
                            "expression index on '{}' does not exist",
                            table
                        )));
                    };
                    crate::cancel::check()?;
                    self.catalog
                        .drop_expression_index(table, existing.index_id)
                        .map_err(|error| QueryError::StorageError(error.to_string()))?;
                    Ok(QueryResult::Executed {
                        message: format!(
                            "expression index {} on '{}' dropped",
                            existing.index_id, table
                        ),
                    })
                }
            },

            PlanNode::DropTable { name, if_exists } => {
                if *if_exists && self.catalog.schema(name).is_none() {
                    return Ok(QueryResult::Executed {
                        message: format!("type '{name}' does not exist (skipped)"),
                    });
                }
                self.catalog
                    .drop_table(name)
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                Ok(QueryResult::Executed {
                    message: format!("table '{name}' dropped"),
                })
            }

            PlanNode::ListTypes => self.introspect_list_types(),

            PlanNode::Describe { table } => self.introspect_describe(table),

            PlanNode::CreateView { name, query_text } => {
                self.create_view(name, query_text)?;
                Ok(QueryResult::Executed {
                    message: format!("materialized view '{name}' created"),
                })
            }

            PlanNode::RefreshView { name } => {
                self.refresh_view(name)?;
                Ok(QueryResult::Executed {
                    message: format!("materialized view '{name}' refreshed"),
                })
            }

            PlanNode::DropView { name, if_exists } => {
                if *if_exists && !self.view_registry.is_view(name) {
                    return Ok(QueryResult::Executed {
                        message: format!("view '{name}' does not exist (skipped)"),
                    });
                }
                self.drop_view(name)?;
                Ok(QueryResult::Executed {
                    message: format!("materialized view '{name}' dropped"),
                })
            }

            PlanNode::Window { input, windows } => {
                let result = self.execute_plan(input)?;
                execute_window(result, windows, self.query_memory_limit)
            }

            PlanNode::Union { left, right, all } => {
                let left_result = self.execute_plan(left)?;
                let right_result = self.execute_plan(right)?;
                let (left_cols, left_rows) = match left_result {
                    QueryResult::Rows { columns, rows } => (columns, rows),
                    _ => return Err("UNION requires query results on left side".into()),
                };
                let (_, right_rows) = match right_result {
                    QueryResult::Rows { columns, rows } => (columns, rows),
                    _ => return Err("UNION requires query results on right side".into()),
                };
                let mut combined = left_rows;
                let mut cancel = CancelCheck::new();
                if *all {
                    // UNION ALL — just concatenate.
                    for row in right_rows {
                        cancel.tick()?;
                        combined.push(row);
                    }
                } else {
                    // UNION — deduplicate using the same HashSet approach
                    // as DISTINCT. Value already implements Hash + Eq.
                    let mut seen = std::collections::HashSet::new();
                    for row in &combined {
                        cancel.tick()?;
                        seen.insert(row.clone());
                    }
                    for row in right_rows {
                        cancel.tick()?;
                        if seen.insert(row.clone()) {
                            combined.push(row);
                        }
                    }
                }
                Ok(QueryResult::Rows {
                    columns: left_cols,
                    rows: combined,
                })
            }

            PlanNode::Explain { input } => {
                // Every execute entry point runs lower_unindexed_scans before
                // dispatch and lowering recurses into Explain, so `input` is
                // already the plan that will actually run.
                let text = format_plan_tree(&self.catalog, input, 0);
                Ok(QueryResult::Rows {
                    columns: vec!["plan".to_string()],
                    rows: text
                        .lines()
                        .map(|line| vec![Value::Str(line.to_string())])
                        .collect(),
                })
            }

            PlanNode::Begin => {
                if self.in_transaction {
                    return Err(QueryError::Execution(
                        "already in a transaction (nested transactions not supported)".into(),
                    ));
                }
                self.catalog
                    .begin_transaction()
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                self.in_transaction = true;
                Ok(QueryResult::Executed {
                    message: "transaction started".to_string(),
                })
            }

            PlanNode::Commit => {
                if !self.in_transaction {
                    return Err(QueryError::Execution(
                        "no active transaction to commit".into(),
                    ));
                }
                self.catalog
                    .commit_transaction()
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                self.in_transaction = false;
                Ok(QueryResult::Executed {
                    message: "transaction committed".to_string(),
                })
            }

            PlanNode::Rollback => {
                if !self.in_transaction {
                    return Err(QueryError::Execution(
                        "no active transaction to roll back".into(),
                    ));
                }
                self.rollback_transaction_preserving_wal_archive()
            }

            PlanNode::IndexScan { table, column, key } => {
                let key_value = literal_to_value(key)?;
                let tbl = self
                    .catalog
                    .get_table(table)
                    .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                let columns: Vec<String> = tbl
                    .schema()
                    .columns
                    .iter()
                    .map(|c| c.name.clone())
                    .collect();

                // Fast path: the table has a B-tree on this column.
                // Uses index_lookup_all to return ALL matching rows for
                // both unique and non-unique indexes.
                if tbl.has_index(column) {
                    let rids = tbl.index_lookup_all(column, &key_value);
                    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(rids.len());
                    let mut cancel = CancelCheck::new();
                    for rid in rids {
                        cancel.tick()?;
                        // Overflow safety (P0-3/P0-4): `tbl.get` reassembles
                        // spilled columns; the old `heap.get` + `decode_row`
                        // returned Empty / wrapped a >= 64KB value.
                        if let Some(row) = tbl.get(rid) {
                            rows.push(row);
                        }
                    }
                    return Ok(QueryResult::Rows { columns, rows });
                }

                // Fallback: no index on this column. The planner emits IndexScan
                // eagerly (it has no visibility into which columns are indexed
                // at plan time), so here we must behave like SeqScan+Filter on
                // `.col = literal`: return *all* matching rows, not just the
                // first one. A non-indexed column isn't necessarily unique.
                // We compile the eq predicate once and stream without any
                // per-row decode for non-matching rows.
                let schema = tbl.schema();
                let fast = FastLayout::new(schema);
                let synth_pred = Expr::BinaryOp(
                    Box::new(Expr::Field(column.clone())),
                    BinOp::Eq,
                    Box::new(key.clone()),
                );
                // Overflow safety (P0-4/P1): the raw compiled scan drops/mis-reads
                // spilled columns; a v2-capable table uses the decoded scan below.
                if !tbl.has_overflow_rows() {
                    if let Some(compiled) = compile_predicate(&synth_pred, &columns, &fast, schema)
                    {
                        // Mission F: skip the first 4 Vec doublings.
                        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(64);
                        for_each_row_raw_cancellable(&self.catalog, table, |_rid, data| {
                            if compiled(data) {
                                rows.push(decode_row(schema, data));
                            }
                        })?;
                        return Ok(QueryResult::Rows { columns, rows });
                    }
                }

                // Last resort: slow eq-check on materialised rows.
                let col_idx =
                    schema
                        .column_index(column)
                        .ok_or_else(|| QueryError::ColumnNotFound {
                            table: String::new(),
                            column: column.clone(),
                        })?;
                let mut cancel = CancelCheck::new();
                let mut rows: Vec<Vec<Value>> = Vec::new();
                for (_, row) in tbl.scan() {
                    cancel.tick()?;
                    if row[col_idx] == key_value {
                        rows.push(row);
                    }
                }
                Ok(QueryResult::Rows { columns, rows })
            }

            PlanNode::RangeScan {
                table,
                column,
                start,
                end,
            } => {
                let tbl = self
                    .catalog
                    .get_table(table)
                    .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                let columns: Vec<String> = tbl
                    .schema()
                    .columns
                    .iter()
                    .map(|c| c.name.clone())
                    .collect();
                let schema = tbl.schema();

                let start_val = match start {
                    Some((expr, _)) => Some(literal_to_value(expr)?),
                    None => None,
                };
                let end_val = match end {
                    Some((expr, _)) => Some(literal_to_value(expr)?),
                    None => None,
                };
                let start_inclusive = start.as_ref().map(|(_, inc)| *inc).unwrap_or(true);
                let end_inclusive = end.as_ref().map(|(_, inc)| *inc).unwrap_or(true);

                // Non-unique index: walk the composite (value, rid) leaf
                // chain between prefix bounds, fetch each row from the heap,
                // and recheck. The recheck enforces exclusive bounds
                // (range_rids is inclusive) and defensively skips any decoded
                // null (nulls are never indexed, so they must not match).
                if tbl.is_index_unique(column) == Some(false) {
                    if let Some(btree) = tbl.index(column) {
                        if start_val.is_some() || end_val.is_some() {
                            let col_idx = schema.column_index(column).ok_or_else(|| {
                                QueryError::ColumnNotFound {
                                    table: String::new(),
                                    column: column.clone(),
                                }
                            })?;
                            let rids = btree.range_rids(start_val.as_ref(), end_val.as_ref());
                            let mut rows: Vec<Vec<Value>> = Vec::with_capacity(rids.len());
                            let mut cancel = CancelCheck::new();
                            for rid in rids {
                                cancel.tick()?;
                                // Overflow safety (P0-3): reassemble spilled cols.
                                if let Some(row) = tbl.get(rid) {
                                    if !row[col_idx].is_empty()
                                        && range_matches(
                                            &row[col_idx],
                                            &start_val,
                                            start_inclusive,
                                            &end_val,
                                            end_inclusive,
                                        )
                                    {
                                        rows.push(row);
                                    }
                                }
                            }
                            return Ok(QueryResult::Rows { columns, rows });
                        }
                    }
                }

                // Range scans use the btree fast path for unique indexes,
                // walking raw column-value keys directly.
                if tbl.is_index_unique(column) == Some(true) {
                    if let Some(btree) = tbl.index(column) {
                        let hits: Vec<(Value, RowId)> = match (&start_val, &end_val) {
                            (Some(s), Some(e)) => btree.range(s, e).collect(),
                            (Some(s), None) => btree.range_from(s),
                            (None, Some(e)) => btree.range_to(e),
                            (None, None) => {
                                let mut cancel = CancelCheck::new();
                                let mut rows: Vec<Vec<Value>> = Vec::new();
                                for (_, row) in tbl.scan() {
                                    cancel.tick()?;
                                    rows.push(row);
                                }
                                return Ok(QueryResult::Rows { columns, rows });
                            }
                        };
                        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(hits.len());
                        let mut cancel = CancelCheck::new();
                        for (key, rid) in hits {
                            cancel.tick()?;
                            if !start_inclusive {
                                if let Some(ref s) = start_val {
                                    if &key == s {
                                        continue;
                                    }
                                }
                            }
                            if !end_inclusive {
                                if let Some(ref e) = end_val {
                                    if &key == e {
                                        continue;
                                    }
                                }
                            }
                            // Overflow safety (P0-3): reassemble spilled cols.
                            if let Some(row) = tbl.get(rid) {
                                rows.push(row);
                            }
                        }
                        return Ok(QueryResult::Rows { columns, rows });
                    }
                }

                // Fallback: no index — synthesize range predicate and scan.
                // Overflow safety (P0-4): v2-capable tables use the decoded
                // last-resort scan below.
                let fast = FastLayout::new(schema);
                let synth = synthesize_range_predicate(column, start, end);
                if !tbl.has_overflow_rows() {
                    if let Some(compiled) = compile_predicate(&synth, &columns, &fast, schema) {
                        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(64);
                        for_each_row_raw_cancellable(&self.catalog, table, |_rid, data| {
                            if compiled(data) {
                                rows.push(decode_row(schema, data));
                            }
                        })?;
                        return Ok(QueryResult::Rows { columns, rows });
                    }
                }

                let col_idx =
                    schema
                        .column_index(column)
                        .ok_or_else(|| QueryError::ColumnNotFound {
                            table: String::new(),
                            column: column.clone(),
                        })?;
                let mut cancel = CancelCheck::new();
                let mut rows: Vec<Vec<Value>> = Vec::new();
                for (_, row) in tbl.scan() {
                    cancel.tick()?;
                    if range_matches(
                        &row[col_idx],
                        &start_val,
                        start_inclusive,
                        &end_val,
                        end_inclusive,
                    ) {
                        rows.push(row);
                    }
                }
                Ok(QueryResult::Rows { columns, rows })
            }
        }
    }

    // ─── Materialized view operations ──────────────────────────────────────

    /// Create a materialized view: execute the source query, store results
    /// in a new backing table, and register the view.
    fn create_view(&mut self, name: &str, query_text: &str) -> Result<(), QueryError> {
        if self.view_registry.is_view(name) {
            return Err(QueryError::ViewError(format!(
                "materialized view '{name}' already exists"
            )));
        }
        // Execute the source query to get the result set.
        let result = self.execute_powql(query_text)?;
        let (columns, rows) = match result {
            QueryResult::Rows { columns, rows } => (columns, rows),
            _ => return Err("view source query must be a SELECT".into()),
        };
        // Derive a schema for the backing table from the query result columns.
        let schema = self.derive_view_schema(name, &columns, &rows);
        // Create the backing table and insert the result rows.
        crate::cancel::check()?;
        self.catalog
            .create_table(schema)
            .map_err(|e| QueryError::StorageError(e.to_string()))?;
        for row in &rows {
            self.catalog
                .insert(name, row)
                .map_err(|e| QueryError::StorageError(e.to_string()))?;
        }
        // Determine which base tables this view depends on by parsing the query.
        let depends_on = self.extract_view_deps(query_text);
        self.view_registry
            .register(ViewDef {
                name: name.to_string(),
                query: query_text.to_string(),
                depends_on,
                dirty: false,
            })
            .map_err(|e| QueryError::StorageError(e.to_string()))?;
        Ok(())
    }

    /// Refresh a materialized view: re-execute its source query and replace
    /// the backing table's contents.
    fn refresh_view(&mut self, name: &str) -> Result<(), QueryError> {
        let def = self
            .view_registry
            .get(name)
            .ok_or_else(|| format!("materialized view '{name}' not found"))?;
        let query_text = def.query.clone();
        // Execute the source query.
        let result = self.execute_powql(&query_text)?;
        let (_columns, rows) = match result {
            QueryResult::Rows { columns, rows } => (columns, rows),
            _ => return Err("view source query must be a SELECT".into()),
        };
        // Clear old data and insert fresh results. Mission B2: logged
        // variant — view refreshes are a mutation and crash recovery
        // must see them.
        crate::cancel::check()?;
        self.catalog
            .scan_delete_matching_logged(name, |_| true)
            .map_err(|e| QueryError::StorageError(e.to_string()))?;
        for row in &rows {
            self.catalog
                .insert(name, row)
                .map_err(|e| QueryError::StorageError(e.to_string()))?;
        }
        self.view_registry.mark_clean(name);
        Ok(())
    }

    /// Drop a materialized view: remove the backing table and unregister.
    fn drop_view(&mut self, name: &str) -> Result<(), QueryError> {
        if !self.view_registry.is_view(name) {
            return Err(QueryError::ViewError(format!(
                "materialized view '{name}' not found"
            )));
        }
        self.view_registry
            .unregister(name)
            .map_err(|e| QueryError::StorageError(e.to_string()))?;
        self.catalog
            .drop_table(name)
            .map_err(|e| QueryError::StorageError(e.to_string()))?;
        Ok(())
    }

    /// Derive a storage `Schema` for a view's backing table from query
    /// result column names and the first row's types.
    fn derive_view_schema(&self, name: &str, columns: &[String], rows: &[Vec<Value>]) -> Schema {
        use powdb_storage::types::{ColumnDef, TypeId};
        let cols: Vec<ColumnDef> = columns
            .iter()
            .enumerate()
            .map(|(i, col_name)| {
                let type_id = rows
                    .first()
                    .and_then(|row| row.get(i))
                    .map(|v| v.type_id())
                    .unwrap_or(TypeId::Str);
                ColumnDef {
                    name: col_name.clone(),
                    type_id,
                    required: false,
                    position: i as u16,
                }
            })
            .collect();
        Schema {
            table_name: name.to_string(),
            columns: cols,
        }
    }

    /// Extract base table dependencies from a view's source query by
    /// parsing it and collecting the source table name.
    fn extract_view_deps(&self, query_text: &str) -> Vec<String> {
        use crate::parser::parse;
        match parse(query_text) {
            Ok(Statement::Query(q)) => {
                let mut deps = vec![q.source.clone()];
                for j in &q.joins {
                    deps.push(j.source.clone());
                }
                deps
            }
            _ => Vec::new(),
        }
    }

    // ─── Specialized fast paths ─────────────────────────────────────────────
    //
    // These methods are helpers for the `execute_plan` match arms above.
    // Each returns `Ok(Some(result))` when the fast path fires, `Ok(None)`
    // when the shape isn't supported (caller falls back to generic code).

    /// Aggregate sum/avg/min/max over a single fixed-size i64 column, with
    /// an optional compiled filter predicate. Walks raw row bytes — zero
    /// per-row allocation. Uses i128 accumulator for sum/avg overflow safety.
    pub(super) fn agg_single_col_fast(
        &self,
        table: &str,
        col: &str,
        function: AggFunc,
        predicate: Option<&Expr>,
    ) -> Result<Option<QueryResult>, QueryError> {
        // Overflow safety (P0-4): this walks raw rehydrated bytes and would
        // silently drop any row carrying a value too large to re-inline
        // (>= 64KB), undercounting the aggregate. Fall back to the decoded path.
        if self.catalog.table_has_overflow(table) {
            return Ok(None);
        }
        let schema = self
            .catalog
            .schema(table)
            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
            .clone();
        let columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
        let col_idx = match schema.column_index(col) {
            Some(i) => i,
            None => return Ok(None),
        };
        // Only fast-path fixed-size numeric columns (Int/Float) for
        // sum/avg/min/max/count. Mission D10: Float parity — prior version
        // bailed on Float columns, forcing them through the generic row-
        // decoding path that allocated a Vec<Value> per row and dispatched
        // on Value::cmp for every compare. f64 decode is structurally the
        // same as i64 (load 8 bytes, cast), so the fast path handles both.
        let col_type = schema.columns[col_idx].type_id;
        if col_type != TypeId::Int && col_type != TypeId::Float {
            return Ok(None);
        }

        let fast = FastLayout::new(&schema);
        // Mission C Phase 20b: inline the numeric-column reader instead of
        // building a `Box<dyn Fn>`. Eliminates 100K vtable dispatches per
        // 100K-row agg scan — every reader call folds directly into the
        // hot loop below.
        let byte_offset = match fast.fixed_offsets[col_idx] {
            Some(o) => o,
            None => return Ok(None),
        };
        let bitmap_byte = col_idx / 8;
        let bitmap_bit = (col_idx % 8) as u32;
        let body_data_offset = 2 + fast.bitmap_size + byte_offset;

        // Optional compiled filter.
        let compiled_pred: Option<CompiledPredicate> = match predicate {
            Some(pred) => match compile_predicate(pred, &columns, &fast, &schema) {
                Some(c) => Some(c),
                None => return Ok(None), // let generic path handle it
            },
            None => None,
        };

        // Mission C Phase 20b: specialize the inner loop per aggregate
        // function. The previous version ran a `match function { ... }`
        // *inside* the closure, which kept LLVM from producing optimal
        // scalar code for each variant (agg_max regressed ~23% vs the
        // baseline Box<dyn Fn> version even though per-row vtable cost
        // should have been strictly lower). Pushing the match out of the
        // hot loop lets each specialized body fold cleanly into
        // `for_each_row_raw` and removes a captured `AggFunc` + match
        // dispatch per row.
        //
        // Mission D10: same specialisation applies to the Float branch.
        // For Min/Max we use `f64::total_cmp` so the result matches
        // `Value::Ord` — this is the same ordering ORDER BY and the
        // top-N sort fast path use, keeping semantics consistent across
        // read paths (NaN compares as greatest, -0.0 < +0.0 for
        // deterministic tie-breaking).
        //
        // Mission D11 Phase 1: each inner loop now splits on presence of
        // a predicate (`if let Some(pred) = &compiled_pred`) so the hot
        // body never re-tests `Option` per row, and reads column bytes
        // via `read_i64_unchecked` / `read_f64_unchecked` helpers that
        // drop two bounds checks per row (null bitmap byte + value
        // slice). Safety is carried by the `FastLayout` invariant that
        // `data_offset + 8 <= row_len` for any fixed-size column; see
        // the helper doc comments. Hot loops are macro-generated so the
        // with-pred / no-pred split can't drift between variants.
        let result = match col_type {
            TypeId::Int => match function {
                AggFunc::Sum | AggFunc::Avg => {
                    let mut sum_i128: i128 = 0;
                    let mut count: i64 = 0;
                    agg_int_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: i64| {
                            count += 1;
                            sum_i128 += v as i128;
                        }
                    );
                    if matches!(function, AggFunc::Sum) {
                        let clamped = sum_i128.clamp(i64::MIN as i128, i64::MAX as i128) as i64;
                        QueryResult::Scalar(Value::Int(clamped))
                    } else if count == 0 {
                        QueryResult::Scalar(Value::Empty)
                    } else {
                        let avg = (sum_i128 as f64) / (count as f64);
                        QueryResult::Scalar(Value::Float(avg))
                    }
                }
                AggFunc::Min => {
                    let mut min_v: Option<i64> = None;
                    agg_int_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: i64| {
                            min_v = Some(match min_v {
                                Some(m) => m.min(v),
                                None => v,
                            });
                        }
                    );
                    QueryResult::Scalar(min_v.map(Value::Int).unwrap_or(Value::Empty))
                }
                AggFunc::Max => {
                    let mut max_v: Option<i64> = None;
                    agg_int_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: i64| {
                            max_v = Some(match max_v {
                                Some(m) => m.max(v),
                                None => v,
                            });
                        }
                    );
                    QueryResult::Scalar(max_v.map(Value::Int).unwrap_or(Value::Empty))
                }
                AggFunc::Count => {
                    let mut count: i64 = 0;
                    agg_int_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |_v: i64| {
                            count += 1;
                        }
                    );
                    QueryResult::Scalar(Value::Int(count))
                }
                AggFunc::CountDistinct => {
                    let mut seen = rustc_hash::FxHashSet::default();
                    agg_int_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: i64| {
                            seen.insert(v);
                        }
                    );
                    QueryResult::Scalar(Value::Int(seen.len() as i64))
                }
            },
            TypeId::Float => match function {
                AggFunc::Sum => {
                    // Use a single f64 accumulator. Naive summation is
                    // sufficient for MVP parity; if precision becomes an
                    // issue on long scans we can upgrade to Kahan–Neumaier
                    // compensated sum (~2x scalar cost, zero error growth).
                    let mut sum: f64 = 0.0;
                    agg_float_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: f64| {
                            sum += v;
                        }
                    );
                    QueryResult::Scalar(Value::Float(sum))
                }
                AggFunc::Avg => {
                    let mut sum: f64 = 0.0;
                    let mut count: i64 = 0;
                    agg_float_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: f64| {
                            sum += v;
                            count += 1;
                        }
                    );
                    if count == 0 {
                        QueryResult::Scalar(Value::Empty)
                    } else {
                        QueryResult::Scalar(Value::Float(sum / count as f64))
                    }
                }
                AggFunc::Min => {
                    // `total_cmp` for deterministic NaN handling (matches
                    // Value::Ord). NaN compares greatest, so Min will
                    // correctly ignore it in favour of any finite value.
                    let mut min_v: Option<f64> = None;
                    agg_float_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: f64| {
                            min_v = Some(match min_v {
                                Some(m) => {
                                    if v.total_cmp(&m).is_lt() {
                                        v
                                    } else {
                                        m
                                    }
                                }
                                None => v,
                            });
                        }
                    );
                    QueryResult::Scalar(min_v.map(Value::Float).unwrap_or(Value::Empty))
                }
                AggFunc::Max => {
                    let mut max_v: Option<f64> = None;
                    agg_float_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: f64| {
                            max_v = Some(match max_v {
                                Some(m) => {
                                    if v.total_cmp(&m).is_gt() {
                                        v
                                    } else {
                                        m
                                    }
                                }
                                None => v,
                            });
                        }
                    );
                    QueryResult::Scalar(max_v.map(Value::Float).unwrap_or(Value::Empty))
                }
                AggFunc::Count => {
                    let mut count: i64 = 0;
                    agg_float_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |_v: f64| {
                            count += 1;
                        }
                    );
                    QueryResult::Scalar(Value::Int(count))
                }
                AggFunc::CountDistinct => {
                    // Hash on `f64::to_bits` — matches `Value::Hash`, so
                    // distinct NaN bit patterns count as distinct and
                    // -0.0/+0.0 count as distinct. Consistent with how
                    // Float values are hashed in every other DISTINCT /
                    // GROUP BY path.
                    let mut seen = rustc_hash::FxHashSet::default();
                    agg_float_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: f64| {
                            seen.insert(v.to_bits());
                        }
                    );
                    QueryResult::Scalar(Value::Int(seen.len() as i64))
                }
            },
            _ => unreachable!("type guard above restricts to Int/Float"),
        };
        Ok(Some(result))
    }

    /// `Project(Limit(Filter(SeqScan)))` and `Project(Limit(SeqScan))`.
    /// Streams rows, decodes only projected columns, stops at the limit.
    pub(super) fn project_filter_limit_fast(
        &self,
        table: &str,
        fields: &[ProjectField],
        limit: usize,
        predicate: Option<&Expr>,
    ) -> Result<Option<QueryResult>, QueryError> {
        // Overflow safety (P0-4): raw-byte projection over rehydrated rows
        // drops any row with a value too large to re-inline (>= 64KB) and
        // cannot return such a value; fall back to the decoded generic path.
        if self.catalog.table_has_overflow(table) {
            return Ok(None);
        }
        let schema = self
            .catalog
            .schema(table)
            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
            .clone();
        let all_columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();

        // Each projection field must be a simple `.field` reference for this
        // fast path. Aliased or computed fields fall through.
        let mut proj_indices: Vec<usize> = Vec::with_capacity(fields.len());
        let mut proj_columns: Vec<String> = Vec::with_capacity(fields.len());
        for f in fields {
            let name = match &f.expr {
                Expr::Field(n) => n.clone(),
                _ => return Ok(None),
            };
            let idx = match all_columns.iter().position(|c| c == &name) {
                Some(i) => i,
                None => return Ok(None),
            };
            proj_indices.push(idx);
            proj_columns.push(f.alias.clone().unwrap_or(name));
        }

        let fast = FastLayout::new(&schema);
        let row_layout = RowLayout::new(&schema);

        let compiled_pred: Option<CompiledPredicate> = match predicate {
            Some(pred) => match compile_predicate(pred, &all_columns, &fast, &schema) {
                Some(c) => Some(c),
                None => return Ok(None),
            },
            None => None,
        };

        let mut out: Vec<Vec<Value>> = Vec::with_capacity(limit.min(1024));
        // Mission D2: use try_for_each_row_raw to actually stop iterating
        // once the limit is reached. The previous `done` flag only short-
        // circuited the closure body, so a `limit 100` over 100K rows still
        // walked all 100K slots — burning ~30x SQLite on scan_filter_project_top100.
        // Cooperative cancellation: an unbounded (limit == usize::MAX) projected
        // scan over a huge table must stay stoppable.
        let mut cancel = CancelCheck::new();
        let mut cancel_err: Option<QueryError> = None;
        self.catalog
            .try_for_each_row_raw(table, |_rid, data| {
                if let Err(e) = cancel.tick() {
                    cancel_err = Some(e);
                    return ControlFlow::Break(());
                }
                if let Some(ref pred) = compiled_pred {
                    if !pred(data) {
                        return ControlFlow::Continue(());
                    }
                }
                let row: Vec<Value> = proj_indices
                    .iter()
                    .map(|&ci| decode_column(&schema, &row_layout, data, ci))
                    .collect();
                out.push(row);
                if out.len() >= limit {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .map_err(|e| QueryError::StorageError(e.to_string()))?;
        if let Some(e) = cancel_err {
            return Err(e);
        }

        Ok(Some(QueryResult::Rows {
            columns: proj_columns,
            rows: out,
        }))
    }

    /// `Project(Limit(Sort(Filter(SeqScan))))` and `Project(Limit(Sort(SeqScan)))`.
    /// Bounded top-N heap over the sort key. Only the sort key needs to be
    /// read per row; projected columns are decoded only for the final
    /// winning rows when the heap drains.
    pub(super) fn project_filter_sort_limit_fast(
        &self,
        table: &str,
        fields: &[ProjectField],
        sort_field: &str,
        descending: bool,
        limit: usize,
        predicate: Option<&Expr>,
    ) -> Result<Option<QueryResult>, QueryError> {
        // Overflow safety (P0-4): raw-byte scan drops/wraps >= 64KB values;
        // let the decoded generic path handle v2-capable tables.
        if self.catalog.table_has_overflow(table) {
            return Ok(None);
        }
        if limit == 0 {
            // Degenerate case — empty result. Let the generic path handle it
            // for proper column naming.
            return Ok(None);
        }
        // The top-N heaps never hold more than `limit` rows, but `limit` is an
        // attacker-supplied literal (`order .x limit 99999999999`). Reserving
        // that capacity up front would allocate gigabytes and abort the
        // process before a single row is read. Cap the pre-allocation; the
        // heaps still grow on demand up to the true `limit`.
        const TOPN_PREALLOC_CAP: usize = 4096;
        let prealloc = limit.min(TOPN_PREALLOC_CAP);
        let schema = self
            .catalog
            .schema(table)
            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
            .clone();
        let all_columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();

        // Sort key must be a fixed-size numeric column (Int or Float).
        // Mission D10: extended from Int-only. Float sort keys use a
        // sortable-u64 transform (see `f64_to_sortable_u64`) so the heap
        // path stays keyed on `u64` and the whole branch shape is
        // identical to the Int case — no new heap types, no `total_cmp`
        // closures in the hot loop.
        let sort_idx = match schema.column_index(sort_field) {
            Some(i) => i,
            None => return Ok(None),
        };
        let sort_col_type = schema.columns[sort_idx].type_id;
        if sort_col_type != TypeId::Int && sort_col_type != TypeId::Float {
            return Ok(None);
        }

        // Each projection field must be a simple `.field`.
        let mut proj_indices: Vec<usize> = Vec::with_capacity(fields.len());
        let mut proj_columns: Vec<String> = Vec::with_capacity(fields.len());
        for f in fields {
            let name = match &f.expr {
                Expr::Field(n) => n.clone(),
                _ => return Ok(None),
            };
            let idx = match all_columns.iter().position(|c| c == &name) {
                Some(i) => i,
                None => return Ok(None),
            };
            proj_indices.push(idx);
            proj_columns.push(f.alias.clone().unwrap_or(name));
        }

        let fast = FastLayout::new(&schema);
        let row_layout = RowLayout::new(&schema);
        // Mission C Phase 20b: inline numeric-column reader (no Box<dyn Fn>).
        let sort_byte_offset = match fast.fixed_offsets[sort_idx] {
            Some(o) => o,
            None => return Ok(None),
        };
        let sort_bitmap_byte = sort_idx / 8;
        let sort_bitmap_bit = (sort_idx % 8) as u32;
        let sort_body_data_offset = 2 + fast.bitmap_size + sort_byte_offset;

        let compiled_pred: Option<CompiledPredicate> = match predicate {
            Some(pred) => match compile_predicate(pred, &all_columns, &fast, &schema) {
                Some(c) => Some(c),
                None => return Ok(None),
            },
            None => None,
        };

        // Bounded top-N heap. For `order .x desc limit N`, we want the N
        // largest values — use a min-heap so the smallest is at the top and
        // can be popped when a better candidate arrives. For ascending, use
        // a max-heap. We tie-break with a monotonic `seq` counter so the
        // result is deterministic and stable.
        //
        // To keep this simple we maintain two typed heaps and pick by
        // direction.
        let drained: Vec<Vec<u8>> = match sort_col_type {
            TypeId::Int => {
                let mut seq: u64 = 0;
                let mut heap_desc: BinaryHeap<Reverse<(i64, u64, Vec<u8>)>> =
                    BinaryHeap::with_capacity(prealloc);
                let mut heap_asc: BinaryHeap<(i64, u64, Vec<u8>)> =
                    BinaryHeap::with_capacity(prealloc);
                let mut null_rows: Vec<Vec<u8>> = Vec::with_capacity(prealloc);

                for_each_row_raw_cancellable(&self.catalog, table, |_rid, data| {
                    if let Some(ref pred) = compiled_pred {
                        if !pred(data) {
                            return;
                        }
                    }
                    // Inlined int-column reader: null check + i64 decode.
                    let base = row_body_base(data);
                    let sort_data_offset = base + sort_body_data_offset;
                    if data.len() < sort_data_offset + 8
                        || data.len() <= base + 2 + sort_bitmap_byte
                    {
                        return;
                    }
                    let is_null = (data[base + 2 + sort_bitmap_byte] >> sort_bitmap_bit) & 1 == 1;
                    let id = seq;
                    seq += 1;
                    if is_null {
                        if null_rows.len() < limit {
                            null_rows.push(data.to_vec());
                        }
                        return;
                    }
                    let key = i64::from_le_bytes(
                        data[sort_data_offset..sort_data_offset + 8]
                            .try_into()
                            .unwrap_or_else(|_| unreachable!()),
                    );
                    if descending {
                        if heap_desc.len() < limit {
                            heap_desc.push(Reverse((key, id, data.to_vec())));
                        } else if let Some(Reverse((top_key, _, _))) = heap_desc.peek() {
                            if key > *top_key {
                                heap_desc.pop();
                                heap_desc.push(Reverse((key, id, data.to_vec())));
                            }
                        }
                    } else if heap_asc.len() < limit {
                        heap_asc.push((key, id, data.to_vec()));
                    } else if let Some((top_key, _, _)) = heap_asc.peek() {
                        if key < *top_key {
                            heap_asc.pop();
                            heap_asc.push((key, id, data.to_vec()));
                        }
                    }
                })?;

                let mut drained: Vec<(i64, u64, Vec<u8>)> = if descending {
                    heap_desc.into_iter().map(|Reverse(t)| t).collect()
                } else {
                    heap_asc.into_iter().collect()
                };
                if descending {
                    cooperative_stable_sort_by(&mut drained, self.query_memory_limit, |a, b| {
                        b.0.cmp(&a.0).then(a.1.cmp(&b.1))
                    })?;
                } else {
                    cooperative_stable_sort_by(&mut drained, self.query_memory_limit, |a, b| {
                        a.0.cmp(&b.0).then(a.1.cmp(&b.1))
                    })?;
                }
                let mut rows: Vec<Vec<u8>> = drained.into_iter().map(|(_, _, d)| d).collect();
                rows.extend(null_rows.into_iter().take(limit.saturating_sub(rows.len())));
                rows
            }
            TypeId::Float => {
                // Novel angle: rather than introducing a `TotalF64` newtype
                // with `Ord via total_cmp`, transform the f64 bit pattern
                // into a sortable `u64` so `BinaryHeap<u64>` orders exactly
                // like `f64::total_cmp` would. Classic trick: flip the sign
                // bit on positives, flip all bits on negatives. Result:
                // - NaN (sign=0) stays greatest, matching total_cmp
                // - -0.0 sorts before +0.0, matching total_cmp
                // - Hot loop is branch-cheap (one compare + one xor)
                let mut seq: u64 = 0;
                let mut heap_desc: BinaryHeap<Reverse<(u64, u64, Vec<u8>)>> =
                    BinaryHeap::with_capacity(prealloc);
                let mut heap_asc: BinaryHeap<(u64, u64, Vec<u8>)> =
                    BinaryHeap::with_capacity(prealloc);
                let mut null_rows: Vec<Vec<u8>> = Vec::with_capacity(prealloc);

                for_each_row_raw_cancellable(&self.catalog, table, |_rid, data| {
                    if let Some(ref pred) = compiled_pred {
                        if !pred(data) {
                            return;
                        }
                    }
                    let base = row_body_base(data);
                    let sort_data_offset = base + sort_body_data_offset;
                    if data.len() < sort_data_offset + 8
                        || data.len() <= base + 2 + sort_bitmap_byte
                    {
                        return;
                    }
                    let is_null = (data[base + 2 + sort_bitmap_byte] >> sort_bitmap_bit) & 1 == 1;
                    let id = seq;
                    seq += 1;
                    if is_null {
                        if null_rows.len() < limit {
                            null_rows.push(data.to_vec());
                        }
                        return;
                    }
                    let bits = u64::from_le_bytes(
                        data[sort_data_offset..sort_data_offset + 8]
                            .try_into()
                            .unwrap_or_else(|_| unreachable!()),
                    );
                    let key = f64_bits_to_sortable_u64(bits);
                    if descending {
                        if heap_desc.len() < limit {
                            heap_desc.push(Reverse((key, id, data.to_vec())));
                        } else if let Some(Reverse((top_key, _, _))) = heap_desc.peek() {
                            if key > *top_key {
                                heap_desc.pop();
                                heap_desc.push(Reverse((key, id, data.to_vec())));
                            }
                        }
                    } else if heap_asc.len() < limit {
                        heap_asc.push((key, id, data.to_vec()));
                    } else if let Some((top_key, _, _)) = heap_asc.peek() {
                        if key < *top_key {
                            heap_asc.pop();
                            heap_asc.push((key, id, data.to_vec()));
                        }
                    }
                })?;

                let mut drained: Vec<(u64, u64, Vec<u8>)> = if descending {
                    heap_desc.into_iter().map(|Reverse(t)| t).collect()
                } else {
                    heap_asc.into_iter().collect()
                };
                if descending {
                    cooperative_stable_sort_by(&mut drained, self.query_memory_limit, |a, b| {
                        b.0.cmp(&a.0).then(a.1.cmp(&b.1))
                    })?;
                } else {
                    cooperative_stable_sort_by(&mut drained, self.query_memory_limit, |a, b| {
                        a.0.cmp(&b.0).then(a.1.cmp(&b.1))
                    })?;
                }
                let mut rows: Vec<Vec<u8>> = drained.into_iter().map(|(_, _, d)| d).collect();
                rows.extend(null_rows.into_iter().take(limit.saturating_sub(rows.len())));
                rows
            }
            _ => unreachable!("type guard above restricts to Int/Float"),
        };

        let mut cancel = CancelCheck::new();
        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(drained.len());
        for data in drained {
            cancel.tick()?;
            rows.push(
                proj_indices
                    .iter()
                    .map(|&ci| decode_column(&schema, &row_layout, &data, ci))
                    .collect(),
            );
        }

        Ok(Some(QueryResult::Rows {
            columns: proj_columns,
            rows,
        }))
    }

    /// Gather the RowIds that a mutation should operate on, without
    /// materialising the full row set. Handles the shapes the planner emits
    /// for update/delete: SeqScan, IndexScan, and Filter(SeqScan). Other
    /// shapes fall back to `generic_rid_match`.
    ///
    /// Perf sprint: try to fuse the predicate evaluation and in-place
    /// byte-level mutation into a single heap walk. Returns `Some(result)`
    /// if the fused path fired, `None` to fall through to the generic
    /// two-pass code.
    ///
    /// Covers two shapes:
    /// 1. Fixed-width non-null literal assignments on non-indexed columns
    ///    → byte-patch every matched row in place (row length unchanged).
    /// 2. Single var-col literal assignment on a non-indexed column
    ///    → `patch_var_column_in_place` on every matched row (may shrink);
    ///    rows that can't be patched in place are collected for fallback.
    fn try_fused_scan_update(
        &mut self,
        table: &str,
        predicate: &Expr,
        resolved: &[(usize, Value)],
        changed_cols: &[usize],
    ) -> Option<Result<QueryResult, QueryError>> {
        // Overflow safety (P0/P1): a table that may hold v2 rows can never take
        // the byte-patch fast paths — patching computes v1 offsets and would
        // corrupt a spilled row, and the compiled predicate over raw bytes
        // mis-evaluates a spilled column. Fall through to the reassembling
        // collect-rids + get/update_hinted path.
        if self.catalog.table_has_overflow(table) {
            return None;
        }
        // Build compiled predicate. Requires a schema borrow that must be
        // dropped before we call scan_patch_matching_logged.
        let compiled = {
            let schema = self.catalog.schema(table)?;
            let columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
            let fast = FastLayout::new(schema);
            compile_predicate(predicate, &columns, &fast, schema)?
        };

        // ── Path 1: fixed-width fast patch ──────────────────────────
        let fixed_patches: Option<Vec<FastPatch>> = {
            let tbl = self.catalog.get_table(table)?;
            let schema = tbl.schema();
            let all_fixed_nonnull = resolved
                .iter()
                .all(|(idx, val)| is_fixed_size(schema.columns[*idx].type_id) && !val.is_empty());
            let no_indexed = !resolved.iter().any(|(idx, _)| tbl.has_indexed_col(*idx));
            if all_fixed_nonnull && no_indexed {
                let layout = RowLayout::new(schema);
                let bitmap_size = layout.bitmap_size();
                Some(
                    resolved
                        .iter()
                        .map(|(idx, val)| {
                            let fixed_off = layout
                                .fixed_offset(*idx)
                                .expect("is_fixed_size already checked");
                            let field_off = 2 + bitmap_size + fixed_off;
                            let bytes: FixedBytes = match val {
                                Value::Int(v) => FixedBytes::I64(v.to_le_bytes()),
                                Value::Float(v) => FixedBytes::F64(v.to_le_bytes()),
                                Value::Bool(v) => FixedBytes::Bool(if *v { 1 } else { 0 }),
                                Value::DateTime(v) => FixedBytes::I64(v.to_le_bytes()),
                                Value::Uuid(v) => FixedBytes::Uuid(*v),
                                _ => unreachable!("all_fixed_nonnull guard"),
                            };
                            FastPatch {
                                field_off,
                                bitmap_byte_off: 2 + idx / 8,
                                bit_mask: 1u8 << (idx % 8),
                                bytes,
                            }
                        })
                        .collect(),
                )
            } else {
                None
            }
        };
        if let Some(patches) = fixed_patches {
            let result = self
                .catalog
                .scan_patch_matching_logged(table, compiled, |row| {
                    let base = row_body_base(row);
                    for p in &patches {
                        row[base + p.bitmap_byte_off] &= !p.bit_mask;
                        let field_bytes = p.bytes.as_slice();
                        row[base + p.field_off..base + p.field_off + field_bytes.len()]
                            .copy_from_slice(field_bytes);
                    }
                    Some(row.len() as u16)
                })
                .map_err(|e| e.to_string());
            match result {
                Ok((count, _)) => {
                    self.view_registry.mark_dependents_dirty(table);
                    return Some(Ok(QueryResult::Modified(count)));
                }
                Err(e) => return Some(Err(QueryError::Execution(e))),
            }
        }

        // ── Path 2: single var-col shrink fast patch ────────────────
        let var_patch: Option<(usize, Option<Vec<u8>>)> = {
            let tbl = self.catalog.get_table(table)?;
            let schema = tbl.schema();
            let is_single = resolved.len() == 1;
            let is_var = is_single && !is_fixed_size(schema.columns[resolved[0].0].type_id);
            let no_indexed = !resolved.iter().any(|(idx, _)| tbl.has_indexed_col(*idx));
            if is_single && is_var && no_indexed {
                let (idx, val) = &resolved[0];
                let bytes_opt = match val {
                    Value::Str(s) => Some(s.as_bytes().to_vec()),
                    Value::Bytes(b) => Some(b.clone()),
                    Value::Empty => None,
                    _ => return None, // type mismatch, fall through
                };
                Some((*idx, bytes_opt))
            } else {
                None
            }
        };
        if let Some((col_idx, ref new_bytes_opt)) = var_patch {
            // Build a fresh RowLayout before the mutable borrow.
            let layout = {
                let schema = self.catalog.schema(table)?;
                RowLayout::new(schema)
            };
            let new_bytes_ref: Option<&[u8]> = new_bytes_opt.as_deref();
            let result = self
                .catalog
                .scan_patch_matching_logged(table, compiled, |row| {
                    patch_var_column_in_place(row, &layout, col_idx, new_bytes_ref)
                })
                .map_err(|e| e.to_string());
            match result {
                Ok((mut count, fallback_rids)) => {
                    // Handle rows where in-place patch failed (new > old).
                    for rid in fallback_rids {
                        let mut row = match self.catalog.get(table, rid) {
                            Some(r) => r,
                            None => continue,
                        };
                        for (idx, val) in resolved.iter() {
                            row[*idx] = val.clone();
                        }
                        if let Err(e) =
                            self.catalog
                                .update_hinted(table, rid, &row, Some(changed_cols))
                        {
                            return Some(Err(QueryError::StorageError(e.to_string())));
                        }
                        count += 1;
                    }
                    self.view_registry.mark_dependents_dirty(table);
                    return Some(Ok(QueryResult::Modified(count)));
                }
                Err(e) => return Some(Err(QueryError::Execution(e))),
            }
        }

        None // no fused path applicable — fall through
    }

    /// Collect the RowIds a lowered index-scan node yields, applying the same
    /// exclusive-bound and null-skip rechecks the SELECT executor uses, or
    /// `None` when `scan` is not an index-scan shape the mutation path can
    /// drive from (the caller then falls back to the generic matcher). This is
    /// what keeps an index-driven conjunction update/delete off the O(N*M)
    /// value-rematch path. Every heap fetch goes through `Table::get`, which
    /// reassembles spilled columns, so it is overflow-safe.
    fn index_scan_rids(&self, scan: &PlanNode) -> Result<Option<Vec<RowId>>, QueryError> {
        match scan {
            PlanNode::IndexScan { table, column, key } => {
                let Some(tbl) = self.catalog.get_table(table) else {
                    return Ok(None);
                };
                if !tbl.has_index(column) {
                    return Ok(None);
                }
                let key_value = literal_to_value(key)?;
                Ok(Some(tbl.index_lookup_all(column, &key_value)))
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
                        .map_err(|error| QueryError::StorageError(error.to_string()))?
                };
                Ok(Some(rids))
            }
            PlanNode::RangeScan {
                table,
                column,
                start,
                end,
            } => {
                let Some(tbl) = self.catalog.get_table(table) else {
                    return Ok(None);
                };
                let start_val = start
                    .as_ref()
                    .map(|(expr, _)| literal_to_value(expr))
                    .transpose()?;
                let end_val = end
                    .as_ref()
                    .map(|(expr, _)| literal_to_value(expr))
                    .transpose()?;
                let start_inclusive = start.as_ref().map(|(_, inc)| *inc).unwrap_or(true);
                let end_inclusive = end.as_ref().map(|(_, inc)| *inc).unwrap_or(true);
                // Unique and non-unique indexes store keys differently, so their
                // range walks differ, so mirror the SELECT `RangeScan` executor.
                match tbl.is_index_unique(column) {
                    Some(false) => {
                        let col_idx = tbl.schema().column_index(column).ok_or_else(|| {
                            QueryError::ColumnNotFound {
                                table: String::new(),
                                column: column.clone(),
                            }
                        })?;
                        let Some(btree) = tbl.index(column) else {
                            return Ok(None);
                        };
                        // `range_rids` is inclusive over the composite prefix;
                        // recheck enforces exclusive bounds and skips nulls
                        // (never indexed).
                        let candidates = btree.range_rids(start_val.as_ref(), end_val.as_ref());
                        let mut rids = Vec::with_capacity(candidates.len());
                        let mut cancel = CancelCheck::new();
                        for rid in candidates {
                            cancel.tick()?;
                            if let Some(row) = tbl.get(rid) {
                                if !row[col_idx].is_empty()
                                    && range_matches(
                                        &row[col_idx],
                                        &start_val,
                                        start_inclusive,
                                        &end_val,
                                        end_inclusive,
                                    )
                                {
                                    rids.push(rid);
                                }
                            }
                        }
                        Ok(Some(rids))
                    }
                    Some(true) => {
                        let Some(btree) = tbl.index(column) else {
                            return Ok(None);
                        };
                        // Unique index: raw column-value keys. An unbounded scan
                        // is not a range shape the planner emits here, so defer it
                        // to the generic path rather than a full index walk.
                        let hits: Vec<(Value, RowId)> = match (&start_val, &end_val) {
                            (Some(s), Some(e)) => btree.range(s, e).collect(),
                            (Some(s), None) => btree.range_from(s),
                            (None, Some(e)) => btree.range_to(e),
                            (None, None) => return Ok(None),
                        };
                        let mut rids = Vec::with_capacity(hits.len());
                        let mut cancel = CancelCheck::new();
                        for (key, rid) in hits {
                            cancel.tick()?;
                            if !start_inclusive {
                                if let Some(ref s) = start_val {
                                    if &key == s {
                                        continue;
                                    }
                                }
                            }
                            if !end_inclusive {
                                if let Some(ref e) = end_val {
                                    if &key == e {
                                        continue;
                                    }
                                }
                            }
                            rids.push(rid);
                        }
                        Ok(Some(rids))
                    }
                    None => Ok(None),
                }
            }
            PlanNode::ExprRangeScan {
                table,
                path,
                start,
                end,
            } => {
                let Some(index) = resolve_expression_index(&self.catalog, table, path) else {
                    return Ok(None);
                };
                let start_val = start
                    .as_ref()
                    .map(|(expr, _)| literal_to_value(expr))
                    .transpose()?;
                let end_val = end
                    .as_ref()
                    .map(|(expr, _)| literal_to_value(expr))
                    .transpose()?;
                let start_inclusive = start.as_ref().map(|(_, inc)| *inc).unwrap_or(true);
                let end_inclusive = end.as_ref().map(|(_, inc)| *inc).unwrap_or(true);
                let candidates = self
                    .catalog
                    .expression_index_range_rids(
                        table,
                        index.index_id,
                        start_val.as_ref(),
                        end_val.as_ref(),
                    )
                    .map_err(|error| QueryError::StorageError(error.to_string()))?;
                let schema = self
                    .catalog
                    .schema(table)
                    .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                let all_columns: Vec<String> =
                    schema.columns.iter().map(|c| c.name.clone()).collect();
                let path_expr = stored_json_path_expr(path);
                let mut rids = Vec::with_capacity(candidates.len());
                let mut cancel = CancelCheck::new();
                for rid in candidates {
                    cancel.tick()?;
                    let Some(row) = self.catalog.get(table, rid) else {
                        continue;
                    };
                    let value = eval_expr(&path_expr, &row, &all_columns);
                    if value.is_empty()
                        || !range_matches(
                            &value,
                            &start_val,
                            start_inclusive,
                            &end_val,
                            end_inclusive,
                        )
                    {
                        continue;
                    }
                    rids.push(rid);
                }
                Ok(Some(rids))
            }
            _ => Ok(None),
        }
    }

    /// Rid collection for `Filter(<index scan>)` mutation discovery: narrow to
    /// the index scan's candidate rids, then recheck the residual predicate
    /// while decoding only the columns it references (`get_projected`), exactly
    /// as [`Self::try_filter_index_residual_fast`] does for reads. Returns
    /// `None` when the inner scan is not an index shape over `table`, or when
    /// the residual carries a subquery (which cannot be rechecked row-at-a-time
    /// here), so the caller keeps the correct generic path.
    fn collect_rids_via_index_residual(
        &self,
        inner: &PlanNode,
        predicate: &Expr,
        table: &str,
    ) -> Result<Option<Vec<RowId>>, QueryError> {
        if contains_subquery(predicate) || scan_table(inner) != Some(table) {
            return Ok(None);
        }
        let Some(candidates) = self.index_scan_rids(inner)? else {
            return Ok(None);
        };
        let schema = self
            .catalog
            .schema(table)
            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
        let all_columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
        let residual_indices = predicate_column_indices_json(predicate, &all_columns);
        let residual_names: Vec<String> = residual_indices
            .iter()
            .map(|&index| all_columns[index].clone())
            .collect();
        let mut rids = Vec::new();
        let mut cancel = CancelCheck::new();
        for rid in candidates {
            cancel.tick()?;
            let Some(sparse) = self
                .catalog
                .get_projected(table, rid, &residual_indices)
                .map_err(|error| QueryError::StorageError(error.to_string()))?
            else {
                continue;
            };
            if eval_predicate(predicate, &sparse, &residual_names) {
                rids.push(rid);
            }
        }
        Ok(Some(rids))
    }

    /// Mission C Phase 3: schema is looked up via `self.catalog.schema(table)`
    /// inside the branches that actually need it. Previously the caller had
    /// to clone the full Schema (6+ String allocs) before every mutation just
    /// so this function could borrow it — a cost the update/delete hot path
    /// did not need.
    fn collect_rids_for_mutation(
        &mut self,
        input: &PlanNode,
        table: &str,
    ) -> Result<Vec<RowId>, QueryError> {
        // Overflow safety (P1/P0-4): the raw-byte fast paths below stream
        // through `for_each_row_raw`, which rehydrates v2 rows to v1 and SKIPS
        // any row carrying a value too large to re-inline (>= 64KB). For a
        // v2-capable table, evaluate the predicate over fully decoded rows
        // instead so no matching row is missed or mis-judged on a spilled
        // column. Exact index lookups (value-size independent) still fall
        // through to the normal path.
        if self.catalog.table_has_overflow(table) {
            if let Some(rids) = self.collect_rids_decoded(input, table)? {
                return Ok(rids);
            }
        }
        match input {
            PlanNode::SeqScan { table: t } if t == table => {
                // "Update/delete everything" — rare but legal.
                let mut cancel = CancelCheck::new();
                let mut rids: Vec<RowId> = Vec::new();
                for (rid, _) in self
                    .catalog
                    .scan(table)
                    .map_err(|e| QueryError::StorageError(e.to_string()))?
                {
                    cancel.tick()?;
                    rids.push(rid);
                }
                Ok(rids)
            }
            PlanNode::IndexScan {
                table: t,
                column,
                key,
            } if t == table => {
                let key_value = literal_to_value(key)?;

                // Indexed case: single lookup, 0 or 1 rows.
                // Mission D7: int-specialized fast path on int-keyed indexes
                // (primary keys, created_at, etc.) — the common case for
                // `update_by_pk` / `delete where id = ?`.
                //
                // Scope the `tbl` borrow so it's released before we fall
                // through to the scan-based paths below (which reborrow
                // `self.catalog`).
                {
                    let tbl = self
                        .catalog
                        .get_table(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                    if tbl.has_index(column) {
                        let rids = tbl.index_lookup_all(column, &key_value);
                        return Ok(rids);
                    }
                }

                // No index: the planner folds `.col = literal` to IndexScan
                // regardless of whether the column is actually unique. When
                // there's no index we must behave like Filter(SeqScan) and
                // return *all* matching RIDs — not just the first one.
                let schema = self
                    .catalog
                    .schema(table)
                    .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                let columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
                let fast = FastLayout::new(schema);
                let synth = Expr::BinaryOp(
                    Box::new(Expr::Field(column.clone())),
                    BinOp::Eq,
                    Box::new(key.clone()),
                );
                if let Some(compiled) = compile_predicate(&synth, &columns, &fast, schema) {
                    // Mission F: skip the first 4 Vec doublings.
                    let mut rids: Vec<RowId> = Vec::with_capacity(64);
                    let mut cancel = CancelCheck::new();
                    let mut cancel_err: Option<QueryError> = None;
                    self.catalog
                        .try_for_each_row_raw(table, |rid, data| {
                            if let Err(e) = cancel.tick() {
                                cancel_err = Some(e);
                                return ControlFlow::Break(());
                            }
                            if compiled(data) {
                                rids.push(rid);
                            }
                            ControlFlow::Continue(())
                        })
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    if let Some(e) = cancel_err {
                        return Err(e);
                    }
                    return Ok(rids);
                }

                // Fallback: decode each row, compare values.
                let col_idx =
                    schema
                        .column_index(column)
                        .ok_or_else(|| QueryError::ColumnNotFound {
                            table: String::new(),
                            column: column.clone(),
                        })?;
                let mut cancel = CancelCheck::new();
                let mut rids: Vec<RowId> = Vec::new();
                for (rid, row) in self
                    .catalog
                    .scan(table)
                    .map_err(|e| QueryError::StorageError(e.to_string()))?
                {
                    cancel.tick()?;
                    if row[col_idx] == key_value {
                        rids.push(rid);
                    }
                }
                Ok(rids)
            }
            PlanNode::RangeScan { table: t, .. }
            | PlanNode::ExprIndexScan { table: t, .. }
            | PlanNode::ExprRangeScan { table: t, .. }
                if t == table =>
            {
                // A conjunction whose residual was fully consumed lowers to a
                // bare index scan (no Filter). Collect its rids from the index
                // directly instead of the generic value rematch.
                match self.index_scan_rids(input)? {
                    Some(rids) => Ok(rids),
                    None => self.generic_rid_match(input, table),
                }
            }
            PlanNode::Filter {
                input: inner,
                predicate,
            } => {
                if let PlanNode::SeqScan { table: t } = inner.as_ref() {
                    if t != table {
                        return self.generic_rid_match(input, table);
                    }
                    let schema = self
                        .catalog
                        .schema(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
                    let columns: Vec<String> =
                        schema.columns.iter().map(|c| c.name.clone()).collect();
                    let fast = FastLayout::new(schema);
                    let row_layout = RowLayout::new(schema);

                    // Cooperative cancellation: the rid-collection scan that
                    // backs `update/delete filter <unindexed pred>` walks the
                    // whole table, so it must stay stoppable.
                    let mut cancel = CancelCheck::new();
                    let mut cancel_err: Option<QueryError> = None;
                    // Try compiled predicate first.
                    if let Some(compiled) = compile_predicate(predicate, &columns, &fast, schema) {
                        // Mission F: skip the first 4 Vec doublings.
                        let mut rids: Vec<RowId> = Vec::with_capacity(64);
                        self.catalog
                            .try_for_each_row_raw(table, |rid, data| {
                                if let Err(e) = cancel.tick() {
                                    cancel_err = Some(e);
                                    return ControlFlow::Break(());
                                }
                                if compiled(data) {
                                    rids.push(rid);
                                }
                                ControlFlow::Continue(())
                            })
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                        if let Some(e) = cancel_err {
                            return Err(e);
                        }
                        return Ok(rids);
                    }

                    // Fallback: selective decode + eval.
                    let pred_cols = predicate_column_indices_json(predicate, &columns);
                    let mut rids: Vec<RowId> = Vec::with_capacity(64);
                    self.catalog
                        .try_for_each_row_raw(table, |rid, data| {
                            if let Err(e) = cancel.tick() {
                                cancel_err = Some(e);
                                return ControlFlow::Break(());
                            }
                            let pred_row = decode_selective(schema, &row_layout, data, &pred_cols);
                            if eval_predicate(predicate, &pred_row, &columns) {
                                rids.push(rid);
                            }
                            ControlFlow::Continue(())
                        })
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    if let Some(e) = cancel_err {
                        return Err(e);
                    }
                    return Ok(rids);
                }
                // Lane A mutation fast path: a conjunction update/delete whose
                // discovery scan lowered to `Filter(<index scan>)` collects
                // candidate rids from the index and rechecks the residual per
                // rid, instead of the O(N*M) generic value rematch.
                if let Some(rids) = self.collect_rids_via_index_residual(inner, predicate, table)? {
                    return Ok(rids);
                }
                self.generic_rid_match(input, table)
            }
            _ => self.generic_rid_match(input, table),
        }
    }

    /// Decode-based rid collection for v2-capable tables (see the guard in
    /// [`Self::collect_rids_for_mutation`]). Scans fully reassembled rows via
    /// `Catalog::scan` (`decode_row_v2`, chain fetch, correct for any value
    /// size) and evaluates the predicate on decoded `Value`s. Returns `None`
    /// for shapes it does not special-case (indexed `IndexScan`, or anything
    /// exotic) so the caller falls through to the normal path.
    fn collect_rids_decoded(
        &mut self,
        input: &PlanNode,
        table: &str,
    ) -> Result<Option<Vec<RowId>>, QueryError> {
        // Determine the per-row predicate (None = match every row).
        let pred: Option<Expr> = match input {
            PlanNode::SeqScan { table: t } if t == table => None,
            PlanNode::Filter {
                input: inner,
                predicate,
            } => match inner.as_ref() {
                PlanNode::SeqScan { table: t } if t == table => Some(predicate.clone()),
                _ => return Ok(None),
            },
            PlanNode::IndexScan {
                table: t,
                column,
                key,
            } if t == table => {
                // A real index makes the lookup exact and value-size
                // independent — let the normal IndexScan path handle it.
                let indexed = self
                    .catalog
                    .get_table(table)
                    .map(|tb| tb.has_index(column))
                    .unwrap_or(false);
                if indexed {
                    return Ok(None);
                }
                Some(Expr::BinaryOp(
                    Box::new(Expr::Field(column.clone())),
                    BinOp::Eq,
                    Box::new(key.clone()),
                ))
            }
            _ => return Ok(None),
        };

        let columns: Vec<String> = {
            let schema = self
                .catalog
                .schema(table)
                .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?;
            schema.columns.iter().map(|c| c.name.clone()).collect()
        };
        let mut rids: Vec<RowId> = Vec::new();
        let mut cancel = CancelCheck::new();
        for (rid, row) in self
            .catalog
            .scan(table)
            .map_err(|e| QueryError::StorageError(e.to_string()))?
        {
            cancel.tick()?;
            let keep = match &pred {
                None => true,
                Some(p) => eval_predicate(p, &row, &columns),
            };
            if keep {
                rids.push(rid);
            }
        }
        Ok(Some(rids))
    }

    /// Last-ditch generic match: execute the plan, collect matching rows,
    /// then find corresponding RowIds by value equality. This is the old
    /// O(N*M) code path; only used when the plan shape is something exotic.
    fn generic_rid_match(
        &mut self,
        input: &PlanNode,
        table: &str,
    ) -> Result<Vec<RowId>, QueryError> {
        #[cfg(test)]
        GENERIC_RID_MATCH_CALLS.with(|calls| calls.set(calls.get() + 1));
        let result = self.execute_plan(input)?;
        let rows = match result {
            QueryResult::Rows { rows, .. } => rows,
            _ => return Err("mutation source must be rows".into()),
        };
        let mut matching: Vec<RowId> = Vec::new();
        let mut cancel = CancelCheck::new();
        for (rid, row) in self
            .catalog
            .scan(table)
            .map_err(|e| QueryError::StorageError(e.to_string()))?
        {
            cancel.tick()?;
            let mut matched = false;
            for candidate in &rows {
                cancel.tick()?;
                if candidate == &row {
                    matched = true;
                    break;
                }
            }
            if matched {
                matching.push(rid);
            }
        }
        Ok(matching)
    }
}

pub(super) fn execute_window(
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
        let mut running_int_sum: i64 = 0;
        let mut running_float_sum: f64 = 0.0;
        let mut running_saw_float = false;
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
                    let final_v = win_values[indices[sorted_pos - 1]].clone();
                    for ri in partition_row_indices.drain(..) {
                        cancel.tick()?;
                        win_values[ri] = final_v.clone();
                    }
                }
                partition_start = sorted_pos;
                running_count = 0;
                running_int_sum = 0;
                running_float_sum = 0.0;
                running_saw_float = false;
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
                        match value {
                            Value::Int(v) => running_int_sum += v,
                            Value::Float(v) => {
                                running_float_sum += v;
                                running_saw_float = true;
                            }
                            _ => {}
                        }
                    }
                    if running_saw_float {
                        Value::Float(running_float_sum + running_int_sum as f64)
                    } else {
                        Value::Int(running_int_sum)
                    }
                }
                WindowFunc::Avg => {
                    if let Some(value) = current_arg() {
                        match value {
                            Value::Int(v) => {
                                running_float_sum += v as f64;
                                running_count += 1;
                            }
                            Value::Float(v) => {
                                running_float_sum += v;
                                running_count += 1;
                            }
                            _ => {}
                        }
                    }
                    if running_count == 0 {
                        Value::Empty
                    } else {
                        Value::Float(running_float_sum / running_count as f64)
                    }
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
            let final_v = win_values[indices[n - 1]].clone();
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
pub(super) fn exec_group_by(
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    keys: &[GroupKey],
    aggregates: &[GroupAgg],
    having: &Option<Expr>,
) -> Result<QueryResult, QueryError> {
    exec_group_by_internal(columns, rows, None, keys, aggregates, having)
}

pub(super) fn exec_group_by_with_provenance(
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
pub(super) fn predicate_column_indices_json(expr: &Expr, columns: &[String]) -> Vec<usize> {
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
pub(super) fn validate_json_path_types(
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

pub(super) fn validate_no_stray_aggregates(plan: &PlanNode) -> Result<(), QueryError> {
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

/// Evaluate a scalar aggregate over already materialized rows. Stored-field
/// aggregates retain their raw-column fast path in the caller; this generic
/// path is also able to aggregate arbitrary expressions such as JSON paths.
pub(super) fn aggregate_rows(
    func: AggFunc,
    argument: Option<&Expr>,
    columns: &[String],
    rows: &[Vec<Value>],
) -> Result<QueryResult, QueryError> {
    let mut cancel = CancelCheck::new();
    if func == AggFunc::Count && argument.is_none() {
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
            let mut sum = 0.0;
            let mut count = 0_u64;
            for value in values {
                match value {
                    Value::Int(v) => {
                        sum += v as f64;
                        count += 1;
                    }
                    Value::Float(v) => {
                        sum += v;
                        count += 1;
                    }
                    _ => {}
                }
            }
            if count == 0 {
                Value::Empty
            } else {
                Value::Float(sum / count as f64)
            }
        }
        AggFunc::Sum => {
            let mut int_sum = 0_i64;
            let mut float_sum = 0.0;
            let mut saw_float = false;
            for value in values {
                match value {
                    Value::Int(v) => int_sum += v,
                    Value::Float(v) => {
                        float_sum += v;
                        saw_float = true;
                    }
                    _ => {}
                }
            }
            if saw_float {
                Value::Float(float_sum + int_sum as f64)
            } else {
                Value::Int(int_sum)
            }
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

pub(super) fn aggregate_rows_with_provenance(
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
    let mut seen = HashSet::new();
    let mut int_sum = 0_i64;
    let mut float_sum = 0.0_f64;
    let mut saw_float = false;
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
            AggFunc::Sum | AggFunc::Avg => match value {
                Value::Int(value) => {
                    int_sum += value;
                    count += 1;
                }
                Value::Float(value) => {
                    float_sum += value;
                    saw_float = true;
                    count += 1;
                }
                _ => {}
            },
            AggFunc::CountDistinct | AggFunc::Min | AggFunc::Max => unreachable!(),
        }
    }
    let value = match func {
        AggFunc::Count => Value::Int(count as i64),
        AggFunc::Sum if saw_float => Value::Float(float_sum + int_sum as f64),
        AggFunc::Sum => Value::Int(int_sum),
        AggFunc::Avg if count == 0 => Value::Empty,
        AggFunc::Avg => Value::Float((float_sum + int_sum as f64) / count as f64),
        AggFunc::CountDistinct | AggFunc::Min | AggFunc::Max => unreachable!(),
    };
    Ok(QueryResult::Scalar(value))
}

/// Mission E2b: compute one aggregate over a set of rows in a group.
pub(super) struct GroupAggregateContext<'a> {
    pub(super) columns: &'a [String],
    pub(super) all_rows: &'a [Vec<Value>],
    pub(super) row_indices: &'a [usize],
    pub(super) source_index: Option<usize>,
    pub(super) provenance: Option<(&'a [Vec<Option<RowId>>], usize)>,
}

pub(super) fn compute_group_aggregate(
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
        AggFunc::Sum => {
            // Mirror the scalar Sum path: accumulate int and float
            // contributions separately and promote the final result to
            // Float if any Float row was observed. Prevents silent
            // drop of Float columns in GROUP BY aggregates.
            let mut int_sum: i64 = 0;
            let mut float_sum: f64 = 0.0;
            let mut saw_float = false;
            for &ri in row_indices {
                cancel.tick()?;
                let value = value_at(ri);
                if value.is_empty()
                    || !accept_symmetric_contribution(ri, source_index, provenance, &mut seen_rids)?
                {
                    continue;
                }
                match value {
                    Value::Int(v) => int_sum += v,
                    Value::Float(v) => {
                        float_sum += v;
                        saw_float = true;
                    }
                    _ => {}
                }
            }
            if saw_float {
                Ok(Value::Float(float_sum + int_sum as f64))
            } else {
                Ok(Value::Int(int_sum))
            }
        }
        AggFunc::Avg => {
            let mut sum = 0.0f64;
            let mut count = 0usize;
            for &ri in row_indices {
                cancel.tick()?;
                let value = value_at(ri);
                if value.is_empty()
                    || !accept_symmetric_contribution(ri, source_index, provenance, &mut seen_rids)?
                {
                    continue;
                }
                match value {
                    Value::Int(v) => {
                        sum += v as f64;
                        count += 1;
                    }
                    Value::Float(v) => {
                        sum += v;
                        count += 1;
                    }
                    _ => {}
                }
            }
            if count == 0 {
                Ok(Value::Empty)
            } else {
                Ok(Value::Float(sum / count as f64))
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

fn flatten_conjunctions<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
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
        Expr::Field(name) => columns.iter().position(|c| c == name),
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
                        .all(|residual| eval_predicate(residual, &combined, &columns))
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
pub(super) fn check_nested_loop_pair_limit(
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
pub(super) fn execute_materialized_join(
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
                    on.is_none_or(|pred| eval_predicate(pred, &combined, &columns))
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

fn execute_provenance_join(
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
                                .all(|residual| eval_predicate(residual, &row, &columns))
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
                    on.is_none_or(|predicate| eval_predicate(predicate, &row, &columns))
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

/// Whether an index probe literal targets the empty / missing / JSON-null
/// sentinel (`Value::Empty`), whose rows live in the tree's separate empty list.
fn probes_empty_sentinel(key: &Expr) -> bool {
    matches!(literal_to_value(key), Ok(Value::Empty))
}

/// Average rows a non-unique equality probe returns: the empty-list length when
/// probing the missing sentinel, otherwise total entries divided by distinct
/// keys (average rows per key). O(1) over already-loaded counters.
fn estimate_eq_rows(stats: &IndexStats, empty_probe: bool) -> u64 {
    if empty_probe {
        stats.empty_count
    } else {
        stats.total_entries / stats.distinct_keys.max(1)
    }
}

/// Estimated rows an equality candidate's index probe returns, used to rank
/// conjunction drivers by selectivity. A unique probe returns at most one row
/// (`tier == 0`, no stats read); a non-unique probe reads the per-index stats.
fn eq_candidate_est(catalog: &Catalog, scan: &PlanNode, tier: u8) -> u64 {
    if tier == 0 {
        return 1;
    }
    let (stats, key) = match scan {
        PlanNode::IndexScan { table, column, key } => (catalog.index_stats(table, column), key),
        PlanNode::ExprIndexScan { table, path, key } => (
            resolve_expression_index(catalog, table, path)
                .and_then(|meta| catalog.expression_index_stats(table, meta.index_id)),
            key,
        ),
        _ => return UNKNOWN_EST,
    };
    stats.map_or(UNKNOWN_EST, |stats| {
        estimate_eq_rows(&stats, probes_empty_sentinel(key))
    })
}

/// Estimated rows a range candidate scans: its index's total entries (range
/// selectivity estimation is out of scope for v0.15). Any equality candidate,
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
/// Selection ranks candidates by `(estimated rows, tier, build order)`, reading
/// coarse per-index stats (O(1) counter fields): a unique equality estimates 1,
/// a non-unique equality estimates average rows per key, and a range estimates
/// its index's full size so an equality still wins. Ties fall back to v0.14's
/// tier order (equality before range) then conjunct order. A wrong pick is only
/// ever slower, never wrong: the residual re-checks the full conjunction on
/// each fetched row.
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

    // Lowest estimated rows wins; ties fall back to tier then build order, and
    // `min_by_key` keeps the first element on a full tie, which is the
    // earliest-built candidate (equalities in conjunct order, then ranges).
    let winner = candidates
        .into_iter()
        .enumerate()
        .min_by_key(|(build_order, candidate)| (candidate.est, candidate.tier, *build_order))?
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
pub(super) fn lower_unindexed_scans(catalog: &Catalog, plan: &PlanNode) -> PlanNode {
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
                    return plan.clone();
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

fn stored_json_path_expr(path: &powdb_storage::stored_json_path::StoredJsonPathV1) -> Expr {
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

fn synthesize_expr_range_predicate(
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
pub(super) fn synthesize_range_predicate(
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
fn scan_table(scan: &PlanNode) -> Option<&str> {
    match scan {
        PlanNode::IndexScan { table, .. }
        | PlanNode::RangeScan { table, .. }
        | PlanNode::ExprIndexScan { table, .. }
        | PlanNode::ExprRangeScan { table, .. } => Some(table),
        _ => None,
    }
}

pub(super) fn range_matches(
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
pub(super) fn format_plan_tree(catalog: &Catalog, plan: &PlanNode, depth: usize) -> String {
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
                    let est = if catalog.is_index_unique(table, column) == Some(true) {
                        1
                    } else {
                        estimate_eq_rows(&stats, probes_empty_sentinel(key))
                    };
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
                    .map(|stats| (m.unique, stats))
            }) {
                Some((unique, stats)) => {
                    let est = if unique {
                        1
                    } else {
                        estimate_eq_rows(&stats, probes_empty_sentinel(key))
                    };
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
