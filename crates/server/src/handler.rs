use crate::metrics::{Metrics, QueryOutcome, SyncOperation, SyncOutcome, SyncRepairLabel};
use crate::protocol::{
    ErrorClass, Message, WireParam, WireRetainedUnit, WireSyncRepairAction, WireSyncStatus,
};
use powdb_auth::{Permission, Role, UserStore};
use powdb_query::executor::{is_read_only_statement, Engine, WalDurabilityTicket};
use powdb_query::parser;
use powdb_query::result::{QueryError, QueryResult};
use powdb_query::sql;
use powdb_storage::types::Value;
use powdb_sync::{
    acknowledge_replica_apply, read_identity, read_units_through, replica_sync_status,
    retained_segments_dir, validate_retained_tail_available, validate_v1_retained_units_applyable,
    ReplicaSyncStatus, RetainedUnit, SegmentIdentity, SyncRepairAction,
    RETAINED_SEGMENT_FORMAT_VERSION,
};
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, error, info, warn};
use zeroize::Zeroizing;

/// Tracks per-IP authentication failure counts for rate limiting.
pub type AuthRateLimiter = Arc<Mutex<HashMap<IpAddr, (u32, Instant)>>>;

/// Fixed reader-permit pool. Read-only autocommit statements take one permit;
/// writers, sync operations, and explicit transactions take the entire pool.
/// Tokio's fair semaphore queue prevents a waiting writer from being starved by
/// later readers.
#[derive(Clone)]
pub struct TxGate {
    semaphore: Arc<Semaphore>,
    permit_count: u32,
}

pub const DEFAULT_TX_GATE_READER_PERMITS: u32 = 1024;

/// Create a transaction gate for a shared engine.
pub fn new_tx_gate() -> TxGate {
    new_tx_gate_with_permits(DEFAULT_TX_GATE_READER_PERMITS)
}

/// Create a transaction gate with an explicit reader capacity.
///
/// The configurable constructor exists so benchmark and compatibility tests can
/// reproduce the former single-permit admission policy using the exact same
/// handler code. Production uses [`new_tx_gate`].
pub fn new_tx_gate_with_permits(permit_count: u32) -> TxGate {
    assert!(permit_count > 0, "transaction gate requires a permit");
    TxGate {
        semaphore: Arc::new(Semaphore::new(permit_count as usize)),
        permit_count,
    }
}

impl TxGate {
    pub fn permit_count(&self) -> u32 {
        self.permit_count
    }

    fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    async fn acquire_many_owned(
        self,
        permits: u32,
    ) -> Result<OwnedSemaphorePermit, tokio::sync::AcquireError> {
        self.semaphore.acquire_many_owned(permits).await
    }

    fn try_acquire_many_owned(
        self,
        permits: u32,
    ) -> Result<OwnedSemaphorePermit, tokio::sync::TryAcquireError> {
        self.semaphore.try_acquire_many_owned(permits)
    }
}

/// Maximum query text length accepted from the wire (1 MB).
const MAX_QUERY_LENGTH: usize = 1024 * 1024;

/// Maximum payload accepted by the post-auth cancellation-safe frame reader.
/// Keep this equal to the protocol reader's public wire limit.
const MAX_WIRE_PAYLOAD_SIZE: usize = 64 * 1024 * 1024;

/// Frames received while a query is executing are retained for normal
/// pipelined processing. Both limits are deliberately much smaller than the
/// ordinary 64 MiB per-frame protocol limit: in-flight read-ahead is merely a
/// liveness aid, not a second request buffer. Reaching either cap cancels the
/// query and closes the connection; socket monitoring is never disabled.
const MAX_IN_FLIGHT_READ_AHEAD_FRAMES: usize = 128;
const MAX_IN_FLIGHT_READ_AHEAD_BYTES: usize = 1024 * 1024;

/// Server-side cap for private sync pull batches.
const MAX_SYNC_PULL_UNITS: u32 = 4096;

/// Server-side cap for retained-unit payload bytes in a private sync pull.
const MAX_SYNC_PULL_BYTES: u64 = 16 * 1024 * 1024;

/// Maximum encoded response payload size (64 MB). The wire format is still a
/// single frame today, so oversized result sets must fail cleanly instead of
/// building an unbounded `Vec<Vec<String>>` and frame in memory.
#[cfg(not(test))]
const MAX_RESPONSE_PAYLOAD_SIZE: usize = 64 * 1024 * 1024;
#[cfg(test)]
const MAX_RESPONSE_PAYLOAD_SIZE: usize = 1024;

/// Timeout for writing a response to the client. Prevents slow-drain
/// clients from blocking the handler indefinitely.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of auth failures per IP within the rate-limit window.
const MAX_AUTH_FAILURES: u32 = 5;

/// Window during which auth failures are counted (60 seconds).
const AUTH_FAILURE_WINDOW: Duration = Duration::from_secs(60);

/// Create a new shared rate limiter.
pub fn new_rate_limiter() -> AuthRateLimiter {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Check whether an IP is rate-limited and record a failure if requested.
/// Returns `true` if the IP should be rejected.
fn is_rate_limited(limiter: &AuthRateLimiter, ip: IpAddr) -> bool {
    let mut map = limiter.lock().unwrap_or_else(|e| e.into_inner());
    // Clean up stale entries while we have the lock.
    let now = Instant::now();
    map.retain(|_, (_, ts)| now.duration_since(*ts) < AUTH_FAILURE_WINDOW);

    if let Some((count, _)) = map.get(&ip) {
        *count >= MAX_AUTH_FAILURES
    } else {
        false
    }
}

/// Record an auth failure for the given IP.
fn record_auth_failure(limiter: &AuthRateLimiter, ip: IpAddr) {
    let mut map = limiter.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let entry = map.entry(ip).or_insert((0, now));
    // Reset counter if the window has elapsed.
    if now.duration_since(entry.1) >= AUTH_FAILURE_WINDOW {
        *entry = (1, now);
    } else {
        entry.0 += 1;
    }
}

/// Clear the failure counter on successful auth.
fn clear_auth_failures(limiter: &AuthRateLimiter, ip: IpAddr) {
    let mut map = limiter.lock().unwrap_or_else(|e| e.into_inner());
    map.remove(&ip);
}

/// Constant-time password comparison. Hashes both inputs to fixed-size
/// SHA-256 digests so neither length nor content leaks through timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    let ha = Sha256::digest(a);
    let hb = Sha256::digest(b);
    let mut diff = 0u8;
    for (x, y) in ha.iter().zip(hb.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// An authenticated connection's identity. Bound at connect time and consulted
/// on every query by `dispatch_query` to enforce the user's role: a
/// `readonly` principal may only execute read statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub name: String,
    pub role: String,
}

/// Whether a parsed statement is data-definition (schema) work: creating,
/// altering, or dropping a type or view. `explain <ddl>` is classified by its
/// inner statement so `explain drop User` needs the same permission as
/// `drop User`. Mutations that change *rows* (insert/update/delete/upsert/
/// refresh) and transaction control are NOT DDL — they fall under `Write`.
fn is_ddl_statement(stmt: &powdb_query::ast::Statement) -> bool {
    use powdb_query::ast::Statement;
    let inner = match stmt {
        Statement::Explain(inner) => inner.as_ref(),
        other => other,
    };
    matches!(
        inner,
        Statement::CreateType(_)
            | Statement::AlterTable(_)
            | Statement::DropTable(_)
            | Statement::CreateView(_)
            | Statement::DropView(_)
    )
}

/// The capability a parsed statement requires under the RBAC lattice
/// (`crates/auth/src/role.rs`). Reads need [`Permission::Read`]; schema
/// definition needs [`Permission::Ddl`]; every other mutation needs
/// [`Permission::Write`]. [`Permission::Admin`] is reserved for user/role
/// management, which is CLI-only today and never reaches this wire path.
fn required_permission(stmt: &powdb_query::ast::Statement) -> Permission {
    if is_read_only_statement(stmt) {
        Permission::Read
    } else if is_ddl_statement(stmt) {
        Permission::Ddl
    } else {
        Permission::Write
    }
}

/// Enforce the principal's role against a parsed statement using the full
/// permission lattice. Reads are always permitted (any authenticated role can
/// read — unknown role names still read but fail closed on any mutation).
/// Mutations require the specific capability the statement maps to: row
/// mutations need `Write`, schema changes need `Ddl`. Unknown role names
/// resolve to no builtin and therefore grant nothing beyond reads.
///
/// Classification uses the parsed AST via
/// [`powdb_query::executor::is_read_only_statement`] — the exact same
/// classifier the RwLock read/write split relies on — so the permission
/// boundary and the concurrency boundary can never disagree.
fn check_statement_permitted(
    principal: Option<&Principal>,
    stmt: &powdb_query::ast::Statement,
) -> Result<(), QueryError> {
    let Some(p) = principal else {
        // No per-user identity (shared-password or open mode): full access,
        // byte-identical to the pre-RBAC behavior.
        return Ok(());
    };
    // Reads are permitted for every authenticated principal (preserves the
    // pre-lattice contract that any connected role may run read-only queries).
    if is_read_only_statement(stmt) {
        return Ok(());
    }
    let needed = required_permission(stmt);
    if Role::builtin(&p.role).is_some_and(|r| r.allows(needed)) {
        return Ok(());
    }
    let kind = if needed == Permission::Ddl {
        "schema-definition"
    } else {
        "write"
    };
    Err(QueryError::Execution(format!(
        "permission denied: role '{}' cannot execute {kind} statements",
        p.role
    )))
}

/// Result of the connect-time authentication decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthOutcome {
    /// Authenticated. `principal` is `Some` when a named user authenticated via
    /// the UserStore, and `None` for the legacy shared-password / open paths
    /// where there is no per-user identity.
    Authenticated { principal: Option<Principal> },
    /// Rejected. The caller sends a generic "authentication failed" error and
    /// records a rate-limit failure — it must not reveal which check failed.
    Rejected,
}

/// Pure, exhaustively-testable authentication decision for a CONNECT handshake.
///
/// Policy:
/// - If `users` has at least one user, multi-user auth is in force: a
///   `username` is required and `users.authenticate(username, password)` must
///   succeed. Unknown user, wrong password, or a missing username all reject
///   with an indistinguishable `Rejected` (no user-vs-password leak).
/// - If `users` is empty, fall back verbatim to the legacy behavior: when
///   `expected_password` is `Some`, the candidate must match it (constant time);
///   when `None`, no auth is required (open). The `username` is ignored here so
///   that a new client talking to a shared-password server still connects.
pub fn authenticate_connect(
    users: &UserStore,
    expected_password: Option<&str>,
    username: Option<&str>,
    password: Option<&str>,
) -> AuthOutcome {
    if !users.is_empty() {
        // Multi-user mode: a username is mandatory.
        let Some(name) = username else {
            return AuthOutcome::Rejected;
        };
        let Some(candidate) = password else {
            return AuthOutcome::Rejected;
        };
        match users.authenticate(name, candidate) {
            Some(user) => AuthOutcome::Authenticated {
                principal: Some(Principal {
                    name: user.name.clone(),
                    role: user.role.clone(),
                }),
            },
            None => AuthOutcome::Rejected,
        }
    } else {
        // Legacy shared-password fallback (byte-identical to prior behavior).
        match expected_password {
            Some(expected) => {
                if password.is_some_and(|p| constant_time_eq(p.as_bytes(), expected.as_bytes())) {
                    AuthOutcome::Authenticated { principal: None }
                } else {
                    AuthOutcome::Rejected
                }
            }
            None => AuthOutcome::Authenticated { principal: None },
        }
    }
}

/// The sentinel database name clients send when the user selected none. Both
/// the CLI and the TS client default to this, so it means "no specific
/// database" and is always accepted — even when the server is pinned to a name.
const DEFAULT_DB_NAME: &str = "default";

/// Decide whether a CONNECT's requested `db_name` is served by this process.
///
/// One server process serves exactly one global database. When it is pinned to
/// a name (`configured = Some`), a request that *explicitly* names a different
/// database is rejected so a client can never silently read/write the wrong
/// store. An empty name or the client default sentinel (`"default"`) means "no
/// specific database selected" and is always accepted. When unpinned (`None`)
/// every name is accepted (0.9.x back-compat); the caller warns on a non-default
/// name so the silent-mismatch footgun is at least visible in the logs.
fn check_db_name(configured: Option<&str>, requested: &str) -> Result<(), String> {
    if requested.is_empty() || requested == DEFAULT_DB_NAME {
        return Ok(());
    }
    match configured {
        None => Ok(()),
        Some(name) if requested == name => Ok(()),
        Some(name) => Err(format!(
            "unknown database '{requested}'; this server serves '{name}'"
        )),
    }
}

/// Error messages that are safe to forward to the client verbatim.
const SAFE_ERROR_PREFIXES: &[&str] = &[
    "table not found",
    // The executor's actual phrasing is `table 'X' not found`, which the
    // bare prefix above never matches — keep both so the real message
    // reaches clients.
    "table '",
    "type '",
    "column not found",
    // Lexer diagnostics (`at position N: unterminated quoted identifier`)
    // are derived purely from the client's own query text.
    "at position",
    "parse error",
    "type mismatch",
    "unknown table",
    "unknown column",
    "unknown function",
    "syntax error",
    "expected",
    "unexpected",
    "missing",
    "duplicate",
    "invalid",
    "cannot",
    "no such",
    "already exists",
    "permission denied",
    "row too large",
    "unique constraint violation",
    // Resource-limit errors carry actionable guidance (e.g. "add a LIMIT
    // clause") and leak no internal state, so surface them verbatim instead
    // of masking them to the generic message. See QueryError::{SortLimit,
    // JoinLimit,MemoryLimit}Exceeded in crates/query/src/result.rs.
    "sort input exceeds",
    "join result exceeds",
    "query exceeded memory budget",
    "result too large",
    // A failed covering fsync means the statement executed in memory but was
    // never made durable — the client MUST be able to distinguish this from
    // an ordinary failed query (the statement may still be visible until the
    // server restarts). The io::Error detail leaks no internal state.
    "wal durability sync failed",
    // Cooperative query cancellation. Both messages are derived purely from
    // the configured timeout / a client disconnect and leak no internal state.
    // See QueryError::{Timeout,Cancelled} in crates/query/src/result.rs.
    "query timeout after",
    "query cancelled",
    // Read-only snapshot serving refuses mutations (and reads needing a writer,
    // e.g. a stale materialized view) with an operator-facing message that names
    // the mode and the fix. It leaks no internal state.
    // See QueryError::ReadonlyMode in crates/query/src/result.rs.
    "readonly mode",
];

/// Build the client-facing error frame: sanitized message plus the stable
/// 1-byte [`ErrorClass`]. The class is orthogonal to the message text: it is
/// derived from the typed error (or the call site), never from the message,
/// so sanitization to a generic string does not degrade it.
fn error_response(message: impl Into<String>, class: ErrorClass) -> Message {
    Message::ErrorWithClass {
        message: message.into(),
        class,
    }
}

/// Map a [`QueryError`] to its stable wire [`ErrorClass`].
///
/// [`QueryError::ReadonlyNeedsWrite`] is an internal retry sentinel the
/// server intercepts before Display; if it ever reaches classification it is
/// reported as [`ErrorClass::Internal`], matching the generic message the
/// caller sends for that path.
fn classify_query_error(e: &QueryError) -> ErrorClass {
    match e {
        QueryError::Parse(_) => ErrorClass::Parse,
        QueryError::Timeout { .. } => ErrorClass::Timeout,
        QueryError::Cancelled => ErrorClass::Cancelled,
        QueryError::ReadonlyMode => ErrorClass::ReadonlyRefused,
        QueryError::ReadonlyNeedsWrite => ErrorClass::Internal,
        QueryError::JoinLimitExceeded
        | QueryError::NestedLoopPairLimitExceeded { .. }
        | QueryError::SortLimitExceeded
        | QueryError::MemoryLimitExceeded { .. } => ErrorClass::LimitExceeded,
        QueryError::TableNotFound(_)
        | QueryError::ColumnNotFound { .. }
        | QueryError::TypeError(_)
        | QueryError::IndexError(_)
        | QueryError::ViewError(_) => ErrorClass::Execution,
        // Unique violations surface from storage as io::Error text; classify
        // them by prefix like the Execution arm so the wire class matches
        // docs/errors.md instead of collapsing to Internal.
        QueryError::StorageError(err) => {
            if err.to_string().contains("unique constraint violation") {
                ErrorClass::ConstraintViolation
            } else {
                ErrorClass::Internal
            }
        }
        QueryError::Execution(msg) => {
            if msg.starts_with("unique constraint violation") {
                ErrorClass::ConstraintViolation
            } else if msg.starts_with("result too large") {
                ErrorClass::LimitExceeded
            } else {
                ErrorClass::Execution
            }
        }
    }
}

/// Sanitize an error message before sending it to the client.
/// Known safe errors are passed through; everything else is replaced
/// with a generic message to avoid leaking internal details.
fn sanitize_error(e: &str) -> String {
    let lower = e.to_lowercase();
    for prefix in SAFE_ERROR_PREFIXES {
        if lower.starts_with(prefix) {
            return e.to_string();
        }
    }
    "query execution error".into()
}

/// Write a message to the client with a timeout. Returns false if the
/// write failed or timed out (caller should close the connection).
async fn write_msg<W: AsyncWrite + Unpin>(writer: &mut BufWriter<W>, msg: &Message) -> bool {
    let write_fut = async {
        if msg.write_to(writer).await.is_err() {
            return false;
        }
        writer.flush().await.is_ok()
    };
    tokio::time::timeout(WRITE_TIMEOUT, write_fut)
        .await
        .unwrap_or_default()
}

/// Options for a single connection, bundled to keep `handle_connection`'s
/// argument list short.
pub struct ConnOpts<'a> {
    pub engine: Arc<RwLock<Engine>>,
    pub tx_gate: TxGate,
    /// Expected client password. Wrapped in `Zeroizing` so the secret is wiped
    /// from memory on drop (defends against leaking via a core dump).
    pub expected_password: Option<Zeroizing<String>>,
    /// Multi-user store loaded from the data dir at startup. When it has users,
    /// the handshake authenticates `(username, password)` against it; when empty
    /// the server falls back to `expected_password`. Shared across connections.
    pub users: Arc<UserStore>,
    pub shutdown_rx: &'a mut watch::Receiver<bool>,
    pub idle_timeout: Duration,
    pub query_timeout: Duration,
    pub rate_limiter: Option<&'a AuthRateLimiter>,
    pub peer_addr: Option<std::net::SocketAddr>,
    /// Shared server metrics. Always present; tests pass `Arc::new(Metrics::new())`.
    pub metrics: Arc<Metrics>,
    /// How long an explicit `begin` waits to acquire the transaction gate while
    /// another connection holds an open explicit transaction, before giving up
    /// with a clear timeout error instead of queueing indefinitely.
    pub tx_wait_timeout: Duration,
    /// When `Some`, the single database name this server serves. A CONNECT that
    /// explicitly names a *different* database is rejected at connect time.
    /// `None` accepts any name (0.9.x behavior) and only warns.
    pub db_name: Option<String>,
}

/// Execute a query against the engine under the RwLock. Read-only
/// statements acquire `.read()` so concurrent SELECTs can scan in
/// parallel; mutations acquire `.write()`.
///
/// When `principal` is `Some`, the user's role is enforced first: a role
/// without the `Write` permission (i.e. `readonly`) gets a clean
/// "permission denied" error for any non-read statement, before any lock
/// is taken or any engine state is touched.
/// A statement's execution result plus its (not-yet-waited) WAL durability
/// obligation. The ticket is settled by [`finalize_durability`] AFTER the
/// TxGate permit is dropped, so overlapping committers on other connections
/// can share a single fsync (group commit).
type DispatchOutcome = (Result<QueryResult, QueryError>, Option<WalDurabilityTicket>);

/// Execute a mutating statement under the engine write lock with WAL group
/// commit: the commit's fsync obligation is registered (not performed) while
/// the lock is held, then the lock is dropped and the un-waited ticket is
/// returned. The caller must settle the ticket (via [`finalize_durability`])
/// before acknowledging the statement — and must do so with the TxGate
/// permit already released, or committers can never overlap.
fn execute_write_deferred(
    engine: &Arc<RwLock<Engine>>,
    f: impl FnOnce(&mut Engine) -> Result<QueryResult, QueryError>,
) -> DispatchOutcome {
    let mut eng = match engine.write() {
        Ok(eng) => eng,
        Err(e) => {
            return (
                Err(QueryError::Execution(format!("lock poisoned: {e}"))),
                None,
            )
        }
    };
    eng.run_with_deferred_durability(f)
}

fn dispatch_query(
    engine: &Arc<RwLock<Engine>>,
    query: &str,
    principal: Option<&Principal>,
    allow_readonly_escalation: bool,
) -> DispatchOutcome {
    let stmt_result = parser::parse(query).map_err(|e| e.to_string());
    dispatch_query_parsed(
        engine,
        query,
        &stmt_result,
        principal,
        allow_readonly_escalation,
    )
}

fn dispatch_query_parsed(
    engine: &Arc<RwLock<Engine>>,
    query: &str,
    stmt_result: &Result<powdb_query::ast::Statement, String>,
    principal: Option<&Principal>,
    allow_readonly_escalation: bool,
) -> DispatchOutcome {
    // Role enforcement happens on the parsed AST. Statements that fail to
    // parse fall through — the engine returns the parse error itself and
    // can never execute anything for them.
    if let Ok(stmt) = &stmt_result {
        if let Err(e) = check_statement_permitted(principal, stmt) {
            return (Err(e), None);
        }
    }

    let can_try_read = matches!(&stmt_result, Ok(s) if is_read_only_statement(s));
    if can_try_read {
        let res = {
            let eng = match engine.read() {
                Ok(eng) => eng,
                Err(e) => {
                    return (
                        Err(QueryError::Execution(format!("lock poisoned: {e}"))),
                        None,
                    )
                }
            };
            eng.execute_powql_readonly(query)
        };
        match res {
            Ok(r) => return (Ok(r), None),
            Err(QueryError::ReadonlyNeedsWrite) => {
                if !allow_readonly_escalation {
                    return (Err(QueryError::ReadonlyNeedsWrite), None);
                }
                // The caller already owns writer admission, so it is safe to
                // fall through and retry under the engine write lock.
            }
            Err(e) => return (Err(e), None),
        }
    }

    if matches!(
        parsed_transaction_control(stmt_result),
        Some(TransactionControl::Rollback)
    ) {
        let mut eng = match engine.write() {
            Ok(eng) => eng,
            Err(e) => {
                return (
                    Err(QueryError::Execution(format!("lock poisoned: {e}"))),
                    None,
                )
            }
        };
        return (execute_rollback_preserving_sync_if_needed(&mut eng), None);
    }
    execute_write_deferred(engine, |eng| eng.execute_powql(query))
}

#[cfg(test)]
fn dispatch_sql_query(
    engine: &Arc<RwLock<Engine>>,
    query: &str,
    principal: Option<&Principal>,
    allow_readonly_escalation: bool,
) -> DispatchOutcome {
    let stmt_result = sql::parse_sql(query).map_err(|e| e.to_string());
    dispatch_sql_query_parsed(
        engine,
        query,
        &stmt_result,
        principal,
        allow_readonly_escalation,
    )
}

fn dispatch_sql_query_parsed(
    engine: &Arc<RwLock<Engine>>,
    query: &str,
    stmt_result: &Result<powdb_query::ast::Statement, String>,
    principal: Option<&Principal>,
    allow_readonly_escalation: bool,
) -> DispatchOutcome {
    if let Ok(stmt) = &stmt_result {
        if let Err(e) = check_statement_permitted(principal, stmt) {
            return (Err(e), None);
        }
    }

    let can_try_read = matches!(&stmt_result, Ok(s) if is_read_only_statement(s));
    if can_try_read {
        let res = {
            let eng = match engine.read() {
                Ok(eng) => eng,
                Err(e) => {
                    return (
                        Err(QueryError::Execution(format!("lock poisoned: {e}"))),
                        None,
                    )
                }
            };
            eng.execute_sql_readonly(query)
        };
        match res {
            Ok(r) => return (Ok(r), None),
            Err(QueryError::ReadonlyNeedsWrite) => {
                if !allow_readonly_escalation {
                    return (Err(QueryError::ReadonlyNeedsWrite), None);
                }
            }
            Err(e) => return (Err(e), None),
        }
    }

    if matches!(
        parsed_transaction_control(stmt_result),
        Some(TransactionControl::Rollback)
    ) {
        let mut eng = match engine.write() {
            Ok(eng) => eng,
            Err(e) => {
                return (
                    Err(QueryError::Execution(format!("lock poisoned: {e}"))),
                    None,
                )
            }
        };
        return (execute_rollback_preserving_sync_if_needed(&mut eng), None);
    }
    execute_write_deferred(engine, |eng| eng.execute_sql(query))
}

/// Convert a wire parameter into the query-crate [`ParamValue`] used for
/// token-level binding.
fn wire_param_to_value(p: &WireParam) -> powdb_query::ast::ParamValue {
    use powdb_query::ast::ParamValue;
    match p {
        WireParam::Null => ParamValue::Null,
        WireParam::Int(v) => ParamValue::Int(*v),
        WireParam::Float(v) => ParamValue::Float(*v),
        WireParam::Bool(v) => ParamValue::Bool(*v),
        WireParam::Str(s) => ParamValue::Str(s.clone()),
    }
}

/// Parameterized counterpart of [`dispatch_query`]. Routes through the exact
/// same role-enforcement and read/write escalation logic, but binds the
/// `$N` placeholders at the token level via the query crate's
/// `parse_with_params` path. A string parameter can never change the query's
/// shape — it is substituted as a literal token, not interpolated text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransactionControl {
    Begin,
    Commit,
    Rollback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionMode {
    Reader,
    Writer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WireResultMode {
    LegacyText,
    Native,
}

fn statement_admission(stmt: &powdb_query::ast::Statement) -> AdmissionMode {
    if is_read_only_statement(stmt) {
        AdmissionMode::Reader
    } else {
        AdmissionMode::Writer
    }
}

/// The terminal error a read-only (snapshot-serving) server returns for a
/// mutating statement, transaction-control frame, or a read that needs a writer
/// (a stale materialized view). The connection stays usable afterward.
fn readonly_terminal_message() -> Message {
    error_response(
        sanitize_error(&QueryError::ReadonlyMode.to_string()),
        ErrorClass::ReadonlyRefused,
    )
}

#[cfg(test)]
fn classify_query_admission(query: &str) -> AdmissionMode {
    parser::parse(query)
        .map(|stmt| statement_admission(&stmt))
        .unwrap_or(AdmissionMode::Writer)
}

#[cfg(test)]
fn classify_sql_admission(query: &str) -> AdmissionMode {
    sql::parse_sql(query)
        .map(|stmt| statement_admission(&stmt))
        .unwrap_or(AdmissionMode::Writer)
}

#[cfg(test)]
fn classify_params_admission(query: &str, params: &[WireParam]) -> AdmissionMode {
    let bound: Vec<powdb_query::ast::ParamValue> = params.iter().map(wire_param_to_value).collect();
    parser::parse_with_params(query, &bound)
        .map(|stmt| statement_admission(&stmt))
        .unwrap_or(AdmissionMode::Writer)
}

fn transaction_control(stmt: &powdb_query::ast::Statement) -> Option<TransactionControl> {
    use powdb_query::ast::Statement;
    match stmt {
        Statement::Begin => Some(TransactionControl::Begin),
        Statement::Commit => Some(TransactionControl::Commit),
        Statement::Rollback => Some(TransactionControl::Rollback),
        _ => None,
    }
}

fn parsed_transaction_control(
    stmt_result: &Result<powdb_query::ast::Statement, String>,
) -> Option<TransactionControl> {
    stmt_result.as_ref().ok().and_then(transaction_control)
}

fn execute_rollback_preserving_sync_if_needed(
    engine: &mut Engine,
) -> Result<QueryResult, QueryError> {
    engine.rollback_transaction_preserving_wal_archive()
}

#[cfg(test)]
fn dispatch_query_with_params(
    engine: &Arc<RwLock<Engine>>,
    query: &str,
    params: &[WireParam],
    principal: Option<&Principal>,
    allow_readonly_escalation: bool,
) -> DispatchOutcome {
    let bound: Vec<powdb_query::ast::ParamValue> = params.iter().map(wire_param_to_value).collect();

    // Parse once (with params bound) so role enforcement and read/write
    // classification see exactly the statement that will execute.
    let stmt_result = parser::parse_with_params(query, &bound).map_err(|e| e.to_string());
    dispatch_query_with_bound_params_parsed(
        engine,
        query,
        &bound,
        &stmt_result,
        principal,
        allow_readonly_escalation,
    )
}

fn dispatch_query_with_bound_params_parsed(
    engine: &Arc<RwLock<Engine>>,
    query: &str,
    bound: &[powdb_query::ast::ParamValue],
    stmt_result: &Result<powdb_query::ast::Statement, String>,
    principal: Option<&Principal>,
    allow_readonly_escalation: bool,
) -> DispatchOutcome {
    if let Ok(stmt) = &stmt_result {
        if let Err(e) = check_statement_permitted(principal, stmt) {
            return (Err(e), None);
        }
    }

    let can_try_read = matches!(&stmt_result, Ok(s) if is_read_only_statement(s));
    if can_try_read {
        let res = {
            let eng = match engine.read() {
                Ok(eng) => eng,
                Err(e) => {
                    return (
                        Err(QueryError::Execution(format!("lock poisoned: {e}"))),
                        None,
                    )
                }
            };
            eng.execute_powql_readonly_with_params(query, bound)
        };
        match res {
            Ok(r) => return (Ok(r), None),
            Err(QueryError::ReadonlyNeedsWrite) => {
                if !allow_readonly_escalation {
                    return (Err(QueryError::ReadonlyNeedsWrite), None);
                }
            }
            Err(e) => return (Err(e), None),
        }
    }

    if matches!(
        parsed_transaction_control(stmt_result),
        Some(TransactionControl::Rollback)
    ) {
        let mut eng = match engine.write() {
            Ok(eng) => eng,
            Err(e) => {
                return (
                    Err(QueryError::Execution(format!("lock poisoned: {e}"))),
                    None,
                )
            }
        };
        return (execute_rollback_preserving_sync_if_needed(&mut eng), None);
    }
    execute_write_deferred(engine, |eng| eng.execute_powql_with_params(query, bound))
}

#[derive(Debug, Clone)]
struct SyncPullRequest {
    replica_id: String,
    since_lsn: u64,
    max_units: u32,
    max_bytes: u64,
    database_id: [u8; 16],
    primary_generation: u64,
    wal_format_version: u16,
    catalog_version: u16,
    segment_format_version: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncErrorClass {
    AuthRequired,
    PermissionDenied,
    InvalidReplicaId,
    ActiveTransaction,
    QueryExecution,
    InvalidMaxUnits,
    InvalidMaxBytes,
    SyncContext,
    StatusRead,
    CursorLsnMismatch,
    IdentityRead,
    IdentityOrFormatMismatch,
    RetainedRead,
    RetainedUnitEncoding,
    RetainedChunkNotApplyable,
    LsnAheadOfRemote,
    AckValidation,
    AckUpdate,
    Internal,
}

impl SyncErrorClass {
    const fn as_label(self) -> &'static str {
        match self {
            Self::AuthRequired => "auth_required",
            Self::PermissionDenied => "permission_denied",
            Self::InvalidReplicaId => "invalid_replica_id",
            Self::ActiveTransaction => "active_transaction",
            Self::QueryExecution => "query_execution",
            Self::InvalidMaxUnits => "invalid_max_units",
            Self::InvalidMaxBytes => "invalid_max_bytes",
            Self::SyncContext => "sync_context",
            Self::StatusRead => "status_read",
            Self::CursorLsnMismatch => "cursor_lsn_mismatch",
            Self::IdentityRead => "identity_read",
            Self::IdentityOrFormatMismatch => "identity_or_format_mismatch",
            Self::RetainedRead => "retained_read",
            Self::RetainedUnitEncoding => "retained_unit_encoding",
            Self::RetainedChunkNotApplyable => "retained_chunk_not_applyable",
            Self::LsnAheadOfRemote => "lsn_ahead_of_remote",
            Self::AckValidation => "ack_validation",
            Self::AckUpdate => "ack_update",
            Self::Internal => "internal",
        }
    }
}

#[derive(Debug, Clone)]
struct SyncDecision {
    message: Message,
    error_class: Option<SyncErrorClass>,
}

impl SyncDecision {
    fn ok(message: Message) -> Self {
        Self {
            message,
            error_class: None,
        }
    }

    fn error(class: SyncErrorClass, message: impl Into<String>) -> Self {
        Self {
            message: Message::Error {
                message: message.into(),
            },
            error_class: Some(class),
        }
    }
}

fn check_sync_protocol_permitted(
    credential_authenticated: bool,
    principal: Option<&Principal>,
) -> Result<(), (SyncErrorClass, String)> {
    if !credential_authenticated {
        return Err((
            SyncErrorClass::AuthRequired,
            "sync protocol requires authentication".to_string(),
        ));
    }
    if let Some(principal) = principal {
        let allowed =
            Role::builtin(&principal.role).is_some_and(|role| role.allows(Permission::Write));
        if !allowed {
            return Err((
                SyncErrorClass::PermissionDenied,
                format!(
                    "permission denied: role '{}' cannot use sync protocol",
                    principal.role
                ),
            ));
        }
    }
    Ok(())
}

fn validate_wire_replica_id(replica_id: &str) -> Result<(), String> {
    if replica_id.is_empty() {
        return Err("replica id must be non-empty".to_string());
    }
    if replica_id.len() > 128 {
        return Err("replica id must be at most 128 bytes".to_string());
    }
    if !replica_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    {
        return Err("replica id contains unsupported characters".to_string());
    }
    Ok(())
}

fn sync_context(engine: &Arc<RwLock<Engine>>) -> Result<SyncContext, String> {
    let engine = engine
        .read()
        .map_err(|e| format!("lock poisoned while reading sync context: {e}"))?;
    let catalog = engine.catalog();
    Ok(SyncContext {
        data_dir: catalog.data_dir().to_path_buf(),
        remote_lsn: catalog.max_lsn(),
        active_catalog_version: catalog.active_catalog_version(),
    })
}

/// Snapshot of the primary's sync-relevant state captured under the engine lock.
/// `active_catalog_version` is the database's *active* on-disk catalog format,
/// used to stamp the expected segment identity a pulling replica is checked
/// against (a database that never activated v6 keeps expecting v5).
struct SyncContext {
    data_dir: PathBuf,
    remote_lsn: u64,
    active_catalog_version: u16,
}

fn wire_repair_action(action: SyncRepairAction) -> WireSyncRepairAction {
    match action {
        SyncRepairAction::None => WireSyncRepairAction::None,
        SyncRepairAction::Pull => WireSyncRepairAction::Pull,
        SyncRepairAction::AwaitArchive => WireSyncRepairAction::AwaitArchive,
        SyncRepairAction::Rebootstrap => WireSyncRepairAction::Rebootstrap,
    }
}

fn wire_sync_status(status: ReplicaSyncStatus) -> WireSyncStatus {
    WireSyncStatus {
        replica_id: status.replica_id,
        active: status.active,
        last_applied_lsn: status.last_applied_lsn,
        remote_lsn: status.remote_lsn,
        servable_lsn: status.servable_lsn,
        unarchived_lsn: status.unarchived_lsn,
        lag_lsn: status.lag_lsn,
        lag_bytes: status.lag_bytes,
        lag_ms: status.lag_ms,
        stale: status.stale,
        repair_action: wire_repair_action(status.repair_action),
        last_sync_error: status.last_sync_error,
    }
}

fn wire_retained_unit(unit: RetainedUnit) -> WireRetainedUnit {
    WireRetainedUnit {
        tx_id: unit.tx_id,
        record_type: unit.record_type,
        lsn: unit.lsn,
        data: unit.data,
    }
}

fn sync_operation_outcome(message: &Message) -> SyncOutcome {
    match message {
        Message::SyncStatusResult { .. }
        | Message::SyncPullResult { .. }
        | Message::SyncAckResult { .. } => SyncOutcome::Ok,
        _ => SyncOutcome::Error,
    }
}

fn sync_operation_label(operation: SyncOperation) -> &'static str {
    match operation {
        SyncOperation::Status => "status",
        SyncOperation::Pull => "pull",
        SyncOperation::Ack => "ack",
    }
}

fn sync_pull_payload_bytes(units: &[WireRetainedUnit]) -> u64 {
    units.iter().fold(0, |total, unit| {
        total.saturating_add(unit.encoded_len().unwrap_or(0))
    })
}

fn sync_repair_label(action: WireSyncRepairAction) -> SyncRepairLabel {
    match action {
        WireSyncRepairAction::None => SyncRepairLabel::None,
        WireSyncRepairAction::Pull => SyncRepairLabel::Pull,
        WireSyncRepairAction::AwaitArchive => SyncRepairLabel::AwaitArchive,
        WireSyncRepairAction::Rebootstrap => SyncRepairLabel::Rebootstrap,
    }
}

fn wire_repair_action_label(action: WireSyncRepairAction) -> &'static str {
    match action {
        WireSyncRepairAction::None => "none",
        WireSyncRepairAction::Pull => "pull",
        WireSyncRepairAction::AwaitArchive => "await_archive",
        WireSyncRepairAction::Rebootstrap => "rebootstrap",
    }
}

const FNV1A64_OFFSET: u64 = 0xcbf29ce484222325;
const FNV1A64_PRIME: u64 = 0x100000001b3;
const INVALID_REPLICA_FINGERPRINT: &str = "invalid";

fn replica_fingerprint(replica_id: &str) -> String {
    let mut hash = FNV1A64_OFFSET;
    for byte in replica_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV1A64_PRIME);
    }
    format!("{hash:016x}")
}

fn log_replica_fingerprint(replica_id: &str) -> String {
    if validate_wire_replica_id(replica_id).is_ok() {
        replica_fingerprint(replica_id)
    } else {
        INVALID_REPLICA_FINGERPRINT.to_string()
    }
}

#[derive(Debug, Clone)]
struct SyncLogContext {
    replica_fingerprint: String,
    since_lsn: Option<u64>,
    applied_lsn: Option<u64>,
    observed_remote_lsn: Option<u64>,
    max_units: Option<u32>,
    max_bytes: Option<u64>,
}

impl SyncLogContext {
    fn status(replica_id: &str) -> Self {
        Self::base(replica_id)
    }

    fn pull(request: &SyncPullRequest) -> Self {
        Self {
            replica_fingerprint: log_replica_fingerprint(&request.replica_id),
            since_lsn: Some(request.since_lsn),
            applied_lsn: None,
            observed_remote_lsn: None,
            max_units: Some(request.max_units),
            max_bytes: Some(request.max_bytes),
        }
    }

    fn ack(replica_id: &str, applied_lsn: u64, observed_remote_lsn: u64) -> Self {
        Self {
            replica_fingerprint: log_replica_fingerprint(replica_id),
            since_lsn: None,
            applied_lsn: Some(applied_lsn),
            observed_remote_lsn: Some(observed_remote_lsn),
            max_units: None,
            max_bytes: None,
        }
    }

    fn base(replica_id: &str) -> Self {
        Self {
            replica_fingerprint: log_replica_fingerprint(replica_id),
            since_lsn: None,
            applied_lsn: None,
            observed_remote_lsn: None,
            max_units: None,
            max_bytes: None,
        }
    }
}

struct SyncExecutionContext<'a> {
    tx_gate: TxGate,
    connection_has_transaction: bool,
    operation: SyncOperation,
    log_context: SyncLogContext,
    metrics: &'a Arc<Metrics>,
    query_timeout: Duration,
}

fn log_sync_decision(
    operation: SyncOperation,
    context: &SyncLogContext,
    elapsed: Duration,
    decision: &SyncDecision,
) {
    let operation = sync_operation_label(operation);
    let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
    match &decision.message {
        Message::SyncStatusResult { status } => {
            let repair_action = wire_repair_action_label(status.repair_action);
            if status.stale || status.repair_action != WireSyncRepairAction::None {
                info!(
                    operation = operation,
                    replica_fingerprint = %context.replica_fingerprint,
                    remote_lsn = status.remote_lsn,
                    last_applied_lsn = ?status.last_applied_lsn,
                    servable_lsn = ?status.servable_lsn,
                    unarchived_lsn = ?status.unarchived_lsn,
                    lag_lsn = ?status.lag_lsn,
                    lag_bytes = ?status.lag_bytes,
                    lag_ms = ?status.lag_ms,
                    stale = status.stale,
                    repair_action,
                    elapsed_ms,
                    "sync decision"
                );
            } else {
                debug!(
                    operation = operation,
                    replica_fingerprint = %context.replica_fingerprint,
                    remote_lsn = status.remote_lsn,
                    last_applied_lsn = ?status.last_applied_lsn,
                    repair_action,
                    elapsed_ms,
                    "sync decision"
                );
            }
        }
        Message::SyncPullResult {
            status,
            units,
            has_more,
        } => {
            let repair_action = wire_repair_action_label(status.repair_action);
            let units_len = units.len();
            let payload_bytes = sync_pull_payload_bytes(units);
            if *has_more || status.stale || status.repair_action != WireSyncRepairAction::None {
                info!(
                    operation = operation,
                    replica_fingerprint = %context.replica_fingerprint,
                    since_lsn = ?context.since_lsn,
                    max_units = ?context.max_units,
                    max_bytes = ?context.max_bytes,
                    units = units_len,
                    payload_bytes,
                    has_more = *has_more,
                    remote_lsn = status.remote_lsn,
                    last_applied_lsn = ?status.last_applied_lsn,
                    servable_lsn = ?status.servable_lsn,
                    unarchived_lsn = ?status.unarchived_lsn,
                    lag_lsn = ?status.lag_lsn,
                    stale = status.stale,
                    repair_action,
                    elapsed_ms,
                    "sync decision"
                );
            } else {
                debug!(
                    operation = operation,
                    replica_fingerprint = %context.replica_fingerprint,
                    since_lsn = ?context.since_lsn,
                    units = units_len,
                    payload_bytes,
                    has_more = *has_more,
                    remote_lsn = status.remote_lsn,
                    repair_action,
                    elapsed_ms,
                    "sync decision"
                );
            }
        }
        Message::SyncAckResult {
            previous_applied_lsn,
            applied_lsn,
            remote_lsn,
            advanced,
            status,
        } => {
            let repair_action = wire_repair_action_label(status.repair_action);
            if *advanced || status.stale || status.repair_action != WireSyncRepairAction::None {
                info!(
                    operation = operation,
                    replica_fingerprint = %context.replica_fingerprint,
                    requested_applied_lsn = ?context.applied_lsn,
                    observed_remote_lsn = ?context.observed_remote_lsn,
                    previous_applied_lsn = *previous_applied_lsn,
                    applied_lsn = *applied_lsn,
                    remote_lsn = *remote_lsn,
                    advanced = *advanced,
                    stale = status.stale,
                    repair_action,
                    elapsed_ms,
                    "sync decision"
                );
            } else {
                debug!(
                    operation = operation,
                    replica_fingerprint = %context.replica_fingerprint,
                    requested_applied_lsn = ?context.applied_lsn,
                    previous_applied_lsn = *previous_applied_lsn,
                    applied_lsn = *applied_lsn,
                    remote_lsn = *remote_lsn,
                    advanced = *advanced,
                    repair_action,
                    elapsed_ms,
                    "sync decision"
                );
            }
        }
        Message::Error { .. } => {
            warn!(
                operation = operation,
                replica_fingerprint = %context.replica_fingerprint,
                since_lsn = ?context.since_lsn,
                applied_lsn = ?context.applied_lsn,
                observed_remote_lsn = ?context.observed_remote_lsn,
                max_units = ?context.max_units,
                max_bytes = ?context.max_bytes,
                error_class = decision
                    .error_class
                    .unwrap_or(SyncErrorClass::Internal)
                    .as_label(),
                elapsed_ms,
                "sync decision rejected"
            );
        }
        _ => {
            debug!(
                operation = operation,
                replica_fingerprint = %context.replica_fingerprint,
                elapsed_ms,
                "unexpected sync decision response"
            );
        }
    }
}

fn trim_to_applyable_v1_prefix(
    raw_units: &mut Vec<RetainedUnit>,
    wire_units: &mut Vec<WireRetainedUnit>,
) -> Result<(), String> {
    let mut last_error = None;
    while !raw_units.is_empty() {
        match validate_v1_retained_units_applyable(raw_units) {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_error = Some(err.to_string());
                raw_units.pop();
                wire_units.pop();
            }
        }
    }
    if let Some(error) = last_error {
        return Err(format!(
            "sync pull cannot serve an applyable V1 retained chunk with current limits: {error}"
        ));
    }
    Ok(())
}

fn validate_sync_ack_applyable_boundary(
    data_dir: &Path,
    replica_id: &str,
    applied_lsn: u64,
    remote_lsn: u64,
) -> Result<(), String> {
    let status =
        replica_sync_status(data_dir, replica_id, remote_lsn).map_err(|err| err.to_string())?;
    let Some(previous_lsn) = status.last_applied_lsn else {
        return Ok(());
    };
    if applied_lsn <= previous_lsn {
        return Ok(());
    }
    let range_len = applied_lsn - previous_lsn;
    if range_len > u64::from(MAX_SYNC_PULL_UNITS) {
        return Err(format!(
            "sync ack range contains {range_len} units; acknowledge ranges no larger than {MAX_SYNC_PULL_UNITS}"
        ));
    }
    let max_units =
        usize::try_from(range_len).map_err(|_| "sync ack range is too large to validate")?;
    let identity = read_identity(data_dir).map_err(|err| err.to_string())?;
    let segment_dir = retained_segments_dir(data_dir);
    let units = read_units_through(
        &segment_dir,
        identity.segment_identity(),
        previous_lsn,
        applied_lsn,
        max_units,
    )
    .map_err(|err| err.to_string())?;
    if units.len() != max_units || units.last().map(|unit| unit.lsn) != Some(applied_lsn) {
        return Err(
            "sync ack does not cover a complete retained-unit range; rebootstrap required".into(),
        );
    }
    validate_v1_retained_units_applyable(&units).map_err(|err| err.to_string())
}

#[cfg(test)]
fn dispatch_sync_status(
    engine: &Arc<RwLock<Engine>>,
    replica_id: String,
    credential_authenticated: bool,
    principal: Option<&Principal>,
) -> Message {
    dispatch_sync_status_decision(engine, replica_id, credential_authenticated, principal).message
}

fn dispatch_sync_status_decision(
    engine: &Arc<RwLock<Engine>>,
    replica_id: String,
    credential_authenticated: bool,
    principal: Option<&Principal>,
) -> SyncDecision {
    if let Err((class, message)) =
        check_sync_protocol_permitted(credential_authenticated, principal)
    {
        return SyncDecision::error(class, message);
    }
    if let Err(message) = validate_wire_replica_id(&replica_id) {
        return SyncDecision::error(SyncErrorClass::InvalidReplicaId, message);
    }
    let SyncContext {
        data_dir,
        remote_lsn,
        ..
    } = match sync_context(engine) {
        Ok(context) => context,
        Err(message) => return SyncDecision::error(SyncErrorClass::SyncContext, message),
    };
    match replica_sync_status(&data_dir, &replica_id, remote_lsn) {
        Ok(status) => SyncDecision::ok(Message::SyncStatusResult {
            status: wire_sync_status(status),
        }),
        Err(err) => SyncDecision::error(SyncErrorClass::StatusRead, err.to_string()),
    }
}

#[cfg(test)]
fn dispatch_sync_pull(
    engine: &Arc<RwLock<Engine>>,
    request: SyncPullRequest,
    credential_authenticated: bool,
    principal: Option<&Principal>,
) -> Message {
    dispatch_sync_pull_decision(engine, request, credential_authenticated, principal).message
}

fn dispatch_sync_pull_decision(
    engine: &Arc<RwLock<Engine>>,
    request: SyncPullRequest,
    credential_authenticated: bool,
    principal: Option<&Principal>,
) -> SyncDecision {
    if let Err((class, message)) =
        check_sync_protocol_permitted(credential_authenticated, principal)
    {
        return SyncDecision::error(class, message);
    }
    if let Err(message) = validate_wire_replica_id(&request.replica_id) {
        return SyncDecision::error(SyncErrorClass::InvalidReplicaId, message);
    }
    if request.max_units == 0 || request.max_units > MAX_SYNC_PULL_UNITS {
        return SyncDecision::error(
            SyncErrorClass::InvalidMaxUnits,
            format!("sync pull maxUnits must be between 1 and {MAX_SYNC_PULL_UNITS}"),
        );
    }
    if request.max_bytes == 0 || request.max_bytes > MAX_SYNC_PULL_BYTES {
        return SyncDecision::error(
            SyncErrorClass::InvalidMaxBytes,
            format!("sync pull maxBytes must be between 1 and {MAX_SYNC_PULL_BYTES}"),
        );
    }

    let SyncContext {
        data_dir,
        remote_lsn,
        active_catalog_version,
    } = match sync_context(engine) {
        Ok(context) => context,
        Err(message) => return SyncDecision::error(SyncErrorClass::SyncContext, message),
    };
    let status = match replica_sync_status(&data_dir, &request.replica_id, remote_lsn) {
        Ok(status) => status,
        Err(err) => {
            return SyncDecision::error(SyncErrorClass::StatusRead, err.to_string());
        }
    };
    let Some(cursor_lsn) = status.last_applied_lsn else {
        return SyncDecision::ok(Message::SyncPullResult {
            status: wire_sync_status(status),
            units: Vec::new(),
            has_more: false,
        });
    };
    if status.repair_action != SyncRepairAction::Pull {
        return SyncDecision::ok(Message::SyncPullResult {
            status: wire_sync_status(status),
            units: Vec::new(),
            has_more: false,
        });
    }
    if request.since_lsn != cursor_lsn {
        return SyncDecision::error(
            SyncErrorClass::CursorLsnMismatch,
            format!(
                "sync pull sinceLsn {} does not match primary cursor LSN {cursor_lsn}",
                request.since_lsn
            ),
        );
    }

    let identity = match read_identity(&data_dir) {
        Ok(identity) => identity,
        Err(err) => {
            return SyncDecision::error(SyncErrorClass::IdentityRead, err.to_string());
        }
    };
    // Stamp the expected identity with the database's *active* catalog version,
    // not this binary's compile-time maximum, so a database that never activated
    // v6 still expects v5 and accepts a v0.12 replica.
    let expected = SegmentIdentity::with_catalog_version(
        identity.database_id,
        identity.primary_generation,
        active_catalog_version,
    );
    if request.database_id != expected.database_id
        || request.primary_generation != expected.primary_generation
        || request.wal_format_version != expected.wal_format_version
        || request.segment_format_version != RETAINED_SEGMENT_FORMAT_VERSION
    {
        return SyncDecision::error(
            SyncErrorClass::IdentityOrFormatMismatch,
            "sync pull identity or format version mismatch; rebootstrap required",
        );
    }
    // The request states the maximum catalog format the replica can read. Accept
    // any replica that can read the active format (>= active); reject a replica
    // whose maximum is older than the data it would receive.
    if request.catalog_version < expected.catalog_version {
        return SyncDecision::error(
            SyncErrorClass::IdentityOrFormatMismatch,
            format!(
                "sync pull replica catalog format v{} cannot read this database's active catalog format v{}; rebootstrap with an upgraded replica required",
                request.catalog_version, expected.catalog_version
            ),
        );
    }

    let effective_max_units = request.max_units.min(MAX_SYNC_PULL_UNITS) as usize;
    let requested_through_lsn = request
        .since_lsn
        .saturating_add(request.max_units as u64)
        .min(remote_lsn)
        .min(status.servable_lsn.unwrap_or(request.since_lsn));
    let segment_dir = retained_segments_dir(&data_dir);
    if requested_through_lsn > request.since_lsn {
        if let Err(err) = validate_retained_tail_available(
            &segment_dir,
            expected,
            request.since_lsn,
            requested_through_lsn,
        ) {
            let mut rebootstrap_status = status;
            rebootstrap_status.stale = true;
            rebootstrap_status.repair_action = SyncRepairAction::Rebootstrap;
            rebootstrap_status.last_sync_error = Some(format!(
                "retained history is unavailable; rebootstrap required: {err}"
            ));
            return SyncDecision::ok(Message::SyncPullResult {
                status: wire_sync_status(rebootstrap_status),
                units: Vec::new(),
                has_more: false,
            });
        }
    }

    let raw_units = match read_units_through(
        &segment_dir,
        expected,
        request.since_lsn,
        requested_through_lsn,
        effective_max_units,
    ) {
        Ok(units) => units,
        Err(err) => {
            return SyncDecision::error(SyncErrorClass::RetainedRead, err.to_string());
        }
    };

    let mut selected_raw = Vec::new();
    let mut selected = Vec::new();
    let mut selected_bytes = 0u64;
    for unit in raw_units {
        let wire_unit = wire_retained_unit(unit.clone());
        let unit_bytes = match wire_unit.encoded_len() {
            Ok(bytes) => bytes,
            Err(message) => {
                return SyncDecision::error(SyncErrorClass::RetainedUnitEncoding, message);
            }
        };
        if selected_bytes.saturating_add(unit_bytes) > request.max_bytes {
            if selected.is_empty() {
                return SyncDecision::error(
                    SyncErrorClass::InvalidMaxBytes,
                    "sync pull maxBytes is too small for the next retained unit",
                );
            }
            break;
        }
        selected_bytes += unit_bytes;
        selected_raw.push(unit);
        selected.push(wire_unit);
    }
    if let Err(message) = trim_to_applyable_v1_prefix(&mut selected_raw, &mut selected) {
        return SyncDecision::error(SyncErrorClass::RetainedChunkNotApplyable, message);
    }

    let fetchable_through_lsn = status.servable_lsn.unwrap_or(remote_lsn).min(remote_lsn);
    let has_more = selected
        .last()
        .is_some_and(|unit| unit.lsn < fetchable_through_lsn);
    SyncDecision::ok(Message::SyncPullResult {
        status: wire_sync_status(status),
        units: selected,
        has_more,
    })
}

#[cfg(test)]
fn dispatch_sync_ack(
    engine: &Arc<RwLock<Engine>>,
    replica_id: String,
    applied_lsn: u64,
    observed_remote_lsn: u64,
    credential_authenticated: bool,
    principal: Option<&Principal>,
) -> Message {
    dispatch_sync_ack_decision(
        engine,
        replica_id,
        applied_lsn,
        observed_remote_lsn,
        credential_authenticated,
        principal,
    )
    .message
}

fn dispatch_sync_ack_decision(
    engine: &Arc<RwLock<Engine>>,
    replica_id: String,
    applied_lsn: u64,
    observed_remote_lsn: u64,
    credential_authenticated: bool,
    principal: Option<&Principal>,
) -> SyncDecision {
    if let Err((class, message)) =
        check_sync_protocol_permitted(credential_authenticated, principal)
    {
        return SyncDecision::error(class, message);
    }
    if let Err(message) = validate_wire_replica_id(&replica_id) {
        return SyncDecision::error(SyncErrorClass::InvalidReplicaId, message);
    }
    if applied_lsn > observed_remote_lsn {
        return SyncDecision::error(
            SyncErrorClass::LsnAheadOfRemote,
            format!(
                "sync ack appliedLsn {applied_lsn} is ahead of observed remoteLsn {observed_remote_lsn}"
            ),
        );
    }
    let SyncContext {
        data_dir,
        remote_lsn,
        ..
    } = match sync_context(engine) {
        Ok(context) => context,
        Err(message) => return SyncDecision::error(SyncErrorClass::SyncContext, message),
    };
    if observed_remote_lsn > remote_lsn {
        return SyncDecision::error(
            SyncErrorClass::LsnAheadOfRemote,
            format!(
                "sync ack remoteLsn {observed_remote_lsn} is ahead of primary LSN {remote_lsn}"
            ),
        );
    }
    if let Err(message) =
        validate_sync_ack_applyable_boundary(&data_dir, &replica_id, applied_lsn, remote_lsn)
    {
        return SyncDecision::error(SyncErrorClass::AckValidation, message);
    }
    match acknowledge_replica_apply(&data_dir, &replica_id, applied_lsn, remote_lsn) {
        Ok(summary) => SyncDecision::ok(Message::SyncAckResult {
            previous_applied_lsn: summary.previous_applied_lsn,
            applied_lsn: summary.applied_lsn,
            remote_lsn: summary.remote_lsn,
            advanced: summary.advanced,
            status: wire_sync_status(summary.status),
        }),
        Err(err) => SyncDecision::error(SyncErrorClass::AckUpdate, err.to_string()),
    }
}

async fn run_blocking_sync<T, F>(input: T, query_timeout: Duration, f: F) -> SyncDecision
where
    T: Send + 'static,
    F: FnOnce(T) -> SyncDecision + Send + 'static,
{
    let mut handle = tokio::task::spawn_blocking(move || f(input));
    tokio::select! {
        result = &mut handle => match result {
            Ok(decision) => decision,
            Err(err) => SyncDecision::error(SyncErrorClass::Internal, format!("internal error: {err}")),
        },
        _ = tokio::time::sleep(query_timeout) => match handle.await {
            Ok(decision) => decision,
            Err(err) => SyncDecision::error(SyncErrorClass::Internal, format!("internal error: {err}")),
        },
    }
}

async fn execute_gated_sync<T, F>(context: SyncExecutionContext<'_>, input: T, f: F) -> Message
where
    T: Send + 'static,
    F: FnOnce(T) -> SyncDecision + Send + 'static,
{
    let SyncExecutionContext {
        tx_gate,
        connection_has_transaction,
        operation,
        log_context,
        metrics,
        query_timeout,
    } = context;
    let start = Instant::now();
    if connection_has_transaction {
        let decision = SyncDecision::error(
            SyncErrorClass::ActiveTransaction,
            "sync protocol is unavailable inside an active transaction",
        );
        let elapsed = start.elapsed();
        log_sync_decision(operation, &log_context, elapsed, &decision);
        metrics.record_sync_operation(operation, elapsed, SyncOutcome::Error);
        return decision.message;
    }

    let permit_count = tx_gate.permit_count();
    let permit = match tx_gate.acquire_many_owned(permit_count).await {
        Ok(permit) => permit,
        Err(_) => {
            let decision =
                SyncDecision::error(SyncErrorClass::QueryExecution, "query execution error");
            let elapsed = start.elapsed();
            log_sync_decision(operation, &log_context, elapsed, &decision);
            metrics.record_sync_operation(operation, elapsed, SyncOutcome::Error);
            return decision.message;
        }
    };
    let decision = run_blocking_sync(input, query_timeout, f).await;
    drop(permit);
    match &decision.message {
        Message::SyncStatusResult { status } => {
            metrics.record_sync_repair_action(operation, sync_repair_label(status.repair_action));
        }
        Message::SyncPullResult { status, units, .. } => {
            metrics.record_sync_repair_action(operation, sync_repair_label(status.repair_action));
            metrics.record_sync_pull_payload(units.len() as u64, sync_pull_payload_bytes(units));
        }
        Message::SyncAckResult {
            advanced, status, ..
        } => {
            metrics.record_sync_repair_action(operation, sync_repair_label(status.repair_action));
            if *advanced {
                metrics.inc_sync_ack_advanced();
            }
        }
        _ => {}
    }
    let elapsed = start.elapsed();
    log_sync_decision(operation, &log_context, elapsed, &decision);
    metrics.record_sync_operation(
        operation,
        elapsed,
        sync_operation_outcome(&decision.message),
    );
    decision.message
}

/// Acquire the TxGate for an explicit `begin`, bounded by `tx_wait_timeout`.
/// Overlapping explicit transactions queue behind the permit rather than being
/// rejected, but a connection gives up with a clear, client-facing error once
/// the wait elapses — so a transaction stalled (or held open) on another
/// connection can never block this one indefinitely. A timeout is recorded so
/// `powdb_tx_gate_timeouts_total` (and the error total) stay truthful.
async fn acquire_begin_permit(
    tx_gate: &TxGate,
    tx_wait_timeout: Duration,
    metrics: &Arc<Metrics>,
) -> Result<OwnedSemaphorePermit, Message> {
    match tokio::time::timeout(
        tx_wait_timeout,
        tx_gate.clone().acquire_many_owned(tx_gate.permit_count()),
    )
    .await
    {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err(error_response(
            "query execution error",
            ErrorClass::Internal,
        )),
        Err(_) => {
            metrics.inc_tx_gate_timeout();
            Err(error_response(
                format!(
                    "transaction gate timeout after {}ms waiting for concurrent transaction to complete",
                    tx_wait_timeout.as_millis()
                ),
                ErrorClass::Timeout,
            ))
        }
    }
}

/// Acquire the TxGate for a BARE autocommit statement, bounded by
/// `tx_wait_timeout` exactly like [`acquire_begin_permit`]. Autocommit writes
/// serialize through the same gate as explicit transactions, so a stalled (or
/// held-open) transaction on another connection would otherwise block this
/// write indefinitely. Bounding the acquire turns that indefinite wait into a
/// clear, client-facing timeout error and records the timeout so
/// `powdb_tx_gate_timeouts_total` (and the error total) stay truthful. This
/// only bounds the ACQUIRE; the permit is still dropped BEFORE the caller's
/// durability wait so overlapping committers can share an fsync.
async fn acquire_autocommit_permit(
    tx_gate: &TxGate,
    admission: AdmissionMode,
    tx_wait_timeout: Duration,
    metrics: &Arc<Metrics>,
) -> Result<OwnedSemaphorePermit, Message> {
    let permits = match admission {
        AdmissionMode::Reader => 1,
        AdmissionMode::Writer => tx_gate.permit_count(),
    };

    // The uncontended path is overwhelmingly common for autocommit work.
    // Avoid constructing and polling a timeout-wrapped semaphore future when
    // the permits are already available. If a reader or writer is queued, the
    // try-acquire fails and we fall back to Tokio's fair semaphore queue, so a
    // waiting writer still cannot be bypassed by later readers.
    if let Ok(permit) = tx_gate.clone().try_acquire_many_owned(permits) {
        return Ok(permit);
    }

    let acquire = tx_gate.clone().acquire_many_owned(permits);
    match tokio::time::timeout(tx_wait_timeout, acquire).await {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err(error_response(
            "query execution error",
            ErrorClass::Internal,
        )),
        Err(_) => {
            metrics.inc_tx_gate_timeout();
            Err(error_response(
                format!(
                    "transaction gate timeout after {}ms waiting for concurrent transaction to complete",
                    tx_wait_timeout.as_millis()
                ),
                ErrorClass::Timeout,
            ))
        }
    }
}

/// Run the shared four-arm transaction-routing state machine for one wire
/// query frame, returning the response plus its un-waited WAL durability
/// ticket. The TxGate permit is managed here and, crucially, is already
/// released (bare statements, commit/rollback) by the time this returns, so
/// the caller's `finalize_durability` wait happens OUTSIDE the gate and
/// overlapping committers can share an fsync.
///
/// The three wire dialects (PowQL, SQL, parameterized PowQL) differ only in
/// how a frame is parsed and dispatched; they share this routing, so a
/// behavior fix (e.g. cancellation rollback parity) lands here exactly once.
/// `parsed_query` is the dialect's already-parsed frame, `tx_control` its
/// transaction-control classification, `autocommit_admission` the admission
/// mode for a bare (non-transaction) statement, and `dispatch` the closure
/// that executes the parsed frame (its `bool` argument is
/// `allow_readonly_escalation`).
#[allow(clippy::too_many_arguments)]
async fn run_wire_query_state_machine<Inner, D, R>(
    engine: Arc<RwLock<Engine>>,
    tx_gate: TxGate,
    tx_permit: &mut Option<OwnedSemaphorePermit>,
    parsed_query: Arc<Inner>,
    tx_control: Option<TransactionControl>,
    autocommit_admission: AdmissionMode,
    result_mode: WireResultMode,
    principal: Option<Principal>,
    query_timeout: Duration,
    query_deadline: Instant,
    tx_wait_timeout: Duration,
    metrics: &Arc<Metrics>,
    reader: &mut BufReader<R>,
    wire_read_buffer: &mut Vec<u8>,
    pending_messages: &mut InFlightReadAhead,
    dispatch: D,
) -> (
    Message,
    Option<PendingDurability>,
    Option<ConnectionTermination>,
)
where
    Inner: Send + Sync + 'static,
    D: Fn(Arc<RwLock<Engine>>, Arc<Inner>, Option<Principal>, bool) -> DispatchOutcome
        + Clone
        + Send
        + Sync
        + 'static,
    R: AsyncRead + Unpin,
{
    // A read-only (snapshot-serving) engine never takes writer admission and
    // never runs a mutating statement. The flag is fixed for the engine's
    // lifetime; reading it costs one uncontended shared lock per frame.
    let read_only = engine.read().map(|eng| eng.is_read_only()).unwrap_or(false);
    if read_only {
        // Transaction control and any write statement require a writer that does
        // not exist in this mode: return the terminal error without touching the
        // gate or the engine write lock.
        let is_write = tx_control.is_some() || autocommit_admission == AdmissionMode::Writer;
        if is_write {
            return (readonly_terminal_message(), None, None);
        }
    }

    match tx_control {
        Some(TransactionControl::Begin) => {
            if tx_permit.is_some() {
                return (
                    error_response(
                        sanitize_error(
                            "cannot begin: a transaction is already active on this connection",
                        ),
                        ErrorClass::Execution,
                    ),
                    None,
                    None,
                );
            }
            let permit = match acquire_begin_permit(&tx_gate, tx_wait_timeout, metrics).await {
                Ok(permit) => permit,
                Err(response) => return (response, None, None),
            };
            let dispatch_begin = dispatch.clone();
            let (response, ticket, mut termination, _) = run_blocking_query(
                engine.clone(),
                Arc::clone(&parsed_query),
                principal.clone(),
                result_mode,
                query_timeout,
                query_deadline,
                metrics,
                reader,
                wire_read_buffer,
                pending_messages,
                move |engine, parsed_query, principal| {
                    dispatch_begin(engine, parsed_query, principal, true)
                },
            )
            .await;
            if is_success_response(&response) {
                *tx_permit = Some(permit);
            } else if is_query_cancellation_response(&response) {
                // Parity with the commit/rollback and in-transaction arms: a
                // cancelled begin must not leave the engine's transaction
                // state ownerless. Install the just-acquired permit so the
                // shared rollback helper can undo any transaction the begin
                // opened, then release the permit and close the connection.
                *tx_permit = Some(permit);
                rollback_connection_transaction(engine, principal, tx_permit).await;
                termination = Some(ConnectionTermination::Closed);
            }
            (response, ticket, termination)
        }
        Some(TransactionControl::Commit | TransactionControl::Rollback) => {
            let standalone_permit = if tx_permit.is_none() {
                match acquire_autocommit_permit(
                    &tx_gate,
                    AdmissionMode::Writer,
                    tx_wait_timeout,
                    metrics,
                )
                .await
                {
                    Ok(permit) => Some(permit),
                    Err(response) => return (response, None, None),
                }
            } else {
                None
            };
            let dispatch_commit = dispatch.clone();
            let (response, ticket, mut termination, _) = run_blocking_query(
                engine.clone(),
                Arc::clone(&parsed_query),
                principal.clone(),
                result_mode,
                query_timeout,
                query_deadline,
                metrics,
                reader,
                wire_read_buffer,
                pending_messages,
                move |engine, parsed_query, principal| {
                    dispatch_commit(engine, parsed_query, principal, true)
                },
            )
            .await;
            if is_success_response(&response) {
                // Release the gate BEFORE the caller waits on the commit's
                // ticket: the engine work is done and WAL order is fixed, so
                // another connection's commit can start (and share the fsync)
                // while this one waits.
                tx_permit.take();
            } else if is_query_cancellation_response(&response) {
                rollback_connection_transaction(engine, principal, tx_permit).await;
                termination = Some(ConnectionTermination::Closed);
            }
            drop(standalone_permit);
            (response, ticket, termination)
        }
        None if tx_permit.is_some() => {
            let dispatch_in_tx = dispatch.clone();
            let mut out = run_blocking_query(
                engine.clone(),
                Arc::clone(&parsed_query),
                principal.clone(),
                result_mode,
                query_timeout,
                query_deadline,
                metrics,
                reader,
                wire_read_buffer,
                pending_messages,
                move |engine, parsed_query, principal| {
                    dispatch_in_tx(engine, parsed_query, principal, true)
                },
            )
            .await;
            if is_query_cancellation_response(&out.0) {
                rollback_connection_transaction(engine, principal, tx_permit).await;
                out.2 = Some(ConnectionTermination::Closed);
            }
            (out.0, out.1, out.2)
        }
        None => {
            let admission = autocommit_admission;
            let permit = match acquire_autocommit_permit(
                &tx_gate,
                admission,
                tx_wait_timeout,
                metrics,
            )
            .await
            {
                Ok(permit) => permit,
                Err(response) => return (response, None, None),
            };
            // The parsed AST can be large. Share the first parse with the
            // rare dirty-view retry instead of cloning it on every successful
            // autocommit read.
            let retry_engine = Arc::clone(&engine);
            let retry_parsed_query = Arc::clone(&parsed_query);
            let retry_principal = principal.clone();
            let allow_readonly_escalation = admission == AdmissionMode::Writer;
            let dispatch_first = dispatch.clone();
            let mut out = run_blocking_query(
                engine,
                parsed_query,
                principal,
                result_mode,
                query_timeout,
                query_deadline,
                metrics,
                reader,
                wire_read_buffer,
                pending_messages,
                move |engine, parsed_query, principal| {
                    dispatch_first(engine, parsed_query, principal, allow_readonly_escalation)
                },
            )
            .await;
            drop(permit);
            if read_only && out.3 {
                // A read reached ReadonlyNeedsWrite (e.g. a stale materialized
                // view). There is no writer to escalate to in read-only mode:
                // surface the terminal error telling the operator to refresh
                // before snapshotting, rather than retrying under a writer.
                out.0 = readonly_terminal_message();
                out.3 = false;
            }
            if out.3 {
                let writer_permit = match acquire_autocommit_permit(
                    &tx_gate,
                    AdmissionMode::Writer,
                    tx_wait_timeout,
                    metrics,
                )
                .await
                {
                    Ok(permit) => permit,
                    Err(response) => return (response, None, None),
                };
                let dispatch_retry = dispatch.clone();
                out = run_blocking_query(
                    retry_engine,
                    retry_parsed_query,
                    retry_principal,
                    result_mode,
                    query_timeout,
                    query_deadline,
                    metrics,
                    reader,
                    wire_read_buffer,
                    pending_messages,
                    move |engine, parsed_query, principal| {
                        dispatch_retry(engine, parsed_query, principal, true)
                    },
                )
                .await;
                drop(writer_permit);
            }
            (out.0, out.1, out.2)
        }
    }
}

/// Execute one PowQL wire query frame. Thin dialect wrapper over
/// [`run_wire_query_state_machine`].
#[allow(clippy::too_many_arguments)]
async fn execute_wire_query<R>(
    engine: Arc<RwLock<Engine>>,
    tx_gate: TxGate,
    tx_permit: &mut Option<OwnedSemaphorePermit>,
    query: String,
    result_mode: WireResultMode,
    principal: Option<Principal>,
    query_timeout: Duration,
    tx_wait_timeout: Duration,
    metrics: &Arc<Metrics>,
    reader: &mut BufReader<R>,
    wire_read_buffer: &mut Vec<u8>,
    pending_messages: &mut InFlightReadAhead,
) -> (
    Message,
    Option<PendingDurability>,
    Option<ConnectionTermination>,
)
where
    R: AsyncRead + Unpin,
{
    let query_deadline = Instant::now() + query_timeout;
    // Parse each frame once for transaction routing, admission, and role
    // enforcement. The engine still canonicalizes/parses as needed for plan
    // cache execution, but the server no longer repeats the same parse in
    // three separate routing helpers before reaching it.
    let stmt_result = parser::parse(&query).map_err(|e| e.to_string());
    let parsed_query = Arc::new((query, stmt_result));
    let tx_control = parsed_transaction_control(&parsed_query.1);
    let autocommit_admission = parsed_query
        .1
        .as_ref()
        .map(statement_admission)
        .unwrap_or(AdmissionMode::Writer);
    run_wire_query_state_machine(
        engine,
        tx_gate,
        tx_permit,
        parsed_query,
        tx_control,
        autocommit_admission,
        result_mode,
        principal,
        query_timeout,
        query_deadline,
        tx_wait_timeout,
        metrics,
        reader,
        wire_read_buffer,
        pending_messages,
        |engine,
         parsed_query: Arc<(String, Result<powdb_query::ast::Statement, String>)>,
         principal: Option<Principal>,
         allow| {
            dispatch_query_parsed(
                &engine,
                &parsed_query.0,
                &parsed_query.1,
                principal.as_ref(),
                allow,
            )
        },
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_wire_query_sql<R>(
    engine: Arc<RwLock<Engine>>,
    tx_gate: TxGate,
    tx_permit: &mut Option<OwnedSemaphorePermit>,
    query: String,
    result_mode: WireResultMode,
    principal: Option<Principal>,
    query_timeout: Duration,
    tx_wait_timeout: Duration,
    metrics: &Arc<Metrics>,
    reader: &mut BufReader<R>,
    wire_read_buffer: &mut Vec<u8>,
    pending_messages: &mut InFlightReadAhead,
) -> (
    Message,
    Option<PendingDurability>,
    Option<ConnectionTermination>,
)
where
    R: AsyncRead + Unpin,
{
    let query_deadline = Instant::now() + query_timeout;
    let stmt_result = sql::parse_sql(&query).map_err(|e| e.to_string());
    let parsed_query = Arc::new((query, stmt_result));
    let tx_control = parsed_transaction_control(&parsed_query.1);
    let autocommit_admission = parsed_query
        .1
        .as_ref()
        .map(statement_admission)
        .unwrap_or(AdmissionMode::Writer);
    run_wire_query_state_machine(
        engine,
        tx_gate,
        tx_permit,
        parsed_query,
        tx_control,
        autocommit_admission,
        result_mode,
        principal,
        query_timeout,
        query_deadline,
        tx_wait_timeout,
        metrics,
        reader,
        wire_read_buffer,
        pending_messages,
        |engine,
         parsed_query: Arc<(String, Result<powdb_query::ast::Statement, String>)>,
         principal: Option<Principal>,
         allow| {
            dispatch_sql_query_parsed(
                &engine,
                &parsed_query.0,
                &parsed_query.1,
                principal.as_ref(),
                allow,
            )
        },
    )
    .await
}

// One over clippy's default arg limit: the metrics handle was threaded through
// to instrument the typed query result. Bundling these into a struct would add
// more noise than it removes for an internal dispatcher.
#[allow(clippy::too_many_arguments)]
async fn execute_wire_query_with_params<R>(
    engine: Arc<RwLock<Engine>>,
    tx_gate: TxGate,
    tx_permit: &mut Option<OwnedSemaphorePermit>,
    query: String,
    params: Vec<WireParam>,
    result_mode: WireResultMode,
    principal: Option<Principal>,
    query_timeout: Duration,
    tx_wait_timeout: Duration,
    metrics: &Arc<Metrics>,
    reader: &mut BufReader<R>,
    wire_read_buffer: &mut Vec<u8>,
    pending_messages: &mut InFlightReadAhead,
) -> (
    Message,
    Option<PendingDurability>,
    Option<ConnectionTermination>,
)
where
    R: AsyncRead + Unpin,
{
    let query_deadline = Instant::now() + query_timeout;
    let bound: Vec<powdb_query::ast::ParamValue> = params.iter().map(wire_param_to_value).collect();
    let stmt_result = parser::parse_with_params(&query, &bound).map_err(|e| e.to_string());
    let parsed_query = Arc::new((query, bound, stmt_result));
    let tx_control = parsed_transaction_control(&parsed_query.2);
    let autocommit_admission = parsed_query
        .2
        .as_ref()
        .map(statement_admission)
        .unwrap_or(AdmissionMode::Writer);
    run_wire_query_state_machine(
        engine,
        tx_gate,
        tx_permit,
        parsed_query,
        tx_control,
        autocommit_admission,
        result_mode,
        principal,
        query_timeout,
        query_deadline,
        tx_wait_timeout,
        metrics,
        reader,
        wire_read_buffer,
        pending_messages,
        |engine,
         parsed_query: Arc<(
            String,
            Vec<powdb_query::ast::ParamValue>,
            Result<powdb_query::ast::Statement, String>,
        )>,
         principal: Option<Principal>,
         allow| {
            dispatch_query_with_bound_params_parsed(
                &engine,
                &parsed_query.0,
                &parsed_query.1,
                &parsed_query.2,
                principal.as_ref(),
                allow,
            )
        },
    )
    .await
}

/// A statement's metric sample whose recording is deferred until its WAL
/// durability obligation settles: a Full-mode fsync failure downgrades the
/// client's success reply to an error, and the metrics must tell the same
/// story (and the latency must include the wait the client observed).
struct DeferredQueryMetric {
    start: Instant,
    outcome: QueryOutcome,
    exceeded_timeout: bool,
}

/// Durability ticket + the deferred metric of the statement that produced it.
type PendingDurability = (WalDurabilityTicket, DeferredQueryMetric);

#[allow(clippy::too_many_arguments)]
async fn run_blocking_query<T, F, R>(
    engine: Arc<RwLock<Engine>>,
    input: T,
    principal: Option<Principal>,
    result_mode: WireResultMode,
    query_timeout: Duration,
    query_deadline: Instant,
    metrics: &Arc<Metrics>,
    reader: &mut BufReader<R>,
    wire_read_buffer: &mut Vec<u8>,
    pending_messages: &mut InFlightReadAhead,
    f: F,
) -> (
    Message,
    Option<PendingDurability>,
    Option<ConnectionTermination>,
    bool,
)
where
    T: Send + 'static,
    F: FnOnce(Arc<RwLock<Engine>>, T, Option<Principal>) -> DispatchOutcome + Send + 'static,
    R: AsyncRead + Unpin,
{
    let _in_flight = metrics.in_flight_guard();
    let start = Instant::now();

    // Cooperative cancellation. The blocking closure installs this token for the
    // executor thread; every unbounded executor loop polls it. We give it a
    // deadline of `now + query_timeout` so the query self-terminates even if the
    // async timeout arm below is slow to be scheduled — that is what actually
    // makes the timeout enforceable (a `spawn_blocking` thread cannot be
    // aborted, so before this the timeout arm just awaited the runaway query to
    // completion while it held the engine lock / tx-gate permit).
    let timeout_ms = query_timeout.as_millis().min(u128::from(u64::MAX)) as u64;
    let cancel = Arc::new(powdb_query::cancel::ExecCancel::with_deadline(
        query_deadline,
        timeout_ms,
    ));
    let cancel_task = Arc::clone(&cancel);
    let mut handle = tokio::task::spawn_blocking(move || {
        let _cancel_guard = powdb_query::cancel::install(cancel_task);
        f(engine, input, principal)
    });
    let mut exceeded_timeout = false;
    let mut termination = None;
    let timeout = tokio::time::sleep(query_deadline.saturating_duration_since(Instant::now()));
    tokio::pin!(timeout);
    let join_result = loop {
        tokio::select! {
            result = &mut handle => break result,
            _ = &mut timeout => {
                exceeded_timeout = true;
                // Signal the executor to stop at its next cancellation checkpoint,
                // then await the (now promptly returning) handle. The closure
                // returns a typed timeout error and releases the engine lock /
                // tx-gate permit as it unwinds.
                cancel.cancel(powdb_query::cancel::CancelReason::Timeout);
                break handle.await;
            }
            read = read_message_cancel_safe(
                reader,
                wire_read_buffer,
                pending_messages.remaining_bytes(),
            ) => {
                match read {
                    Ok(Some(DecodedWireMessage { message: Message::Disconnect, .. })) => {
                        cancel.cancel(powdb_query::cancel::CancelReason::Disconnect);
                        termination = Some(ConnectionTermination::Closed);
                        break handle.await;
                    }
                    Ok(Some(frame)) => {
                        if pending_messages.len() + 1 >= MAX_IN_FLIGHT_READ_AHEAD_FRAMES
                            || pending_messages.wire_bytes + frame.wire_len
                                >= MAX_IN_FLIGHT_READ_AHEAD_BYTES
                        {
                            // Never stop observing the socket behind a full
                            // queue. Reaching either hard cap cancels the query
                            // and closes this connection immediately.
                            cancel.cancel(powdb_query::cancel::CancelReason::Disconnect);
                            termination = Some(ConnectionTermination::ReadError);
                            break handle.await;
                        }
                        // Preserve frames that arrive while the blocking query
                        // runs. The normal batching/main-loop path consumes them
                        // in order once execution completes.
                        pending_messages.push_back(frame);
                    }
                    Ok(None) => {
                        cancel.cancel(powdb_query::cancel::CancelReason::Disconnect);
                        termination = Some(ConnectionTermination::Closed);
                        break handle.await;
                    }
                    Err(_) => {
                        cancel.cancel(powdb_query::cancel::CancelReason::Disconnect);
                        termination = Some(ConnectionTermination::ReadError);
                        break handle.await;
                    }
                }
            }
        }
    };

    let (message, ticket, outcome, readonly_needs_write) = match join_result {
        Ok((Ok(result), ticket)) => match query_result_to_message(result, result_mode) {
            Ok(message) => (message, ticket, QueryOutcome::Ok, false),
            Err(e) => (
                error_response(sanitize_error(&e.to_string()), classify_query_error(&e)),
                ticket,
                QueryOutcome::Error,
                false,
            ),
        },
        Ok((Err(QueryError::ReadonlyNeedsWrite), ticket)) => {
            if exceeded_timeout {
                (
                    error_response(
                        sanitize_error(&QueryError::Timeout { timeout_ms }.to_string()),
                        ErrorClass::Timeout,
                    ),
                    ticket,
                    QueryOutcome::Timeout,
                    true,
                )
            } else {
                (
                    error_response("query execution error", ErrorClass::Internal),
                    ticket,
                    QueryOutcome::Error,
                    true,
                )
            }
        }
        Ok((Err(e), ticket)) => {
            // A deadline-driven cancellation returns Timeout even when the async
            // timeout arm has not fired yet (the executor self-cancels): treat it
            // as a timeout for metrics either way.
            if matches!(e, QueryError::Timeout { .. }) {
                exceeded_timeout = true;
            }
            let outcome = if matches!(e, QueryError::MemoryLimitExceeded { .. }) {
                QueryOutcome::MemoryLimit
            } else if matches!(e, QueryError::Timeout { .. }) {
                QueryOutcome::Timeout
            } else {
                QueryOutcome::Error
            };
            (
                error_response(sanitize_error(&e.to_string()), classify_query_error(&e)),
                ticket,
                outcome,
                false,
            )
        }
        Err(e) => (
            error_response(format!("internal error: {e}"), ErrorClass::Internal),
            None,
            QueryOutcome::Error,
            false,
        ),
    };
    let readonly_retry = readonly_needs_write && !exceeded_timeout && termination.is_none();
    match ticket {
        // The statement's durability (and thus its true outcome and the
        // latency the client observes) settles at batch end — defer the
        // metric to the settlement site instead of recording a success that
        // a failed fsync would falsify.
        Some(ticket) => (
            message,
            Some((
                ticket,
                DeferredQueryMetric {
                    start,
                    outcome,
                    exceeded_timeout,
                },
            )),
            termination,
            readonly_retry,
        ),
        None => {
            if !readonly_retry {
                if exceeded_timeout {
                    metrics.record_query(start.elapsed(), QueryOutcome::Timeout);
                } else {
                    metrics.record_query(start.elapsed(), outcome);
                }
            }
            (message, None, termination, readonly_retry)
        }
    }
}

/// Settle a WAL durability ticket off the async path, AFTER the TxGate
/// permit has been dropped — that ordering is what lets committers on other
/// connections append (and share the fsync) while this one waits.
///
/// Returns `None` when the covering fsync succeeded, or `Some(client-facing
/// error message)` when it failed — in which case no statement the ticket
/// covers may be acknowledged as durable (it executed in memory only).
async fn settle_durability_ticket(ticket: WalDurabilityTicket) -> Option<String> {
    match tokio::task::spawn_blocking(move || ticket.wait()).await {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(sanitize_error(&format!("WAL durability sync failed: {e}"))),
        Err(e) => Some(format!("internal error: {e}")),
    }
}

fn is_success_response(msg: &Message) -> bool {
    matches!(
        msg,
        Message::ResultRows { .. }
            | Message::ResultScalar { .. }
            | Message::ResultRowsNative { .. }
            | Message::ResultScalarNative { .. }
            | Message::ResultOk { .. }
            | Message::ResultMessage { .. }
    )
}

fn rollback_open_transaction(engine: Arc<RwLock<Engine>>, principal: Option<Principal>) {
    let (res, ticket) = dispatch_query(&engine, "rollback", principal.as_ref(), true);
    let _ = res;
    // Rollback takes the sync-preserving path (no ticket), but settle one
    // defensively if it ever appears so the durability watermark stays honest.
    if let Some(ticket) = ticket {
        let _ = ticket.wait();
    }
}

fn is_query_cancellation_response(message: &Message) -> bool {
    matches!(
        message,
        Message::Error { message } | Message::ErrorWithClass { message, .. }
            if message.starts_with("query timeout after")
                || message == "query cancelled by client disconnect"
    )
}

/// Roll back this connection's explicit transaction while it still owns the
/// transaction-gate permit, then release the permit. A timed-out/cancelled
/// statement cannot leave an ambiguous transaction open and block every later
/// writer; releasing first would let another connection enter the engine before
/// this rollback has restored the prior snapshot.
async fn rollback_connection_transaction(
    engine: Arc<RwLock<Engine>>,
    principal: Option<Principal>,
    tx_permit: &mut Option<OwnedSemaphorePermit>,
) {
    if tx_permit.is_none() {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || rollback_open_transaction(engine, principal)).await;
    tx_permit.take();
}

/// Read one post-auth wire frame without losing partially-read bytes when the
/// future is cancelled by `tokio::select!`.
///
/// `Message::read_from` uses `read_exact`, whose future is not cancellation
/// safe: racing it against query completion can consume part of the next frame
/// and then drop those bytes. This reader stores every completed `read` in a
/// connection-owned buffer before awaiting again, so a query may safely race
/// socket EOF / `DISCONNECT` while preserving ordinary pipelined frames.
struct DecodedWireMessage {
    message: Message,
    wire_len: usize,
}

#[derive(Default)]
struct InFlightReadAhead {
    frames: VecDeque<DecodedWireMessage>,
    wire_bytes: usize,
}

impl InFlightReadAhead {
    fn len(&self) -> usize {
        self.frames.len()
    }

    fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    fn remaining_bytes(&self) -> usize {
        MAX_IN_FLIGHT_READ_AHEAD_BYTES.saturating_sub(self.wire_bytes)
    }

    fn push_back(&mut self, frame: DecodedWireMessage) {
        self.wire_bytes += frame.wire_len;
        self.frames.push_back(frame);
    }

    fn pop_front(&mut self) -> Option<Message> {
        let frame = self.frames.pop_front()?;
        self.wire_bytes -= frame.wire_len;
        Some(frame.message)
    }
}

async fn read_message_cancel_safe<R>(
    reader: &mut BufReader<R>,
    buffered: &mut Vec<u8>,
    max_frame_len: usize,
) -> std::io::Result<Option<DecodedWireMessage>>
where
    R: AsyncRead + Unpin,
{
    loop {
        if buffered.len() >= 6 {
            let payload_len = u32::from_le_bytes(
                buffered[2..6]
                    .try_into()
                    .expect("four-byte wire payload length"),
            ) as usize;
            if payload_len > MAX_WIRE_PAYLOAD_SIZE {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("payload too large: {payload_len} bytes (max {MAX_WIRE_PAYLOAD_SIZE})"),
                ));
            }
            let frame_len = 6usize.checked_add(payload_len).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "wire frame length overflow",
                )
            })?;
            if frame_len > max_frame_len {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "wire frame exceeds the available in-flight read-ahead budget: \
                         {frame_len} bytes (available {max_frame_len})"
                    ),
                ));
            }
            if buffered.len() >= frame_len {
                let frame: Vec<u8> = buffered.drain(..frame_len).collect();
                return Message::decode(&frame)
                    .map(|message| {
                        Some(DecodedWireMessage {
                            message,
                            wire_len: frame_len,
                        })
                    })
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e));
            }
        }

        let mut chunk = [0u8; 8192];
        // Read only the bytes needed for this frame stage. Besides preserving
        // cancellation safety, this prevents a large pipelined payload from
        // overshooting the in-flight byte budget in one buffered read.
        let wanted = if buffered.len() < 6 {
            6 - buffered.len()
        } else {
            let payload_len = u32::from_le_bytes(
                buffered[2..6]
                    .try_into()
                    .expect("four-byte wire payload length"),
            ) as usize;
            6usize
                .checked_add(payload_len)
                .and_then(|frame_len| frame_len.checked_sub(buffered.len()))
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "invalid buffered wire frame length",
                    )
                })?
        };
        let read_limit = wanted.min(chunk.len());
        let read = reader.read(&mut chunk[..read_limit]).await?;
        if read == 0 {
            if buffered.len() < 6 {
                // Match the existing protocol behavior: EOF before a complete
                // header is a clean connection close.
                buffered.clear();
                return Ok(None);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed in the middle of a wire frame",
            ));
        }
        buffered.extend_from_slice(&chunk[..read]);
    }
}

#[derive(Clone, Copy, Debug)]
enum ConnectionTermination {
    Closed,
    ReadError,
}

pub async fn handle_connection<S>(stream: S, opts: ConnOpts<'_>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let ConnOpts {
        engine,
        tx_gate,
        expected_password,
        users,
        shutdown_rx,
        idle_timeout,
        query_timeout,
        rate_limiter,
        peer_addr,
        metrics,
        tx_wait_timeout,
        db_name: server_db_name,
    } = opts;

    let peer = peer_addr
        .map(|a| a.to_string())
        .unwrap_or_else(|| "unknown".into());
    let peer_ip = peer_addr.map(|a| a.ip());

    let (reader, writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    // Wait for Connect message (with idle timeout).
    // Accept Ping messages before authentication so load balancers can
    // health-check without completing a full CONNECT handshake.
    // Uses the smaller pre-auth payload limit (4 KB) to prevent memory abuse.
    let connect_msg = loop {
        match tokio::time::timeout(idle_timeout, Message::read_from_preauth(&mut reader)).await {
            Ok(Ok(Some(Message::Ping))) => {
                debug!(peer = %peer, "pre-auth ping");
                if !write_msg(&mut writer, &Message::Pong).await {
                    return;
                }
                continue;
            }
            Ok(Ok(Some(msg))) => break msg,
            Ok(Ok(None)) => {
                debug!(peer = %peer, "client closed before CONNECT");
                return;
            }
            Ok(Err(e)) => {
                error!(peer = %peer, error = %e, "error reading CONNECT");
                return;
            }
            Err(_) => {
                warn!(peer = %peer, "idle timeout waiting for CONNECT");
                return;
            }
        }
    };

    // The authenticated identity for this connection. Bound at connect time
    // and enforced on every query by `dispatch_query`.
    let principal: Option<Principal>;
    let credential_auth_configured = !users.is_empty() || expected_password.is_some();
    match connect_msg {
        Message::Connect {
            db_name,
            password,
            username,
        } => {
            // Check rate limiting before verifying credentials.
            if let (Some(limiter), Some(ip)) = (rate_limiter, peer_ip) {
                if is_rate_limited(limiter, ip) {
                    warn!(peer = %peer, "rate limited: too many auth failures");
                    let err = error_response(
                        "too many auth failures, try again later",
                        ErrorClass::RateLimited,
                    );
                    write_msg(&mut writer, &err).await;
                    return;
                }
            }

            let outcome = authenticate_connect(
                &users,
                expected_password.as_ref().map(|p| p.as_str()),
                username.as_deref(),
                password.as_ref().map(|p| p.as_str()),
            );

            match outcome {
                AuthOutcome::Rejected => {
                    warn!(peer = %peer, db = %db_name, "auth rejected");
                    metrics.inc_auth_failure();
                    // Record the failure for rate limiting.
                    if let (Some(limiter), Some(ip)) = (rate_limiter, peer_ip) {
                        record_auth_failure(limiter, ip);
                    }
                    let err = error_response("authentication failed", ErrorClass::AuthFailed);
                    write_msg(&mut writer, &err).await;
                    return;
                }
                AuthOutcome::Authenticated {
                    principal: auth_principal,
                } => {
                    // Auth succeeded — clear any prior failure count.
                    if let (Some(limiter), Some(ip)) = (rate_limiter, peer_ip) {
                        clear_auth_failures(limiter, ip);
                    }
                    match &auth_principal {
                        Some(p) => {
                            info!(peer = %peer, db = %db_name, user = %p.name, role = %p.role, "authenticated");
                        }
                        None => {
                            info!(peer = %peer, db = %db_name, "client connected");
                        }
                    }
                    principal = auth_principal;
                }
            }

            // One process serves one database. When pinned to a name, reject a
            // CONNECT that explicitly asks for a different one (checked after
            // auth so db existence never leaks to unauthenticated clients).
            // When unpinned, accept anything but warn once per process so the
            // silent one-db-per-process mismatch is visible without spamming
            // the log on every pooled connection.
            match check_db_name(server_db_name.as_deref(), &db_name) {
                Ok(()) => {
                    if server_db_name.is_none() && !db_name.is_empty() && db_name != DEFAULT_DB_NAME
                    {
                        static NAMED_DB_WARNED: std::sync::atomic::AtomicBool =
                            std::sync::atomic::AtomicBool::new(false);
                        if !NAMED_DB_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                            warn!(
                                peer = %peer, db = %db_name,
                                "client requested a named database but this server serves a single global database; name ignored"
                            );
                        }
                    }
                }
                Err(msg) => {
                    warn!(peer = %peer, db = %db_name, "rejected: unknown database");
                    let err = error_response(msg, ErrorClass::AuthFailed);
                    write_msg(&mut writer, &err).await;
                    return;
                }
            }

            let ok = Message::ConnectOk {
                version: env!("CARGO_PKG_VERSION").into(),
            };
            if !write_msg(&mut writer, &ok).await {
                return;
            }
        }
        _ => {
            warn!(peer = %peer, "first message was not CONNECT");
            let err = error_response("expected CONNECT", ErrorClass::Internal);
            write_msg(&mut writer, &err).await;
            return;
        }
    }

    let mut tx_permit: Option<OwnedSemaphorePermit> = None;
    // Persistent framing state makes reads cancellation-safe while they race
    // an in-flight blocking query. Frames decoded during execution retain
    // their original order here for normal pipelined processing afterwards.
    let mut wire_read_buffer = Vec::new();
    let mut pending_messages = InFlightReadAhead::default();
    // A non-query frame decoded during read-ahead batching, carried over to
    // the next iteration of the main loop.
    let mut carry: Option<Message> = None;

    // Main query loop with idle timeout and shutdown awareness.
    'conn: loop {
        let msg = if let Some(m) = carry.take() {
            m
        } else if let Some(m) = pending_messages.pop_front() {
            m
        } else {
            tokio::select! {
                // Read next message with idle timeout.
                result = tokio::time::timeout(
                    idle_timeout,
                    read_message_cancel_safe(
                        &mut reader,
                        &mut wire_read_buffer,
                        MAX_WIRE_PAYLOAD_SIZE + 6,
                    ),
                ) => {
                    match result {
                        Ok(Ok(Some(frame))) => frame.message,
                        Ok(Ok(None)) => break,
                        Ok(Err(e)) => {
                            error!(peer = %peer, error = %e, "read error");
                            break;
                        }
                        Err(_) => {
                            info!(peer = %peer, "idle timeout, closing connection");
                            let err = error_response("idle timeout", ErrorClass::Timeout);
                            write_msg(&mut writer, &err).await;
                            break;
                        }
                    }
                }
                // If server is shutting down, notify client and close.
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!(peer = %peer, "server shutting down, closing connection");
                        let err = error_response("server shutting down", ErrorClass::Internal);
                        write_msg(&mut writer, &err).await;
                        break;
                    }
                    continue;
                }
            }
        };

        // Plain query frames take the batched path: a pipelining client's
        // whole burst is executed with ONE durability wait at the end
        // (durability generations are cumulative, so the newest statement's
        // ticket covers every earlier one). Everything else is handled one
        // frame at a time exactly as before.
        if matches!(
            msg,
            Message::Query { .. }
                | Message::QuerySql { .. }
                | Message::QueryWithParams { .. }
                | Message::QueryNative { .. }
                | Message::QuerySqlNative { .. }
                | Message::QueryWithParamsNative { .. }
        ) {
            /// Read-ahead cap per batch: bounds unflushed responses and keeps
            /// the reply latency of the first statement bounded.
            const MAX_PIPELINE_BATCH: usize = 128;
            /// Byte cap on retained (unflushed) response payloads per batch:
            /// large row results stop read-ahead, so one connection can never
            /// hold gigabytes of replies hostage to the batch's durability
            /// wait.
            const MAX_PIPELINE_BATCH_BYTES: usize = 4 << 20;

            /// Approximate encoded size of a response, for the batch byte
            /// cap. Counts the dominant string payloads; exact per-frame
            /// overhead is irrelevant at the 4 MiB cap.
            fn approx_response_bytes(msg: &Message) -> usize {
                match msg {
                    Message::ResultRows { columns, rows } => {
                        columns.iter().map(|c| c.len() + 4).sum::<usize>()
                            + rows
                                .iter()
                                .map(|r| r.iter().map(|v| v.len() + 4).sum::<usize>())
                                .sum::<usize>()
                    }
                    Message::ResultScalar { value } => value.len(),
                    Message::ResultRowsNative { columns, rows } => {
                        columns.iter().map(|c| c.len() + 4).sum::<usize>()
                            + rows
                                .iter()
                                .map(|row| {
                                    row.iter()
                                        .map(|value| 5 + native_value_body_len(value))
                                        .sum::<usize>()
                                })
                                .sum::<usize>()
                    }
                    Message::ResultScalarNative { value } => 5 + native_value_body_len(value),
                    Message::ResultMessage { message }
                    | Message::Error { message }
                    | Message::ErrorWithClass { message, .. } => message.len(),
                    _ => 16,
                }
            }

            /// Whether the reader's buffered bytes hold at least one COMPLETE
            /// frame (6-byte header + payload). Read-ahead must never await
            /// the socket: blocking on a partial frame would hold the batch's
            /// durability settlement and unflushed replies hostage to a slow
            /// (or malicious) client, up to the idle timeout.
            fn complete_frame_buffered(buf: &[u8]) -> bool {
                buf.len() >= 6 && {
                    let payload_len =
                        u32::from_le_bytes(buf[2..6].try_into().expect("4-byte slice")) as usize;
                    buf.len() - 6 >= payload_len
                }
            }

            let mut responses: Vec<Message> = Vec::new();
            let mut response_bytes: usize = 0;
            let mut last_ticket: Option<WalDurabilityTicket> = None;
            let mut deferred_metrics: Vec<DeferredQueryMetric> = Vec::new();
            let mut fatal: Option<ConnectionTermination> = None;
            let mut current = msg;
            loop {
                let (response, ticket, termination) = match current {
                    Message::Query { query } => {
                        if query.len() > MAX_QUERY_LENGTH {
                            (
                                error_response(
                                    format!(
                                        "query too large: {} bytes (max {})",
                                        query.len(),
                                        MAX_QUERY_LENGTH
                                    ),
                                    ErrorClass::LimitExceeded,
                                ),
                                None,
                                None,
                            )
                        } else {
                            debug!(peer = %peer, query = %query, "received query");
                            execute_wire_query(
                                engine.clone(),
                                tx_gate.clone(),
                                &mut tx_permit,
                                query,
                                WireResultMode::LegacyText,
                                principal.clone(),
                                query_timeout,
                                tx_wait_timeout,
                                &metrics,
                                &mut reader,
                                &mut wire_read_buffer,
                                &mut pending_messages,
                            )
                            .await
                        }
                    }
                    Message::QuerySql { query } => {
                        if query.len() > MAX_QUERY_LENGTH {
                            (
                                error_response(
                                    format!(
                                        "query too large: {} bytes (max {})",
                                        query.len(),
                                        MAX_QUERY_LENGTH
                                    ),
                                    ErrorClass::LimitExceeded,
                                ),
                                None,
                                None,
                            )
                        } else {
                            debug!(peer = %peer, query = %query, "received SQL query");
                            execute_wire_query_sql(
                                engine.clone(),
                                tx_gate.clone(),
                                &mut tx_permit,
                                query,
                                WireResultMode::LegacyText,
                                principal.clone(),
                                query_timeout,
                                tx_wait_timeout,
                                &metrics,
                                &mut reader,
                                &mut wire_read_buffer,
                                &mut pending_messages,
                            )
                            .await
                        }
                    }
                    Message::QueryWithParams { query, params } => {
                        if query.len() > MAX_QUERY_LENGTH {
                            (
                                error_response(
                                    format!(
                                        "query too large: {} bytes (max {})",
                                        query.len(),
                                        MAX_QUERY_LENGTH
                                    ),
                                    ErrorClass::LimitExceeded,
                                ),
                                None,
                                None,
                            )
                        } else {
                            debug!(peer = %peer, query = %query, n_params = params.len(), "received parameterized query");
                            execute_wire_query_with_params(
                                engine.clone(),
                                tx_gate.clone(),
                                &mut tx_permit,
                                query,
                                params,
                                WireResultMode::LegacyText,
                                principal.clone(),
                                query_timeout,
                                tx_wait_timeout,
                                &metrics,
                                &mut reader,
                                &mut wire_read_buffer,
                                &mut pending_messages,
                            )
                            .await
                        }
                    }
                    Message::QueryNative { query } => {
                        if query.len() > MAX_QUERY_LENGTH {
                            (
                                error_response(
                                    format!(
                                        "query too large: {} bytes (max {})",
                                        query.len(),
                                        MAX_QUERY_LENGTH
                                    ),
                                    ErrorClass::LimitExceeded,
                                ),
                                None,
                                None,
                            )
                        } else {
                            debug!(peer = %peer, query = %query, "received native query");
                            execute_wire_query(
                                engine.clone(),
                                tx_gate.clone(),
                                &mut tx_permit,
                                query,
                                WireResultMode::Native,
                                principal.clone(),
                                query_timeout,
                                tx_wait_timeout,
                                &metrics,
                                &mut reader,
                                &mut wire_read_buffer,
                                &mut pending_messages,
                            )
                            .await
                        }
                    }
                    Message::QuerySqlNative { query } => {
                        if query.len() > MAX_QUERY_LENGTH {
                            (
                                error_response(
                                    format!(
                                        "query too large: {} bytes (max {})",
                                        query.len(),
                                        MAX_QUERY_LENGTH
                                    ),
                                    ErrorClass::LimitExceeded,
                                ),
                                None,
                                None,
                            )
                        } else {
                            debug!(peer = %peer, query = %query, "received native SQL query");
                            execute_wire_query_sql(
                                engine.clone(),
                                tx_gate.clone(),
                                &mut tx_permit,
                                query,
                                WireResultMode::Native,
                                principal.clone(),
                                query_timeout,
                                tx_wait_timeout,
                                &metrics,
                                &mut reader,
                                &mut wire_read_buffer,
                                &mut pending_messages,
                            )
                            .await
                        }
                    }
                    Message::QueryWithParamsNative { query, params } => {
                        if query.len() > MAX_QUERY_LENGTH {
                            (
                                error_response(
                                    format!(
                                        "query too large: {} bytes (max {})",
                                        query.len(),
                                        MAX_QUERY_LENGTH
                                    ),
                                    ErrorClass::LimitExceeded,
                                ),
                                None,
                                None,
                            )
                        } else {
                            debug!(peer = %peer, query = %query, n_params = params.len(), "received native parameterized query");
                            execute_wire_query_with_params(
                                engine.clone(),
                                tx_gate.clone(),
                                &mut tx_permit,
                                query,
                                params,
                                WireResultMode::Native,
                                principal.clone(),
                                query_timeout,
                                tx_wait_timeout,
                                &metrics,
                                &mut reader,
                                &mut wire_read_buffer,
                                &mut pending_messages,
                            )
                            .await
                        }
                    }
                    _ => unreachable!("batch loop only receives plain query frames"),
                };
                if let Some((t, m)) = ticket {
                    // Later tickets cover earlier generations — keep only the
                    // newest; the batch-end wait settles them all. Every
                    // deferred metric is kept: each records after settlement.
                    last_ticket = Some(t);
                    deferred_metrics.push(m);
                }
                response_bytes += approx_response_bytes(&response);
                responses.push(response);
                if let Some(reason) = termination {
                    fatal = Some(reason);
                    break;
                }

                // Read ahead only when a COMPLETE next frame is already
                // buffered (never await the socket mid-batch) and the
                // retained replies stay small. While an explicit transaction
                // is open the connection holds the TxGate, so batching would
                // only extend the exclusive window — flush instead.
                if tx_permit.is_some()
                    || responses.len() >= MAX_PIPELINE_BATCH
                    || response_bytes >= MAX_PIPELINE_BATCH_BYTES
                    || (pending_messages.is_empty()
                        && !complete_frame_buffered(&wire_read_buffer)
                        && !complete_frame_buffered(reader.buffer()))
                {
                    break;
                }
                // The full frame is buffered, so this returns without socket
                // I/O; the timeout is a defensive backstop only.
                let next_message = if let Some(message) = pending_messages.pop_front() {
                    Ok(Some(message))
                } else {
                    tokio::time::timeout(
                        idle_timeout,
                        read_message_cancel_safe(
                            &mut reader,
                            &mut wire_read_buffer,
                            MAX_WIRE_PAYLOAD_SIZE + 6,
                        ),
                    )
                    .await
                    .map(|result| result.map(|frame| frame.map(|frame| frame.message)))
                    .unwrap_or_else(|_| {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "timeout decoding fully-buffered frame",
                        ))
                    })
                };
                match next_message {
                    Ok(Some(
                        next @ (Message::Query { .. }
                        | Message::QuerySql { .. }
                        | Message::QueryWithParams { .. }
                        | Message::QueryNative { .. }
                        | Message::QuerySqlNative { .. }
                        | Message::QueryWithParamsNative { .. }),
                    )) => {
                        // If another connection currently holds the TxGate,
                        // the next statement would block on the gate with
                        // this batch's replies still unflushed (pre-batching,
                        // they'd already have been written). Flush first and
                        // handle the frame on the next main-loop iteration.
                        // Benign TOCTOU: worst case is one early flush or one
                        // gate wait with an empty reply queue.
                        if tx_gate.available_permits() == 0 {
                            carry = Some(next);
                            break;
                        }
                        current = next;
                    }
                    Ok(Some(other)) => {
                        // Not a plain query — flush this batch, then handle
                        // the frame on the next main-loop iteration.
                        carry = Some(other);
                        break;
                    }
                    Ok(None) => {
                        fatal = Some(ConnectionTermination::Closed);
                        break;
                    }
                    Err(e) => {
                        error!(peer = %peer, error = %e, "read error");
                        fatal = Some(ConnectionTermination::ReadError);
                        break;
                    }
                }
            }

            // ONE durability wait for the whole batch, then the deferred
            // metrics: a durability failure records Ok statements as errors,
            // and latency includes the settlement wait the client observed.
            let mut durability_failed = false;
            if let Some(ticket) = last_ticket {
                if let Some(message) = settle_durability_ticket(ticket).await {
                    // The covering fsync failed: nothing in this batch may be
                    // acknowledged as durable.
                    durability_failed = true;
                    for r in responses.iter_mut() {
                        if is_success_response(r) {
                            *r = error_response(message.clone(), ErrorClass::Internal);
                        }
                    }
                }
            }
            for m in deferred_metrics.drain(..) {
                let outcome = if m.exceeded_timeout {
                    QueryOutcome::Timeout
                } else if durability_failed && matches!(m.outcome, QueryOutcome::Ok) {
                    QueryOutcome::Error
                } else {
                    m.outcome
                };
                metrics.record_query(m.start.elapsed(), outcome);
            }

            for r in &responses {
                if !write_msg(&mut writer, r).await {
                    break 'conn;
                }
            }
            match fatal {
                None => continue,
                Some(ConnectionTermination::Closed | ConnectionTermination::ReadError) => break,
            }
        }

        let response = match msg {
            Message::Ping => {
                debug!(peer = %peer, "ping");
                Message::Pong
            }
            Message::SyncStatus { replica_id } => {
                let engine = engine.clone();
                let principal = principal.clone();
                let log_context = SyncLogContext::status(&replica_id);
                execute_gated_sync(
                    SyncExecutionContext {
                        tx_gate: tx_gate.clone(),
                        connection_has_transaction: tx_permit.is_some(),
                        operation: SyncOperation::Status,
                        log_context,
                        metrics: &metrics,
                        query_timeout,
                    },
                    (engine, replica_id, credential_auth_configured, principal),
                    |(engine, replica_id, credential_authenticated, principal)| {
                        dispatch_sync_status_decision(
                            &engine,
                            replica_id,
                            credential_authenticated,
                            principal.as_ref(),
                        )
                    },
                )
                .await
            }
            Message::SyncPull {
                replica_id,
                since_lsn,
                max_units,
                max_bytes,
                database_id,
                primary_generation,
                wal_format_version,
                catalog_version,
                segment_format_version,
            } => {
                let engine = engine.clone();
                let principal = principal.clone();
                let request = SyncPullRequest {
                    replica_id,
                    since_lsn,
                    max_units,
                    max_bytes,
                    database_id,
                    primary_generation,
                    wal_format_version,
                    catalog_version,
                    segment_format_version,
                };
                let log_context = SyncLogContext::pull(&request);
                execute_gated_sync(
                    SyncExecutionContext {
                        tx_gate: tx_gate.clone(),
                        connection_has_transaction: tx_permit.is_some(),
                        operation: SyncOperation::Pull,
                        log_context,
                        metrics: &metrics,
                        query_timeout,
                    },
                    (engine, request, credential_auth_configured, principal),
                    |(engine, request, credential_authenticated, principal)| {
                        dispatch_sync_pull_decision(
                            &engine,
                            request,
                            credential_authenticated,
                            principal.as_ref(),
                        )
                    },
                )
                .await
            }
            Message::SyncAck {
                replica_id,
                applied_lsn,
                remote_lsn,
            } => {
                let engine = engine.clone();
                let principal = principal.clone();
                let log_context = SyncLogContext::ack(&replica_id, applied_lsn, remote_lsn);
                execute_gated_sync(
                    SyncExecutionContext {
                        tx_gate: tx_gate.clone(),
                        connection_has_transaction: tx_permit.is_some(),
                        operation: SyncOperation::Ack,
                        log_context,
                        metrics: &metrics,
                        query_timeout,
                    },
                    (
                        engine,
                        replica_id,
                        applied_lsn,
                        remote_lsn,
                        credential_auth_configured,
                        principal,
                    ),
                    |(
                        engine,
                        replica_id,
                        applied_lsn,
                        observed_remote_lsn,
                        credential_authenticated,
                        principal,
                    )| {
                        dispatch_sync_ack_decision(
                            &engine,
                            replica_id,
                            applied_lsn,
                            observed_remote_lsn,
                            credential_authenticated,
                            principal.as_ref(),
                        )
                    },
                )
                .await
            }
            Message::Disconnect => {
                debug!(peer = %peer, "received DISCONNECT");
                break;
            }
            _ => error_response("unexpected message type", ErrorClass::Internal),
        };

        if !write_msg(&mut writer, &response).await {
            break;
        }
    }

    // Roll back any open transaction the client left behind on disconnect.
    // The permit must stay alive in `tx_permit` for the duration of the awaited
    // rollback and be released only afterwards — mirroring the query-timeout
    // path above. Using `tx_permit.take().is_some()` here would drop the permit
    // (freeing the TxGate) *before* the rollback runs, letting another
    // connection BEGIN a transaction that this stale rollback would then clobber.
    if tx_permit.is_some() {
        let engine = engine.clone();
        let principal = principal.clone();
        let _ =
            tokio::task::spawn_blocking(move || rollback_open_transaction(engine, principal)).await;
    }
    tx_permit.take();

    info!(peer = %peer, "client disconnected");
}

fn charge_response_bytes(total: &mut usize, bytes: usize) -> Result<(), QueryError> {
    *total = total.saturating_add(bytes);
    if *total > MAX_RESPONSE_PAYLOAD_SIZE {
        return Err(QueryError::Execution(format!(
            "result too large: encoded response exceeds {} bytes; add a limit or narrower projection",
            MAX_RESPONSE_PAYLOAD_SIZE
        )));
    }
    Ok(())
}

fn native_value_body_len(value: &Value) -> usize {
    match value {
        Value::Empty => 0,
        Value::Int(_) | Value::Float(_) | Value::DateTime(_) => 8,
        Value::Bool(_) => 1,
        Value::Str(value) => value.len(),
        Value::Uuid(_) => 16,
        Value::Bytes(value) => value.len(),
        Value::Json(value) => value.len(),
    }
}

fn query_result_to_message(
    result: QueryResult,
    result_mode: WireResultMode,
) -> Result<Message, QueryError> {
    match result {
        QueryResult::Rows { columns, rows } => {
            let mut encoded_bytes = 2usize; // column count
            for col in &columns {
                charge_response_bytes(&mut encoded_bytes, 4 + col.len())?;
            }
            charge_response_bytes(&mut encoded_bytes, 4)?; // row count

            match result_mode {
                WireResultMode::Native => {
                    for row in &rows {
                        for value in row {
                            charge_response_bytes(
                                &mut encoded_bytes,
                                5 + native_value_body_len(value),
                            )?;
                        }
                    }
                    Ok(Message::ResultRowsNative { columns, rows })
                }
                WireResultMode::LegacyText => {
                    let mut str_rows = Vec::with_capacity(rows.len());
                    for row in rows {
                        let mut str_row = Vec::with_capacity(row.len());
                        for value in row {
                            let display = value_to_display(&value);
                            charge_response_bytes(&mut encoded_bytes, 4 + display.len())?;
                            str_row.push(display);
                        }
                        str_rows.push(str_row);
                    }
                    Ok(Message::ResultRows {
                        columns,
                        rows: str_rows,
                    })
                }
            }
        }
        QueryResult::Scalar(value) => match result_mode {
            WireResultMode::Native => {
                let mut encoded_bytes = 0;
                charge_response_bytes(&mut encoded_bytes, 5 + native_value_body_len(&value))?;
                Ok(Message::ResultScalarNative { value })
            }
            WireResultMode::LegacyText => Ok(Message::ResultScalar {
                value: value_to_display(&value),
            }),
        },
        QueryResult::Modified(n) => Ok(Message::ResultOk { affected: n }),
        QueryResult::Created(name) => Ok(Message::ResultMessage {
            message: format!("type {name} created"),
        }),
        QueryResult::Executed { message } => Ok(Message::ResultMessage { message }),
    }
}

// Canonical wire rendering lives on `Value` (`powdb_storage`) so the server,
// CLI, and embedded bindings render results identically. Kept as a thin alias
// to minimize churn at the call sites in this module.
fn value_to_display(v: &Value) -> String {
    v.to_wire_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use powdb_storage::wal::WalRecordType;
    use powdb_sync::{
        write_identity_snapshot, write_segment_atomic, DatabaseIdentity, IdentitySnapshot,
        ReplicaCursor, RetainedSegment, RetainedUnit,
    };

    // ---- Wire NULL rendering (Fix: remote protocol rendered NULL as `{}`) ----

    #[test]
    fn null_serializes_as_null_bareword_on_wire() {
        assert_eq!(value_to_display(&Value::Empty), "null");
    }

    // ---- Error sanitization allowlist ----

    #[test]
    fn unique_violation_error_surfaces_to_remote_clients() {
        // The storage layer reports the actionable message; the server must
        // not replace it with the generic "query execution error".
        assert_eq!(
            sanitize_error("unique constraint violation on User.email"),
            "unique constraint violation on User.email"
        );
    }

    #[test]
    fn internal_errors_stay_generic() {
        assert_eq!(
            sanitize_error("some internal io panic detail"),
            "query execution error"
        );
    }

    #[test]
    fn cancellation_errors_surface_to_remote_clients() {
        // A cancelled/timed-out query must reach the client with its real
        // message (both are derived from the configured timeout or a client
        // disconnect and leak no internal state) rather than the generic mask.
        for msg in [
            &QueryError::Timeout { timeout_ms: 2000 }.to_string(),
            &QueryError::Cancelled.to_string(),
        ] {
            assert_eq!(sanitize_error(msg), *msg, "should pass through verbatim");
        }
        // Sanity-check the exact wording the executor emits.
        assert_eq!(
            QueryError::Timeout { timeout_ms: 2000 }.to_string(),
            "query timeout after 2000ms"
        );
        assert_eq!(
            QueryError::Cancelled.to_string(),
            "query cancelled by client disconnect"
        );
    }

    // ---- JSON (v0.12): canonical-text wire rendering + parse-error passthrough ----

    #[test]
    fn json_cell_renders_canonical_text_on_wire() {
        // A Json value flows through the same string-cell path as every other
        // value (value_to_display -> Value::to_wire_string). PJ1 is canonical,
        // so keys come back sorted bytewise regardless of input order and the
        // client receives parseable JSON text with no protocol change.
        let pj1 = powdb_storage::pj1::parse_json_text(r#"{"b":2,"a":1,"nested":{"z":true}}"#)
            .expect("valid JSON");
        let result = QueryResult::Rows {
            columns: vec!["doc".into()],
            rows: vec![vec![Value::Json(pj1.into())]],
        };
        match query_result_to_message(result, WireResultMode::LegacyText).expect("encodes") {
            Message::ResultRows { columns, rows } => {
                assert_eq!(columns, vec!["doc"]);
                assert_eq!(
                    rows,
                    vec![vec![r#"{"a":1,"b":2,"nested":{"z":true}}"#.to_string()]]
                );
            }
            other => panic!("expected ResultRows, got {other:?}"),
        }
    }

    #[test]
    fn json_parse_error_surfaces_to_remote_clients() {
        // Lane B rejects invalid JSON on insert as QueryError::TypeError, whose
        // Display is "type mismatch: <detail>" (crates/query/src/result.rs).
        // That prefix is allowlisted, so the actionable detail reaches the
        // client instead of the generic "query execution error". The raw
        // storage-layer phrasing ("invalid JSON: ...") is also allowlisted as
        // defense-in-depth. Internal PJ1 corruption ("malformed PJ1: ...") is
        // deliberately NOT allowlisted: it leaks storage internals and never
        // occurs on the client-driven insert path.
        for msg in [
            "type mismatch: invalid JSON: unexpected character 'x' at position 3",
            "invalid JSON: nesting exceeds depth cap 128",
        ] {
            assert_eq!(sanitize_error(msg), msg, "should pass through verbatim");
        }
        assert_eq!(
            sanitize_error("malformed PJ1: truncated"),
            "query execution error",
            "internal storage corruption must stay masked"
        );
    }

    // `describe <Type>` renders a json column's type as the bareword "json"
    // over the wire. introspect_describe emits type_id_to_name(TypeId::Json) =
    // "json" (crates/query/src/executor/compiled.rs) as a Str cell, which flows
    // through value_to_display unchanged; Lane B's DDL keyword makes `type Doc
    // { body: json }` accepted, so this runs end to end (v0.12, Lane D).
    #[test]
    fn describe_shows_json_type_over_the_wire() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type Doc { required id: int, body: json }")
            .expect("json column DDL should be accepted once Lane B lands");
        let result = engine.execute_powql("describe Doc").expect("describe runs");
        let msg = query_result_to_message(result, WireResultMode::LegacyText).expect("encodes");
        match msg {
            Message::ResultRows { columns, rows } => {
                assert_eq!(columns[1], "type");
                // The `body` column's type cell must be the bareword "json".
                let body = rows
                    .iter()
                    .find(|r| r[0] == "body")
                    .expect("body column present");
                assert_eq!(body[1], "json");
            }
            other => panic!("expected ResultRows, got {other:?}"),
        }
    }

    // ---- Named-database gate (P-10) ----

    #[test]
    fn db_name_unpinned_accepts_any_name() {
        for requested in ["", "default", "prod", "anything"] {
            assert!(
                check_db_name(None, requested).is_ok(),
                "rejected {requested}"
            );
        }
    }

    #[test]
    fn db_name_pinned_accepts_match_empty_and_default_sentinel() {
        // The configured name, the empty name, and the client default sentinel
        // are all "no foreign database explicitly requested".
        assert!(check_db_name(Some("prod"), "prod").is_ok());
        assert!(check_db_name(Some("prod"), "").is_ok());
        assert!(check_db_name(Some("prod"), DEFAULT_DB_NAME).is_ok());
    }

    #[test]
    fn db_name_pinned_rejects_foreign_with_clear_message() {
        let err = check_db_name(Some("prod"), "staging").unwrap_err();
        assert_eq!(err, "unknown database 'staging'; this server serves 'prod'");
    }

    // ---- Explicit-transaction gate wait timeout (P-4) ----

    #[tokio::test]
    async fn begin_permit_acquires_when_gate_is_free() {
        let gate = new_tx_gate();
        let metrics = Arc::new(Metrics::new());
        let permit = acquire_begin_permit(&gate, Duration::from_secs(5), &metrics)
            .await
            .expect("should acquire a free gate");
        assert_eq!(gate.available_permits(), 0, "permit must be held");
        drop(permit);
        assert_eq!(
            gate.available_permits(),
            DEFAULT_TX_GATE_READER_PERMITS as usize,
            "permit pool must release on drop"
        );
    }

    #[tokio::test]
    async fn begin_permit_times_out_with_clear_error_and_truthful_metric() {
        let gate = new_tx_gate();
        let metrics = Arc::new(Metrics::new());
        // Hold the full writer admission so the next acquire must time out.
        let _held = gate
            .clone()
            .acquire_many_owned(DEFAULT_TX_GATE_READER_PERMITS)
            .await
            .unwrap();
        let err = acquire_begin_permit(&gate, Duration::from_millis(25), &metrics)
            .await
            .expect_err("must time out while the gate is held");
        match err {
            Message::ErrorWithClass { message, class } => {
                assert_eq!(class, ErrorClass::Timeout);
                assert!(
                    message.contains("transaction gate timeout after 25ms"),
                    "unexpected message: {message}"
                );
                assert!(
                    message.contains("waiting for concurrent transaction"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
        let rendered = metrics.render();
        assert!(rendered.contains("powdb_tx_gate_timeouts_total 1"));
        // A timed-out begin is a failed statement from the client's view.
        assert!(rendered.contains("powdb_queries_total{result=\"error\"} 1"));
    }

    #[test]
    fn admission_classification_has_query_shape_parity_and_fails_closed() {
        assert_eq!(
            classify_query_admission("User filter .id = 1"),
            AdmissionMode::Reader
        );
        assert_eq!(
            classify_sql_admission("SELECT * FROM User WHERE id = 1"),
            AdmissionMode::Reader
        );
        assert_eq!(
            classify_params_admission("User filter .id = $1", &[WireParam::Int(1)]),
            AdmissionMode::Reader
        );

        assert_eq!(
            classify_query_admission("insert User { id := 1 }"),
            AdmissionMode::Writer
        );
        assert_eq!(
            classify_sql_admission("INSERT INTO User (id) VALUES (1)"),
            AdmissionMode::Writer
        );
        assert_eq!(
            classify_params_admission("insert User { id := $1 }", &[WireParam::Int(1)]),
            AdmissionMode::Writer
        );
        assert_eq!(
            classify_query_admission("this is not valid PowQL"),
            AdmissionMode::Writer,
            "uncertain statements must never enter through reader admission"
        );
    }

    #[tokio::test]
    async fn writer_admission_excludes_readers() {
        let gate = new_tx_gate();
        let metrics = Arc::new(Metrics::new());
        let writer = acquire_begin_permit(&gate, Duration::from_secs(1), &metrics)
            .await
            .expect("writer admission");
        let blocked = acquire_autocommit_permit(
            &gate,
            AdmissionMode::Reader,
            Duration::from_millis(25),
            &metrics,
        )
        .await;
        assert!(blocked.is_err(), "reader must wait behind writer admission");
        drop(writer);
        let _reader = acquire_autocommit_permit(
            &gate,
            AdmissionMode::Reader,
            Duration::from_secs(1),
            &metrics,
        )
        .await
        .expect("reader must proceed after writer releases");
    }

    #[tokio::test]
    async fn queued_writer_is_not_starved_by_later_readers() {
        let gate = new_tx_gate();
        let metrics = Arc::new(Metrics::new());
        let first_reader = acquire_autocommit_permit(
            &gate,
            AdmissionMode::Reader,
            Duration::from_secs(1),
            &metrics,
        )
        .await
        .expect("first reader admission");

        let writer_gate = gate.clone();
        let writer_metrics = metrics.clone();
        let mut writer = tokio::spawn(async move {
            acquire_begin_permit(&writer_gate, Duration::from_secs(1), &writer_metrics).await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;

        let late_gate = gate.clone();
        let late_metrics = metrics.clone();
        let mut late_reader = tokio::spawn(async move {
            acquire_autocommit_permit(
                &late_gate,
                AdmissionMode::Reader,
                Duration::from_secs(1),
                &late_metrics,
            )
            .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut late_reader)
                .await
                .is_err(),
            "a later reader must queue behind the waiting writer"
        );

        drop(first_reader);
        let writer_permit = tokio::time::timeout(Duration::from_secs(1), &mut writer)
            .await
            .expect("writer must acquire once prior readers drain")
            .expect("writer task")
            .expect("writer admission");
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut late_reader)
                .await
                .is_err(),
            "writer must hold exclusive admission before the late reader"
        );
        drop(writer_permit);
        let _late_reader = tokio::time::timeout(Duration::from_secs(1), late_reader)
            .await
            .expect("late reader must eventually acquire")
            .expect("late reader task")
            .expect("late reader admission");
    }

    fn dirty_view_engine() -> (tempfile::TempDir, Arc<RwLock<Engine>>) {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type Source { required id: int }")
            .unwrap();
        engine.execute_powql("insert Source { id := 1 }").unwrap();
        engine
            .execute_powql("materialize Snapshot as Source")
            .unwrap();
        engine.execute_powql("insert Source { id := 2 }").unwrap();
        (dir, Arc::new(RwLock::new(engine)))
    }

    #[test]
    fn dirty_view_requests_explicit_escalation_on_every_frontend() {
        let (_dir, engine) = dirty_view_engine();

        assert!(matches!(
            dispatch_query(&engine, "Snapshot", None, false).0,
            Err(QueryError::ReadonlyNeedsWrite)
        ));
        assert!(matches!(
            dispatch_sql_query(&engine, "SELECT * FROM Snapshot", None, false).0,
            Err(QueryError::ReadonlyNeedsWrite)
        ));
        assert!(matches!(
            dispatch_query_with_params(
                &engine,
                "Snapshot filter .id = $1",
                &[WireParam::Int(1)],
                None,
                false,
            )
            .0,
            Err(QueryError::ReadonlyNeedsWrite)
        ));
    }

    #[tokio::test]
    async fn dirty_view_upgrade_waits_for_held_reader_then_records_once() {
        let (_dir, engine) = dirty_view_engine();
        let gate = new_tx_gate_with_permits(2);
        let metrics = Arc::new(Metrics::new());
        let held_reader = acquire_autocommit_permit(
            &gate,
            AdmissionMode::Reader,
            Duration::from_secs(1),
            &metrics,
        )
        .await
        .expect("held reader admission");

        // Keep the peer open so the query monitor waits instead of treating
        // EOF as a client disconnect while the admission upgrade is blocked.
        let (_client, server) = tokio::io::duplex(1024);
        let task_gate = gate.clone();
        let task_metrics = metrics.clone();
        let mut task = tokio::spawn(async move {
            let mut reader = BufReader::new(server);
            let mut wire_read_buffer = Vec::new();
            let mut pending_messages = InFlightReadAhead::default();
            let mut tx_permit = None;
            execute_wire_query(
                engine,
                task_gate,
                &mut tx_permit,
                "Snapshot".into(),
                WireResultMode::Native,
                None,
                Duration::from_secs(2),
                Duration::from_secs(1),
                &task_metrics,
                &mut reader,
                &mut wire_read_buffer,
                &mut pending_messages,
            )
            .await
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut task)
                .await
                .is_err(),
            "dirty-view retry must wait for exclusive admission while another reader is held"
        );
        drop(held_reader);

        let (message, ticket, termination) = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("upgrade must finish after the held reader releases")
            .expect("query task");
        assert!(matches!(message, Message::ResultRowsNative { .. }));
        assert!(termination.is_none());

        let (ticket, metric) = ticket.expect("view refresh must defer its WAL metric");
        drop(ticket);
        metrics.record_query(metric.start.elapsed(), metric.outcome);

        let rendered = metrics.render();
        assert!(rendered.contains("powdb_queries_total{result=\"ok\"} 1"));
        assert!(rendered.contains("powdb_queries_total{result=\"error\"} 0"));
    }

    #[tokio::test]
    async fn timed_out_readonly_escalation_is_not_retried_or_reported_as_generic_error() {
        let (_dir, engine) = dirty_view_engine();
        let metrics = Arc::new(Metrics::new());
        // Keep the peer open so the socket monitor does not turn this into a
        // disconnect before the deadline fires.
        let (_client, server) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(server);
        let mut wire_read_buffer = Vec::new();
        let mut pending_messages = InFlightReadAhead::default();
        let query_timeout = Duration::from_millis(20);
        let query_deadline = Instant::now() + query_timeout;

        let (message, ticket, termination, retry) = run_blocking_query(
            engine,
            (),
            None,
            WireResultMode::Native,
            query_timeout,
            query_deadline,
            &metrics,
            &mut reader,
            &mut wire_read_buffer,
            &mut pending_messages,
            |_engine, (), _principal| {
                // Ignore the token deliberately: this reproduces the race in
                // which the async deadline wins but the joined task's final
                // result is the internal dirty-view escalation sentinel.
                std::thread::sleep(Duration::from_millis(50));
                (Err(QueryError::ReadonlyNeedsWrite), None)
            },
        )
        .await;

        assert!(ticket.is_none());
        assert!(termination.is_none());
        assert!(!retry, "a timed-out query must never be resurrected");
        match message {
            Message::ErrorWithClass { message, class } => {
                assert_eq!(class, ErrorClass::Timeout);
                assert!(
                    message.contains("query timeout after 20ms"),
                    "timeout must remain client-visible, got {message}"
                );
            }
            other => panic!("expected timeout error, got {other:?}"),
        }
        let rendered = metrics.render();
        assert!(rendered.contains("powdb_query_timeouts_total 1"));
        assert!(rendered.contains("powdb_queries_total{result=\"error\"} 1"));
    }

    #[test]
    fn resource_limit_errors_surface_actionable_hints() {
        // These carry user-actionable guidance and leak no internal state, so
        // they must reach the client verbatim — not be masked to the generic
        // message. The exact strings come from QueryError's Display impl
        // (crates/query/src/result.rs).
        for msg in [
            "sort input exceeds row limit — add a LIMIT clause",
            "join result exceeds row limit",
            "query exceeded memory budget: requested 100 bytes, limit 50 bytes",
            "result too large: encoded response exceeds 1024 bytes; add a limit or narrower projection",
        ] {
            assert_eq!(sanitize_error(msg), msg, "should pass through verbatim");
        }
    }

    #[test]
    fn oversized_result_is_rejected_before_wire_encoding() {
        let long = "x".repeat(MAX_RESPONSE_PAYLOAD_SIZE);
        let result = QueryResult::Rows {
            columns: vec!["payload".into()],
            rows: vec![vec![Value::Str(long)]],
        };
        let err = query_result_to_message(result, WireResultMode::LegacyText).unwrap_err();
        assert!(
            err.to_string().starts_with("result too large"),
            "unexpected error: {err}"
        );
    }

    // ---- Role enforcement (Fix: readonly role was not enforced) ----

    fn parsed(q: &str) -> powdb_query::ast::Statement {
        parser::parse(q).unwrap()
    }

    fn principal(role: &str) -> Option<Principal> {
        Some(Principal {
            name: "u".into(),
            role: role.into(),
        })
    }

    #[test]
    fn readonly_can_read_but_not_write() {
        let p = principal("readonly");
        // Reads pass.
        assert!(check_statement_permitted(p.as_ref(), &parsed("User")).is_ok());
        assert!(check_statement_permitted(p.as_ref(), &parsed("count(User)")).is_ok());
        assert!(check_statement_permitted(p.as_ref(), &parsed("explain User")).is_ok());
        // Writes, DDL, and transaction control are denied.
        for q in [
            r#"insert User { name := "x" }"#,
            "User filter .id = 1 update { age := 2 }",
            "User filter .id = 1 delete",
            "drop User",
            "alter User add column c: str",
            "type T { required id: int }",
            "begin",
            "commit",
            "rollback",
        ] {
            let err = check_statement_permitted(p.as_ref(), &parsed(q))
                .expect_err(&format!("must deny: {q}"));
            assert!(
                err.to_string().contains("permission denied"),
                "unexpected error for {q}: {err}"
            );
        }
    }

    #[test]
    fn readwrite_and_admin_have_full_query_access() {
        for role in ["readwrite", "admin"] {
            let p = principal(role);
            assert!(check_statement_permitted(p.as_ref(), &parsed("User")).is_ok());
            assert!(check_statement_permitted(
                p.as_ref(),
                &parsed(r#"insert User { name := "x" }"#)
            )
            .is_ok());
            assert!(check_statement_permitted(p.as_ref(), &parsed("drop User")).is_ok());
        }
    }

    #[test]
    fn unknown_role_fails_closed_for_writes() {
        let p = principal("mystery");
        assert!(check_statement_permitted(p.as_ref(), &parsed("User")).is_ok());
        assert!(
            check_statement_permitted(p.as_ref(), &parsed(r#"insert User { name := "x" }"#))
                .is_err()
        );
    }

    #[test]
    fn no_principal_means_full_access() {
        // Shared-password / open mode: no per-user identity, no restriction.
        assert!(check_statement_permitted(None, &parsed("drop User")).is_ok());
        assert!(check_statement_permitted(None, &parsed(r#"insert User { name := "x" }"#)).is_ok());
    }

    fn store_with_alice() -> UserStore {
        let mut s = UserStore::new();
        s.create_user("alice", "pw", "readwrite").unwrap();
        s
    }

    // ---- Empty store: legacy shared-password fallback ----

    #[test]
    fn empty_store_no_password_is_open() {
        let s = UserStore::new();
        assert_eq!(
            authenticate_connect(&s, None, None, None),
            AuthOutcome::Authenticated { principal: None }
        );
        // Even a stray username/password is accepted (legacy open behavior).
        assert_eq!(
            authenticate_connect(&s, None, Some("x"), Some("y")),
            AuthOutcome::Authenticated { principal: None }
        );
    }

    #[test]
    fn empty_store_correct_shared_password_succeeds() {
        let s = UserStore::new();
        assert_eq!(
            authenticate_connect(&s, Some("pw"), None, Some("pw")),
            AuthOutcome::Authenticated { principal: None }
        );
    }

    #[test]
    fn empty_store_wrong_shared_password_rejected() {
        let s = UserStore::new();
        assert_eq!(
            authenticate_connect(&s, Some("pw"), None, Some("bad")),
            AuthOutcome::Rejected
        );
    }

    #[test]
    fn empty_store_missing_password_rejected_when_expected() {
        let s = UserStore::new();
        assert_eq!(
            authenticate_connect(&s, Some("pw"), None, None),
            AuthOutcome::Rejected
        );
    }

    #[test]
    fn empty_store_ignores_username_for_shared_password() {
        // A new client may send a username even against a shared-password
        // server; the username is ignored and the password still governs.
        let s = UserStore::new();
        assert_eq!(
            authenticate_connect(&s, Some("pw"), Some("whoever"), Some("pw")),
            AuthOutcome::Authenticated { principal: None }
        );
    }

    // ---- Populated store: multi-user auth ----

    #[test]
    fn user_auth_success_binds_principal() {
        let s = store_with_alice();
        assert_eq!(
            authenticate_connect(&s, None, Some("alice"), Some("pw")),
            AuthOutcome::Authenticated {
                principal: Some(Principal {
                    name: "alice".into(),
                    role: "readwrite".into(),
                })
            }
        );
    }

    #[test]
    fn user_auth_wrong_password_rejected() {
        let s = store_with_alice();
        assert_eq!(
            authenticate_connect(&s, None, Some("alice"), Some("bad")),
            AuthOutcome::Rejected
        );
    }

    #[test]
    fn user_auth_unknown_user_rejected() {
        let s = store_with_alice();
        assert_eq!(
            authenticate_connect(&s, None, Some("mallory"), Some("pw")),
            AuthOutcome::Rejected
        );
    }

    #[test]
    fn user_auth_missing_username_rejected() {
        let s = store_with_alice();
        assert_eq!(
            authenticate_connect(&s, None, None, Some("pw")),
            AuthOutcome::Rejected
        );
    }

    #[test]
    fn user_auth_missing_password_rejected() {
        let s = store_with_alice();
        assert_eq!(
            authenticate_connect(&s, Some("pw"), Some("alice"), None),
            AuthOutcome::Rejected
        );
    }

    #[test]
    fn user_auth_ignores_shared_password_when_users_present() {
        // With users present, the shared password is irrelevant: supplying it as
        // the password without a valid user must NOT authenticate.
        let s = store_with_alice();
        assert_eq!(
            authenticate_connect(&s, Some("shared"), None, Some("shared")),
            AuthOutcome::Rejected
        );
    }

    #[test]
    fn replica_fingerprint_is_stable_and_redacted() {
        let replica_id = "customer-prod-replica-a";
        let fingerprint = replica_fingerprint(replica_id);
        assert_eq!(fingerprint, replica_fingerprint(replica_id));
        assert_eq!(fingerprint, log_replica_fingerprint(replica_id));
        assert_ne!(fingerprint, replica_fingerprint("customer-prod-replica-b"));
        assert_eq!(fingerprint.len(), 16);
        assert!(fingerprint.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!fingerprint.contains("customer"));
        assert!(!fingerprint.contains("replica"));
        assert!(!fingerprint.contains(replica_id));
    }

    #[test]
    fn invalid_replica_ids_use_fixed_log_fingerprint() {
        assert_eq!(log_replica_fingerprint(""), INVALID_REPLICA_FINGERPRINT);
        assert_eq!(
            log_replica_fingerprint("customer/prod/replica"),
            INVALID_REPLICA_FINGERPRINT
        );
        assert_eq!(
            log_replica_fingerprint(&"a".repeat(4096)),
            INVALID_REPLICA_FINGERPRINT
        );
    }

    #[test]
    fn sync_error_classes_use_bounded_labels() {
        assert_eq!(SyncErrorClass::AuthRequired.as_label(), "auth_required");
        assert_eq!(
            SyncErrorClass::PermissionDenied.as_label(),
            "permission_denied"
        );
        assert_eq!(
            SyncErrorClass::IdentityOrFormatMismatch.as_label(),
            "identity_or_format_mismatch"
        );
        assert_eq!(SyncErrorClass::AckValidation.as_label(), "ack_validation");
        assert_eq!(SyncErrorClass::Internal.as_label(), "internal");
    }

    fn sync_identity() -> DatabaseIdentity {
        DatabaseIdentity {
            database_id: *b"server-sync-test",
            primary_generation: 1,
        }
    }

    fn retained_unit(lsn: u64) -> RetainedUnit {
        RetainedUnit {
            tx_id: 1,
            record_type: 4,
            lsn,
            data: lsn.to_le_bytes().to_vec(),
        }
    }

    fn retained_unit_with(tx_id: u64, record_type: WalRecordType, lsn: u64) -> RetainedUnit {
        RetainedUnit {
            tx_id,
            record_type: record_type as u8,
            lsn,
            data: lsn.to_le_bytes().to_vec(),
        }
    }

    fn write_sync_identity_and_tail(data_dir: &std::path::Path, through_lsn: u64) {
        let identity = sync_identity();
        write_identity_snapshot(data_dir, &IdentitySnapshot::from_identity(identity, 1)).unwrap();
        let units = (1..=through_lsn).map(retained_unit).collect();
        let segment = RetainedSegment::new(identity.segment_identity(), units).unwrap();
        write_segment_atomic(&retained_segments_dir(data_dir), &segment).unwrap();
    }

    fn write_sync_identity_and_units(data_dir: &std::path::Path, units: Vec<RetainedUnit>) {
        let identity = sync_identity();
        write_identity_snapshot(data_dir, &IdentitySnapshot::from_identity(identity, 1)).unwrap();
        let segment = RetainedSegment::new(identity.segment_identity(), units).unwrap();
        write_segment_atomic(&retained_segments_dir(data_dir), &segment).unwrap();
    }

    fn write_sync_identity_only(data_dir: &std::path::Path) {
        let identity = sync_identity();
        write_identity_snapshot(data_dir, &IdentitySnapshot::from_identity(identity, 1)).unwrap();
    }

    fn admin_principal() -> Principal {
        Principal {
            name: "admin".into(),
            role: "admin".into(),
        }
    }

    #[test]
    fn sync_protocol_requires_credential_auth_and_rejects_readonly() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(RwLock::new(Engine::new(dir.path()).unwrap()));

        match dispatch_sync_status(&engine, "replica-a".into(), false, None) {
            Message::Error { message } => {
                assert!(message.contains("requires authentication"));
            }
            other => panic!("expected auth error, got {other:?}"),
        }

        let readonly = Principal {
            name: "reader".into(),
            role: "readonly".into(),
        };
        match dispatch_sync_status(&engine, "replica-a".into(), true, Some(&readonly)) {
            Message::Error { message } => {
                assert!(message.contains("permission denied"));
            }
            other => panic!("expected permission error, got {other:?}"),
        }
    }

    #[test]
    fn sync_status_pull_and_ack_use_server_remote_lsn() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type SyncT { required id: int, v: str }")
            .unwrap();
        engine
            .execute_powql(r#"insert SyncT { id := 1, v := "one" }"#)
            .unwrap();
        let remote_lsn = engine.catalog().max_lsn();
        assert!(remote_lsn > 0);
        write_sync_identity_and_tail(dir.path(), remote_lsn);
        powdb_sync::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 0))
            .unwrap();

        let engine = Arc::new(RwLock::new(engine));
        let principal = admin_principal();
        let status = match dispatch_sync_status(&engine, "replica-a".into(), true, Some(&principal))
        {
            Message::SyncStatusResult { status } => status,
            other => panic!("expected sync status, got {other:?}"),
        };
        assert_eq!(status.remote_lsn, remote_lsn);
        assert_eq!(status.servable_lsn, Some(remote_lsn));
        assert_eq!(status.unarchived_lsn, Some(0));
        assert_eq!(status.last_applied_lsn, Some(0));
        assert_eq!(status.repair_action, WireSyncRepairAction::Pull);
        assert!(status.stale);

        let identity = sync_identity().segment_identity();
        let pull = SyncPullRequest {
            replica_id: "replica-a".into(),
            since_lsn: 0,
            max_units: MAX_SYNC_PULL_UNITS,
            max_bytes: MAX_SYNC_PULL_BYTES,
            database_id: identity.database_id,
            primary_generation: identity.primary_generation,
            wal_format_version: identity.wal_format_version,
            catalog_version: identity.catalog_version,
            segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
        };
        let units = match dispatch_sync_pull(&engine, pull, true, Some(&principal)) {
            Message::SyncPullResult {
                status,
                units,
                has_more,
            } => {
                assert_eq!(status.repair_action, WireSyncRepairAction::Pull);
                assert!(!has_more);
                units
            }
            other => panic!("expected sync pull result, got {other:?}"),
        };
        assert_eq!(units.len() as u64, remote_lsn);
        assert_eq!(units.last().unwrap().lsn, remote_lsn);

        let ack = match dispatch_sync_ack(
            &engine,
            "replica-a".into(),
            remote_lsn,
            remote_lsn,
            true,
            Some(&principal),
        ) {
            Message::SyncAckResult {
                previous_applied_lsn,
                applied_lsn,
                remote_lsn: ack_remote_lsn,
                advanced,
                status,
            } => {
                assert_eq!(previous_applied_lsn, 0);
                assert_eq!(applied_lsn, remote_lsn);
                assert_eq!(ack_remote_lsn, remote_lsn);
                assert!(advanced);
                status
            }
            other => panic!("expected sync ack result, got {other:?}"),
        };
        assert_eq!(ack.repair_action, WireSyncRepairAction::None);
        assert!(!ack.stale);
        assert_eq!(ack.lag_lsn, Some(0));
    }

    fn seed_pullable_replica(engine: &mut Engine) -> u64 {
        let data_dir = engine.catalog().data_dir().to_path_buf();
        let remote_lsn = engine.catalog().max_lsn();
        assert!(remote_lsn > 0);
        write_sync_identity_and_tail(&data_dir, remote_lsn);
        powdb_sync::upsert_replica_cursor(&data_dir, ReplicaCursor::active("replica-a", 0))
            .unwrap();
        remote_lsn
    }

    fn pull_request_with_catalog_version(catalog_version: u16) -> SyncPullRequest {
        let identity = sync_identity().segment_identity();
        SyncPullRequest {
            replica_id: "replica-a".into(),
            since_lsn: 0,
            max_units: MAX_SYNC_PULL_UNITS,
            max_bytes: MAX_SYNC_PULL_BYTES,
            database_id: identity.database_id,
            primary_generation: identity.primary_generation,
            wal_format_version: identity.wal_format_version,
            catalog_version,
            segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
        }
    }

    #[test]
    fn fresh_database_expects_legacy_catalog_version_and_accepts_v5_replica() {
        use powdb_storage::catalog::{CATALOG_VERSION, LEGACY_CATALOG_VERSION};

        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type Doc { required id: int, data: json }")
            .unwrap();
        engine
            .execute_powql(r#"insert Doc { id := 1, data := "{\"score\":20}" }"#)
            .unwrap();
        // No expression index created yet: the database stays at the legacy
        // catalog format, exactly as a v0.12 database on disk.
        assert_eq!(
            engine.catalog().active_catalog_version(),
            LEGACY_CATALOG_VERSION
        );
        let remote_lsn = seed_pullable_replica(&mut engine);

        let engine = Arc::new(RwLock::new(engine));
        let principal = admin_principal();

        // A replica whose maximum is the legacy version (as v0.12 clients state)
        // is accepted against a legacy-active server.
        let pull = pull_request_with_catalog_version(LEGACY_CATALOG_VERSION);
        match dispatch_sync_pull(&engine, pull, true, Some(&principal)) {
            Message::SyncPullResult { units, .. } => {
                assert_eq!(units.len() as u64, remote_lsn);
            }
            other => panic!("expected sync pull result, got {other:?}"),
        }

        // A newer replica (states this binary's max) is also accepted.
        let pull = pull_request_with_catalog_version(CATALOG_VERSION);
        assert!(matches!(
            dispatch_sync_pull(&engine, pull, true, Some(&principal)),
            Message::SyncPullResult { .. }
        ));

        // A replica whose maximum is older than the active format is rejected
        // with a message naming both versions.
        let pull = pull_request_with_catalog_version(LEGACY_CATALOG_VERSION - 1);
        match dispatch_sync_pull(&engine, pull, true, Some(&principal)) {
            Message::Error { message } => {
                assert!(message.contains("v4"), "message: {message}");
                assert!(message.contains("v5"), "message: {message}");
                assert!(
                    message.contains("rebootstrap with an upgraded replica required"),
                    "message: {message}"
                );
            }
            other => panic!("expected identity mismatch error, got {other:?}"),
        }
    }

    #[test]
    fn activated_database_expects_v6_and_rejects_v5_replica() {
        use powdb_storage::catalog::{EXPRESSION_INDEX_CATALOG_VERSION, LEGACY_CATALOG_VERSION};

        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type Doc { required id: int, data: json }")
            .unwrap();
        engine
            .execute_powql(r#"insert Doc { id := 1, data := "{\"score\":20}" }"#)
            .unwrap();
        // Creating a JSON-path expression index activates the v6 catalog format.
        engine
            .execute_powql("alter Doc add index (.data->score)")
            .unwrap();
        assert_eq!(
            engine.catalog().active_catalog_version(),
            EXPRESSION_INDEX_CATALOG_VERSION
        );
        let remote_lsn = seed_pullable_replica(&mut engine);

        let engine = Arc::new(RwLock::new(engine));
        let principal = admin_principal();

        // A v0.12 replica (states catalog_version 5) genuinely cannot read the
        // now-activated v6 data and is rejected with the targeted message.
        let pull = pull_request_with_catalog_version(LEGACY_CATALOG_VERSION);
        match dispatch_sync_pull(&engine, pull, true, Some(&principal)) {
            Message::Error { message } => {
                assert!(message.contains("v5"), "message: {message}");
                assert!(message.contains("v6"), "message: {message}");
                assert!(
                    message.contains("rebootstrap with an upgraded replica required"),
                    "message: {message}"
                );
            }
            other => panic!("expected identity mismatch error, got {other:?}"),
        }

        // A v6-capable replica is accepted.
        let pull = pull_request_with_catalog_version(EXPRESSION_INDEX_CATALOG_VERSION);
        match dispatch_sync_pull(&engine, pull, true, Some(&principal)) {
            Message::SyncPullResult { units, .. } => {
                assert_eq!(units.len() as u64, remote_lsn);
            }
            other => panic!("expected sync pull result, got {other:?}"),
        }
    }

    #[test]
    fn sync_pull_and_ack_reject_transaction_cut_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type SyncT { required id: int }")
            .unwrap();
        for id in 1..=3 {
            engine
                .execute_powql(&format!("insert SyncT {{ id := {id} }}"))
                .unwrap();
        }
        let remote_lsn = engine.catalog().max_lsn();
        assert!(remote_lsn >= 3);
        write_sync_identity_and_units(
            dir.path(),
            vec![
                retained_unit_with(77, WalRecordType::Begin, 1),
                retained_unit_with(77, WalRecordType::Insert, 2),
                retained_unit_with(77, WalRecordType::Commit, 3),
            ],
        );
        powdb_sync::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 0))
            .unwrap();

        let engine = Arc::new(RwLock::new(engine));
        let principal = admin_principal();
        let identity = sync_identity().segment_identity();
        let cut_pull = SyncPullRequest {
            replica_id: "replica-a".into(),
            since_lsn: 0,
            max_units: 2,
            max_bytes: MAX_SYNC_PULL_BYTES,
            database_id: identity.database_id,
            primary_generation: identity.primary_generation,
            wal_format_version: identity.wal_format_version,
            catalog_version: identity.catalog_version,
            segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
        };
        match dispatch_sync_pull(&engine, cut_pull, true, Some(&principal)) {
            Message::Error { message } => assert!(message.contains("cuts through transaction")),
            other => panic!("expected transaction-cut pull error, got {other:?}"),
        }

        let cut_bytes_pull = SyncPullRequest {
            replica_id: "replica-a".into(),
            since_lsn: 0,
            max_units: 3,
            max_bytes: 58,
            database_id: identity.database_id,
            primary_generation: identity.primary_generation,
            wal_format_version: identity.wal_format_version,
            catalog_version: identity.catalog_version,
            segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
        };
        match dispatch_sync_pull(&engine, cut_bytes_pull, true, Some(&principal)) {
            Message::Error { message } => assert!(message.contains("cuts through transaction")),
            other => panic!("expected byte-capped transaction-cut pull error, got {other:?}"),
        }

        let full_pull = SyncPullRequest {
            replica_id: "replica-a".into(),
            since_lsn: 0,
            max_units: 3,
            max_bytes: MAX_SYNC_PULL_BYTES,
            database_id: identity.database_id,
            primary_generation: identity.primary_generation,
            wal_format_version: identity.wal_format_version,
            catalog_version: identity.catalog_version,
            segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
        };
        match dispatch_sync_pull(&engine, full_pull, true, Some(&principal)) {
            Message::SyncPullResult { units, .. } => {
                assert_eq!(units.len(), 3);
                assert_eq!(units.last().unwrap().lsn, 3);
            }
            other => panic!("expected complete transaction pull, got {other:?}"),
        }

        match dispatch_sync_ack(
            &engine,
            "replica-a".into(),
            2,
            remote_lsn,
            true,
            Some(&principal),
        ) {
            Message::Error { message } => assert!(message.contains("cuts through transaction")),
            other => panic!("expected transaction-cut ack error, got {other:?}"),
        }
        let cursor = powdb_sync::read_replica_cursors(dir.path()).unwrap();
        assert_eq!(cursor[0].applied_lsn, 0);

        match dispatch_sync_ack(
            &engine,
            "replica-a".into(),
            3,
            remote_lsn,
            true,
            Some(&principal),
        ) {
            Message::SyncAckResult {
                previous_applied_lsn,
                applied_lsn,
                advanced,
                ..
            } => {
                assert_eq!(previous_applied_lsn, 0);
                assert_eq!(applied_lsn, 3);
                assert!(advanced);
            }
            other => panic!("expected complete transaction ack, got {other:?}"),
        }
    }

    #[test]
    fn sync_pull_byte_cap_returns_applyable_prefix_with_reused_tx_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type SyncT { required id: int }")
            .unwrap();
        for id in 1..=6 {
            engine
                .execute_powql(&format!("insert SyncT {{ id := {id} }}"))
                .unwrap();
        }
        let remote_lsn = engine.catalog().max_lsn();
        assert!(remote_lsn >= 6);
        write_sync_identity_and_units(
            dir.path(),
            vec![
                retained_unit_with(1, WalRecordType::Begin, 1),
                retained_unit_with(1, WalRecordType::Insert, 2),
                retained_unit_with(1, WalRecordType::Commit, 3),
                retained_unit_with(1, WalRecordType::Begin, 4),
                retained_unit_with(1, WalRecordType::Insert, 5),
                retained_unit_with(1, WalRecordType::Commit, 6),
            ],
        );
        powdb_sync::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 0))
            .unwrap();

        let engine = Arc::new(RwLock::new(engine));
        let principal = admin_principal();
        let identity = sync_identity().segment_identity();
        let pull = SyncPullRequest {
            replica_id: "replica-a".into(),
            since_lsn: 0,
            max_units: 6,
            max_bytes: 100,
            database_id: identity.database_id,
            primary_generation: identity.primary_generation,
            wal_format_version: identity.wal_format_version,
            catalog_version: identity.catalog_version,
            segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
        };

        match dispatch_sync_pull(&engine, pull, true, Some(&principal)) {
            Message::SyncPullResult {
                status,
                units,
                has_more,
            } => {
                assert_eq!(status.repair_action, WireSyncRepairAction::Pull);
                assert_eq!(units.len(), 3);
                assert_eq!(units.last().unwrap().lsn, 3);
                assert!(has_more);
            }
            other => panic!("expected byte-capped applyable prefix, got {other:?}"),
        }
    }

    #[test]
    fn sync_pull_never_serves_units_beyond_server_remote_lsn() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type SyncT { required id: int }")
            .unwrap();
        engine.execute_powql("insert SyncT { id := 1 }").unwrap();
        let remote_lsn = engine.catalog().max_lsn();
        assert!(remote_lsn > 0);
        write_sync_identity_and_tail(dir.path(), remote_lsn + 2);
        powdb_sync::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 0))
            .unwrap();

        let engine = Arc::new(RwLock::new(engine));
        let principal = admin_principal();
        let identity = sync_identity().segment_identity();
        let pull = SyncPullRequest {
            replica_id: "replica-a".into(),
            since_lsn: 0,
            max_units: MAX_SYNC_PULL_UNITS,
            max_bytes: MAX_SYNC_PULL_BYTES,
            database_id: identity.database_id,
            primary_generation: identity.primary_generation,
            wal_format_version: identity.wal_format_version,
            catalog_version: identity.catalog_version,
            segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
        };

        match dispatch_sync_pull(&engine, pull, true, Some(&principal)) {
            Message::SyncPullResult {
                status,
                units,
                has_more,
            } => {
                assert_eq!(status.remote_lsn, remote_lsn);
                assert_eq!(status.servable_lsn, Some(remote_lsn));
                assert_eq!(status.unarchived_lsn, Some(0));
                assert_eq!(status.repair_action, WireSyncRepairAction::Pull);
                assert!(!has_more);
                assert_eq!(units.len() as u64, remote_lsn);
                assert_eq!(units.last().unwrap().lsn, remote_lsn);
                assert!(units.iter().all(|unit| unit.lsn <= remote_lsn));
            }
            other => panic!("expected capped sync pull result, got {other:?}"),
        }
    }

    #[test]
    fn sync_status_reports_await_archive_when_primary_outruns_retained_tail() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type SyncT { required id: int }")
            .unwrap();
        engine.execute_powql("insert SyncT { id := 1 }").unwrap();
        let remote_lsn = engine.catalog().max_lsn();
        assert!(remote_lsn > 0);
        write_sync_identity_only(dir.path());
        powdb_sync::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 0))
            .unwrap();

        let engine = Arc::new(RwLock::new(engine));
        let principal = admin_principal();
        let identity = sync_identity().segment_identity();
        let status = match dispatch_sync_status(&engine, "replica-a".into(), true, Some(&principal))
        {
            Message::SyncStatusResult { status } => status,
            other => panic!("expected sync status, got {other:?}"),
        };
        assert_eq!(status.remote_lsn, remote_lsn);
        assert_eq!(status.servable_lsn, Some(0));
        assert_eq!(status.unarchived_lsn, Some(remote_lsn));
        assert_eq!(status.repair_action, WireSyncRepairAction::AwaitArchive);
        assert!(status
            .last_sync_error
            .as_deref()
            .unwrap()
            .contains("not yet archived"));

        let pull = SyncPullRequest {
            replica_id: "replica-a".into(),
            since_lsn: 0,
            max_units: MAX_SYNC_PULL_UNITS,
            max_bytes: MAX_SYNC_PULL_BYTES,
            database_id: identity.database_id,
            primary_generation: identity.primary_generation,
            wal_format_version: identity.wal_format_version,
            catalog_version: identity.catalog_version,
            segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
        };
        match dispatch_sync_pull(&engine, pull, true, Some(&principal)) {
            Message::SyncPullResult {
                status,
                units,
                has_more,
            } => {
                assert_eq!(status.repair_action, WireSyncRepairAction::AwaitArchive);
                assert!(units.is_empty());
                assert!(!has_more);
            }
            other => panic!("expected await-archive sync pull result, got {other:?}"),
        }
    }

    #[test]
    fn sync_pull_serves_partial_retained_prefix_when_archive_lags_remote_lsn() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type SyncT { required id: int }")
            .unwrap();
        engine.execute_powql("insert SyncT { id := 1 }").unwrap();
        engine.execute_powql("insert SyncT { id := 2 }").unwrap();
        let remote_lsn = engine.catalog().max_lsn();
        assert!(remote_lsn > 1);
        let servable_lsn = remote_lsn - 1;
        write_sync_identity_and_tail(dir.path(), servable_lsn);
        powdb_sync::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 0))
            .unwrap();

        let engine = Arc::new(RwLock::new(engine));
        let principal = admin_principal();
        let identity = sync_identity().segment_identity();
        let pull = SyncPullRequest {
            replica_id: "replica-a".into(),
            since_lsn: 0,
            max_units: MAX_SYNC_PULL_UNITS,
            max_bytes: MAX_SYNC_PULL_BYTES,
            database_id: identity.database_id,
            primary_generation: identity.primary_generation,
            wal_format_version: identity.wal_format_version,
            catalog_version: identity.catalog_version,
            segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
        };

        match dispatch_sync_pull(&engine, pull, true, Some(&principal)) {
            Message::SyncPullResult {
                status,
                units,
                has_more,
            } => {
                assert_eq!(status.remote_lsn, remote_lsn);
                assert_eq!(status.servable_lsn, Some(servable_lsn));
                assert_eq!(status.unarchived_lsn, Some(1));
                assert_eq!(status.repair_action, WireSyncRepairAction::Pull);
                assert!(!has_more);
                assert_eq!(units.len() as u64, servable_lsn);
                assert_eq!(units.last().unwrap().lsn, servable_lsn);
                assert!(units.iter().all(|unit| unit.lsn <= servable_lsn));
            }
            other => panic!("expected partial sync pull result, got {other:?}"),
        }
    }

    #[test]
    fn sync_pull_rejects_cursor_or_format_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type SyncT { required id: int }")
            .unwrap();
        engine.execute_powql("insert SyncT { id := 1 }").unwrap();
        let remote_lsn = engine.catalog().max_lsn();
        write_sync_identity_and_tail(dir.path(), remote_lsn);
        powdb_sync::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 0))
            .unwrap();
        let engine = Arc::new(RwLock::new(engine));
        let principal = admin_principal();
        let identity = sync_identity().segment_identity();

        let wrong_cursor = SyncPullRequest {
            replica_id: "replica-a".into(),
            since_lsn: 1,
            max_units: 10,
            max_bytes: MAX_SYNC_PULL_BYTES,
            database_id: identity.database_id,
            primary_generation: identity.primary_generation,
            wal_format_version: identity.wal_format_version,
            catalog_version: identity.catalog_version,
            segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
        };
        match dispatch_sync_pull(&engine, wrong_cursor, true, Some(&principal)) {
            Message::Error { message } => assert!(message.contains("does not match")),
            other => panic!("expected cursor mismatch error, got {other:?}"),
        }

        let wrong_format = SyncPullRequest {
            replica_id: "replica-a".into(),
            since_lsn: 0,
            max_units: 10,
            max_bytes: MAX_SYNC_PULL_BYTES,
            database_id: identity.database_id,
            primary_generation: identity.primary_generation,
            wal_format_version: identity.wal_format_version,
            catalog_version: identity.catalog_version,
            segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION + 1,
        };
        match dispatch_sync_pull(&engine, wrong_format, true, Some(&principal)) {
            Message::Error { message } => assert!(message.contains("rebootstrap required")),
            other => panic!("expected format mismatch error, got {other:?}"),
        }
    }
}
