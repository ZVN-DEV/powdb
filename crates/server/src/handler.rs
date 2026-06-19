use crate::metrics::{Metrics, QueryOutcome};
use crate::protocol::{Message, WireParam};
use powdb_auth::{Permission, Role, UserStore};
use powdb_query::executor::{is_read_only_statement, Engine};
use powdb_query::parser;
use powdb_query::result::{QueryError, QueryResult};
use powdb_query::sql;
use powdb_storage::types::Value;
use std::collections::HashMap;
use std::net::IpAddr;
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
/// on every query by [`dispatch_query`] to enforce the user's role: a
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

/// Error messages that are safe to forward to the client verbatim.
const SAFE_ERROR_PREFIXES: &[&str] = &[
    "table not found",
    "column not found",
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
}

/// Execute a query against the engine under the RwLock. Read-only
/// statements acquire `.read()` so concurrent SELECTs can scan in
/// parallel; mutations acquire `.write()`.
///
/// When `principal` is `Some`, the user's role is enforced first: a role
/// without the `Write` permission (i.e. `readonly`) gets a clean
/// "permission denied" error for any non-read statement, before any lock
/// is taken or any engine state is touched.
fn dispatch_query(
    engine: &Arc<RwLock<Engine>>,
    query: &str,
    principal: Option<&Principal>,
) -> Result<QueryResult, QueryError> {
    let stmt_result = parser::parse(query).map_err(|e| e.to_string());

    // Role enforcement happens on the parsed AST. Statements that fail to
    // parse fall through — the engine returns the parse error itself and
    // can never execute anything for them.
    if let Ok(stmt) = &stmt_result {
        check_statement_permitted(principal, stmt)?;
    }

    let can_try_read = matches!(&stmt_result, Ok(s) if is_read_only_statement(s));
    if can_try_read {
        let res = {
            let eng = engine
                .read()
                .map_err(|e| QueryError::Execution(format!("lock poisoned: {e}")))?;
            eng.execute_powql_readonly(query)
        };
        match res {
            Ok(r) => return Ok(r),
            Err(QueryError::ReadonlyNeedsWrite) => {
                // Escalate: fall through to the write path below.
            }
            Err(e) => return Err(e),
        }
    }

    let mut eng = engine
        .write()
        .map_err(|e| QueryError::Execution(format!("lock poisoned: {e}")))?;
    eng.execute_powql(query)
}

fn dispatch_sql_query(
    engine: &Arc<RwLock<Engine>>,
    query: &str,
    principal: Option<&Principal>,
) -> Result<QueryResult, QueryError> {
    let stmt_result = sql::parse_sql(query).map_err(|e| e.to_string());

    if let Ok(stmt) = &stmt_result {
        check_statement_permitted(principal, stmt)?;
    }

    let can_try_read = matches!(&stmt_result, Ok(s) if is_read_only_statement(s));
    if can_try_read {
        let res = {
            let eng = engine
                .read()
                .map_err(|e| QueryError::Execution(format!("lock poisoned: {e}")))?;
            eng.execute_sql_readonly(query)
        };
        match res {
            Ok(r) => return Ok(r),
            Err(QueryError::ReadonlyNeedsWrite) => {}
            Err(e) => return Err(e),
        }
    }

    let mut eng = engine
        .write()
        .map_err(|e| QueryError::Execution(format!("lock poisoned: {e}")))?;
    eng.execute_sql(query)
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

fn dispatch_query_with_params(
    engine: &Arc<RwLock<Engine>>,
    query: &str,
    params: &[WireParam],
    principal: Option<&Principal>,
) -> Result<QueryResult, QueryError> {
    let bound: Vec<powdb_query::ast::ParamValue> = params.iter().map(wire_param_to_value).collect();

    // Parse once (with params bound) so role enforcement and read/write
    // classification see exactly the statement that will execute.
    let stmt_result = parser::parse_with_params(query, &bound).map_err(|e| e.to_string());

    if let Ok(stmt) = &stmt_result {
        check_statement_permitted(principal, stmt)?;
    }

    let can_try_read = matches!(&stmt_result, Ok(s) if is_read_only_statement(s));
    if can_try_read {
        let res = {
            let eng = engine
                .read()
                .map_err(|e| QueryError::Execution(format!("lock poisoned: {e}")))?;
            eng.execute_powql_readonly_with_params(query, &bound)
        };
        match res {
            Ok(r) => return Ok(r),
            Err(QueryError::ReadonlyNeedsWrite) => {
                // Escalate to the write path below.
            }
            Err(e) => return Err(e),
        }
    }

    let mut eng = engine
        .write()
        .map_err(|e| QueryError::Execution(format!("lock poisoned: {e}")))?;
    eng.execute_powql_with_params(query, &bound)
}

async fn execute_wire_query(
    engine: Arc<RwLock<Engine>>,
    tx_gate: TxGate,
    tx_permit: &mut Option<OwnedSemaphorePermit>,
    query: String,
    principal: Option<Principal>,
    query_timeout: Duration,
    metrics: &Arc<Metrics>,
) -> Message {
    match classify_query_transaction_control(&query) {
        Some(TransactionControl::Begin) => {
            if tx_permit.is_some() {
                return Message::Error {
                    message: sanitize_error("transaction already active"),
                };
            }
            let permit = match tx_gate.acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    return Message::Error {
                        message: "query execution error".into(),
                    }
                }
            };
            let response = run_blocking_query(
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
            response
        }
        Some(TransactionControl::Commit | TransactionControl::Rollback) => {
            let response = run_blocking_query(
                engine,
                query,
                principal,
                query_timeout,
                metrics,
                |engine, query, principal| dispatch_query(&engine, &query, principal.as_ref()),
            )
            .await;
            if is_success_response(&response) {
                tx_permit.take();
            }
            response
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
            let permit = match tx_gate.acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    return Message::Error {
                        message: "query execution error".into(),
                    }
                }
            };
            let response = run_blocking_query(
                engine,
                query,
                principal,
                query_timeout,
                metrics,
                |engine, query, principal| dispatch_query(&engine, &query, principal.as_ref()),
            )
            .await;
            drop(permit);
            response
        }
    }
}

async fn execute_wire_query_sql(
    engine: Arc<RwLock<Engine>>,
    tx_gate: TxGate,
    tx_permit: &mut Option<OwnedSemaphorePermit>,
    query: String,
    principal: Option<Principal>,
    query_timeout: Duration,
    metrics: &Arc<Metrics>,
) -> Message {
    match classify_sql_transaction_control(&query) {
        Some(TransactionControl::Begin) => {
            if tx_permit.is_some() {
                return Message::Error {
                    message: sanitize_error("transaction already active"),
                };
            }
            let permit = match tx_gate.acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    return Message::Error {
                        message: "query execution error".into(),
                    }
                }
            };
            let response = run_blocking_query(
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
            response
        }
        Some(TransactionControl::Commit | TransactionControl::Rollback) => {
            let response = run_blocking_query(
                engine,
                query,
                principal,
                query_timeout,
                metrics,
                |engine, query, principal| dispatch_sql_query(&engine, &query, principal.as_ref()),
            )
            .await;
            if is_success_response(&response) {
                tx_permit.take();
            }
            response
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
            let permit = match tx_gate.acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    return Message::Error {
                        message: "query execution error".into(),
                    }
                }
            };
            let response = run_blocking_query(
                engine,
                query,
                principal,
                query_timeout,
                metrics,
                |engine, query, principal| dispatch_sql_query(&engine, &query, principal.as_ref()),
            )
            .await;
            drop(permit);
            response
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
    metrics: &Arc<Metrics>,
) -> Message {
    match classify_params_transaction_control(&query, &params) {
        Some(TransactionControl::Begin) => {
            if tx_permit.is_some() {
                return Message::Error {
                    message: sanitize_error("transaction already active"),
                };
            }
            let permit = match tx_gate.acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    return Message::Error {
                        message: "query execution error".into(),
                    }
                }
            };
            let response = run_blocking_query(
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
            response
        }
        Some(TransactionControl::Commit | TransactionControl::Rollback) => {
            let response = run_blocking_query(
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
                tx_permit.take();
            }
            response
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
            let permit = match tx_gate.acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    return Message::Error {
                        message: "query execution error".into(),
                    }
                }
            };
            let response = run_blocking_query(
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
            response
        }
    }
}

async fn run_blocking_query<T, F>(
    engine: Arc<RwLock<Engine>>,
    input: T,
    principal: Option<Principal>,
    query_timeout: Duration,
    metrics: &Arc<Metrics>,
    f: F,
) -> Message
where
    T: Send + 'static,
    F: FnOnce(Arc<RwLock<Engine>>, T, Option<Principal>) -> Result<QueryResult, QueryError>
        + Send
        + 'static,
{
    let _in_flight = metrics.in_flight_guard();
    let start = Instant::now();
    let mut handle = tokio::task::spawn_blocking(move || f(engine, input, principal));
    let mut exceeded_timeout = false;
    let join_result = tokio::select! {
        result = &mut handle => result,
        _ = tokio::time::sleep(query_timeout) => {
            exceeded_timeout = true;
            // `spawn_blocking` tasks that have started cannot be aborted safely.
            // Wait for completion before replying so a client never receives a
            // timeout while the same query keeps running and possibly mutating
            // state in the background.
            handle.await
        }
    };

    let (message, outcome) = match join_result {
        Ok(Ok(result)) => match query_result_to_message(result) {
            Ok(message) => (message, QueryOutcome::Ok),
            Err(e) => (
                Message::Error {
                    message: sanitize_error(&e.to_string()),
                },
                QueryOutcome::Error,
            ),
        },
        Ok(Err(e)) => {
            let outcome = if matches!(e, QueryError::MemoryLimitExceeded { .. }) {
                QueryOutcome::MemoryLimit
            } else {
                QueryOutcome::Error
            };
            (
                Message::Error {
                    message: sanitize_error(&e.to_string()),
                },
                outcome,
            )
        }
        Err(e) => (
            Message::Error {
                message: format!("internal error: {e}"),
            },
            QueryOutcome::Error,
        ),
    };
    if exceeded_timeout {
        metrics.record_query(start.elapsed(), QueryOutcome::Timeout);
    } else {
        metrics.record_query(start.elapsed(), outcome);
    }
    message
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
    let _ = dispatch_query(&engine, "rollback", principal.as_ref());
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

    // Main query loop with idle timeout and shutdown awareness.
    loop {
        let msg = tokio::select! {
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
        };

        let response = match msg {
            Message::Ping => {
                debug!(peer = %peer, "ping");
                Message::Pong
            }
            Message::Query { query } => {
                if query.len() > MAX_QUERY_LENGTH {
                    Message::Error {
                        message: format!(
                            "query too large: {} bytes (max {})",
                            query.len(),
                            MAX_QUERY_LENGTH
                        ),
                    }
                } else {
                    debug!(peer = %peer, query = %query, "received query");
                    let response = execute_wire_query(
                        engine.clone(),
                        tx_gate.clone(),
                        &mut tx_permit,
                        query.clone(),
                        principal.clone(),
                        query_timeout,
                        &metrics,
                    )
                    .await;
                    response
                }
            }
            Message::QuerySql { query } => {
                if query.len() > MAX_QUERY_LENGTH {
                    Message::Error {
                        message: format!(
                            "query too large: {} bytes (max {})",
                            query.len(),
                            MAX_QUERY_LENGTH
                        ),
                    }
                } else {
                    debug!(peer = %peer, query = %query, "received SQL query");
                    let response = execute_wire_query_sql(
                        engine.clone(),
                        tx_gate.clone(),
                        &mut tx_permit,
                        query.clone(),
                        principal.clone(),
                        query_timeout,
                        &metrics,
                    )
                    .await;
                    response
                }
            }
            Message::QueryWithParams { query, params } => {
                if query.len() > MAX_QUERY_LENGTH {
                    Message::Error {
                        message: format!(
                            "query too large: {} bytes (max {})",
                            query.len(),
                            MAX_QUERY_LENGTH
                        ),
                    }
                } else {
                    debug!(peer = %peer, query = %query, n_params = params.len(), "received parameterized query");
                    let response = execute_wire_query_with_params(
                        engine.clone(),
                        tx_gate.clone(),
                        &mut tx_permit,
                        query.clone(),
                        params.clone(),
                        principal.clone(),
                        query_timeout,
                        &metrics,
                    )
                    .await;
                    response
                }
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

fn value_to_display(v: &Value) -> String {
    match v {
        Value::Int(n)      => n.to_string(),
        Value::Float(n)    => format!("{n}"),
        Value::Bool(b)     => b.to_string(),
        Value::Str(s)      => s.clone(),
        Value::DateTime(t) => format!("{t}"),
        Value::Uuid(u)     => format!("{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            u[0], u[1], u[2], u[3], u[4], u[5], u[6], u[7],
            u[8], u[9], u[10], u[11], u[12], u[13], u[14], u[15]),
        Value::Bytes(b)    => format!("<{} bytes>", b.len()),
        // NULL is serialized as the bareword "null" on the wire. This is the
        // sentinel the TypeScript client's typed-row decoder already
        // documents and matches (`coerceValue` treats the exact token
        // "null" as NULL for non-str columns); the previous "{}" rendering
        // was a bug that neither the TS client nor the CLI recognized.
        Value::Empty       => "null".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
