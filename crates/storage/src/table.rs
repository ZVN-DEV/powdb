use crate::btree::BTree;
use crate::error::StorageError;
use crate::heap::HeapFile;
use crate::page::{OVERFLOW_CHAIN_END, OVERFLOW_PAYLOAD_CAP};
use crate::row::{
    decode_column, decode_row, encode_row_into_with_layout, encode_row_v2_into,
    patch_var_column_in_place, plan_spill, OverflowStub, RowLayout, MAX_VALUE_SIZE,
};
use crate::types::*;
use std::io;
use std::path::Path;

/// Per-indexed-column metadata owning the BTree inline.
///
/// Mission C Phase 15 introduced this struct as a cache of `col_idx`,
/// `col_name`, and `is_int` so the hot `Table::insert` path could skip
/// the schema column-name linear scan. Mission C Phase 17 folds the
/// BTree itself into this struct, retiring the parallel
/// `FxHashMap<String, BTree>` that the hot write paths were otherwise
/// forced to probe every single call. Everything the write paths need
/// is now in a single tight `Vec<IndexedCol>` — no hash, no string
/// compare, no out-of-line allocation.
pub(crate) struct IndexedCol {
    /// Schema column index of the indexed column.
    pub col_idx: usize,
    /// Column name — still needed to resolve name-based lookups from the
    /// executor (`tbl.index("id")`, etc.). Cost is only paid on the
    /// rarer name-keyed read paths.
    pub col_name: String,
    /// `true` when the column type is `TypeId::Int`. Lets `insert` /
    /// `delete` take the `insert_int` / `delete_int` fast paths without
    /// re-matching the schema every call.
    pub is_int: bool,
    /// `true` for primary key / explicitly unique indexes. `false` for
    /// secondary indexes on non-unique columns. Non-unique indexes use
    /// composite keys (column_value + RowId) so duplicate column values
    /// don't overwrite each other.
    pub unique: bool,
    /// The B+ tree. Lives inline alongside the metadata so the hot
    /// insert/delete/update loops can touch a single cache line per
    /// index entry instead of chasing a separate HashMap probe.
    pub btree: BTree,
}

/// A table combines a heap file, schema, and optional indexes.
///
/// Mission C Phase 17: indexes used to live in a `FxHashMap<String,
/// BTree>` alongside a parallel `Vec<IndexedCol>` of metadata. Every row
/// insert paid an FxHash of the index column name to look the btree back
/// out of the map. This phase collapses both data structures into a
/// single `Vec<IndexedCol>` where each entry owns its btree inline —
/// the hot write path walks one small vec and calls straight through to
/// `insert_int`.
///
/// Mission C Phase 2: holds `encode_scratch`, a reusable buffer for
/// [`crate::row::encode_row_into`]. Bench loops that push thousands of
/// rows through `insert`/`update` reuse the same allocation across calls,
/// cutting the allocator traffic to ~zero after the first row.
pub struct Table {
    pub schema: Schema,
    pub heap: HeapFile,
    /// Reusable scratch buffer for row encoding. Cleared on every call.
    encode_scratch: Vec<u8>,
    /// Per-indexed-column metadata, each entry owning its BTree inline.
    /// Public to the crate so the query executor's IndexScan fast paths
    /// can reach in via the `index()` / `index_mut()` helpers instead
    /// of probing a separate hash map.
    pub(crate) indexed_cols: Vec<IndexedCol>,
    /// Mission C Phase 7: cached row layout so `delete` can decode only
    /// the indexed columns out of the raw page bytes without running the
    /// full per-row offset calculation every call.
    row_layout: RowLayout,
    /// Per-column literal defaults, aligned to `schema.columns` by position.
    /// Empty means "no defaults" (the common case); otherwise `defaults[i]`
    /// is the default for column `i`, or `None` if that column has none.
    defaults: Vec<Option<Value>>,
    /// Which columns are `auto` (auto-incrementing), aligned to
    /// `schema.columns`. Empty means no auto columns (the common case).
    auto_cols: Vec<bool>,
    /// Next value to assign per auto column, aligned to `schema.columns`.
    /// Lazily computed from the persisted rows on first insert after open
    /// (so the sequence resumes above the highest existing id), guarded by
    /// `auto_next_ready`.
    auto_next: Vec<i64>,
    auto_next_ready: bool,
}

impl Table {
    pub fn create(schema: Schema, data_dir: &Path) -> io::Result<Self> {
        let heap_path = data_dir.join(format!("{}.heap", schema.table_name));
        let heap = HeapFile::create(&heap_path)?;
        let row_layout = RowLayout::new(&schema);
        Ok(Table {
            schema,
            heap,
            encode_scratch: Vec::new(),
            indexed_cols: Vec::new(),
            row_layout,
            defaults: Vec::new(),
            auto_cols: Vec::new(),
            auto_next: Vec::new(),
            auto_next_ready: false,
        })
    }

    /// Reopen an existing table from disk. Caller supplies the schema (loaded
    /// from the catalog file). Indexes are NOT rebuilt — they live in memory
    /// until `create_index` is called again. Prefer `open_with_indexes` when
    /// the catalog knows which columns are indexed.
    pub fn open(schema: Schema, data_dir: &Path) -> io::Result<Self> {
        Self::open_with_indexes(schema, data_dir, &[])
    }

    /// Mission 3: reopen an existing table from disk, also rehydrating any
    /// persisted b-tree indexes.
    ///
    /// For each name in `indexed_col_names`:
    ///   - If the `{table}_{col}.idx` file exists, load it via
    ///     `BTree::load` — O(file size) memcpy+decode, no heap scan.
    ///   - If the file is missing (e.g. first open after upgrading from
    ///     pre-Mission-3 catalogs), fall back to the create-time rebuild
    ///     path: scan the heap and insert every non-empty value. After the
    ///     rebuild, `save` the freshly built tree so subsequent opens hit
    ///     the fast path.
    pub(crate) fn open_with_indexes(
        schema: Schema,
        data_dir: &Path,
        indexed_col_metas: &[crate::catalog::IndexedColMeta],
    ) -> io::Result<Self> {
        let heap_path = data_dir.join(format!("{}.heap", schema.table_name));
        let heap = HeapFile::open(&heap_path)?;
        let row_layout = RowLayout::new(&schema);
        let mut table = Table {
            schema,
            heap,
            encode_scratch: Vec::new(),
            indexed_cols: Vec::new(),
            row_layout,
            defaults: Vec::new(),
            auto_cols: Vec::new(),
            auto_next: Vec::new(),
            auto_next_ready: false,
        };

        for meta in indexed_col_metas {
            let col_name = &meta.name;
            let unique = meta.unique;
            let col_idx = match table.schema.column_index(col_name) {
                Some(i) => i,
                // Schema drift: the catalog lists an index on a column that
                // no longer exists. Silently drop the index rather than
                // failing the whole open — matches the `drop column`
                // rewrite path, which already blows away indexes.
                None => continue,
            };
            let is_int = table.schema.columns[col_idx].type_id == TypeId::Int;
            let idx_path = data_dir.join(format!("{}_{}.idx", table.schema.table_name, col_name));

            let btree = if idx_path.exists() {
                BTree::load(&idx_path)?
            } else {
                // Missing file: rebuild from the heap and save so we
                // take the fast path next time. Reassemble via `table.scan()`
                // so a spilled (v2) indexed column contributes its true key -- a
                // v1-only `decode_row` would read it as `Empty` and build a
                // btree missing that row's key (P2).
                let mut bt = BTree::create(&idx_path)?;
                for (rid, row) in table.scan() {
                    if !row[col_idx].is_empty() {
                        if unique {
                            bt.insert(row[col_idx].clone(), rid);
                        } else {
                            bt.insert_non_unique(row[col_idx].clone(), rid);
                        }
                    }
                }
                bt.save()?;
                bt
            };

            table.indexed_cols.push(IndexedCol {
                col_idx,
                col_name: col_name.clone(),
                is_int,
                unique,
                btree,
            });
        }

        Ok(table)
    }

    /// Mission 3: catalog uses this to snapshot the list of columns that
    /// currently have an index, so it can be persisted in `catalog.bin`.
    pub(crate) fn indexed_column_names(&self) -> Vec<String> {
        self.indexed_cols
            .iter()
            .map(|c| c.col_name.clone())
            .collect()
    }

    /// Snapshot index metadata (name + uniqueness) for catalog persistence.
    pub(crate) fn indexed_column_metas(&self) -> Vec<crate::catalog::IndexedColMeta> {
        self.indexed_cols
            .iter()
            .map(|c| crate::catalog::IndexedColMeta {
                name: c.col_name.clone(),
                unique: c.unique,
            })
            .collect()
    }

    /// Install the per-column defaults (called at create time and on reopen
    /// from the persisted catalog). Aligned to `schema.columns` by position.
    pub(crate) fn set_defaults(&mut self, defaults: Vec<Option<Value>>) {
        self.defaults = defaults;
    }

    /// Per-column defaults, aligned to `schema.columns` by position. Empty when
    /// no column has a default.
    pub(crate) fn defaults(&self) -> &[Option<Value>] {
        &self.defaults
    }

    /// Install which columns are `auto` (called at create time and on reopen
    /// from the persisted catalog), aligned to `schema.columns` by position.
    pub(crate) fn set_auto_cols(&mut self, auto_cols: Vec<bool>) {
        self.auto_cols = auto_cols;
        // Force the counters to be recomputed from the current rows on next use.
        self.auto_next_ready = false;
    }

    /// Whether this table has any `auto` column.
    pub(crate) fn has_auto(&self) -> bool {
        self.auto_cols.iter().any(|&a| a)
    }

    /// Which columns are `auto`, aligned to `schema.columns`. Empty when none.
    pub(crate) fn auto_cols(&self) -> &[bool] {
        &self.auto_cols
    }

    /// Fill any omitted (`Empty`) `auto` column in `values` from the per-table
    /// sequence and advance it. An explicitly-provided value is left as-is but
    /// still pushes the sequence past it, so later auto ids never collide with
    /// an id the caller chose. No-op when the table has no auto columns.
    pub(crate) fn assign_auto(&mut self, values: &mut [Value]) {
        if !self.has_auto() {
            return;
        }
        if !self.auto_next_ready {
            self.init_auto_next();
        }
        for (i, &is_auto) in self.auto_cols.iter().enumerate() {
            if !is_auto || i >= values.len() {
                continue;
            }
            match values[i] {
                Value::Empty => {
                    values[i] = Value::Int(self.auto_next[i]);
                    self.auto_next[i] += 1;
                }
                Value::Int(v) if v >= self.auto_next[i] => {
                    self.auto_next[i] = v + 1;
                }
                _ => {}
            }
        }
    }

    /// Seed each auto column's next value to one past the highest value already
    /// stored, so the sequence resumes correctly after a restart (committed
    /// rows are already replayed from the WAL by the time inserts run). An
    /// empty column starts at 1.
    fn init_auto_next(&mut self) {
        let n = self.schema.columns.len();
        let mut next = vec![1i64; n];
        let auto_idxs: Vec<usize> = self
            .auto_cols
            .iter()
            .enumerate()
            .filter_map(|(i, &a)| if a { Some(i) } else { None })
            .collect();
        if !auto_idxs.is_empty() {
            for (_rid, row) in self.heap.scan() {
                let decoded = crate::row::decode_row(&self.schema, &row);
                for &i in &auto_idxs {
                    if let Some(Value::Int(v)) = decoded.get(i) {
                        if *v >= next[i] {
                            next[i] = *v + 1;
                        }
                    }
                }
            }
        }
        self.auto_next = next;
        self.auto_next_ready = true;
    }

    /// Recalculate the cached row layout from the current schema. Must be
    /// called after any schema mutation (add/drop column).
    pub fn refresh_layout(&mut self) {
        self.row_layout = RowLayout::new(&self.schema);
    }

    /// Rewrite every live heap row to match a new schema shape.
    ///
    /// This is the backfill path for `ALTER TABLE ADD COLUMN`. Before
    /// this existed, the catalog happily swapped the schema in memory
    /// and left old rows on disk with the OLD variable-column offset
    /// table layout. Any subsequent `decode_row` then panicked with
    /// `range end index X out of range` because the decoder reads
    /// `n_var + 1` offsets using the NEW schema.
    ///
    /// The caller passes in the pre-mutation schema so rows can be
    /// decoded correctly; `self.schema` must already hold the NEW
    /// schema when this is invoked. `fill_values` must have
    /// `new_schema.columns.len()` entries and supplies the values for
    /// columns that did not exist in the old schema (use
    /// `Value::Empty` for optional adds).
    ///
    /// Rewrites every row via `HeapFile::update`, which may move the
    /// row to a new page when the new encoding is larger. Any secondary
    /// indexes are rebuilt from scratch at the end because their
    /// `RowId` pointers can become stale during the rewrite.
    ///
    /// Not on any hot path — ALTER is a rare administrative op, so this
    /// intentionally prefers simplicity (collect snapshot → rewrite →
    /// rebuild indexes) over any of the fast-path tricks used by
    /// insert/update/delete.
    pub(crate) fn rewrite_rows_for_schema_change(
        &mut self,
        old_schema: &Schema,
        fill_values: &[Value],
        data_dir: &Path,
    ) -> io::Result<()> {
        debug_assert_eq!(fill_values.len(), self.schema.columns.len());

        // Snapshot every live (rid, old_bytes) pair up front. We can't
        // mutate `self.heap` while iterating it, and the rewrite grows
        // every row (+2 bytes of offset table at minimum), so in-place
        // updates are not guaranteed.
        let snapshot: Vec<(RowId, Vec<u8>)> = self.heap.scan().collect();

        // Map from old column index → new column index, or `None` if
        // the old column was dropped by the schema change. The caller
        // is expected to keep surviving columns in their original
        // positions. We look up by name so ADD and DROP can share the
        // same path: ADD has every old column present in the new
        // schema; DROP has exactly one missing.
        let old_to_new: Vec<Option<usize>> = old_schema
            .columns
            .iter()
            .map(|c| self.schema.column_index(&c.name))
            .collect();

        // Phase 1 (immutable reads): reassemble each old row to its logical
        // values, mapping columns into the new shape, and gather any overflow
        // chain it referenced. A v2 (spilled) old row MUST be reassembled via
        // `decode_row_v2` -- a v1-only `decode_row` reads its spilled columns as
        // `Empty` and the ALTER would silently drop the out-of-line value (P1).
        // We use the OLD schema's layout because `self.row_layout` already
        // reflects the NEW schema.
        let old_layout = RowLayout::new(old_schema);
        let mut rewritten: Vec<(RowId, Vec<Value>, Vec<u32>)> = Vec::with_capacity(snapshot.len());
        for (rid, old_bytes) in &snapshot {
            let old_row = if crate::row::row_is_v2(old_bytes) {
                crate::row::decode_row_v2(old_schema, &old_layout, old_bytes, |stub| {
                    self.heap.read_overflow_value(stub).map_err(io::Error::from)
                })?
            } else {
                decode_row(old_schema, old_bytes)
            };
            // Chain pages the old row referenced (empty for inline rows). Freed
            // after the rewrite so the ALTER does not leak the old out-of-line
            // values.
            let mut old_pages: Vec<u32> = Vec::new();
            if crate::row::row_is_v2(old_bytes) {
                let mut heads: Vec<u32> = Vec::new();
                crate::row::for_each_stub(old_schema, &old_layout, old_bytes, |_c, stub| {
                    heads.push(stub.first_page);
                });
                for h in heads {
                    old_pages.extend(self.heap.overflow_chain_pages(h)?);
                }
            }
            // Start from the caller-supplied defaults for the new shape, then
            // overwrite with whatever the old row had. Dropped columns are
            // skipped (their value has nowhere to go in the new row).
            let mut new_row: Vec<Value> = fill_values.to_vec();
            for (old_idx, val) in old_row.into_iter().enumerate() {
                if let Some(new_idx) = old_to_new[old_idx] {
                    new_row[new_idx] = val;
                }
            }
            rewritten.push((*rid, new_row, old_pages));
        }

        // Phase 2 (mutations): re-encode each row into the new shape and write
        // it back. A row whose surviving values still exceed the inline cap
        // re-spills through `encode_row_spilling` (writing fresh chains), so a
        // large value survives ALTER; then the old chain is freed.
        for (rid, new_row, old_pages) in rewritten {
            if crate::row::v1_encoded_len(&self.row_layout, &new_row)
                <= crate::page::MAX_ROW_DATA_SIZE
            {
                encode_row_into_with_layout(
                    &self.schema,
                    &self.row_layout,
                    &new_row,
                    &mut self.encode_scratch,
                );
                let encoded = std::mem::take(&mut self.encode_scratch);
                self.heap.update(rid, &encoded)?;
                self.encode_scratch = encoded;
            } else {
                let encoded = self.encode_row_spilling(&new_row)?;
                self.heap.update(rid, &encoded)?;
            }
            // Free the old chain now that the row has been rewritten. Fresh
            // chains for later rows may reuse these pages.
            if !old_pages.is_empty() {
                self.heap.release_overflow_pages(&old_pages);
            }
        }

        // Rebuild every secondary index from the rewritten heap. The
        // in-memory btree is the source of truth for reads, and its
        // RowId pointers may now be stale after the heap rewrite.
        if !self.indexed_cols.is_empty() {
            // Preserve per-index metadata (col_idx, col_name, is_int)
            // via fresh BTree instances. The old btrees are dropped
            // when `indexed_cols` is reassigned.
            let existing: Vec<(usize, String, bool, bool)> = self
                .indexed_cols
                .iter()
                .map(|c| (c.col_idx, c.col_name.clone(), c.is_int, c.unique))
                .collect();

            // Drain the old entries first so the borrow of
            // `self.indexed_cols` is clear before we start scanning.
            self.indexed_cols.clear();

            // Snapshot the rewritten heap once. The rewrite above may have
            // re-spilled a surviving large indexed value, so rows can be v2 and
            // must be reassembled (`self.scan()`) -- a v1-only `decode_row`
            // would read a spilled indexed column as `Empty` and build a btree
            // missing that key (P2).
            let rebuilt_rows: Vec<(RowId, Vec<Value>)> = self.scan().collect();
            for (col_idx, col_name, is_int, unique) in existing {
                // Mission 3: write the freshly rebuilt index back to its
                // canonical `{table}_{col}.idx` file so a subsequent
                // restart loads the up-to-date tree instead of the stale
                // pre-rewrite version (whose RowIds may now point at
                // moved rows).
                let idx_path =
                    data_dir.join(format!("{}_{}.idx", self.schema.table_name, col_name));
                let mut btree = crate::btree::BTree::create(&idx_path)?;
                for (rid, row) in &rebuilt_rows {
                    let (rid, v) = (*rid, &row[col_idx]);
                    if v.is_empty() {
                        continue;
                    }
                    if unique {
                        if is_int {
                            if let Value::Int(i) = v {
                                btree.insert_int(*i, rid);
                                continue;
                            }
                        }
                        btree.insert(v.clone(), rid);
                    } else {
                        btree.insert_non_unique(v.clone(), rid);
                    }
                }
                btree.save()?;
                self.indexed_cols.push(IndexedCol {
                    col_idx,
                    col_name,
                    is_int,
                    unique,
                    btree,
                });
            }
        }

        Ok(())
    }

    /// Look up an index by column name. Returns `None` if no index on
    /// this column. Used by the read-side executor paths (IndexScan,
    /// Project(IndexScan), etc.) that still need name-based resolution;
    /// the write-side hot paths iterate `indexed_cols` directly.
    #[inline]
    pub fn index(&self, col_name: &str) -> Option<&BTree> {
        self.indexed_cols
            .iter()
            .find(|c| c.col_name == col_name)
            .map(|c| &c.btree)
    }

    /// Mutable counterpart to [`Self::index`].
    #[inline]
    pub fn index_mut(&mut self, col_name: &str) -> Option<&mut BTree> {
        self.indexed_cols
            .iter_mut()
            .find(|c| c.col_name == col_name)
            .map(|c| &mut c.btree)
    }

    /// `true` if this table has an index on the named column.
    #[inline]
    pub fn has_index(&self, col_name: &str) -> bool {
        self.indexed_cols.iter().any(|c| c.col_name == col_name)
    }

    /// `true` if this table has no secondary indexes at all.
    #[inline]
    pub fn indexes_is_empty(&self) -> bool {
        self.indexed_cols.is_empty()
    }

    /// Mission C Phase 15: the hot insert path used to do two wasted
    /// things per secondary index, on every row:
    ///   1. `for (col_name, btree) in &mut self.indexes` walked an
    ///      FxHashMap by iterator (cheap but not free), and
    ///   2. `self.schema.column_index(col_name)` walked `schema.columns`
    ///      doing an O(n_cols) strcmp linear search to translate the
    ///      column name back into its schema position.
    ///
    /// For the `insert_batch_1k` bench (1K rows, User table, one index on
    /// `id`) that came out to ~6 strcmps * 1000 rows = 6K wasted
    /// comparisons per iteration, plus the HashMap iter overhead. We now
    /// iterate the precomputed `indexed_cols` slice directly, which hands
    /// us `(col_idx, col_name, is_int)` per entry, and route int keys
    /// straight through `BTree::insert_int` to skip the generic
    /// `Value::Ord` dispatch on every binary-search comparison.
    pub fn insert(&mut self, values: &Row) -> io::Result<RowId> {
        self.check_unique_on_insert(values)?;
        // Common case: the row fits inline (v1) — encode straight into scratch,
        // byte-identical to pre-v0.11. Size it first WITHOUT encoding so a huge
        // value (which the debug v1 encoder would panic on) routes to spill.
        if crate::row::v1_encoded_len(&self.row_layout, values) <= crate::page::MAX_ROW_DATA_SIZE {
            encode_row_into_with_layout(
                &self.schema,
                &self.row_layout,
                values,
                &mut self.encode_scratch,
            );
            let rid = self.heap.insert(&self.encode_scratch)?;
            self.maintain_indexes_on_insert(values, rid);
            return Ok(rid);
        }
        // Otherwise spill the largest var values out of line and store a v2
        // stub row. This self-contained path (no WAL) backs the WAL-off and
        // direct-`Table` callers; the WAL path in `Catalog` writes the same
        // chains but logs each chunk for crash recovery.
        let encoded = self.encode_row_spilling(values)?;
        let rid = self.heap.insert(&encoded)?;
        self.maintain_indexes_on_insert(values, rid);
        Ok(rid)
    }

    /// Mark-and-sweep this table's overflow pages (design 3.6). Reclaims every
    /// Overflow-typed page not referenced by a live row's stub — i.e. orphans
    /// left by crashed transactions or by delete/chain-replacing updates whose
    /// pages were never returned to the free list. Returns the reclaimed page
    /// ids; the caller (catalog) logs them as one `OverflowFree` record.
    ///
    /// Runs against an on-disk-consistent view (flushes dirty pages first) and
    /// reads only version words, bitmaps, and stubs — never a full decode.
    pub(crate) fn sweep_overflow(&mut self) -> io::Result<Vec<u32>> {
        self.heap.flush_all_dirty()?;
        // Mark: collect every referenced chain head from live v2 rows, then
        // walk each chain into the referenced set. Stubs are gathered first so
        // the row scan's `&heap` borrow is released before the chain walks.
        let schema = &self.schema;
        let layout = &self.row_layout;
        let mut heads: Vec<u32> = Vec::new();
        self.heap.for_each_row(|_rid, bytes| {
            if crate::row::row_is_v2(bytes) {
                crate::row::for_each_stub(schema, layout, bytes, |_col, stub| {
                    heads.push(stub.first_page);
                });
            }
        });
        let mut referenced: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for head in heads {
            for pid in self.heap.overflow_chain_pages(head)? {
                referenced.insert(pid);
            }
        }
        // Sweep: reclaim unreferenced overflow pages below the watermark.
        self.heap.sweep_unreferenced_overflow(&referenced)
    }

    /// Collect every overflow-chain page id referenced by the row at `rid`, or
    /// an empty vec when the table has never spilled, the row is gone, or the
    /// row is inline (v1). Used by the catalog to free a row's old chain when an
    /// update replaces/removes its spilled value or the row is deleted (design
    /// 3.6), so steady-state churn reclaims pages instead of leaking them.
    ///
    /// Does not touch the free list itself: the caller decides when it is safe
    /// to release (immediately for autocommit, at commit for an explicit tx).
    pub(crate) fn overflow_chain_pages_at(&self, rid: RowId) -> io::Result<Vec<u32>> {
        if !self.has_overflow_rows() {
            return Ok(Vec::new());
        }
        let data = match self.heap.get(rid) {
            Some(d) => d,
            None => return Ok(Vec::new()),
        };
        if !crate::row::row_is_v2(&data) {
            return Ok(Vec::new());
        }
        let mut heads: Vec<u32> = Vec::new();
        crate::row::for_each_stub(&self.schema, &self.row_layout, &data, |_col, stub| {
            heads.push(stub.first_page);
        });
        let mut pages = Vec::new();
        for head in heads {
            pages.extend(self.heap.overflow_chain_pages(head)?);
        }
        Ok(pages)
    }

    /// Return a set of overflow-chain pages to this table's in-memory free list
    /// for reuse by the next spill. The catalog calls this once it is safe to
    /// reclaim (see [`Self::overflow_chain_pages_at`]).
    pub(crate) fn release_overflow_pages(&mut self, pages: &[u32]) {
        self.heap.release_overflow_pages(pages);
    }

    /// Build the v2 stub-row encoding for `values`, writing each spilled
    /// value's overflow chain directly to the heap (no WAL — the WAL path
    /// lives in `Catalog`). Enforces `MAX_VALUE_SIZE` per value.
    fn encode_row_spilling(&mut self, values: &Row) -> io::Result<Vec<u8>> {
        let v1_len = crate::row::v1_encoded_len(&self.row_layout, values);
        let is_indexed = self.indexed_col_mask();
        let chosen = plan_spill(&self.row_layout, values, v1_len, &is_indexed);
        let n_var = self.row_layout.n_var();
        let mut spilled: Vec<Option<OverflowStub>> = vec![None; n_var];
        for col_idx in chosen {
            let var_idx = self
                .row_layout
                .var_index(col_idx)
                .expect("plan_spill only returns var columns");
            let bytes: Vec<u8> = match &values[col_idx] {
                Value::Str(s) => s.as_bytes().to_vec(),
                Value::Bytes(b) => b.to_vec(),
                Value::Json(b) => b.to_vec(),
                _ => continue,
            };
            if bytes.len() > MAX_VALUE_SIZE {
                return Err(StorageError::ValueTooLarge {
                    size: bytes.len(),
                    max: MAX_VALUE_SIZE,
                }
                .into());
            }
            spilled[var_idx] = Some(self.write_value_chain(&bytes)?);
        }
        let mut out = Vec::new();
        encode_row_v2_into(&self.schema, &self.row_layout, values, &spilled, &mut out);
        Ok(out)
    }

    /// Allocate and write an overflow chain for `value` directly to the heap
    /// (LSN 0, no WAL). Head-first, singly linked. Returns the stub.
    fn write_value_chain(&mut self, value: &[u8]) -> io::Result<OverflowStub> {
        let n = value.len().div_ceil(OVERFLOW_PAYLOAD_CAP).max(1);
        let mut pages = Vec::with_capacity(n);
        for _ in 0..n {
            pages.push(self.heap.allocate_overflow_page()?);
        }
        for i in 0..n {
            let start = i * OVERFLOW_PAYLOAD_CAP;
            let end = (start + OVERFLOW_PAYLOAD_CAP).min(value.len());
            let next = if i + 1 < n {
                pages[i + 1]
            } else {
                OVERFLOW_CHAIN_END
            };
            self.heap
                .write_overflow_page(pages[i], next, &value[start..end], 0)?;
        }
        Ok(OverflowStub::new(
            value.len() as u64,
            pages[0],
            crc32fast::hash(value),
        ))
    }

    /// Insert a row whose bytes were encoded by the caller (used by the
    /// overflow spill path, which builds a v2 stub row and writes the
    /// out-of-line chains before calling here). Index maintenance still uses
    /// the LOGICAL `values` — extraction happens on the full value before
    /// spill, so indexes never see a stub.
    pub(crate) fn insert_encoded(&mut self, values: &Row, encoded: &[u8]) -> io::Result<RowId> {
        self.check_unique_on_insert(values)?;
        let rid = self.heap.insert(encoded)?;
        self.maintain_indexes_on_insert(values, rid);
        Ok(rid)
    }

    /// Unique-constraint pre-check: reject BEFORE touching the heap if any
    /// unique column already holds the incoming value.
    fn check_unique_on_insert(&self, values: &Row) -> io::Result<()> {
        for entry in &self.indexed_cols {
            if !entry.unique {
                continue;
            }
            let val = &values[entry.col_idx];
            if !val.is_empty() && entry.btree.lookup(val).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "unique constraint violation on {}.{}",
                        self.schema.table_name, entry.col_name
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Insert the row's indexed columns into every b-tree from the logical
    /// values. Blocker B3: marks trees dirty in memory; the save is deferred
    /// to the next checkpoint.
    fn maintain_indexes_on_insert(&mut self, values: &Row, rid: RowId) {
        if self.indexed_cols.is_empty() {
            return;
        }
        for entry in &mut self.indexed_cols {
            let val = &values[entry.col_idx];
            if val.is_empty() {
                continue;
            }
            if entry.unique {
                if entry.is_int {
                    if let Value::Int(i) = val {
                        entry.btree.insert_int(*i, rid);
                        continue;
                    }
                }
                entry.btree.insert(val.clone(), rid);
            } else {
                entry.btree.insert_non_unique(val.clone(), rid);
            }
        }
    }

    /// Blocker B3: flush every dirty btree index to disk. Wired into
    /// [`crate::catalog::Catalog::checkpoint`] and its `Drop` impl so
    /// we get one fsync + rename per dirty index per checkpoint, not
    /// one per inserted row. Clean trees (no mutations since last
    /// save) are free — `BTree::save_if_dirty` early-returns.
    pub(crate) fn save_dirty_indexes(&mut self) -> io::Result<()> {
        for entry in self.indexed_cols.iter_mut() {
            entry.btree.save_if_dirty()?;
        }
        Ok(())
    }

    /// Discard uncommitted, in-memory index mutations so they never reach
    /// disk. Called by ROLLBACK on the catalog it is about to drop: without
    /// this, the drop-time checkpoint's `save_dirty_indexes` would flush the
    /// rolled-back index writes to the `.idx` files, poisoning the unique
    /// index (see `Catalog::rollback_to_last_sync_inner`). Mirrors
    /// `Heap::discard_dirty` for the heap side.
    pub(crate) fn discard_dirty_indexes(&mut self) {
        for entry in self.indexed_cols.iter_mut() {
            entry.btree.discard_dirty();
        }
    }

    /// Blocker B3: rebuild every secondary index from the heap.
    ///
    /// Used by the crash-recovery path in `Catalog::open`: after WAL
    /// replay lands rows back in the heap, the on-disk `.idx` files
    /// may lag (or lead) the heap because the prior session deferred
    /// btree saves until checkpoint. Replaying is cheap — we walk the
    /// heap once per index — and produces a tree that exactly matches
    /// the current heap state, which is the invariant subsequent
    /// inserts assume.
    ///
    /// After this call, every indexed tree is marked dirty so the
    /// next `Catalog::checkpoint` persists the recovered state.
    pub(crate) fn rebuild_indexes_from_heap(&mut self) -> io::Result<()> {
        if self.indexed_cols.is_empty() {
            return Ok(());
        }

        // Snapshot raw rows once so the reassembly below can take a second
        // shared borrow of the heap (read_overflow_value) without conflicting
        // with a live scan iterator. Rebuild is a cold recovery path, so the
        // owned snapshot is fine.
        let raw_rows: Vec<(RowId, Vec<u8>)> = self.heap.scan().collect();
        let schema = &self.schema;
        let layout = &self.row_layout;
        let heap = &self.heap;
        for entry in self.indexed_cols.iter_mut() {
            let mut fresh = BTree::create(entry.btree.file_path())?;
            for (rid, raw) in &raw_rows {
                let (rid, raw) = (*rid, raw.as_slice());
                // v2 rows must be reassembled so a spilled indexed column
                // produces its true key, not Empty (P2: create-index /
                // rebuild-after-spill must not build a btree of missing keys).
                let owned;
                let v: &Value = if crate::row::row_is_v2(raw) {
                    match crate::row::decode_row_v2(schema, layout, raw, |stub| {
                        heap.read_overflow_value(stub).map_err(io::Error::from)
                    }) {
                        Ok(row) => {
                            owned = row[entry.col_idx].clone();
                            &owned
                        }
                        Err(_) => continue,
                    }
                } else {
                    owned = decode_column(schema, layout, raw, entry.col_idx);
                    &owned
                };
                if v.is_empty() {
                    continue;
                }
                if entry.unique {
                    if entry.is_int {
                        if let Value::Int(i) = v {
                            fresh.insert_int(*i, rid);
                            continue;
                        }
                    }
                    fresh.insert(v.clone(), rid);
                } else {
                    fresh.insert_non_unique(v.clone(), rid);
                }
            }
            // Force-mark dirty so the next checkpoint flushes the
            // freshly rebuilt tree, even if no further mutations
            // happen before shutdown.
            fresh.mark_dirty();
            entry.btree = fresh;
        }
        Ok(())
    }

    pub fn get(&self, rid: RowId) -> Option<Row> {
        let data = self.heap.get(rid)?;
        if crate::row::row_is_v2(&data) {
            // v2 row: reassemble each spilled column from its overflow chain.
            return crate::row::decode_row_v2(&self.schema, &self.row_layout, &data, |stub| {
                self.heap.read_overflow_value(stub).map_err(io::Error::from)
            })
            .ok();
        }
        Some(decode_row(&self.schema, &data))
    }

    /// Delete a row. Mission C Phase 7: if the table has indexes, we used to
    /// call `decode_row` here — allocating `Row` + every column's `Value`
    /// just to read the two or three columns that actually feed the index.
    /// Now we borrow the raw page bytes once and call `decode_column` for
    /// exactly the indexed columns, skipping the rest of the row entirely.
    ///
    /// Mission C Phase 11: the Phase 7 version still allocated a
    /// `Vec<(usize, Value)>` per row so the btree mutations could happen
    /// after the hot-page borrow closed. That's 3300 heap allocations per
    /// 100K-row `delete_by_filter` iteration — gone in Phase 11 via
    /// struct-field borrow splitting, so the btree lives alongside the
    /// page borrow inside the closure.
    pub fn delete(&mut self, rid: RowId) -> io::Result<()> {
        if self.indexed_cols.is_empty() {
            return self.heap.delete(rid);
        }

        // Split the borrow so `indexed_cols` (mutable — the btree lives
        // inside each entry now) can be captured by the closure alongside
        // `heap` (also mutable). Rust's disjoint-field borrowing lets
        // this compile without cloning anything.
        let Table {
            heap,
            schema,
            row_layout: layout,
            indexed_cols,
            ..
        } = self;

        // A spilled indexed column holds only a stub inline, so its key must be
        // reassembled from the overflow chain — a v1-only `decode_column`
        // yields `Empty` and leaves the btree entry dangling (P2). Collect such
        // columns under the pinned borrow, reassemble after it closes. The v1
        // fast path is unchanged (no v2 row ⇒ `raw_stub` is always None ⇒ no
        // allocation, no deferral).
        let mut deferred: Vec<(usize, crate::row::OverflowStub)> = Vec::new();
        heap.with_row_bytes(rid, |data| {
            for (slot, entry) in indexed_cols.iter_mut().enumerate() {
                if let Some(stub) = crate::row::raw_stub(schema, layout, data, entry.col_idx) {
                    deferred.push((slot, stub));
                    continue;
                }
                let val = decode_column(schema, layout, data, entry.col_idx);
                if val.is_empty() {
                    continue;
                }
                if entry.unique {
                    // Unique index: key is the column value directly.
                    match &val {
                        Value::Int(i) => {
                            entry.btree.delete_int(*i);
                        }
                        _ => {
                            entry.btree.delete(&val);
                        }
                    }
                } else {
                    // Non-unique index: key is composite (col_val, rid).
                    entry.btree.delete_non_unique(&val, rid);
                }
            }
        })?;

        // Reassemble + delete keys for any spilled indexed columns (rare:
        // `plan_spill` keeps indexed columns inline, so this only fires for
        // legacy rows or a column indexed AFTER its values spilled). The chain
        // pages are still intact here — `heap.delete` below only clears the
        // stub row's slot, never the overflow chain.
        for (slot, stub) in deferred {
            let bytes = heap.read_overflow_value(&stub).map_err(io::Error::from)?;
            let entry = &mut indexed_cols[slot];
            let val = match schema.columns[entry.col_idx].type_id {
                TypeId::Str => Value::Str(String::from_utf8_lossy(&bytes).into_owned()),
                TypeId::Bytes => Value::Bytes(bytes),
                TypeId::Json => Value::Json(bytes.into()),
                _ => continue,
            };
            if entry.unique {
                entry.btree.delete(&val);
            } else {
                entry.btree.delete_non_unique(&val, rid);
            }
        }

        self.heap.delete(rid)?;
        // Blocker B3: btree mutations above marked the indexes dirty.
        // The actual persist happens at the next `Catalog::checkpoint`
        // (or `Drop`), batching many deletes into one fsync per index.
        Ok(())
    }

    /// Mission C Phase 12: bulk delete a list of rids, batching the
    /// secondary-index maintenance.
    ///
    /// For a 100K-row `delete_by_filter` that removes ~20% of the rows,
    /// the per-row `Table::delete` path pays ~4ms of pure `Vec::remove`
    /// memmove inside the btree: every call shifts up to 4KB of leaf
    /// entries. This helper collects the indexed-column keys first,
    /// deletes the heap slots one by one (hot-page writes), then compacts
    /// each btree in a single pass via [`BTree::delete_many_int`].
    ///
    /// Restrictions / fall-through:
    /// - If the table has no indexes, this is equivalent to looping over
    ///   `heap.delete`.
    /// - If any indexed column is not `TypeId::Int`, this falls back to
    ///   the per-row `delete` path. The int-only constraint matches the
    ///   only btree batch primitive we have (`delete_many_int`) and
    ///   covers the overwhelmingly common case (primary keys,
    ///   `created_at`, foreign keys).
    ///
    /// Returns the number of rows removed.
    pub fn delete_many(&mut self, rids: &[RowId]) -> io::Result<u64> {
        if rids.is_empty() {
            return Ok(0);
        }
        if self.indexed_cols.is_empty() {
            for &rid in rids {
                self.heap.delete(rid)?;
            }
            return Ok(rids.len() as u64);
        }

        // All indexed cols must be int AND unique for the batch btree
        // path to apply. Non-unique indexes use composite keys, so the
        // `delete_many_int` primitive (which searches by raw i64) won't
        // find them.
        let all_int_unique = self.indexed_cols.iter().all(|c| c.is_int && c.unique);
        if !all_int_unique {
            // Mixed index types — defer to the generic per-row path.
            let mut count = 0u64;
            for &rid in rids {
                self.delete(rid)?;
                count += 1;
            }
            return Ok(count);
        }

        // Split the borrow so the closure can capture `schema`/`layout`/
        // `indexed_cols` while `heap` is borrowed mutably by
        // `delete_with_hook`.
        let Table {
            heap,
            schema,
            row_layout: layout,
            indexed_cols,
            ..
        } = self;

        let n_indexed = indexed_cols.len();
        let mut keys_per_index: Vec<Vec<i64>> = (0..n_indexed)
            .map(|_| Vec::with_capacity(rids.len()))
            .collect();

        let mut count = 0u64;
        for &rid in rids {
            let found = heap.delete_with_hook(rid, |data| {
                for (slot_i, entry) in indexed_cols.iter().enumerate() {
                    if let Value::Int(i) = decode_column(schema, layout, data, entry.col_idx) {
                        keys_per_index[slot_i].push(i);
                    }
                }
            })?;
            if found {
                count += 1;
            }
        }

        // Batch-compact each btree in a single leaf-chain walk. Mission C
        // Phase 17: btrees now live inline in indexed_cols, so this is a
        // direct `iter_mut()` over the same slice the hook above borrowed
        // immutably — no HashMap probe required.
        for (slot_i, entry) in indexed_cols.iter_mut().enumerate() {
            let keys = &mut keys_per_index[slot_i];
            keys.sort_unstable();
            entry.btree.delete_many_int(keys);
        }

        // Blocker B3: indexes are now dirty in memory; `delete_many_int`
        // already flipped the dirty flag on each mutated btree above.
        // Checkpoint batches the persist.

        Ok(count)
    }

    /// Single-pass scan-and-delete driven by a raw-bytes predicate. Walks
    /// the heap once, marks matching rows deleted in place, and updates
    /// any int-keyed secondary indexes in a single batched
    /// `delete_many_int` per index at the end. Non-int secondary indexes
    /// fall back to per-key `btree.delete`, but still ride the same
    /// single heap pass.
    ///
    /// Mission C Phase 16: this is the Table-level hook for
    /// [`HeapFile::scan_delete_matching`]. See that method for the
    /// fusion rationale. The executor's `Delete` fast path routes
    /// `Filter(SeqScan)` / `SeqScan`-shaped delete plans here when the
    /// predicate compiles.
    pub fn scan_delete_matching<P>(&mut self, pred: P) -> io::Result<u64>
    where
        P: FnMut(&[u8]) -> bool,
    {
        self.scan_delete_matching_with_hook(pred, |_, _| {})
    }

    /// Variant of [`Self::scan_delete_matching`] that lets the caller
    /// observe every matched row just before it's marked deleted. Used
    /// by [`crate::catalog::Catalog::scan_delete_matching_logged`] to
    /// emit one WAL `Delete` record per victim in the same single-pass
    /// scan — no second walk over the heap, no per-row `ensure_hot`
    /// round-trip.
    ///
    /// The user hook runs inside the heap's pinned hot-page borrow, so
    /// it must not call back into the catalog / table / heap. The WAL
    /// append path only writes into an in-memory buffer and is safe.
    pub fn scan_delete_matching_with_hook<P, H>(
        &mut self,
        pred: P,
        mut user_hook: H,
    ) -> io::Result<u64>
    where
        P: FnMut(&[u8]) -> bool,
        H: FnMut(RowId, &[u8]),
    {
        if self.indexed_cols.is_empty() {
            return self.heap.scan_delete_matching(pred, |rid, bytes| {
                user_hook(rid, bytes);
            });
        }

        // Split the borrow so the hook closure can capture schema /
        // layout / indexed_cols (immutably for reads) while `heap` is
        // mutably borrowed by `scan_delete_matching`. After the scan
        // completes, the closure is dropped, freeing the shared borrow
        // of `indexed_cols` so we can flip to `iter_mut()` for the
        // batch btree compaction.
        let Table {
            heap,
            schema,
            row_layout: layout,
            indexed_cols,
            ..
        } = self;

        let n_indexed = indexed_cols.len();
        let all_int_unique = indexed_cols.iter().all(|c| c.is_int && c.unique);

        if all_int_unique {
            let mut keys_per_index: Vec<Vec<i64>> =
                (0..n_indexed).map(|_| Vec::with_capacity(1024)).collect();

            let count = heap.scan_delete_matching(pred, |rid, data| {
                for (slot_i, entry) in indexed_cols.iter().enumerate() {
                    if let Value::Int(i) = decode_column(schema, layout, data, entry.col_idx) {
                        keys_per_index[slot_i].push(i);
                    }
                }
                user_hook(rid, data);
            })?;

            // Mission C Phase 17: btrees live inline in indexed_cols,
            // so this direct iter_mut replaces the old HashMap probe.
            for (slot_i, entry) in indexed_cols.iter_mut().enumerate() {
                let keys = &mut keys_per_index[slot_i];
                keys.sort_unstable();
                entry.btree.delete_many_int(keys);
            }
            // Blocker B3: dirty flags are already set by the
            // per-btree `delete_many_int` call above; checkpoint
            // handles the persist.
            return Ok(count);
        }

        // Mixed / non-int / non-unique secondary indexes: single heap
        // pass, per-key btree deletes at the end. We collect (value, rid)
        // pairs so non-unique indexes can delete the correct composite key.
        let mut entries_per_index: Vec<Vec<(Value, RowId)>> =
            (0..n_indexed).map(|_| Vec::with_capacity(256)).collect();
        // Spilled indexed columns (v2 rows) can't be decoded inline — collect
        // their stubs and reassemble after the scan releases the heap borrow
        // (P2: a v1-only decode_column would yield Empty ⇒ dangling entry).
        let mut deferred: Vec<(usize, crate::row::OverflowStub, RowId)> = Vec::new();

        let count = heap.scan_delete_matching(pred, |rid, data| {
            for (slot_i, entry) in indexed_cols.iter().enumerate() {
                if let Some(stub) = crate::row::raw_stub(schema, layout, data, entry.col_idx) {
                    deferred.push((slot_i, stub, rid));
                    continue;
                }
                let v = decode_column(schema, layout, data, entry.col_idx);
                if !v.is_empty() {
                    entries_per_index[slot_i].push((v, rid));
                }
            }
            user_hook(rid, data);
        })?;

        // Reassemble spilled indexed keys now that the scan's heap borrow is
        // released (the deleted rows' overflow chains are still intact — the
        // scan clears slots only, not chains).
        for (slot_i, stub, rid) in deferred {
            let bytes = heap.read_overflow_value(&stub).map_err(io::Error::from)?;
            let v = match schema.columns[indexed_cols[slot_i].col_idx].type_id {
                TypeId::Str => Value::Str(String::from_utf8_lossy(&bytes).into_owned()),
                TypeId::Bytes => Value::Bytes(bytes),
                TypeId::Json => Value::Json(bytes.into()),
                _ => continue,
            };
            entries_per_index[slot_i].push((v, rid));
        }

        for (slot_i, entry) in indexed_cols.iter_mut().enumerate() {
            for (v, rid) in &entries_per_index[slot_i] {
                if entry.unique {
                    entry.btree.delete(v);
                } else {
                    entry.btree.delete_non_unique(v, *rid);
                }
            }
        }
        // Blocker B3: btree dirty flags are set by `delete`; checkpoint
        // flushes later.
        Ok(count)
    }

    /// Single-pass fused scan + in-place patch. Evaluates `pred` on raw
    /// row bytes and applies `try_mutate` to each match on the same hot
    /// page — no second pass. Returns `(patched_count, fallback_rids)`.
    ///
    /// The `hook` closure fires after each successful patch with the
    /// post-mutation bytes, used for WAL logging.
    ///
    /// Perf sprint: this is the update analogue of
    /// `scan_delete_matching_with_hook`. Eliminates the two-pass
    /// collect-then-patch pattern that doubled `ensure_hot` calls for
    /// `update_by_filter`.
    pub fn scan_patch_matching_with_hook<P, M, H>(
        &mut self,
        pred: P,
        try_mutate: M,
        hook: H,
    ) -> io::Result<(u64, Vec<RowId>)>
    where
        P: FnMut(&[u8]) -> bool,
        M: FnMut(&mut [u8]) -> Option<u16>,
        H: FnMut(RowId, &[u8]),
    {
        // No index maintenance needed — callers guarantee the patched
        // columns are not indexed (same constraint as the per-rid
        // `with_row_bytes_mut` / `patch_var_col_in_place` fast paths).
        self.heap.scan_patch_matching(pred, try_mutate, hook)
    }

    /// Update a row in place when possible. Falls back to delete+insert only
    /// if the new encoding doesn't fit in the current slot.
    ///
    /// Mission D5: the previous implementation always did `delete + insert`,
    /// which:
    ///   1. read+wrote the page twice (once to clear the slot, once to fill it
    ///      again — usually on a different page),
    ///   2. did an O(N) scan over `pages_with_space` for every insert,
    ///   3. mutated every index even when the indexed column hadn't changed.
    ///
    /// On `update_by_filter` (50K matching rows, status-only update, no
    /// index on status) that turned ~1ms of work into 30 seconds — a
    /// catastrophic O(N²)-ish gap vs SQLite (6.7ms total). The fix is to
    /// (a) prefer `heap.update` which tries in-place first and (b) only
    /// touch indexes whose value actually changed.
    pub fn update(&mut self, rid: RowId, values: &Row) -> io::Result<RowId> {
        self.update_hinted(rid, values, None)
    }

    /// Same as `update`, but the caller can supply the set of column
    /// indices that actually changed. If supplied, the old-row read is
    /// skipped entirely when none of the changed columns is indexed.
    ///
    /// Mission C Phase 2: `update_by_filter` hits this path ~50K times with
    /// a single-column assignment (status) on a table whose only index is
    /// on `id`. The old code called `self.get(rid)` unconditionally — a
    /// heap read + full decode every time — even though the result was
    /// always thrown away for non-indexed updates. Skipping that read is
    /// worth ~300ns/row, or ~15ms on a 50K-row update_by_filter.
    pub fn update_hinted(
        &mut self,
        rid: RowId,
        values: &Row,
        changed_col_indices: Option<&[usize]>,
    ) -> io::Result<RowId> {
        // Size the new row first: if it exceeds the inline cap, an overflow
        // transition takes delete+insert of a v2 stub row (self-contained,
        // no WAL — the WAL path lives in `Catalog::update`). In-place patch
        // fast paths stay v1/inline-only (they never reach here for big rows).
        if crate::row::v1_encoded_len(&self.row_layout, values) > crate::page::MAX_ROW_DATA_SIZE {
            let encoded = self.encode_row_spilling(values)?;
            return self.apply_update(rid, values, &encoded, changed_col_indices);
        }
        encode_row_into_with_layout(
            &self.schema,
            &self.row_layout,
            values,
            &mut self.encode_scratch,
        );
        // Move the scratch out so `apply_update` can borrow `self` mutably for
        // the heap update without aliasing the scratch buffer.
        let encoded = std::mem::take(&mut self.encode_scratch);
        let result = self.apply_update(rid, values, &encoded, changed_col_indices);
        self.encode_scratch = encoded;
        result
    }

    /// Update a row with caller-supplied pre-encoded bytes (used by the
    /// catalog WAL path, which builds the v2 stub row and logs the overflow
    /// chains itself). Index maintenance uses the logical `values`.
    pub(crate) fn update_encoded(
        &mut self,
        rid: RowId,
        values: &Row,
        encoded: &[u8],
        changed_col_indices: Option<&[usize]>,
    ) -> io::Result<RowId> {
        self.apply_update(rid, values, encoded, changed_col_indices)
    }

    /// Shared core of the update paths: unique pre-check, heap update with the
    /// already-encoded bytes, and secondary-index maintenance from the logical
    /// `values`.
    fn apply_update(
        &mut self,
        rid: RowId,
        values: &Row,
        encoded: &[u8],
        changed_col_indices: Option<&[usize]>,
    ) -> io::Result<RowId> {
        let touches_index = if self.indexed_cols.is_empty() {
            false
        } else if let Some(changed) = changed_col_indices {
            self.indexed_cols
                .iter()
                .any(|c| changed.contains(&c.col_idx))
        } else {
            // No hint — fall back to the safe path that reads the old row.
            true
        };

        let old_row = if touches_index { self.get(rid) } else { None };

        // Unique constraint pre-check (before any heap mutation): a unique
        // column's new value must not already exist on a DIFFERENT row.
        // Updating a row to its own current value is always legal.
        if touches_index {
            for entry in &self.indexed_cols {
                if !entry.unique {
                    continue;
                }
                let new_val = &values[entry.col_idx];
                if new_val.is_empty() {
                    continue;
                }
                // No change to this column's value → cannot create a dup.
                if let Some(old) = old_row.as_ref() {
                    if &old[entry.col_idx] == new_val {
                        continue;
                    }
                }
                if let Some(existing_rid) = entry.btree.lookup(new_val) {
                    if existing_rid != rid {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "unique constraint violation on {}.{}",
                                self.schema.table_name, entry.col_name
                            ),
                        ));
                    }
                }
            }
        }

        let new_rid = self.heap.update(rid, encoded)?;

        // P0 (overflow relocation): a spill/unspill (or any grow) update turns
        // into heap delete+insert and hands back a NEW rid. When no indexed
        // column changed, `touches_index` is false and the block below is
        // skipped -- but every index entry still points at the OLD rid, so a
        // relocated row vanishes from all keyed access (point lookup, filtered
        // update/delete) and only an unfiltered scan finds it. Repoint every
        // index from `rid` -> `new_rid` using the (unchanged) current values,
        // which for an unchanged column equal the old key. The `touches_index`
        // path already handles relocation via its `new_rid == rid` guards.
        if !touches_index && new_rid != rid && !self.indexed_cols.is_empty() {
            for entry in self.indexed_cols.iter_mut() {
                let val = &values[entry.col_idx];
                if val.is_empty() {
                    continue;
                }
                if entry.unique {
                    entry.btree.delete(val);
                    entry.btree.insert(val.clone(), new_rid);
                } else {
                    entry.btree.delete_non_unique(val, rid);
                    entry.btree.insert_non_unique(val.clone(), new_rid);
                }
            }
        }

        if touches_index {
            // Mission C Phase 17: walk the Vec<IndexedCol> directly.
            // `col_idx` is already precomputed on each entry, so we
            // don't even re-probe schema.column_index here.
            for entry in self.indexed_cols.iter_mut() {
                let new_val = &values[entry.col_idx];
                let old_val_opt = old_row.as_ref().map(|r| &r[entry.col_idx]);

                if entry.unique {
                    if let Some(old_val) = old_val_opt {
                        if old_val == new_val && new_rid == rid {
                            continue;
                        }
                        if !old_val.is_empty() {
                            entry.btree.delete(old_val);
                        }
                    }
                    if !new_val.is_empty() {
                        entry.btree.insert(new_val.clone(), new_rid);
                    }
                } else {
                    // Non-unique: delete old composite, insert new composite.
                    if let Some(old_val) = old_val_opt {
                        if old_val == new_val && new_rid == rid {
                            continue;
                        }
                        if !old_val.is_empty() {
                            entry.btree.delete_non_unique(old_val, rid);
                        }
                    }
                    if !new_val.is_empty() {
                        entry.btree.insert_non_unique(new_val.clone(), new_rid);
                    }
                }
            }
        }
        // Blocker B3: any mutated btree is now dirty; checkpoint will
        // persist it. No per-row fsync on this hot path.
        Ok(new_rid)
    }

    /// Patch a row's raw bytes in place. Caller guarantees the mutation
    /// does not change the row's total length and does not touch any
    /// indexed column — indexes are NOT updated by this path.
    ///
    /// Mission C Phase 4: see `HeapFile::with_row_bytes_mut`. This is the
    /// primitive that backs the executor's single-column fixed-width
    /// update fast path.
    #[inline]
    pub fn with_row_bytes_mut<F>(&mut self, rid: RowId, f: F) -> io::Result<bool>
    where
        F: FnOnce(&mut [u8]),
    {
        self.heap.with_row_bytes_mut(rid, f)
    }

    /// Patch a single var-length column in place, shrinking the row when
    /// the new value is smaller than the old one. Returns `Ok(true)` on
    /// success, `Ok(false)` when the new value would grow the row or the
    /// slot is gone (caller should fall back to the full update path).
    ///
    /// The caller is responsible for ensuring no indexed column is
    /// touched by this patch — indexes are NOT maintained here.
    ///
    /// Mission C Phase 10: backs the executor's `update_by_filter` fast
    /// path for var-length single-column assignments.
    #[inline]
    pub fn patch_var_col_in_place(
        &mut self,
        rid: RowId,
        col_idx: usize,
        new_value: Option<&[u8]>,
    ) -> io::Result<bool> {
        let layout = &self.row_layout;
        self.heap.patch_row_shrink(rid, |bytes| {
            patch_var_column_in_place(bytes, layout, col_idx, new_value)
        })
    }

    /// Cached row layout for this table. Used by the executor to plan
    /// the byte-patch fast paths without re-walking the schema.
    #[inline]
    pub fn row_layout(&self) -> &RowLayout {
        &self.row_layout
    }

    /// Mission C Phase 15: does the given schema column index have an
    /// index attached? Used by the executor's update fast-path planner
    /// to decide whether a byte-patch update is safe (no index to
    /// maintain). Linear scan over `indexed_cols` — typically 1–3
    /// entries, so cheaper than a HashMap lookup by name.
    #[inline]
    pub fn has_indexed_col(&self, col_idx: usize) -> bool {
        self.indexed_cols.iter().any(|c| c.col_idx == col_idx)
    }

    /// A `is_indexed[col_idx]` mask over all schema columns, for
    /// [`crate::row::plan_spill`] so indexed columns are kept inline (see the
    /// P2 dangling-index-entry fix). Cheap: one bool vec, a handful of index
    /// entries walked.
    pub(crate) fn indexed_col_mask(&self) -> Vec<bool> {
        let mut mask = vec![false; self.schema.columns.len()];
        for entry in &self.indexed_cols {
            if entry.col_idx < mask.len() {
                mask[entry.col_idx] = true;
            }
        }
        mask
    }

    /// Heap on-disk format version. `>= HEAP_FORMAT_VERSION_WITH_OVERFLOW` (3)
    /// means the table has used overflow pages at least once, so it may hold
    /// v2 (spilled) rows. The executor uses this to route such tables away from
    /// the v1-only raw-byte fast paths (which cannot correctly read or patch a
    /// v2 row) and onto the reassembling decode paths.
    #[inline]
    pub fn format_version(&self) -> u16 {
        self.heap.format_version()
    }

    /// Whether this table may hold v2 (spilled) rows: true once its heap has
    /// ever written an overflow chain. The executor gates the v1-only raw-byte
    /// read/patch fast paths on this — a spilled table takes the reassembling
    /// decode paths instead (correct for values of any size, including the
    /// `>= 64KB` values that cannot be re-inlined into a u16 v1 row).
    #[inline]
    pub fn has_overflow_rows(&self) -> bool {
        self.heap.format_version() >= crate::heap::HEAP_FORMAT_VERSION_WITH_OVERFLOW
    }

    pub fn scan(&self) -> impl Iterator<Item = (RowId, Row)> + '_ {
        self.heap.scan().map(|(rid, data)| {
            if crate::row::row_is_v2(&data) {
                let row =
                    crate::row::decode_row_v2(&self.schema, &self.row_layout, &data, |stub| {
                        self.heap.read_overflow_value(stub).map_err(io::Error::from)
                    })
                    // A corrupt chain during a scan degrades to Empty cells rather
                    // than aborting the whole scan; `get` surfaces the typed error.
                    .unwrap_or_else(|_| vec![Value::Empty; self.schema.columns.len()]);
                (rid, row)
            } else {
                (rid, decode_row(&self.schema, &data))
            }
        })
    }

    /// Zero-copy scan that passes raw row bytes to the callback. v1/v0 rows
    /// are handed through untouched (zero copy). A v2 row is reassembled into
    /// an equivalent v1 (fully inline) row first — its spilled columns are
    /// fetched from the overflow chains — so every downstream consumer
    /// (`decode_row`, `decode_column`, compiled predicates) sees a v1 layout
    /// and needs no v2 awareness. Only the rare v2 rows pay the reassembly;
    /// v1 rows stay on the mmap zero-copy path. A row whose chain is corrupt
    /// is skipped (its typed error surfaces via `get`).
    pub fn for_each_row_raw<F>(&self, mut f: F)
    where
        F: FnMut(RowId, &[u8]),
    {
        let schema = &self.schema;
        let layout = &self.row_layout;
        let heap = &self.heap;
        heap.for_each_row(|rid, data| {
            if crate::row::row_is_v2(data) {
                if let Ok(v1) = crate::row::rehydrate_v2_to_v1(schema, layout, data, |stub| {
                    heap.read_overflow_value(stub).map_err(io::Error::from)
                }) {
                    f(rid, &v1);
                }
            } else {
                f(rid, data);
            }
        });
    }

    /// Zero-copy scan with early termination. The callback returns
    /// `ControlFlow::Break(())` to stop. Used by `Limit` fast paths. v2 rows
    /// are reassembled to v1 first (see [`Self::for_each_row_raw`]).
    pub fn try_for_each_row_raw<F>(&self, mut f: F)
    where
        F: FnMut(RowId, &[u8]) -> std::ops::ControlFlow<()>,
    {
        use std::ops::ControlFlow;
        let schema = &self.schema;
        let layout = &self.row_layout;
        let heap = &self.heap;
        heap.try_for_each_row(|rid, data| {
            if crate::row::row_is_v2(data) {
                match crate::row::rehydrate_v2_to_v1(schema, layout, data, |stub| {
                    heap.read_overflow_value(stub).map_err(io::Error::from)
                }) {
                    Ok(v1) => f(rid, &v1),
                    Err(_) => ControlFlow::Continue(()),
                }
            } else {
                f(rid, data)
            }
        });
    }

    pub fn index_lookup(&self, col_name: &str, key: &Value) -> Option<(RowId, Row)> {
        let entry = self.indexed_cols.iter().find(|c| c.col_name == col_name)?;
        if entry.unique {
            let rid = entry.btree.lookup(key)?;
            let row = self.get(rid)?;
            Some((rid, row))
        } else {
            // Non-unique: return the first match (for backwards compat).
            let rids = entry.btree.lookup_prefix(key);
            let rid = *rids.first()?;
            let row = self.get(rid)?;
            Some((rid, row))
        }
    }

    /// Look up ALL matching rows for a column value. For unique indexes
    /// this returns 0 or 1 results. For non-unique indexes this returns
    /// all rows whose indexed column equals `key`.
    pub fn index_lookup_all(&self, col_name: &str, key: &Value) -> Vec<RowId> {
        let entry = match self.indexed_cols.iter().find(|c| c.col_name == col_name) {
            Some(e) => e,
            None => return Vec::new(),
        };
        if entry.unique {
            match entry.btree.lookup(key) {
                Some(rid) => vec![rid],
                None => Vec::new(),
            }
        } else {
            entry.btree.lookup_prefix(key)
        }
    }

    /// Check if an index on the given column is unique.
    pub fn is_index_unique(&self, col_name: &str) -> Option<bool> {
        self.indexed_cols
            .iter()
            .find(|c| c.col_name == col_name)
            .map(|c| c.unique)
    }

    /// Create a non-unique secondary index on a column. Duplicate column
    /// values are supported via composite keys (column_value, RowId).
    pub fn create_index(&mut self, col_name: &str, data_dir: &Path) -> io::Result<()> {
        self.create_index_with_unique(col_name, data_dir, false)
    }

    /// Create an index on a column with an explicit uniqueness flag.
    /// `unique = true` creates a traditional unique index where duplicate
    /// key inserts overwrite (suitable for primary keys). `unique = false`
    /// creates a non-unique secondary index using composite keys.
    pub fn create_index_with_unique(
        &mut self,
        col_name: &str,
        data_dir: &Path,
        unique: bool,
    ) -> io::Result<()> {
        let col_idx = self
            .schema
            .column_index(col_name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "column not found"))?;

        // Mission C Phase 17: if this column already has an index,
        // no-op (matches the prior map.insert semantics of silently
        // replacing a duplicate, minus the wasted work).
        if self.indexed_cols.iter().any(|c| c.col_idx == col_idx) {
            return Ok(());
        }

        let idx_path = data_dir.join(format!("{}_{}.idx", self.schema.table_name, col_name));
        let mut btree = BTree::create(&idx_path)?;

        // Build index from existing data.
        for (rid, row) in self.scan() {
            if !row[col_idx].is_empty() {
                if unique {
                    btree.insert(row[col_idx].clone(), rid);
                } else {
                    btree.insert_non_unique(row[col_idx].clone(), rid);
                }
            }
        }

        // Mission 3: persist the freshly-built index so it survives a
        // restart. `BTree::create` stashed the path inside the tree, so
        // `save()` writes to the right place. Subsequent inserts / updates
        // / deletes will re-save after each mutation (see `save_if_touched`).
        btree.save()?;

        // Mission C Phase 17: store the btree inline alongside the
        // cached col_idx / col_name / is_int metadata — single tight
        // entry per index, walked directly by the hot write paths.
        let is_int = self.schema.columns[col_idx].type_id == TypeId::Int;
        self.indexed_cols.push(IndexedCol {
            col_idx,
            col_name: col_name.to_string(),
            is_int,
            unique,
            btree,
        });
        Ok(())
    }
}
