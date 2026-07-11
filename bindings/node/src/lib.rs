//! Node native addon (napi-rs) for **embedded PowDB** — run the engine
//! in-process, no server, no socket. Thin wrapper over [`powdb::Database`]
//! (the embedded facade), exposing a JS `Database` class whose result shape
//! matches the `@zvndev/powdb-client` `QueryResult` so embedded and networked
//! code paths are interchangeable.
//!
//! This crate is built with `panic = "unwind"` (see Cargo.toml) so the facade's
//! `catch_unwind` actually catches: an engine panic poisons the handle and
//! returns a JS error instead of aborting the host process.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use napi::bindgen_prelude::{BigInt, Buffer, Either, Uint8Array};
use napi_derive::napi;

use powdb::{
    Database as Inner, Error as PowdbError, QueryResult, RetainedApplyRequest, RetainedApplyResult,
    RetainedUnitInput, SyncApplyIdentity, Value,
};

fn to_napi_err(e: PowdbError) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

/// The `DirLock` in the storage engine deliberately allows a *same-process*
/// reopen (the crash-recovery suite relies on it), so two `Database.open()`
/// calls for one directory in a single Node process would each get a live
/// engine over the same heap/WAL — the exact concurrent-writer corruption the
/// lock exists to prevent. This process-wide registry of canonicalized open
/// paths closes that user-facing hole; `close()` (or GC) clears the entry.
fn open_registry() -> &'static Mutex<HashSet<PathBuf>> {
    static REGISTRY: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Best-effort canonical key for a data dir that may not exist yet. Falls back
/// to canonicalizing the parent (then re-appending the leaf) or the raw path.
fn canonical_key(dir: &str) -> PathBuf {
    let path = Path::new(dir);
    if let Ok(canon) = path.canonicalize() {
        return canon;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => match parent.canonicalize()
        {
            Ok(canon) => canon.join(name),
            Err(_) => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

/// Reserve `key` in the process registry, or fail if it is already open here.
fn register_open(key: &Path) -> napi::Result<()> {
    let mut set = open_registry().lock().unwrap_or_else(|e| e.into_inner());
    if !set.insert(key.to_path_buf()) {
        return Err(napi::Error::from_reason(format!(
            "data directory {} is already open in this process; close that handle first",
            key.display()
        )));
    }
    Ok(())
}

fn unregister_open(key: &Path) {
    let mut set = open_registry().lock().unwrap_or_else(|e| e.into_inner());
    set.remove(key);
}

/// A query result, shaped to match the `@zvndev/powdb-client` `QueryResult`
/// union. `kind` selects which fields are populated:
/// - `"rows"`   → `columns`, `rows`
/// - `"scalar"` → `value`
/// - `"ok"`     → `affected`
/// - `"message"`→ `message`
#[napi(object)]
pub struct QueryResultJs {
    pub kind: String,
    pub columns: Option<Vec<String>>,
    pub rows: Option<Vec<Vec<String>>>,
    pub value: Option<String>,
    // `BigInt` to match `@zvndev/powdb-client`'s `affected: bigint` (the wire
    // protocol carries a u64), so embedded and networked results are identical.
    pub affected: Option<BigInt>,
    pub message: Option<String>,
}

/// One retained unit from `@zvndev/powdb-client`'s `syncPull(...)` result.
#[napi(object)]
pub struct RetainedUnitJs {
    pub tx_id: BigInt,
    pub record_type: u32,
    pub lsn: BigInt,
    pub data: Either<Uint8Array, Buffer>,
}

/// Request shape consumed by `Database.applyRetainedUnits(...)`.
#[napi(object)]
pub struct ApplyRetainedUnitsRequestJs {
    pub since_lsn: BigInt,
    pub database_id: Either<String, Uint8Array>,
    pub primary_generation: BigInt,
    pub wal_format_version: u32,
    pub catalog_version: u32,
    pub segment_format_version: u32,
    pub units: Vec<RetainedUnitJs>,
}

/// Summary returned by `Database.applyRetainedUnits(...)`.
#[napi(object)]
pub struct ApplyRetainedUnitsResultJs {
    pub through_lsn: BigInt,
    pub units_applied: u32,
}

fn empty() -> QueryResultJs {
    QueryResultJs {
        kind: String::new(),
        columns: None,
        rows: None,
        value: None,
        affected: None,
        message: None,
    }
}

fn to_js(r: QueryResult) -> QueryResultJs {
    match r {
        QueryResult::Rows { columns, rows } => QueryResultJs {
            kind: "rows".into(),
            columns: Some(columns),
            rows: Some(
                rows.into_iter()
                    .map(|row| row.iter().map(Value::to_wire_string).collect())
                    .collect(),
            ),
            ..empty()
        },
        QueryResult::Scalar(v) => QueryResultJs {
            kind: "scalar".into(),
            value: Some(v.to_wire_string()),
            ..empty()
        },
        QueryResult::Modified(n) => QueryResultJs {
            kind: "ok".into(),
            affected: Some(BigInt::from(n)),
            ..empty()
        },
        // Match the server's wording so embedded and wire results agree.
        QueryResult::Created(name) => QueryResultJs {
            kind: "message".into(),
            message: Some(format!("type {name} created")),
            ..empty()
        },
        QueryResult::Executed { message } => QueryResultJs {
            kind: "message".into(),
            message: Some(message),
            ..empty()
        },
    }
}

fn bigint_to_u64(value: &BigInt, label: &str) -> napi::Result<u64> {
    let (signed, raw, lossless) = value.get_u64();
    if signed || !lossless {
        return Err(napi::Error::from_reason(format!(
            "{label} must be a non-negative u64 BigInt"
        )));
    }
    Ok(raw)
}

fn u32_to_u16(value: u32, label: &str) -> napi::Result<u16> {
    u16::try_from(value).map_err(|_| napi::Error::from_reason(format!("{label} must fit in u16")))
}

fn u32_to_u8(value: u32, label: &str) -> napi::Result<u8> {
    u8::try_from(value).map_err(|_| napi::Error::from_reason(format!("{label} must fit in u8")))
}

fn decode_hex_16(hex: &str) -> napi::Result<[u8; 16]> {
    if hex.len() != 32 || !hex.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        return Err(napi::Error::from_reason(
            "databaseId must be exactly 32 hex characters",
        ));
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        let start = i * 2;
        *byte = u8::from_str_radix(&hex[start..start + 2], 16).map_err(|_| {
            napi::Error::from_reason("databaseId must be exactly 32 hex characters")
        })?;
    }
    Ok(out)
}

fn decode_database_id(database_id: Either<String, Uint8Array>) -> napi::Result<[u8; 16]> {
    match database_id {
        Either::A(hex) => decode_hex_16(&hex),
        Either::B(bytes) => {
            let bytes = bytes.as_ref();
            if bytes.len() != 16 {
                return Err(napi::Error::from_reason(format!(
                    "databaseId must be exactly 16 bytes, got {}",
                    bytes.len()
                )));
            }
            let mut out = [0u8; 16];
            out.copy_from_slice(bytes);
            Ok(out)
        }
    }
}

fn retained_data_to_vec(data: Either<Uint8Array, Buffer>) -> Vec<u8> {
    match data {
        Either::A(bytes) => bytes.as_ref().to_vec(),
        Either::B(bytes) => bytes.as_ref().to_vec(),
    }
}

fn to_apply_request(request: ApplyRetainedUnitsRequestJs) -> napi::Result<RetainedApplyRequest> {
    let units = request
        .units
        .into_iter()
        .map(|unit| {
            Ok(RetainedUnitInput {
                tx_id: bigint_to_u64(&unit.tx_id, "txId")?,
                record_type: u32_to_u8(unit.record_type, "recordType")?,
                lsn: bigint_to_u64(&unit.lsn, "lsn")?,
                data: retained_data_to_vec(unit.data),
            })
        })
        .collect::<napi::Result<Vec<_>>>()?;
    Ok(RetainedApplyRequest {
        since_lsn: bigint_to_u64(&request.since_lsn, "sinceLsn")?,
        identity: SyncApplyIdentity {
            database_id: decode_database_id(request.database_id)?,
            primary_generation: bigint_to_u64(&request.primary_generation, "primaryGeneration")?,
            wal_format_version: u32_to_u16(request.wal_format_version, "walFormatVersion")?,
            catalog_version: u32_to_u16(request.catalog_version, "catalogVersion")?,
            segment_format_version: u32_to_u16(
                request.segment_format_version,
                "segmentFormatVersion",
            )?,
        },
        units,
    })
}

fn apply_result_to_js(result: RetainedApplyResult) -> napi::Result<ApplyRetainedUnitsResultJs> {
    let units_applied = u32::try_from(result.units_applied).map_err(|_| {
        napi::Error::from_reason("unitsApplied does not fit in a JavaScript number")
    })?;
    Ok(ApplyRetainedUnitsResultJs {
        through_lsn: BigInt::from(result.through_lsn),
        units_applied,
    })
}

/// An in-process PowDB database. Open it once and reuse the handle.
#[napi(js_name = "Database")]
pub struct Database {
    // `None` after `close()`. Every method rejects a closed handle instead of
    // operating on a dropped engine.
    inner: Option<Inner>,
    key: PathBuf,
}

const CLOSED: &str = "database is closed";

#[napi]
impl Database {
    /// Open (or create) a database at `dir`. No server is started. Throws if
    /// the same directory is already open elsewhere in this process.
    #[napi(factory)]
    pub fn open(dir: String) -> napi::Result<Database> {
        let key = canonical_key(&dir);
        register_open(&key)?;
        match Inner::open(&dir) {
            Ok(inner) => Ok(Database {
                inner: Some(inner),
                key,
            }),
            Err(e) => {
                unregister_open(&key);
                Err(to_napi_err(e))
            }
        }
    }

    /// Open (or create) a database at `dir` with an explicit per-query memory
    /// budget in bytes (caps sort/join/GROUP BY materialization). Throws if the
    /// same directory is already open elsewhere in this process.
    #[napi(factory)]
    pub fn open_with_memory_limit(dir: String, limit_bytes: i64) -> napi::Result<Database> {
        let limit = usize::try_from(limit_bytes)
            .map_err(|_| napi::Error::from_reason("limit_bytes must be a non-negative integer"))?;
        let key = canonical_key(&dir);
        register_open(&key)?;
        match Inner::open_with_memory_limit(&dir, limit) {
            Ok(inner) => Ok(Database {
                inner: Some(inner),
                key,
            }),
            Err(e) => {
                unregister_open(&key);
                Err(to_napi_err(e))
            }
        }
    }

    fn inner_mut(&mut self) -> napi::Result<&mut Inner> {
        self.inner
            .as_mut()
            .ok_or_else(|| napi::Error::from_reason(CLOSED))
    }

    fn inner_ref(&self) -> napi::Result<&Inner> {
        self.inner
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason(CLOSED))
    }

    /// Set the WAL durability mode: `"full"` (default — one fsync per commit,
    /// safest), `"normal"` (off-lock background fsync — much faster writes, a
    /// bounded crash-loss window), or `"off"` (no durability — tests/bench
    /// only). `"normal"` is what closes the embedded write gap vs SQLite.
    #[napi]
    pub fn set_sync_mode(&mut self, mode: String) -> napi::Result<()> {
        self.inner_mut()?
            .set_sync_mode_str(&mode)
            .map_err(to_napi_err)
    }

    /// Run a PowQL statement.
    #[napi]
    pub fn query(&mut self, powql: String) -> napi::Result<QueryResultJs> {
        self.inner_mut()?
            .query(&powql)
            .map(to_js)
            .map_err(to_napi_err)
    }

    /// Run a SQL statement (lowered to PowQL by the SQL frontend).
    #[napi]
    pub fn query_sql(&mut self, sql: String) -> napi::Result<QueryResultJs> {
        self.inner_mut()?
            .query_sql(&sql)
            .map(to_js)
            .map_err(to_napi_err)
    }

    /// Run a read-only PowQL statement. Errors if it would mutate.
    #[napi]
    pub fn query_readonly(&self, powql: String) -> napi::Result<QueryResultJs> {
        self.inner_ref()?
            .query_readonly(&powql)
            .map(to_js)
            .map_err(to_napi_err)
    }

    /// Apply one already-pulled retained-unit chunk to this embedded replica.
    #[napi]
    pub fn apply_retained_units(
        &mut self,
        request: ApplyRetainedUnitsRequestJs,
    ) -> napi::Result<ApplyRetainedUnitsResultJs> {
        let request = to_apply_request(request)?;
        self.inner_mut()?
            .apply_retained_units(request)
            .map_err(to_napi_err)
            .and_then(apply_result_to_js)
    }

    /// Whether a previous call panicked and poisoned the handle (reopen needed).
    /// A closed handle reports `false`.
    #[napi]
    pub fn is_poisoned(&self) -> bool {
        self.inner.as_ref().is_some_and(Inner::is_poisoned)
    }

    /// Close the database: flush and checkpoint (unless poisoned), then release
    /// the data-directory lock so another process — or another handle in this
    /// one — can open it. Every later call throws `database is closed`. Calling
    /// `close()` on an already-closed handle throws the same error.
    #[napi]
    pub fn close(&mut self) -> napi::Result<()> {
        match self.inner.take() {
            Some(inner) => {
                inner.close();
                unregister_open(&self.key);
                Ok(())
            }
            None => Err(napi::Error::from_reason(CLOSED)),
        }
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        // GC finalizer path: if the handle was never explicitly closed, release
        // the registry slot (the engine's own Drop handles the checkpoint/lock).
        if self.inner.is_some() {
            unregister_open(&self.key);
        }
    }
}
