use crate::metrics::{Metrics, QueryOutcome, SyncOperation, SyncOutcome, SyncRepairLabel};
use crate::protocol::{Message, WireParam, WireRetainedUnit, WireSyncRepairAction, WireSyncStatus};
use powdb_auth::{Permission, Role, UserStore};
use powdb_query::executor::{is_read_only_statement, Engine, WalDurabilityTicket};
use powdb_query::parser;
use powdb_query::result::{QueryError, QueryResult};
use powdb_query::sql;
use powdb_storage::types::Value;
use powdb_sync::{
    acknowledge_replica_apply, read_identity, read_units_through, replica_sync_status,
    retained_segments_dir, validate_retained_tail_available, validate_v1_retained_units_applyable,
    ReplicaSyncStatus, RetainedUnit, SyncRepairAction, RETAINED_SEGMENT_FORMAT_VERSION,
};
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, error, info, warn};
use zeroize::Zeroizing;

/// Tracks per-IP authentication failure counts for rate limiting.
pub type AuthRateLimiter = Arc<Mutex<HashMap<IpAddr, (u32, Instant)>>>;

/// Gate that serializes wire-protocol statements while an explicit
/// transaction is open on any connection. The connection that runs `begin`
/// keeps an owned permit until `commit`, `rollback`, disconnect, or timeout,
/// preventing other connections from observing or joining uncommitted state.
pub type TxGate = Arc<Semaphore>;

/// Create a transaction gate for a shared engine.
pub fn new_tx_gate() -> TxGate {
    Arc::new(Semaphore::new(1))
}

/// Maximum query text length accepted from the wire (1 MB).
const MAX_QUERY_LENGTH: usize = 1024 * 1024;

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
];

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
) -> DispatchOutcome {
    let stmt_result = parser::parse(query).map_err(|e| e.to_string());

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
                // Escalate: fall through to the write path below.
            }
            Err(e) => return (Err(e), None),
        }
    }

    if matches!(
        parsed_transaction_control(&stmt_result),
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

fn dispatch_sql_query(
    engine: &Arc<RwLock<Engine>>,
    query: &str,
    principal: Option<&Principal>,
) -> DispatchOutcome {
    let stmt_result = sql::parse_sql(query).map_err(|e| e.to_string());

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
            Err(QueryError::ReadonlyNeedsWrite) => {}
            Err(e) => return (Err(e), None),
        }
    }

    if matches!(
        parsed_transaction_control(&stmt_result),
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

fn transaction_control(stmt: &powdb_query::ast::Statement) -> Option<TransactionControl> {
    use powdb_query::ast::Statement;
    match stmt {
        Statement::Begin => Some(TransactionControl::Begin),
        Statement::Commit => Some(TransactionControl::Commit),
        Statement::Rollback => Some(TransactionControl::Rollback),
        _ => None,
    }
}

fn classify_query_transaction_control(query: &str) -> Option<TransactionControl> {
    parser::parse(query)
        .ok()
        .and_then(|stmt| transaction_control(&stmt))
}

fn classify_sql_transaction_control(query: &str) -> Option<TransactionControl> {
    sql::parse_sql(query)
        .ok()
        .and_then(|stmt| transaction_control(&stmt))
}

fn classify_params_transaction_control(
    query: &str,
    params: &[WireParam],
) -> Option<TransactionControl> {
    let bound: Vec<powdb_query::ast::ParamValue> = params.iter().map(wire_param_to_value).collect();
    parser::parse_with_params(query, &bound)
        .ok()
        .and_then(|stmt| transaction_control(&stmt))
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

fn dispatch_query_with_params(
    engine: &Arc<RwLock<Engine>>,
    query: &str,
    params: &[WireParam],
    principal: Option<&Principal>,
) -> DispatchOutcome {
    let bound: Vec<powdb_query::ast::ParamValue> = params.iter().map(wire_param_to_value).collect();

    // Parse once (with params bound) so role enforcement and read/write
    // classification see exactly the statement that will execute.
    let stmt_result = parser::parse_with_params(query, &bound).map_err(|e| e.to_string());

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
            eng.execute_powql_readonly_with_params(query, &bound)
        };
        match res {
            Ok(r) => return (Ok(r), None),
            Err(QueryError::ReadonlyNeedsWrite) => {
                // Escalate to the write path below.
            }
            Err(e) => return (Err(e), None),
        }
    }

    if matches!(
        parsed_transaction_control(&stmt_result),
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
    execute_write_deferred(engine, |eng| eng.execute_powql_with_params(query, &bound))
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

fn sync_context(engine: &Arc<RwLock<Engine>>) -> Result<(PathBuf, u64), String> {
    let engine = engine
        .read()
        .map_err(|e| format!("lock poisoned while reading sync context: {e}"))?;
    let catalog = engine.catalog();
    Ok((catalog.data_dir().to_path_buf(), catalog.max_lsn()))
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
    let (data_dir, remote_lsn) = match sync_context(engine) {
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

    let (data_dir, remote_lsn) = match sync_context(engine) {
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
    let expected = identity.segment_identity();
    if request.database_id != expected.database_id
        || request.primary_generation != expected.primary_generation
        || request.wal_format_version != expected.wal_format_version
        || request.catalog_version != expected.catalog_version
        || request.segment_format_version != RETAINED_SEGMENT_FORMAT_VERSION
    {
        return SyncDecision::error(
            SyncErrorClass::IdentityOrFormatMismatch,
            "sync pull identity or format version mismatch; rebootstrap required",
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
    let (data_dir, remote_lsn) = match sync_context(engine) {
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

    let permit = match tx_gate.acquire_owned().await {
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
    match tokio::time::timeout(tx_wait_timeout, tx_gate.clone().acquire_owned()).await {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err(Message::Error {
            message: "query execution error".into(),
        }),
        Err(_) => {
            metrics.inc_tx_gate_timeout();
            Err(Message::Error {
                message: format!(
                    "transaction gate timeout after {}ms waiting for concurrent transaction to complete",
                    tx_wait_timeout.as_millis()
                ),
            })
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
    tx_wait_timeout: Duration,
    metrics: &Arc<Metrics>,
) -> Result<OwnedSemaphorePermit, Message> {
    match tokio::time::timeout(tx_wait_timeout, tx_gate.clone().acquire_owned()).await {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err(Message::Error {
            message: "query execution error".into(),
        }),
        Err(_) => {
            metrics.inc_tx_gate_timeout();
            Err(Message::Error {
                message: format!(
                    "transaction gate timeout after {}ms waiting for concurrent transaction to complete",
                    tx_wait_timeout.as_millis()
                ),
            })
        }
    }
}

/// Execute one wire query frame and return the response plus its un-waited
/// WAL durability ticket. The TxGate permit is managed here and — crucially —
/// is already released (bare statements, commit/rollback) by the time this
/// returns, so the caller's `finalize_durability` wait happens OUTSIDE the
/// gate and overlapping committers can share an fsync.
#[allow(clippy::too_many_arguments)]
async fn execute_wire_query(
    engine: Arc<RwLock<Engine>>,
    tx_gate: TxGate,
    tx_permit: &mut Option<OwnedSemaphorePermit>,
    query: String,
    principal: Option<Principal>,
    query_timeout: Duration,
    tx_wait_timeout: Duration,
    metrics: &Arc<Metrics>,
) -> (Message, Option<PendingDurability>) {
    match classify_query_transaction_control(&query) {
        Some(TransactionControl::Begin) => {
            if tx_permit.is_some() {
                return (
                    Message::Error {
                        message: sanitize_error(
                            "cannot begin: a transaction is already active on this connection",
                        ),
                    },
                    None,
                );
            }
            let permit = match acquire_begin_permit(&tx_gate, tx_wait_timeout, metrics).await {
                Ok(permit) => permit,
                Err(response) => return (response, None),
            };
            let (response, ticket) = run_blocking_query(
                engine,
                query,
                principal,
                query_timeout,
                metrics,
                |engine, query, principal| dispatch_query(&engine, &query, principal.as_ref()),
            )
            .await;
            if is_success_response(&response) {
                *tx_permit = Some(permit);
            }
            (response, ticket)
        }
        Some(TransactionControl::Commit | TransactionControl::Rollback) => {
            let (response, ticket) = run_blocking_query(
                engine,
                query,
                principal,
                query_timeout,
                metrics,
                |engine, query, principal| dispatch_query(&engine, &query, principal.as_ref()),
            )
            .await;
            if is_success_response(&response) {
                // Release the gate BEFORE the caller waits on the commit's
                // ticket: the engine work is done and WAL order is fixed, so
                // another connection's commit can start (and share the fsync)
                // while this one waits.
                tx_permit.take();
            }
            (response, ticket)
        }
        None if tx_permit.is_some() => {
            run_blocking_query(
                engine,
                query,
                principal,
                query_timeout,
                metrics,
                |engine, query, principal| dispatch_query(&engine, &query, principal.as_ref()),
            )
            .await
        }
        None => {
            let permit = match acquire_autocommit_permit(&tx_gate, tx_wait_timeout, metrics).await {
                Ok(permit) => permit,
                Err(response) => return (response, None),
            };
            let out = run_blocking_query(
                engine,
                query,
                principal,
                query_timeout,
                metrics,
                |engine, query, principal| dispatch_query(&engine, &query, principal.as_ref()),
            )
            .await;
            drop(permit);
            out
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_wire_query_sql(
    engine: Arc<RwLock<Engine>>,
    tx_gate: TxGate,
    tx_permit: &mut Option<OwnedSemaphorePermit>,
    query: String,
    principal: Option<Principal>,
    query_timeout: Duration,
    tx_wait_timeout: Duration,
    metrics: &Arc<Metrics>,
) -> (Message, Option<PendingDurability>) {
    match classify_sql_transaction_control(&query) {
        Some(TransactionControl::Begin) => {
            if tx_permit.is_some() {
                return (
                    Message::Error {
                        message: sanitize_error(
                            "cannot begin: a transaction is already active on this connection",
                        ),
                    },
                    None,
                );
            }
            let permit = match acquire_begin_permit(&tx_gate, tx_wait_timeout, metrics).await {
                Ok(permit) => permit,
                Err(response) => return (response, None),
            };
            let (response, ticket) = run_blocking_query(
                engine,
                query,
                principal,
                query_timeout,
                metrics,
                |engine, query, principal| dispatch_sql_query(&engine, &query, principal.as_ref()),
            )
            .await;
            if is_success_response(&response) {
                *tx_permit = Some(permit);
            }
            (response, ticket)
        }
        Some(TransactionControl::Commit | TransactionControl::Rollback) => {
            let (response, ticket) = run_blocking_query(
                engine,
                query,
                principal,
                query_timeout,
                metrics,
                |engine, query, principal| dispatch_sql_query(&engine, &query, principal.as_ref()),
            )
            .await;
            if is_success_response(&response) {
                // See execute_wire_query: release the gate before the
                // caller's durability wait so commits can coalesce.
                tx_permit.take();
            }
            (response, ticket)
        }
        None if tx_permit.is_some() => {
            run_blocking_query(
                engine,
                query,
                principal,
                query_timeout,
                metrics,
                |engine, query, principal| dispatch_sql_query(&engine, &query, principal.as_ref()),
            )
            .await
        }
        None => {
            let permit = match acquire_autocommit_permit(&tx_gate, tx_wait_timeout, metrics).await {
                Ok(permit) => permit,
                Err(response) => return (response, None),
            };
            let out = run_blocking_query(
                engine,
                query,
                principal,
                query_timeout,
                metrics,
                |engine, query, principal| dispatch_sql_query(&engine, &query, principal.as_ref()),
            )
            .await;
            drop(permit);
            out
        }
    }
}

// One over clippy's default arg limit: the metrics handle was threaded through
// to instrument the typed query result. Bundling these into a struct would add
// more noise than it removes for an internal dispatcher.
#[allow(clippy::too_many_arguments)]
async fn execute_wire_query_with_params(
    engine: Arc<RwLock<Engine>>,
    tx_gate: TxGate,
    tx_permit: &mut Option<OwnedSemaphorePermit>,
    query: String,
    params: Vec<WireParam>,
    principal: Option<Principal>,
    query_timeout: Duration,
    tx_wait_timeout: Duration,
    metrics: &Arc<Metrics>,
) -> (Message, Option<PendingDurability>) {
    match classify_params_transaction_control(&query, &params) {
        Some(TransactionControl::Begin) => {
            if tx_permit.is_some() {
                return (
                    Message::Error {
                        message: sanitize_error(
                            "cannot begin: a transaction is already active on this connection",
                        ),
                    },
                    None,
                );
            }
            let permit = match acquire_begin_permit(&tx_gate, tx_wait_timeout, metrics).await {
                Ok(permit) => permit,
                Err(response) => return (response, None),
            };
            let (response, ticket) = run_blocking_query(
                engine,
                (query, params),
                principal,
                query_timeout,
                metrics,
                |engine, (query, params), principal| {
                    dispatch_query_with_params(&engine, &query, &params, principal.as_ref())
                },
            )
            .await;
            if is_success_response(&response) {
                *tx_permit = Some(permit);
            }
            (response, ticket)
        }
        Some(TransactionControl::Commit | TransactionControl::Rollback) => {
            let (response, ticket) = run_blocking_query(
                engine,
                (query, params),
                principal,
                query_timeout,
                metrics,
                |engine, (query, params), principal| {
                    dispatch_query_with_params(&engine, &query, &params, principal.as_ref())
                },
            )
            .await;
            if is_success_response(&response) {
                // See execute_wire_query: release the gate before the
                // caller's durability wait so commits can coalesce.
                tx_permit.take();
            }
            (response, ticket)
        }
        None if tx_permit.is_some() => {
            run_blocking_query(
                engine,
                (query, params),
                principal,
                query_timeout,
                metrics,
                |engine, (query, params), principal| {
                    dispatch_query_with_params(&engine, &query, &params, principal.as_ref())
                },
            )
            .await
        }
        None => {
            let permit = match acquire_autocommit_permit(&tx_gate, tx_wait_timeout, metrics).await {
                Ok(permit) => permit,
                Err(response) => return (response, None),
            };
            let out = run_blocking_query(
                engine,
                (query, params),
                principal,
                query_timeout,
                metrics,
                |engine, (query, params), principal| {
                    dispatch_query_with_params(&engine, &query, &params, principal.as_ref())
                },
            )
            .await;
            drop(permit);
            out
        }
    }
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

async fn run_blocking_query<T, F>(
    engine: Arc<RwLock<Engine>>,
    input: T,
    principal: Option<Principal>,
    query_timeout: Duration,
    metrics: &Arc<Metrics>,
    f: F,
) -> (Message, Option<PendingDurability>)
where
    T: Send + 'static,
    F: FnOnce(Arc<RwLock<Engine>>, T, Option<Principal>) -> DispatchOutcome + Send + 'static,
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
        Instant::now() + query_timeout,
        timeout_ms,
    ));
    let cancel_task = Arc::clone(&cancel);
    let mut handle = tokio::task::spawn_blocking(move || {
        let _cancel_guard = powdb_query::cancel::install(cancel_task);
        f(engine, input, principal)
    });
    let mut exceeded_timeout = false;
    let join_result = tokio::select! {
        result = &mut handle => result,
        _ = tokio::time::sleep(query_timeout) => {
            exceeded_timeout = true;
            // Signal the executor to stop at its next cancellation checkpoint,
            // then await the (now promptly returning) handle. The closure
            // returns a typed timeout error and releases the engine lock /
            // tx-gate permit as it unwinds.
            cancel.cancel(powdb_query::cancel::CancelReason::Timeout);
            handle.await
        }
    };

    let (message, ticket, outcome) = match join_result {
        Ok((Ok(result), ticket)) => match query_result_to_message(result) {
            Ok(message) => (message, ticket, QueryOutcome::Ok),
            Err(e) => (
                Message::Error {
                    message: sanitize_error(&e.to_string()),
                },
                ticket,
                QueryOutcome::Error,
            ),
        },
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
                Message::Error {
                    message: sanitize_error(&e.to_string()),
                },
                ticket,
                outcome,
            )
        }
        Err(e) => (
            Message::Error {
                message: format!("internal error: {e}"),
            },
            None,
            QueryOutcome::Error,
        ),
    };
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
        ),
        None => {
            if exceeded_timeout {
                metrics.record_query(start.elapsed(), QueryOutcome::Timeout);
            } else {
                metrics.record_query(start.elapsed(), outcome);
            }
            (message, None)
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
            | Message::ResultOk { .. }
            | Message::ResultMessage { .. }
    )
}

fn rollback_open_transaction(engine: Arc<RwLock<Engine>>, principal: Option<Principal>) {
    let (res, ticket) = dispatch_query(&engine, "rollback", principal.as_ref());
    let _ = res;
    // Rollback takes the sync-preserving path (no ticket), but settle one
    // defensively if it ever appears so the durability watermark stays honest.
    if let Some(ticket) = ticket {
        let _ = ticket.wait();
    }
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
                    let err = Message::Error {
                        message: "too many auth failures, try again later".into(),
                    };
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
                    let err = Message::Error {
                        message: "authentication failed".into(),
                    };
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
                    let err = Message::Error { message: msg };
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
            let err = Message::Error {
                message: "expected CONNECT".into(),
            };
            write_msg(&mut writer, &err).await;
            return;
        }
    }

    let mut tx_permit: Option<OwnedSemaphorePermit> = None;
    // A non-query frame decoded during read-ahead batching, carried over to
    // the next iteration of the main loop.
    let mut carry: Option<Message> = None;

    // Main query loop with idle timeout and shutdown awareness.
    'conn: loop {
        let msg = if let Some(m) = carry.take() {
            m
        } else {
            tokio::select! {
                // Read next message with idle timeout.
                result = tokio::time::timeout(idle_timeout, Message::read_from(&mut reader)) => {
                    match result {
                        Ok(Ok(Some(msg))) => msg,
                        Ok(Ok(None)) => break,
                        Ok(Err(e)) => {
                            error!(peer = %peer, error = %e, "read error");
                            break;
                        }
                        Err(_) => {
                            info!(peer = %peer, "idle timeout, closing connection");
                            let err = Message::Error { message: "idle timeout".into() };
                            write_msg(&mut writer, &err).await;
                            break;
                        }
                    }
                }
                // If server is shutting down, notify client and close.
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!(peer = %peer, "server shutting down, closing connection");
                        let err = Message::Error { message: "server shutting down".into() };
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
            Message::Query { .. } | Message::QuerySql { .. } | Message::QueryWithParams { .. }
        ) {
            /// Read-ahead cap per batch: bounds unflushed responses and keeps
            /// the reply latency of the first statement bounded.
            const MAX_PIPELINE_BATCH: usize = 128;
            /// Byte cap on retained (unflushed) response payloads per batch:
            /// large row results stop read-ahead, so one connection can never
            /// hold gigabytes of replies hostage to the batch's durability
            /// wait.
            const MAX_PIPELINE_BATCH_BYTES: usize = 4 << 20;

            /// How the read-ahead loop stopped, when it stopped the whole
            /// connection rather than just the batch.
            enum BatchFatal {
                Closed,
                ReadError,
            }

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
                    Message::ResultMessage { message } | Message::Error { message } => {
                        message.len()
                    }
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
            let mut fatal: Option<BatchFatal> = None;
            let mut current = msg;
            loop {
                let (response, ticket) = match current {
                    Message::Query { query } => {
                        if query.len() > MAX_QUERY_LENGTH {
                            (
                                Message::Error {
                                    message: format!(
                                        "query too large: {} bytes (max {})",
                                        query.len(),
                                        MAX_QUERY_LENGTH
                                    ),
                                },
                                None,
                            )
                        } else {
                            debug!(peer = %peer, query = %query, "received query");
                            execute_wire_query(
                                engine.clone(),
                                tx_gate.clone(),
                                &mut tx_permit,
                                query,
                                principal.clone(),
                                query_timeout,
                                tx_wait_timeout,
                                &metrics,
                            )
                            .await
                        }
                    }
                    Message::QuerySql { query } => {
                        if query.len() > MAX_QUERY_LENGTH {
                            (
                                Message::Error {
                                    message: format!(
                                        "query too large: {} bytes (max {})",
                                        query.len(),
                                        MAX_QUERY_LENGTH
                                    ),
                                },
                                None,
                            )
                        } else {
                            debug!(peer = %peer, query = %query, "received SQL query");
                            execute_wire_query_sql(
                                engine.clone(),
                                tx_gate.clone(),
                                &mut tx_permit,
                                query,
                                principal.clone(),
                                query_timeout,
                                tx_wait_timeout,
                                &metrics,
                            )
                            .await
                        }
                    }
                    Message::QueryWithParams { query, params } => {
                        if query.len() > MAX_QUERY_LENGTH {
                            (
                                Message::Error {
                                    message: format!(
                                        "query too large: {} bytes (max {})",
                                        query.len(),
                                        MAX_QUERY_LENGTH
                                    ),
                                },
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
                                principal.clone(),
                                query_timeout,
                                tx_wait_timeout,
                                &metrics,
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

                // Read ahead only when a COMPLETE next frame is already
                // buffered (never await the socket mid-batch) and the
                // retained replies stay small. While an explicit transaction
                // is open the connection holds the TxGate, so batching would
                // only extend the exclusive window — flush instead.
                if tx_permit.is_some()
                    || responses.len() >= MAX_PIPELINE_BATCH
                    || response_bytes >= MAX_PIPELINE_BATCH_BYTES
                    || !complete_frame_buffered(reader.buffer())
                {
                    break;
                }
                // The full frame is buffered, so this returns without socket
                // I/O; the timeout is a defensive backstop only.
                match tokio::time::timeout(idle_timeout, Message::read_from(&mut reader)).await {
                    Ok(Ok(Some(
                        next @ (Message::Query { .. }
                        | Message::QuerySql { .. }
                        | Message::QueryWithParams { .. }),
                    ))) => {
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
                    Ok(Ok(Some(other))) => {
                        // Not a plain query — flush this batch, then handle
                        // the frame on the next main-loop iteration.
                        carry = Some(other);
                        break;
                    }
                    Ok(Ok(None)) => {
                        fatal = Some(BatchFatal::Closed);
                        break;
                    }
                    Ok(Err(e)) => {
                        error!(peer = %peer, error = %e, "read error");
                        fatal = Some(BatchFatal::ReadError);
                        break;
                    }
                    Err(_) => {
                        // Unreachable in practice: the frame was fully
                        // buffered, so read_from needs no socket I/O.
                        error!(peer = %peer, "timeout decoding fully-buffered frame");
                        fatal = Some(BatchFatal::ReadError);
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
                            *r = Message::Error {
                                message: message.clone(),
                            };
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
                Some(BatchFatal::Closed | BatchFatal::ReadError) => break,
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
            _ => Message::Error {
                message: "unexpected message type".into(),
            },
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

fn query_result_to_message(result: QueryResult) -> Result<Message, QueryError> {
    match result {
        QueryResult::Rows { columns, rows } => {
            let mut encoded_bytes = 2usize; // column count
            let mut out_columns = Vec::with_capacity(columns.len());
            for col in columns {
                charge_response_bytes(&mut encoded_bytes, 4 + col.len())?;
                out_columns.push(col);
            }
            charge_response_bytes(&mut encoded_bytes, 4)?; // row count

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
                columns: out_columns,
                rows: str_rows,
            })
        }
        QueryResult::Scalar(val) => Ok(Message::ResultScalar {
            value: value_to_display(&val),
        }),
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
        match query_result_to_message(result).expect("encodes") {
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
        let msg = query_result_to_message(result).expect("encodes");
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
        assert_eq!(gate.available_permits(), 1, "permit must release on drop");
    }

    #[tokio::test]
    async fn begin_permit_times_out_with_clear_error_and_truthful_metric() {
        let gate = new_tx_gate();
        let metrics = Arc::new(Metrics::new());
        // Hold the only permit so the next acquire must wait, then time out.
        let _held = gate.clone().acquire_owned().await.unwrap();
        let err = acquire_begin_permit(&gate, Duration::from_millis(25), &metrics)
            .await
            .expect_err("must time out while the gate is held");
        match err {
            Message::Error { message } => {
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
        let err = query_result_to_message(result).unwrap_err();
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
