//! Node native addon (napi-rs) for **embedded PowDB** — run the engine
//! in-process, no server, no socket. Thin wrapper over [`powdb::Database`]
//! (the embedded facade), exposing a JS `Database` class whose result shape
//! matches the `@zvndev/powdb-client` `QueryResult` so embedded and networked
//! code paths are interchangeable.
//!
//! This crate is built with `panic = "unwind"` (see Cargo.toml) so the facade's
//! `catch_unwind` actually catches: an engine panic poisons the handle and
//! returns a JS error instead of aborting the host process.

use napi::bindgen_prelude::{BigInt, Buffer, Either, Uint8Array};
use napi_derive::napi;

use powdb::{
    Database as Inner, Error as PowdbError, QueryResult, RetainedApplyRequest, RetainedApplyResult,
    RetainedUnitInput, SyncApplyIdentity, Value,
};

fn to_napi_err(e: PowdbError) -> napi::Error {
    napi::Error::from_reason(e.to_string())
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
    inner: Inner,
}

#[napi]
impl Database {
    /// Open (or create) a database at `dir`. No server is started.
    #[napi(factory)]
    pub fn open(dir: String) -> napi::Result<Database> {
        Inner::open(&dir)
            .map(|inner| Database { inner })
            .map_err(to_napi_err)
    }

    /// Open (or create) a database at `dir` with an explicit per-query memory
    /// budget in bytes (caps sort/join/GROUP BY materialization).
    #[napi(factory)]
    pub fn open_with_memory_limit(dir: String, limit_bytes: i64) -> napi::Result<Database> {
        let limit = usize::try_from(limit_bytes)
            .map_err(|_| napi::Error::from_reason("limit_bytes must be a non-negative integer"))?;
        Inner::open_with_memory_limit(&dir, limit)
            .map(|inner| Database { inner })
            .map_err(to_napi_err)
    }

    /// Set the WAL durability mode: `"full"` (default — one fsync per commit,
    /// safest), `"normal"` (off-lock background fsync — much faster writes, a
    /// bounded crash-loss window), or `"off"` (no durability — tests/bench
    /// only). `"normal"` is what closes the embedded write gap vs SQLite.
    #[napi]
    pub fn set_sync_mode(&mut self, mode: String) -> napi::Result<()> {
        self.inner.set_sync_mode_str(&mode).map_err(to_napi_err)
    }

    /// Run a PowQL statement.
    #[napi]
    pub fn query(&mut self, powql: String) -> napi::Result<QueryResultJs> {
        self.inner.query(&powql).map(to_js).map_err(to_napi_err)
    }

    /// Run a SQL statement (lowered to PowQL by the SQL frontend).
    #[napi]
    pub fn query_sql(&mut self, sql: String) -> napi::Result<QueryResultJs> {
        self.inner.query_sql(&sql).map(to_js).map_err(to_napi_err)
    }

    /// Run a read-only PowQL statement. Errors if it would mutate.
    #[napi]
    pub fn query_readonly(&self, powql: String) -> napi::Result<QueryResultJs> {
        self.inner
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
        self.inner
            .apply_retained_units(request)
            .map_err(to_napi_err)
            .and_then(apply_result_to_js)
    }

    /// Whether a previous call panicked and poisoned the handle (reopen needed).
    #[napi]
    pub fn is_poisoned(&self) -> bool {
        self.inner.is_poisoned()
    }
}
