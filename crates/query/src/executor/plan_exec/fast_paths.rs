//! Specialized read fast paths: single-column aggregate, project+filter+limit,
//! and project+filter+sort+limit over raw row bytes.

use crate::cancel::CancelCheck;
use crate::result::{QueryError, QueryResult};
use powdb_storage::error::StorageError;
use powdb_storage::row::{decode_column, raw_stub, OverflowStub, RowLayout};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ops::ControlFlow;

use crate::executor::row_body_base;
use crate::executor::Engine;

use super::aggregate::{agg_overflow_error, CompensatedSum};
use super::*;

/// Bounded top-N heap for a descending sort, keyed by `(sort key, scan seq)`.
///
/// `Reverse` on the whole tuple makes `pop()` evict the smallest key, which is
/// what a bounded max-N heap needs. The inner `Reverse` on the sequence number
/// is what makes ties correct: the final order is key-descending then
/// seq-*ascending*, so among rows tied on the boundary key the row that must be
/// evicted is the one with the *largest* seq, not the smallest.
type DescTopN<K> = BinaryHeap<Reverse<(K, Reverse<u64>, Vec<u8>)>>;

impl Engine {
    // ─── Specialized fast paths ─────────────────────────────────────────────
    //
    // These methods are helpers for the `execute_plan` match arms above.
    // Each returns `Ok(Some(result))` when the fast path fires, `Ok(None)`
    // when the shape isn't supported (caller falls back to generic code).

    /// Aggregate sum/avg/min/max over a single fixed-size numeric column, with
    /// an optional compiled filter predicate. Walks raw row bytes. Accumulates
    /// sum/avg in i128; a `sum` total that does not fit back into i64 is a
    /// typed error, not a clamped number. An overflow table reads that column
    /// with [`decode_column`] and does not fetch any other column.
    pub(crate) fn agg_single_col_fast(
        &self,
        table: &str,
        col: &str,
        function: AggFunc,
        predicate: Option<&Expr>,
    ) -> Result<Option<QueryResult>, QueryError> {
        if self.generic_path_forced("agg-single-col") {
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
            Some(pred) => match self.compile_predicate_unless_forced(
                "agg-single-col:predicate",
                pred,
                &columns,
                &fast,
                &schema,
            ) {
                Some(c) => Some(c),
                None => return Ok(None), // let generic path handle it
            },
            None => None,
        };

        // The v1 loops below use a byte offset that an overflow bitmap shifts,
        // and they `return` on a short slice, which drops the row. A numeric
        // column cannot spill. A predicate on a variable-length column stays
        // generic: the compiled v2 fallback reports a stub as Empty. A
        // predicate that does not compile already returned Ok(None) above.
        if self.catalog.table_has_overflow(table) {
            if predicate.is_some_and(|pred| expr_forces_generic_on_overflow(pred, &schema)) {
                return Ok(None);
            }
            return self
                .on_disk_numeric_agg(
                    table,
                    &schema,
                    col_idx,
                    col_type,
                    function,
                    compiled_pred.as_ref(),
                )
                .map(Some);
        }

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
                AggFunc::Sum => {
                    // SUM needs only presence, not AVG's per-row count. Keep
                    // the wide total until finalization so cancellation of
                    // intermediate i64 overflow remains valid.
                    let mut sum_i128: i128 = 0;
                    let mut seen = false;
                    agg_int_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: i64| {
                            seen = true;
                            sum_i128 += v as i128;
                        }
                    );
                    QueryResult::Scalar(if seen {
                        Value::Int(i64::try_from(sum_i128).map_err(|_| agg_overflow_error("sum"))?)
                    } else {
                        Value::Empty
                    })
                }
                AggFunc::Avg => {
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
                    if count == 0 {
                        QueryResult::Scalar(Value::Empty)
                    } else {
                        let avg = (sum_i128 as f64) / (count as f64);
                        QueryResult::Scalar(Value::Float(avg))
                    }
                }
                AggFunc::Min => {
                    // Presence is independent of the sentinel: i64::MAX is
                    // itself a valid non-null minimum.
                    let mut min_v = i64::MAX;
                    let mut seen = false;
                    agg_int_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: i64| {
                            seen = true;
                            min_v = min_v.min(v);
                        }
                    );
                    QueryResult::Scalar(if seen {
                        Value::Int(min_v)
                    } else {
                        Value::Empty
                    })
                }
                AggFunc::Max => {
                    let mut max_v = i64::MIN;
                    let mut seen = false;
                    agg_int_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: i64| {
                            seen = true;
                            max_v = max_v.max(v);
                        }
                    );
                    QueryResult::Scalar(if seen {
                        Value::Int(max_v)
                    } else {
                        Value::Empty
                    })
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
                    // Same compensated accumulator as the generic path: a
                    // float total must not depend on which path read the rows.
                    let mut sum = CompensatedSum::default();
                    let mut count: i64 = 0;
                    agg_float_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: f64| {
                            sum.add(v);
                            count += 1;
                        }
                    );
                    if count == 0 {
                        // Empty (SQL NULL), matching avg and NumericAgg::sum.
                        // Before this check the float loop answered 0.0 while
                        // the generic path answered Int(0) for the same
                        // question.
                        QueryResult::Scalar(Value::Empty)
                    } else {
                        QueryResult::Scalar(Value::Float(sum.total()))
                    }
                }
                AggFunc::Avg => {
                    let mut sum = CompensatedSum::default();
                    let mut count: i64 = 0;
                    agg_float_loop!(
                        self,
                        table,
                        compiled_pred,
                        bitmap_byte,
                        bitmap_bit,
                        body_data_offset,
                        |v: f64| {
                            sum.add(v);
                            count += 1;
                        }
                    );
                    if count == 0 {
                        QueryResult::Scalar(Value::Empty)
                    } else {
                        QueryResult::Scalar(Value::Float(sum.total() / count as f64))
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

    /// Sum/avg/min/max/count over one numeric column of a spilled table.
    ///
    /// Walks raw slots. [`decode_column`] reads the aggregated column.
    /// `Value::Empty` is null: the value is skipped and the row is not.
    /// Does not call `read_overflow_value`.
    fn on_disk_numeric_agg(
        &self,
        table: &str,
        schema: &Schema,
        col_idx: usize,
        col_type: TypeId,
        function: AggFunc,
        pred: Option<&CompiledPredicate>,
    ) -> Result<QueryResult, QueryError> {
        match col_type {
            TypeId::Int => self.on_disk_int_agg(table, schema, col_idx, function, pred),
            TypeId::Float => self.on_disk_float_agg(table, schema, col_idx, function, pred),
            _ => unreachable!("type guard above restricts to Int/Float"),
        }
    }

    fn on_disk_int_agg(
        &self,
        table: &str,
        schema: &Schema,
        col_idx: usize,
        function: AggFunc,
        pred: Option<&CompiledPredicate>,
    ) -> Result<QueryResult, QueryError> {
        match function {
            AggFunc::Sum => {
                let mut sum_i128: i128 = 0;
                let mut seen = false;
                self.fold_on_disk_numeric(table, schema, col_idx, pred, |value| {
                    seen = true;
                    sum_i128 += int_column(value) as i128;
                })?;
                Ok(QueryResult::Scalar(if seen {
                    Value::Int(i64::try_from(sum_i128).map_err(|_| agg_overflow_error("sum"))?)
                } else {
                    Value::Empty
                }))
            }
            AggFunc::Avg => {
                let mut sum_i128: i128 = 0;
                let mut count: i64 = 0;
                self.fold_on_disk_numeric(table, schema, col_idx, pred, |value| {
                    count += 1;
                    sum_i128 += int_column(value) as i128;
                })?;
                if count == 0 {
                    Ok(QueryResult::Scalar(Value::Empty))
                } else {
                    let avg = (sum_i128 as f64) / (count as f64);
                    Ok(QueryResult::Scalar(Value::Float(avg)))
                }
            }
            AggFunc::Min => {
                // Presence is independent of the sentinel: i64::MAX is itself
                // a valid non-null minimum.
                let mut min_v = i64::MAX;
                let mut seen = false;
                self.fold_on_disk_numeric(table, schema, col_idx, pred, |value| {
                    seen = true;
                    min_v = min_v.min(int_column(value));
                })?;
                Ok(QueryResult::Scalar(if seen {
                    Value::Int(min_v)
                } else {
                    Value::Empty
                }))
            }
            AggFunc::Max => {
                let mut max_v = i64::MIN;
                let mut seen = false;
                self.fold_on_disk_numeric(table, schema, col_idx, pred, |value| {
                    seen = true;
                    max_v = max_v.max(int_column(value));
                })?;
                Ok(QueryResult::Scalar(if seen {
                    Value::Int(max_v)
                } else {
                    Value::Empty
                }))
            }
            AggFunc::Count => {
                let mut count: i64 = 0;
                self.fold_on_disk_numeric(table, schema, col_idx, pred, |_value| {
                    count += 1;
                })?;
                Ok(QueryResult::Scalar(Value::Int(count)))
            }
            AggFunc::CountDistinct => {
                let mut seen = rustc_hash::FxHashSet::default();
                self.fold_on_disk_numeric(table, schema, col_idx, pred, |value| {
                    seen.insert(int_column(value));
                })?;
                Ok(QueryResult::Scalar(Value::Int(seen.len() as i64)))
            }
        }
    }

    fn on_disk_float_agg(
        &self,
        table: &str,
        schema: &Schema,
        col_idx: usize,
        function: AggFunc,
        pred: Option<&CompiledPredicate>,
    ) -> Result<QueryResult, QueryError> {
        match function {
            AggFunc::Sum => {
                let mut sum = CompensatedSum::default();
                let mut count: i64 = 0;
                self.fold_on_disk_numeric(table, schema, col_idx, pred, |value| {
                    sum.add(float_column(value));
                    count += 1;
                })?;
                if count == 0 {
                    Ok(QueryResult::Scalar(Value::Empty))
                } else {
                    Ok(QueryResult::Scalar(Value::Float(sum.total())))
                }
            }
            AggFunc::Avg => {
                let mut sum = CompensatedSum::default();
                let mut count: i64 = 0;
                self.fold_on_disk_numeric(table, schema, col_idx, pred, |value| {
                    sum.add(float_column(value));
                    count += 1;
                })?;
                if count == 0 {
                    Ok(QueryResult::Scalar(Value::Empty))
                } else {
                    Ok(QueryResult::Scalar(Value::Float(
                        sum.total() / count as f64,
                    )))
                }
            }
            AggFunc::Min => {
                let mut min_v: Option<f64> = None;
                self.fold_on_disk_numeric(table, schema, col_idx, pred, |value| {
                    let v = float_column(value);
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
                })?;
                Ok(QueryResult::Scalar(
                    min_v.map(Value::Float).unwrap_or(Value::Empty),
                ))
            }
            AggFunc::Max => {
                let mut max_v: Option<f64> = None;
                self.fold_on_disk_numeric(table, schema, col_idx, pred, |value| {
                    let v = float_column(value);
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
                })?;
                Ok(QueryResult::Scalar(
                    max_v.map(Value::Float).unwrap_or(Value::Empty),
                ))
            }
            AggFunc::Count => {
                let mut count: i64 = 0;
                self.fold_on_disk_numeric(table, schema, col_idx, pred, |_value| {
                    count += 1;
                })?;
                Ok(QueryResult::Scalar(Value::Int(count)))
            }
            AggFunc::CountDistinct => {
                let mut seen = rustc_hash::FxHashSet::default();
                self.fold_on_disk_numeric(table, schema, col_idx, pred, |value| {
                    seen.insert(float_column(value).to_bits());
                })?;
                Ok(QueryResult::Scalar(Value::Int(seen.len() as i64)))
            }
        }
    }

    /// Non-null values of one column. `Value::Empty` is skipped. The row is
    /// still visited: a fired cancel token is an error, not a partial fold.
    fn fold_on_disk_numeric(
        &self,
        table: &str,
        schema: &Schema,
        col_idx: usize,
        pred: Option<&CompiledPredicate>,
        mut on_value: impl FnMut(&Value),
    ) -> Result<(), QueryError> {
        let layout = RowLayout::new(schema);
        self.for_each_on_disk_slot(table, |data| {
            if let Some(pred) = pred {
                if !pred(data) {
                    return;
                }
            }
            let value = decode_column(schema, &layout, data, col_idx);
            if !value.is_empty() {
                on_value(&value);
            }
        })
    }

    /// Every live slot, raw on-disk bytes. No rehydrate and no skip.
    ///
    /// Same cancel split as [`for_each_row_raw_cancellable`]: no per-row poll
    /// unless a token is installed, and a fired token is
    /// [`QueryError::Cancelled`].
    fn for_each_on_disk_slot(
        &self,
        table: &str,
        mut f: impl FnMut(&[u8]),
    ) -> Result<(), QueryError> {
        let mut cancel = CancelCheck::new();
        let mut cancel_err: Option<QueryError> = None;
        let poll = crate::cancel::has_active_install();
        self.catalog
            .try_for_each_on_disk_row(table, |_rid, slot| {
                if poll {
                    if let Err(err) = cancel.tick() {
                        cancel_err = Some(err);
                        return ControlFlow::Break(());
                    }
                }
                f(slot.bytes());
                ControlFlow::Continue(())
            })
            .map_err(QueryError::from_storage_io)?;
        match cancel_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// `count(T)` over every live slot of a spilled table.
    pub(crate) fn count_on_disk_slots(
        &self,
        table: &str,
        pred: Option<&CompiledPredicate>,
    ) -> Result<i64, QueryError> {
        let mut count: i64 = 0;
        self.for_each_on_disk_slot(table, |data| {
            if pred.is_none_or(|pred| pred(data)) {
                count += 1;
            }
        })?;
        Ok(count)
    }

    /// Filtered `count(T)` on a spilled table.
    ///
    /// `Ok(None)` when the predicate names a variable-length column or does
    /// not compile. Those stay generic. A compiled v2 fallback would see a
    /// spilled value as Empty, and `decode_selective` on raw v2 bytes would
    /// too.
    pub(crate) fn count_filtered_on_disk(
        &self,
        table: &str,
        predicate: &Expr,
        site: &'static str,
    ) -> Result<Option<i64>, QueryError> {
        let schema = self
            .catalog
            .schema(table)
            .ok_or_else(|| QueryError::TableNotFound(table.to_string()))?
            .clone();
        if expr_forces_generic_on_overflow(predicate, &schema) {
            return Ok(None);
        }
        let columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
        let fast = FastLayout::new(&schema);
        let Some(compiled) =
            self.compile_predicate_unless_forced(site, predicate, &columns, &fast, &schema)
        else {
            return Ok(None);
        };
        self.count_on_disk_slots(table, Some(&compiled)).map(Some)
    }

    /// `Project(Limit(Filter(SeqScan)))`, `Project(Limit(SeqScan))`,
    /// `Project(Filter(SeqScan))`, and `Project(SeqScan)`.
    ///
    /// Streams raw on-disk bytes, decodes only projected columns, and stops
    /// at the limit. A spilled column is fetched only when it is projected.
    /// A corrupt chain on a projected column is an error, not an empty cell.
    /// A predicate that names a variable-length column still takes the
    /// generic path: the compiled predicate's v2 fallback cannot see the heap.
    pub(crate) fn project_filter_limit_fast(
        &self,
        table: &str,
        fields: &[ProjectField],
        limit: usize,
        predicate: Option<&Expr>,
    ) -> Result<Option<QueryResult>, QueryError> {
        if self.generic_path_forced("project-filter-limit") {
            return Ok(None);
        }
        if limit == 0 {
            // The scan loop below pushes a row before testing the limit, so
            // `limit 0` would emit one row. Degenerate case: let the generic
            // path produce the empty result with proper column naming, as the
            // sort-limit fast path already does.
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
            Some(pred) => match self.compile_predicate_unless_forced(
                "project-filter-limit:predicate",
                pred,
                &all_columns,
                &fast,
                &schema,
            ) {
                Some(c) => Some(c),
                None => return Ok(None),
            },
            None => None,
        };

        // Fixed types never spill, so a predicate on them is correct against
        // raw v2 bytes (the compiled leaf's v2 fallback uses `decode_column`).
        // A variable-length column may be a stub, and that fallback reports
        // the stub as Empty. Unknown names have the same problem: bail.
        if self.catalog.table_has_overflow(table) {
            if let Some(pred) = predicate {
                if expr_forces_generic_on_overflow(pred, &schema) {
                    return Ok(None);
                }
            }
        }

        let mut out: Vec<Vec<Value>> = Vec::with_capacity(limit.min(1024));
        // Mission D2: stop iterating once the limit is reached. The previous
        // `done` flag only short-circuited the closure body, so a `limit 100`
        // over 100K rows still walked all 100K slots.
        // Cooperative cancellation: an unbounded (limit == usize::MAX) projected
        // scan over a huge table must stay stoppable.
        let mut cancel = CancelCheck::new();
        let mut cancel_err: Option<QueryError> = None;
        let mut overflow_err: Option<QueryError> = None;
        self.catalog
            .try_for_each_on_disk_row(table, |_rid, slot| {
                if let Err(e) = cancel.tick() {
                    cancel_err = Some(e);
                    return ControlFlow::Break(());
                }
                let data = slot.bytes();
                if let Some(ref pred) = compiled_pred {
                    if !pred(data) {
                        return ControlFlow::Continue(());
                    }
                }
                let mut row = Vec::with_capacity(proj_indices.len());
                for &column in &proj_indices {
                    match decode_projected_column(&schema, &row_layout, data, column, |stub| {
                        slot.read_overflow(stub)
                    }) {
                        Ok(value) => row.push(value),
                        Err(error) => {
                            overflow_err = Some(error);
                            return ControlFlow::Break(());
                        }
                    }
                }
                out.push(row);
                if out.len() >= limit {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .map_err(QueryError::from_storage_io)?;
        if let Some(error) = overflow_err {
            return Err(error);
        }
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
    pub(crate) fn project_filter_sort_limit_fast(
        &self,
        table: &str,
        fields: &[ProjectField],
        sort_field: &str,
        descending: bool,
        limit: usize,
        predicate: Option<&Expr>,
    ) -> Result<Option<QueryResult>, QueryError> {
        if self.generic_path_forced("project-filter-sort-limit") {
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
            Some(pred) => match self.compile_predicate_unless_forced(
                "project-filter-sort-limit:predicate",
                pred,
                &all_columns,
                &fast,
                &schema,
            ) {
                Some(c) => Some(c),
                None => return Ok(None),
            },
            None => None,
        };

        // v1 sort-key offsets below are wrong once an overflow bitmap is
        // present, and a short slice would drop the row. Variable-length
        // predicates stay generic: the compiled v2 fallback treats a stub
        // as Empty.
        if self.catalog.table_has_overflow(table) {
            if let Some(pred) = predicate {
                if expr_forces_generic_on_overflow(pred, &schema) {
                    return Ok(None);
                }
            }
            return self.project_filter_sort_limit_overflow(OverflowSort {
                table,
                schema: &schema,
                row_layout: &row_layout,
                proj_indices: &proj_indices,
                proj_columns,
                sort_idx,
                descending,
                limit,
                prealloc,
                compiled_pred,
            });
        }

        // Bounded top-N heap. For `order .x desc limit N`, we want the N
        // largest values — use a min-heap so the smallest is at the top and
        // can be popped when a better candidate arrives. For ascending, use
        // a max-heap. We tie-break with a monotonic `seq` counter so the
        // result is deterministic and stable.
        //
        // The heap's top must be the row this query would rank *last*, because
        // that is the one eviction removes. The emitted order is key-directional
        // then `seq` **ascending** in both directions (see the drain sort below),
        // so among rows tied on the boundary key the last-ranked row is the one
        // with the largest `seq`. The ascending heap gets that for free: its top
        // is the maximum `(key, seq)`. The descending heap is `Reverse`d, so its
        // top is the minimum `(key, seq)`, the *earliest* tied row, exactly the
        // one that must be kept. Wrapping `seq` in a second `Reverse` flips the
        // tie-break back, so the top is `(smallest key, largest seq)`. Without
        // it, `order .k desc limit N` returned a different row *set* from the
        // first N rows of `order .k desc`.
        //
        // To keep this simple we maintain two typed heaps and pick by
        // direction.
        let drained: Vec<Vec<u8>> = match sort_col_type {
            TypeId::Int => {
                let mut seq: u64 = 0;
                let mut heap_desc: DescTopN<i64> = BinaryHeap::with_capacity(prealloc);
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
                            heap_desc.push(Reverse((key, Reverse(id), data.to_vec())));
                        } else if let Some(Reverse((top_key, _, _))) = heap_desc.peek() {
                            if key > *top_key {
                                heap_desc.pop();
                                heap_desc.push(Reverse((key, Reverse(id), data.to_vec())));
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
                    heap_desc
                        .into_iter()
                        .map(|Reverse((key, Reverse(id), data))| (key, id, data))
                        .collect()
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
                let mut heap_desc: DescTopN<u64> = BinaryHeap::with_capacity(prealloc);
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
                            heap_desc.push(Reverse((key, Reverse(id), data.to_vec())));
                        } else if let Some(Reverse((top_key, _, _))) = heap_desc.peek() {
                            if key > *top_key {
                                heap_desc.pop();
                                heap_desc.push(Reverse((key, Reverse(id), data.to_vec())));
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
                    heap_desc
                        .into_iter()
                        .map(|Reverse((key, Reverse(id), data))| (key, id, data))
                        .collect()
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

    /// Top-N over a table that has spilled.
    ///
    /// Ranks raw on-disk bytes with [`decode_column`] and fetches an overflow
    /// chain only for a projected column of a winning row. An unprojected
    /// chain is never read, so a corrupt one does not drop the row or fail
    /// the query. A corrupt chain on a projected winner is an error, not an
    /// empty cell.
    fn project_filter_sort_limit_overflow(
        &self,
        spec: OverflowSort<'_>,
    ) -> Result<Option<QueryResult>, QueryError> {
        let drained = match spec.schema.columns[spec.sort_idx].type_id {
            TypeId::Int => self.collect_keyed_topn(&spec, |value| match value {
                Value::Empty => None,
                Value::Int(key) => Some(*key),
                _ => unreachable!("int sort key"),
            })?,
            TypeId::Float => self.collect_keyed_topn(&spec, |value| match value {
                Value::Empty => None,
                Value::Float(key) => Some(f64_bits_to_sortable_u64(key.to_bits())),
                _ => unreachable!("float sort key"),
            })?,
            _ => unreachable!("type guard above restricts to Int/Float"),
        };

        let table = self
            .catalog
            .get_table(spec.table)
            .ok_or_else(|| QueryError::TableNotFound(spec.table.to_string()))?;
        let heap = &table.heap;
        let mut cancel = CancelCheck::new();
        let mut rows = Vec::with_capacity(drained.len());
        for data in &drained {
            cancel.tick()?;
            let mut row = Vec::with_capacity(spec.proj_indices.len());
            for &column in spec.proj_indices {
                row.push(decode_projected_column(
                    spec.schema,
                    spec.row_layout,
                    data,
                    column,
                    |stub| heap.read_overflow_value(stub),
                )?);
            }
            rows.push(row);
        }
        Ok(Some(QueryResult::Rows {
            columns: spec.proj_columns,
            rows,
        }))
    }

    /// One pass over raw slots. `classify` returns `None` for the null bucket
    /// and `Some` for an Int or sortable-float key. No per-row poll unless a
    /// cancellation token is installed. A fired token returns that error
    /// instead of the partial heap. A top-N still visits every row.
    fn collect_keyed_topn<K, F>(
        &self,
        spec: &OverflowSort<'_>,
        mut classify: F,
    ) -> Result<Vec<Vec<u8>>, QueryError>
    where
        K: Ord + Copy,
        F: FnMut(&Value) -> Option<K>,
    {
        let mut seq: u64 = 0;
        let mut heap_desc: DescTopN<K> = BinaryHeap::with_capacity(spec.prealloc);
        let mut heap_asc: BinaryHeap<(K, u64, Vec<u8>)> = BinaryHeap::with_capacity(spec.prealloc);
        let mut null_rows: Vec<Vec<u8>> = Vec::with_capacity(spec.prealloc);
        let mut cancel = CancelCheck::new();
        let mut cancel_err: Option<QueryError> = None;
        let poll = crate::cancel::has_active_install();
        self.catalog
            .try_for_each_on_disk_row(spec.table, |_rid, slot| {
                if poll {
                    if let Err(err) = cancel.tick() {
                        cancel_err = Some(err);
                        return ControlFlow::Break(());
                    }
                }
                let data = slot.bytes();
                if let Some(pred) = spec.compiled_pred.as_ref() {
                    if !pred(data) {
                        return ControlFlow::Continue(());
                    }
                }
                let decoded = decode_column(spec.schema, spec.row_layout, data, spec.sort_idx);
                let id = seq;
                seq += 1;
                match classify(&decoded) {
                    None => {
                        if null_rows.len() < spec.limit {
                            null_rows.push(data.to_vec());
                        }
                    }
                    Some(key) => offer_topn(
                        key,
                        id,
                        data,
                        spec.descending,
                        spec.limit,
                        &mut heap_desc,
                        &mut heap_asc,
                    ),
                }
                ControlFlow::Continue(())
            })
            .map_err(QueryError::from_storage_io)?;
        if let Some(err) = cancel_err {
            return Err(err);
        }
        drain_topn(
            spec.descending,
            spec.limit,
            self.query_memory_limit,
            heap_desc,
            heap_asc,
            null_rows,
        )
    }
}

/// Accepted shape for the spilled top-N reader. The caller already rejected
/// limit 0, a non-field projection, a non-numeric sort key, and a predicate
/// that did not compile.
struct OverflowSort<'a> {
    table: &'a str,
    schema: &'a Schema,
    row_layout: &'a RowLayout,
    proj_indices: &'a [usize],
    proj_columns: Vec<String>,
    sort_idx: usize,
    descending: bool,
    limit: usize,
    prealloc: usize,
    compiled_pred: Option<CompiledPredicate>,
}

fn offer_topn<K: Ord + Copy>(
    key: K,
    id: u64,
    bytes: &[u8],
    descending: bool,
    limit: usize,
    heap_desc: &mut DescTopN<K>,
    heap_asc: &mut BinaryHeap<(K, u64, Vec<u8>)>,
) {
    if descending {
        if heap_desc.len() < limit {
            heap_desc.push(Reverse((key, Reverse(id), bytes.to_vec())));
        } else if let Some(Reverse((top_key, _, _))) = heap_desc.peek() {
            if key > *top_key {
                heap_desc.pop();
                heap_desc.push(Reverse((key, Reverse(id), bytes.to_vec())));
            }
        }
    } else if heap_asc.len() < limit {
        heap_asc.push((key, id, bytes.to_vec()));
    } else if let Some((top_key, _, _)) = heap_asc.peek() {
        if key < *top_key {
            heap_asc.pop();
            heap_asc.push((key, id, bytes.to_vec()));
        }
    }
}

fn drain_topn<K: Ord>(
    descending: bool,
    limit: usize,
    memory_limit: usize,
    heap_desc: DescTopN<K>,
    heap_asc: BinaryHeap<(K, u64, Vec<u8>)>,
    null_rows: Vec<Vec<u8>>,
) -> Result<Vec<Vec<u8>>, QueryError> {
    let mut drained: Vec<(K, u64, Vec<u8>)> = if descending {
        heap_desc
            .into_iter()
            .map(|Reverse((key, Reverse(id), data))| (key, id, data))
            .collect()
    } else {
        heap_asc.into_iter().collect()
    };
    if descending {
        cooperative_stable_sort_by(&mut drained, memory_limit, |a, b| {
            b.0.cmp(&a.0).then(a.1.cmp(&b.1))
        })?;
    } else {
        cooperative_stable_sort_by(&mut drained, memory_limit, |a, b| {
            a.0.cmp(&b.0).then(a.1.cmp(&b.1))
        })?;
    }
    let mut rows: Vec<Vec<u8>> = drained.into_iter().map(|(_, _, data)| data).collect();
    rows.extend(null_rows.into_iter().take(limit.saturating_sub(rows.len())));
    Ok(rows)
}

/// Decode one projected column from raw on-disk bytes.
///
/// A stub is fetched. Anything else uses [`decode_column`], which is correct
/// for fixed columns and inline var columns and returns Empty for a stub the
/// caller did not fetch. A stub on a fixed type is a bug, not a value.
fn decode_projected_column(
    schema: &Schema,
    layout: &RowLayout,
    data: &[u8],
    column: usize,
    read_overflow: impl FnOnce(&OverflowStub) -> Result<Vec<u8>, StorageError>,
) -> Result<Value, QueryError> {
    let Some(stub) = raw_stub(schema, layout, data, column) else {
        return Ok(decode_column(schema, layout, data, column));
    };
    let bytes = read_overflow(&stub)?;
    Ok(match schema.columns[column].type_id {
        TypeId::Str => Value::Str(String::from_utf8_lossy(&bytes).into_owned()),
        TypeId::Bytes => Value::Bytes(bytes),
        TypeId::Json => Value::Json(bytes.into()),
        _ => {
            return Err(QueryError::from(StorageError::CorruptData(
                "overflow stub on a fixed-width column".into(),
            )));
        }
    })
}

/// Whether a predicate on an overflow table must leave the raw fast path.
///
/// True when any field is variable-length ([`TypeId::Str`], [`TypeId::Bytes`],
/// [`TypeId::Json`]) or names a column this schema does not have. Fixed
/// columns never spill. No predicate is not this function's problem.
pub(crate) fn expr_forces_generic_on_overflow(expr: &Expr, schema: &Schema) -> bool {
    match expr {
        Expr::Field(name) => field_forces_generic_on_overflow(schema, name),
        Expr::QualifiedField { field, .. } => field_forces_generic_on_overflow(schema, field),
        Expr::Literal(_) | Expr::Param(_) | Expr::ValueLit(_) | Expr::Null => false,
        Expr::BinaryOp(left, _, right) | Expr::Coalesce(left, right) => {
            expr_forces_generic_on_overflow(left, schema)
                || expr_forces_generic_on_overflow(right, schema)
        }
        Expr::UnaryOp(_, inner) | Expr::Cast(inner, _) | Expr::FunctionCall(_, inner, _) => {
            expr_forces_generic_on_overflow(inner, schema)
        }
        Expr::ScalarFunc(_, args) => args
            .iter()
            .any(|arg| expr_forces_generic_on_overflow(arg, schema)),
        Expr::InList { expr, list, .. } => {
            expr_forces_generic_on_overflow(expr, schema)
                || list
                    .iter()
                    .any(|item| expr_forces_generic_on_overflow(item, schema))
        }
        Expr::InSubquery { expr, subquery, .. } => {
            expr_forces_generic_on_overflow(expr, schema)
                || query_forces_generic_on_overflow(subquery, schema)
        }
        Expr::ExistsSubquery { subquery, .. } => query_forces_generic_on_overflow(subquery, schema),
        Expr::Case { whens, else_expr } => {
            whens.iter().any(|(when, then)| {
                expr_forces_generic_on_overflow(when, schema)
                    || expr_forces_generic_on_overflow(then, schema)
            }) || else_expr
                .as_ref()
                .is_some_and(|expr| expr_forces_generic_on_overflow(expr, schema))
        }
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            args.iter()
                .any(|arg| expr_forces_generic_on_overflow(arg, schema))
                || partition_by
                    .iter()
                    .any(|arg| expr_forces_generic_on_overflow(arg, schema))
                || order_by
                    .iter()
                    .any(|key| expr_forces_generic_on_overflow(&key.expr, schema))
        }
        Expr::NestedQuery(nested) => nested_forces_generic_on_overflow(nested, schema),
        Expr::LinkPath { column, .. } => field_forces_generic_on_overflow(schema, column),
        Expr::JsonPath { base, .. } => expr_forces_generic_on_overflow(base, schema),
    }
}

fn field_forces_generic_on_overflow(schema: &Schema, name: &str) -> bool {
    match schema.column_index(name) {
        Some(index) => matches!(
            schema.columns[index].type_id,
            TypeId::Str | TypeId::Bytes | TypeId::Json
        ),
        None => true,
    }
}

fn query_forces_generic_on_overflow(query: &QueryExpr, schema: &Schema) -> bool {
    query.joins.iter().any(|join| {
        join.on
            .as_ref()
            .is_some_and(|on| expr_forces_generic_on_overflow(on, schema))
    }) || query
        .filter
        .as_ref()
        .is_some_and(|filter| expr_forces_generic_on_overflow(filter, schema))
        || query
            .order
            .as_ref()
            .is_some_and(|order| order_forces_generic_on_overflow(order, schema))
        || query
            .limit
            .as_ref()
            .is_some_and(|limit| expr_forces_generic_on_overflow(limit, schema))
        || query
            .offset
            .as_ref()
            .is_some_and(|offset| expr_forces_generic_on_overflow(offset, schema))
        || query.projection.as_ref().is_some_and(|fields| {
            fields
                .iter()
                .any(|field| expr_forces_generic_on_overflow(&field.expr, schema))
        })
        || query.aggregation.as_ref().is_some_and(|agg| {
            agg.argument
                .as_ref()
                .is_some_and(|argument| expr_forces_generic_on_overflow(argument, schema))
        })
        || query.group_by.as_ref().is_some_and(|group| {
            group
                .keys
                .iter()
                .any(|key| expr_forces_generic_on_overflow(&key.expr, schema))
                || group
                    .having
                    .as_ref()
                    .is_some_and(|having| expr_forces_generic_on_overflow(having, schema))
        })
}

fn nested_forces_generic_on_overflow(nested: &NestedQuery, schema: &Schema) -> bool {
    expr_forces_generic_on_overflow(&nested.filter, schema)
        || nested
            .order
            .as_ref()
            .is_some_and(|order| order_forces_generic_on_overflow(order, schema))
        || nested
            .limit
            .as_ref()
            .is_some_and(|limit| expr_forces_generic_on_overflow(limit, schema))
        || nested
            .offset
            .as_ref()
            .is_some_and(|offset| expr_forces_generic_on_overflow(offset, schema))
        || nested
            .fields
            .iter()
            .any(|field| expr_forces_generic_on_overflow(&field.expr, schema))
}

fn order_forces_generic_on_overflow(order: &OrderClause, schema: &Schema) -> bool {
    order
        .keys
        .iter()
        .any(|key| expr_forces_generic_on_overflow(&key.expr, schema))
}

fn int_column(value: &Value) -> i64 {
    match value {
        Value::Int(v) => *v,
        _ => unreachable!("int column"),
    }
}

fn float_column(value: &Value) -> f64 {
    match value {
        Value::Float(v) => *v,
        _ => unreachable!("float column"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    const HUGE: i64 = 4_000_000_000_000_000_000;

    #[test]
    fn scalar_int_aggregates_honor_live_cancellation() {
        use crate::cancel::{CancelReason, ExecCancel};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::new(dir.path()).unwrap();
        engine.execute_powql("type T { n: int }").unwrap();
        for n in 0..10_000 {
            engine
                .catalog_mut()
                .get_table_mut("T")
                .unwrap()
                .insert(&vec![Value::Int(n)])
                .unwrap();
        }
        for function in [AggFunc::Sum, AggFunc::Min, AggFunc::Max] {
            let token = Arc::new(ExecCancel::new());
            token.block_at_checkpoint(2);
            let observer = Arc::clone(&token);
            let sender = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(3);
                while observer.checkpoint_count() < 2 && Instant::now() < deadline {
                    std::thread::yield_now();
                }
                let reached = observer.checkpoint_count() >= 2;
                observer.cancel(CancelReason::Disconnect);
                reached
            });
            let guard = crate::cancel::install(token);
            // Direct invocation places both checkpoints inside the scan,
            // after rows have contributed to the accumulator.
            let result = engine.agg_single_col_fast("T", "n", function, None);
            drop(guard);
            assert!(sender.join().unwrap(), "scan never reached its checkpoint");
            assert!(matches!(result, Err(QueryError::Cancelled)), "{result:?}");
        }
        assert!(matches!(
            engine.execute_powql("sum(T { .n })").unwrap(),
            QueryResult::Scalar(Value::Int(49_995_000))
        ));
    }

    /// Three rows whose int total (1.2e19) is well past `i64::MAX`, plus a
    /// str column the compiled aggregate path always declines.
    fn overflow_engine() -> Engine {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("powdb_agg_overflow_{}_{}", std::process::id(), id));
        let mut engine = Engine::new(&dir).unwrap();
        engine
            .execute_powql("type Big { required label: str, required n: int }")
            .unwrap();
        for _ in 0..3 {
            engine
                .execute_powql(&format!(r#"insert Big {{ label := "x", n := {HUGE} }}"#))
                .unwrap();
        }
        engine
    }

    fn error_of(engine: &mut Engine, query: &str) -> QueryError {
        engine
            .execute_powql(query)
            .expect_err("expected an error, got a result")
    }

    // `sum(Big { .n })` is the shape that reaches `agg_single_col_fast`:
    // Aggregate(SeqScan) over a fixed-size Int column. It used to answer
    // i64::MAX for a total of 1.2e19.
    #[test]
    fn compiled_fast_path_sum_overflow_is_an_error() {
        let mut engine = overflow_engine();
        let error = error_of(&mut engine, "sum(Big { .n })");
        assert!(
            matches!(&error, QueryError::Execution(message) if message.contains("overflow")),
            "got {error:?}"
        );
    }

    // Same data, same total, but `.n + 0` is not a bare field so the compiled
    // path declines and the generic path runs. The two must not disagree.
    #[test]
    fn compiled_and_generic_sum_paths_agree_on_overflow() {
        let mut engine = overflow_engine();
        assert_eq!(
            error_of(&mut engine, "sum(Big { .n })").to_string(),
            error_of(&mut engine, "sum(Big { .n + 0 })").to_string()
        );
    }

    // The compiled path only handles Int/Float columns, so a str column falls
    // through to the generic path, which now rejects it instead of answering
    // zero.
    #[test]
    fn sum_over_a_str_column_is_a_type_error() {
        let mut engine = overflow_engine();
        let error = error_of(&mut engine, "sum(Big { .label })");
        assert!(
            matches!(&error, QueryError::TypeError(message) if message.contains("numeric")),
            "got {error:?}"
        );
    }

    // A total that still fits keeps its exact value on both paths.
    #[test]
    fn sums_within_range_are_unchanged_on_both_paths() {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("powdb_agg_in_range_{}_{}", std::process::id(), id));
        let mut engine = Engine::new(&dir).unwrap();
        engine.execute_powql("type Small { n: int }").unwrap();
        for n in [1, 2, 3] {
            engine
                .execute_powql(&format!("insert Small {{ n := {n} }}"))
                .unwrap();
        }
        for query in ["sum(Small { .n })", "sum(Small { .n + 0 })"] {
            match engine.execute_powql(query).unwrap() {
                QueryResult::Scalar(Value::Int(total)) => assert_eq!(total, 6, "{query}"),
                other => panic!("{query}: expected Int(6), got {other:?}"),
            }
        }
    }
}
