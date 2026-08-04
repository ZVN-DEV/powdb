use crate::btree::{BTree, IndexStats};
use crate::error::StorageError;
use crate::heap::{DirtyPageBudget, HeapFile};
use crate::page::{OVERFLOW_CHAIN_END, OVERFLOW_PAYLOAD_CAP};
use crate::row::{encode_row_into, encode_row_v2_into, plan_spill, OverflowStub, MAX_VALUE_SIZE};
use crate::stored_json_path::{StoredJsonPathSegmentV1, StoredJsonPathV1};
use crate::table::Table;
use crate::types::*;
use crate::wal::{Wal, WalDurabilityTicket, WalRecord, WalRecordType, WalSyncMode};
use rustc_hash::FxHashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

static NEXT_STRUCTURE_GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_structure_generation() -> u64 {
    NEXT_STRUCTURE_GENERATION.fetch_add(1, Ordering::Relaxed)
}

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
pub const LEGACY_CATALOG_VERSION: u16 = 5;
/// Version 6 (activated lazily since v0.13.0) appends the expression-index
/// section plus a next-index-id header field. A database that declares an
/// expression index but no relationship link stays at exactly this version.
pub const EXPRESSION_INDEX_CATALOG_VERSION: u16 = 6;
/// Version 7 appends a relationship-link section after the table entries (and
/// after each table's expression indexes) and before the trailing CRC. It
/// activates lazily on the first `create_link`, mirroring how v6 activates on
/// the first expression index; a link-free database stays byte-for-byte a v6
/// (or older) file forever.
pub const CATALOG_VERSION: u16 = 7;

/// Persisted metadata for a JSON-path expression index. Expression index files
/// are addressed only by `index_id`; canonical expression text never reaches a
/// filesystem path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpressionIndexMeta {
    pub index_id: u64,
    pub unique: bool,
    pub canonical_version: u16,
    pub canonical_text: String,
    pub json_path: StoredJsonPathV1,
}

/// Cardinality of a relationship link: whether the target key is backed by a
/// unique index/constraint. The fact lives in the index metadata and nowhere
/// else, and [`Catalog::derive_link_kind`] is the only thing that reads it.
///
/// The engine keeps no cached copy of the answer. The `u8` written into
/// [`LinkDef::kind`] is an advisory record of what the derivation returned when
/// the link was declared: it stays in the v7 catalog format so old and new
/// files stay byte-compatible, it is never resynced, and nothing in the engine
/// may branch on it. See `docs/FORMAT.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// N:1 scalar hop — the target key is unique, so a hop resolves to at most
    /// one target row (`0` on disk).
    ToOne,
    /// 1:N nested block — the target key is non-unique, so a hop can fan out to
    /// many target rows (`1` on disk).
    ToMany,
}

impl LinkKind {
    fn to_u8(self) -> u8 {
        match self {
            LinkKind::ToOne => 0,
            LinkKind::ToMany => 1,
        }
    }

    fn from_u8(tag: u8) -> io::Result<Self> {
        match tag {
            0 => Ok(LinkKind::ToOne),
            1 => Ok(LinkKind::ToMany),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown link kind tag: {other}"),
            )),
        }
    }
}

/// Persisted relationship-link metadata. A link is a read-only naming layer over
/// columns that already exist: it names a traversal path (`owner.name -> target`)
/// resolved through `local_key = target_key`. Links add no storage and enforce no
/// referential integrity on write; dropping a referenced table or column is
/// refused while the link exists (the same discipline indexes use).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkDef {
    /// Owner table the link is declared on. Registry key part 1.
    pub owner_type: String,
    /// Traversal name (`o.<name>...`). Unique per owner type. Registry key part 2.
    pub name: String,
    /// Target table the link resolves to. Must exist at declare time.
    pub target_type: String,
    /// Column on the owner supplying the join value.
    pub local_key: String,
    /// Column on the target matched against `local_key`.
    pub target_key: String,
    /// ADVISORY ONLY. Whatever [`Catalog::derive_link_kind`] returned at the
    /// moment the link was declared, kept so the v7 on-disk layout does not
    /// change. It is deliberately never refreshed, so it goes stale as soon as
    /// `alter <Target> add unique .<key>` runs after the link, and it is wrong
    /// in every database written by the declare-order-dependent versions.
    ///
    /// Never branch on this field. Every correctness decision (traversal gates,
    /// `describe`, `schema links`) must call [`Catalog::link_kind`] or
    /// [`Catalog::derive_link_kind`], which read index uniqueness live.
    pub kind: LinkKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexKeySource {
    Column {
        column: String,
    },
    Expression {
        index_id: u64,
        canonical_version: u16,
        canonical_text: String,
        json_path: StoredJsonPathV1,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexMetadata {
    pub unique: bool,
    pub source: IndexKeySource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexOrderDirection {
    Asc,
    Desc,
}

/// Expression-index artifacts live in a filename namespace disjoint from
/// legacy column indexes. Column indexes always end in `.idx`; expression
/// indexes always end in `.eidx`. Keeping the extension distinct prevents a
/// table/column underscore decomposition from ever aliasing an expression ID.
pub fn expression_index_file_name(table: &str, index_id: u64) -> String {
    format!("{table}_{index_id}.eidx")
}

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

#[cfg(test)]
thread_local! {
    static CATALOG_PERSIST_FAILPOINT: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn take_catalog_persist_failpoint(stage: u8) -> bool {
    CATALOG_PERSIST_FAILPOINT.with(|failpoint| {
        if failpoint.get() == stage {
            failpoint.set(0);
            true
        } else {
            false
        }
    })
}

enum CatalogPersistError {
    BeforeActivation(io::Error),
    AfterActivation(io::Error),
}

impl CatalogPersistError {
    fn into_io_error(self) -> io::Error {
        match self {
            Self::BeforeActivation(error) | Self::AfterActivation(error) => error,
        }
    }
}

fn max_record_lsn(records: &[WalRecord]) -> Option<u64> {
    records.iter().map(|record| record.lsn).max()
}

/// System catalog: registry of all tables.
///
/// Mission C Phase 18: tables live in a `Vec<Table>` addressed by a `slot`
/// index, with a parallel `FxHashMap<String, usize>` for name-based resolution.
/// DROP TABLE can move slots, so prepared fast paths pair a cached slot with the
/// O(1) structural generation below and fall back when any DDL invalidates it.
///
/// Earlier design (pre-Phase 18) held tables in a `FxHashMap<String, Table>`
/// directly. That meant the `insert_batch_1k` hot path paid an
/// `FxHash("User")` + bucket walk per row just to dispatch into the
/// table — about 20-40ns out of a 233ns budget.
pub struct Catalog {
    /// All tables, in insertion order. Indexed by `slot: usize`.
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
    /// Overflow-chain pages to return to their table's free list once the
    /// current EXPLICIT transaction commits (design 3.6 pending-free list).
    /// Populated only while `active_tx_id.is_some()`: a chain-replacing update
    /// or a delete inside a transaction cannot free its old chain immediately,
    /// because ROLLBACK resurrects the old row and its stub must still address a
    /// live chain. Autocommit mutations free immediately (no rollback window).
    /// Drained by `commit_transaction`; discarded (via reopen) by ROLLBACK.
    /// Entries are `(table_slot, chain_pages)`.
    pending_free_overflow: Vec<(usize, Vec<u32>)>,
    /// Catalog format currently active on disk. v6 activates lazily on the
    /// first successful expression-index metadata creation.
    active_catalog_version: u16,
    /// Global, durable, monotonically increasing expression-index identity.
    next_index_id: u64,
    /// Relationship-link registry, in declaration order (so serialization and
    /// the `links()` iterator are deterministic). Keyed logically by
    /// `(owner_type, name)`; uniqueness of that pair is enforced in
    /// `create_link`. Populated from disk on `open`. v7 activates on the first
    /// entry; a link-free catalog never writes the links section.
    links: Vec<LinkDef>,
    /// Process-local catalog structure identity. Any table/schema/default/
    /// auto/index DDL replaces this token, invalidating cached prepared
    /// metadata in O(1). Opening a replacement Catalog (including rollback)
    /// also receives a fresh token.
    structure_generation: u64,
    /// True when opened via [`Catalog::open_read_only`] for snapshot serving.
    /// The heap/index/WAL files are read-only handles, no LSN stamping or
    /// overflow sweep ran at open, and [`Drop`] skips the checkpoint (which would
    /// otherwise flush pages and truncate the WAL, mutating the directory).
    read_only: bool,
    /// Ceiling on unflushed heap pages, charged across every table here. An
    /// explicit transaction that exceeds it is refused rather than allowed to
    /// grow the per-table dirty buffers until the process is OOM-killed.
    dirty_budget: Arc<DirtyPageBudget>,
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
            pending_free_overflow: Vec::new(),
            checkpointed: false,
            durable_lsn: 0,
            active_catalog_version: LEGACY_CATALOG_VERSION,
            next_index_id: 1,
            links: Vec::new(),
            structure_generation: next_structure_generation(),
            read_only: false,
            dirty_budget: Arc::new(DirtyPageBudget::default()),
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
        let catalog_file = read_catalog_file(&cat_path)?;
        let active_catalog_version = catalog_file.version;
        let next_index_id = catalog_file.next_index_id;
        let links = catalog_file.links;
        let entries = catalog_file.entries;
        let durable_lsn = read_durable_lsn(data_dir)?;
        let mut tables: Vec<Table> = Vec::with_capacity(entries.len());
        let mut name_to_slot =
            FxHashMap::with_capacity_and_hasher(entries.len(), Default::default());
        let dirty_budget = Arc::new(DirtyPageBudget::default());
        for CatalogEntry {
            schema,
            indexed_cols,
            expression_indexes: expression_metas,
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
            let mut table =
                Table::open_with_indexes(schema, data_dir, &indexed_cols, &expression_metas)?;
            table.heap.set_dirty_budget(Arc::clone(&dirty_budget));
            table.set_defaults(defaults);
            table.set_auto_cols(auto_cols);
            name_to_slot.insert(name.clone(), tables.len());
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
            pending_free_overflow: Vec::new(),
            checkpointed: false,
            durable_lsn,
            active_catalog_version,
            next_index_id,
            links,
            structure_generation: next_structure_generation(),
            read_only: false,
            dirty_budget,
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
        // Auto-sweep overflow orphans after recovery: a crash is exactly when
        // a chain page can end up flushed but referenced by no committed row
        // (its Insert was uncommitted, or its Delete committed). Reclaim them
        // now (design 3.6). Best-effort — a sweep failure must not block open.
        if let Err(e) = cat.sweep_all() {
            warn!(error = %e, "post-recovery overflow sweep failed (non-fatal)");
        }
        Ok(cat)
    }

    /// Open a catalog **read-only** for snapshot serving (tier 1 of the replica
    /// story). This is for a *quiescent* directory: a restored backup or a
    /// checkpointed replica, both guaranteed WAL-clean.
    ///
    /// Unlike [`Catalog::open`], this path:
    /// - opens every heap and index file read-only (no writable descriptor);
    /// - never calls `set_permissions` (it validates the directory instead);
    /// - **refuses** a non-empty WAL rather than replaying and truncating it ,
    ///   an unclean directory must be recovered by a read-write engine first;
    /// - stamps no page LSNs, sweeps no overflow orphans, and truncates nothing.
    ///
    /// The result never mutates the directory, so N read-only processes can serve
    /// the same snapshot concurrently.
    pub fn open_read_only(data_dir: &Path) -> io::Result<Self> {
        crate::validate_data_dir_read_only(data_dir)?;
        let cat_path = data_dir.join(CATALOG_FILE);
        if !cat_path.exists() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no catalog file"));
        }

        // Refuse a non-empty WAL: the directory has un-checkpointed mutations
        // that only a read-write open may safely replay. Naming the remedy keeps
        // the operator from guessing.
        let wal_path = data_dir.join(WAL_FILE);
        if crate::wal::wal_has_committed_records(&wal_path)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cannot open read-only: the WAL is not empty (the directory has \
                 un-checkpointed writes). Open the directory once with a read-write \
                 engine to recover, or restore from a backup, then serve it read-only",
            ));
        }

        let catalog_file = read_catalog_file(&cat_path)?;
        let active_catalog_version = catalog_file.version;
        let next_index_id = catalog_file.next_index_id;
        let links = catalog_file.links;
        let entries = catalog_file.entries;
        let durable_lsn = read_durable_lsn(data_dir)?;
        let mut tables: Vec<Table> = Vec::with_capacity(entries.len());
        let mut name_to_slot =
            FxHashMap::with_capacity_and_hasher(entries.len(), Default::default());
        let dirty_budget = Arc::new(DirtyPageBudget::default());
        for CatalogEntry {
            schema,
            indexed_cols,
            expression_indexes: expression_metas,
            defaults,
            auto_cols,
        } in entries
        {
            let name = schema.table_name.clone();
            let mut table = Table::open_with_indexes_read_only(
                schema,
                data_dir,
                &indexed_cols,
                &expression_metas,
            )?;
            table.heap.set_dirty_budget(Arc::clone(&dirty_budget));
            table.set_defaults(defaults);
            table.set_auto_cols(auto_cols);
            name_to_slot.insert(name.clone(), tables.len());
            tables.push(table);
        }
        let wal = Wal::open_read_only(&wal_path, WAL_BATCH_SIZE)?;
        Ok(Catalog {
            tables,
            name_to_slot,
            data_dir: data_dir.to_path_buf(),
            wal,
            next_tx_id: 1,
            active_tx_id: None,
            tx_start_len: None,
            pending_autocommit_tx_ids: Vec::new(),
            pending_free_overflow: Vec::new(),
            checkpointed: false,
            durable_lsn,
            active_catalog_version,
            next_index_id,
            links,
            structure_generation: next_structure_generation(),
            read_only: true,
            dirty_budget,
        })
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
                    WalRecordType::Insert
                    | WalRecordType::Update
                    | WalRecordType::Delete
                    | WalRecordType::OverflowWrite
                    | WalRecordType::OverflowFree
                        if rec.tx_id == 0 =>
                    {
                        committed_row_records[index] = true;
                    }
                    WalRecordType::Insert
                    | WalRecordType::Update
                    | WalRecordType::Delete
                    | WalRecordType::OverflowWrite
                    | WalRecordType::OverflowFree => {
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
                    WalRecordType::Insert
                        | WalRecordType::Update
                        | WalRecordType::Delete
                        | WalRecordType::OverflowWrite
                        | WalRecordType::OverflowFree
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
                WalRecordType::OverflowWrite => {
                    // Physical redo of one chain chunk. Applied by page id
                    // under the per-page LSN skip, so double replay is a
                    // no-op. Ordered before its Insert/Update in the log, so
                    // the stub the row carries always points at live pages.
                    if let Some((table_name, page_id, next_page, chunk)) =
                        decode_overflow_write_payload(&rec.data)
                    {
                        if let Some(slot) = self.name_to_slot.get(&table_name).copied() {
                            let tbl = &mut self.tables[slot];
                            if rec.lsn > 0 && tbl.heap.overflow_page_lsn(page_id) >= rec.lsn {
                                skipped += 1;
                                continue;
                            }
                            tbl.heap
                                .write_overflow_page(page_id, next_page, &chunk, rec.lsn)?;
                        }
                    }
                }
                WalRecordType::OverflowFree => {
                    // Return a freed chain's pages to the in-memory free list.
                    // Only reached for committed records (uncommitted frees
                    // are skipped above), so a live row can never lose its
                    // chain to a rolled-back free.
                    if let Some((table_name, pages)) = decode_overflow_free_payload(&rec.data) {
                        if let Some(slot) = self.name_to_slot.get(&table_name).copied() {
                            self.tables[slot].heap.release_overflow_pages(&pages);
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
                                table.heap.set_dirty_budget(Arc::clone(&self.dirty_budget));
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
                            for index_id in self.tables[slot].expression_index_ids() {
                                let idx_path = self
                                    .data_dir
                                    .join(expression_index_file_name(&table_name, index_id));
                                let _ = fs::remove_file(idx_path);
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
                            {
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

                            let removed_ids =
                                self.tables[slot].remove_expression_indexes_for_root(&col_name);
                            for index_id in removed_ids {
                                let idx_path = self
                                    .data_dir
                                    .join(expression_index_file_name(&table_name, index_id));
                                let _ = fs::remove_file(idx_path);
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

    /// Refuse DDL while an explicit transaction is active.
    ///
    /// DDL is not transactional here: `drop_table` unlinks the heap and
    /// rewrites the catalog immediately, and `alter_table_*` rewrites every
    /// row in place. ROLLBACK restores the catalog from disk, which by then
    /// already reflects the DDL, so a `begin / drop / rollback` sequence used
    /// to report success at every step and leave the table permanently gone.
    /// Refusing the statement is the correct fix; making DDL transactional is
    /// a separate, deliberately deferred decision.
    fn ensure_no_active_transaction_for_ddl(&self, verb: &'static str) -> io::Result<()> {
        if self.active_tx_id.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                StorageError::DdlInTransaction { verb },
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
    /// Free (or defer freeing) the overflow-chain pages a mutation just
    /// orphaned. In autocommit there is no rollback window, so the pages return
    /// to the table's free list immediately and the next spill reuses them
    /// (bounding steady-state churn). Inside an explicit transaction the free is
    /// held on `pending_free_overflow` until COMMIT: a ROLLBACK reopens the
    /// catalog from disk, discarding this list, so the resurrected old row still
    /// points at a live chain. Reuse is crash-safe without an `OverflowFree`
    /// record because a later spill that overwrites a reused page logs its own
    /// per-page `OverflowWrite` (LSN-idempotent), and post-recovery `sweep`
    /// reclaims anything the in-memory list lost.
    fn free_overflow_chain(&mut self, slot: usize, pages: Vec<u32>) {
        if pages.is_empty() {
            return;
        }
        if self.active_tx_id.is_some() {
            self.pending_free_overflow.push((slot, pages));
        } else {
            self.tables[slot].release_overflow_pages(&pages);
        }
    }

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
        // From here until COMMIT/ROLLBACK the dirty pages are the only copy of
        // the transaction's state, so they may not be spilled to disk to
        // relieve the budget. See `DirtyPageBudget`.
        self.dirty_budget.set_rollback_pinned(true);
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
        self.dirty_budget.set_rollback_pinned(false);
        if let Some(id) = self.active_tx_id.take() {
            if !self.wal.is_off() {
                self.wal.append(id, WalRecordType::Commit, &[])?;
                self.wal.flush()?;
            }
        }
        self.tx_start_len = None;
        // The transaction committed: its rows are durable and can no longer be
        // resurrected by ROLLBACK, so the old chains they replaced/removed are
        // safe to reclaim. (Populated only while a tx was active.)
        for (slot, pages) in std::mem::take(&mut self.pending_free_overflow) {
            self.tables[slot].release_overflow_pages(&pages);
        }
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

    /// Ceiling on unflushed heap pages held across every table, in bytes.
    /// Defaults to [`crate::heap::DEFAULT_DIRTY_PAGE_BUDGET`]. A transaction
    /// that would exceed it fails with `StorageError::TransactionTooLarge`
    /// instead of pinning memory until the process is OOM-killed.
    pub fn set_dirty_page_budget_bytes(&mut self, limit_bytes: usize) {
        self.dirty_budget.set_limit_bytes(limit_bytes);
    }

    pub fn dirty_page_budget_bytes(&self) -> usize {
        self.dirty_budget.limit_bytes()
    }

    /// Unflushed heap pages currently buffered across every table.
    pub fn dirty_pages_buffered(&self) -> usize {
        self.dirty_budget.charged_pages()
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
        // upcoming Drop of `*self` has nothing dirty to flush. This covers
        // both the heap pages AND the btree index mutations: the Drop below
        // runs `checkpoint()` (active_tx_id was already taken above), whose
        // `save_dirty_indexes` would otherwise flush the rolled-back index
        // writes to the `.idx` files — poisoning the unique index. The
        // freshly-opened replacement catalog reloads clean trees from the
        // untouched on-disk `.idx`, so discarding the dirty flags here is
        // what actually reverts the transaction's index writes.
        for tbl in &mut self.tables {
            tbl.heap.discard_dirty();
            tbl.discard_dirty_indexes();
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
        let mut restored = if prearchived {
            let mut already_archived = |_dir: &Path, _records: &[WalRecord]| Ok(());
            let archive: WalArchiveCallback<'_> = &mut already_archived;
            Self::open_inner(&data_dir, Some(archive))?
        } else {
            Self::open_inner(&data_dir, archive)?
        };
        // Row-only rollback reopens the catalog to discard dirty heap/index
        // state, but it does not change prepared-query metadata. Preserve the
        // O(1) token in that common case so existing PreparedQuery handles keep
        // their fast path. Any schema/default/auto/index difference retains the
        // fresh token assigned by open_inner and invalidates cached metadata.
        if self.has_same_prepared_structure(&restored) {
            restored.structure_generation = self.structure_generation;
        }
        let dirty_budget_limit = self.dirty_budget.limit_bytes();
        *self = restored;
        self.wal.set_sync_mode(sync_mode);
        // The replacement catalog brought a fresh (unpinned, empty) budget;
        // carry the configured ceiling across, like the sync mode above.
        self.dirty_budget.set_limit_bytes(dirty_budget_limit);
        Ok(())
    }

    fn abandon_active_transaction_for_drop(&mut self) -> io::Result<()> {
        self.dirty_budget.set_rollback_pinned(false);
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
        self.ensure_no_active_transaction_for_ddl("create table")?;
        self.invalidate_structure();
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
        table.heap.set_dirty_budget(Arc::clone(&self.dirty_budget));
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
    fn persist_at_activation_boundary(&self) -> Result<(), CatalogPersistError> {
        let cat_path = self.data_dir.join(CATALOG_FILE);
        let tmp_path = self.data_dir.join(format!("{CATALOG_FILE}.tmp"));
        let entries: Vec<CatalogEntryRef<'_>> = self
            .tables
            .iter()
            .map(|t| CatalogEntryRef {
                schema: &t.schema,
                indexed_cols: t.indexed_column_metas(),
                expression_indexes: t.expression_index_metas(),
                defaults: t.defaults(),
                auto_cols: t.auto_cols(),
            })
            .collect();
        write_catalog_file(
            &tmp_path,
            self.active_catalog_version,
            self.next_index_id,
            &entries,
            &self.links,
        )
        .map_err(CatalogPersistError::BeforeActivation)?;
        #[cfg(test)]
        if take_catalog_persist_failpoint(1) {
            return Err(CatalogPersistError::BeforeActivation(io::Error::other(
                "injected catalog failure before rename",
            )));
        }
        fs::rename(&tmp_path, &cat_path).map_err(CatalogPersistError::BeforeActivation)?;
        #[cfg(test)]
        let directory_sync = if take_catalog_persist_failpoint(2) {
            Err(io::Error::other(
                "injected catalog directory sync failure after rename",
            ))
        } else {
            sync_directory(&self.data_dir)
        };
        #[cfg(not(test))]
        let directory_sync = sync_directory(&self.data_dir);
        directory_sync.map_err(CatalogPersistError::AfterActivation)
    }

    fn persist(&self) -> io::Result<()> {
        self.persist_at_activation_boundary()
            .map_err(CatalogPersistError::into_io_error)
    }

    /// Resolve a table name to its current slot index. DROP TABLE uses
    /// swap-remove, so prepared-query fast paths pair this value with
    /// [`Self::structure_generation`] before every slot-indexed access.
    #[inline]
    pub fn table_slot(&self, name: &str) -> Option<usize> {
        self.name_to_slot.get(name).copied()
    }

    /// O(1) prepared-metadata validity token. It is process-local by design:
    /// prepared queries do not cross process boundaries, and a reopened or
    /// rollback-replaced Catalog must invalidate every cached slot/offset.
    #[inline]
    pub fn structure_generation(&self) -> u64 {
        self.structure_generation
    }

    #[inline]
    fn invalidate_structure(&mut self) {
        self.structure_generation = next_structure_generation();
    }

    fn has_same_prepared_structure(&self, other: &Self) -> bool {
        // Links are part of the structure prepared plans resolve against, even
        // though DDL inside a transaction is refused today and so no rollback
        // can currently change them.
        self.links == other.links
            && self.tables.len() == other.tables.len()
            && self.tables.iter().zip(&other.tables).all(|(left, right)| {
                let left_schema = &left.schema;
                let right_schema = &right.schema;
                left_schema.table_name == right_schema.table_name
                    && left_schema.columns.len() == right_schema.columns.len()
                    && left_schema.columns.iter().zip(&right_schema.columns).all(
                        |(left_col, right_col)| {
                            left_col.name == right_col.name
                                && left_col.type_id == right_col.type_id
                                && left_col.required == right_col.required
                                && left_col.position == right_col.position
                        },
                    )
                    && left.defaults() == right.defaults()
                    && left.auto_cols() == right.auto_cols()
                    && {
                        let left_indexes = left.indexed_column_metas();
                        let right_indexes = right.indexed_column_metas();
                        left_indexes.len() == right_indexes.len()
                            && left_indexes.iter().zip(&right_indexes).all(
                                |(left_index, right_index)| {
                                    left_index.name == right_index.name
                                        && left_index.unique == right_index.unique
                                },
                            )
                    }
                    && left.expression_index_metas() == right.expression_index_metas()
            })
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

    /// Whether `table` may hold v2 (spilled) rows (see
    /// [`Table::has_overflow_rows`]). Unknown table ⇒ false. The executor gates
    /// its v1-only raw-byte fast paths on this.
    #[inline]
    pub fn table_has_overflow(&self, table: &str) -> bool {
        self.get_table(table)
            .map(|t| t.has_overflow_rows())
            .unwrap_or(false)
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
        let slot = self.slot_of(table)?;
        Ok(&mut self.tables[slot])
    }

    /// Mark-and-sweep one table's overflow pages, returning the number of pages
    /// reclaimed (design 3.6, door D12). Reclaimed pages are logged as a single
    /// `OverflowFree` record so the reclamation is crash-safe, then returned to
    /// the free list for reuse. Intended to run under the table write lock.
    pub fn sweep(&mut self, table: &str) -> io::Result<usize> {
        let slot = self.slot_of(table)?;
        let reclaimed = self.tables[slot].sweep_overflow()?;
        if !reclaimed.is_empty() && !self.wal.is_off() {
            let payload = encode_overflow_free_payload(table, &reclaimed);
            self.wal.append(0, WalRecordType::OverflowFree, &payload)?;
            self.wal.flush()?;
        }
        Ok(reclaimed.len())
    }

    /// Sweep overflow pages across every table. Returns the total reclaimed.
    pub fn sweep_all(&mut self) -> io::Result<usize> {
        let names: Vec<String> = self
            .tables
            .iter()
            .map(|t| t.schema.table_name.clone())
            .collect();
        let mut total = 0;
        for name in names {
            total += self.sweep(&name)?;
        }
        Ok(total)
    }

    fn slot_of(&self, table: &str) -> io::Result<usize> {
        self.name_to_slot.get(table).copied().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("table '{table}' not found"),
            )
        })
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
        let slot = self.slot_of(table)?;
        let _ = self.tables[slot].preflight_insert(values)?;
        // Allocate the tx id up front: any overflow chains for a spilled row
        // must be logged under the SAME tx (and before the Insert record) so
        // an uncommitted big row's chain writes are skipped on replay.
        let tx_id = self.next_tx();
        let row_bytes = {
            let Catalog { tables, wal, .. } = self;
            encode_row_with_spill_logged(&mut tables[slot], wal, tx_id, values)?
        };
        // Insert the (v1 or v2) row bytes into the heap FIRST so the Insert
        // record carries the real RowId. Index maintenance uses the logical
        // `values`, so a spilled column is indexed by its full value, never
        // the stub. See the v0.4.x idempotency rationale in the git history.
        let new_rid = self.tables[slot].insert_encoded(values, &row_bytes)?;
        self.wal_log(tx_id, WalRecordType::Insert, table, new_rid, &row_bytes)?;
        let lsn = self.wal.last_appended_lsn();
        if lsn > 0 {
            self.tables[slot].heap.set_page_lsn(new_rid.page_id, lsn)?;
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
        let _ = self.tables[slot].preflight_insert(values)?;
        let tx_id = self.next_tx();
        let autocommit = self.active_tx_id.is_none();
        let Catalog { tables, wal, .. } = self;
        let tbl = &mut tables[slot];
        // Spill-aware encode (logs any overflow chains under `tx_id`, before
        // the Insert record). Returns v1 bytes for rows that fit inline.
        let row_bytes = encode_row_with_spill_logged(tbl, wal, tx_id, values)?;
        // Insert first so the WAL record carries the real RowId (see
        // `insert` for the ordering/durability argument).
        let new_rid = tbl.insert_encoded(values, &row_bytes)?;
        let payload = encode_wal_payload(&tbl.schema.table_name, new_rid, &row_bytes);
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

    pub fn get_projected(
        &self,
        table: &str,
        rid: RowId,
        column_indices: &[usize],
    ) -> io::Result<Option<Vec<Value>>> {
        self.by_name(table)?.get_projected(rid, column_indices)
    }

    pub fn delete(&mut self, table: &str, rid: RowId) -> io::Result<()> {
        let slot = self.slot_of(table)?;
        // Capture the deleted row's overflow chain BEFORE the heap slot is
        // cleared, so it can be freed once safe (design 3.6). Empty for
        // inline-only tables (cheap `has_overflow_rows` check).
        let old_pages = self.tables[slot].overflow_chain_pages_at(rid)?;
        // Mission B (post-review, second pass): WAL Off → no payload
        // construction.
        if self.wal.is_off() {
            self.tables[slot].delete(rid)?;
            self.free_overflow_chain(slot, old_pages);
            return Ok(());
        }
        let tx_id = self.next_tx();
        // Delete records carry only the rid — no row payload.
        self.wal_log(tx_id, WalRecordType::Delete, table, rid, &[])?;
        self.tables[slot].delete(rid)?;
        self.free_overflow_chain(slot, old_pages);
        Ok(())
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
        let slot = self.slot_of(table)?;
        // Gather every deleted row's overflow chain up front (empty and cheap
        // for inline-only tables) so the pages can be freed once safe.
        let old_pages = self.collect_overflow_pages(slot, rids)?;
        if self.wal.is_off() {
            let count = self.tables[slot].delete_many(rids)?;
            self.free_overflow_chain(slot, old_pages);
            return Ok(count);
        }
        let tx_id = self.next_tx();
        for &rid in rids {
            let payload = encode_wal_payload(table, rid, &[]);
            self.wal.append(tx_id, WalRecordType::Delete, &payload)?;
        }
        if self.active_tx_id.is_none() && !rids.is_empty() {
            self.pending_autocommit_tx_ids.push(tx_id);
        }
        let count = self.tables[slot].delete_many(rids)?;
        self.free_overflow_chain(slot, old_pages);
        Ok(count)
    }

    /// Collect all overflow-chain pages referenced by `rids` in one table.
    /// Returns empty for inline-only tables without touching any row.
    fn collect_overflow_pages(&self, slot: usize, rids: &[RowId]) -> io::Result<Vec<u32>> {
        if !self.tables[slot].has_overflow_rows() {
            return Ok(Vec::new());
        }
        let mut pages = Vec::new();
        for &rid in rids {
            pages.extend(self.tables[slot].overflow_chain_pages_at(rid)?);
        }
        Ok(pages)
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
            let slot = self.slot_of(table)?;
            let old_pages = self.tables[slot].overflow_chain_pages_at(rid)?;
            let new_rid = self.tables[slot].update(rid, values)?;
            self.free_overflow_chain(slot, old_pages);
            return Ok(new_rid);
        }
        let slot = self.slot_of(table)?;
        self.tables[slot].preflight_update(rid, values)?;
        let tx_id = self.next_tx();
        // Capture the old row's overflow chain (empty for inline-only tables)
        // BEFORE the update replaces it, so it can be freed once safe (design
        // 3.6). A chain-replacing update always orphans the old chain.
        let old_pages = self.tables[slot].overflow_chain_pages_at(rid)?;
        // Spill-aware encode: logs any overflow chains under `tx_id` (before
        // the Update record) and returns the v1/v2 row bytes. An overflow
        // transition relocates the row via heap delete+insert inside
        // `update_encoded`; the old row's chain (if any) is left for `sweep`.
        let row_bytes = {
            let Catalog { tables, wal, .. } = self;
            encode_row_with_spill_logged(&mut tables[slot], wal, tx_id, values)?
        };
        // Reject oversized rows BEFORE appending the Update record: a logged
        // Update the heap then rejects would poison the next replay. (A v2
        // stub row is always small; only a non-spilled v1 row can trip this.)
        check_encoded_row_size(&row_bytes)?;
        self.wal_log(tx_id, WalRecordType::Update, table, rid, &row_bytes)?;
        let new_rid = self.tables[slot].update_encoded(rid, values, &row_bytes, None)?;
        self.free_overflow_chain(slot, old_pages);
        Ok(new_rid)
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
            let slot = self.slot_of(table)?;
            let old_pages = self.tables[slot].overflow_chain_pages_at(rid)?;
            let new_rid = self.tables[slot].update_hinted(rid, values, changed_col_indices)?;
            self.free_overflow_chain(slot, old_pages);
            return Ok(new_rid);
        }
        let slot = self.slot_of(table)?;
        self.tables[slot].preflight_update(rid, values)?;
        let tx_id = self.next_tx();
        let old_pages = self.tables[slot].overflow_chain_pages_at(rid)?;
        let row_bytes = {
            let Catalog { tables, wal, .. } = self;
            encode_row_with_spill_logged(&mut tables[slot], wal, tx_id, values)?
        };
        // Same pre-WAL size gate as [`Self::update`].
        check_encoded_row_size(&row_bytes)?;
        self.wal_log(tx_id, WalRecordType::Update, table, rid, &row_bytes)?;
        let new_rid =
            self.tables[slot].update_encoded(rid, values, &row_bytes, changed_col_indices)?;
        self.free_overflow_chain(slot, old_pages);
        Ok(new_rid)
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
        self.ensure_no_active_transaction_for_ddl("create index")?;
        self.invalidate_structure();
        let data_dir = self.data_dir.clone();
        self.by_name_mut(table)?
            .create_index_with_unique(column, &data_dir, unique)?;
        // Mission 3: persist the updated catalog so the indexed column
        // list survives a restart. `Table::create_index` already saved
        // the btree file itself.
        self.persist()
    }

    pub fn active_catalog_version(&self) -> u16 {
        self.active_catalog_version
    }

    pub fn next_index_id(&self) -> u64 {
        self.next_index_id
    }

    /// Return both legacy column-index and v6 expression-index identities.
    pub fn index_metadata(&self, table: &str) -> Option<Vec<IndexMetadata>> {
        let table_ref = self.get_table(table)?;
        let mut metadata = table_ref
            .indexed_column_metas()
            .into_iter()
            .map(|index| IndexMetadata {
                unique: index.unique,
                source: IndexKeySource::Column { column: index.name },
            })
            .collect::<Vec<_>>();
        metadata.extend(table_ref.expression_index_metas().into_iter().map(|index| {
            IndexMetadata {
                unique: index.unique,
                source: IndexKeySource::Expression {
                    index_id: index.index_id,
                    canonical_version: index.canonical_version,
                    canonical_text: index.canonical_text,
                    json_path: index.json_path,
                },
            }
        }));
        Some(metadata)
    }

    pub fn expression_index_metadata(&self, table: &str) -> Option<Vec<ExpressionIndexMeta>> {
        Some(self.get_table(table)?.expression_index_metas())
    }

    pub fn expression_index_btree(&self, table: &str, index_id: u64) -> Option<&BTree> {
        self.get_table(table)?.expression_index_btree(index_id)
    }

    /// Per-index statistics for a column index. O(1) read of the loaded tree's
    /// in-memory counters; `None` when the table or column index is absent. Used
    /// by the conjunction index chooser during plan lowering.
    pub fn index_stats(&self, table: &str, column: &str) -> Option<IndexStats> {
        Some(self.get_table(table)?.index(column)?.stats())
    }

    /// Per-index statistics for an expression index by id. O(1).
    pub fn expression_index_stats(&self, table: &str, index_id: u64) -> Option<IndexStats> {
        Some(
            self.get_table(table)?
                .expression_index_btree(index_id)?
                .stats(),
        )
    }

    /// Capped count of column-index entries equal to `key`, or `None` when
    /// `column` has no index. `O(min(count, cap))` allocation-free leaf walk used
    /// by the planner's skew guard to detect a hot literal without materialising
    /// its (possibly huge) RowId list. Routes to the raw-key counter for a unique
    /// index and the composite-prefix counter for a non-unique one.
    pub fn index_key_count_capped(
        &self,
        table: &str,
        column: &str,
        key: &Value,
        cap: usize,
    ) -> Option<usize> {
        let unique = self.is_index_unique(table, column)?;
        let tree = self.get_table(table)?.index(column)?;
        Some(if unique {
            tree.count_key_capped(key, cap)
        } else {
            tree.count_prefix_capped(key, cap)
        })
    }

    /// Capped count of expression-index entries equal to `key` (a raw-key tree
    /// whose duplicate keys repeat physically). `O(min(count, cap))`.
    pub fn expression_index_key_count_capped(
        &self,
        table: &str,
        index_id: u64,
        key: &Value,
        cap: usize,
    ) -> Option<usize> {
        Some(
            self.get_table(table)?
                .expression_index_btree(index_id)?
                .count_key_capped(key, cap),
        )
    }

    pub fn expression_index_btree_mut(&mut self, table: &str, index_id: u64) -> Option<&mut BTree> {
        self.get_table_mut(table)?
            .expression_index_btree_mut(index_id)
    }

    pub fn expression_index_lookup_all(
        &self,
        table: &str,
        index_id: u64,
        key: &Value,
    ) -> io::Result<Vec<RowId>> {
        let tree = self
            .by_name(table)?
            .expression_index_btree(index_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "expression index not found"))?;
        Ok(tree.lookup_all(key))
    }

    pub fn expression_index_range_rids(
        &self,
        table: &str,
        index_id: u64,
        start: Option<&Value>,
        end: Option<&Value>,
    ) -> io::Result<Vec<RowId>> {
        let tree = self
            .by_name(table)?
            .expression_index_btree(index_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "expression index not found"))?;
        Ok(tree.raw_range_rids(start, end))
    }

    pub fn expression_index_ordered_rids(
        &self,
        table: &str,
        index_id: u64,
    ) -> io::Result<Vec<RowId>> {
        let tree = self
            .by_name(table)?
            .expression_index_btree(index_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "expression index not found"))?;
        Ok(tree.ordered_rids_nulls_last())
    }

    pub fn expression_index_ordered_rids_bounded(
        &self,
        table: &str,
        index_id: u64,
        direction: IndexOrderDirection,
        offset: usize,
        limit: usize,
    ) -> io::Result<Vec<RowId>> {
        let tree = self
            .by_name(table)?
            .expression_index_btree(index_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "expression index not found"))?;
        Ok(tree.bounded_ordered_rids_nulls_last(
            direction == IndexOrderDirection::Desc,
            offset,
            limit,
        ))
    }

    pub fn drop_expression_index(&mut self, table: &str, index_id: u64) -> io::Result<()> {
        self.ensure_no_active_transaction_for_ddl("drop index")?;
        self.invalidate_structure();
        validate_table_name(table)?;
        let removed = self
            .by_name_mut(table)?
            .take_expression_index(index_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "expression index not found"))?;
        match self.persist_at_activation_boundary() {
            Ok(()) => {}
            Err(CatalogPersistError::BeforeActivation(error)) => {
                self.by_name_mut(table)?.restore_expression_index(removed);
                return Err(error);
            }
            Err(CatalogPersistError::AfterActivation(error)) => {
                warn!(
                    path = %self.data_dir.display(),
                    error = %error,
                    "expression index drop committed but catalog directory sync failed"
                );
            }
        }
        let index_path = self
            .data_dir
            .join(expression_index_file_name(table, index_id));
        if let Err(error) = fs::remove_file(&index_path) {
            if error.kind() != io::ErrorKind::NotFound {
                warn!(path = %index_path.display(), error = %error, "failed to remove dropped expression index file");
            }
        } else if let Err(error) = sync_directory(&self.data_dir) {
            warn!(path = %self.data_dir.display(), error = %error, "failed to sync expression index deletion");
        }
        Ok(())
    }

    /// Persist expression-index identity and create its backup-compatible
    /// `.eidx` file. The catalog stays at v5 until every validation and file
    /// creation step succeeds; the v6 catalog rename is the activation point.
    pub fn create_expression_index_metadata(
        &mut self,
        table: &str,
        canonical_version: u16,
        canonical_text: impl Into<String>,
        json_path: StoredJsonPathV1,
        unique: bool,
    ) -> io::Result<u64> {
        self.ensure_no_active_transaction_for_ddl("create index")?;
        self.invalidate_structure();
        validate_table_name(table)?;
        validate_column_name(&json_path.column)?;
        if canonical_version == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expression canonical version must be non-zero",
            ));
        }
        let canonical_text = canonical_text.into();
        if canonical_text.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expression canonical text must not be empty",
            ));
        }
        if canonical_version == 1 && canonical_text != json_path.canonical_text() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expression canonical text does not match its stored JSON path",
            ));
        }
        let table_ref = self.by_name(table)?;
        let root_index = table_ref
            .schema
            .column_index(&json_path.column)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "JSON root column not found"))?;
        if table_ref.schema.columns[root_index].type_id != TypeId::Json {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expression index root column must have type json",
            ));
        }
        if table_ref.expression_index_metas().iter().any(|index| {
            index.canonical_version == canonical_version && index.canonical_text == canonical_text
        }) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "expression index already exists",
            ));
        }

        let index_id = self.next_index_id;
        let next_index_id = index_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("expression index id space exhausted"))?;
        let index_path = self
            .data_dir
            .join(expression_index_file_name(table, index_id));
        if index_path.exists() {
            // The allocator proves this ID is not referenced by the active
            // catalog. A file here can therefore only be an orphan from a
            // crash after the index-file fsync but before catalog activation.
            fs::remove_file(&index_path)?;
            sync_directory(&self.data_dir)?;
        }
        let meta = ExpressionIndexMeta {
            index_id,
            unique,
            canonical_version,
            canonical_text,
            json_path,
        };
        self.by_name_mut(table)?
            .install_expression_index(meta, &index_path)?;
        if let Err(error) = sync_directory(&self.data_dir) {
            self.by_name_mut(table)?
                .remove_expression_index_by_id(index_id);
            let _ = fs::remove_file(&index_path);
            return Err(error);
        }

        let previous_version = self.active_catalog_version;
        let previous_next_id = self.next_index_id;
        // Activate exactly the format version this feature needs (v6). Using a
        // `max` (never a bare assign) keeps a database that already declared a
        // link at v7 from being silently downgraded when it later adds an
        // expression index.
        self.active_catalog_version = self
            .active_catalog_version
            .max(EXPRESSION_INDEX_CATALOG_VERSION);
        self.next_index_id = next_index_id;
        match self.persist_at_activation_boundary() {
            Ok(()) => {}
            Err(CatalogPersistError::BeforeActivation(error)) => {
                self.by_name_mut(table)?
                    .remove_expression_index_by_id(index_id);
                self.active_catalog_version = previous_version;
                self.next_index_id = previous_next_id;
                let _ = fs::remove_file(&index_path);
                let _ = sync_directory(&self.data_dir);
                return Err(error);
            }
            Err(CatalogPersistError::AfterActivation(error)) => {
                warn!(
                    path = %self.data_dir.display(),
                    error = %error,
                    "expression index creation committed but catalog directory sync failed"
                );
            }
        }
        Ok(index_id)
    }

    /// Declare a relationship link on `def.owner_type`. This is the first
    /// operation that activates catalog format v7; a database that never calls
    /// it stays at its current (v6-or-older) version.
    ///
    /// Validation (all at declare time): both `owner_type` and `target_type`
    /// must be existing tables, `local_key` must be a column on the owner,
    /// `target_key` a column on the target, and `name` must collide with neither
    /// a column on the owner nor an existing link on the same owner.
    ///
    /// Declaration order does not pin the cardinality, because the engine does
    /// not store the cardinality: every reader calls [`Self::link_kind`], which
    /// derives it from the target key's uniqueness at the moment of the read.
    /// `link` then `unique` and `unique` then `link` therefore reach the same
    /// schema with the same behaviour. The advisory [`LinkDef::kind`] byte is
    /// seeded here from the same derivation purely so the v7 on-disk layout
    /// keeps a value in that position; any `kind` supplied by the caller is
    /// ignored, and nothing reads the byte back.
    ///
    /// Activation is lazy and crash-safe, mirroring
    /// [`Self::create_expression_index_metadata`]: on any persist failure before
    /// the catalog rename, the in-memory registry and the format version both
    /// revert and no partial state remains.
    pub fn create_link(&mut self, def: LinkDef) -> io::Result<()> {
        self.ensure_no_active_transaction_for_ddl("create link")?;
        self.invalidate_structure();
        validate_table_name(&def.owner_type)?;
        validate_table_name(&def.target_type)?;
        validate_column_name(&def.name)?;
        validate_column_name(&def.local_key)?;
        validate_column_name(&def.target_key)?;

        // Owner table + local key must exist; the link name must not shadow a
        // column or an existing link on the owner.
        {
            let owner = self.by_name(&def.owner_type)?;
            if owner.schema.column_index(&def.local_key).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "link local key '{}' is not a column on owner type '{}'",
                        def.local_key, def.owner_type
                    ),
                ));
            }
            if owner.schema.column_index(&def.name).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "link name '{}' collides with a column on owner type '{}'",
                        def.name, def.owner_type
                    ),
                ));
            }
        }
        if self
            .links
            .iter()
            .any(|l| l.owner_type == def.owner_type && l.name == def.name)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "link '{}' already exists on owner type '{}'",
                    def.name, def.owner_type
                ),
            ));
        }

        // Target table + target key must exist.
        {
            let target = self.by_name(&def.target_type)?;
            if target.schema.column_index(&def.target_key).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "link target key '{}' is not a column on target type '{}'",
                        def.target_key, def.target_type
                    ),
                ));
            }
        }

        // Seed the advisory byte so the v7 layout keeps a value in that slot.
        // It is written once and never refreshed; readers derive instead, so
        // this value is a record of the past, not a decision about the future.
        let kind = self.derive_link_kind(&def.target_type, &def.target_key);
        let stored = LinkDef { kind, ..def };

        // Lazy activation + proven rollback pattern (see
        // create_expression_index_metadata). Register, bump the version, persist;
        // on a pre-rename failure, undo both.
        self.links.push(stored);
        let previous_version = self.active_catalog_version;
        self.active_catalog_version = self.active_catalog_version.max(CATALOG_VERSION);
        match self.persist_at_activation_boundary() {
            Ok(()) => {}
            Err(CatalogPersistError::BeforeActivation(error)) => {
                self.links.pop();
                self.active_catalog_version = previous_version;
                let _ = sync_directory(&self.data_dir);
                return Err(error);
            }
            Err(CatalogPersistError::AfterActivation(error)) => {
                warn!(
                    path = %self.data_dir.display(),
                    error = %error,
                    "link creation committed but catalog directory sync failed"
                );
            }
        }
        Ok(())
    }

    /// Cardinality of a link between `target_type.target_key` and its owners,
    /// computed from the catalog as it stands right now. A unique index on the
    /// target key means a hop matches at most one row (`ToOne`); anything else
    /// (a plain index, or no index at all) can fan out (`ToMany`).
    ///
    /// This is the only place the fact is computed, and there is no cached copy
    /// of the answer anywhere: [`LinkDef::kind`] is advisory and must not be
    /// consulted. Every cardinality decision in the engine ends up here, so a
    /// schema change is visible to the next statement with no repair pass, no
    /// re-declaration and no reopen.
    pub fn derive_link_kind(&self, target_type: &str, target_key: &str) -> LinkKind {
        if self.is_index_unique(target_type, target_key) == Some(true) {
            LinkKind::ToOne
        } else {
            LinkKind::ToMany
        }
    }

    /// Cardinality of the link registered under `(owner_type, name)`, derived
    /// from the catalog as it stands now. `None` when no such link exists.
    /// This is the accessor every correctness decision should use; reading
    /// [`LinkDef::kind`] instead is the bug this API exists to prevent.
    pub fn link_kind(&self, owner_type: &str, name: &str) -> Option<LinkKind> {
        let link = self.link(owner_type, name)?;
        Some(self.derive_link_kind(&link.target_type, &link.target_key))
    }

    /// Resolve a link by its `(owner_type, name)` registry key. The returned
    /// reference borrows the catalog immutably.
    pub fn link(&self, owner_type: &str, name: &str) -> Option<&LinkDef> {
        self.links
            .iter()
            .find(|l| l.owner_type == owner_type && l.name == name)
    }

    /// Iterate every declared link in declaration order.
    pub fn links(&self) -> impl Iterator<Item = &LinkDef> + '_ {
        self.links.iter()
    }

    /// Remove a link by its `(owner_type, name)` registry key. Metadata-only:
    /// it deletes no data and touches no secondary structures. Errors with
    /// `NotFound` if no such link exists. Does not downgrade the format version
    /// (consistent with every other drop path — the version floor only rises).
    pub fn drop_link(&mut self, owner_type: &str, name: &str) -> io::Result<()> {
        self.ensure_no_active_transaction_for_ddl("drop link")?;
        self.invalidate_structure();
        let idx = self
            .links
            .iter()
            .position(|l| l.owner_type == owner_type && l.name == name)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("link '{name}' not found on owner type '{owner_type}'"),
                )
            })?;
        let removed = self.links.remove(idx);
        if let Err(error) = self.persist() {
            // Restore the in-memory registry so it matches what is still on disk.
            self.links.insert(idx, removed);
            return Err(error);
        }
        Ok(())
    }

    /// First link that references `table` as either owner or target, if any.
    /// Used to guard `DROP TABLE` (a referenced table cannot be dropped while a
    /// link names it, the same discipline indexes use).
    fn link_referencing_table(&self, table: &str) -> Option<&LinkDef> {
        self.links
            .iter()
            .find(|l| l.owner_type == table || l.target_type == table)
    }

    /// First link that references `table.column` (owner local key or target
    /// key), if any. Used to guard `ALTER TABLE DROP COLUMN`.
    fn link_referencing_column(&self, table: &str, column: &str) -> Option<&LinkDef> {
        self.links.iter().find(|l| {
            (l.owner_type == table && l.local_key == column)
                || (l.target_type == table && l.target_key == column)
        })
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
        self.ensure_no_active_transaction_for_ddl("drop table")?;
        self.invalidate_structure();
        validate_table_name(name)?;
        let slot = *self.name_to_slot.get(name).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("table '{name}' not found"))
        })?;
        // A live relationship link that names this table (as owner or target)
        // pins it in place, the same integrity discipline indexes use. The
        // link has to go first, and PowQL has no statement that removes one,
        // so the message names the surface that does rather than inventing a
        // `drop link` statement the parser would reject.
        if let Some(link) = self.link_referencing_table(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "cannot drop table '{name}': link '{}' on '{}' references it. \
                     Remove the link first with the embedded API \
                     `Catalog::drop_link(\"{}\", \"{}\")`; PowQL has no statement \
                     that removes a link",
                    link.name, link.owner_type, link.owner_type, link.name
                ),
            ));
        }
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
        let expression_index_ids = table.expression_index_ids();
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
        for index_id in expression_index_ids {
            let idx_path = self
                .data_dir
                .join(expression_index_file_name(name, index_id));
            let _ = fs::remove_file(idx_path);
        }
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
        self.ensure_no_active_transaction_for_ddl("alter table add column")?;
        self.invalidate_structure();
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
        self.ensure_no_active_transaction_for_ddl("alter table drop column")?;
        self.invalidate_structure();
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
        // A live link that names this column (as owner local key or target key)
        // pins it in place. Same remedy wording as `drop_table`: name the API
        // that can actually remove a link instead of a PowQL statement that
        // does not exist.
        if let Some(link) = self.link_referencing_column(table, col_name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "cannot drop column '{col_name}' from '{table}': link '{}' on '{}' \
                     references it. Remove the link first with the embedded API \
                     `Catalog::drop_link(\"{}\", \"{}\")`; PowQL has no statement \
                     that removes a link",
                    link.name, link.owner_type, link.owner_type, link.name
                ),
            ));
        }
        let removed_expression_index_ids = self
            .by_name_mut(table)?
            .remove_expression_indexes_for_root(col_name);
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
        for index_id in removed_expression_index_ids {
            let idx_path = self
                .data_dir
                .join(expression_index_file_name(table, index_id));
            let _ = fs::remove_file(idx_path);
        }
        Ok(())
    }
}

impl Drop for Catalog {
    fn drop(&mut self) {
        // A read-only snapshot handle never wrote anything and holds read-only
        // file descriptors; checkpointing would try to flush pages and truncate
        // the WAL, mutating a directory that must stay byte-identical.
        if self.read_only {
            return;
        }
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

/// Write one out-of-line value's overflow chain to the heap (head-first,
/// singly linked) and log each chunk as a `WalRecordType::OverflowWrite`
/// record under `tx_id`, ordered BEFORE the row's Insert/Update record so the
/// stub the row carries always points at logged, replayable pages. Returns the
/// stub (u64 length, head page, whole-value CRC32). Enforces `MAX_VALUE_SIZE`.
fn write_overflow_chain_logged(
    heap: &mut HeapFile,
    wal: &mut Wal,
    table: &str,
    tx_id: u64,
    value: &[u8],
) -> io::Result<OverflowStub> {
    if value.len() > MAX_VALUE_SIZE {
        return Err(StorageError::ValueTooLarge {
            size: value.len(),
            max: MAX_VALUE_SIZE,
        }
        .into());
    }
    let n = value.len().div_ceil(OVERFLOW_PAYLOAD_CAP).max(1);
    let mut pages = Vec::with_capacity(n);
    for _ in 0..n {
        pages.push(heap.allocate_overflow_page()?);
    }
    for i in 0..n {
        let start = i * OVERFLOW_PAYLOAD_CAP;
        let end = (start + OVERFLOW_PAYLOAD_CAP).min(value.len());
        let chunk = &value[start..end];
        let next = if i + 1 < n {
            pages[i + 1]
        } else {
            OVERFLOW_CHAIN_END
        };
        let payload = encode_overflow_write_payload(table, pages[i], next, chunk);
        wal.append(tx_id, WalRecordType::OverflowWrite, &payload)?;
        let lsn = wal.last_appended_lsn();
        heap.write_overflow_page(pages[i], next, chunk, lsn)?;
    }
    Ok(OverflowStub::new(
        value.len() as u64,
        pages[0],
        crc32fast::hash(value),
    ))
}

/// Spill-aware encode for the WAL path. If the row fits inline, returns its v1
/// bytes untouched. Otherwise writes each spilled value's chain (with WAL
/// logging under `tx_id`) and returns the v2 stub-row bytes to be inserted and
/// logged in the row's Insert/Update record.
fn encode_row_with_spill_logged(
    tbl: &mut Table,
    wal: &mut Wal,
    tx_id: u64,
    values: &Row,
) -> io::Result<Vec<u8>> {
    // Size the v1 encoding WITHOUT encoding it (a >64KB value would panic the
    // debug-mode v1 encoder). Only actually encode v1 when the row fits inline.
    let v1_len = crate::row::v1_encoded_len(tbl.row_layout(), values);
    let is_indexed = tbl.indexed_col_mask();
    let chosen = plan_spill(tbl.row_layout(), values, v1_len, &is_indexed);
    if chosen.is_empty() {
        let mut v1 = Vec::new();
        encode_row_into(&tbl.schema, values, &mut v1);
        return Ok(v1);
    }
    let table_name = tbl.schema.table_name.clone();
    let n_var = tbl.row_layout().n_var();
    let mut spilled: Vec<Option<OverflowStub>> = vec![None; n_var];
    for col_idx in chosen {
        let var_idx = tbl
            .row_layout()
            .var_index(col_idx)
            .expect("plan_spill only returns var columns");
        let bytes: Vec<u8> = match &values[col_idx] {
            Value::Str(s) => s.as_bytes().to_vec(),
            Value::Bytes(b) => b.to_vec(),
            Value::Json(b) => b.to_vec(),
            _ => continue,
        };
        let stub = write_overflow_chain_logged(&mut tbl.heap, wal, &table_name, tx_id, &bytes)?;
        spilled[var_idx] = Some(stub);
    }
    let mut out = Vec::new();
    encode_row_v2_into(&tbl.schema, tbl.row_layout(), values, &spilled, &mut out);
    Ok(out)
}

/// `OverflowWrite` payload: `table_len u16 | table | page_id u32 |
/// next_page u32 | chunk_len u16 | chunk bytes`.
///
/// NOTE (deviation from design 3.5): the design lists the payload as
/// `page_id | next_page | chunk_len | chunk`, but overflow pages live in
/// per-table heap files with independent page-id spaces, so replay needs the
/// table identity to route the write. The table name is length-prefixed
/// exactly like [`encode_wal_payload`]. The chunk-level fields are unchanged.
fn encode_overflow_write_payload(
    table: &str,
    page_id: u32,
    next_page: u32,
    chunk: &[u8],
) -> Vec<u8> {
    let name = table.as_bytes();
    let mut out = Vec::with_capacity(2 + name.len() + 4 + 4 + 2 + chunk.len());
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&page_id.to_le_bytes());
    out.extend_from_slice(&next_page.to_le_bytes());
    out.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
    out.extend_from_slice(chunk);
    out
}

fn decode_overflow_write_payload(data: &[u8]) -> Option<(String, u32, u32, Vec<u8>)> {
    let mut pos = 0usize;
    if data.len() < 2 {
        return None;
    }
    let name_len = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
    pos += 2;
    if pos + name_len + 4 + 4 + 2 > data.len() {
        return None;
    }
    let name = std::str::from_utf8(&data[pos..pos + name_len])
        .ok()?
        .to_string();
    pos += name_len;
    let page_id = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?);
    pos += 4;
    let next_page = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?);
    pos += 4;
    let chunk_len = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
    pos += 2;
    if pos + chunk_len > data.len() {
        return None;
    }
    Some((
        name,
        page_id,
        next_page,
        data[pos..pos + chunk_len].to_vec(),
    ))
}

/// `OverflowFree` payload: `table_len u16 | table | count u32 |
/// page_id u32 x count`. Table name added for the same routing reason as
/// [`encode_overflow_write_payload`].
fn encode_overflow_free_payload(table: &str, pages: &[u32]) -> Vec<u8> {
    let name = table.as_bytes();
    let mut out = Vec::with_capacity(2 + name.len() + 4 + pages.len() * 4);
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&(pages.len() as u32).to_le_bytes());
    for p in pages {
        out.extend_from_slice(&p.to_le_bytes());
    }
    out
}

fn decode_overflow_free_payload(data: &[u8]) -> Option<(String, Vec<u32>)> {
    let mut pos = 0usize;
    if data.len() < 2 {
        return None;
    }
    let name_len = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
    pos += 2;
    if pos + name_len + 4 > data.len() {
        return None;
    }
    let name = std::str::from_utf8(&data[pos..pos + name_len])
        .ok()?
        .to_string();
    pos += name_len;
    let count = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
    pos += 4;
    if pos + count * 4 > data.len() {
        return None;
    }
    let mut pages = Vec::with_capacity(count);
    for _ in 0..count {
        pages.push(u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?));
        pos += 4;
    }
    Some((name, pages))
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
    pub expression_indexes: Vec<ExpressionIndexMeta>,
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
    pub expression_indexes: Vec<ExpressionIndexMeta>,
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
        Value::Json(b) => {
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
        TypeId::Json => {
            let len = u32::from_le_bytes(take_fixed(pos, 4)?.try_into().ok()?) as usize;
            Some(Value::Json(take_fixed(pos, len)?.into()))
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

fn push_catalog_string(out: &mut Vec<u8>, value: &str) -> io::Result<()> {
    let len = u32::try_from(value.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "catalog string is too large"))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn encode_expression_indexes(out: &mut Vec<u8>, indexes: &[ExpressionIndexMeta]) -> io::Result<()> {
    let count = u16::try_from(indexes.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many expression indexes on one table",
        )
    })?;
    out.extend_from_slice(&count.to_le_bytes());
    for index in indexes {
        out.extend_from_slice(&index.index_id.to_le_bytes());
        out.push(u8::from(index.unique));
        out.extend_from_slice(&index.canonical_version.to_le_bytes());
        push_catalog_string(out, &index.canonical_text)?;
        push_catalog_string(out, &index.json_path.column)?;
        let segment_count = u16::try_from(index.json_path.segments.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "JSON path has too many segments",
            )
        })?;
        out.extend_from_slice(&segment_count.to_le_bytes());
        for segment in &index.json_path.segments {
            match segment {
                StoredJsonPathSegmentV1::Key(key) => {
                    out.push(1);
                    push_catalog_string(out, key)?;
                }
                StoredJsonPathSegmentV1::Index(position) => {
                    out.push(2);
                    out.extend_from_slice(&position.to_le_bytes());
                }
            }
        }
    }
    Ok(())
}

fn decode_expression_indexes(data: &[u8], pos: &mut usize) -> io::Result<Vec<ExpressionIndexMeta>> {
    let count = read_u16(data, pos)? as usize;
    let mut indexes = Vec::with_capacity(count);
    for _ in 0..count {
        let index_id = read_u64(data, pos)?;
        if index_id == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expression index id must be non-zero",
            ));
        }
        let unique = read_u8(data, pos)? != 0;
        let canonical_version = read_u16(data, pos)?;
        let canonical_len = read_u32(data, pos)? as usize;
        let canonical_text = read_string(data, pos, canonical_len)?;
        let column_len = read_u32(data, pos)? as usize;
        let column = read_string(data, pos, column_len)?;
        let segment_count = read_u16(data, pos)? as usize;
        let mut segments = Vec::with_capacity(segment_count);
        for _ in 0..segment_count {
            match read_u8(data, pos)? {
                1 => {
                    let len = read_u32(data, pos)? as usize;
                    segments.push(StoredJsonPathSegmentV1::Key(read_string(data, pos, len)?));
                }
                2 => segments.push(StoredJsonPathSegmentV1::Index(read_u32(data, pos)?)),
                tag => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unknown stored JSON path segment tag: {tag}"),
                    ));
                }
            }
        }
        indexes.push(ExpressionIndexMeta {
            index_id,
            unique,
            canonical_version,
            canonical_text,
            json_path: StoredJsonPathV1 { column, segments },
        });
    }
    Ok(indexes)
}

/// Encode the v7 relationship-link section: a `u32` count followed by that many
/// records of five length-prefixed strings plus one `u8` kind, matching the
/// len-prefix conventions the table/column/expression-index codecs use.
fn encode_links_section(out: &mut Vec<u8>, links: &[LinkDef]) -> io::Result<()> {
    let count = u32::try_from(links.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many catalog links"))?;
    out.extend_from_slice(&count.to_le_bytes());
    for link in links {
        push_catalog_string(out, &link.owner_type)?;
        push_catalog_string(out, &link.name)?;
        push_catalog_string(out, &link.target_type)?;
        push_catalog_string(out, &link.local_key)?;
        push_catalog_string(out, &link.target_key)?;
        out.push(link.kind.to_u8());
    }
    Ok(())
}

/// Inverse of [`encode_links_section`]. Bounds-checks the count against the
/// remaining buffer so a corrupt file fails closed rather than pre-allocating a
/// huge `Vec` (mirrors the table-count and btree node-count guards).
fn decode_links_section(data: &[u8], pos: &mut usize) -> io::Result<Vec<LinkDef>> {
    let count = read_u32(data, pos)? as usize;
    if count > data.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("catalog file corrupt: implausible link count {count}"),
        ));
    }
    let mut links = Vec::with_capacity(count);
    for _ in 0..count {
        let owner_type = read_len_prefixed_string(data, pos)?;
        let name = read_len_prefixed_string(data, pos)?;
        let target_type = read_len_prefixed_string(data, pos)?;
        let local_key = read_len_prefixed_string(data, pos)?;
        let target_key = read_len_prefixed_string(data, pos)?;
        let kind = LinkKind::from_u8(read_u8(data, pos)?)?;
        links.push(LinkDef {
            owner_type,
            name,
            target_type,
            local_key,
            target_key,
            kind,
        });
    }
    Ok(links)
}

fn read_len_prefixed_string(data: &[u8], pos: &mut usize) -> io::Result<String> {
    let len = read_u32(data, pos)? as usize;
    read_string(data, pos, len)
}

fn write_catalog_file(
    path: &Path,
    version: u16,
    next_index_id: u64,
    entries: &[CatalogEntryRef<'_>],
    links: &[LinkDef],
) -> io::Result<()> {
    if !(1..=CATALOG_VERSION).contains(&version) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported catalog write version: {version}"),
        ));
    }
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    buf.extend_from_slice(CATALOG_MAGIC);
    buf.extend_from_slice(&version.to_le_bytes());
    buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    if version >= 6 {
        buf.extend_from_slice(&next_index_id.to_le_bytes());
    }

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
        if version >= 6 {
            encode_expression_indexes(&mut buf, &entry.expression_indexes)?;
        }
    }

    // Version 7 appends the relationship-link section after every table entry
    // and before the CRC. A v6-or-older file omits it entirely (the reader
    // defaults n_links = 0), so a link-free database stays byte-for-byte
    // unchanged.
    if version >= 7 {
        encode_links_section(&mut buf, links)?;
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

struct CatalogFile {
    version: u16,
    next_index_id: u64,
    entries: Vec<CatalogEntry>,
    links: Vec<LinkDef>,
}

fn read_catalog_file(path: &Path) -> io::Result<CatalogFile> {
    read_catalog_file_with_max_version(path, CATALOG_VERSION)
}

/// Read the catalog format version currently persisted on disk for `data_dir`
/// without rehydrating tables. This is the database's *active* catalog version:
/// a database that has never activated an expression index stays at
/// [`LEGACY_CATALOG_VERSION`]. Sync producers use it to stamp published segments
/// with the active version rather than this binary's compile-time maximum.
pub fn read_active_catalog_version(data_dir: &Path) -> io::Result<u16> {
    let cat_path = data_dir.join(CATALOG_FILE);
    Ok(read_catalog_file(&cat_path)?.version)
}

fn read_catalog_file_with_max_version(
    path: &Path,
    max_supported_version: u16,
) -> io::Result<CatalogFile> {
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
    if version == 0 || version > max_supported_version {
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
    // Additive legacy read branches. Version history: v1 (no index list),
    // v2 (index names), v3 (uniqueness flag), all written by pre-v0.5.0
    // builds; v4 (column defaults) and v5 (auto-increment columns), both
    // introduced in v0.7.0; v6 (expression indexes + next-index-id header),
    // activated lazily since v0.13.0. v5 is still written today by databases
    // that never activate an expression index, so only v1-v4 are legacy.
    // Per the support policy in docs/FORMAT.md (4 minor versions after
    // superseded), the v1-v4 branches became removable in v0.11.0 at the
    // earliest; they are retained deliberately.
    let next_index_id = if version >= 6 {
        let id = read_u64(buf, &mut pos)?;
        if id == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "catalog next index id must be non-zero",
            ));
        }
        id
    } else {
        // Legacy v1-v5 (pre-v0.13.0, or v6 never activated): no
        // next-index-id header field.
        1
    };

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
        // to non-unique). Version 1 has no index info at all. v1/v2 files
        // (pre-v0.5.0 writers) are legacy; removable per the docs/FORMAT.md
        // policy (floor long passed), kept deliberately.
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

        // Version 4 appends a column-defaults section after the index list
        // (v0.7.0). Legacy v1-v3 files have none; that branch became
        // removable in v0.11.0 per docs/FORMAT.md, kept deliberately.
        let defaults = if version >= 4 {
            decode_defaults_section(buf, &mut pos, columns.len()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "truncated catalog defaults")
            })?
        } else {
            Vec::new()
        };

        // Version 5 appends an auto-increment column section after that
        // (v0.7.0). Legacy v1-v4 files have none; that branch became
        // removable in v0.11.0 per docs/FORMAT.md, kept deliberately.
        let auto_cols = if version >= 5 {
            decode_auto_section(buf, &mut pos, columns.len()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "truncated catalog auto columns")
            })?
        } else {
            Vec::new()
        };

        // Version 6 appends an expression-index section (lazily activated
        // since v0.13.0). v5 is still an active writer version, so the
        // below-6 branch is NOT legacy and is not removal-eligible.
        let expression_indexes = if version >= 6 {
            decode_expression_indexes(buf, &mut pos)?
        } else {
            Vec::new()
        };

        entries.push(CatalogEntry {
            schema: Schema {
                table_name,
                columns,
            },
            indexed_cols,
            expression_indexes,
            defaults,
            auto_cols,
        });
    }

    // Version 7 appends a relationship-link section after the table entries.
    // Any pre-v7 file stops here; the reader defaults n_links = 0 (staircase
    // contract). v6 remains an active writer version, so the below-7 branch is
    // NOT legacy and is not removal-eligible.
    let links = if version >= 7 {
        decode_links_section(buf, &mut pos)?
    } else {
        Vec::new()
    };

    let mut seen_index_ids = FxHashMap::default();
    let mut max_index_id = 0;
    for entry in &entries {
        for index in &entry.expression_indexes {
            if index.canonical_version == 0 || index.canonical_text.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expression index has invalid canonical identity",
                ));
            }
            if index.canonical_version == 1
                && index.canonical_text != index.json_path.canonical_text()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expression index canonical identity does not match its JSON path",
                ));
            }
            let Some(root) = entry
                .schema
                .columns
                .iter()
                .find(|column| column.name == index.json_path.column)
            else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expression index JSON root is absent from its table",
                ));
            };
            if root.type_id != TypeId::Json {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expression index root column is not JSON",
                ));
            }
            if seen_index_ids.insert(index.index_id, ()).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "duplicate expression index id in catalog",
                ));
            }
            max_index_id = max_index_id.max(index.index_id);
        }
    }
    if next_index_id <= max_index_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "catalog next index id does not exceed persisted index ids",
        ));
    }
    Ok(CatalogFile {
        version,
        next_index_id,
        entries,
        links,
    })
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
fn read_u64(buf: &[u8], pos: &mut usize) -> io::Result<u64> {
    if *pos + 8 > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated catalog",
        ));
    }
    let value = u64::from_le_bytes(
        buf[*pos..*pos + 8]
            .try_into()
            .expect("bounds checked above"),
    );
    *pos += 8;
    Ok(value)
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
        8 => Ok(TypeId::Json),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown type id: {v}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fail_next_catalog_persist_at(stage: u8) {
        CATALOG_PERSIST_FAILPOINT.with(|failpoint| failpoint.set(stage));
    }

    fn temp_catalog(name: &str) -> Catalog {
        let dir = std::env::temp_dir().join(format!("powdb_cat_{name}_{}", std::process::id()));
        Catalog::create(&dir).unwrap()
    }

    /// Recursively hash every file's path + bytes under `dir` so a test can
    /// assert a read-only open leaves the directory byte-identical. Lock
    /// artifacts are not created at the catalog layer (only the engine takes a
    /// lock), so nothing needs excluding here.
    fn hash_dir_tree(dir: &std::path::Path) -> String {
        let mut entries: Vec<std::path::PathBuf> = Vec::new();
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let mut items: Vec<_> = fs::read_dir(dir).unwrap().flatten().collect();
            items.sort_by_key(std::fs::DirEntry::path);
            for item in items {
                let path = item.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(path);
                }
            }
        }
        walk(dir, &mut entries);
        let mut hasher = crc32fast::Hasher::new();
        for path in &entries {
            hasher.update(path.to_string_lossy().as_bytes());
            hasher.update(&fs::read(path).unwrap());
        }
        format!("{:08x}", hasher.finalize())
    }

    fn seed_quiescent_dir(dir: &std::path::Path) {
        let mut catalog = Catalog::create(dir).unwrap();
        catalog
            .create_table(Schema {
                table_name: "User".into(),
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
        catalog.create_index("User", "age").unwrap();
        catalog
            .insert("User", &vec![Value::Str("Ada".into()), Value::Int(36)])
            .unwrap();
        catalog
            .insert("User", &vec![Value::Str("Bo".into()), Value::Int(20)])
            .unwrap();
        // Clean drop checkpoints: flush heaps + truncate the WAL, leaving a
        // quiescent (WAL-clean) directory.
        drop(catalog);
    }

    #[test]
    fn open_read_only_serves_reads_on_clean_dir() {
        let dir = tempfile::tempdir().unwrap();
        seed_quiescent_dir(dir.path());

        let catalog = Catalog::open_read_only(dir.path()).unwrap();
        let rows: Vec<_> = catalog.scan("User").unwrap().collect();
        assert_eq!(rows.len(), 2);
        // Column-index read works read-only.
        let hit = catalog
            .index_lookup("User", "age", &Value::Int(36))
            .unwrap();
        assert_eq!(hit.unwrap()[0], Value::Str("Ada".into()));
    }

    #[test]
    fn open_read_only_never_mutates_dir() {
        let dir = tempfile::tempdir().unwrap();
        seed_quiescent_dir(dir.path());
        let before = hash_dir_tree(dir.path());

        {
            let catalog = Catalog::open_read_only(dir.path()).unwrap();
            let _ = catalog.scan("User").unwrap().count();
            let _ = catalog
                .index_lookup("User", "age", &Value::Int(20))
                .unwrap();
            // Drop the read-only catalog: must not checkpoint/truncate.
        }
        let after = hash_dir_tree(dir.path());
        assert_eq!(
            before, after,
            "read-only open + queries + drop must leave the directory byte-identical"
        );
    }

    #[test]
    fn open_read_only_refuses_non_empty_wal() {
        let dir = tempfile::tempdir().unwrap();
        // Seed rows but DO NOT checkpoint: keep the WAL non-empty by forgetting
        // the catalog (a crash), so recovery would be required.
        {
            let mut catalog = Catalog::create(dir.path()).unwrap();
            catalog
                .create_table(Schema {
                    table_name: "T".into(),
                    columns: vec![ColumnDef {
                        name: "id".into(),
                        type_id: TypeId::Int,
                        required: true,
                        position: 0,
                    }],
                })
                .unwrap();
            catalog.insert("T", &vec![Value::Int(1)]).unwrap();
            catalog.sync_wal().unwrap();
            std::mem::forget(catalog); // leave the WAL non-empty, as a crash would
        }
        let err = match Catalog::open_read_only(dir.path()) {
            Ok(_) => panic!("read-only open must refuse a non-empty WAL"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("WAL is not empty"),
            "expected a WAL-not-empty refusal naming the remedy, got: {err}"
        );
        assert!(err.to_string().contains("read-write engine"));
    }

    #[test]
    fn open_read_only_expression_index_reads_work() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut catalog = Catalog::create(dir.path()).unwrap();
            catalog
                .create_table(Schema {
                    table_name: "Doc".into(),
                    columns: vec![ColumnDef {
                        name: "data".into(),
                        type_id: TypeId::Json,
                        required: false,
                        position: 0,
                    }],
                })
                .unwrap();
            let path =
                StoredJsonPathV1::new("data", vec![StoredJsonPathSegmentV1::Key("author".into())]);
            catalog
                .create_expression_index_metadata("Doc", 1, path.canonical_text(), path, false)
                .unwrap();
            drop(catalog);
        }
        // Opening read-only must load the expression index without writing.
        let before = hash_dir_tree(dir.path());
        let catalog = Catalog::open_read_only(dir.path()).unwrap();
        assert_eq!(catalog.scan("Doc").unwrap().count(), 0);
        drop(catalog);
        let after = hash_dir_tree(dir.path());
        assert_eq!(
            before, after,
            "read-only expression-index load must not write"
        );
    }

    #[test]
    fn v5_reader_rejects_v6_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::create(dir.path()).unwrap();
        catalog
            .create_table(Schema {
                table_name: "Doc".into(),
                columns: vec![ColumnDef {
                    name: "data".into(),
                    type_id: TypeId::Json,
                    required: false,
                    position: 0,
                }],
            })
            .unwrap();
        let path =
            StoredJsonPathV1::new("data", vec![StoredJsonPathSegmentV1::Key("author".into())]);
        catalog
            .create_expression_index_metadata("Doc", 1, path.canonical_text(), path, false)
            .unwrap();
        let result = read_catalog_file_with_max_version(
            &dir.path().join(CATALOG_FILE),
            LEGACY_CATALOG_VERSION,
        );
        let error = match result {
            Ok(_) => panic!("a v5 reader must reject v6 before decoding its payload"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("unsupported catalog version: 6"));
    }

    /// A v7 catalog (one that declared a link) must be refused by a reader
    /// capped at v6, with the same "unsupported catalog version" error the
    /// version gate already produces — not a crash, not silent corruption.
    #[test]
    fn v6_reader_rejects_v7_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::create(dir.path()).unwrap();
        catalog
            .create_table(Schema {
                table_name: "Order".into(),
                columns: vec![ColumnDef {
                    name: "user_id".into(),
                    type_id: TypeId::Int,
                    required: false,
                    position: 0,
                }],
            })
            .unwrap();
        catalog
            .create_table(Schema {
                table_name: "User".into(),
                columns: vec![ColumnDef {
                    name: "id".into(),
                    type_id: TypeId::Int,
                    required: true,
                    position: 0,
                }],
            })
            .unwrap();
        catalog
            .create_link(LinkDef {
                owner_type: "Order".into(),
                name: "user".into(),
                target_type: "User".into(),
                local_key: "user_id".into(),
                target_key: "id".into(),
                kind: LinkKind::ToMany,
            })
            .unwrap();
        assert_eq!(catalog.active_catalog_version(), CATALOG_VERSION);

        let result = read_catalog_file_with_max_version(
            &dir.path().join(CATALOG_FILE),
            EXPRESSION_INDEX_CATALOG_VERSION,
        );
        let error = match result {
            Ok(_) => panic!("a v6 reader must reject v7 before decoding its payload"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("unsupported catalog version: 7"));
    }

    #[test]
    fn expression_index_rolls_back_only_before_catalog_rename() {
        let before_dir = tempfile::tempdir().unwrap();
        let mut before = Catalog::create(before_dir.path()).unwrap();
        before
            .create_table(Schema {
                table_name: "Doc".into(),
                columns: vec![ColumnDef {
                    name: "data".into(),
                    type_id: TypeId::Json,
                    required: false,
                    position: 0,
                }],
            })
            .unwrap();
        let path =
            StoredJsonPathV1::new("data", vec![StoredJsonPathSegmentV1::Key("score".into())]);

        fail_next_catalog_persist_at(1);
        let error = before
            .create_expression_index_metadata("Doc", 1, path.canonical_text(), path.clone(), false)
            .unwrap_err();
        assert!(error.to_string().contains("before rename"));
        assert_eq!(before.active_catalog_version(), LEGACY_CATALOG_VERSION);
        assert_eq!(before.next_index_id(), 1);
        assert!(before.expression_index_metadata("Doc").unwrap().is_empty());
        assert!(!before_dir
            .path()
            .join(expression_index_file_name("Doc", 1))
            .exists());

        let before_index_id = before
            .create_expression_index_metadata("Doc", 1, path.canonical_text(), path.clone(), false)
            .unwrap();
        fail_next_catalog_persist_at(1);
        let error = before
            .drop_expression_index("Doc", before_index_id)
            .unwrap_err();
        assert!(error.to_string().contains("before rename"));
        assert!(before
            .expression_index_btree("Doc", before_index_id)
            .is_some());
        assert!(before_dir
            .path()
            .join(expression_index_file_name("Doc", 1))
            .exists());
        std::mem::forget(before);
        let before_reopened = Catalog::open(before_dir.path()).unwrap();
        assert!(before_reopened
            .expression_index_btree("Doc", before_index_id)
            .is_some());

        let after_dir = tempfile::tempdir().unwrap();
        let mut after = Catalog::create(after_dir.path()).unwrap();
        after
            .create_table(Schema {
                table_name: "Doc".into(),
                columns: vec![ColumnDef {
                    name: "data".into(),
                    type_id: TypeId::Json,
                    required: false,
                    position: 0,
                }],
            })
            .unwrap();
        fail_next_catalog_persist_at(2);
        let index_id = after
            .create_expression_index_metadata("Doc", 1, path.canonical_text(), path.clone(), false)
            .unwrap();
        assert_eq!(index_id, 1);
        assert_eq!(
            after.active_catalog_version(),
            EXPRESSION_INDEX_CATALOG_VERSION
        );
        assert_eq!(after.next_index_id(), 2);
        assert!(after.expression_index_btree("Doc", index_id).is_some());
        assert!(after_dir
            .path()
            .join(expression_index_file_name("Doc", 1))
            .exists());
        std::mem::forget(after);

        let mut reopened = Catalog::open(after_dir.path()).unwrap();
        assert!(reopened.expression_index_btree("Doc", index_id).is_some());
        fail_next_catalog_persist_at(2);
        reopened.drop_expression_index("Doc", index_id).unwrap();
        assert!(reopened
            .expression_index_metadata("Doc")
            .unwrap()
            .is_empty());
        assert!(!after_dir
            .path()
            .join(expression_index_file_name("Doc", 1))
            .exists());
        std::mem::forget(reopened);

        let final_open = Catalog::open(after_dir.path()).unwrap();
        assert!(final_open
            .expression_index_metadata("Doc")
            .unwrap()
            .is_empty());
    }

    /// A pre-rename persist failure during the *first* `create_link` must revert
    /// the format version 7 -> 6/5, leave the in-memory registry empty, and
    /// leave no v7 catalog on disk (mirrors the expression-index rollback test).
    #[test]
    fn first_create_link_rolls_back_version_and_registry_before_rename() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::create(dir.path()).unwrap();
        catalog
            .create_table(Schema {
                table_name: "Order".into(),
                columns: vec![ColumnDef {
                    name: "user_id".into(),
                    type_id: TypeId::Int,
                    required: false,
                    position: 0,
                }],
            })
            .unwrap();
        catalog
            .create_table(Schema {
                table_name: "User".into(),
                columns: vec![ColumnDef {
                    name: "id".into(),
                    type_id: TypeId::Int,
                    required: true,
                    position: 0,
                }],
            })
            .unwrap();
        catalog.create_index_unique("User", "id", true).unwrap();

        let version_before = catalog.active_catalog_version();
        assert_eq!(version_before, LEGACY_CATALOG_VERSION);

        fail_next_catalog_persist_at(1);
        let error = catalog
            .create_link(LinkDef {
                owner_type: "Order".into(),
                name: "user".into(),
                target_type: "User".into(),
                local_key: "user_id".into(),
                target_key: "id".into(),
                kind: LinkKind::ToMany,
            })
            .unwrap_err();
        assert!(error.to_string().contains("before rename"));
        // Version and registry reverted; nothing persisted.
        assert_eq!(catalog.active_catalog_version(), version_before);
        assert!(catalog.link("Order", "user").is_none());
        assert_eq!(catalog.links().count(), 0);
        assert_eq!(
            read_active_catalog_version(dir.path()).unwrap(),
            version_before
        );

        // A subsequent clean create_link succeeds and derives ToOne (unique id).
        catalog
            .create_link(LinkDef {
                owner_type: "Order".into(),
                name: "user".into(),
                target_type: "User".into(),
                local_key: "user_id".into(),
                target_key: "id".into(),
                kind: LinkKind::ToMany, // ignored; derived from uniqueness
            })
            .unwrap();
        assert_eq!(catalog.active_catalog_version(), CATALOG_VERSION);
        assert_eq!(catalog.link("Order", "user").unwrap().kind, LinkKind::ToOne);
    }

    #[test]
    fn ordinary_catalog_persist_reports_post_rename_directory_sync_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::create(dir.path()).unwrap();
        fail_next_catalog_persist_at(2);
        let error = catalog
            .create_table(Schema {
                table_name: "VisibleAfterRename".into(),
                columns: vec![ColumnDef {
                    name: "id".into(),
                    type_id: TypeId::Int,
                    required: true,
                    position: 0,
                }],
            })
            .unwrap_err();
        assert!(error.to_string().contains("after rename"));
        assert!(catalog.schema("VisibleAfterRename").is_some());

        std::mem::forget(catalog);
        let reopened = Catalog::open(dir.path()).unwrap();
        assert!(reopened.schema("VisibleAfterRename").is_some());
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
            let catalog_file = read_catalog_file(&path)
                .unwrap_or_else(|e| panic!("version {version} catalog must load, got: {e}"));
            let entries = catalog_file.entries;
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
        buf.extend_from_slice(&1u64.to_le_bytes()); // valid v6 next-index id
                                                    // …but no table data follows.
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

    /// Two-column table used by the DDL-in-transaction refusal tests.
    fn ddl_guard_schema(name: &str) -> Schema {
        Schema {
            table_name: name.into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    type_id: TypeId::Int,
                    required: true,
                    position: 0,
                },
                ColumnDef {
                    name: "label".into(),
                    type_id: TypeId::Str,
                    required: false,
                    position: 1,
                },
            ],
        }
    }

    fn seed_ddl_guard_catalog(dir: &std::path::Path) -> Catalog {
        let mut cat = Catalog::create(dir).unwrap();
        cat.create_table(ddl_guard_schema("Keep")).unwrap();
        cat.create_index_unique("Keep", "id", true).unwrap();
        cat.insert("Keep", &vec![Value::Int(1), Value::Str("one".into())])
            .unwrap();
        cat.sync_wal().unwrap();
        cat
    }

    fn assert_refused_in_transaction(result: io::Result<()>, verb: &str) {
        let err = result.unwrap_err();
        assert_eq!(
            err.kind(),
            io::ErrorKind::InvalidInput,
            "{verb} inside a transaction must be refused as InvalidInput"
        );
        let message = err.to_string();
        assert!(
            message.starts_with("cannot ") && message.contains("explicit transaction"),
            "{verb} refusal must name the active transaction, got: {message}"
        );
    }

    #[test]
    fn drop_table_inside_transaction_is_refused_and_rollback_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let mut cat = seed_ddl_guard_catalog(dir.path());
        // Checkpoint first so the WAL no longer holds the records that created
        // and populated `Keep`: rollback then has nothing to replay and the
        // table can only survive if the DROP was refused outright.
        cat.checkpoint().unwrap();

        cat.begin_transaction().unwrap();
        assert_refused_in_transaction(cat.drop_table("Keep"), "drop table");
        cat.rollback_to_last_sync().unwrap();

        let rows: Vec<_> = cat.scan("Keep").unwrap().collect();
        assert_eq!(rows.len(), 1, "rolled-back DROP must not destroy data");
        assert_eq!(rows[0].1[0], Value::Int(1));

        // The heap file itself must still be there for the next open.
        drop(cat);
        let reopened = Catalog::open(dir.path()).unwrap();
        assert_eq!(reopened.scan("Keep").unwrap().count(), 1);
    }

    #[test]
    fn every_ddl_verb_inside_transaction_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut cat = seed_ddl_guard_catalog(dir.path());
        cat.create_table(ddl_guard_schema("Other")).unwrap();
        cat.create_index_unique("Other", "id", true).unwrap();
        cat.checkpoint().unwrap();

        cat.begin_transaction().unwrap();

        assert_refused_in_transaction(cat.create_table(ddl_guard_schema("New")), "create table");
        assert_refused_in_transaction(cat.drop_table("Keep"), "drop table");
        assert_refused_in_transaction(
            cat.alter_table_add_column(
                "Keep",
                ColumnDef {
                    name: "extra".into(),
                    type_id: TypeId::Int,
                    required: false,
                    position: 2,
                },
            ),
            "alter table add column",
        );
        assert_refused_in_transaction(
            cat.alter_table_drop_column("Keep", "label"),
            "alter table drop column",
        );
        assert_refused_in_transaction(cat.create_index("Keep", "label"), "create index");
        assert_refused_in_transaction(
            cat.create_index_unique("Keep", "label", true),
            "create unique index",
        );
        assert_refused_in_transaction(
            cat.create_link(LinkDef {
                owner_type: "Other".into(),
                name: "keep".into(),
                target_type: "Keep".into(),
                local_key: "id".into(),
                target_key: "id".into(),
                kind: LinkKind::ToOne,
            }),
            "create link",
        );
        assert_refused_in_transaction(cat.drop_link("Other", "keep"), "drop link");

        cat.rollback_to_last_sync().unwrap();

        // Every refused verb left the catalog exactly as it was.
        let mut tables = cat.list_tables();
        tables.sort_unstable();
        assert_eq!(tables, vec!["Keep", "Other"]);
        let schema = cat.schema("Keep").unwrap();
        assert_eq!(schema.columns.len(), 2);
        assert!(!cat.has_index("Keep", "label"));
        assert!(cat.links().next().is_none());
        assert_eq!(cat.scan("Keep").unwrap().count(), 1);
    }

    #[test]
    fn ddl_still_works_after_commit_and_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let mut cat = seed_ddl_guard_catalog(dir.path());

        cat.begin_transaction().unwrap();
        assert!(cat.drop_table("Keep").is_err());
        cat.rollback_to_last_sync().unwrap();
        cat.create_table(ddl_guard_schema("AfterRollback")).unwrap();

        cat.begin_transaction().unwrap();
        cat.insert("Keep", &vec![Value::Int(2), Value::Str("two".into())])
            .unwrap();
        cat.commit_transaction().unwrap();
        cat.drop_table("AfterRollback").unwrap();

        let mut tables = cat.list_tables();
        tables.sort_unstable();
        assert_eq!(tables, vec!["Keep"]);
        assert_eq!(cat.scan("Keep").unwrap().count(), 2);
    }

    #[test]
    fn transaction_over_dirty_page_budget_is_refused_and_catalog_stays_usable() {
        let dir = tempfile::tempdir().unwrap();
        let mut cat = Catalog::create(dir.path()).unwrap();
        cat.create_table(ddl_guard_schema("Big")).unwrap();
        // 8 pages across every table: small enough to trip in a few hundred
        // rows, large enough that the first insert still fits.
        cat.set_dirty_page_budget_bytes(8 * crate::page::PAGE_SIZE);
        assert_eq!(cat.dirty_page_budget_bytes(), 8 * crate::page::PAGE_SIZE);

        cat.begin_transaction().unwrap();
        let mut refusal = None;
        for i in 0..100_000i64 {
            let row = vec![Value::Int(i), Value::Str(format!("row-{i:06}"))];
            if let Err(e) = cat.insert("Big", &row) {
                refusal = Some(e);
                break;
            }
        }
        let err = refusal.expect("an 8-page budget must refuse an unbounded transaction");
        let typed = err
            .get_ref()
            .and_then(|source| source.downcast_ref::<StorageError>());
        assert!(
            matches!(typed, Some(StorageError::TransactionTooLarge { .. })),
            "expected a typed TransactionTooLarge, got: {err}"
        );
        assert!(cat.dirty_pages_buffered() <= 8);

        // The refusal is not fatal: the connection rolls back and keeps working.
        cat.rollback_to_last_sync().unwrap();
        assert_eq!(cat.dirty_page_budget_bytes(), 8 * crate::page::PAGE_SIZE);
        assert_eq!(cat.scan("Big").unwrap().count(), 0);
        cat.insert("Big", &vec![Value::Int(1), Value::Str("after".into())])
            .unwrap();
        assert_eq!(cat.scan("Big").unwrap().count(), 1);
    }

    #[test]
    fn autocommit_writes_are_not_capped_by_the_dirty_page_budget() {
        let dir = tempfile::tempdir().unwrap();
        let mut cat = Catalog::create(dir.path()).unwrap();
        cat.create_table(ddl_guard_schema("Bulk")).unwrap();
        cat.set_dirty_page_budget_bytes(8 * crate::page::PAGE_SIZE);

        // Nothing pins the buffer for ROLLBACK here, so the budget is relieved
        // by writing pages out rather than by failing the statement.
        for i in 0..5_000i64 {
            let row = vec![Value::Int(i), Value::Str(format!("row-{i:06}"))];
            cat.insert("Bulk", &row).unwrap();
        }
        assert!(cat.dirty_pages_buffered() <= 8);
        assert_eq!(cat.scan("Bulk").unwrap().count(), 5_000);
    }
}
