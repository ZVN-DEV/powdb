//! Node native addon (napi-rs) for **embedded PowDB** — run the engine
//! in-process, no server, no socket. Thin wrapper over [`powdb::Database`]
//! (the embedded facade), exposing a JS `Database` class whose result shape
//! matches the `@zvndev/powdb-client` `QueryResult` so embedded and networked
//! code paths are interchangeable.
//!
//! This crate is built with `panic = "unwind"` (see Cargo.toml) so the facade's
//! `catch_unwind` actually catches: an engine panic poisons the handle and
//! returns a JS error instead of aborting the host process.

use napi::bindgen_prelude::BigInt;
use napi_derive::napi;

use powdb::{Database as Inner, Error as PowdbError, QueryResult, Value};

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

    /// Whether a previous call panicked and poisoned the handle (reopen needed).
    #[napi]
    pub fn is_poisoned(&self) -> bool {
        self.inner.is_poisoned()
    }
}
