//! # PowDB — embedded
//!
//! Run the PowDB engine **in-process** — no server, no socket. This is the
//! SQLite-shaped front door to the same storage engine, indexes, WAL durability,
//! and PowQL/SQL frontends that the `powdb-server` crate exposes over the wire.
//! Because there is no network round-trip, single-op latency is the engine's
//! own cost (~µs), and the database works fully offline — the foundation for
//! local-first apps.
//!
//! ```no_run
//! use powdb::{Database, QueryResult, Value};
//!
//! let mut db = Database::open("./data")?;
//! db.query("type User { required name: str, age: int }")?;
//! db.query(r#"insert User { name := "Ada", age := 36 }"#)?;
//! match db.query("count(User)")? {
//!     QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
//!     other => panic!("unexpected: {other:?}"),
//! }
//! # Ok::<(), powdb::Error>(())
//! ```
//!
//! ## Concurrency
//!
//! The embedded facade is single-writer at **compile time**, not via a
//! runtime lock: [`Database::query`] takes `&mut self`, so the borrow checker
//! guarantees a writer never runs concurrently with any other call on the
//! same handle. There is no internal mutex to contend on and no admission
//! gate (that exists only in `powdb-server`). Read-only fan-out works through
//! `&self` ([`Database::query_readonly`]), so shared references can read in
//! parallel. To share a writable handle across threads, supply your own
//! synchronization (e.g. `Arc<RwLock<Database>>`); handing out `&mut` without
//! it will not compile, which is the point.
//!
//! ## Panic safety
//!
//! The server crate is built crash-only (`panic = "abort"`): a panic exits the
//! process and a supervisor restarts it, recovering via WAL replay. An embedded
//! host can't have the database abort *it*, so every query here is wrapped in
//! [`std::panic::catch_unwind`]. A caught panic **poisons the handle** — further
//! calls return [`Error::Poisoned`] — and the handle is dropped **without** a
//! clean checkpoint (a panic mid-mutation may have left in-memory pages torn;
//! flushing them would persist garbage). Committed data is already durable in
//! the WAL, so reopening the database recovers a consistent state by replay.
//! This is the same crash-only contract, scoped to a handle instead of the
//! process. (`catch_unwind` only catches when the final binary is built with
//! `panic = "unwind"`, the default for applications and the official Node addon.)
//!
//! ## Lossless typed results
//!
//! Every query method here ([`Database::query`], [`Database::query_sql`],
//! [`Database::query_readonly`], and the `*_with_params` variants) returns a
//! [`QueryResult`] whose rows and scalar are strongly typed [`Value`]s, not
//! strings. This is the **lossless** surface: an [`Value::Int`] keeps its full
//! `i64` range, [`Value::Bytes`] keeps its raw bytes, and a JSON `null`
//! ([`Value::Json`]) stays distinct from a missing/absent cell
//! ([`Value::Empty`]). String rendering via [`Value::to_wire_string`] is a
//! separate, deliberately lossy convenience for display and the legacy string
//! protocol. Prefer the typed [`Value`] directly when a distinction matters.
//!
//! ## Parameters
//!
//! [`Database::query_with_params`] and
//! [`Database::query_readonly_with_params`] bind positional `$1..$N`
//! placeholders. Parameters are substituted as literal *tokens* before parsing,
//! so an injection-shaped string is inert data that can never change the
//! query's shape.

#![warn(missing_docs)]

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

pub use powdb_query::ast::ParamValue;
pub use powdb_query::executor::{Engine, WalSyncMode};
pub use powdb_query::result::{QueryError, QueryResult};
pub use powdb_storage::types::{TypeId, Value};
pub use powdb_sync::RETAINED_SEGMENT_FORMAT_VERSION;

/// Render canonical PJ1 (binary JSON) bytes as canonical JSON text.
///
/// Re-exported so callers of the lossless typed surface (see below) can turn a
/// [`Value::Json`] cell into JSON text without depending on `powdb-storage`
/// directly. The Node addon uses this to decode a JSON cell into a parsed
/// JavaScript value while still handing back the raw PJ1 bytes untouched.
pub use powdb_storage::pj1::pj1_to_text;

fn archive_wal_records_if_sync_enabled(
    data_dir: &Path,
    records: &[powdb_storage::wal::WalRecord],
) -> std::io::Result<()> {
    match powdb_sync::read_identity(data_dir) {
        Ok(identity) => powdb_sync::archive_wal_records_for_identity(data_dir, identity, records),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// An error from the embedded API.
#[derive(Debug)]
pub enum Error {
    /// Opening the data directory failed (I/O, permissions, corruption).
    Open(std::io::Error),
    /// The query failed (parse, plan, type, or execution error).
    Query(QueryError),
    /// The handle is poisoned: a previous call panicked. Reopen the database.
    Poisoned,
    /// Opening the database panicked (e.g. a corrupt heap/index header). Caught
    /// at the boundary so it never aborts the embedded host. The data directory
    /// is likely corrupt; restore from a backup.
    OpenPanicked,
    /// A caller-supplied argument was invalid (e.g. an unknown sync-mode name).
    InvalidArgument(String),
    /// Embedded retained-unit sync apply failed.
    Sync(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Open(e) => write!(f, "failed to open database: {e}"),
            Error::Query(e) => write!(f, "query failed: {e}"),
            Error::Poisoned => write!(
                f,
                "database handle is poisoned (a previous call panicked); reopen the database"
            ),
            Error::OpenPanicked => write!(
                f,
                "opening the database panicked (data directory may be corrupt); restore from a backup"
            ),
            Error::InvalidArgument(msg) => write!(f, "{msg}"),
            Error::Sync(e) => write!(f, "sync apply failed: {e}"),
        }
    }
}

/// Parse a JS-/CLI-facing sync-mode name into a [`WalSyncMode`] (case
/// insensitive). `None` for anything other than `full` / `normal` / `off`.
pub fn parse_sync_mode(mode: &str) -> Option<WalSyncMode> {
    match mode.to_ascii_lowercase().as_str() {
        "full" => Some(WalSyncMode::Full),
        "normal" => Some(WalSyncMode::Normal),
        "off" => Some(WalSyncMode::Off),
        _ => None,
    }
}

/// Convert typed embedded [`Value`] parameters into the query-crate
/// [`ParamValue`]s the engine binds. Only the five scalar shapes PowQL literals
/// can take are bindable; a binary shape ([`Value::Bytes`], [`Value::Uuid`],
/// [`Value::Json`], [`Value::DateTime`]) has no parameter encoding, so it is
/// rejected with an actionable [`Error::InvalidArgument`] rather than silently
/// coerced or truncated.
fn values_to_params(params: &[Value]) -> Result<Vec<ParamValue>, Error> {
    params
        .iter()
        .map(|value| match value {
            Value::Int(v) => Ok(ParamValue::Int(*v)),
            Value::Float(v) => Ok(ParamValue::Float(*v)),
            Value::Bool(v) => Ok(ParamValue::Bool(*v)),
            Value::Str(v) => Ok(ParamValue::Str(v.clone())),
            Value::Empty => Ok(ParamValue::Null),
            other => Err(Error::InvalidArgument(format!(
                "cannot bind a {:?} value as a query parameter; supported parameter types are int, float, bool, str, and null",
                other.type_id()
            ))),
        })
        .collect()
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Open(e) => Some(e),
            Error::Sync(e) => Some(e),
            _ => None,
        }
    }
}

/// Database identity and format metadata required to apply retained sync units.
///
/// Every field comes from the primary's sync identity (see `powdb-sync`); the
/// applier refuses a chunk whose identity does not match this replica's, so a
/// mismatched value is rejected rather than silently corrupting state.
#[derive(Debug, Clone)]
pub struct SyncApplyIdentity {
    /// The primary's 16-byte database id. Must equal this replica's.
    pub database_id: [u8; 16],
    /// The primary's generation counter, bumped each time it forks a new
    /// sync lineage. Guards against applying units from a diverged primary.
    pub primary_generation: u64,
    /// WAL record format version the units were written with.
    pub wal_format_version: u16,
    /// Catalog format version the units were written with.
    pub catalog_version: u16,
    /// Retained-segment container format version. Must equal
    /// [`RETAINED_SEGMENT_FORMAT_VERSION`] or the apply is rejected.
    pub segment_format_version: u16,
}

/// One retained replication unit accepted by the embedded sync applier.
#[derive(Debug, Clone)]
pub struct RetainedUnitInput {
    /// Transaction id the unit belongs to (units are applied atomically per tx).
    pub tx_id: u64,
    /// WAL record type discriminant for the unit's payload.
    pub record_type: u8,
    /// Log sequence number of the unit; units apply in ascending `lsn` order.
    pub lsn: u64,
    /// Raw serialized WAL record payload.
    pub data: Vec<u8>,
}

/// Request to apply one already-pulled retained-unit chunk.
#[derive(Debug, Clone)]
pub struct RetainedApplyRequest {
    /// The replica's current apply boundary; units at or below this lsn are
    /// skipped. Must match the seeded boundary from the sync bootstrap.
    pub since_lsn: u64,
    /// Primary identity and format metadata the units were produced under.
    pub identity: SyncApplyIdentity,
    /// The contiguous retained units to apply, in ascending `lsn` order.
    pub units: Vec<RetainedUnitInput>,
}

/// Summary returned after retained-unit chunk apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetainedApplyResult {
    /// The LSN the replica is now applied through (the chunk's last unit).
    pub through_lsn: u64,
    /// How many retained units this call replayed; zero for a no-op resume.
    pub units_applied: usize,
}

/// An in-process PowDB database handle.
///
/// One handle owns one [`Engine`] over a data directory. The handle is **not**
/// `Sync` for concurrent mutation — clone the data dir or use the server for
/// multi-writer access. Reads ([`Database::query_readonly`]) take `&self`.
pub struct Database {
    // `Option` so a poisoned handle can `take()` + `forget` the engine on drop,
    // skipping the clean checkpoint. Always `Some` until `Drop`.
    engine: Option<Engine>,
    poisoned: AtomicBool,
}

impl Database {
    /// Open (or create) a database at `dir`.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, Error> {
        let dir = dir.as_ref();
        Self::wrap_open(catch_unwind(AssertUnwindSafe(|| {
            Engine::new_with_wal_archive(dir, archive_wal_records_if_sync_enabled)
        })))
    }

    /// Open a database **read-only** over a quiescent directory for snapshot
    /// serving (a restored backup or a checkpointed replica). Nothing on disk is
    /// ever mutated: reads work, and every mutating statement (via
    /// [`Database::query`] / [`Database::query_sql`] / the `*_with_params`
    /// variants) returns a terminal read-only error instead of writing. N
    /// read-only handles across processes may serve the same directory
    /// concurrently. A non-empty WAL is refused with an actionable error: the
    /// directory must be recovered by a read-write open first.
    pub fn open_read_only(dir: impl AsRef<Path>) -> Result<Self, Error> {
        let dir = dir.as_ref();
        Self::wrap_open(catch_unwind(AssertUnwindSafe(|| {
            Engine::open_read_only(dir)
        })))
    }

    /// Read-only open with an explicit per-query memory budget (bytes).
    pub fn open_read_only_with_memory_limit(
        dir: impl AsRef<Path>,
        limit_bytes: usize,
    ) -> Result<Self, Error> {
        let dir = dir.as_ref();
        Self::wrap_open(catch_unwind(AssertUnwindSafe(|| {
            Engine::open_read_only_with_memory_limit(dir, limit_bytes)
        })))
    }

    /// Open with an explicit per-query memory budget (bytes).
    pub fn open_with_memory_limit(
        dir: impl AsRef<Path>,
        limit_bytes: usize,
    ) -> Result<Self, Error> {
        let dir = dir.as_ref();
        Self::wrap_open(catch_unwind(AssertUnwindSafe(|| {
            Engine::with_memory_limit_and_wal_archive(
                dir,
                limit_bytes,
                archive_wal_records_if_sync_enabled,
            )
        })))
    }

    /// Turn a `catch_unwind`'d engine-open result into a `Database`. A panic in
    /// the open path (e.g. a corrupt heap/index header) becomes
    /// [`Error::OpenPanicked`] instead of unwinding past the boundary and
    /// aborting the embedded host — the same crash-only contract the query
    /// methods use, applied to open.
    fn wrap_open(result: std::thread::Result<std::io::Result<Engine>>) -> Result<Self, Error> {
        let engine = result
            .map_err(|_| Error::OpenPanicked)?
            .map_err(Error::Open)?;
        Ok(Self {
            engine: Some(engine),
            poisoned: AtomicBool::new(false),
        })
    }

    /// Run a PowQL statement.
    pub fn query(&mut self, powql: &str) -> Result<QueryResult, Error> {
        self.run_mut(|e| e.execute_powql(powql))
    }

    /// Run a SQL statement (lowered to PowQL by the SQL frontend).
    pub fn query_sql(&mut self, sql: &str) -> Result<QueryResult, Error> {
        self.run_mut(|e| e.execute_sql(sql))
    }

    /// Run a read-only PowQL statement under a shared borrow. Errors if the
    /// statement would mutate.
    pub fn query_readonly(&self, powql: &str) -> Result<QueryResult, Error> {
        match self.run_ref(|e| e.execute_powql_readonly(powql)) {
            // The read-only path signals "this statement writes" with an
            // internal sentinel; translate it into an actionable message
            // rather than leaking the sentinel to embedded callers.
            Err(Error::Query(QueryError::ReadonlyNeedsWrite)) => Err(Error::InvalidArgument(
                "statement would mutate the database; use query() instead of query_readonly()"
                    .to_string(),
            )),
            other => other,
        }
    }

    /// Run a PowQL statement with positional `$1..$N` parameters bound from
    /// `params`, returning the same lossless typed [`QueryResult`] as
    /// [`Database::query`]. Parameters are substituted as literal tokens before
    /// parsing, so untrusted input can never change the query's shape.
    ///
    /// Only the five scalar parameter shapes are bindable: [`Value::Int`],
    /// [`Value::Float`], [`Value::Bool`], [`Value::Str`], and [`Value::Empty`]
    /// (which binds PowQL `null`). Binary shapes ([`Value::Bytes`],
    /// [`Value::Uuid`], [`Value::Json`], [`Value::DateTime`]) have no parameter
    /// encoding and are rejected with [`Error::InvalidArgument`].
    pub fn query_with_params(
        &mut self,
        powql: &str,
        params: &[Value],
    ) -> Result<QueryResult, Error> {
        let bound = values_to_params(params)?;
        self.run_mut(|e| e.execute_powql_with_params(powql, &bound))
    }

    /// Read-only variant of [`Database::query_with_params`] under a shared
    /// borrow. Errors with [`Error::InvalidArgument`] if the statement would
    /// mutate.
    pub fn query_readonly_with_params(
        &self,
        powql: &str,
        params: &[Value],
    ) -> Result<QueryResult, Error> {
        let bound = values_to_params(params)?;
        match self.run_ref(|e| e.execute_powql_readonly_with_params(powql, &bound)) {
            // Same sentinel translation as `query_readonly`: turn the internal
            // "this statement writes" marker into an actionable message.
            Err(Error::Query(QueryError::ReadonlyNeedsWrite)) => Err(Error::InvalidArgument(
                "statement would mutate the database; use query_with_params() instead of query_readonly_with_params()"
                    .to_string(),
            )),
            other => other,
        }
    }

    /// Set the WAL durability mode (`Full` default | `Normal` | `Off`).
    pub fn set_sync_mode(&mut self, mode: WalSyncMode) {
        if let Some(engine) = self.engine.as_mut() {
            engine.set_wal_sync_mode(mode);
        }
    }

    /// Set the WAL durability mode from a string (`"full"` | `"normal"` |
    /// `"off"`, case insensitive) — the form the Node addon and CLIs use.
    /// Errors on an unknown name rather than silently keeping the old mode.
    pub fn set_sync_mode_str(&mut self, mode: &str) -> Result<(), Error> {
        let parsed = parse_sync_mode(mode).ok_or_else(|| {
            Error::InvalidArgument(format!(
                "unknown sync mode {mode:?}; expected \"full\", \"normal\", or \"off\""
            ))
        })?;
        self.set_sync_mode(parsed);
        Ok(())
    }

    /// Whether the handle has been poisoned by a caught panic.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    /// Apply one retained-unit chunk to this embedded replica.
    ///
    /// This is the native adapter behind `@zvndev/powdb-sync`'s
    /// `LocalReplica.applyRetainedUnits` contract. The request must start at a
    /// trusted local apply boundary seeded by sync backup bootstrap.
    pub fn apply_retained_units(
        &mut self,
        request: RetainedApplyRequest,
    ) -> Result<RetainedApplyResult, Error> {
        if request.identity.segment_format_version != RETAINED_SEGMENT_FORMAT_VERSION {
            return Err(Error::InvalidArgument(format!(
                "unsupported retained segment format {}; expected {}",
                request.identity.segment_format_version, RETAINED_SEGMENT_FORMAT_VERSION
            )));
        }
        let identity = powdb_sync::SegmentIdentity {
            database_id: request.identity.database_id,
            primary_generation: request.identity.primary_generation,
            wal_format_version: request.identity.wal_format_version,
            catalog_version: request.identity.catalog_version,
        };
        let units = request
            .units
            .into_iter()
            .map(|unit| powdb_sync::RetainedUnit {
                tx_id: unit.tx_id,
                record_type: unit.record_type,
                lsn: unit.lsn,
                data: unit.data,
            })
            .collect::<Vec<_>>();

        self.run_sync_mut(|engine| {
            let summary = powdb_sync::apply_retained_units_chunk(
                engine.catalog_mut(),
                identity,
                request.since_lsn,
                &units,
            )?;
            Ok(RetainedApplyResult {
                through_lsn: summary.through_lsn,
                units_applied: summary.units_applied,
            })
        })
    }

    /// Close the database, flushing and checkpointing. Equivalent to dropping
    /// the handle, but explicit at the call site.
    pub fn close(self) {
        // Drop runs the checkpoint (or the poison path).
    }

    fn run_mut(
        &mut self,
        f: impl FnOnce(&mut Engine) -> Result<QueryResult, QueryError>,
    ) -> Result<QueryResult, Error> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(Error::Poisoned);
        }
        let engine = self.engine.as_mut().expect("engine present until drop");
        match catch_unwind(AssertUnwindSafe(|| f(engine))) {
            Ok(inner) => inner.map_err(Error::Query),
            Err(_) => {
                self.poisoned.store(true, Ordering::Release);
                Err(Error::Poisoned)
            }
        }
    }

    fn run_ref(
        &self,
        f: impl FnOnce(&Engine) -> Result<QueryResult, QueryError>,
    ) -> Result<QueryResult, Error> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(Error::Poisoned);
        }
        let engine = self.engine.as_ref().expect("engine present until drop");
        match catch_unwind(AssertUnwindSafe(|| f(engine))) {
            Ok(inner) => inner.map_err(Error::Query),
            Err(_) => {
                self.poisoned.store(true, Ordering::Release);
                Err(Error::Poisoned)
            }
        }
    }

    fn run_sync_mut<T>(
        &mut self,
        f: impl FnOnce(&mut Engine) -> std::io::Result<T>,
    ) -> Result<T, Error> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(Error::Poisoned);
        }
        let engine = self.engine.as_mut().expect("engine present until drop");
        match catch_unwind(AssertUnwindSafe(|| f(engine))) {
            Ok(inner) => inner.map_err(Error::Sync),
            Err(_) => {
                self.poisoned.store(true, Ordering::Release);
                Err(Error::Poisoned)
            }
        }
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        if let Some(engine) = self.engine.take() {
            if self.poisoned.load(Ordering::Acquire) {
                // A caught panic may have left in-memory pages torn. Skip the
                // engine's clean checkpoint (it would persist garbage) by
                // leaking the handle; committed data is durable in the WAL and
                // recovered by replay on the next open. Crash-only, per-handle.
                std::mem::forget(engine);
            }
            // Otherwise the engine drops normally (checkpoint: flush + WAL
            // truncate).
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard converts a panic into `Error::Poisoned` and flips the flag,
    /// instead of unwinding past the boundary. This is the load-bearing
    /// behavior the Node addon relies on to never abort its host.
    #[test]
    fn caught_panic_poisons_and_returns_error() {
        let poisoned = AtomicBool::new(false);
        let result: Result<(), Error> = (|| {
            if poisoned.load(Ordering::Acquire) {
                return Err(Error::Poisoned);
            }
            match catch_unwind(AssertUnwindSafe(|| panic!("boom"))) {
                Ok(()) => Ok(()),
                Err(_) => {
                    poisoned.store(true, Ordering::Release);
                    Err(Error::Poisoned)
                }
            }
        })();
        assert!(matches!(result, Err(Error::Poisoned)));
        assert!(poisoned.load(Ordering::Acquire));
    }

    /// A poisoned handle rejects further queries without touching the engine,
    /// and a poisoned drop skips the checkpoint yet loses no committed data
    /// (WAL replay recovers it on reopen).
    #[test]
    fn poisoned_handle_rejects_then_reopen_recovers() {
        let dir = std::env::temp_dir().join(format!("powdb_facade_poison_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut db = Database::open(&dir).unwrap();
            db.query("type T { required id: int }").unwrap();
            db.query("insert T { id := 1 }").unwrap(); // committed (Full mode fsyncs)
                                                       // Simulate a caught panic by poisoning the handle directly.
            db.poisoned.store(true, Ordering::Release);
            assert!(db.is_poisoned());
            assert!(matches!(db.query("count(T)"), Err(Error::Poisoned)));
            // Drop here takes the poison path: forget the engine, no checkpoint.
        }
        // Reopen: the committed row is recovered from the WAL.
        let mut db = Database::open(&dir).unwrap();
        match db.query("count(T)").unwrap() {
            QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
            other => panic!("expected 1 row after poisoned-drop reopen, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn query_error_displays_human_message_not_debug() {
        let dir = std::env::temp_dir().join(format!("powdb_facade_disp_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut db = Database::open(&dir).unwrap();
        let err = db.query("Missing filter .x > 1").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("table 'Missing' not found"),
            "expected human Display, got {msg:?}"
        );
        assert!(!msg.contains("TableNotFound"), "leaked Debug: {msg:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn query_readonly_rejects_mutation_with_actionable_message() {
        let dir = std::env::temp_dir().join(format!("powdb_facade_ro_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut db = Database::open(&dir).unwrap();
        db.query("type T { required id: int }").unwrap();
        let err = db.query_readonly("insert T { id := 1 }").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("use query()"),
            "expected actionable hint, got {msg:?}"
        );
        assert!(
            !msg.contains("__POWDB_READONLY_NEEDS_WRITE__"),
            "internal sentinel leaked to caller: {msg:?}"
        );
        assert!(matches!(err, Error::InvalidArgument(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_sync_mode_is_case_insensitive_and_rejects_unknown() {
        assert!(matches!(parse_sync_mode("full"), Some(WalSyncMode::Full)));
        assert!(matches!(
            parse_sync_mode("Normal"),
            Some(WalSyncMode::Normal)
        ));
        assert!(matches!(parse_sync_mode("OFF"), Some(WalSyncMode::Off)));
        assert!(parse_sync_mode("bogus").is_none());
    }

    #[test]
    fn set_sync_mode_str_sets_known_and_errors_on_unknown() {
        let dir = std::env::temp_dir().join(format!("powdb_syncstr_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut db = Database::open(&dir).unwrap();
        db.query("type T { required id: int }").unwrap();
        db.set_sync_mode_str("normal").unwrap();
        // A write still commits under Normal durability.
        db.query("insert T { id := 1 }").unwrap();
        assert!(db.set_sync_mode_str("bogus").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_retained_units_noop_uses_seeded_sync_boundary() {
        let dir = std::env::temp_dir().join(format!("powdb_facade_apply_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let database_id = [7u8; 16];
        seed_apply_boundary(&dir, database_id, 1, 0);

        let mut db = Database::open(&dir).unwrap();
        let result = db
            .apply_retained_units(RetainedApplyRequest {
                since_lsn: 0,
                identity: SyncApplyIdentity {
                    database_id,
                    primary_generation: 1,
                    wal_format_version: powdb_storage::wal::WAL_FORMAT_VERSION,
                    catalog_version: powdb_storage::catalog::CATALOG_VERSION,
                    segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
                },
                units: Vec::new(),
            })
            .unwrap();
        assert_eq!(
            result,
            RetainedApplyResult {
                through_lsn: 0,
                units_applied: 0
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_retained_units_rejects_wrong_segment_format() {
        let dir =
            std::env::temp_dir().join(format!("powdb_facade_apply_badfmt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut db = Database::open(&dir).unwrap();
        let err = db
            .apply_retained_units(RetainedApplyRequest {
                since_lsn: 0,
                identity: SyncApplyIdentity {
                    database_id: [7u8; 16],
                    primary_generation: 1,
                    wal_format_version: powdb_storage::wal::WAL_FORMAT_VERSION,
                    catalog_version: powdb_storage::catalog::CATALOG_VERSION,
                    segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION + 1,
                },
                units: Vec::new(),
            })
            .unwrap_err();
        assert!(matches!(err, Error::InvalidArgument(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_archives_pending_sync_wal_before_recovery_truncates() {
        let dir =
            std::env::temp_dir().join(format!("powdb_facade_sync_recovery_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let identity = {
            let mut catalog = powdb_storage::catalog::Catalog::create(&dir).unwrap();
            catalog
                .create_table(powdb_storage::types::Schema {
                    table_name: "T".into(),
                    columns: vec![powdb_storage::types::ColumnDef {
                        name: "id".into(),
                        type_id: powdb_storage::types::TypeId::Int,
                        required: true,
                        position: 0,
                    }],
                })
                .unwrap();
            catalog.insert("T", &vec![Value::Int(1)]).unwrap();
            catalog.sync_wal().unwrap();
            powdb_sync::open_or_create_identity(&dir).unwrap()
        };

        let db = Database::open(&dir).unwrap();
        drop(db);
        let units = powdb_sync::read_units_since(
            &powdb_sync::retained_segments_dir(&dir),
            identity.segment_identity(),
            0,
            100,
        )
        .unwrap();
        assert!(
            !units.is_empty(),
            "embedded facade open must preserve replayed sync WAL"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A corrupt data directory must surface as an `Err`, never unwind/abort the
    /// embedded host. A trashed heap header panics deep in the open path; the
    /// facade's `catch_unwind` must convert that into `Error::OpenPanicked`.
    /// (Only meaningful under `panic = "unwind"`, which the Node addon and the
    /// test profile use.)
    #[test]
    fn open_with_corrupt_heap_returns_error_not_panic() {
        let dir = std::env::temp_dir().join(format!("powdb_corrupt_heap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut db = Database::open(&dir).unwrap();
            db.query("type T { required id: int }").unwrap();
            db.query("insert T { id := 1 }").unwrap();
            // Clean drop checkpoints: the heap file is now the durable state.
        }
        let heap = dir.join("T.heap");
        let mut bytes = std::fs::read(&heap).unwrap();
        for b in bytes.iter_mut().take(20) {
            *b = 0xFF;
        }
        std::fs::write(&heap, &bytes).unwrap();

        let result = Database::open(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            result.is_err(),
            "corrupt heap must return Err, not panic/abort the host"
        );
    }

    /// The typed surface returns real `Value`s, not strings: an `i64` beyond
    /// `Number.MAX_SAFE_INTEGER` survives exactly, bytes survive, and a JSON
    /// `null` stays distinct from a missing (Empty) cell.
    #[test]
    fn query_returns_lossless_typed_values() {
        let dir = std::env::temp_dir().join(format!("powdb_facade_typed_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut db = Database::open(&dir).unwrap();
        db.query("type Doc { required id: int, big: int, body: json }")
            .unwrap();
        // 9_007_199_254_740_993 = 2^53 + 1, unrepresentable as an f64/JS number.
        db.query("insert Doc { id := 1, big := 9007199254740993, body := \"null\" }")
            .unwrap();
        // Row 2 omits the optional `body`, leaving it Empty (missing), and its
        // big value is a distinct large int.
        db.query("insert Doc { id := 2, big := 9223372036854775807 }")
            .unwrap();

        match db.query("Doc { id, big, body }").unwrap() {
            QueryResult::Rows { rows, .. } => {
                assert_eq!(rows.len(), 2);
                // Full i64 range preserved, not rounded through f64.
                assert_eq!(rows[0][1], Value::Int(9_007_199_254_740_993));
                assert_eq!(rows[1][1], Value::Int(9_223_372_036_854_775_807));
                // Whole JSON column carrying `null` is a typed Json value,
                // distinct from the missing/Empty cell in row 2.
                assert!(matches!(rows[0][2], Value::Json(_)));
                assert_eq!(rows[1][2], Value::Empty);
                assert_ne!(rows[0][2], rows[1][2]);
            }
            other => panic!("expected rows, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn query_with_params_binds_positional_values() {
        let dir = std::env::temp_dir().join(format!("powdb_facade_params_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut db = Database::open(&dir).unwrap();
        db.query("type User { required name: str, age: int }")
            .unwrap();
        db.query(r#"insert User { name := "Ada", age := 36 }"#)
            .unwrap();
        db.query(r#"insert User { name := "Bo", age := 20 }"#)
            .unwrap();

        // Bind a str and an int as positional params.
        match db
            .query_with_params(
                "User filter .name = $1 and .age > $2 { .name, .age }",
                &[Value::Str("Ada".into()), Value::Int(30)],
            )
            .unwrap()
        {
            QueryResult::Rows { rows, .. } => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0][0], Value::Str("Ada".into()));
                assert_eq!(rows[0][1], Value::Int(36));
            }
            other => panic!("expected rows, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn query_with_params_rejects_unbindable_shapes() {
        let dir =
            std::env::temp_dir().join(format!("powdb_facade_badparam_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut db = Database::open(&dir).unwrap();
        db.query("type T { required id: int }").unwrap();
        let err = db
            .query_with_params("T filter .id = $1 { .id }", &[Value::Bytes(vec![1, 2, 3])])
            .unwrap_err();
        assert!(matches!(err, Error::InvalidArgument(_)));
        assert!(
            err.to_string().contains("cannot bind"),
            "expected actionable message, got {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn query_readonly_with_params_rejects_mutation() {
        let dir = std::env::temp_dir().join(format!("powdb_facade_roparam_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut db = Database::open(&dir).unwrap();
        db.query("type T { required id: int }").unwrap();
        let err = db
            .query_readonly_with_params("insert T { id := $1 }", &[Value::Int(1)])
            .unwrap_err();
        assert!(matches!(err, Error::InvalidArgument(_)));
        assert!(
            !err.to_string().contains("__POWDB_READONLY_NEEDS_WRITE__"),
            "internal sentinel leaked: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn query_readonly_with_params_reads_under_shared_borrow() {
        let dir = std::env::temp_dir().join(format!("powdb_facade_rords_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut db = Database::open(&dir).unwrap();
        db.query("type T { required id: int }").unwrap();
        db.query("insert T { id := 7 }").unwrap();
        match db
            .query_readonly_with_params("T filter .id = $1 { .id }", &[Value::Int(7)])
            .unwrap()
        {
            QueryResult::Rows { rows, .. } => {
                assert_eq!(rows, vec![vec![Value::Int(7)]]);
            }
            other => panic!("expected rows, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_read_only_serves_reads_and_rejects_mutations() {
        let dir = std::env::temp_dir().join(format!("powdb_facade_ro_open_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut db = Database::open(&dir).unwrap();
            db.query("type User { required name: str, age: int }")
                .unwrap();
            db.query(r#"insert User { name := "Ada", age := 36 }"#)
                .unwrap();
            // Clean drop checkpoints: quiescent (WAL-clean) directory.
        }

        let mut db = Database::open_read_only(&dir).unwrap();
        // Reads work through both query() and query_readonly().
        match db.query("count(User)").unwrap() {
            QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
            other => panic!("expected scalar 1, got {other:?}"),
        }
        match db
            .query_readonly("User filter .age = 36 { .name }")
            .unwrap()
        {
            QueryResult::Rows { rows, .. } => {
                assert_eq!(rows[0][0], Value::Str("Ada".into()))
            }
            other => panic!("expected rows, got {other:?}"),
        }
        // A mutation via query() returns a terminal read-only error, not a panic.
        let err = db
            .query(r#"insert User { name := "Bo", age := 20 }"#)
            .unwrap_err();
        assert!(
            err.to_string().contains("readonly mode"),
            "expected a read-only error, got {err}"
        );
        assert!(!err.to_string().contains("__POWDB_READONLY_NEEDS_WRITE__"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_read_only_refuses_non_empty_wal() {
        let dir =
            std::env::temp_dir().join(format!("powdb_facade_ro_dirtywal_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            // Leave the WAL non-empty by forgetting the catalog (a crash).
            let mut catalog = powdb_storage::catalog::Catalog::create(&dir).unwrap();
            catalog
                .create_table(powdb_storage::types::Schema {
                    table_name: "T".into(),
                    columns: vec![powdb_storage::types::ColumnDef {
                        name: "id".into(),
                        type_id: powdb_storage::types::TypeId::Int,
                        required: true,
                        position: 0,
                    }],
                })
                .unwrap();
            catalog.insert("T", &vec![Value::Int(1)]).unwrap();
            catalog.sync_wal().unwrap();
            std::mem::forget(catalog);
        }
        let err = match Database::open_read_only(&dir) {
            Ok(_) => panic!("read-only open must refuse a non-empty WAL"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("WAL is not empty"),
            "expected WAL-not-empty refusal, got {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn seed_apply_boundary(
        dir: &std::path::Path,
        database_id: [u8; 16],
        generation: u64,
        lsn: u64,
    ) {
        let sync_dir = dir.join(".powdb-sync");
        std::fs::create_dir_all(&sync_dir).unwrap();
        let database_id_hex = encode_hex_16(database_id);
        std::fs::write(
            sync_dir.join("identity.json"),
            format!(
                r#"{{"format_version":1,"database_id":"{database_id_hex}","primary_generation":{generation},"created_unix_secs":1}}"#
            ),
        )
        .unwrap();
        std::fs::write(
            sync_dir.join("apply-state.json"),
            format!(
                r#"{{"format_version":1,"database_id":{},"primary_generation":{generation},"wal_format_version":{},"catalog_version":{},"from_lsn":{lsn},"through_lsn":{lsn},"applied_lsn":{lsn},"status":"complete","started_unix_secs":1,"updated_unix_secs":1}}"#,
                json_u8_array(database_id),
                powdb_storage::wal::WAL_FORMAT_VERSION,
                powdb_storage::catalog::CATALOG_VERSION,
            ),
        )
        .unwrap();
    }

    fn encode_hex_16(bytes: [u8; 16]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn json_u8_array(bytes: [u8; 16]) -> String {
        let body = bytes
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",");
        format!("[{body}]")
    }
}
