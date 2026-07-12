use crate::row::encode_row_into;
use crate::table::Table;
use crate::types::*;
use crate::wal::{Wal, WalDurabilityTicket, WalRecord, WalRecordType, WalSyncMode};
use rustc_hash::FxHashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Reject an encoded row that exceeds the single-page capacity BEFORE it is
/// appended to the WAL. The heap performs the same check at its own insert/
/// update boundary, but the update paths log to the WAL first — a logged
/// record whose row the heap then rejects would poison the next replay.
fn check_encoded_row_size(encoded: &[u8]) -> io::Result<()> {
    if encoded.len() > crate::page::MAX_ROW_DATA_SIZE {
        return Err(crate::error::StorageError::RowTooLarge {
            size: encoded.len(),
            max: crate::page::MAX_ROW_DATA_SIZE,
        }
        .into());
    }
    Ok(())
}

/// Validate that a name (table or column) is safe for use in file paths and
/// follows the identifier convention: starts with a letter or underscore,
/// followed by letters, digits, or underscores.
fn validate_identifier(kind: &str, name: &str) -> io::Result<()> {
    if name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {kind} name: must not be empty"),
        ));
    }
    let mut chars = name.chars();
    // Infallible: we returned early if `name.is_empty()` above.
    let first = chars.next().expect("non-empty name");
    if !first.is_ascii_alphabetic() && first != '_' {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {kind} name '{name}': must start with a letter or underscore"),
        ));
    }
    for ch in chars {
        if !ch.is_ascii_alphanumeric() && ch != '_' {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "invalid {kind} name '{name}': must contain only letters, digits, and underscores"
                ),
            ));
        }
    }
    Ok(())
}

/// Validate a table name for path safety.
fn validate_table_name(name: &str) -> io::Result<()> {
    validate_identifier("table", name)
}

/// Validate a column name for path safety.
fn validate_column_name(name: &str) -> io::Result<()> {
    validate_identifier("column", name)
}

/// On-disk catalog file: lists every table's schema so we can reopen them
/// after a restart. Format is a small custom binary blob (no serde dep).
///
/// Mission 3: version 2 appends a per-table list of indexed column names
/// after the column list, so indexes can be rehydrated on `Catalog::open`.
/// Version 1 files still load cleanly — they're treated as having zero
/// indexed columns, and the next `create_index` (or implicit rebuild on
/// first open, depending on the caller) will populate the list.
const CATALOG_FILE: &str = "catalog.bin";
pub const CATALOG_LSN_FILE: &str = "catalog.lsn";
const CATALOG_MAGIC: &[u8; 4] = b"BCAT";
/// Version 4 appends a per-table column-defaults section after the indexed
/// column list; version 5 appends an auto-increment column section after that.
/// Older files load cleanly (no defaults / no auto columns).
pub const CATALOG_VERSION: u16 = 5;

/// Mission 2 (durability): the single shared WAL file lives under the catalog's
/// data directory with this name. One WAL covers every table in the catalog.
const WAL_FILE: &str = "wal.log";
const SYNC_STATE_DIR: &str = ".powdb-sync";
const SYNC_IDENTITY_FILE: &str = "identity.json";

/// WAL batch size: flush auto-triggers after this many records, in addition
/// to the explicit `wal.flush()` each top-level mutation does. Kept small so
/// the tests see a predictable amount of buffering.
const WAL_BATCH_SIZE: usize = 64;
type WalArchiveCallback<'a> = &'a mut dyn FnMut(&Path, &[WalRecord]) -> io::Result<()>;

fn read_durable_lsn(data_dir: &Path) -> io::Result<u64> {
    let path = data_dir.join(CATALOG_LSN_FILE);
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };
    if bytes.len() != 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "catalog LSN sidecar has invalid length",
        ));
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes);
    Ok(u64::from_le_bytes(buf))
}

fn write_durable_lsn(data_dir: &Path, lsn: u64) -> io::Result<()> {
    let path = data_dir.join(CATALOG_LSN_FILE);
    let tmp_path = data_dir.join(format!("{CATALOG_LSN_FILE}.tmp"));
    let mut file = fs::File::create(&tmp_path)?;
    file.write_all(&lsn.to_le_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp_path, &path)?;
    sync_directory(data_dir)?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(path: &Path) -> io::Result<()> {
    let _ = path;
    Ok(())
}

fn max_record_lsn(records: &[WalRecord]) -> Option<u64> {
    records.iter().map(|record| record.lsn).max()
}

/// System catalog: registry of all tables.
///
/// Mission C Phase 18: tables live in a `Vec<Table>` addressed by a
/// stable `slot` index, with a parallel `FxHashMap<String, usize>` for
/// name-based resolution. Append-only (PowDB has no DROP TABLE yet), so
/// slots are stable for the lifetime of the `Catalog` — callers like
/// `PreparedQuery::insert_fast` cache a slot at prepare time and skip
/// the name probe on every subsequent `execute_prepared_take`.
///
/// Earlier design (pre-Phase 18) held tables in a `FxHashMap<String, Table>`
/// directly. That meant the `insert_batch_1k` hot path paid an
/// `FxHash("User")` + bucket walk per row just to dispatch into the
/// table — about 20-40ns out of a 233ns budget.
pub struct Catalog {
    /// All tables, in insertion order. Indexed by `slot: usize`. A table's
    /// slot is assigned by `create_table`/`open` and never reused.
    tables: Vec<Table>,
    /// Name → slot index. Populated in sync with `tables` on every
    /// `create_table` / `open`.
    name_to_slot: FxHashMap<String, usize>,
    data_dir: PathBuf,
    /// Mission 2: shared write-ahead log owned by the catalog. Every
    /// mutation (insert/update/delete) records its intent here BEFORE
    /// touching the heap so a mid-write crash can be recovered from on the
    /// next open. Flushed to disk at the end of every top-level op.
    wal: Wal,
    /// Monotonic transaction-id counter. Autocommit statements may allocate
    /// multiple ids (one per row-level primitive), while explicit transactions
    /// reuse one id for the whole BEGIN..COMMIT scope.
    next_tx_id: u64,
    /// Active explicit transaction id, if any. Owned by the connection/session
    /// driving this catalog through `Engine`.
    active_tx_id: Option<u64>,
    /// Durable WAL byte offset captured at BEGIN. ROLLBACK truncates back to
    /// this boundary so auto-flushed uncommitted records cannot replay later.
    tx_start_len: Option<u64>,
    /// Autocommit row-mutation tx ids appended since the previous group commit.
    /// `commit_autocommit` writes commit markers for these ids before fsync.
    pending_autocommit_tx_ids: Vec<u64>,
    /// Has this catalog been cleanly checkpointed at least once since it
    /// was opened? Used by `Drop` to decide whether to treat its own flush
    /// as fatal (it isn't — we still try best-effort).
    checkpointed: bool,
    /// Catalog-level durable LSN. Heap page LSNs cover row mutations, but
    /// DDL-only changes can advance the WAL without touching a data page.
    durable_lsn: u64,
}

impl Catalog {
    /// Create a brand-new catalog. Wipes any existing catalog file in this directory.
    ///
    /// # Examples
    ///
    /// ```
    /// use powdb_storage::catalog::Catalog;
    /// use powdb_storage::types::{Schema, ColumnDef, TypeId};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let mut catalog = Catalog::create(dir.path()).unwrap();
    ///
    /// let schema = Schema {
    ///     table_name: "User".to_string(),
    ///     columns: vec![
    ///         ColumnDef { name: "name".to_string(), type_id: TypeId::Str, required: true, position: 0 },
    ///         ColumnDef { name: "age".to_string(), type_id: TypeId::Int, required: false, position: 1 },
    ///     ],
    /// };
    /// catalog.create_table(schema).unwrap();
    /// ```
    pub fn create(data_dir: &Path) -> io::Result<Self> {
        crate::create_data_dir_secure(data_dir)?;
        let wal_path = data_dir.join(WAL_FILE);
        let wal = Wal::create(&wal_path, WAL_BATCH_SIZE)?;
        let cat = Catalog {
            tables: Vec::new(),
            name_to_slot: FxHashMap::default(),
            data_dir: data_dir.to_path_buf(),
            wal,
            next_tx_id: 1,
            active_tx_id: None,
            tx_start_len: None,
            pending_autocommit_tx_ids: Vec::new(),
            checkpointed: false,
            durable_lsn: 0,
        };
        cat.persist()?;
        Ok(cat)
    }

    /// Open an existing catalog from disk, rehydrating every table. If no
    /// catalog file is present this returns NotFound — callers can fall back
    /// to `create` for a fresh data dir.
    ///
    /// Mission 2: after the per-table heap files are reopened, this replays
    /// any records left in the WAL from a previous (crashed) session. The
    /// WAL is then truncated once the replay lands cleanly on disk — that
    /// re-establishes the "empty WAL = last shutdown was clean" invariant.
    pub fn open(data_dir: &Path) -> io::Result<Self> {
        Self::open_inner(data_dir, None)
    }

    /// Open an existing catalog and archive any replayed WAL records before
    /// recovery truncates the WAL. This is for sync-aware callers that must
    /// retain history needed by replicas.
    ///
    /// Replication boundary: this hook exists so `powdb-sync` can preserve WAL
    /// history before storage recovery truncates it. Ordinary embedded/server
    /// callers should use `open`; do not build application-level recovery flows
    /// directly on this hook.
    pub fn open_with_wal_archive<F>(data_dir: &Path, mut archive: F) -> io::Result<Self>
    where
        F: FnMut(&Path, &[WalRecord]) -> io::Result<()>,
    {
        let archive: WalArchiveCallback<'_> = &mut archive;
        Self::open_inner(data_dir, Some(archive))
    }

    fn open_inner(data_dir: &Path, archive: Option<WalArchiveCallback<'_>>) -> io::Result<Self> {
        let cat_path = data_dir.join(CATALOG_FILE);
        if !cat_path.exists() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no catalog file"));
        }
        let entries = read_catalog_file(&cat_path)?;
        let durable_lsn = read_durable_lsn(data_dir)?;
        let mut tables: Vec<Table> = Vec::with_capacity(entries.len());
        let mut name_to_slot =
            FxHashMap::with_capacity_and_hasher(entries.len(), Default::default());
        for CatalogEntry {
            schema,
            indexed_cols,
            defaults,
            auto_cols,
        } in entries
        {
            let name = schema.table_name.clone();
            // Mission 3: rehydrate persisted indexes. `Table::open_with_indexes`
            // tries to `BTree::load` each named index file; if a file is
            // missing (e.g. first open after upgrade from catalog v1) it
            // falls back to rebuilding from the heap scan and saving to
            // disk so subsequent opens hit the fast path.
            let mut table = Table::open_with_indexes(schema, data_dir, &indexed_cols)?;
            table.set_defaults(defaults);
            table.set_auto_cols(auto_cols);
            name_to_slot.insert(name, tables.len());
            tables.push(table);
        }
        let wal_path = data_dir.join(WAL_FILE);
        let wal = Wal::open(&wal_path, WAL_BATCH_SIZE)?;
        let mut cat = Catalog {
            tables,
            name_to_slot,
            data_dir: data_dir.to_path_buf(),
            wal,
            next_tx_id: 1,
            active_tx_id: None,
            tx_start_len: None,
            pending_autocommit_tx_ids: Vec::new(),
            checkpointed: false,
            durable_lsn,
        };
        cat.replay_wal(archive)?;
        // Restore WAL LSN monotonicity across the restart. Heap pages carry
        // LSNs stamped by replay (catalog.rs set_page_lsn) and by DDL
        // rewrites (stamp_all_pages_min_lsn), but `Wal::open` reset the
        // counter to 1. If the next write reused an LSN <= a stamped page
        // LSN, the following crash's replay would skip it as already-applied
        // — the data-loss bug behind the v0.4.x yanks. This runs on every
        // open (including the empty-WAL clean-shutdown path, where pages may
        // still carry LSNs from an earlier recovery). LSNs must be monotonic
        // across restarts.
        let max_page_lsn = cat
            .tables
            .iter()
            .map(|t| t.heap.max_page_lsn())
            .max()
            .unwrap_or(0);
        let max_known_lsn = max_page_lsn.max(cat.durable_lsn);
        cat.wal.set_next_lsn_at_least(max_known_lsn + 1);
        Ok(cat)
    }

    /// Replay every record currently buffered in the WAL file onto the open
    /// tables. This is the recovery path: after a crash the heap files on
    /// disk may be missing mutations that were logged to the WAL but never
    /// written back to their pages. We re-apply every record unconditionally.
    ///
    /// **Idempotence:**
    /// - `Delete`: idempotent — `HeapFile::delete` on an already-deleted or
    ///   missing slot is a no-op.
    /// - `Update`: idempotent — re-applies the same new row bytes to the
    ///   same `RowId`, which either replaces the existing (already-updated)
    ///   row with itself or lands the update for the first time.
    /// - `Insert`: **NOT strictly idempotent**. `HeapFile::insert` allocates
    ///   a fresh `RowId` on every call, so a row that was already flushed
    ///   to disk will be re-inserted at a new location, producing a
    ///   duplicate. See the mission report for the full caveat.
    ///
    /// The practical consequences are:
    ///   1. On a "pure crash" (no heap pages ever flushed between open and
    ///      crash), replay cleanly restores every logged row.
    ///   2. On a crash where some heap pages were flushed by the hot-page
    ///      eviction logic, replay may restore those rows a second time.
    ///      A future mission can fix this with LSN-tagged pages.
    ///
    /// After a successful replay we truncate the WAL so the next shutdown
    /// (crash or otherwise) replays only the NEW records.
    fn replay_wal(&mut self, mut archive: Option<WalArchiveCallback<'_>>) -> io::Result<()> {
        let records = self.wal.read_all()?;
        if records.is_empty() {
            return Ok(());
        }
        if archive.is_none() {
            self.ensure_plain_wal_truncate_allowed(&records)?;
        }
        self.replay_records(&records)?;
        if let Some(archive) = archive.as_mut() {
            archive(&self.data_dir, &records)?;
        }
        self.wal.truncate()?;
        Ok(())
    }

    /// Apply an LSN-preserving WAL record stream without appending it to the
    /// local WAL. Sync callers must validate lineage and contiguity before
    /// calling this method.
    ///
    /// Replication boundary: this is a storage adapter for `powdb-sync`, not a
    /// general mutation API. Callers must reject unsupported record classes,
    /// hold their own replica progress state, and pass only contiguous,
    /// transaction-complete ranges or chunks.
    pub fn apply_wal_records(&mut self, records: &[WalRecord]) -> io::Result<()> {
        self.ensure_no_active_transaction_for_checkpoint()?;
        self.ensure_no_pending_wal_records()?;
        self.replay_records(records)
    }

    /// Sync callers use this before deciding an apply is a no-op. A replica with
    /// local WAL history is divergent until a higher layer explicitly repairs it.
    pub fn ensure_no_pending_wal_records(&self) -> io::Result<()> {
        if self.wal.has_pending() || !self.wal.read_all()?.is_empty() {
            return Err(io::Error::other(
                "cannot apply replicated WAL records while local WAL records are pending",
            ));
        }
        Ok(())
    }

    fn replay_records(&mut self, records: &[WalRecord]) -> io::Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        info!(count = records.len(), "applying WAL records");

        // Per-page LSN redo (ARIES-style). A record is already durable iff
        // its *target page* carries an LSN >= the record's LSN. The previous
        // implementation used a single per-table max LSN, which is unsafe:
        // a low-LSN record on an unflushed page would be wrongly skipped
        // because some other, flushed page of the same table advertised a
        // higher LSN — silently dropping the record (one of the v0.4.x
        // data-loss bugs). Every record now carries its real RowId (inserts
        // included), so the target page is always known.
        let has_boundaries = records.iter().any(|rec| {
            matches!(
                rec.record_type,
                WalRecordType::Begin | WalRecordType::Commit | WalRecordType::Rollback
            )
        });
        let mut committed_row_records = vec![true; records.len()];
        if has_boundaries {
            committed_row_records.fill(false);
            let mut pending_tx_spans: Vec<(u64, Vec<usize>)> = Vec::new();
            for (index, rec) in records.iter().enumerate() {
                match rec.record_type {
                    WalRecordType::Insert | WalRecordType::Update | WalRecordType::Delete
                        if rec.tx_id == 0 =>
                    {
                        committed_row_records[index] = true;
                    }
                    WalRecordType::Insert | WalRecordType::Update | WalRecordType::Delete => {
                        if let Some((_, rows)) = pending_tx_spans
                            .iter_mut()
                            .rev()
                            .find(|(tx_id, _)| *tx_id == rec.tx_id)
                        {
                            rows.push(index);
                        } else {
                            pending_tx_spans.push((rec.tx_id, vec![index]));
                        }
                    }
                    WalRecordType::Begin if rec.tx_id != 0 => {
                        pending_tx_spans.push((rec.tx_id, Vec::new()));
                    }
                    WalRecordType::Commit if rec.tx_id != 0 => {
                        if let Some(span_index) = pending_tx_spans
                            .iter()
                            .rposition(|(tx_id, _)| *tx_id == rec.tx_id)
                        {
                            let (_, rows) = pending_tx_spans.remove(span_index);
                            for row_index in rows {
                                committed_row_records[row_index] = true;
                            }
                        }
                    }
                    WalRecordType::Rollback if rec.tx_id != 0 => {
                        if let Some(span_index) = pending_tx_spans
                            .iter()
                            .rposition(|(tx_id, _)| *tx_id == rec.tx_id)
                        {
                            pending_tx_spans.remove(span_index);
                        }
                    }
                    _ => {}
                }
            }
        }

        let mut replayed_inserts = 0usize;
        let mut replayed_updates = 0usize;
        let mut replayed_deletes = 0usize;
        let mut skipped = 0usize;
        let mut skipped_uncommitted = 0usize;
        let mut saw_ddl = false;
        for (index, rec) in records.iter().enumerate() {
            if has_boundaries
                && !committed_row_records[index]
                && matches!(
                    rec.record_type,
                    WalRecordType::Insert | WalRecordType::Update | WalRecordType::Delete
                )
            {
                skipped_uncommitted += 1;
                continue;
            }
            match rec.record_type {
                WalRecordType::Insert => {
                    if let Some((table_name, rid, row_bytes)) = decode_wal_payload(&rec.data) {
                        if let Some(slot) = self.name_to_slot.get(&table_name).copied() {
                            let tbl = &mut self.tables[slot];
                            // Already persisted on its page? Skip — re-running
                            // the insert would allocate a fresh slot and
                            // duplicate the row.
                            if rec.lsn > 0 && tbl.heap.page_lsn(rid.page_id) >= rec.lsn {
                                skipped += 1;
                                continue;
                            }
                            // Not yet durable: place the row at its exact
                            // logged RowId so later Update/Delete records
                            // (which carry that RowId) stay correctly
                            // targeted. A plain re-`insert` would self-assign
                            // a fresh slot whose position can diverge from the
                            // original after a partial-flush crash.
                            tbl.heap.insert_at(rid, &row_bytes)?;
                            tbl.heap.set_page_lsn(rid.page_id, rec.lsn)?;
                            replayed_inserts += 1;
                        }
                    }
                }
                WalRecordType::Update => {
                    if let Some((table_name, rid, row_bytes)) = decode_wal_payload(&rec.data) {
                        if let Some(slot) = self.name_to_slot.get(&table_name).copied() {
                            let tbl = &mut self.tables[slot];
                            if rec.lsn > 0 && tbl.heap.page_lsn(rid.page_id) >= rec.lsn {
                                skipped += 1;
                                continue;
                            }
                            let new_rid = tbl.heap.update(rid, &row_bytes)?;
                            tbl.heap.set_page_lsn(new_rid.page_id, rec.lsn)?;
                            replayed_updates += 1;
                        }
                    }
                }
                WalRecordType::Delete => {
                    if let Some((table_name, rid, _)) = decode_wal_payload(&rec.data) {
                        if let Some(slot) = self.name_to_slot.get(&table_name).copied() {
                            let tbl = &mut self.tables[slot];
                            if rec.lsn > 0 && tbl.heap.page_lsn(rid.page_id) >= rec.lsn {
                                skipped += 1;
                                continue;
                            }
                            let _ = tbl.heap.delete(rid);
                            tbl.heap.set_page_lsn(rid.page_id, rec.lsn)?;
                            replayed_deletes += 1;
                        }
                    }
                }
                WalRecordType::Begin | WalRecordType::Commit | WalRecordType::Rollback => {
                    // Boundary records were consumed in the first pass.
                }
                WalRecordType::DdlCreateTable => {
                    saw_ddl = true;
                    if let Some((schema, defaults, auto_cols)) = decode_ddl_create_table(&rec.data)
                    {
                        if !self.name_to_slot.contains_key(&schema.table_name) {
                            if let Ok(mut table) = Table::create(schema, &self.data_dir) {
                                table.set_defaults(defaults);
                                table.set_auto_cols(auto_cols);
                                let slot = self.tables.len();
                                let name = table.schema.table_name.clone();
                                self.tables.push(table);
                                self.name_to_slot.insert(name, slot);
                            }
                        }
                    }
                }
                WalRecordType::DdlDropTable => {
                    saw_ddl = true;
                    if let Some((table_name, _)) = decode_ddl_table_name(&rec.data) {
                        if let Some(&slot) = self.name_to_slot.get(&table_name) {
                            let heap_path = self.data_dir.join(format!("{table_name}.heap"));
                            if heap_path.exists() {
                                let _ = fs::remove_file(&heap_path);
                            }
                            for col_name in self.tables[slot].indexed_column_names() {
                                let idx_path =
                                    self.data_dir.join(format!("{table_name}_{col_name}.idx"));
                                if idx_path.exists() {
                                    let _ = fs::remove_file(&idx_path);
                                }
                            }
                            self.name_to_slot.remove(&table_name);
                            let last = self.tables.len() - 1;
                            if slot != last {
                                let moved_name = self.tables[last].schema.table_name.clone();
                                self.tables.swap(slot, last);
                                self.name_to_slot.insert(moved_name, slot);
                            }
                            self.tables.pop();
                        }
                    }
                }
                WalRecordType::DdlAddColumn => {
                    saw_ddl = true;
                    if let Some((table_name, col)) = decode_ddl_alter_add_column(&rec.data) {
                        if let Some(&slot) = self.name_to_slot.get(&table_name) {
                            let tbl = &mut self.tables[slot];
                            if !tbl.schema.columns.iter().any(|c| c.name == col.name) {
                                let old_schema = tbl.schema.clone();
                                let has_rows = tbl.heap.scan().next().is_some();
                                tbl.schema.columns.push(col);
                                tbl.refresh_layout();
                                if has_rows {
                                    let fill = vec![Value::Empty; tbl.schema.columns.len()];
                                    let data_dir = self.data_dir.clone();
                                    let _ = tbl.rewrite_rows_for_schema_change(
                                        &old_schema,
                                        &fill,
                                        &data_dir,
                                    );
                                }
                            }
                            // Stamp every page with the DDL's LSN so a
                            // subsequent restart's per-page check skips the
                            // pre-DDL Insert/Update/Delete records — they
                            // have already been folded into the new layout
                            // by the rewrite above. See
                            // `stamp_all_pages_min_lsn` doc.
                            if rec.lsn > 0 {
                                let _ = tbl.heap.stamp_all_pages_min_lsn(rec.lsn);
                            }
                        }
                    }
                }
                WalRecordType::DdlDropColumn => {
                    saw_ddl = true;
                    if let Some((table_name, col_name)) = decode_ddl_alter_drop_column(&rec.data) {
                        if let Some(&slot) = self.name_to_slot.get(&table_name) {
                            let tbl = &mut self.tables[slot];
                            if let Some(idx) =
                                tbl.schema.columns.iter().position(|c| c.name == col_name)
                            {
                                let old_schema = tbl.schema.clone();
                                let has_rows = tbl.heap.scan().next().is_some();
                                tbl.schema.columns.remove(idx);
                                for (i, c) in tbl.schema.columns.iter_mut().enumerate() {
                                    c.position = i as u16;
                                }
                                tbl.refresh_layout();
                                if has_rows {
                                    let fill = vec![Value::Empty; tbl.schema.columns.len()];
                                    let data_dir = self.data_dir.clone();
                                    let _ = tbl.rewrite_rows_for_schema_change(
                                        &old_schema,
                                        &fill,
                                        &data_dir,
                                    );
                                }
                            }
                            if rec.lsn > 0 {
                                let _ = tbl.heap.stamp_all_pages_min_lsn(rec.lsn);
                            }
                        }
                    }
                }
            }
        }
        info!(
            inserts = replayed_inserts,
            updates = replayed_updates,
            deletes = replayed_deletes,
            skipped = skipped,
            skipped_uncommitted = skipped_uncommitted,
            "WAL record apply complete (commit-boundary + LSN idempotent)"
        );
        if saw_ddl {
            self.persist()?;
        }
        // Persist the replayed changes to disk before truncating the WAL,
        // otherwise a crash between here and the next checkpoint would lose
        // the replayed records. `flush_all_dirty` on every heap moves every
        // dirty page through the normal write path.
        //
        // Blocker B3: under the deferred-index-save model, the on-disk
        // `.idx` files may lag the heap because the pre-crash session
        // never got to its next `checkpoint`. Replay restored the
        // heap rows above, but the btrees that loaded from those
        // possibly-stale `.idx` files don't know about them. Rebuild
        // every secondary index from the post-replay heap so the
        // trees exactly match disk. The rebuild is O(heap) per
        // indexed column, which is fine on a crash-recovery path.
        for tbl in &mut self.tables {
            tbl.heap.flush_all_dirty()?;
            tbl.heap.flush()?;
            tbl.rebuild_indexes_from_heap()?;
            // Flush the rebuilt indexes now so a crash between here
            // and the next mutation still leaves `.idx` files matching
            // the heap. Without this, a second crash before any
            // insert could leave us back where we started.
            tbl.save_dirty_indexes()?;
        }
        if let Some(max_lsn) = max_record_lsn(records) {
            self.record_durable_lsn_at_least(max_lsn)?;
            self.wal.set_next_lsn_at_least(max_lsn.saturating_add(1));
        }
        Ok(())
    }

    /// Flush every dirty heap page and truncate the WAL. This is the
    /// "clean shutdown" point — after this returns, the on-disk heap files
    /// are fully consistent and the WAL is empty, so the next `open` will
    /// skip replay entirely.
    ///
    /// Safe to call multiple times. Safe to call on a catalog that has
    /// performed zero mutations since the last checkpoint (in which case
    /// the flushes are no-ops and the truncate is a bounded syscall).
    pub fn checkpoint(&mut self) -> io::Result<()> {
        self.ensure_no_active_transaction_for_checkpoint()?;
        self.ensure_plain_checkpoint_allowed_before_flush()?;
        self.flush_checkpoint_state()?;
        self.wal.flush()?;
        self.record_durable_lsn_at_least(self.wal.last_appended_lsn())?;
        self.wal.truncate()?;
        self.checkpointed = true;
        Ok(())
    }

    /// Flush every dirty heap page, archive retained WAL records, then
    /// truncate the WAL. Sync-aware callers use this to make archive-before-
    /// truncate explicit without making storage depend on the sync crate.
    ///
    /// Replication boundary: this hook is for retained-history publication.
    /// It should stay behind sync-aware lifecycle helpers rather than becoming
    /// an ordinary checkpoint surface for application code.
    pub fn checkpoint_with_wal_archive<F>(&mut self, mut archive: F) -> io::Result<()>
    where
        F: FnMut(&Path, &[WalRecord]) -> io::Result<()>,
    {
        self.ensure_no_active_transaction_for_checkpoint()?;
        self.commit_autocommit()?;
        self.flush_checkpoint_state()?;
        self.wal.flush()?;
        let records = self.wal.read_all()?;
        let archive: WalArchiveCallback<'_> = &mut archive;
        archive(&self.data_dir, &records)?;
        if let Some(max_lsn) = max_record_lsn(&records) {
            self.record_durable_lsn_at_least(max_lsn)?;
        } else {
            self.record_durable_lsn_at_least(self.wal.last_appended_lsn())?;
        }
        self.wal.truncate()?;
        self.checkpointed = true;
        Ok(())
    }

    fn ensure_no_active_transaction_for_checkpoint(&self) -> io::Result<()> {
        if self.active_tx_id.is_some() {
            return Err(io::Error::other(
                "cannot checkpoint while an explicit transaction is active",
            ));
        }
        Ok(())
    }

    fn flush_checkpoint_state(&mut self) -> io::Result<()> {
        for tbl in &mut self.tables {
            tbl.heap.flush_all_dirty()?;
            tbl.heap.flush()?;
            // Blocker B3: the hot insert/update/delete paths no longer
            // fsync index files per row — they only mark the in-memory
            // btree dirty. Checkpoint is where those deferred saves
            // actually hit disk. Clean (non-dirty) indexes are free.
            tbl.save_dirty_indexes()?;
        }
        Ok(())
    }

    fn ensure_plain_checkpoint_allowed_before_flush(&self) -> io::Result<()> {
        if !self.sync_identity_file_exists() {
            return Ok(());
        }
        if self.wal.has_pending() {
            return Err(io::Error::other(
                "sync identity exists but checkpoint/recovery was called without a WAL archive hook; refusing to truncate retained history",
            ));
        }
        let records = self.wal.read_all()?;
        self.ensure_plain_wal_truncate_allowed(&records)
    }

    fn ensure_plain_wal_truncate_allowed(&self, records: &[WalRecord]) -> io::Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        if self.sync_identity_file_exists() {
            return Err(io::Error::other(
                "sync identity exists but checkpoint/recovery was called without a WAL archive hook; refusing to truncate retained history",
            ));
        }
        Ok(())
    }

    fn sync_identity_file_exists(&self) -> bool {
        self.data_dir
            .join(SYNC_STATE_DIR)
            .join(SYNC_IDENTITY_FILE)
            .exists()
    }

    fn record_durable_lsn_at_least(&mut self, lsn: u64) -> io::Result<()> {
        if lsn <= self.durable_lsn {
            return Ok(());
        }
        self.durable_lsn = lsn;
        write_durable_lsn(&self.data_dir, lsn)
    }

    /// Allocate or return the transaction id for the current mutation.
    #[inline]
    fn next_tx(&mut self) -> u64 {
        if let Some(id) = self.active_tx_id {
            return id;
        }
        let id = self.next_tx_id;
        self.next_tx_id = self.next_tx_id.wrapping_add(1);
        id
    }

    /// Begin a connection/session-scoped explicit transaction.
    pub fn begin_transaction(&mut self) -> io::Result<()> {
        if self.active_tx_id.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "explicit transaction is already active",
            ));
        }
        let start_len = self.wal.synced_len()?;
        let id = self.next_tx_id;
        self.next_tx_id = self.next_tx_id.wrapping_add(1);
        self.active_tx_id = Some(id);
        self.tx_start_len = Some(start_len);
        self.pending_autocommit_tx_ids.clear();
        if !self.wal.is_off() {
            self.wal.append(id, WalRecordType::Begin, &[])?;
            self.wal.flush()?;
        }
        Ok(())
    }

    /// Commit the active explicit transaction by appending a durable boundary
    /// marker after its row records.
    pub fn commit_transaction(&mut self) -> io::Result<()> {
        if let Some(id) = self.active_tx_id.take() {
            if !self.wal.is_off() {
                self.wal.append(id, WalRecordType::Commit, &[])?;
                self.wal.flush()?;
            }
        }
        self.tx_start_len = None;
        Ok(())
    }

    /// Commit any autocommit row mutations accumulated by the current
    /// statement. Pure reads/DDL have no pending tx ids and fall through to a
    /// cheap WAL flush/no-op.
    pub fn commit_autocommit(&mut self) -> io::Result<()> {
        if !self.wal.is_off() && !self.pending_autocommit_tx_ids.is_empty() {
            self.pending_autocommit_tx_ids.sort_unstable();
            self.pending_autocommit_tx_ids.dedup();
            for id in self.pending_autocommit_tx_ids.drain(..) {
                self.wal.append(id, WalRecordType::Commit, &[])?;
            }
        }
        self.wal.flush()
    }

    /// Append a mutation record to the WAL buffer. **Does not flush.**
    ///
    /// Mission B (post-review): per-row `wal.flush()` was a ~1ms fsync on
    /// every mutation, turning `update_by_filter` into a ~19s workload.
    /// The flush is now deferred to [`Self::sync_wal`], which the executor
    /// calls exactly once at the end of every mutating statement. This
    /// gives us statement-level group commit: N-row updates pay one fsync,
    /// not N.
    ///
    /// Durability contract: any path that observes `Ok(...)` back from
    /// the executor must have called `sync_wal` before returning that
    /// Ok. Replay is still correct because WAL records are appended in
    /// order and only records that reached `fdatasync`ed bytes are
    /// replayed.
    fn wal_log(
        &mut self,
        tx_id: u64,
        record_type: WalRecordType,
        table: &str,
        rid: RowId,
        row_bytes: &[u8],
    ) -> io::Result<()> {
        // Mission B (post-review, second pass): when the WAL is in Off
        // mode the `append` call below is a no-op, so building the
        // payload first wastes a `Vec` allocation + ~3 extends per
        // mutation. The catalog hot paths check `wal.is_off()` before
        // calling here, but this guard is the belt-and-braces version
        // for any internal caller that doesn't.
        if self.wal.is_off() {
            return Ok(());
        }
        let payload = encode_wal_payload(table, rid, row_bytes);
        self.wal.append(tx_id, record_type, &payload)?;
        if self.active_tx_id.is_none() {
            self.pending_autocommit_tx_ids.push(tx_id);
        }
        Ok(())
    }

    /// Flush any buffered WAL records to disk. Called by the executor
    /// at the end of every mutating statement so the group-commit
    /// window is exactly one statement.
    ///
    /// See `Self::wal_log` for the durability contract.
    #[inline]
    pub fn sync_wal(&mut self) -> io::Result<()> {
        self.wal.flush()
    }

    /// Set the WAL sync mode. Production code should leave this at the
    /// default ([`WalSyncMode::Full`]). Benchmarks set it to
    /// [`WalSyncMode::Off`] to compare apples-to-apples against
    /// `:memory:` SQLite (which has zero fsync cost).
    ///
    /// **Never** call this with `Off` in production — a machine crash
    /// can lose any record written since the last `sync_wal` returned.
    pub fn set_wal_sync_mode(&mut self, mode: WalSyncMode) {
        self.wal.set_sync_mode(mode);
    }

    /// Defer Full-mode commit fsyncs (WAL group commit). While enabled, the
    /// commit paths register the WAL generation they need durable instead of
    /// fsyncing inline; the pending claim is retrieved with
    /// [`Self::take_wal_durability_ticket`] and the caller must wait on it
    /// before acknowledging the statement. This lets the fsync leave the
    /// engine's exclusive-lock hold so overlapping committers can share one
    /// fsync. `Normal`/`Off` modes are unaffected.
    pub fn set_wal_sync_deferred(&mut self, defer: bool) {
        self.wal.set_defer_sync(defer);
    }

    /// Take the durability claim registered by deferred commit flushes since
    /// the last take, if any. See [`Self::set_wal_sync_deferred`].
    pub fn take_wal_durability_ticket(&mut self) -> Option<WalDurabilityTicket> {
        self.wal.take_durability_ticket()
    }

    /// Number of fsyncs issued against the WAL (test/metrics hook).
    pub fn wal_fsync_count(&self) -> u64 {
        self.wal.fsync_count()
    }

    /// Discard in-memory mutations made since the last `sync_wal()` and
    /// restore the catalog to its on-disk state. Used by ROLLBACK to
    /// undo an in-progress transaction's changes.
    ///
    /// This re-opens the catalog from the checkpoint file and replays
    /// only the durable (already flushed) WAL records. Any WAL records
    /// that were appended but not yet flushed are lost.
    ///
    /// **Critical**: before replacing `*self` we must discard every
    /// dirty in-memory page across all heaps. Otherwise the old
    /// `Catalog`'s `Drop` impl calls `checkpoint()` which flushes those
    /// dirty pages to disk — and the freshly-opened replacement catalog
    /// would then read the flushed (uncommitted) rows back, defeating
    /// the entire rollback.
    pub fn rollback_to_last_sync(&mut self) -> io::Result<()> {
        self.rollback_to_last_sync_inner(None)
    }

    /// Roll back the active transaction, then reopen/replay any remaining WAL
    /// through an archive hook before recovery truncates it. Sync-aware callers
    /// use this when committed pre-transaction records must remain available to
    /// replicas after rollback.
    pub fn rollback_to_last_sync_with_wal_archive<F>(&mut self, mut archive: F) -> io::Result<()>
    where
        F: FnMut(&Path, &[WalRecord]) -> io::Result<()>,
    {
        let archive: WalArchiveCallback<'_> = &mut archive;
        self.rollback_to_last_sync_inner(Some(archive))
    }

    fn rollback_to_last_sync_inner(
        &mut self,
        mut archive: Option<WalArchiveCallback<'_>>,
    ) -> io::Result<()> {
        let start_len = self.tx_start_len.unwrap_or(0);
        let prearchived = if let Some(archive) = archive.as_mut() {
            let records = self.wal.read_through_len(start_len)?;
            if !records.is_empty() {
                archive(&self.data_dir, &records)?;
            }
            true
        } else {
            false
        };

        let start_len = self.tx_start_len.take().unwrap_or(0);
        if let Some(id) = self.active_tx_id.take() {
            if !self.wal.is_off() {
                let _ = self.wal.append(id, WalRecordType::Rollback, &[]);
            }
        }
        self.wal.discard_and_truncate_to(start_len)?;

        // Step 1: throw away every uncommitted in-memory write so the
        // upcoming Drop of `*self` has nothing dirty to flush.
        for tbl in &mut self.tables {
            tbl.heap.discard_dirty();
        }
        // Step 2: discard WAL records appended since the last explicit
        // sync point. Large pending records can spill through BufWriter and
        // become file-visible before `sync_wal()`; truncating to the last
        // synced boundary prevents `open()` below from replaying rolled-back
        // transaction records.
        self.wal.discard_pending()?;
        // Step 3: re-open the catalog from disk. The heap files on disk
        // still reflect the last checkpoint (pre-transaction state)
        // because we never flushed the transaction's dirty pages.
        let data_dir = self.data_dir.clone();
        let sync_mode = self.wal.sync_mode();
        let restored = if prearchived {
            let mut already_archived = |_dir: &Path, _records: &[WalRecord]| Ok(());
            let archive: WalArchiveCallback<'_> = &mut already_archived;
            Self::open_inner(&data_dir, Some(archive))?
        } else {
            Self::open_inner(&data_dir, archive)?
        };
        *self = restored;
        self.wal.set_sync_mode(sync_mode);
        Ok(())
    }

    fn abandon_active_transaction_for_drop(&mut self) -> io::Result<()> {
        for tbl in &mut self.tables {
            tbl.heap.discard_dirty();
        }
        self.pending_autocommit_tx_ids.clear();
        let truncate_result = match self.tx_start_len.take() {
            Some(start_len) => self.wal.discard_and_truncate_to(start_len),
            None => self.wal.discard_pending(),
        };
        self.active_tx_id = None;
        truncate_result
    }

    /// Returns a reference to the data directory.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Highest page LSN across all tables (0 if nothing has been written).
    /// This is the durability high-water mark — the LSN a backup taken now
    /// corresponds to, and the value `Catalog::open` uses to restore
    /// `next_lsn` after a reopen/restore.
    pub fn max_lsn(&self) -> u64 {
        let max_page_lsn = self
            .tables
            .iter()
            .map(|t| t.heap.max_page_lsn())
            .max()
            .unwrap_or(0);
        max_page_lsn
            .max(self.durable_lsn)
            .max(self.wal.last_appended_lsn())
    }

    pub fn create_table(&mut self, schema: Schema) -> io::Result<()> {
        self.create_table_full(schema, Vec::new(), Vec::new())
    }

    /// Create a table whose columns carry literal defaults. `defaults` is
    /// aligned to `schema.columns` by position (and may be shorter / empty for
    /// columns without a default).
    pub fn create_table_with_defaults(
        &mut self,
        schema: Schema,
        defaults: Vec<Option<Value>>,
    ) -> io::Result<()> {
        self.create_table_full(schema, defaults, Vec::new())
    }

    /// Create a table with per-column literal defaults and auto-increment
    /// flags. Both vecs are aligned to `schema.columns` by position (and may be
    /// empty). Defaults and auto flags are WAL-logged and persisted in the
    /// catalog so they survive a restart.
    pub fn create_table_full(
        &mut self,
        schema: Schema,
        defaults: Vec<Option<Value>>,
        auto_cols: Vec<bool>,
    ) -> io::Result<()> {
        validate_table_name(&schema.table_name)?;
        for col in &schema.columns {
            validate_column_name(&col.name)?;
        }
        let name = schema.table_name.clone();
        if self.name_to_slot.contains_key(&name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("table '{name}' already exists"),
            ));
        }
        if !self.wal.is_off() {
            let payload = encode_ddl_create_table(&schema, &defaults, &auto_cols);
            self.wal
                .append(0, WalRecordType::DdlCreateTable, &payload)?;
            self.wal.flush()?;
        }
        let mut table = Table::create(schema, &self.data_dir)?;
        table.set_defaults(defaults);
        table.set_auto_cols(auto_cols);
        let slot = self.tables.len();
        self.tables.push(table);
        self.name_to_slot.insert(name, slot);
        self.persist()?;
        Ok(())
    }

    /// Per-column literal defaults for a table, aligned to its columns by
    /// position. `None` when the table is unknown; an empty slice when no
    /// column has a default.
    pub fn column_defaults(&self, table: &str) -> Option<&[Option<Value>]> {
        let slot = *self.name_to_slot.get(table)?;
        Some(self.tables[slot].defaults())
    }

    /// Which columns of a table are `auto`, aligned to its columns by position.
    /// `None` when the table is unknown; an empty slice when none are auto.
    pub fn auto_columns(&self, table: &str) -> Option<&[bool]> {
        let slot = *self.name_to_slot.get(table)?;
        Some(self.tables[slot].auto_cols())
    }

    /// Fill any omitted (`Empty`) auto column in `values` from the table's
    /// sequence and advance it. No-op when the table is unknown or has no auto
    /// columns.
    pub fn assign_auto_columns(&mut self, table: &str, values: &mut [Value]) {
        if let Some(&slot) = self.name_to_slot.get(table) {
            self.tables[slot].assign_auto(values);
        }
    }

    /// Write the current set of schemas to disk atomically (write-then-rename).
    ///
    /// Mission 3: also writes the per-table list of indexed column names so
    /// `Catalog::open` can rehydrate b-tree indexes on restart.
    fn persist(&self) -> io::Result<()> {
        let cat_path = self.data_dir.join(CATALOG_FILE);
        let tmp_path = self.data_dir.join(format!("{CATALOG_FILE}.tmp"));
        let entries: Vec<CatalogEntryRef<'_>> = self
            .tables
            .iter()
            .map(|t| CatalogEntryRef {
                schema: &t.schema,
                indexed_cols: t.indexed_column_metas(),
                defaults: t.defaults(),
                auto_cols: t.auto_cols(),
            })
            .collect();
        write_catalog_file(&tmp_path, &entries)?;
        fs::rename(&tmp_path, &cat_path)?;
        Ok(())
    }

    /// Resolve a table name to its stable slot index. Prepared-query
    /// fast paths cache this once and skip the hash probe on every
    /// subsequent execution. Slots never shift once assigned.
    #[inline]
    pub fn table_slot(&self, name: &str) -> Option<usize> {
        self.name_to_slot.get(name).copied()
    }

    /// O(1) slot-indexed table access. Panics on an out-of-range slot
    /// — callers must have obtained the slot via `table_slot()`.
    #[inline]
    pub fn table_by_slot(&self, slot: usize) -> &Table {
        &self.tables[slot]
    }

    /// Mutable counterpart to [`Self::table_by_slot`].
    #[inline]
    pub fn table_by_slot_mut(&mut self, slot: usize) -> &mut Table {
        &mut self.tables[slot]
    }

    pub fn get_table(&self, name: &str) -> Option<&Table> {
        let slot = *self.name_to_slot.get(name)?;
        Some(&self.tables[slot])
    }

    pub fn get_table_mut(&mut self, name: &str) -> Option<&mut Table> {
        let slot = *self.name_to_slot.get(name)?;
        Some(&mut self.tables[slot])
    }

    /// Private helper: resolve a table name to `&Table`, or return an
    /// `io::Error` with the same "table '<name>' not found" message the
    /// older `get_mut().ok_or_else(...)` callers produced. Phase 18
    /// consolidates ~14 copies of that idiom into this one place.
    #[inline]
    fn by_name(&self, table: &str) -> io::Result<&Table> {
        let slot = *self.name_to_slot.get(table).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("table '{table}' not found"),
            )
        })?;
        Ok(&self.tables[slot])
    }

    /// Mutable counterpart to [`Self::by_name`].
    #[inline]
    fn by_name_mut(&mut self, table: &str) -> io::Result<&mut Table> {
        let slot = *self.name_to_slot.get(table).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("table '{table}' not found"),
            )
        })?;
        Ok(&mut self.tables[slot])
    }

    pub fn insert(&mut self, table: &str, values: &Row) -> io::Result<RowId> {
        // Mission 2: encode the row into a scratch buffer first so we can
        // log it to the WAL before touching the heap. We re-encode inside
        // `Table::insert`, which keeps the insert hot path untouched — the
        // WAL encode here is additive.
        //
        // Mission B (post-review, second pass): in `WalSyncMode::Off` the
        // entire WAL pipeline is a no-op, so skip the per-row
        // `encode_row_into` allocation and `wal_log` call entirely.
        if self.wal.is_off() {
            return self.by_name_mut(table)?.insert(values);
        }
        let tbl = self.by_name_mut(table)?;
        let mut wal_bytes: Vec<u8> = Vec::new();
        encode_row_into(&tbl.schema, values, &mut wal_bytes);
        // Insert into the heap FIRST so we can log the record with the real
        // RowId. Replay needs the true (page, slot) to (a) know which page
        // an Insert targets — for the per-page LSN durability check in
        // `replay_wal` — and (b) reproduce the exact slot assignment, which
        // makes Insert replay idempotent (no duplicate rows after a
        // partial-flush crash, the v0.4.x data-loss bug).
        //
        // Ordering note: mutating the heap before appending+fsyncing the WAL
        // is safe. A crash between the heap insert and the WAL append leaves
        // the row only in an un-fsynced hot page (not durable) and the
        // statement has not returned `Ok` (the executor's end-of-statement
        // `sync_wal` hasn't run), so the write was never acknowledged.
        // Durability is still gated on the WAL fsync at statement end.
        let new_rid = tbl.insert(values)?;
        let tx_id = self.next_tx();
        self.wal_log(tx_id, WalRecordType::Insert, table, new_rid, &wal_bytes)?;
        // Stamp the landing page with this record's LSN so a future replay
        // recognises the row as already persisted (per-page idempotency).
        // The page is hot (just inserted into), so this is an in-memory
        // header write — no extra I/O on the insert hot path.
        let lsn = self.wal.last_appended_lsn();
        if lsn > 0 {
            self.by_name_mut(table)?
                .heap
                .set_page_lsn(new_rid.page_id, lsn)?;
        }
        Ok(new_rid)
    }

    /// WAL-logged insert addressed by table slot index instead of name.
    /// Backs the executor's prepared-insert fast path, which resolves the
    /// slot at prepare time to skip the name→slot hash probe. Behaves exactly
    /// like [`Self::insert`] (logs the record with the real RowId, stamps the
    /// landing page's LSN) — the prepared path previously called the raw
    /// `Table::insert` and bypassed the WAL entirely, silently losing every
    /// prepared insert on a crash.
    pub fn insert_by_slot(&mut self, slot: usize, values: &Row) -> io::Result<RowId> {
        if self.wal.is_off() {
            return self.tables[slot].insert(values);
        }
        let tx_id = self.next_tx();
        let autocommit = self.active_tx_id.is_none();
        let Catalog { tables, wal, .. } = self;
        let tbl = &mut tables[slot];
        let mut wal_bytes: Vec<u8> = Vec::new();
        encode_row_into(&tbl.schema, values, &mut wal_bytes);
        // Insert first so the WAL record carries the real RowId (see
        // `insert` for the ordering/durability argument).
        let new_rid = tbl.insert(values)?;
        let payload = encode_wal_payload(&tbl.schema.table_name, new_rid, &wal_bytes);
        wal.append(tx_id, WalRecordType::Insert, &payload)?;
        if autocommit {
            self.pending_autocommit_tx_ids.push(tx_id);
        }
        let lsn = wal.last_appended_lsn();
        if lsn > 0 {
            tbl.heap.set_page_lsn(new_rid.page_id, lsn)?;
        }
        Ok(new_rid)
    }

    pub fn get(&self, table: &str, rid: RowId) -> Option<Row> {
        self.get_table(table)?.get(rid)
    }

    pub fn delete(&mut self, table: &str, rid: RowId) -> io::Result<()> {
        // Mission B (post-review, second pass): WAL Off → no payload
        // construction.
        if self.wal.is_off() {
            return self.by_name_mut(table)?.delete(rid);
        }
        let tx_id = self.next_tx();
        // Delete records carry only the rid — no row payload.
        self.wal_log(tx_id, WalRecordType::Delete, table, rid, &[])?;
        self.by_name_mut(table)?.delete(rid)
    }

    /// Mission C Phase 12: bulk delete a list of rids, batching btree
    /// maintenance. See [`Table::delete_many`] for the full explanation
    /// and fall-through rules. Returns the number of rows removed.
    pub fn delete_many(&mut self, table: &str, rids: &[RowId]) -> io::Result<u64> {
        // Mission 2: log every rid as an individual Delete record. The
        // WAL flush is deferred to the executor's statement-end
        // `sync_wal` — see [`Self::wal_log`] for the group-commit rules.
        //
        // Mission B (post-review, second pass): in Off mode skip the
        // entire per-row payload loop — `wal.append` would no-op every
        // call but the `encode_wal_payload` Vec alloc would still run.
        if self.wal.is_off() {
            return self.by_name_mut(table)?.delete_many(rids);
        }
        let tx_id = self.next_tx();
        for &rid in rids {
            let payload = encode_wal_payload(table, rid, &[]);
            self.wal.append(tx_id, WalRecordType::Delete, &payload)?;
        }
        if self.active_tx_id.is_none() && !rids.is_empty() {
            self.pending_autocommit_tx_ids.push(tx_id);
        }
        self.by_name_mut(table)?.delete_many(rids)
    }

    /// Single-pass scan-and-delete driven by a raw-bytes predicate. See
    /// [`Table::scan_delete_matching`] and `HeapFile::scan_delete_matching`
    /// for the fusion rationale.
    ///
    /// Prefer [`Self::scan_delete_matching_logged`] from any
    /// caller that needs crash durability. This variant writes no WAL
    /// records, so a crash between the scan and the next checkpoint
    /// would lose the deletes. Kept here for internal paths (e.g.
    /// `drop_table`) where the whole heap is about to be removed anyway.
    pub fn scan_delete_matching<P>(&mut self, table: &str, pred: P) -> io::Result<u64>
    where
        P: FnMut(&[u8]) -> bool,
    {
        self.by_name_mut(table)?.scan_delete_matching(pred)
    }

    /// WAL-logged variant of [`Self::scan_delete_matching`].
    /// Every matched row emits one `WalRecordType::Delete` record in the
    /// same single-pass scan (via the table's `_with_hook` variant), so
    /// crash recovery sees every deletion. Used by the executor's
    /// `Delete(Filter(SeqScan))` and bare `Delete(SeqScan)` fast paths.
    ///
    /// Performance cost vs the non-logged primitive is one per-row WAL
    /// append into the in-memory buffer plus one `fsync` at the end —
    /// the heap scan itself still runs as a single pass with one
    /// `ensure_hot` per page.
    pub fn scan_delete_matching_logged<P>(&mut self, table: &str, pred: P) -> io::Result<u64>
    where
        P: FnMut(&[u8]) -> bool,
    {
        // Mission B (post-review, second pass): in Off mode the per-row
        // hook would build a Vec, do five extends, and then `append`
        // would no-op. Skip the WAL hook entirely and route through
        // the no-WAL primitive — same single-pass scan, zero per-row
        // payload work.
        if self.wal.is_off() {
            return self.by_name_mut(table)?.scan_delete_matching(pred);
        }
        // Resolve slot up front so we can split the borrow — the user
        // hook closes over `&mut self.wal`, which can't coexist with a
        // `by_name_mut` borrow of `self.tables`.
        let slot = *self.name_to_slot.get(table).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("table '{table}' not found"),
            )
        })?;
        let tx_id = self.next_tx();
        let autocommit = self.active_tx_id.is_none();
        // Split-borrow the catalog fields so the hook can write into
        // `wal` while the scan pins `tables[slot]` mutably.
        let Catalog { tables, wal, .. } = self;
        let tbl = &mut tables[slot];
        // Pre-encode the table-name prefix of every WAL payload once —
        // it doesn't vary row-to-row, and the per-row rid+row bytes are
        // the only things we append inside the hook.
        let name_bytes = table.as_bytes();
        let count = tbl.scan_delete_matching_with_hook(pred, |rid, row_bytes| {
            let mut payload: Vec<u8> =
                Vec::with_capacity(4 + name_bytes.len() + 10 + row_bytes.len());
            payload.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
            payload.extend_from_slice(name_bytes);
            payload.extend_from_slice(&rid.page_id.to_le_bytes());
            payload.extend_from_slice(&rid.slot_index.to_le_bytes());
            // Delete records carry no row payload on replay, but we
            // match the `encode_wal_payload` layout so `decode_wal_payload`
            // (which is type-agnostic) parses them cleanly.
            payload.extend_from_slice(&0u32.to_le_bytes());
            // Best-effort append — if it errors we have no way to
            // propagate from inside the hook; we swallow it here and
            // the outer scan's `io::Result` will still succeed. In
            // practice the `BufWriter`-backed `Wal::append` only errors
            // on allocation failure or a disk-full fsync, both of
            // which would fail the outer flush below as well.
            let _ = wal.append(tx_id, WalRecordType::Delete, &payload);
        })?;
        if autocommit && count > 0 {
            self.pending_autocommit_tx_ids.push(tx_id);
        }
        // Flush is deferred to the executor's statement-end `sync_wal`.
        Ok(count)
    }

    /// Single-pass fused scan + in-place patch with WAL logging.
    /// Evaluates `pred` on raw row bytes and applies `try_mutate` to each
    /// match on the same hot page — no second pass. Returns
    /// `(patched_count, fallback_rids)`.
    ///
    /// Perf sprint: update analogue of `scan_delete_matching_logged`.
    /// Eliminates the two-pass collect-then-patch pattern.
    pub fn scan_patch_matching_logged<P, M>(
        &mut self,
        table: &str,
        pred: P,
        try_mutate: M,
    ) -> io::Result<(u64, Vec<RowId>)>
    where
        P: FnMut(&[u8]) -> bool,
        M: FnMut(&mut [u8]) -> Option<u16>,
    {
        if self.wal.is_off() {
            return self.by_name_mut(table)?.scan_patch_matching_with_hook(
                pred,
                try_mutate,
                |_, _| {},
            );
        }
        let slot = *self.name_to_slot.get(table).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("table '{table}' not found"),
            )
        })?;
        let tx_id = self.next_tx();
        let autocommit = self.active_tx_id.is_none();
        let Catalog { tables, wal, .. } = self;
        let tbl = &mut tables[slot];
        let name_bytes = table.as_bytes();
        let result = tbl.scan_patch_matching_with_hook(pred, try_mutate, |rid, row_bytes| {
            let mut payload: Vec<u8> =
                Vec::with_capacity(4 + name_bytes.len() + 10 + row_bytes.len());
            payload.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
            payload.extend_from_slice(name_bytes);
            payload.extend_from_slice(&rid.page_id.to_le_bytes());
            payload.extend_from_slice(&rid.slot_index.to_le_bytes());
            payload.extend_from_slice(&(row_bytes.len() as u32).to_le_bytes());
            payload.extend_from_slice(row_bytes);
            let _ = wal.append(tx_id, WalRecordType::Update, &payload);
        })?;
        if autocommit && result.0 > 0 {
            self.pending_autocommit_tx_ids.push(tx_id);
        }
        Ok(result)
    }

    pub fn update(&mut self, table: &str, rid: RowId, values: &Row) -> io::Result<RowId> {
        // Mission B (post-review, second pass): WAL Off → no payload
        // construction.
        if self.wal.is_off() {
            return self.by_name_mut(table)?.update(rid, values);
        }
        let tbl = self.by_name_mut(table)?;
        let mut wal_bytes: Vec<u8> = Vec::new();
        encode_row_into(&tbl.schema, values, &mut wal_bytes);
        // Reject oversized rows BEFORE appending the WAL record: a logged
        // Update that the heap then rejects would poison the next replay.
        check_encoded_row_size(&wal_bytes)?;
        let tx_id = self.next_tx();
        self.wal_log(tx_id, WalRecordType::Update, table, rid, &wal_bytes)?;
        self.by_name_mut(table)?.update(rid, values)
    }

    /// Mission C Phase 2: update with a hint about which columns actually
    /// changed. Lets [`Table::update_hinted`] skip the old-row read when
    /// the hint shows no indexed column is in the changed set.
    pub fn update_hinted(
        &mut self,
        table: &str,
        rid: RowId,
        values: &Row,
        changed_col_indices: Option<&[usize]>,
    ) -> io::Result<RowId> {
        // Mission B (post-review, second pass): WAL Off → no payload
        // construction. The `update_by_filter` powql bench drives this
        // path tens of thousands of times per iteration.
        if self.wal.is_off() {
            return self
                .by_name_mut(table)?
                .update_hinted(rid, values, changed_col_indices);
        }
        let tbl = self.by_name_mut(table)?;
        let mut wal_bytes: Vec<u8> = Vec::new();
        encode_row_into(&tbl.schema, values, &mut wal_bytes);
        // Same pre-WAL size gate as [`Self::update`].
        check_encoded_row_size(&wal_bytes)?;
        let tx_id = self.next_tx();
        self.wal_log(tx_id, WalRecordType::Update, table, rid, &wal_bytes)?;
        self.by_name_mut(table)?
            .update_hinted(rid, values, changed_col_indices)
    }

    /// Mission C Phase 4: fast-path update that patches a row's raw bytes
    /// in place, skipping decode/encode. Caller guarantees the mutation
    /// preserves the row length and touches no indexed column. Returns
    /// `Ok(true)` if the patch landed, `Ok(false)` if the row is gone.
    ///
    /// This primitive does NOT log to the WAL. Executor
    /// callers must route through [`Self::update_row_bytes_logged`] (or
    /// [`Self::update_row_bytes_logged_by_slot`]) so crash recovery
    /// sees the patched bytes. This raw form is retained for replay
    /// itself and any future callers that can tolerate the non-durable
    /// contract.
    #[inline]
    pub fn with_row_bytes_mut<F>(&mut self, table: &str, rid: RowId, f: F) -> io::Result<bool>
    where
        F: FnOnce(&mut [u8]),
    {
        self.by_name_mut(table)?.with_row_bytes_mut(rid, f)
    }

    /// WAL-logged variant of [`Self::with_row_bytes_mut`].
    /// Applies `f` to the live row bytes on the hot page, then reads
    /// the mutated bytes back and emits a `WalRecordType::Update`
    /// record so replay will re-apply the same patch after a crash.
    ///
    /// Ordering: the hot-page mutation happens first (in-memory only,
    /// no disk I/O), then the WAL record is appended and flushed. A
    /// crash after the mutation but before the WAL flush loses the
    /// update, but the caller never saw success in that case, so the
    /// contract holds: any `Ok(true)` return is durable.
    ///
    /// No hot-page eviction can happen between steps because this
    /// method holds the catalog's `&mut self` exclusively.
    #[inline]
    pub fn update_row_bytes_logged<F>(&mut self, table: &str, rid: RowId, f: F) -> io::Result<bool>
    where
        F: FnOnce(&mut [u8]),
    {
        let slot = *self.name_to_slot.get(table).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("table '{table}' not found"),
            )
        })?;
        self.update_row_bytes_logged_by_slot(slot, rid, f)
    }

    /// Slot-indexed counterpart to [`Self::update_row_bytes_logged`].
    /// Used by prepared-query fast paths that already cached the table
    /// slot at prepare time and want to skip the name->slot probe on
    /// every execution.
    #[inline]
    pub fn update_row_bytes_logged_by_slot<F>(
        &mut self,
        slot: usize,
        rid: RowId,
        f: F,
    ) -> io::Result<bool>
    where
        F: FnOnce(&mut [u8]),
    {
        // Step 1: apply the mutation on the hot page. Failure here
        // (slot gone) short-circuits with Ok(false) — no WAL record.
        let tbl = &mut self.tables[slot];
        let ok = tbl.with_row_bytes_mut(rid, f)?;
        if !ok {
            return Ok(false);
        }
        // Mission B (post-review, second pass): in Off mode the per-row
        // get + clone + table-name clone + wal_log call are all wasted
        // — `wal.append` would no-op. Skip the snapshot path entirely.
        if self.wal.is_off() {
            return Ok(true);
        }
        // Step 2: snapshot the now-mutated bytes. `HeapFile::get`
        // observes the pinned hot page, so it returns the fresh row.
        let new_bytes = match tbl.heap.get(rid) {
            Some(b) => b,
            // Shouldn't happen — we just patched it — but be defensive.
            None => return Ok(false),
        };
        // Step 3: log + flush. Clone the table name out of the schema
        // so we can drop the `&mut tbl` borrow before touching `self.wal`.
        let table_name = tbl.schema.table_name.clone();
        let tx_id = self.next_tx();
        self.wal_log(tx_id, WalRecordType::Update, &table_name, rid, &new_bytes)?;
        Ok(true)
    }

    /// Mission C Phase 10: var-column in-place update fast path. Patches
    /// a single variable-length column's bytes directly into the row's
    /// slot, shrinking the row if the new value is smaller. Returns
    /// `Ok(false)` if the new value would grow the row (caller must fall
    /// back to the full encode path) or the row is gone.
    ///
    /// Caller guarantees no indexed column is touched — indexes are NOT
    /// maintained by this primitive.
    ///
    /// Not WAL-logged. Executor callers should use
    /// [`Self::patch_var_col_logged`] instead.
    #[inline]
    pub fn patch_var_col_in_place(
        &mut self,
        table: &str,
        rid: RowId,
        col_idx: usize,
        new_value: Option<&[u8]>,
    ) -> io::Result<bool> {
        self.by_name_mut(table)?
            .patch_var_col_in_place(rid, col_idx, new_value)
    }

    /// WAL-logged variant of [`Self::patch_var_col_in_place`].
    /// Runs the in-place shrink on the hot page, then reads the mutated
    /// row bytes back and logs a `WalRecordType::Update` record. On a
    /// `false` return (grow-case bail) nothing is logged — the caller's
    /// fall-through to `update_hinted` handles the WAL itself.
    pub fn patch_var_col_logged(
        &mut self,
        table: &str,
        rid: RowId,
        col_idx: usize,
        new_value: Option<&[u8]>,
    ) -> io::Result<bool> {
        let slot = *self.name_to_slot.get(table).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("table '{table}' not found"),
            )
        })?;
        let tbl = &mut self.tables[slot];
        let ok = tbl.patch_var_col_in_place(rid, col_idx, new_value)?;
        if !ok {
            return Ok(false);
        }
        // Mission B (post-review, second pass): WAL Off → skip the
        // snapshot + clone + log entirely.
        if self.wal.is_off() {
            return Ok(true);
        }
        let new_bytes = match tbl.heap.get(rid) {
            Some(b) => b,
            None => return Ok(false),
        };
        let table_name = tbl.schema.table_name.clone();
        let tx_id = self.next_tx();
        self.wal_log(tx_id, WalRecordType::Update, &table_name, rid, &new_bytes)?;
        Ok(true)
    }

    pub fn scan(&self, table: &str) -> io::Result<impl Iterator<Item = (RowId, Row)> + '_> {
        Ok(self.by_name(table)?.scan())
    }

    /// Zero-copy scan: passes raw row bytes to the callback without any
    /// per-row allocation. Used by the executor's fast paths.
    pub fn for_each_row_raw<F>(&self, table: &str, f: F) -> io::Result<()>
    where
        F: FnMut(RowId, &[u8]),
    {
        self.by_name(table)?.for_each_row_raw(f);
        Ok(())
    }

    /// Zero-copy scan with early termination. The callback returns
    /// `ControlFlow::Break(())` to stop. Used by `Limit` fast paths so a
    /// `limit 100` query doesn't pay decode/predicate cost for every row
    /// in the table after the limit is reached.
    pub fn try_for_each_row_raw<F>(&self, table: &str, f: F) -> io::Result<()>
    where
        F: FnMut(RowId, &[u8]) -> std::ops::ControlFlow<()>,
    {
        self.by_name(table)?.try_for_each_row_raw(f);
        Ok(())
    }

    pub fn create_index(&mut self, table: &str, column: &str) -> io::Result<()> {
        self.create_index_unique(table, column, false)
    }

    /// Create an index with an explicit uniqueness flag. `unique = true`
    /// for primary-key-like columns where duplicate values should
    /// overwrite. `unique = false` for secondary indexes that allow
    /// duplicate column values (the default via `create_index`).
    pub fn create_index_unique(
        &mut self,
        table: &str,
        column: &str,
        unique: bool,
    ) -> io::Result<()> {
        let data_dir = self.data_dir.clone();
        self.by_name_mut(table)?
            .create_index_with_unique(column, &data_dir, unique)?;
        // Mission 3: persist the updated catalog so the indexed column
        // list survives a restart. `Table::create_index` already saved
        // the btree file itself.
        self.persist()
    }

    /// Whether `table.column` has a UNIQUE index. Returns `Some(true)` for
    /// a unique index, `Some(false)` for a non-unique index, and `None`
    /// when the column is not indexed or the table is unknown.
    pub fn is_index_unique(&self, table: &str, column: &str) -> Option<bool> {
        self.get_table(table)?.is_index_unique(column)
    }

    /// Whether `table.column` has any index (unique or non-unique).
    pub fn has_index(&self, table: &str, column: &str) -> bool {
        self.get_table(table)
            .map(|t| t.has_index(column))
            .unwrap_or(false)
    }

    pub fn index_lookup(&self, table: &str, column: &str, key: &Value) -> io::Result<Option<Row>> {
        Ok(self
            .by_name(table)?
            .index_lookup(column, key)
            .map(|(_, row)| row))
    }

    pub fn list_tables(&self) -> Vec<&str> {
        // Phase 18: iterate the Vec directly — schema.table_name is
        // the source of truth, and Vec order is insertion order (more
        // deterministic than the old FxHashMap keys).
        self.tables
            .iter()
            .map(|t| t.schema.table_name.as_str())
            .collect()
    }

    pub fn schema(&self, table: &str) -> Option<&Schema> {
        let slot = *self.name_to_slot.get(table)?;
        Some(&self.tables[slot].schema)
    }

    /// Drop a table: remove from the catalog and delete its data files.
    /// Returns `Err` if the table doesn't exist.
    pub fn drop_table(&mut self, name: &str) -> io::Result<()> {
        validate_table_name(name)?;
        let slot = *self.name_to_slot.get(name).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("table '{name}' not found"))
        })?;
        if !self.wal.is_off() {
            let payload = encode_ddl_drop_table(name);
            self.wal.append(0, WalRecordType::DdlDropTable, &payload)?;
            self.wal.flush()?;
        }
        // Remove the data file.
        let table = &self.tables[slot];
        let heap_path = self
            .data_dir
            .join(format!("{}.heap", table.schema.table_name));
        if heap_path.exists() {
            fs::remove_file(&heap_path)?;
        }
        // Mission 3: remove only the .idx files that actually exist
        // (i.e. the columns the table currently has indexed). The pre-
        // Mission-3 code iterated every schema column blindly — harmless
        // but noisy. Now that we persist a real list of indexed columns,
        // we can be precise.
        for col_name in table.indexed_column_names() {
            let idx_path = self.data_dir.join(format!("{name}_{col_name}.idx"));
            if idx_path.exists() {
                let _ = fs::remove_file(&idx_path);
            }
        }
        // Swap-remove from the Vec and fix up name_to_slot.
        self.name_to_slot.remove(name);
        let last = self.tables.len() - 1;
        if slot != last {
            let moved_name = self.tables[last].schema.table_name.clone();
            self.tables.swap(slot, last);
            self.name_to_slot.insert(moved_name, slot);
        }
        self.tables.pop();
        self.persist()?;
        Ok(())
    }

    /// Add a column to an existing table's schema and backfill all
    /// existing rows to match the new shape.
    ///
    /// Older versions of this method only mutated the in-memory schema
    /// and relied on a (false) claim that "the heap format already
    /// handles short rows gracefully". It doesn't: `decode_row` reads
    /// exactly `n_var + 1` variable-column offsets from the row bytes
    /// using the CURRENT schema. Any row encoded with the old schema's
    /// (smaller) offset table would walk off the end of its buffer and
    /// panic with "range end index X out of range for slice of length Y"
    /// — which is exactly what a bare `Type` scan triggered right after
    /// an ALTER ADD COLUMN.
    ///
    /// The fix: rewrite every existing row through
    /// `Table::rewrite_rows_for_schema_change` so the on-disk
    /// encoding matches the new schema layout. Existing rows get
    /// `Value::Empty` for the new column.
    ///
    /// If the new column is `required` we refuse to add it to a
    /// non-empty table — there is no default value to backfill with,
    /// and silently storing `Empty` in a required slot would just
    /// shift the invariant violation to the next query.
    pub fn alter_table_add_column(&mut self, table: &str, col: ColumnDef) -> io::Result<()> {
        let data_dir = self.data_dir.clone();
        {
            let tbl = self.by_name_mut(table)?;
            if tbl.schema.columns.iter().any(|c| c.name == col.name) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("column '{}' already exists in table '{table}'", col.name),
                ));
            }
        }
        let barrier_lsn = if !self.wal.is_off() {
            let payload = encode_ddl_alter_add_column(table, &col);
            self.wal.append(0, WalRecordType::DdlAddColumn, &payload)?;
            self.wal.flush()?;
            self.wal.last_appended_lsn()
        } else {
            0
        };
        let tbl = self.by_name_mut(table)?;

        let old_schema = tbl.schema.clone();

        // Peek at the heap to learn whether there are any existing
        // rows at all. An empty table is always safe to alter — no
        // rewrite needed, required columns are fine, etc.
        let has_rows = tbl.heap.scan().next().is_some();

        if has_rows && col.required {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "cannot add required column '{}' to non-empty table '{table}': \
                     no default value to backfill existing rows with",
                    col.name
                ),
            ));
        }

        // Commit the new column into the schema and refresh the
        // cached layout so the rewrite below encodes with the new
        // shape.
        tbl.schema.columns.push(col);
        tbl.refresh_layout();

        if has_rows {
            // Build the "fill" template: all Empty, matching the new
            // schema width. `rewrite_rows_for_schema_change` will
            // overwrite old-column slots from each live row and leave
            // the new slot as Empty.
            let fill: Vec<Value> = vec![Value::Empty; tbl.schema.columns.len()];
            tbl.rewrite_rows_for_schema_change(&old_schema, &fill, &data_dir)?;
        }
        // P0 fix (v0.4.3): stamp every heap page with the DDL record's
        // LSN so any pre-DDL Insert/Update/Delete WAL record gets
        // skipped on replay. Without this barrier, a restart after
        // `alter add column` would replay pre-alter inserts (encoded in
        // the OLD layout) onto a heap that's already in the NEW layout,
        // producing a mixed-version heap that panics on the next
        // projection. Regression: see `restart_after_alter_add_column_then_index`.
        if barrier_lsn > 0 {
            tbl.heap.stamp_all_pages_min_lsn(barrier_lsn)?;
            tbl.heap.flush()?;
        }

        self.persist()?;
        Ok(())
    }

    /// Remove a column from an existing table's schema and rewrite
    /// every live row to match the new shape.
    ///
    /// Older versions of this method only mutated the in-memory schema
    /// and claimed that "reads simply won't decode the dropped column".
    /// That was wrong in several ways:
    ///
    ///   1. The null bitmap is indexed by column position. Dropping a
    ///      column shifts every later column's bit left, but old rows
    ///      still have bits in the original positions — so `is_null`
    ///      checks silently lie for every column after the dropped one.
    ///   2. The bitmap's byte width (`ceil(n_cols/8)`) can shrink when
    ///      `n_cols` crosses an 8-boundary, shifting every subsequent
    ///      byte of the row against the decoder's cursor.
    ///   3. Fixed-region size and the variable-offset-table width both
    ///      depend on the column set, so dropping any fixed or variable
    ///      column slides every following byte.
    ///
    /// The fix mirrors `alter_table_add_column`: snapshot the old
    /// schema, mutate to the new schema, then rewrite every row
    /// through `Table::rewrite_rows_for_schema_change`. Dropping a
    /// column from an empty table skips the rewrite.
    pub fn alter_table_drop_column(&mut self, table: &str, col_name: &str) -> io::Result<()> {
        let data_dir = self.data_dir.clone();
        {
            let tbl = self.by_name_mut(table)?;
            tbl.schema
                .columns
                .iter()
                .position(|c| c.name == col_name)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("column '{col_name}' not found in table '{table}'"),
                    )
                })?;
        }
        let barrier_lsn = if !self.wal.is_off() {
            let payload = encode_ddl_alter_drop_column(table, col_name);
            self.wal.append(0, WalRecordType::DdlDropColumn, &payload)?;
            self.wal.flush()?;
            self.wal.last_appended_lsn()
        } else {
            0
        };
        let tbl = self.by_name_mut(table)?;
        let idx = tbl
            .schema
            .columns
            .iter()
            .position(|c| c.name == col_name)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("column '{col_name}' not found in table '{table}'"),
                )
            })?;

        // Snapshot for decoding old rows.
        let old_schema = tbl.schema.clone();
        let has_rows = tbl.heap.scan().next().is_some();

        // Commit the schema change.
        tbl.schema.columns.remove(idx);
        for (i, col) in tbl.schema.columns.iter_mut().enumerate() {
            col.position = i as u16;
        }
        tbl.refresh_layout();

        if has_rows {
            // Build a filler matching the new (smaller) shape. The
            // rewrite path overwrites each new-column slot from the
            // matching old-column value by name, so the filler only
            // matters for brand-new columns — drop has none, so
            // `Empty` is a safe placeholder that never gets read.
            let fill: Vec<Value> = vec![Value::Empty; tbl.schema.columns.len()];
            tbl.rewrite_rows_for_schema_change(&old_schema, &fill, &data_dir)?;
        }
        // P0 fix: see matching comment in alter_table_add_column.
        if barrier_lsn > 0 {
            tbl.heap.stamp_all_pages_min_lsn(barrier_lsn)?;
            tbl.heap.flush()?;
        }

        self.persist()?;
        Ok(())
    }
}

impl Drop for Catalog {
    fn drop(&mut self) {
        if self.active_tx_id.is_some() {
            if let Err(e) = self.abandon_active_transaction_for_drop() {
                warn!(error = %e, "catalog drop active transaction cleanup failed");
            }
            return;
        }
        // Mission 2: best-effort clean shutdown. `checkpoint` flushes
        // every heap and truncates the WAL, which is what
        // [`Catalog::open`] relies on to know that no replay is needed.
        //
        // We swallow errors here because Rust's `Drop` can't propagate
        // them and panicking during unwind is always a bigger problem
        // than a failed flush. The worst case on a failed drop-time
        // checkpoint is that the next open sees a non-empty WAL and
        // replays it (potentially producing duplicates — see the
        // [`Self::replay_wal`] caveat). That's strictly better than
        // losing committed writes.
        if let Err(e) = self.checkpoint() {
            warn!(error = %e, "catalog drop checkpoint failed");
        }
    }
}

// ─── WAL payload codec ─────────────────────────────────────────────────────
//
// Per-record payload layout (little-endian):
//
//   table_name_len : u32
//   table_name     : utf-8 bytes
//   page_id        : u32   (for insert: 0, ignored on replay)
//   slot_index     : u16   (for insert: 0, ignored on replay)
//   row_len        : u32
//   row_bytes      : raw encoded row (length = row_len)
//
// Lives next to `Catalog` because this is the only code that produces or
// consumes these records — the `Wal` itself is payload-agnostic.

fn encode_wal_payload(table: &str, rid: RowId, row_bytes: &[u8]) -> Vec<u8> {
    let name = table.as_bytes();
    let mut out = Vec::with_capacity(4 + name.len() + 4 + 2 + 4 + row_bytes.len());
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&rid.page_id.to_le_bytes());
    out.extend_from_slice(&rid.slot_index.to_le_bytes());
    out.extend_from_slice(&(row_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(row_bytes);
    out
}

fn decode_wal_payload(data: &[u8]) -> Option<(String, RowId, Vec<u8>)> {
    let mut pos = 0usize;
    if data.len() < 4 {
        return None;
    }
    let name_len = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
    pos += 4;
    if pos + name_len > data.len() {
        return None;
    }
    let name = std::str::from_utf8(&data[pos..pos + name_len])
        .ok()?
        .to_string();
    pos += name_len;
    if pos + 4 + 2 + 4 > data.len() {
        return None;
    }
    let page_id = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?);
    pos += 4;
    let slot_index = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?);
    pos += 2;
    let row_len = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
    pos += 4;
    if pos + row_len > data.len() {
        return None;
    }
    let row_bytes = data[pos..pos + row_len].to_vec();
    Some((
        name,
        RowId {
            page_id,
            slot_index,
        },
        row_bytes,
    ))
}

// ─── DDL WAL payload codecs ─────────────────────────────────────────────────

fn encode_ddl_create_table(
    schema: &Schema,
    defaults: &[Option<Value>],
    auto_cols: &[bool],
) -> Vec<u8> {
    let name = schema.table_name.as_bytes();
    let mut out = Vec::new();
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&(schema.columns.len() as u16).to_le_bytes());
    for col in &schema.columns {
        let cn = col.name.as_bytes();
        out.extend_from_slice(&(cn.len() as u32).to_le_bytes());
        out.extend_from_slice(cn);
        out.push(col.type_id as u8);
        out.push(col.required as u8);
        out.extend_from_slice(&col.position.to_le_bytes());
    }
    // Trailing sections. Records written before each feature existed simply
    // lack the corresponding trailing bytes, so the decoder treats their
    // absence as "none" (length-detected, append-only).
    encode_defaults_section(&mut out, defaults);
    encode_auto_section(&mut out, auto_cols);
    out
}

fn decode_ddl_create_table(data: &[u8]) -> Option<(Schema, Vec<Option<Value>>, Vec<bool>)> {
    let mut pos = 0usize;
    if data.len() < 4 {
        return None;
    }
    let name_len = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
    pos += 4;
    if pos + name_len > data.len() {
        return None;
    }
    let table_name = std::str::from_utf8(&data[pos..pos + name_len])
        .ok()?
        .to_string();
    pos += name_len;
    if pos + 2 > data.len() {
        return None;
    }
    let n_cols = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
    pos += 2;
    let mut columns = Vec::with_capacity(n_cols);
    for _ in 0..n_cols {
        if pos + 4 > data.len() {
            return None;
        }
        let cn_len = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        pos += 4;
        if pos + cn_len + 4 > data.len() {
            return None;
        }
        let col_name = std::str::from_utf8(&data[pos..pos + cn_len])
            .ok()?
            .to_string();
        pos += cn_len;
        let type_id = TypeId::from_u8(data[pos])?;
        pos += 1;
        let required = data[pos] != 0;
        pos += 1;
        if pos + 2 > data.len() {
            return None;
        }
        let position = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?);
        pos += 2;
        columns.push(ColumnDef {
            name: col_name,
            type_id,
            required,
            position,
        });
    }
    // Trailing sections are present on records written after each feature
    // landed; older records end early, decoding to "none".
    let defaults = if pos < data.len() {
        decode_defaults_section(data, &mut pos, columns.len())?
    } else {
        Vec::new()
    };
    let auto_cols = if pos < data.len() {
        decode_auto_section(data, &mut pos, columns.len())?
    } else {
        Vec::new()
    };
    Some((
        Schema {
            table_name,
            columns,
        },
        defaults,
        auto_cols,
    ))
}

fn encode_ddl_drop_table(table_name: &str) -> Vec<u8> {
    let name = table_name.as_bytes();
    let mut out = Vec::with_capacity(4 + name.len());
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name);
    out
}

fn encode_ddl_alter_add_column(table_name: &str, col: &ColumnDef) -> Vec<u8> {
    let name = table_name.as_bytes();
    let cn = col.name.as_bytes();
    let mut out = Vec::with_capacity(4 + name.len() + 4 + cn.len() + 4);
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&(cn.len() as u32).to_le_bytes());
    out.extend_from_slice(cn);
    out.push(col.type_id as u8);
    out.push(col.required as u8);
    out.extend_from_slice(&col.position.to_le_bytes());
    out
}

fn encode_ddl_alter_drop_column(table_name: &str, col_name: &str) -> Vec<u8> {
    let name = table_name.as_bytes();
    let cn = col_name.as_bytes();
    let mut out = Vec::with_capacity(4 + name.len() + 4 + cn.len());
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&(cn.len() as u32).to_le_bytes());
    out.extend_from_slice(cn);
    out
}

fn decode_ddl_table_name(data: &[u8]) -> Option<(String, usize)> {
    if data.len() < 4 {
        return None;
    }
    let name_len = u32::from_le_bytes(data[0..4].try_into().ok()?) as usize;
    if 4 + name_len > data.len() {
        return None;
    }
    let name = std::str::from_utf8(&data[4..4 + name_len])
        .ok()?
        .to_string();
    Some((name, 4 + name_len))
}

fn decode_ddl_alter_add_column(data: &[u8]) -> Option<(String, ColumnDef)> {
    let (table_name, mut pos) = decode_ddl_table_name(data)?;
    if pos + 4 > data.len() {
        return None;
    }
    let cn_len = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
    pos += 4;
    if pos + cn_len + 4 > data.len() {
        return None;
    }
    let col_name = std::str::from_utf8(&data[pos..pos + cn_len])
        .ok()?
        .to_string();
    pos += cn_len;
    let type_id = TypeId::from_u8(data[pos])?;
    pos += 1;
    let required = data[pos] != 0;
    pos += 1;
    if pos + 2 > data.len() {
        return None;
    }
    let position = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?);
    Some((
        table_name,
        ColumnDef {
            name: col_name,
            type_id,
            required,
            position,
        },
    ))
}

fn decode_ddl_alter_drop_column(data: &[u8]) -> Option<(String, String)> {
    let (table_name, pos) = decode_ddl_table_name(data)?;
    if pos + 4 > data.len() {
        return None;
    }
    let cn_len = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
    if pos + 4 + cn_len > data.len() {
        return None;
    }
    let col_name = std::str::from_utf8(&data[pos + 4..pos + 4 + cn_len])
        .ok()?
        .to_string();
    Some((table_name, col_name))
}

// ─── Catalog file format ────────────────────────────────────────────────────
//
// Layout (version 2):
//   magic     [4]      = "BCAT"
//   version   u16
//   n_tables  u32
//   for each table:
//     table_name_len  u32
//     table_name      utf8 bytes
//     n_columns       u16
//     for each column:
//       name_len      u32
//       name          utf8 bytes
//       type_id       u8
//       required      u8
//       position      u16
//     ── version 2 appends: ──
//     n_indexed_cols  u16
//     for each indexed column:
//       name_len      u32
//       name          utf8 bytes
//
// Version 1 files are accepted by the reader (same shape minus the
// trailing indexed-column block) and treated as having zero indexed
// columns. Writers always emit version 2 from Mission 3 onwards.

/// Per-indexed-column metadata persisted in the catalog file.
pub(crate) struct IndexedColMeta {
    pub name: String,
    pub unique: bool,
}

/// In-memory catalog entry pairing a schema with its indexed column list.
/// Produced by the reader; the writer takes the borrowed counterpart below.
pub(crate) struct CatalogEntry {
    pub schema: Schema,
    pub indexed_cols: Vec<IndexedColMeta>,
    /// Per-column defaults aligned to `schema.columns` by position. Empty when
    /// no column has a default (v1–v3 files always decode to empty).
    pub defaults: Vec<Option<Value>>,
    /// Which columns are `auto`, aligned to `schema.columns`. Empty when none
    /// (v1–v4 files always decode to empty).
    pub auto_cols: Vec<bool>,
}

/// Borrowed view passed to the writer.
pub(crate) struct CatalogEntryRef<'a> {
    pub schema: &'a Schema,
    pub indexed_cols: Vec<IndexedColMeta>,
    pub defaults: &'a [Option<Value>],
    pub auto_cols: &'a [bool],
}

// ─── Column-default codecs (shared by catalog.bin and the WAL DDL record) ────

/// Encode a single scalar value: a `type_id` tag byte followed by a
/// type-specific, length-prefixed (for variable-width types) payload. Lossless
/// — used to persist literal column defaults.
fn encode_value_blob(out: &mut Vec<u8>, v: &Value) {
    out.push(v.type_id() as u8);
    match v {
        Value::Int(n) => out.extend_from_slice(&n.to_le_bytes()),
        Value::Float(f) => out.extend_from_slice(&f.to_bits().to_le_bytes()),
        Value::Bool(b) => out.push(*b as u8),
        Value::Str(s) => {
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Value::DateTime(n) => out.extend_from_slice(&n.to_le_bytes()),
        Value::Uuid(u) => out.extend_from_slice(u),
        Value::Bytes(b) => {
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
        Value::Empty => {}
    }
}

/// Inverse of [`encode_value_blob`]. Returns `None` on any malformed/truncated
/// input so a corrupt record fails closed rather than panicking.
fn decode_value_blob(data: &[u8], pos: &mut usize) -> Option<Value> {
    let tag = *data.get(*pos)?;
    *pos += 1;
    let type_id = TypeId::from_u8(tag)?;
    let take_fixed = |pos: &mut usize, n: usize| -> Option<Vec<u8>> {
        if *pos + n > data.len() {
            return None;
        }
        let slice = data[*pos..*pos + n].to_vec();
        *pos += n;
        Some(slice)
    };
    match type_id {
        TypeId::Empty => Some(Value::Empty),
        TypeId::Int => Some(Value::Int(i64::from_le_bytes(
            take_fixed(pos, 8)?.try_into().ok()?,
        ))),
        TypeId::Float => Some(Value::Float(f64::from_bits(u64::from_le_bytes(
            take_fixed(pos, 8)?.try_into().ok()?,
        )))),
        TypeId::Bool => Some(Value::Bool(take_fixed(pos, 1)?[0] != 0)),
        TypeId::DateTime => Some(Value::DateTime(i64::from_le_bytes(
            take_fixed(pos, 8)?.try_into().ok()?,
        ))),
        TypeId::Uuid => Some(Value::Uuid(take_fixed(pos, 16)?.try_into().ok()?)),
        TypeId::Str => {
            let len = u32::from_le_bytes(take_fixed(pos, 4)?.try_into().ok()?) as usize;
            Some(Value::Str(String::from_utf8(take_fixed(pos, len)?).ok()?))
        }
        TypeId::Bytes => {
            let len = u32::from_le_bytes(take_fixed(pos, 4)?.try_into().ok()?) as usize;
            Some(Value::Bytes(take_fixed(pos, len)?))
        }
    }
}

/// Encode the per-table defaults as a sparse list: a `u16` count of columns
/// that have a default, then `(position: u16, value blob)` pairs. The common
/// "no defaults" case costs two bytes.
fn encode_defaults_section(out: &mut Vec<u8>, defaults: &[Option<Value>]) {
    let present: Vec<(u16, &Value)> = defaults
        .iter()
        .enumerate()
        .filter_map(|(i, d)| d.as_ref().map(|v| (i as u16, v)))
        .collect();
    out.extend_from_slice(&(present.len() as u16).to_le_bytes());
    for (pos, v) in present {
        out.extend_from_slice(&pos.to_le_bytes());
        encode_value_blob(out, v);
    }
}

/// Inverse of [`encode_defaults_section`]. Builds a `Vec` of length `n_cols`
/// with `None` for columns without a default. Returns `None` on truncation.
fn decode_defaults_section(
    data: &[u8],
    pos: &mut usize,
    n_cols: usize,
) -> Option<Vec<Option<Value>>> {
    if *pos + 2 > data.len() {
        return None;
    }
    let count = u16::from_le_bytes(data[*pos..*pos + 2].try_into().ok()?) as usize;
    *pos += 2;
    let mut out = vec![None; n_cols];
    for _ in 0..count {
        if *pos + 2 > data.len() {
            return None;
        }
        let col = u16::from_le_bytes(data[*pos..*pos + 2].try_into().ok()?) as usize;
        *pos += 2;
        let value = decode_value_blob(data, pos)?;
        if col < n_cols {
            out[col] = Some(value);
        }
    }
    Some(out)
}

/// Encode the per-table `auto` columns as a sparse list: a `u16` count of auto
/// columns, then their positions (`u16` each). "No auto columns" costs two
/// bytes.
fn encode_auto_section(out: &mut Vec<u8>, auto_cols: &[bool]) {
    let present: Vec<u16> = auto_cols
        .iter()
        .enumerate()
        .filter_map(|(i, &a)| if a { Some(i as u16) } else { None })
        .collect();
    out.extend_from_slice(&(present.len() as u16).to_le_bytes());
    for pos in present {
        out.extend_from_slice(&pos.to_le_bytes());
    }
}

/// Inverse of [`encode_auto_section`]. Builds a `bool` vec of length `n_cols`.
/// Returns `None` on truncation.
fn decode_auto_section(data: &[u8], pos: &mut usize, n_cols: usize) -> Option<Vec<bool>> {
    if *pos + 2 > data.len() {
        return None;
    }
    let count = u16::from_le_bytes(data[*pos..*pos + 2].try_into().ok()?) as usize;
    *pos += 2;
    let mut out = vec![false; n_cols];
    for _ in 0..count {
        if *pos + 2 > data.len() {
            return None;
        }
        let col = u16::from_le_bytes(data[*pos..*pos + 2].try_into().ok()?) as usize;
        *pos += 2;
        if col < n_cols {
            out[col] = true;
        }
    }
    Some(out)
}

fn write_catalog_file(path: &Path, entries: &[CatalogEntryRef<'_>]) -> io::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    buf.extend_from_slice(CATALOG_MAGIC);
    buf.extend_from_slice(&CATALOG_VERSION.to_le_bytes());
    buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());

    for entry in entries {
        let schema = entry.schema;
        let name = schema.table_name.as_bytes();
        buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
        buf.extend_from_slice(name);
        buf.extend_from_slice(&(schema.columns.len() as u16).to_le_bytes());
        for col in &schema.columns {
            let cn = col.name.as_bytes();
            buf.extend_from_slice(&(cn.len() as u32).to_le_bytes());
            buf.extend_from_slice(cn);
            buf.push(col.type_id as u8);
            buf.push(if col.required { 1 } else { 0 });
            buf.extend_from_slice(&col.position.to_le_bytes());
        }
        // Per-table indexed column list with uniqueness flags (version 3).
        buf.extend_from_slice(&(entry.indexed_cols.len() as u16).to_le_bytes());
        for meta in &entry.indexed_cols {
            let cn = meta.name.as_bytes();
            buf.extend_from_slice(&(cn.len() as u32).to_le_bytes());
            buf.extend_from_slice(cn);
            buf.push(if meta.unique { 1 } else { 0 });
        }
        // Per-table column defaults (version 4).
        encode_defaults_section(&mut buf, entry.defaults);
        // Per-table auto-increment columns (version 5).
        encode_auto_section(&mut buf, entry.auto_cols);
    }

    // Append a CRC32 checksum of the entire payload so the reader can
    // detect corruption (the WAL and btree .idx files already do this;
    // catalog.bin was the one file missing a checksum).
    let crc = crc32fast::hash(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());

    let mut f = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    f.write_all(&buf)?;
    f.sync_data()?;
    Ok(())
}

fn read_catalog_file(path: &Path) -> io::Result<Vec<CatalogEntry>> {
    let mut f = fs::File::open(path)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;

    let mut pos = 0usize;
    // Minimum: 4 (magic) + 2 (version) + 4 (n_tables) + 4 (crc) = 14
    if buf.len() < 14 || &buf[0..4] != CATALOG_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad catalog magic",
        ));
    }

    // Verify the trailing CRC32 checksum.
    let payload = &buf[..buf.len() - 4];
    let stored_crc = u32::from_le_bytes(
        buf[buf.len() - 4..]
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "truncated catalog CRC"))?,
    );
    let computed_crc = crc32fast::hash(payload);
    if stored_crc != computed_crc {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "catalog CRC32 mismatch: expected {stored_crc:#010x}, got {computed_crc:#010x}"
            ),
        ));
    }
    // Strip the CRC suffix so the parsing loop below doesn't walk into it.
    let buf = &buf[..buf.len() - 4];
    pos += 4;
    let version = u16::from_le_bytes(
        buf[pos..pos + 2]
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "truncated catalog header"))?,
    );
    pos += 2;
    // Accept every version from 1 up to the current CATALOG_VERSION: the
    // field-reading staircase below fills in fields a newer version added
    // (indexed-col uniqueness at v3, defaults at v4, auto columns at v5) and
    // defaults them for older files, so any 1..=CATALOG_VERSION file loads.
    // A range check (not an enumerated list) is what makes this back-compat
    // hold automatically on the next bump — the previous `version != 1 &&
    // version != 2 && version != CATALOG_VERSION` form silently rejected the
    // intermediate v3/v4 files when the constant moved to 5, which would have
    // failed to open a v0.6.x database on upgrade (data loss).
    if version == 0 || version > CATALOG_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported catalog version: {version}"),
        ));
    }
    let n_tables = u32::from_le_bytes(
        buf[pos..pos + 4]
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "truncated catalog header"))?,
    ) as usize;
    pos += 4;

    // Don't size an allocation from an unvalidated count: a corrupt or hostile
    // catalog could claim billions of tables and make the `Vec::with_capacity`
    // below attempt a huge allocation (host abort — fatal in embedded mode). A
    // file of `buf.len()` bytes can describe at most that many tables (each
    // needs several header bytes), so a larger count is corrupt. Mirrors the
    // btree's node-count guard.
    if n_tables > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("catalog file corrupt: implausible table count {n_tables}"),
        ));
    }

    let mut entries = Vec::with_capacity(n_tables);
    for _ in 0..n_tables {
        let name_len = read_u32(buf, &mut pos)? as usize;
        let table_name = read_string(buf, &mut pos, name_len)?;
        let n_cols = read_u16(buf, &mut pos)? as usize;

        let mut columns = Vec::with_capacity(n_cols);
        for _ in 0..n_cols {
            let cname_len = read_u32(buf, &mut pos)? as usize;
            let name = read_string(buf, &mut pos, cname_len)?;
            let type_id_raw = read_u8(buf, &mut pos)?;
            let type_id = type_id_from_u8(type_id_raw)?;
            let required = read_u8(buf, &mut pos)? != 0;
            let position = read_u16(buf, &mut pos)?;
            columns.push(ColumnDef {
                name,
                type_id,
                required,
                position,
            });
        }

        // Version 3 appends indexed column list with uniqueness flag.
        // Version 2 has indexed column names without uniqueness (default
        // to non-unique). Version 1 has no index info at all.
        let indexed_cols: Vec<IndexedColMeta> = if version >= 3 {
            let n = read_u16(buf, &mut pos)? as usize;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                let l = read_u32(buf, &mut pos)? as usize;
                let name = read_string(buf, &mut pos, l)?;
                let unique = read_u8(buf, &mut pos)? != 0;
                v.push(IndexedColMeta { name, unique });
            }
            v
        } else if version >= 2 {
            let n = read_u16(buf, &mut pos)? as usize;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                let l = read_u32(buf, &mut pos)? as usize;
                let name = read_string(buf, &mut pos, l)?;
                v.push(IndexedColMeta {
                    name,
                    unique: false,
                });
            }
            v
        } else {
            Vec::new()
        };

        // Version 4 appends a column-defaults section after the index list.
        let defaults = if version >= 4 {
            decode_defaults_section(buf, &mut pos, columns.len()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "truncated catalog defaults")
            })?
        } else {
            Vec::new()
        };

        // Version 5 appends an auto-increment column section after that.
        let auto_cols = if version >= 5 {
            decode_auto_section(buf, &mut pos, columns.len()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "truncated catalog auto columns")
            })?
        } else {
            Vec::new()
        };

        entries.push(CatalogEntry {
            schema: Schema {
                table_name,
                columns,
            },
            indexed_cols,
            defaults,
            auto_cols,
        });
    }

    Ok(entries)
}

fn read_u8(buf: &[u8], pos: &mut usize) -> io::Result<u8> {
    if *pos >= buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated catalog",
        ));
    }
    let v = buf[*pos];
    *pos += 1;
    Ok(v)
}
fn read_u16(buf: &[u8], pos: &mut usize) -> io::Result<u16> {
    if *pos + 2 > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated catalog",
        ));
    }
    let v = u16::from_le_bytes(
        buf[*pos..*pos + 2]
            .try_into()
            .expect("bounds checked above"),
    );
    *pos += 2;
    Ok(v)
}
fn read_u32(buf: &[u8], pos: &mut usize) -> io::Result<u32> {
    if *pos + 4 > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated catalog",
        ));
    }
    let v = u32::from_le_bytes(
        buf[*pos..*pos + 4]
            .try_into()
            .expect("bounds checked above"),
    );
    *pos += 4;
    Ok(v)
}
fn read_string(buf: &[u8], pos: &mut usize, len: usize) -> io::Result<String> {
    if *pos + len > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated catalog string",
        ));
    }
    let s = std::str::from_utf8(&buf[*pos..*pos + len])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 in catalog"))?
        .to_string();
    *pos += len;
    Ok(s)
}
fn type_id_from_u8(v: u8) -> io::Result<TypeId> {
    match v {
        0 => Ok(TypeId::Empty),
        1 => Ok(TypeId::Int),
        2 => Ok(TypeId::Float),
        3 => Ok(TypeId::Bool),
        4 => Ok(TypeId::Str),
        5 => Ok(TypeId::DateTime),
        6 => Ok(TypeId::Uuid),
        7 => Ok(TypeId::Bytes),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown type id: {v}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp_catalog(name: &str) -> Catalog {
        let dir = std::env::temp_dir().join(format!("powdb_cat_{name}_{}", std::process::id()));
        Catalog::create(&dir).unwrap()
    }

    fn schema_two_cols() -> Schema {
        Schema {
            table_name: "T".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    type_id: TypeId::Int,
                    required: true,
                    position: 0,
                },
                ColumnDef {
                    name: "status".into(),
                    type_id: TypeId::Str,
                    required: false,
                    position: 1,
                },
            ],
        }
    }

    #[test]
    fn replay_records_treats_reused_tx_ids_as_ordered_spans() {
        let mut cat = temp_catalog("reused_tx_ids");
        let schema = schema_two_cols();
        cat.create_table(schema.clone()).unwrap();
        cat.checkpoint().unwrap();

        let mut committed_row = Vec::new();
        encode_row_into(
            &schema,
            &[Value::Int(1), Value::Str("committed".into())],
            &mut committed_row,
        );
        let mut incomplete_row = Vec::new();
        encode_row_into(
            &schema,
            &[Value::Int(2), Value::Str("incomplete".into())],
            &mut incomplete_row,
        );

        let records = vec![
            WalRecord {
                tx_id: 1,
                record_type: WalRecordType::Begin,
                lsn: 1,
                data: Vec::new(),
            },
            WalRecord {
                tx_id: 1,
                record_type: WalRecordType::Insert,
                lsn: 2,
                data: encode_wal_payload(
                    "T",
                    RowId {
                        page_id: 1,
                        slot_index: 0,
                    },
                    &committed_row,
                ),
            },
            WalRecord {
                tx_id: 1,
                record_type: WalRecordType::Commit,
                lsn: 3,
                data: Vec::new(),
            },
            WalRecord {
                tx_id: 1,
                record_type: WalRecordType::Begin,
                lsn: 4,
                data: Vec::new(),
            },
            WalRecord {
                tx_id: 1,
                record_type: WalRecordType::Insert,
                lsn: 5,
                data: encode_wal_payload(
                    "T",
                    RowId {
                        page_id: 1,
                        slot_index: 1,
                    },
                    &incomplete_row,
                ),
            },
        ];

        cat.apply_wal_records(&records).unwrap();
        let rows: Vec<_> = cat.scan("T").unwrap().collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1[0], Value::Int(1));
        assert_eq!(rows[0].1[1], Value::Str("committed".into()));
    }

    #[test]
    fn ddl_create_table_codec_roundtrips_defaults_and_auto() {
        let schema = schema_two_cols();
        let defaults = vec![None, Some(Value::Str("active".into()))];
        let auto_cols = vec![true, false];
        let encoded = encode_ddl_create_table(&schema, &defaults, &auto_cols);
        let (decoded_schema, decoded_defaults, decoded_auto) =
            decode_ddl_create_table(&encoded).unwrap();
        assert_eq!(decoded_schema.columns.len(), 2);
        assert_eq!(decoded_defaults, defaults);
        assert_eq!(decoded_auto, auto_cols);
    }

    #[test]
    fn ddl_create_table_codec_back_compat_without_trailing_sections() {
        // Simulate a record written before column defaults / auto existed: the
        // old encoder stopped right after the columns, with no trailing
        // sections. The new decoder must read those as "none".
        let schema = schema_two_cols();
        let full = encode_ddl_create_table(&schema, &[], &[]);
        // Each empty trailing section is a u16 count of 0 (two bytes); chop
        // both off to mimic the pre-feature on-disk shape.
        let legacy = &full[..full.len() - 4];
        let (decoded_schema, decoded_defaults, decoded_auto) =
            decode_ddl_create_table(legacy).unwrap();
        assert_eq!(decoded_schema.columns.len(), 2);
        assert!(decoded_defaults.is_empty(), "no defaults section -> empty");
        assert!(decoded_auto.is_empty(), "no auto section -> empty");
    }

    #[test]
    fn ddl_create_table_codec_back_compat_defaults_but_no_auto() {
        // A record from the column-defaults release (#129) has a defaults
        // section but no auto section; the auto-aware decoder must still read it.
        let schema = schema_two_cols();
        let defaults = vec![None, Some(Value::Str("active".into()))];
        let full = encode_ddl_create_table(&schema, &defaults, &[]);
        // Drop only the trailing auto section (its empty u16 count).
        let legacy = &full[..full.len() - 2];
        let (_schema, decoded_defaults, decoded_auto) = decode_ddl_create_table(legacy).unwrap();
        assert_eq!(decoded_defaults, defaults);
        assert!(decoded_auto.is_empty());
    }

    #[test]
    fn read_catalog_file_accepts_intermediate_versions_3_and_4() {
        // Regression: the version gate accepted only {1, 2, CATALOG_VERSION}, so
        // a catalog written at version 3 (v0.6.x) or 4 (the column-defaults
        // release) was rejected with "unsupported catalog version" — the
        // database would fail to open on upgrade from those releases = data
        // loss. The field-reading staircase already handles v3/v4; only the gate
        // was stale. Build faithful v3/v4 catalog files by hand and confirm they
        // load (defaults/auto default to empty for the versions that lack them).
        use std::io::Write as _;
        fn write_legacy_catalog(path: &std::path::Path, version: u16) {
            let mut buf: Vec<u8> = Vec::new();
            buf.extend_from_slice(CATALOG_MAGIC);
            buf.extend_from_slice(&version.to_le_bytes());
            buf.extend_from_slice(&1u32.to_le_bytes()); // n_tables
                                                        // table "T"
            buf.extend_from_slice(&1u32.to_le_bytes());
            buf.extend_from_slice(b"T");
            buf.extend_from_slice(&2u16.to_le_bytes()); // n_cols
                                                        // col id: Int, required, pos 0
            buf.extend_from_slice(&2u32.to_le_bytes());
            buf.extend_from_slice(b"id");
            buf.push(TypeId::Int as u8);
            buf.push(1);
            buf.extend_from_slice(&0u16.to_le_bytes());
            // col status: Str, not required, pos 1
            buf.extend_from_slice(&6u32.to_le_bytes());
            buf.extend_from_slice(b"status");
            buf.push(TypeId::Str as u8);
            buf.push(0);
            buf.extend_from_slice(&1u16.to_le_bytes());
            // version >= 3: indexed-column section (count 0).
            buf.extend_from_slice(&0u16.to_le_bytes());
            // version >= 4: column-defaults section (none here). v3 omits it.
            if version >= 4 {
                encode_defaults_section(&mut buf, &[None, None]);
            }
            // v3/v4 never wrote the v5 auto section.
            let crc = crc32fast::hash(&buf);
            buf.extend_from_slice(&crc.to_le_bytes());
            let mut f = fs::File::create(path).unwrap();
            f.write_all(&buf).unwrap();
        }

        for version in [3u16, 4u16] {
            let path = std::env::temp_dir().join(format!(
                "powdb_cat_v{version}_compat_{}.bin",
                std::process::id()
            ));
            write_legacy_catalog(&path, version);
            let entries = read_catalog_file(&path)
                .unwrap_or_else(|e| panic!("version {version} catalog must load, got: {e}"));
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].schema.table_name, "T");
            assert_eq!(entries[0].schema.columns.len(), 2);
            assert!(
                entries[0].auto_cols.is_empty(),
                "v{version} has no auto cols"
            );
            fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn read_catalog_file_rejects_implausible_table_count() {
        // A corrupt/hostile catalog must not be trusted to size an allocation:
        // `Vec::with_capacity(n_tables)` on an unvalidated u32 would attempt a
        // huge allocation and abort the host. A file can describe at most as
        // many tables as it has bytes, so a count exceeding the payload length
        // is rejected with a clear error before any allocation. (We use a small
        // implausible count over a tiny buffer; a genuinely huge count would
        // abort the test runner pre-fix, but it hits the very same guard.)
        use std::io::Write as _;
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(CATALOG_MAGIC);
        buf.extend_from_slice(&CATALOG_VERSION.to_le_bytes());
        buf.extend_from_slice(&1000u32.to_le_bytes()); // claims 1000 tables…
                                                       // …but no table data follows (payload is only 10 bytes).
        let crc = crc32fast::hash(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        let path =
            std::env::temp_dir().join(format!("powdb_cat_badcount_{}.bin", std::process::id()));
        fs::File::create(&path).unwrap().write_all(&buf).unwrap();

        let msg = match read_catalog_file(&path) {
            Ok(_) => panic!("implausible table count must be rejected, got Ok"),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("implausible table count"),
            "expected an implausible-table-count error, got: {msg}"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn data_dir_and_max_lsn_accessors() {
        let dir = std::env::temp_dir().join(format!("powdb_cat_maxlsn_{}", std::process::id()));
        let mut cat = Catalog::create(&dir).unwrap();

        // data_dir() reflects the directory the catalog was created in.
        assert_eq!(cat.data_dir(), dir.as_path());

        // A fresh catalog has stamped no page LSNs yet.
        assert_eq!(cat.max_lsn(), 0);

        let schema = Schema {
            table_name: "users".into(),
            columns: vec![ColumnDef {
                name: "name".into(),
                type_id: TypeId::Str,
                required: true,
                position: 0,
            }],
        };
        cat.create_table(schema).unwrap();

        cat.insert("users", &vec![Value::Str("Alice".into())])
            .unwrap();
        cat.sync_wal().unwrap();

        // An inserted (and synced) row stamps a page LSN, raising the
        // durability high-water mark above zero.
        assert!(cat.max_lsn() > 0);
    }

    #[test]
    fn test_create_table_and_insert() {
        let mut cat = temp_catalog("basic");
        let schema = Schema {
            table_name: "users".into(),
            columns: vec![
                ColumnDef {
                    name: "name".into(),
                    type_id: TypeId::Str,
                    required: true,
                    position: 0,
                },
                ColumnDef {
                    name: "age".into(),
                    type_id: TypeId::Int,
                    required: false,
                    position: 1,
                },
            ],
        };
        cat.create_table(schema).unwrap();

        let row = vec![Value::Str("Alice".into()), Value::Int(30)];
        let rid = cat.insert("users", &row).unwrap();

        let result = cat.get("users", rid).unwrap();
        assert_eq!(result[0], Value::Str("Alice".into()));
        assert_eq!(result[1], Value::Int(30));
    }

    #[test]
    fn test_scan_table() {
        let mut cat = temp_catalog("scan");
        let schema = Schema {
            table_name: "items".into(),
            columns: vec![
                ColumnDef {
                    name: "name".into(),
                    type_id: TypeId::Str,
                    required: true,
                    position: 0,
                },
                ColumnDef {
                    name: "price".into(),
                    type_id: TypeId::Float,
                    required: true,
                    position: 1,
                },
            ],
        };
        cat.create_table(schema).unwrap();

        for i in 0..50 {
            cat.insert(
                "items",
                &vec![
                    Value::Str(format!("item_{i}")),
                    Value::Float(i as f64 * 1.5),
                ],
            )
            .unwrap();
        }

        let rows: Vec<_> = cat.scan("items").unwrap().collect();
        assert_eq!(rows.len(), 50);
    }

    #[test]
    fn test_index_lookup() {
        let mut cat = temp_catalog("idx");
        let schema = Schema {
            table_name: "users".into(),
            columns: vec![
                ColumnDef {
                    name: "email".into(),
                    type_id: TypeId::Str,
                    required: true,
                    position: 0,
                },
                ColumnDef {
                    name: "name".into(),
                    type_id: TypeId::Str,
                    required: true,
                    position: 1,
                },
            ],
        };
        cat.create_table(schema).unwrap();
        cat.create_index("users", "email").unwrap();

        cat.insert(
            "users",
            &vec![
                Value::Str("alice@example.com".into()),
                Value::Str("Alice".into()),
            ],
        )
        .unwrap();
        cat.insert(
            "users",
            &vec![
                Value::Str("bob@example.com".into()),
                Value::Str("Bob".into()),
            ],
        )
        .unwrap();

        let result = cat
            .index_lookup("users", "email", &Value::Str("bob@example.com".into()))
            .unwrap();
        assert!(result.is_some());
        let row = result.unwrap();
        assert_eq!(row[1], Value::Str("Bob".into()));
    }

    #[test]
    fn test_delete_row() {
        let mut cat = temp_catalog("delete");
        let schema = Schema {
            table_name: "t".into(),
            columns: vec![ColumnDef {
                name: "v".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            }],
        };
        cat.create_table(schema).unwrap();
        let r1 = cat.insert("t", &vec![Value::Int(1)]).unwrap();
        let r2 = cat.insert("t", &vec![Value::Int(2)]).unwrap();
        cat.delete("t", r1).unwrap();
        assert!(cat.get("t", r1).is_none());
        assert!(cat.get("t", r2).is_some());
    }

    #[test]
    fn test_update_row() {
        let mut cat = temp_catalog("update");
        let schema = Schema {
            table_name: "t".into(),
            columns: vec![ColumnDef {
                name: "v".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            }],
        };
        cat.create_table(schema).unwrap();
        let rid = cat.insert("t", &vec![Value::Int(1)]).unwrap();
        let new_rid = cat.update("t", rid, &vec![Value::Int(99)]).unwrap();
        let row = cat.get("t", new_rid).unwrap();
        assert_eq!(row[0], Value::Int(99));
    }

    #[test]
    fn test_persist_and_reopen() {
        let dir = std::env::temp_dir().join(format!("powdb_cat_persist_{}", std::process::id()));
        // Fresh dir
        let _ = std::fs::remove_dir_all(&dir);

        {
            let mut cat = Catalog::create(&dir).unwrap();
            cat.create_table(Schema {
                table_name: "users".into(),
                columns: vec![
                    ColumnDef {
                        name: "name".into(),
                        type_id: TypeId::Str,
                        required: true,
                        position: 0,
                    },
                    ColumnDef {
                        name: "age".into(),
                        type_id: TypeId::Int,
                        required: false,
                        position: 1,
                    },
                ],
            })
            .unwrap();
            cat.insert("users", &vec![Value::Str("Alice".into()), Value::Int(30)])
                .unwrap();
            cat.insert("users", &vec![Value::Str("Bob".into()), Value::Int(25)])
                .unwrap();
        }

        // Reopen — schema and rows should both still be there
        let cat = Catalog::open(&dir).unwrap();
        let schema = cat.schema("users").unwrap();
        assert_eq!(schema.columns.len(), 2);
        assert_eq!(schema.columns[0].name, "name");
        assert_eq!(schema.columns[0].type_id, TypeId::Str);
        assert_eq!(schema.columns[1].type_id, TypeId::Int);

        let rows: Vec<_> = cat.scan("users").unwrap().collect();
        assert_eq!(rows.len(), 2);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_open_missing_dir_errors() {
        let dir = std::env::temp_dir().join(format!("powdb_cat_missing_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // No catalog.bin yet
        assert!(Catalog::open(&dir).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_list_tables() {
        let mut cat = temp_catalog("list");
        cat.create_table(Schema {
            table_name: "a".into(),
            columns: vec![ColumnDef {
                name: "x".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            }],
        })
        .unwrap();
        cat.create_table(Schema {
            table_name: "b".into(),
            columns: vec![ColumnDef {
                name: "y".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            }],
        })
        .unwrap();
        let mut tables = cat.list_tables();
        tables.sort();
        assert_eq!(tables, vec!["a", "b"]);
    }

    #[test]
    fn test_path_traversal_table_name_rejected() {
        let mut cat = temp_catalog("path_trav");
        // Names with path separators must be rejected.
        let bad_names = vec![
            "../etc/passwd",
            "foo/bar",
            "table\0name",
            "",
            "123starts_with_digit",
            "has-dashes",
            "has spaces",
            "has.dots",
        ];
        for name in bad_names {
            let schema = Schema {
                table_name: name.into(),
                columns: vec![ColumnDef {
                    name: "x".into(),
                    type_id: TypeId::Int,
                    required: true,
                    position: 0,
                }],
            };
            let result = cat.create_table(schema);
            assert!(result.is_err(), "expected error for table name '{name}'");
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        }
        // Valid names must still work.
        let good_names = vec!["users", "_private", "Table_123", "_"];
        for name in good_names {
            let schema = Schema {
                table_name: name.into(),
                columns: vec![ColumnDef {
                    name: "x".into(),
                    type_id: TypeId::Int,
                    required: true,
                    position: 0,
                }],
            };
            assert!(
                cat.create_table(schema).is_ok(),
                "expected ok for table name '{name}'"
            );
        }
    }

    #[test]
    fn test_path_traversal_column_name_rejected() {
        let mut cat = temp_catalog("col_path_trav");
        let schema = Schema {
            table_name: "valid_table".into(),
            columns: vec![ColumnDef {
                name: "../bad".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            }],
        };
        let result = cat.create_table(schema);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn test_drop_table_validates_name() {
        let mut cat = temp_catalog("drop_trav");
        let result = cat.drop_table("../etc/passwd");
        assert!(result.is_err());
        // Should fail with InvalidInput (validation), not NotFound.
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
}
