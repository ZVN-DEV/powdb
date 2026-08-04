//! Specialized read fast paths: single-column aggregate, project+filter+limit,
//! and project+filter+sort+limit over raw row bytes.

use crate::cancel::CancelCheck;
use crate::result::{QueryError, QueryResult};
use powdb_storage::row::{decode_column, RowLayout};
use powdb_storage::types::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ops::ControlFlow;

use crate::executor::compiled::*;
use crate::executor::row_body_base;
use crate::executor::Engine;

use super::aggregate::agg_overflow_error;
use super::*;

impl Engine {
    // ─── Specialized fast paths ─────────────────────────────────────────────
    //
    // These methods are helpers for the `execute_plan` match arms above.
    // Each returns `Ok(Some(result))` when the fast path fires, `Ok(None)`
    // when the shape isn't supported (caller falls back to generic code).

    /// Aggregate sum/avg/min/max over a single fixed-size i64 column, with
    /// an optional compiled filter predicate. Walks raw row bytes — zero
    /// per-row allocation. Accumulates sum/avg in i128; a `sum` total that
    /// does not fit back into i64 is a typed error, not a clamped number.
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
                        // Clamping to i64::MAX here used to report a plausible
                        // number for a total that never happened. The generic
                        // paths raise the same error via `NumericAgg::sum`.
                        let total =
                            i64::try_from(sum_i128).map_err(|_| agg_overflow_error("sum"))?;
                        QueryResult::Scalar(Value::Int(total))
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
        // Overflow safety (P0-4): raw-byte projection over rehydrated rows
        // drops any row with a value too large to re-inline (>= 64KB) and
        // cannot return such a value; fall back to the decoded generic path.
        if self.catalog.table_has_overflow(table) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    const HUGE: i64 = 4_000_000_000_000_000_000;

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
