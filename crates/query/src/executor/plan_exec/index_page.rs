//! Bounded ordered pages over a unique integer index.

use std::ops::{Bound, ControlFlow};

use crate::executor::eval::literal_to_value;
use crate::executor::Engine;
use crate::result::{QueryError, QueryResult};

use super::*;

impl Engine {
    /// Recognize only a directly bounded ascending index order. Residual
    /// predicates, offsets, computed projections and other orders keep their
    /// existing execution paths. Both admission surfaces call this helper.
    pub(crate) fn try_bounded_index_page(
        &self,
        plan: &PlanNode,
    ) -> Result<Option<QueryResult>, QueryError> {
        let (plan, fields) = match plan {
            PlanNode::Project { input, fields } => (input.as_ref(), Some(fields.as_slice())),
            _ => (plan, None),
        };
        let PlanNode::Limit { input, count } = plan else {
            return Ok(None);
        };
        let Some(limit) = literal_limit(count) else {
            return Ok(None);
        };
        let PlanNode::Sort { input, keys } = input.as_ref() else {
            return Ok(None);
        };
        let [SortKey {
            expr: Expr::Field(sort_column),
            descending: false,
        }] = keys.as_slice()
        else {
            return Ok(None);
        };
        let (table, start, end) = match input.as_ref() {
            PlanNode::RangeScan {
                table,
                column,
                start,
                end,
            } if column == sort_column => (table, start.as_ref(), end.as_ref()),
            PlanNode::SeqScan { table } => (table, None, None),
            _ => return Ok(None),
        };
        if self.generic_path_forced("bounded-index-page") || self.view_registry.is_dirty(table) {
            return Ok(None);
        }
        let Some(tbl) = self.catalog.get_table(table) else {
            return Ok(None);
        };
        let schema = tbl.schema();
        let Some(sort_index) = schema.column_index(sort_column) else {
            return Ok(None);
        };
        // Unique indexes omit nulls. A bounded numeric predicate excludes
        // them too; an unfiltered order may only omit them if required.
        if schema.columns[sort_index].type_id != TypeId::Int
            || (start.is_none() && end.is_none() && !schema.columns[sort_index].required)
            || tbl.is_index_unique(sort_column) != Some(true)
        {
            return Ok(None);
        }
        let Some(index) = tbl.index(sort_column) else {
            return Ok(None);
        };
        let start_value = start.map(|(expr, _)| literal_to_value(expr)).transpose()?;
        let end_value = end.map(|(expr, _)| literal_to_value(expr)).transpose()?;
        if start_value
            .iter()
            .chain(end_value.iter())
            .any(|value| !matches!(value, Value::Int(_)))
        {
            return Ok(None);
        }
        let projection = match fields {
            Some(fields) => {
                let indices = fields
                    .iter()
                    .map(|field| match &field.expr {
                        Expr::Field(name) => schema.column_index(name),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>();
                // Keep the generic evaluator responsible for expressions.
                match indices {
                    Some(indices) => Some(indices),
                    None => return Ok(None),
                }
            }
            None => None,
        };
        let columns: Vec<String> = match fields {
            Some(fields) => fields
                .iter()
                .map(|field| {
                    field
                        .alias
                        .clone()
                        .unwrap_or_else(|| projection_output_name(&field.expr))
                })
                .collect(),
            None => schema.columns.iter().map(|col| col.name.clone()).collect(),
        };
        let mut rows = Vec::new();
        if limit == 0 {
            return Ok(Some(QueryResult::Rows { columns, rows }));
        }
        fn bound(value: Option<&Value>, inclusive: bool) -> Bound<&Value> {
            match (value, inclusive) {
                (Some(value), true) => Bound::Included(value),
                (Some(value), false) => Bound::Excluded(value),
                (None, _) => Bound::Unbounded,
            }
        }
        let start_bound = bound(start_value.as_ref(), start.is_none_or(|(_, inc)| *inc));
        let end_bound = bound(end_value.as_ref(), end.is_none_or(|(_, inc)| *inc));
        index.try_for_each_range(start_bound, end_bound, crate::cancel::check, |_key, rid| {
            #[cfg(feature = "testing")]
            {
                crate::executor::work_stats::record_index_entry();
                crate::executor::work_stats::record_heap_fetch();
            }
            // Table::get reconstructs overflow values; raw heap decoding
            // is not interchangeable with this fetch.
            if let Some(full) = tbl.get(rid).map_err(QueryError::from_storage_io)? {
                mem_budget::charge(
                    mem_budget::estimate_row_size(&full),
                    self.query_memory_limit(),
                )?;
                let row = match &projection {
                    Some(indices) => {
                        let projected: Vec<Value> =
                            indices.iter().map(|&idx| full[idx].clone()).collect();
                        mem_budget::charge(
                            mem_budget::estimate_row_size(&projected),
                            self.query_memory_limit(),
                        )?;
                        projected
                    }
                    None => full,
                };
                rows.push(row);
            }
            Ok(if rows.len() == limit {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            })
        })?;
        Ok(Some(QueryResult::Rows { columns, rows }))
    }
}
