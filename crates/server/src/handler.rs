use crate::metrics::{Metrics, QueryOutcome, SyncOperation, SyncOutcome, SyncRepairLabel};
use crate::protocol::{
    negotiate_protocol, stated_client_hello, ErrorClass, Message, WireParam, WireRetainedUnit,
    WireSyncRepairAction, WireSyncStatus, CLIENT_CATALOG_VERSION, MAX_SUPPORTED_PROTOCOL_VERSION,
    MIN_SUPPORTED_PROTOCOL_VERSION, SERVER_FEATURES,
};
use powdb_auth::{Permission, Role, UserStore};
use powdb_query::executor::{is_read_only_statement, Engine, WalDurabilityTicket};
use powdb_query::parser;
use powdb_query::result::{QueryError, QueryResult};
use powdb_query::sql;
use powdb_storage::error::StorageError;
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
///
/// The gate also carries the maximum time ONE connection may hold it inside an
/// explicit transaction. That bound lives here, not in [`ConnOpts`], because it
/// is a property of the gate being held rather than of a connection: every
/// listener (TCP, TLS, Unix socket) already clones the same gate into every
/// connection, so the bound reaches all of them without a per-listener wiring
/// step that a future frontend could forget.
#[derive(Clone)]
pub struct TxGate {
    semaphore: Arc<Semaphore>,
    permit_count: u32,
    max_tx_lifetime: Option<Duration>,
}

pub const DEFAULT_TX_GATE_READER_PERMITS: u32 = 1024;

/// Default ceiling on how long one connection may hold the gate inside an
/// explicit transaction before the server rolls it back. Matches the default
/// connection idle timeout, which is what used to bound a *silent* holder; the
/// lifetime bound is what bounds a NOISY one, because the idle deadline
/// re-arms on every frame and a bare PING is a frame.
pub const DEFAULT_TX_MAX_LIFETIME: Duration = Duration::from_secs(300);

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
    new_tx_gate_with_permits_and_max_tx_lifetime(permit_count, Some(DEFAULT_TX_MAX_LIFETIME))
}

/// Create a transaction gate with an explicit maximum transaction lifetime.
/// `None` disables the bound (the `POWDB_TX_MAX_LIFETIME_MS=0` opt-out), which
/// restores the pre-0.22 behavior where a client controlled the hold duration.
pub fn new_tx_gate_with_max_tx_lifetime(max_tx_lifetime: Option<Duration>) -> TxGate {
    new_tx_gate_with_permits_and_max_tx_lifetime(DEFAULT_TX_GATE_READER_PERMITS, max_tx_lifetime)
}

fn new_tx_gate_with_permits_and_max_tx_lifetime(
    permit_count: u32,
    max_tx_lifetime: Option<Duration>,
) -> TxGate {
    assert!(permit_count > 0, "transaction gate requires a permit");
    TxGate {
        semaphore: Arc::new(Semaphore::new(permit_count as usize)),
        permit_count,
        max_tx_lifetime,
    }
}

impl TxGate {
    pub fn permit_count(&self) -> u32 {
        self.permit_count
    }

    /// How long one connection may hold this gate inside an explicit
    /// transaction before the server rolls the transaction back and releases
    /// it. `None` means unbounded.
    pub fn max_tx_lifetime(&self) -> Option<Duration> {
        self.max_tx_lifetime
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
    // Entity-link diagnostics. Every one of these is derived from the client's
    // own statement plus catalog names the client just used, exactly like the
    // `table '...'` / `column not found` entries above, and every one names the
    // fix. Without these prefixes a remote client saw "query execution error"
    // for the whole link feature while an embedded caller saw the real message,
    // so a driver could not tell a typo from a server fault.
    // Covers the catalog's `link '<name>' not found on owner type '<T>'`,
    // `link '<name>' already exists on owner type '<T>'`, `link local key ...`,
    // `link target key ...`, `link name '<name>' collides with a column ...`,
    // the planner's `link path starts at unknown alias ...`, and the executor's
    // `link traversal requires ...`.
    "link ",
    "links ",
    // The executor's own phrasing for a link that was never declared
    // (`unknown link `x` on type `T``), which the bare `unknown table` /
    // `unknown column` entries above never matched.
    "unknown link",
    // The planner's correct-by-default refusal of an aggregate over a nested or
    // link projection: it names the statement the client sent and the rewrite
    // that works. See crates/query/src/planner.rs.
    "aggregates over",
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
        // Storage refusals arrive here already rendered to text: the query
        // crate stores them as `QueryError::StorageError(e.to_string())`, so
        // the variant is gone. Recognize the ones the client can act on, using
        // the predicates that live beside the messages they read
        // (crates/storage/src/error.rs), so each gets the class docs/errors.md
        // promises instead of collapsing to Internal ("server bug"), which
        // tells a driver there is nothing to fix on its side.
        QueryError::StorageError(err) => {
            if err.contains("unique constraint violation") {
                ErrorClass::ConstraintViolation
            } else if StorageError::is_transaction_too_large_message(err) {
                ErrorClass::LimitExceeded
            } else if StorageError::is_ddl_in_transaction_message(err) {
                ErrorClass::Execution
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

/// Log a received query with every literal value redacted.
///
/// Query text is user data: `filter .email = "ada@example.com"` would otherwise
/// put a real address into a log that is shipped and retained. We log the
/// literal-free shape plus the plan-cache canonical hash, which is enough to
/// identify the query, correlate repeats, and match it to a cached plan, and
/// carries no values. See [`crate::redact`].
fn log_received_query(peer: &str, query: &str, message: &'static str) {
    debug!(
        peer = %peer,
        query_shape = %crate::redact::redact_query_literals(query),
        query_hash = ?crate::redact::query_shape_hash(query),
        query_len = query.len(),
        message
    );
}

/// [`log_received_query`] for the parameterized paths, where the bound values
/// travel outside the text and are never logged at all.
fn log_received_query_with_params(peer: &str, query: &str, n_params: usize, message: &'static str) {
    debug!(
        peer = %peer,
        query_shape = %crate::redact::redact_query_literals(query),
        query_hash = ?crate::redact::query_shape_hash(query),
        query_len = query.len(),
        n_params,
        message
    );
}

/// Write a message to the client with a timeout. Returns false if the
/// write failed or timed out (caller should close the connection).
async fn write_msg<W: AsyncWrite + Unpin>(writer: &mut BufWriter<W>, msg: &Message) -> bool {
    write_msg_with_budget(writer, msg, WRITE_TIMEOUT).await
}

/// How long a reply to this connection may block, given the transaction
/// deadline (if any) it is being written under.
///
/// [`WRITE_TIMEOUT`] alone is not enough while a transaction holds the gate.
/// The reap only runs between frames, so a client that opens a transaction,
/// asks for a reply larger than the socket buffers, and then stops reading
/// parks the handler inside the write for the full `WRITE_TIMEOUT` with the
/// gate still held. That is the same outage `POWDB_TX_MAX_LIFETIME_MS` exists
/// to prevent, reached through the write side instead of the read side, and it
/// made the advertised budget false by 30s/budget (100x at the default). The
/// remaining lifetime therefore caps the write budget too, so the gate is
/// released on the budget the operator configured no matter which side of the
/// socket the client stalls.
fn write_budget(tx_deadline: Option<Instant>) -> Duration {
    match tx_deadline {
        Some(deadline) => WRITE_TIMEOUT.min(deadline.saturating_duration_since(Instant::now())),
        None => WRITE_TIMEOUT,
    }
}

/// [`write_msg`] bounded by an explicit budget.
async fn write_msg_with_budget<W: AsyncWrite + Unpin>(
    writer: &mut BufWriter<W>,
    msg: &Message,
    budget: Duration,
) -> bool {
    let write_fut = async {
        if msg.write_to(writer).await.is_err() {
            return false;
        }
        writer.flush().await.is_ok()
    };
    tokio::time::timeout(budget, write_fut)
        .await
        .unwrap_or_default()
}

/// How long the connection may spend flushing what is left in its buffer on
/// the way out. Long enough for a client that is merely slow, short enough
/// that a client which stopped reading cannot park the teardown.
const FINAL_FLUSH_BUDGET: Duration = Duration::from_millis(250);

/// Push whatever is still buffered to the socket before the connection ends.
///
/// `BufWriter` has no `Drop` that flushes, so a frame that was written into it
/// but not yet drained is discarded when the connection object goes away. Every
/// reply path flushes on its own, but a flush that is cancelled by its budget
/// leaves the remainder in the buffer, and that remainder is the tail of a
/// frame the client is waiting on. Returns whether the buffer actually drained.
async fn flush_before_close<W: AsyncWrite + Unpin>(writer: &mut BufWriter<W>) -> bool {
    tokio::time::timeout(FINAL_FLUSH_BUDGET, writer.flush())
        .await
        .is_ok_and(|result| result.is_ok())
}

/// [`write_msg`] for replies sent while this connection may be holding the
/// transaction gate: the write can never outlast the transaction's remaining
/// lifetime. See [`write_budget`].
async fn write_msg_within<W: AsyncWrite + Unpin>(
    writer: &mut BufWriter<W>,
    msg: &Message,
    tx_deadline: Option<Instant>,
) -> bool {
    write_msg_with_budget(writer, msg, write_budget(tx_deadline)).await
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
    GateTimeout,
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
    /// The primary refused to advance this replica's cursor for a reason the
    /// replica itself must act on: no cursor, an inactive cursor, or an LSN
    /// behind the one already recorded. Sibling of
    /// [`SyncErrorClass::IdentityOrFormatMismatch`], and classified the same
    /// way, so the two "rebootstrap required" answers a replica has to branch
    /// on cannot arrive with different wire classes.
    AckRejected,
    /// The cursor update failed for a reason on the server's side (an I/O
    /// error). Not actionable by the replica.
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
            Self::GateTimeout => "gate_timeout",
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
            Self::AckRejected => "ack_rejected",
            Self::AckUpdate => "ack_update",
            Self::Internal => "internal",
        }
    }

    /// The stable wire [`ErrorClass`] this sync rejection is reported as.
    ///
    /// Sync errors used to reach the wire as a bare `MSG_ERROR` with no class
    /// byte at all, which is the one thing this protocol promises never to do:
    /// a driver could tell a timeout from an auth failure on every query
    /// frontend but not on this one. The match is exhaustive, so a new
    /// [`SyncErrorClass`] cannot be added without choosing a class, and no arm
    /// falls back to [`ErrorClass::Internal`] just to compile.
    ///
    /// Classes are the ones the query frontends already use for the same
    /// meaning, so a client branches identically whichever frontend answered:
    /// a role refusal is `Execution` because `check_statement_permitted`
    /// reports its "permission denied" that way, and a gate wait is `Timeout`
    /// because `acquire_begin_permit` reports its wait that way.
    const fn wire_class(self) -> ErrorClass {
        match self {
            // Fixable by reconnecting with credentials.
            Self::AuthRequired => ErrorClass::AuthFailed,
            // Not fixable by re-authenticating as the same principal: parity
            // with the query frontends' role refusals.
            Self::PermissionDenied => ErrorClass::Execution,
            // The client's own request field is wrong and it can say so.
            Self::InvalidReplicaId
            | Self::ActiveTransaction
            | Self::CursorLsnMismatch
            | Self::IdentityOrFormatMismatch
            | Self::RetainedChunkNotApplyable
            | Self::LsnAheadOfRemote
            | Self::AckValidation
            // A refused cursor advance is the ack-side twin of
            // `IdentityOrFormatMismatch`: both tell the replica to rebootstrap,
            // so both must arrive as the same class. Reporting this one as
            // `Internal` made it indistinguishable from an unclassified server
            // fault, which is the one answer a replica cannot act on.
            | Self::AckRejected => ErrorClass::Execution,
            // A caller-supplied bound outside the server's accepted range.
            Self::InvalidMaxUnits | Self::InvalidMaxBytes => ErrorClass::LimitExceeded,
            // The gate wait elapsed; retryable, like every other time budget.
            Self::GateTimeout => ErrorClass::Timeout,
            // Server-side failures the client cannot act on.
            Self::QueryExecution
            | Self::SyncContext
            | Self::StatusRead
            | Self::IdentityRead
            | Self::RetainedRead
            | Self::RetainedUnitEncoding
            | Self::AckUpdate
            | Self::Internal => ErrorClass::Internal,
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
            message: error_response(message, class.wire_class()),
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

/// Range check for a sync pull's caller-supplied batch bounds. Pure: it needs
/// no engine access, so it belongs to [`SyncPreGate`], which is the ONLY place
/// it runs. [`dispatch_sync_pull_decision`] reaches it through the same
/// pre-gate rather than repeating it.
fn check_sync_pull_bounds(max_units: u32, max_bytes: u64) -> Result<(), (SyncErrorClass, String)> {
    if max_units == 0 || max_units > MAX_SYNC_PULL_UNITS {
        return Err((
            SyncErrorClass::InvalidMaxUnits,
            format!("sync pull maxUnits must be between 1 and {MAX_SYNC_PULL_UNITS}"),
        ));
    }
    if max_bytes == 0 || max_bytes > MAX_SYNC_PULL_BYTES {
        return Err((
            SyncErrorClass::InvalidMaxBytes,
            format!("sync pull maxBytes must be between 1 and {MAX_SYNC_PULL_BYTES}"),
        ));
    }
    Ok(())
}

/// The half of the sync ack LSN validation that compares only the two numbers
/// the client sent. The remaining checks need the primary's LSN and therefore
/// stay inside [`dispatch_sync_ack_decision`], under the gate.
fn check_sync_ack_lsn_bounds(
    applied_lsn: u64,
    observed_remote_lsn: u64,
) -> Result<(), (SyncErrorClass, String)> {
    if applied_lsn > observed_remote_lsn {
        return Err((
            SyncErrorClass::LsnAheadOfRemote,
            format!(
                "sync ack appliedLsn {applied_lsn} is ahead of observed remoteLsn {observed_remote_lsn}"
            ),
        ));
    }
    Ok(())
}

/// Everything a sync frame can be rejected for WITHOUT touching the engine,
/// carried per sync message type.
///
/// This exists so [`execute_gated_sync`] can apply the rule the three query
/// frontends already follow ("a statement that executes nothing must acquire
/// nothing", see [`parse_failure_response`]) to the sync frontend. Before it,
/// every one of these rejections ran *after* `acquire_many_owned(all permits)`
/// inside the blocking decision function, so an unauthenticated peer sending a
/// frame that would be refused in microseconds still seized the entire gate
/// first.
///
/// A sync message type reaches the gate only by building one of these
/// variants, and [`SyncPreGate::check`] matches exhaustively, so a fourth sync
/// frame cannot be added without deciding what it may be rejected for
/// pre-gate.
enum SyncPreGate {
    Status {
        replica_id: String,
    },
    Pull {
        replica_id: String,
        max_units: u32,
        max_bytes: u64,
    },
    Ack {
        replica_id: String,
        applied_lsn: u64,
        observed_remote_lsn: u64,
    },
}

impl SyncPreGate {
    fn check(
        &self,
        credential_authenticated: bool,
        principal: Option<&Principal>,
    ) -> Result<(), (SyncErrorClass, String)> {
        check_sync_protocol_permitted(credential_authenticated, principal)?;
        let replica_id = match self {
            Self::Status { replica_id }
            | Self::Pull { replica_id, .. }
            | Self::Ack { replica_id, .. } => replica_id,
        };
        validate_wire_replica_id(replica_id)
            .map_err(|message| (SyncErrorClass::InvalidReplicaId, message))?;
        match self {
            Self::Status { .. } => Ok(()),
            Self::Pull {
                max_units,
                max_bytes,
                ..
            } => check_sync_pull_bounds(*max_units, *max_bytes),
            Self::Ack {
                applied_lsn,
                observed_remote_lsn,
                ..
            } => check_sync_ack_lsn_bounds(*applied_lsn, *observed_remote_lsn),
        }
    }
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
    tx_wait_timeout: Duration,
    credential_authenticated: bool,
    principal: Option<Principal>,
    pre_gate: SyncPreGate,
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
        Message::Error { .. } | Message::ErrorWithClass { .. } => {
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
    // Everything decidable without the engine goes through the SAME pre-gate
    // the wire path applies before taking a permit, so the two can never
    // disagree about what is refusable for free. See
    // `no_sync_dispatch_function_refuses_a_frame_before_it_reads_the_engine`.
    let pre_gate = SyncPreGate::Status {
        replica_id: replica_id.clone(),
    };
    if let Err((class, message)) = pre_gate.check(credential_authenticated, principal) {
        return SyncDecision::error(class, message);
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
    // See the note in `dispatch_sync_status_decision`: one pre-gate, shared.
    let pre_gate = SyncPreGate::Pull {
        replica_id: request.replica_id.clone(),
        max_units: request.max_units,
        max_bytes: request.max_bytes,
    };
    if let Err((class, message)) = pre_gate.check(credential_authenticated, principal) {
        return SyncDecision::error(class, message);
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
    // See the note in `dispatch_sync_status_decision`: one pre-gate, shared.
    let pre_gate = SyncPreGate::Ack {
        replica_id: replica_id.clone(),
        applied_lsn,
        observed_remote_lsn,
    };
    if let Err((class, message)) = pre_gate.check(credential_authenticated, principal) {
        return SyncDecision::error(class, message);
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
        Err(err) => SyncDecision::error(classify_sync_ack_failure(&err), err.to_string()),
    }
}

/// Split a failed cursor advance into "the replica must act" and "the server
/// failed". `acknowledge_replica_apply` rejects a missing cursor with
/// `NotFound` and an inactive or behind cursor with `InvalidInput`; every other
/// kind is a real I/O failure the replica cannot do anything about.
fn classify_sync_ack_failure(err: &std::io::Error) -> SyncErrorClass {
    match err.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput => {
            SyncErrorClass::AckRejected
        }
        _ => SyncErrorClass::AckUpdate,
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
        tx_wait_timeout,
        credential_authenticated,
        principal,
        pre_gate,
    } = context;
    let start = Instant::now();
    let reject = |decision: SyncDecision| -> Message {
        let elapsed = start.elapsed();
        log_sync_decision(operation, &log_context, elapsed, &decision);
        metrics.record_sync_operation(operation, elapsed, SyncOutcome::Error);
        decision.message
    };

    if connection_has_transaction {
        return reject(SyncDecision::error(
            SyncErrorClass::ActiveTransaction,
            "sync protocol is unavailable inside an active transaction",
        ));
    }
    // Every rejection that needs no engine access happens HERE, before the gate
    // is touched: a frame that will execute nothing must acquire nothing. The
    // decision functions run the SAME pre-gate as their first act, so a
    // non-wire caller gets the identical answer and the two can never drift
    // (`no_sync_dispatch_function_refuses_a_frame_before_it_reads_the_engine`).
    if let Err((class, message)) = pre_gate.check(credential_authenticated, principal.as_ref()) {
        return reject(SyncDecision::error(class, message));
    }

    let permit = match acquire_sync_permit(&tx_gate, tx_wait_timeout, metrics).await {
        Ok(permit) => permit,
        Err(decision) => return reject(decision),
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

/// Acquire the whole TxGate for a sync frame, bounded by `tx_wait_timeout`
/// exactly like [`acquire_begin_permit`] and [`acquire_autocommit_permit`].
///
/// This was the only unbounded gate acquire in the server. A sync frame that
/// arrived while another connection held an explicit transaction waited out
/// that connection's ENTIRE hold: it blew straight through `tx_wait_timeout`,
/// wrote no error frame at all, and was invisible to
/// `powdb_tx_gate_timeouts_total` because only the two query-side acquires
/// recorded a timeout. Bounding it here makes every gate waiter, on every
/// frontend, give up on the same deadline with the same typed error.
async fn acquire_sync_permit(
    tx_gate: &TxGate,
    tx_wait_timeout: Duration,
    metrics: &Arc<Metrics>,
) -> Result<OwnedSemaphorePermit, SyncDecision> {
    match tokio::time::timeout(
        tx_wait_timeout,
        tx_gate.clone().acquire_many_owned(tx_gate.permit_count()),
    )
    .await
    {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err(SyncDecision::error(
            SyncErrorClass::QueryExecution,
            "query execution error",
        )),
        Err(_) => {
            metrics.inc_tx_gate_timeout();
            Err(SyncDecision::error(
                SyncErrorClass::GateTimeout,
                format!(
                    "transaction gate timeout after {}ms waiting for concurrent transaction to complete",
                    tx_wait_timeout.as_millis()
                ),
            ))
        }
    }
}

/// Reject a frame whose text failed to parse, before the TxGate is touched.
///
/// A statement that executes nothing must acquire nothing. Admission is
/// derived from the parsed AST and fails closed to [`AdmissionMode::Writer`],
/// so routing an unparsable frame into the state machine made it queue for
/// every gate permit (and then the engine write lock) only to have the engine
/// return the same parse error: any principal, including a readonly role,
/// could hold the whole gate by looping on garbage. The error is still
/// counted so `powdb_queries_total{result="error"}` stays truthful.
fn parse_failure_response(
    message: &str,
    metrics: &Arc<Metrics>,
    start: Instant,
) -> (
    Message,
    Option<PendingDurability>,
    Option<ConnectionTermination>,
) {
    metrics.record_query(start.elapsed(), QueryOutcome::Error);
    (
        error_response(sanitize_error(message), ErrorClass::Parse),
        None,
        None,
    )
}

/// Reject a frame the principal's role may not run, before the TxGate is
/// touched.
///
/// Same rule as [`parse_failure_response`], and the same gap it closed: a
/// PARSEABLE write from a readonly role took full writer admission (the whole
/// gate) and queued for the engine write lock before the dispatcher reached
/// [`check_statement_permitted`] and refused it. That variant needs no garbage
/// at all, just `insert T { ... }` from a role without `Write`. Denying here
/// makes the permit acquisition unreachable for a statement that will never
/// execute; the dispatcher still re-checks, so the boundary stays enforced for
/// every non-wire caller.
fn permission_denied_response(
    denied: &QueryError,
    metrics: &Arc<Metrics>,
    start: Instant,
) -> (
    Message,
    Option<PendingDurability>,
    Option<ConnectionTermination>,
) {
    metrics.record_query(start.elapsed(), QueryOutcome::Error);
    (
        error_response(
            sanitize_error(&denied.to_string()),
            classify_query_error(denied),
        ),
        None,
        None,
    )
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
    let start = Instant::now();
    let query_deadline = start + query_timeout;
    // Parse each frame once for transaction routing, admission, and role
    // enforcement. The engine still canonicalizes/parses as needed for plan
    // cache execution, but the server no longer repeats the same parse in
    // three separate routing helpers before reaching it.
    let stmt_result = parser::parse(&query).map_err(|e| e.to_string());
    match &stmt_result {
        Err(message) => return parse_failure_response(message, metrics, start),
        Ok(stmt) => {
            if let Err(denied) = check_statement_permitted(principal.as_ref(), stmt) {
                return permission_denied_response(&denied, metrics, start);
            }
        }
    }
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
    let start = Instant::now();
    let query_deadline = start + query_timeout;
    let stmt_result = sql::parse_sql(&query).map_err(|e| e.to_string());
    match &stmt_result {
        Err(message) => return parse_failure_response(message, metrics, start),
        Ok(stmt) => {
            if let Err(denied) = check_statement_permitted(principal.as_ref(), stmt) {
                return permission_denied_response(&denied, metrics, start);
            }
        }
    }
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
    let start = Instant::now();
    let query_deadline = start + query_timeout;
    let bound: Vec<powdb_query::ast::ParamValue> = params.iter().map(wire_param_to_value).collect();
    let stmt_result = parser::parse_with_params(&query, &bound).map_err(|e| e.to_string());
    match &stmt_result {
        Err(message) => return parse_failure_response(message, metrics, start),
        Ok(stmt) => {
            if let Err(denied) = check_statement_permitted(principal.as_ref(), stmt) {
                return permission_denied_response(&denied, metrics, start);
            }
        }
    }
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

/// Keep the transaction-lifetime deadline in step with the gate permit itself.
///
/// The deadline is DERIVED from `tx_permit` rather than armed at each install
/// site on purpose. The permit is installed and released in seven places
/// (begin, commit/rollback, three cancellation rollbacks, the standalone
/// commit permit, disconnect teardown), and a bound that has to be re-armed at
/// each of them is exactly the partial application this bound exists to
/// prevent: miss one and that path silently becomes unbounded again. Reading
/// the permit's own presence cannot miss a site.
///
/// Called once per handled frame, so `begin; commit; begin` inside one
/// pipelined batch re-arms rather than carrying the first transaction's
/// deadline into the second.
fn sync_tx_deadline(
    tx_permit: &Option<OwnedSemaphorePermit>,
    tx_deadline: &mut Option<Instant>,
    max_tx_lifetime: Option<Duration>,
) {
    match (tx_permit.is_some(), tx_deadline.is_some()) {
        (true, false) => *tx_deadline = max_tx_lifetime.map(|max| Instant::now() + max),
        (false, true) => *tx_deadline = None,
        _ => {}
    }
}

/// Roll back a transaction that has held the gate for its whole permitted
/// lifetime, tell the client why, and release the gate.
///
/// Nothing else bounds this. The connection idle timeout is re-armed by every
/// frame the client sends and a bare `PING` is a frame, so a client that pings
/// once per idle period holds the entire writer gate forever while every other
/// connection, readers included, times out against it. Serializing readers
/// behind an explicit transaction is deliberate and documented; letting the
/// client choose how long that lasts is not.
///
/// On a connection whose last write completed, the reply is a typed
/// [`ErrorClass::Timeout`] naming the budget and the knob, never a silent
/// disconnect: the client's transaction is gone and it has to know that before
/// it sends the next statement. When the reap was triggered BY a write that
/// could not finish, there is nowhere to put that frame; see [`ReapNotice`].
#[allow(clippy::too_many_arguments)]
async fn reap_expired_transaction<W>(
    engine: &Arc<RwLock<Engine>>,
    principal: &Option<Principal>,
    tx_permit: &mut Option<OwnedSemaphorePermit>,
    tx_deadline: &mut Option<Instant>,
    max_tx_lifetime: Duration,
    writer: &mut BufWriter<W>,
    peer: &str,
    metrics: &Arc<Metrics>,
    notice: ReapNotice,
) where
    W: AsyncWrite + Unpin,
{
    warn!(
        peer = %peer,
        max_tx_lifetime_ms = max_tx_lifetime.as_millis(),
        notified = matches!(notice, ReapNotice::Speak(_)),
        "transaction exceeded its maximum lifetime; rolling back and releasing the transaction gate"
    );
    rollback_connection_transaction(engine.clone(), principal.clone(), tx_permit).await;
    *tx_deadline = None;
    metrics.inc_tx_reaped();
    let ReapNotice::Speak(budget) = notice else {
        return;
    };
    let err = error_response(
        format!(
            "transaction exceeded the maximum lifetime of {}ms and was rolled back; \
             raise POWDB_TX_MAX_LIFETIME_MS if transactions on this server legitimately run longer",
            max_tx_lifetime.as_millis()
        ),
        ErrorClass::Timeout,
    );
    write_msg_with_budget(writer, &err, budget).await;
}

/// Whether a reap may still tell the client what happened.
///
/// A frame may only be written on a frame boundary, and a reply write that
/// failed did not necessarily fail before touching the socket:
/// [`Message::write_to`] is one `write_all` of the encoded frame, and any
/// frame larger than the `BufWriter`'s buffer goes straight through to the
/// socket, so cancelling that write on the budget leaves a partial frame on
/// the wire with its length already announced. There is no resume and no
/// rollback for those bytes. Anything written next is read by the client as
/// the dead frame's payload: the notification does not arrive, and the bytes
/// that carry it corrupt the framing of a stream the client was about to see
/// closed anyway.
///
/// So the reap speaks only where speaking is possible. Everything else it does
/// (roll back, release the gate, log, count) happens either way, which is what
/// keeps the event operator-visible when the wire cannot be.
#[derive(Clone, Copy, Debug)]
enum ReapNotice {
    /// The last write on this connection completed, so the next byte written
    /// starts a frame. The budget bounds the attempt: the gate is already
    /// released, so a client that has stopped reading can no longer cost
    /// anyone else anything.
    Speak(Duration),
    /// A reply write failed partway. The stream is no longer framable; close
    /// it without writing anything else.
    Silence,
}

/// A reply write failed. If this connection was inside an explicit transaction
/// whose lifetime has run out, that is a REAP, not an ordinary write failure,
/// and it must be treated as one: rolled back, released, logged, and counted.
///
/// Without this the connection simply broke out of the loop and let the
/// disconnect teardown roll the transaction back with no log line and no
/// counter, so the one budget that was actually being enforced left no trace
/// anywhere an operator could see it. A write that failed for any other reason
/// falls through to the same teardown as before.
///
/// This reap is always [`ReapNotice::Silence`]: it exists BECAUSE a write
/// failed, and a write after a failed write cannot be framed. The log line and
/// `powdb_tx_reaped_total` are what make it visible instead.
#[allow(clippy::too_many_arguments)]
async fn reap_after_stalled_write<W>(
    engine: &Arc<RwLock<Engine>>,
    principal: &Option<Principal>,
    tx_permit: &mut Option<OwnedSemaphorePermit>,
    tx_deadline: &mut Option<Instant>,
    max_tx_lifetime: Option<Duration>,
    writer: &mut BufWriter<W>,
    peer: &str,
    metrics: &Arc<Metrics>,
) where
    W: AsyncWrite + Unpin,
{
    let (Some(deadline), Some(max)) = (*tx_deadline, max_tx_lifetime) else {
        return;
    };
    if Instant::now() < deadline {
        return;
    }
    reap_expired_transaction(
        engine,
        principal,
        tx_permit,
        tx_deadline,
        max,
        writer,
        peer,
        metrics,
        ReapNotice::Silence,
    )
    .await;
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

/// Serve one client connection, then close it.
///
/// The buffered writer is owned HERE rather than inside [`serve_connection`]
/// so that every way that function can end (a handshake it refuses, a
/// `DISCONNECT`, a read error, a reap) passes through the same closing flush.
/// `BufWriter` does not flush on drop, so a reply that is complete but still
/// buffered would otherwise be discarded by the close.
pub async fn handle_connection<S>(stream: S, opts: ConnOpts<'_>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);
    serve_connection(&mut reader, &mut writer, opts).await;
    flush_before_close(&mut writer).await;
}

/// The connection itself: handshake, then frames until something ends it.
async fn serve_connection<R, W>(
    reader: &mut BufReader<R>,
    writer: &mut BufWriter<W>,
    opts: ConnOpts<'_>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
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

    // Wait for Connect message (with idle timeout).
    // Accept Ping messages before authentication so load balancers can
    // health-check without completing a full CONNECT handshake.
    // Uses the smaller pre-auth payload limit (4 KB) to prevent memory abuse.
    let connect_msg = loop {
        match tokio::time::timeout(idle_timeout, Message::read_from_preauth(reader)).await {
            Ok(Ok(Some(Message::Ping))) => {
                debug!(peer = %peer, "pre-auth ping");
                if !write_msg(writer, &Message::Pong).await {
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

    // Lift the optional protocol hello off the CONNECT frame so the auth path
    // below sees one shape. `None` means a pre-v0.22.0 client that stated
    // nothing, which negotiates as protocol v1 with no named features.
    let (connect_msg, client_hello) = match connect_msg {
        Message::ConnectWithHello {
            db_name,
            password,
            username,
            hello,
        } => (
            Message::Connect {
                db_name,
                password,
                username,
            },
            Some(hello),
        ),
        other => (other, None),
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
                    write_msg(writer, &err).await;
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
                    write_msg(writer, &err).await;
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
                    write_msg(writer, &err).await;
                    return;
                }
            }

            // Version negotiation runs last, after auth and the database-name
            // check, so an unauthenticated peer still learns nothing about
            // this server (it already could not see the version string). It
            // is still inside the handshake: a mismatch is answered here with
            // a typed error and the connection closes, so no version
            // disagreement can ever surface on a later frame.
            let version = env!("CARGO_PKG_VERSION").to_string();
            let stated = stated_client_hello(client_hello.as_ref());
            let ok = match negotiate_protocol(
                &stated,
                MIN_SUPPORTED_PROTOCOL_VERSION,
                MAX_SUPPORTED_PROTOCOL_VERSION,
                SERVER_FEATURES,
                CLIENT_CATALOG_VERSION,
            ) {
                // Answer a hello only to a peer that sent one: a legacy client
                // gets a byte-identical legacy CONNECT_OK.
                Ok(server_hello) if client_hello.is_some() => Message::ConnectOkWithHello {
                    version,
                    hello: server_hello,
                },
                Ok(_) => Message::ConnectOk { version },
                Err((mismatch, message)) => {
                    warn!(peer = %peer, reason = ?mismatch, "protocol negotiation failed");
                    let err = error_response(message, ErrorClass::ProtocolVersion);
                    write_msg(writer, &err).await;
                    return;
                }
            };
            if !write_msg(writer, &ok).await {
                return;
            }
        }
        _ => {
            warn!(peer = %peer, "first message was not CONNECT");
            let err = error_response("expected CONNECT", ErrorClass::Internal);
            write_msg(writer, &err).await;
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

    // Wall-clock deadline for the explicit transaction this connection holds
    // the gate for, if any. Derived from `tx_permit` by `sync_tx_deadline`.
    let max_tx_lifetime = tx_gate.max_tx_lifetime();
    let mut tx_deadline: Option<Instant> = None;

    // Main query loop with idle timeout and shutdown awareness.
    'conn: loop {
        // Reap an over-long transaction BEFORE serving the next frame. The
        // read timeout below cannot be the only check: a client that keeps
        // sending frames (a `PING` loop is enough) never reaches it.
        if let (Some(deadline), Some(max)) = (tx_deadline, max_tx_lifetime) {
            if Instant::now() >= deadline {
                reap_expired_transaction(
                    &engine,
                    &principal,
                    &mut tx_permit,
                    &mut tx_deadline,
                    max,
                    writer,
                    &peer,
                    &metrics,
                    // Nothing is half-written here: this runs between frames,
                    // after the previous reply completed, so the notification
                    // starts on a frame boundary.
                    ReapNotice::Speak(WRITE_TIMEOUT),
                )
                .await;
                break;
            }
        }

        let msg = if let Some(m) = carry.take() {
            m
        } else if let Some(m) = pending_messages.pop_front() {
            m
        } else {
            // An open transaction shortens the wait to whichever budget
            // expires first, so a client that holds the gate and then goes
            // quiet is reaped on the transaction's deadline rather than the
            // (typically far longer) idle one.
            let read_wait = match tx_deadline {
                Some(deadline) => {
                    idle_timeout.min(deadline.saturating_duration_since(Instant::now()))
                }
                None => idle_timeout,
            };
            tokio::select! {
                // Read next message with idle timeout.
                result = tokio::time::timeout(
                    read_wait,
                    read_message_cancel_safe(
                        reader,
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
                            // The wait above may have been shortened by an
                            // open transaction's deadline rather than being
                            // the idle timeout. Go round: the check at the top
                            // of `'conn` owns the reap, so a transaction is
                            // reaped from exactly ONE place no matter which
                            // budget woke us.
                            if tx_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                                continue;
                            }
                            info!(peer = %peer, "idle timeout, closing connection");
                            let err = error_response("idle timeout", ErrorClass::Timeout);
                            write_msg_within(writer, &err, tx_deadline).await;
                            break;
                        }
                    }
                }
                // If server is shutting down, notify client and close.
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!(peer = %peer, "server shutting down, closing connection");
                        let err = error_response("server shutting down", ErrorClass::Internal);
                        write_msg_within(writer, &err, tx_deadline).await;
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
                            log_received_query(&peer, &query, "received query");
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
                                reader,
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
                            log_received_query(&peer, &query, "received SQL query");
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
                                reader,
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
                            log_received_query_with_params(
                                &peer,
                                &query,
                                params.len(),
                                "received parameterized query",
                            );
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
                                reader,
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
                            log_received_query(&peer, &query, "received native query");
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
                                reader,
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
                            log_received_query(&peer, &query, "received native SQL query");
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
                                reader,
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
                            log_received_query_with_params(
                                &peer,
                                &query,
                                params.len(),
                                "received native parameterized query",
                            );
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
                                reader,
                                &mut wire_read_buffer,
                                &mut pending_messages,
                            )
                            .await
                        }
                    }
                    _ => unreachable!("batch loop only receives plain query frames"),
                };
                // This frame may have opened or closed the connection's
                // transaction. Re-derive the lifetime deadline from the permit
                // rather than from the statement, so no install or release
                // site can be missed.
                sync_tx_deadline(&tx_permit, &mut tx_deadline, max_tx_lifetime);
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
                            reader,
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
                // Bounded by the transaction's remaining lifetime, not only by
                // WRITE_TIMEOUT: this is the write side of the same budget.
                if !write_msg_within(writer, r, tx_deadline).await {
                    reap_after_stalled_write(
                        &engine,
                        &principal,
                        &mut tx_permit,
                        &mut tx_deadline,
                        max_tx_lifetime,
                        writer,
                        &peer,
                        &metrics,
                    )
                    .await;
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
                        tx_wait_timeout,
                        credential_authenticated: credential_auth_configured,
                        principal: principal.clone(),
                        pre_gate: SyncPreGate::Status {
                            replica_id: replica_id.clone(),
                        },
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
                        tx_wait_timeout,
                        credential_authenticated: credential_auth_configured,
                        principal: principal.clone(),
                        pre_gate: SyncPreGate::Pull {
                            replica_id: request.replica_id.clone(),
                            max_units: request.max_units,
                            max_bytes: request.max_bytes,
                        },
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
                        tx_wait_timeout,
                        credential_authenticated: credential_auth_configured,
                        principal: principal.clone(),
                        pre_gate: SyncPreGate::Ack {
                            replica_id: replica_id.clone(),
                            applied_lsn,
                            observed_remote_lsn: remote_lsn,
                        },
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

        if !write_msg_within(writer, &response, tx_deadline).await {
            reap_after_stalled_write(
                &engine,
                &principal,
                &mut tx_permit,
                &mut tx_deadline,
                max_tx_lifetime,
                writer,
                &peer,
                &metrics,
            )
            .await;
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

    // ---- Entity-link diagnostics reach remote clients ----

    /// Build a schema with a link, ready for the failure cases below.
    fn linked_engine() -> (tempfile::TempDir, Engine) {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::new(dir.path()).unwrap();
        for ddl in [
            "type User { required id: int, name: str }",
            "type Order { required id: int, user_id: int, total: int }",
            "link Order.user -> User on user_id = id",
        ] {
            engine.execute_powql(ddl).unwrap();
        }
        (dir, engine)
    }

    /// Every way a link statement or a link projection can be refused, executed
    /// for real and then run through the sanitizer that guards the wire.
    ///
    /// The sanitizer is an allowlist: a message family with no prefix in it is
    /// replaced by "query execution error" on the way out. That is what
    /// happened to the whole entity-link feature. An embedded caller saw
    /// `link 'author' not found on owner type 'Post'` and a remote client saw
    /// nothing it could act on, so the same typo was diagnosable in one
    /// deployment shape and not the other.
    ///
    /// The failures are enumerated by EXECUTING them rather than by quoting
    /// strings, so rewording a message keeps it covered and only a genuinely
    /// new failure is uncovered.
    #[test]
    fn every_link_diagnostic_survives_the_wire_sanitizer() {
        let (_dir, mut engine) = linked_engine();
        let refusals = [
            // Catalog-side refusals of the link DDL itself.
            "link Order.other -> User on nope = id",
            "link Order.other -> User on user_id = nope",
            "link Order.user_id -> User on user_id = id",
            "link Order.user -> User on user_id = id",
            // Planner and executor refusals of a link PROJECTION.
            "Order as o { o.nosuchlink.name }",
            "Order as o { wrongalias.user.name }",
            "count(Order as o { o.user.name })",
        ];
        let mut masked = Vec::new();
        for statement in refusals {
            let err = engine
                .execute_powql(statement)
                .expect_err(&format!("`{statement}` must be refused"));
            let message = err.to_string();
            if sanitize_error(&message) != message {
                masked.push(format!("  {statement}\n    -> {message}"));
            }
        }
        assert!(
            masked.is_empty(),
            "these link diagnostics are masked to \"query execution error\" on their way to a \
             remote client, so only embedded callers can see what went wrong. Add a prefix to \
             SAFE_ERROR_PREFIXES for each:\n{}",
            masked.join("\n")
        );
    }

    /// The same guarantee, asserted where it is actually delivered: the frame
    /// `execute_wire_query` hands back. Testing `sanitize_error` alone would
    /// pass even if the wire path stopped calling it.
    #[tokio::test]
    async fn a_link_error_reaches_the_wire_with_its_real_message() {
        let (_dir, engine) = linked_engine();
        let engine = Arc::new(RwLock::new(engine));
        let gate = new_tx_gate_with_permits(1);
        let metrics = Arc::new(Metrics::new());
        let (_client, server) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(server);
        let mut wire_read_buffer = Vec::new();
        let mut pending_messages = InFlightReadAhead::default();
        let mut tx_permit = None;

        let (message, _, _) = execute_wire_query(
            engine,
            gate,
            &mut tx_permit,
            "Order as o { o.nosuchlink.name }".into(),
            WireResultMode::Native,
            None,
            Duration::from_secs(2),
            Duration::from_secs(2),
            &metrics,
            &mut reader,
            &mut wire_read_buffer,
            &mut pending_messages,
        )
        .await;

        match message {
            Message::ErrorWithClass { message, .. } => {
                assert!(
                    message.contains("nosuchlink"),
                    "the client was told nothing about its own typo: {message}"
                );
                assert_ne!(message, "query execution error");
            }
            other => panic!("expected a typed error frame, got {other:?}"),
        }
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

    fn one_row_engine() -> (tempfile::TempDir, Arc<RwLock<Engine>>) {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type User { required id: int }")
            .unwrap();
        engine.execute_powql("insert User { id := 1 }").unwrap();
        (dir, Arc::new(RwLock::new(engine)))
    }

    /// The transaction-lifetime deadline is derived from the permit, so it
    /// cannot be armed on some install sites and forgotten on others, and a
    /// later frame on the SAME transaction cannot re-arm it (which is exactly
    /// how a `PING` loop defeated the idle deadline).
    #[tokio::test]
    async fn transaction_deadline_tracks_the_permit_and_never_re_arms_mid_transaction() {
        let gate = new_tx_gate_with_permits(1);
        let metrics = Arc::new(Metrics::new());
        let max = Some(Duration::from_secs(60));
        let mut deadline: Option<Instant> = None;
        let mut permit: Option<OwnedSemaphorePermit> = None;

        sync_tx_deadline(&permit, &mut deadline, max);
        assert!(deadline.is_none(), "no transaction, no deadline");

        permit = Some(
            acquire_begin_permit(&gate, Duration::from_secs(1), &metrics)
                .await
                .expect("begin permit"),
        );
        sync_tx_deadline(&permit, &mut deadline, max);
        let armed = deadline.expect("an open transaction arms the deadline");

        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(2)).await;
            sync_tx_deadline(&permit, &mut deadline, max);
        }
        assert_eq!(
            deadline,
            Some(armed),
            "frames inside a transaction must not push its deadline out"
        );

        permit = None;
        sync_tx_deadline(&permit, &mut deadline, max);
        assert!(deadline.is_none(), "releasing the gate clears the deadline");

        permit = Some(
            acquire_begin_permit(&gate, Duration::from_secs(1), &metrics)
                .await
                .expect("second begin permit"),
        );
        sync_tx_deadline(&permit, &mut deadline, max);
        assert!(
            deadline.is_some_and(|next| next >= armed),
            "a new transaction gets a fresh deadline, not the previous one"
        );

        // The documented opt-out.
        let mut unbounded: Option<Instant> = None;
        sync_tx_deadline(&permit, &mut unbounded, None);
        assert!(unbounded.is_none());
        drop(permit);
    }

    #[tokio::test]
    async fn unparsable_frame_is_rejected_without_acquiring_any_permit() {
        let (_dir, engine) = one_row_engine();
        // A single-permit gate whose only permit is already held: any
        // acquisition at all, reader or writer, would have to wait.
        let gate = new_tx_gate_with_permits(1);
        let metrics = Arc::new(Metrics::new());
        let held_reader = acquire_autocommit_permit(
            &gate,
            AdmissionMode::Reader,
            Duration::from_secs(1),
            &metrics,
        )
        .await
        .expect("held reader admission");
        assert_eq!(gate.available_permits(), 0);

        let (_client, server) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(server);
        let mut wire_read_buffer = Vec::new();
        let mut pending_messages = InFlightReadAhead::default();
        let mut tx_permit = None;
        let (message, ticket, termination) = tokio::time::timeout(
            Duration::from_millis(250),
            execute_wire_query(
                engine,
                gate.clone(),
                &mut tx_permit,
                "this is not valid PowQL".into(),
                WireResultMode::Native,
                None,
                Duration::from_secs(2),
                Duration::from_secs(10),
                &metrics,
                &mut reader,
                &mut wire_read_buffer,
                &mut pending_messages,
            ),
        )
        .await
        .expect("an unparsable frame must never wait on the transaction gate");

        match message {
            Message::ErrorWithClass { class, .. } => assert_eq!(class, ErrorClass::Parse),
            other => panic!("expected a typed parse error, got {other:?}"),
        }
        assert!(ticket.is_none());
        assert!(termination.is_none());
        assert!(tx_permit.is_none());
        assert_eq!(
            gate.available_permits(),
            0,
            "a statement that executes nothing must acquire nothing"
        );
        drop(held_reader);
        assert_eq!(gate.available_permits(), 1);
        // The rejected frame is still a failed statement from the client's view.
        assert!(metrics
            .render()
            .contains("powdb_queries_total{result=\"error\"} 1"));
    }

    // ---- The write side of the transaction budget ----

    /// A socket that accepts a fixed amount of data and then never accepts
    /// another byte, recording everything the server offers it afterwards.
    ///
    /// `seal()` marks the boundary between the write that failed and whatever
    /// the server does next: bytes offered after the seal are bytes written
    /// AFTER a write had already failed, which is the thing that tears a frame
    /// in half on a real socket.
    #[derive(Clone)]
    struct StalledSocket(Arc<Mutex<StalledSocketState>>);

    struct StalledSocketState {
        room: usize,
        accepted: usize,
        sealed: bool,
        offered_after_seal: usize,
    }

    impl StalledSocket {
        fn with_room(room: usize) -> Self {
            Self(Arc::new(Mutex::new(StalledSocketState {
                room,
                accepted: 0,
                sealed: false,
                offered_after_seal: 0,
            })))
        }

        fn accepted(&self) -> usize {
            self.0.lock().unwrap().accepted
        }

        fn seal(&self) {
            self.0.lock().unwrap().sealed = true;
        }

        fn offered_after_seal(&self) -> usize {
            self.0.lock().unwrap().offered_after_seal
        }

        /// The client started reading again.
        fn drain(&self, more: usize) {
            let mut state = self.0.lock().unwrap();
            state.room = state.room.saturating_add(more);
        }
    }

    impl AsyncWrite for StalledSocket {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let mut state = self.0.lock().unwrap();
            if state.sealed {
                state.offered_after_seal += buf.len();
            }
            let room = state.room.saturating_sub(state.accepted);
            if room == 0 {
                // Deliberately no waker: the write budget is the only thing
                // that ends this write, which is the stall being modelled.
                return std::task::Poll::Pending;
            }
            let taken = buf.len().min(room);
            state.accepted += taken;
            std::task::Poll::Ready(Ok(taken))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// A reply larger than any buffer, so the write goes straight to the
    /// socket and a cancelled write leaves the frame half delivered.
    fn unwritable_reply() -> Message {
        error_response("x".repeat(4 << 20), ErrorClass::Internal)
    }

    /// The reap that fires because a reply write failed must write NOTHING
    /// more on that connection.
    ///
    /// The frame it could not finish is already partly on the wire with its
    /// declared length announced, and there is no resume: a client counting
    /// that length down reads whatever comes next as the dead frame's payload.
    /// So the notification cannot arrive, and the bytes that carry it corrupt
    /// the client's framing on the way to a reset it would have seen anyway.
    /// Rolling back, releasing the gate, logging, and counting still happen;
    /// only the wire goes quiet.
    #[tokio::test]
    async fn a_reap_after_a_stalled_write_writes_nothing_more() {
        let (_dir, engine) = one_row_engine();
        let metrics = Arc::new(Metrics::new());
        let socket = StalledSocket::with_room(64 * 1024);
        let mut writer = BufWriter::new(socket.clone());

        let reply = unwritable_reply();
        assert!(
            !write_msg_with_budget(&mut writer, &reply, Duration::from_millis(50)).await,
            "a socket that stops accepting must fail the write"
        );
        assert_eq!(
            socket.accepted(),
            64 * 1024,
            "the frame must be torn on the wire, which is what makes the reap unable to speak"
        );
        socket.seal();

        let principal: Option<Principal> = None;
        let mut tx_permit = None;
        let mut tx_deadline = Some(Instant::now() - Duration::from_millis(1));
        reap_after_stalled_write(
            &engine,
            &principal,
            &mut tx_permit,
            &mut tx_deadline,
            Some(Duration::from_millis(600)),
            &mut writer,
            "peer",
            &metrics,
        )
        .await;

        assert_eq!(
            socket.offered_after_seal(),
            0,
            "the reap wrote {} bytes after a frame it could not finish; they land inside that \
             frame's declared payload",
            socket.offered_after_seal()
        );
        assert!(
            tx_deadline.is_none(),
            "the transaction must still be reaped, not merely left unwritten to"
        );
        let rendered = metrics.render();
        assert!(
            rendered.contains("powdb_tx_reaped_total 1"),
            "a silent reap is still an operator-visible reap:\n{rendered}"
        );
    }

    /// The other half of the same rule: on a connection whose last write
    /// COMPLETED, the next byte starts a frame, so the reap still says why.
    /// Suppressing the notification everywhere would trade one lie for another.
    #[tokio::test]
    async fn a_reap_on_a_quiet_connection_still_tells_the_client_why() {
        let (_dir, engine) = one_row_engine();
        let metrics = Arc::new(Metrics::new());
        let mut writer = BufWriter::new(Vec::new());

        let principal: Option<Principal> = None;
        let mut tx_permit = None;
        let mut tx_deadline = Some(Instant::now());
        reap_expired_transaction(
            &engine,
            &principal,
            &mut tx_permit,
            &mut tx_deadline,
            Duration::from_millis(600),
            &mut writer,
            "peer",
            &metrics,
            ReapNotice::Speak(WRITE_TIMEOUT),
        )
        .await;

        let frame = writer.into_inner();
        let decoded = Message::decode(&frame).expect("the reap notification must be a whole frame");
        match &decoded {
            Message::Error { message } | Message::ErrorWithClass { message, .. } => assert!(
                message.contains("maximum lifetime")
                    && message.contains("POWDB_TX_MAX_LIFETIME_MS"),
                "the reaped client must be told what happened and which budget to raise: {message}"
            ),
            other => panic!("expected a typed error frame, got {other:?}"),
        }
        assert_eq!(
            crate::protocol::decode_error_class(&frame),
            Some(ErrorClass::Timeout.as_u8()),
            "a reap is a time budget, never an unclassified failure"
        );
    }

    /// The budget a reply is written under, at both ends of its range.
    #[test]
    fn the_write_budget_is_the_smaller_of_the_write_timeout_and_the_remaining_lifetime() {
        assert_eq!(
            write_budget(None),
            WRITE_TIMEOUT,
            "with no transaction there is no lifetime to cap the write"
        );
        assert_eq!(
            write_budget(Some(Instant::now() + WRITE_TIMEOUT * 2)),
            WRITE_TIMEOUT,
            "a distant deadline must not RAISE the write timeout"
        );

        let budget = write_budget(Some(Instant::now() + Duration::from_millis(500)));
        assert!(
            budget <= Duration::from_millis(500) && budget > Duration::from_millis(400),
            "a transaction with 500ms left may write for at most 500ms, got {budget:?}"
        );

        // The edge the reap depends on: a deadline that has already passed is
        // ZERO, never a wrapped-around eternity.
        assert_eq!(
            write_budget(Some(Instant::now() - Duration::from_secs(1))),
            Duration::ZERO,
            "an expired transaction must not be handed a budget at all"
        );
    }

    /// `Duration::ZERO` means "no WAITING", not "no write": a reply the socket
    /// can take immediately still goes out, and one that would block fails at
    /// once instead of parking the handler with the gate still held.
    #[tokio::test]
    async fn a_zero_write_budget_delivers_what_never_blocks_and_nothing_else() {
        let reply = error_response("small", ErrorClass::Internal);
        let mut ready = BufWriter::new(Vec::new());
        assert!(
            write_msg_with_budget(&mut ready, &reply, Duration::ZERO).await,
            "a write that never has to wait must still be delivered on a spent budget"
        );
        assert_eq!(ready.into_inner(), reply.encode());

        let socket = StalledSocket::with_room(0);
        let mut blocked = BufWriter::new(socket.clone());
        assert!(
            !write_msg_with_budget(&mut blocked, &unwritable_reply(), Duration::ZERO).await,
            "a write that would have to wait must fail immediately on a spent budget"
        );
        assert_eq!(socket.accepted(), 0);
    }

    /// A frame the connection already handed to the writer must not be thrown
    /// away because the connection is ending: `BufWriter` has no `Drop` that
    /// flushes, so without a closing flush a complete reply left in the buffer
    /// is silently discarded. The flush is bounded, because a client that
    /// stopped reading must not be able to park the teardown either.
    #[tokio::test]
    async fn the_closing_flush_delivers_buffered_frames_without_parking_teardown() {
        let reply = error_response("buffered", ErrorClass::Internal);
        let socket = StalledSocket::with_room(0);
        let mut writer = BufWriter::new(socket.clone());

        // Small enough to sit entirely in the BufWriter: the socket never sees
        // it, so nothing is torn and the frame is still whole and deliverable.
        assert!(!write_msg_with_budget(&mut writer, &reply, Duration::from_millis(50)).await);
        assert_eq!(
            socket.accepted(),
            0,
            "the frame is buffered, not on the wire"
        );

        let started = Instant::now();
        assert!(
            !flush_before_close(&mut writer).await,
            "a client that is still not reading cannot be flushed to"
        );
        assert!(
            started.elapsed() < FINAL_FLUSH_BUDGET * 4,
            "the closing flush parked teardown for {:?}",
            started.elapsed()
        );

        socket.drain(usize::MAX / 2);
        assert!(
            flush_before_close(&mut writer).await,
            "once the client reads again the buffered frame must go out"
        );
        assert_eq!(
            socket.accepted(),
            reply.encode().len(),
            "the whole frame, exactly once"
        );
    }

    // ---- Transaction-gate parity matrix ----
    //
    // The tests this replaces named three frontends by hand and stayed green
    // for a year while a FOURTH (sync) seized the whole gate before running
    // its own auth check, waited on it with no timeout at all, and answered
    // with an untyped error frame.
    //
    // Naming frontends by hand was only half the defect. The first repair
    // enumerated the FRONTENDS and then probed each one with a single
    // hard-coded example, which exercises each RULE on exactly one frontend:
    // pre-gate replica-id validation could be narrowed to `SyncStatus` alone,
    // or dropped from `SyncPull` and `SyncAck`, with the whole suite still
    // green. That is a spot check wearing a matrix's clothes.
    //
    // What follows is the CROSS PRODUCT of two enumerations:
    //
    //   axis 1  every rule a frame can be refused by BEFORE the gate
    //   axis 2  every frontend that rule is reachable from
    //
    // and axis 2 is DERIVED rather than written down. A rule declares which
    // SLOT of a runnable frame it corrupts, and it is probed on every frontend
    // whose runnable frame has that slot. `SyncSlot::replica_id` is not
    // optional, because every `SyncPreGate` variant carries a replica id and
    // `SyncPreGate::check` destructures all three together, so a replica-id
    // rule is probed on all three sync frontends by construction.
    //
    // The last test in this section closes the other direction: it enumerates
    // every rejection site inside the sync dispatch functions straight out of
    // this file's source, so a NEW refusal added under the gate fails the
    // build until its author says whether it needs the engine.

    use std::collections::{BTreeMap, BTreeSet};

    /// Declare the gate-frontend enum and its iteration list together, so a
    /// variant cannot exist without being in the list the matrix walks.
    macro_rules! gate_frontends {
        ($($variant:ident),+ $(,)?) => {
            #[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
            enum GateFrontend { $($variant),+ }

            impl GateFrontend {
                const ALL: &'static [GateFrontend] = &[$(GateFrontend::$variant),+];
            }
        };
    }

    gate_frontends!(PowQl, Sql, Params, SyncStatus, SyncPull, SyncAck);

    /// Declare the pre-gate rule enum and its iteration list together, for the
    /// same reason: a rule cannot exist without the matrix walking it, and
    /// walking it means probing every frontend it reaches.
    macro_rules! gate_rules {
        ($($variant:ident),+ $(,)?) => {
            #[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
            enum GateRule { $($variant),+ }

            impl GateRule {
                const ALL: &'static [GateRule] = &[$(GateRule::$variant),+];
            }
        };
    }

    gate_rules!(
        UnparsableStatement,
        RoleForbidsStatement,
        SyncCredentialMissing,
        SyncRoleForbidsProtocol,
        SyncInvalidReplicaId,
        SyncPullMaxUnitsOutOfRange,
        SyncPullMaxBytesOutOfRange,
        SyncAckLsnAheadOfClientRemote,
        SyncInsideActiveTransaction,
    );

    /// Which frontend serves a wire frame, or `None` when the frame never
    /// reaches the transaction gate.
    ///
    /// Exhaustive on purpose: a new `Message` variant does not compile until
    /// someone decides whether it can reach the gate, and answering "yes"
    /// forces a `GateFrontend` variant, which the macro forces into `ALL`,
    /// which the matrix forces probes for.
    fn gate_frontend(msg: &Message) -> Option<GateFrontend> {
        match msg {
            // Six wire message types, three frontends: the native variants
            // differ only in result encoding and share the same wrapper.
            Message::Query { .. } | Message::QueryNative { .. } => Some(GateFrontend::PowQl),
            Message::QuerySql { .. } | Message::QuerySqlNative { .. } => Some(GateFrontend::Sql),
            Message::QueryWithParams { .. } | Message::QueryWithParamsNative { .. } => {
                Some(GateFrontend::Params)
            }
            Message::SyncStatus { .. } => Some(GateFrontend::SyncStatus),
            Message::SyncPull { .. } => Some(GateFrontend::SyncPull),
            Message::SyncAck { .. } => Some(GateFrontend::SyncAck),
            // Handshake frames, server responses, and control frames the main
            // loop answers without ever touching the gate.
            Message::Connect { .. }
            | Message::ConnectWithHello { .. }
            | Message::ConnectOk { .. }
            | Message::ConnectOkWithHello { .. }
            | Message::SyncStatusResult { .. }
            | Message::SyncPullResult { .. }
            | Message::SyncAckResult { .. }
            | Message::ResultRows { .. }
            | Message::ResultScalar { .. }
            | Message::ResultRowsNative { .. }
            | Message::ResultScalarNative { .. }
            | Message::ResultOk { .. }
            | Message::ResultMessage { .. }
            | Message::Error { .. }
            | Message::ErrorWithClass { .. }
            | Message::Disconnect
            | Message::Ping
            | Message::Pong => None,
        }
    }

    /// A wire frame served by `frontend`, used to prove the matrix and the
    /// dispatch surface describe the same six frontends.
    fn representative_frame(frontend: GateFrontend) -> Message {
        match frontend {
            GateFrontend::PowQl => Message::Query {
                query: "User".into(),
            },
            GateFrontend::Sql => Message::QuerySql {
                query: "SELECT * FROM User".into(),
            },
            GateFrontend::Params => Message::QueryWithParams {
                query: "User filter .id = $1".into(),
                params: vec![WireParam::Int(1)],
            },
            GateFrontend::SyncStatus => Message::SyncStatus {
                replica_id: "replica-a".into(),
            },
            GateFrontend::SyncPull => Message::SyncPull {
                replica_id: "replica-a".into(),
                since_lsn: 0,
                max_units: 1,
                max_bytes: 1024,
                database_id: [0; 16],
                primary_generation: 1,
                wal_format_version: 1,
                catalog_version: 6,
                segment_format_version: 1,
            },
            GateFrontend::SyncAck => Message::SyncAck {
                replica_id: "replica-a".into(),
                applied_lsn: 0,
                remote_lsn: 0,
            },
        }
    }

    /// The statement slot of a runnable frame. The unparsable and forbidden
    /// variants live beside the runnable text, in the same language, so a rule
    /// can swap in its violation without knowing which frontend it is looking
    /// at.
    #[derive(Clone)]
    struct QuerySlot {
        text: String,
        unparsable: &'static str,
        forbidden_write: &'static str,
        params: Vec<WireParam>,
        principal: Option<Principal>,
    }

    /// The identity and cursor fields only a pull frame carries.
    #[derive(Clone, Copy)]
    struct PullIdentity {
        since_lsn: u64,
        database_id: [u8; 16],
        primary_generation: u64,
        wal_format_version: u16,
        catalog_version: u16,
        segment_format_version: u16,
    }

    /// The sync slot of a runnable frame.
    ///
    /// `replica_id` is NOT optional. Every `SyncPreGate` variant carries one
    /// (see the combined destructure in `SyncPreGate::check`), so every sync
    /// frontend can be given an invalid replica id, and a rule that corrupts
    /// the replica id is therefore probed on all three of them without anyone
    /// listing which three.
    #[derive(Clone)]
    struct SyncSlot {
        replica_id: String,
        credential_authenticated: bool,
        principal: Option<Principal>,
        connection_has_transaction: bool,
        /// Present only on the frontend whose frame carries batch bounds.
        pull_bounds: Option<(u32, u64)>,
        /// Present only on the frontend whose frame carries the identity and
        /// cursor fields.
        pull_identity: Option<PullIdentity>,
        /// Present only on the frontend whose frame carries the two LSNs.
        ack_lsns: Option<(u64, u64)>,
    }

    /// A frame this frontend would run, before any rule corrupts it.
    #[derive(Clone)]
    struct ProbeSpec {
        frontend: GateFrontend,
        query: Option<QuerySlot>,
        sync: Option<SyncSlot>,
    }

    fn runnable_spec(frontend: GateFrontend) -> ProbeSpec {
        let query = |text: &str,
                     unparsable: &'static str,
                     forbidden_write: &'static str,
                     params: Vec<WireParam>| {
            Some(QuerySlot {
                text: text.to_string(),
                unparsable,
                forbidden_write,
                params,
                principal: None,
            })
        };
        let sync = |pull_bounds, pull_identity, ack_lsns| {
            Some(SyncSlot {
                replica_id: "replica-a".to_string(),
                credential_authenticated: true,
                principal: Some(admin_principal()),
                connection_has_transaction: false,
                pull_bounds,
                pull_identity,
                ack_lsns,
            })
        };
        let identity = PullIdentity {
            since_lsn: 0,
            database_id: [0; 16],
            primary_generation: 1,
            wal_format_version: 1,
            catalog_version: 6,
            segment_format_version: 1,
        };
        match frontend {
            GateFrontend::PowQl => ProbeSpec {
                frontend,
                query: query(
                    "User",
                    "this is not valid PowQL",
                    "insert User { id := 2 }",
                    Vec::new(),
                ),
                sync: None,
            },
            GateFrontend::Sql => ProbeSpec {
                frontend,
                query: query(
                    "SELECT * FROM User",
                    "SELEKT * FROM",
                    "INSERT INTO User (id) VALUES (2)",
                    Vec::new(),
                ),
                sync: None,
            },
            GateFrontend::Params => ProbeSpec {
                frontend,
                query: query(
                    "User filter .id = $1",
                    "User filter .id = = $1",
                    "insert User { id := $1 }",
                    vec![WireParam::Int(1)],
                ),
                sync: None,
            },
            GateFrontend::SyncStatus => ProbeSpec {
                frontend,
                query: None,
                sync: sync(None, None, None),
            },
            GateFrontend::SyncPull => ProbeSpec {
                frontend,
                query: None,
                sync: sync(Some((1, 1024)), Some(identity), None),
            },
            GateFrontend::SyncAck => ProbeSpec {
                frontend,
                query: None,
                sync: sync(None, None, Some((0, 0))),
            },
        }
    }

    impl GateRule {
        /// Corrupt `spec` so it violates exactly this rule, and report whether
        /// this frontend even HAS the slot the rule corrupts.
        ///
        /// Returning `false` is how "not reachable from this frontend" is
        /// derived. Nothing anywhere lists which frontends a rule applies to:
        /// the answer falls out of which slots `runnable_spec` gave them.
        fn violate(self, spec: &mut ProbeSpec) -> bool {
            match self {
                Self::UnparsableStatement => match spec.query.as_mut() {
                    Some(slot) => {
                        slot.text = slot.unparsable.to_string();
                        true
                    }
                    None => false,
                },
                Self::RoleForbidsStatement => match spec.query.as_mut() {
                    Some(slot) => {
                        slot.text = slot.forbidden_write.to_string();
                        slot.principal = principal("readonly");
                        true
                    }
                    None => false,
                },
                Self::SyncCredentialMissing => match spec.sync.as_mut() {
                    Some(slot) => {
                        slot.credential_authenticated = false;
                        true
                    }
                    None => false,
                },
                Self::SyncRoleForbidsProtocol => match spec.sync.as_mut() {
                    Some(slot) => {
                        slot.principal = principal("readonly");
                        true
                    }
                    None => false,
                },
                Self::SyncInvalidReplicaId => match spec.sync.as_mut() {
                    Some(slot) => {
                        slot.replica_id = String::new();
                        true
                    }
                    None => false,
                },
                Self::SyncPullMaxUnitsOutOfRange => {
                    match spec.sync.as_mut().and_then(|s| s.pull_bounds.as_mut()) {
                        Some(bounds) => {
                            bounds.0 = 0;
                            true
                        }
                        None => false,
                    }
                }
                Self::SyncPullMaxBytesOutOfRange => {
                    match spec.sync.as_mut().and_then(|s| s.pull_bounds.as_mut()) {
                        Some(bounds) => {
                            bounds.1 = MAX_SYNC_PULL_BYTES + 1;
                            true
                        }
                        None => false,
                    }
                }
                Self::SyncAckLsnAheadOfClientRemote => {
                    match spec.sync.as_mut().and_then(|s| s.ack_lsns.as_mut()) {
                        Some(lsns) => {
                            *lsns = (5, 0);
                            true
                        }
                        None => false,
                    }
                }
                Self::SyncInsideActiveTransaction => match spec.sync.as_mut() {
                    Some(slot) => {
                        slot.connection_has_transaction = true;
                        true
                    }
                    None => false,
                },
            }
        }

        /// The wire class this refusal must carry, whichever frontend answered.
        /// No arm is a fallback: each is the class the query frontends already
        /// use for that meaning.
        fn expected_class(self) -> ErrorClass {
            match self {
                Self::UnparsableStatement => ErrorClass::Parse,
                // Fixable only by reconnecting with credentials.
                Self::SyncCredentialMissing => ErrorClass::AuthFailed,
                // A caller-supplied bound outside the server's accepted range.
                Self::SyncPullMaxUnitsOutOfRange | Self::SyncPullMaxBytesOutOfRange => {
                    ErrorClass::LimitExceeded
                }
                // The caller's own request is wrong and it can say so.
                Self::RoleForbidsStatement
                | Self::SyncRoleForbidsProtocol
                | Self::SyncInvalidReplicaId
                | Self::SyncAckLsnAheadOfClientRemote
                | Self::SyncInsideActiveTransaction => ErrorClass::Execution,
            }
        }

        /// The class `SyncPreGate::check` itself must answer with.
        ///
        /// `None` for the two query-side rules, which have no `SyncPreGate` at
        /// all, and for the active-transaction refusal, which
        /// `execute_gated_sync` makes before it consults the pre-gate.
        fn pre_gate_class(self) -> Option<SyncErrorClass> {
            match self {
                Self::UnparsableStatement
                | Self::RoleForbidsStatement
                | Self::SyncInsideActiveTransaction => None,
                Self::SyncCredentialMissing => Some(SyncErrorClass::AuthRequired),
                Self::SyncRoleForbidsProtocol => Some(SyncErrorClass::PermissionDenied),
                Self::SyncInvalidReplicaId => Some(SyncErrorClass::InvalidReplicaId),
                Self::SyncPullMaxUnitsOutOfRange => Some(SyncErrorClass::InvalidMaxUnits),
                Self::SyncPullMaxBytesOutOfRange => Some(SyncErrorClass::InvalidMaxBytes),
                Self::SyncAckLsnAheadOfClientRemote => Some(SyncErrorClass::LsnAheadOfRemote),
            }
        }
    }

    struct GateProbeEnv {
        engine: Arc<RwLock<Engine>>,
        gate: TxGate,
        metrics: Arc<Metrics>,
        tx_wait_timeout: Duration,
    }

    /// The pre-gate a sync frontend builds for this slot. Exhaustive on the
    /// frontend, and every arm reads `slot.replica_id`, which is what makes the
    /// replica-id rule reach all three.
    fn sync_pre_gate(frontend: GateFrontend, slot: &SyncSlot) -> SyncPreGate {
        match frontend {
            GateFrontend::SyncStatus => SyncPreGate::Status {
                replica_id: slot.replica_id.clone(),
            },
            GateFrontend::SyncPull => {
                let (max_units, max_bytes) = slot.pull_bounds.expect("a pull frame carries bounds");
                SyncPreGate::Pull {
                    replica_id: slot.replica_id.clone(),
                    max_units,
                    max_bytes,
                }
            }
            GateFrontend::SyncAck => {
                let (applied_lsn, observed_remote_lsn) =
                    slot.ack_lsns.expect("an ack frame carries both LSNs");
                SyncPreGate::Ack {
                    replica_id: slot.replica_id.clone(),
                    applied_lsn,
                    observed_remote_lsn,
                }
            }
            GateFrontend::PowQl | GateFrontend::Sql | GateFrontend::Params => {
                panic!("{frontend:?} is not a sync frontend")
            }
        }
    }

    fn sync_pull_request(slot: &SyncSlot) -> SyncPullRequest {
        let (max_units, max_bytes) = slot.pull_bounds.expect("a pull frame carries bounds");
        let identity = slot
            .pull_identity
            .expect("a pull frame carries its identity");
        SyncPullRequest {
            replica_id: slot.replica_id.clone(),
            since_lsn: identity.since_lsn,
            max_units,
            max_bytes,
            database_id: identity.database_id,
            primary_generation: identity.primary_generation,
            wal_format_version: identity.wal_format_version,
            catalog_version: identity.catalog_version,
            segment_format_version: identity.segment_format_version,
        }
    }

    /// Run the frontend's own decision function against `engine`, with no gate
    /// and no pre-gate in the way. Used to ask what the code UNDER the gate
    /// would decide.
    fn dispatch_sync_decision(
        engine: &Arc<RwLock<Engine>>,
        frontend: GateFrontend,
        slot: &SyncSlot,
    ) -> SyncDecision {
        match frontend {
            GateFrontend::SyncStatus => dispatch_sync_status_decision(
                engine,
                slot.replica_id.clone(),
                slot.credential_authenticated,
                slot.principal.as_ref(),
            ),
            GateFrontend::SyncPull => dispatch_sync_pull_decision(
                engine,
                sync_pull_request(slot),
                slot.credential_authenticated,
                slot.principal.as_ref(),
            ),
            GateFrontend::SyncAck => {
                let (applied_lsn, observed_remote_lsn) =
                    slot.ack_lsns.expect("an ack frame carries both LSNs");
                dispatch_sync_ack_decision(
                    engine,
                    slot.replica_id.clone(),
                    applied_lsn,
                    observed_remote_lsn,
                    slot.credential_authenticated,
                    slot.principal.as_ref(),
                )
            }
            GateFrontend::PowQl | GateFrontend::Sql | GateFrontend::Params => {
                panic!("{frontend:?} is not a sync frontend")
            }
        }
    }

    /// Run one probe frame through the real frontend that serves it, and
    /// return the response the client would receive.
    async fn run_gate_probe(env: &GateProbeEnv, spec: &ProbeSpec) -> Message {
        let (_client, server) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(server);
        let mut wire_read_buffer = Vec::new();
        let mut pending_messages = InFlightReadAhead::default();
        let mut tx_permit = None;
        let engine = Arc::clone(&env.engine);
        let metrics = Arc::clone(&env.metrics);
        let query_timeout = Duration::from_secs(2);

        match spec.frontend {
            GateFrontend::PowQl => {
                let slot = spec
                    .query
                    .as_ref()
                    .expect("a query frontend has a statement");
                execute_wire_query(
                    engine,
                    env.gate.clone(),
                    &mut tx_permit,
                    slot.text.clone(),
                    WireResultMode::Native,
                    slot.principal.clone(),
                    query_timeout,
                    env.tx_wait_timeout,
                    &metrics,
                    &mut reader,
                    &mut wire_read_buffer,
                    &mut pending_messages,
                )
                .await
                .0
            }
            GateFrontend::Sql => {
                let slot = spec
                    .query
                    .as_ref()
                    .expect("a query frontend has a statement");
                execute_wire_query_sql(
                    engine,
                    env.gate.clone(),
                    &mut tx_permit,
                    slot.text.clone(),
                    WireResultMode::Native,
                    slot.principal.clone(),
                    query_timeout,
                    env.tx_wait_timeout,
                    &metrics,
                    &mut reader,
                    &mut wire_read_buffer,
                    &mut pending_messages,
                )
                .await
                .0
            }
            GateFrontend::Params => {
                let slot = spec
                    .query
                    .as_ref()
                    .expect("a query frontend has a statement");
                execute_wire_query_with_params(
                    engine,
                    env.gate.clone(),
                    &mut tx_permit,
                    slot.text.clone(),
                    slot.params.clone(),
                    WireResultMode::Native,
                    slot.principal.clone(),
                    query_timeout,
                    env.tx_wait_timeout,
                    &metrics,
                    &mut reader,
                    &mut wire_read_buffer,
                    &mut pending_messages,
                )
                .await
                .0
            }
            GateFrontend::SyncStatus | GateFrontend::SyncPull | GateFrontend::SyncAck => {
                let slot = spec
                    .sync
                    .as_ref()
                    .expect("a sync frontend has a sync slot")
                    .clone();
                let frontend = spec.frontend;
                let operation = match frontend {
                    GateFrontend::SyncStatus => SyncOperation::Status,
                    GateFrontend::SyncPull => SyncOperation::Pull,
                    _ => SyncOperation::Ack,
                };
                let log_context = match frontend {
                    GateFrontend::SyncStatus => SyncLogContext::status(&slot.replica_id),
                    GateFrontend::SyncPull => SyncLogContext::pull(&sync_pull_request(&slot)),
                    _ => {
                        let (applied_lsn, observed_remote_lsn) =
                            slot.ack_lsns.expect("an ack frame carries both LSNs");
                        SyncLogContext::ack(&slot.replica_id, applied_lsn, observed_remote_lsn)
                    }
                };
                execute_gated_sync(
                    SyncExecutionContext {
                        tx_gate: env.gate.clone(),
                        connection_has_transaction: slot.connection_has_transaction,
                        operation,
                        log_context,
                        metrics: &metrics,
                        query_timeout,
                        tx_wait_timeout: env.tx_wait_timeout,
                        credential_authenticated: slot.credential_authenticated,
                        principal: slot.principal.clone(),
                        pre_gate: sync_pre_gate(frontend, &slot),
                    },
                    (engine, frontend, slot),
                    |(engine, frontend, slot)| dispatch_sync_decision(&engine, frontend, &slot),
                )
                .await
            }
        }
    }

    #[test]
    fn every_gate_frontend_is_reachable_from_the_dispatch_surface() {
        for frontend in GateFrontend::ALL.iter().copied() {
            assert_eq!(
                gate_frontend(&representative_frame(frontend)),
                Some(frontend),
                "{frontend:?} is in the parity matrix but no wire frame dispatches to it"
            );
        }
        for msg in [Message::Ping, Message::Pong, Message::Disconnect] {
            assert_eq!(
                gate_frontend(&msg),
                None,
                "{msg:?} does not reach the transaction gate"
            );
        }
    }

    /// Every frontend that has a sync slot, derived from `runnable_spec`.
    fn sync_frontends() -> Vec<GateFrontend> {
        GateFrontend::ALL
            .iter()
            .copied()
            .filter(|f| runnable_spec(*f).sync.is_some())
            .collect()
    }

    /// PARITY, axis 1 x axis 2: every pre-gate rule, on every frontend it
    /// reaches, must be refused without acquiring a permit and with the class
    /// that rule means. The held single-permit gate is the mutation check: any
    /// frontend that queues for a permit before deciding blows the 250ms
    /// budget, a fortieth of the 10s `tx_wait_timeout` it would be waiting on.
    #[tokio::test]
    async fn every_pre_gate_rule_is_refused_without_a_permit_on_every_frontend_it_reaches() {
        let (_dir, engine) = one_row_engine();
        let gate = new_tx_gate_with_permits(1);
        let metrics = Arc::new(Metrics::new());
        let held_reader = acquire_autocommit_permit(
            &gate,
            AdmissionMode::Reader,
            Duration::from_secs(1),
            &metrics,
        )
        .await
        .expect("held reader admission");
        assert_eq!(gate.available_permits(), 0);

        let env = GateProbeEnv {
            engine,
            gate: gate.clone(),
            metrics: Arc::clone(&metrics),
            tx_wait_timeout: Duration::from_secs(10),
        };

        let mut coverage: BTreeMap<GateRule, Vec<GateFrontend>> = BTreeMap::new();
        for rule in GateRule::ALL.iter().copied() {
            for frontend in GateFrontend::ALL.iter().copied() {
                let mut spec = runnable_spec(frontend);
                if !rule.violate(&mut spec) {
                    continue;
                }
                coverage.entry(rule).or_default().push(frontend);
                let response =
                    tokio::time::timeout(Duration::from_millis(250), run_gate_probe(&env, &spec))
                        .await
                        .unwrap_or_else(|_| {
                            panic!(
                                "{rule:?} on {frontend:?} waited on the transaction gate for a \
                                 frame that executes nothing"
                            )
                        });
                match response {
                    Message::ErrorWithClass { class, message } => assert_eq!(
                        class,
                        rule.expected_class(),
                        "{rule:?} on {frontend:?} carried the wrong class: {message}"
                    ),
                    other => panic!(
                        "{rule:?} on {frontend:?} answered without a wire error class: {other:?}"
                    ),
                }
                assert_eq!(
                    gate.available_permits(),
                    0,
                    "{rule:?} on {frontend:?} must acquire nothing"
                );
            }
            assert!(
                coverage.contains_key(&rule),
                "{rule:?} is enumerated but reaches no frontend: either the rule is dead or the \
                 slot it corrupts was renamed out from under `violate`"
            );
        }

        // Derived, not listed: a rule that corrupts a field EVERY sync frame
        // carries must have been probed on EVERY sync frontend. This is the
        // assertion the one-example-per-frontend matrix could not make, and it
        // is the one that fails when a pre-gate check is narrowed to a single
        // sync message type.
        let sync = sync_frontends();
        for rule in GateRule::ALL.iter().copied() {
            let universal = sync.iter().all(|frontend| {
                let mut probe = runnable_spec(*frontend);
                rule.violate(&mut probe)
            });
            if universal {
                assert_eq!(
                    coverage[&rule], sync,
                    "{rule:?} corrupts a field every sync frame carries but was probed on a subset"
                );
            }
        }

        assert!(
            metrics.render().contains("powdb_tx_gate_timeouts_total 0"),
            "no frontend may report a gate timeout it never waited for"
        );
        drop(held_reader);
        assert_eq!(gate.available_permits(), 1);
    }

    /// The same cross product asserted directly against `SyncPreGate::check`,
    /// with no gate, no timing, and no engine in the way.
    ///
    /// The wire-level matrix above catches a narrowed pre-gate check by timing
    /// out on a held gate; this one catches it by name, on every sync frontend,
    /// in microseconds.
    #[test]
    fn every_sync_pre_gate_rule_is_checked_on_every_sync_frontend() {
        for frontend in sync_frontends() {
            let runnable = runnable_spec(frontend);
            let slot = runnable.sync.as_ref().expect("a sync frontend has a slot");
            assert!(
                sync_pre_gate(frontend, slot)
                    .check(slot.credential_authenticated, slot.principal.as_ref())
                    .is_ok(),
                "{frontend:?}'s runnable frame must pass the pre-gate, or every probe below \
                 proves nothing"
            );
        }

        for rule in GateRule::ALL.iter().copied() {
            let Some(expected) = rule.pre_gate_class() else {
                continue;
            };
            let mut checked = 0usize;
            for frontend in sync_frontends() {
                let mut spec = runnable_spec(frontend);
                if !rule.violate(&mut spec) {
                    continue;
                }
                let slot = spec.sync.as_ref().expect("a sync frontend has a slot");
                match sync_pre_gate(frontend, slot)
                    .check(slot.credential_authenticated, slot.principal.as_ref())
                {
                    Err((class, message)) => assert_eq!(
                        class, expected,
                        "{rule:?} on {frontend:?} was refused as {class:?}, not {expected:?}: \
                         {message}"
                    ),
                    Ok(()) => panic!(
                        "{rule:?} is not checked pre-gate on {frontend:?}: this frame reaches \
                         `acquire_sync_permit` and takes the whole transaction gate before the \
                         dispatch function refuses it"
                    ),
                }
                checked += 1;
            }
            assert!(
                checked > 0,
                "{rule:?} declares a pre-gate class but reaches no sync frontend"
            );
        }
    }

    /// PARITY: every frontend's gate acquire is bounded by `tx_wait_timeout`,
    /// answers with a typed `Timeout`, and is counted. The sync frontend had
    /// none of the three: it waited out the whole hold, wrote no error frame,
    /// and never touched the counter, so a starved replica was invisible.
    #[tokio::test]
    async fn every_gate_frontend_bounds_its_acquire_and_counts_the_timeout() {
        let (_dir, engine) = one_row_engine();
        let gate = new_tx_gate_with_permits(1);
        let metrics = Arc::new(Metrics::new());
        let tx_wait_timeout = Duration::from_millis(150);
        let held_reader = acquire_autocommit_permit(
            &gate,
            AdmissionMode::Reader,
            Duration::from_secs(1),
            &metrics,
        )
        .await
        .expect("held reader admission");

        let env = GateProbeEnv {
            engine,
            gate: gate.clone(),
            metrics: Arc::clone(&metrics),
            tx_wait_timeout,
        };
        for (waited, frontend) in GateFrontend::ALL.iter().copied().enumerate() {
            let started = Instant::now();
            let spec = runnable_spec(frontend);
            let response =
                tokio::time::timeout(Duration::from_secs(5), run_gate_probe(&env, &spec))
                    .await
                    .unwrap_or_else(|_| {
                        panic!("{frontend:?} acquires the gate with no timeout at all")
                    });
            assert!(
                started.elapsed() >= tx_wait_timeout,
                "{frontend:?} gave up before its wait elapsed"
            );
            match response {
                Message::ErrorWithClass { class, message } => {
                    assert_eq!(class, ErrorClass::Timeout, "{frontend:?}: {message}");
                    assert!(
                        message.contains("transaction gate timeout"),
                        "{frontend:?} did not name the gate wait: {message}"
                    );
                }
                other => panic!("{frontend:?} answered without a wire error class: {other:?}"),
            }
            assert!(
                metrics
                    .render()
                    .contains(&format!("powdb_tx_gate_timeouts_total {}", waited + 1)),
                "{frontend:?} timed out on the gate without counting it"
            );
        }
        drop(held_reader);
    }

    /// Poison the engine lock so any read of it fails. Every sync dispatch
    /// function reaches the engine through `sync_context`, so after this the
    /// ONLY answers they can still give are the ones that need no engine at
    /// all, plus `SyncContext` itself.
    fn poison_engine_lock(engine: &Arc<RwLock<Engine>>) {
        let poisoner = Arc::clone(engine);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.write().expect("lock is not poisoned yet");
            panic!("poisoning the engine lock on purpose (expected in this test)");
        })
        .join();
        assert!(
            engine.read().is_err(),
            "the engine lock did not end up poisoned; this test proves nothing without it"
        );
    }

    /// One-field-at-a-time sweep from this frontend's runnable frame. A refusal
    /// keyed on any single request field shows up here.
    fn sync_corpus(frontend: GateFrontend) -> Vec<SyncSlot> {
        let base = runnable_spec(frontend)
            .sync
            .expect("a sync frontend has a slot");
        let mut out = vec![base.clone()];
        let mut push = |mutate: &dyn Fn(&mut SyncSlot)| {
            let mut slot = base.clone();
            mutate(&mut slot);
            out.push(slot);
        };

        for authenticated in [false, true] {
            push(&|slot: &mut SyncSlot| slot.credential_authenticated = authenticated);
        }
        for role in ["readonly", "admin", "no-such-role"] {
            push(&|slot: &mut SyncSlot| slot.principal = principal(role));
        }
        push(&|slot: &mut SyncSlot| slot.principal = None);
        let long_id = "x".repeat(129);
        for replica_id in ["", "replica-a", "bad id!", long_id.as_str()] {
            push(&|slot: &mut SyncSlot| slot.replica_id = replica_id.to_string());
        }
        if base.pull_bounds.is_some() {
            for units in [
                0u32,
                1,
                MAX_SYNC_PULL_UNITS,
                MAX_SYNC_PULL_UNITS + 1,
                u32::MAX,
            ] {
                push(&|slot: &mut SyncSlot| {
                    if let Some(bounds) = slot.pull_bounds.as_mut() {
                        bounds.0 = units;
                    }
                });
            }
            for bytes in [
                0u64,
                1,
                MAX_SYNC_PULL_BYTES,
                MAX_SYNC_PULL_BYTES + 1,
                u64::MAX,
            ] {
                push(&|slot: &mut SyncSlot| {
                    if let Some(bounds) = slot.pull_bounds.as_mut() {
                        bounds.1 = bytes;
                    }
                });
            }
        }
        if base.pull_identity.is_some() {
            for since_lsn in [0u64, 1, 2, 3, 4, 5, 6, 7, 8, u64::MAX] {
                push(&|slot: &mut SyncSlot| {
                    if let Some(identity) = slot.pull_identity.as_mut() {
                        identity.since_lsn = since_lsn;
                    }
                });
            }
            for database_id in [[0u8; 16], [0xffu8; 16]] {
                push(&|slot: &mut SyncSlot| {
                    if let Some(identity) = slot.pull_identity.as_mut() {
                        identity.database_id = database_id;
                    }
                });
            }
            for generation in [0u64, 1, u64::MAX] {
                push(&|slot: &mut SyncSlot| {
                    if let Some(identity) = slot.pull_identity.as_mut() {
                        identity.primary_generation = generation;
                    }
                });
            }
            for wal in [0u16, 1, u16::MAX] {
                push(&|slot: &mut SyncSlot| {
                    if let Some(identity) = slot.pull_identity.as_mut() {
                        identity.wal_format_version = wal;
                    }
                });
            }
            for catalog in [0u16, 5, 6, 7, u16::MAX] {
                push(&|slot: &mut SyncSlot| {
                    if let Some(identity) = slot.pull_identity.as_mut() {
                        identity.catalog_version = catalog;
                    }
                });
            }
            for segment in [0u16, 1, u16::MAX] {
                push(&|slot: &mut SyncSlot| {
                    if let Some(identity) = slot.pull_identity.as_mut() {
                        identity.segment_format_version = segment;
                    }
                });
            }
        }
        if base.ack_lsns.is_some() {
            for lsns in [
                (0u64, 0u64),
                (0, 1),
                (1, 0),
                (1, 1),
                (5, 0),
                (u64::MAX, 0),
                (0, u64::MAX),
                (u64::MAX, u64::MAX),
            ] {
                push(&|slot: &mut SyncSlot| slot.ack_lsns = Some(lsns));
            }
        }
        out
    }

    /// PARITY, the other direction: a refusal the dispatch functions can reach
    /// WITHOUT reading the engine must also be a pre-gate refusal.
    ///
    /// This is the runtime half of the guard against a pure rejection sitting
    /// under the gate. With the engine lock poisoned every engine read fails,
    /// so any answer other than `SyncContext` is by construction one the server
    /// could have given before taking a single permit. If the pre-gate does not
    /// give that same answer, the frame takes the entire gate on its way to
    /// being refused, which is exactly the outage this whole section exists to
    /// prevent.
    #[test]
    fn every_engine_free_sync_refusal_is_also_a_pre_gate_refusal() {
        let (_dir, engine) = one_row_engine();
        poison_engine_lock(&engine);

        let mut observed_pure: BTreeSet<&'static str> = BTreeSet::new();
        let mut saw_engine_dependent = false;
        for frontend in sync_frontends() {
            for slot in sync_corpus(frontend) {
                let decision = dispatch_sync_decision(&engine, frontend, &slot);
                let class = decision.error_class.unwrap_or_else(|| {
                    panic!(
                        "{frontend:?} answered successfully with a poisoned engine: {:?}",
                        decision.message
                    )
                });
                if class == SyncErrorClass::SyncContext {
                    saw_engine_dependent = true;
                    continue;
                }
                observed_pure.insert(class.as_label());
                match sync_pre_gate(frontend, &slot)
                    .check(slot.credential_authenticated, slot.principal.as_ref())
                {
                    Err((pre_gate_class, _)) => assert_eq!(
                        pre_gate_class, class,
                        "{frontend:?} refuses replica {:?} as {class:?} without reading the \
                         engine, but the pre-gate refuses it as {pre_gate_class:?}",
                        slot.replica_id
                    ),
                    Ok(()) => panic!(
                        "{frontend:?} refuses replica {:?} as {class:?} without ever reading the \
                         engine, and the pre-gate lets it through: that refusal runs under the \
                         whole transaction gate. Move the check into `SyncPreGate::check`, add a \
                         `GateRule` for it, and the matrix will probe it on every frontend it \
                         reaches.",
                        slot.replica_id
                    ),
                }
            }
        }
        assert!(
            saw_engine_dependent,
            "no corpus entry reached the engine, so this test proved nothing"
        );
        for rule in GateRule::ALL.iter().copied() {
            if let Some(class) = rule.pre_gate_class() {
                assert!(
                    observed_pure.contains(class.as_label()),
                    "{rule:?} declares the pre-gate class {class:?} but no corpus frame produced \
                     it, so the sweep does not actually cover that rule"
                );
            }
        }
    }

    /// Every rejection site inside the gated sync path, declared.
    ///
    /// `(function, SyncErrorClass, occurrences)`. The test below reads this
    /// file's own source and rebuilds the same table; a mismatch means someone
    /// added, removed, or moved a way for a sync frame to be refused.
    ///
    /// This exists because the wire-level and pre-gate matrices above can only
    /// probe rules they know about. A BRAND-NEW refusal added inside a dispatch
    /// function is invisible to both of them: it is decided under the gate, so
    /// a frame that will be refused in microseconds still waits out another
    /// connection's entire transaction first. Failing the build until the
    /// author classifies the new site is the only way that stays caught.
    ///
    /// When this test fails: if the new refusal needs NO engine access, move it
    /// into `SyncPreGate::check` and add a `GateRule` for it, which makes the
    /// matrix probe it on every frontend it reaches. If it genuinely needs the
    /// engine, it belongs under the gate, and updating this table is the whole
    /// fix.
    const DECLARED_SYNC_REJECTION_SITES: &[(&str, &str, usize)] = &[
        ("acquire_sync_permit", "GateTimeout", 1),
        ("acquire_sync_permit", "QueryExecution", 1),
        ("classify_sync_ack_failure", "AckRejected", 1),
        ("classify_sync_ack_failure", "AckUpdate", 1),
        ("dispatch_sync_ack_decision", "AckValidation", 1),
        ("dispatch_sync_ack_decision", "LsnAheadOfRemote", 1),
        ("dispatch_sync_ack_decision", "SyncContext", 1),
        ("dispatch_sync_pull_decision", "CursorLsnMismatch", 1),
        ("dispatch_sync_pull_decision", "IdentityOrFormatMismatch", 2),
        ("dispatch_sync_pull_decision", "IdentityRead", 1),
        ("dispatch_sync_pull_decision", "InvalidMaxBytes", 1),
        (
            "dispatch_sync_pull_decision",
            "RetainedChunkNotApplyable",
            1,
        ),
        ("dispatch_sync_pull_decision", "RetainedRead", 1),
        ("dispatch_sync_pull_decision", "RetainedUnitEncoding", 1),
        ("dispatch_sync_pull_decision", "StatusRead", 1),
        ("dispatch_sync_pull_decision", "SyncContext", 1),
        ("dispatch_sync_status_decision", "StatusRead", 1),
        ("dispatch_sync_status_decision", "SyncContext", 1),
        ("execute_gated_sync", "ActiveTransaction", 1),
        ("run_blocking_sync", "Internal", 2),
    ];

    /// Everything a sync dispatch function may do before it reads the engine,
    /// declared: clone what it needs for the pre-gate, consult the pre-gate,
    /// and build the pre-gate's refusal. Nothing else.
    ///
    /// `Err` is on the list because the scan below is syntactic and cannot
    /// tell `Err(..)` in a pattern from a call; it decides nothing either way.
    const PRE_ENGINE_CALLS_ALLOWED_IN_SYNC_DISPATCH: &[&str] =
        &["Err", "SyncDecision::error", "check", "clone"];

    /// The three dispatch functions run under the whole transaction gate, so a
    /// refusal they make BEFORE they read the engine is a refusal another
    /// connection's open transaction can delay by minutes. Structurally there
    /// may be exactly one such refusal per function, and it must be the shared
    /// pre-gate the wire path already applied.
    ///
    /// WHAT THIS CATCHES. A new refusal written into one of these three
    /// functions ahead of the engine read, in each form it can reach source:
    /// a second `SyncDecision::error(`; a second early `return` (any early exit
    /// from a function returning `SyncDecision` needs one, since there is no
    /// `?` to hide behind); or a call to a helper holding either. The last one
    /// is why the permitted calls are declared rather than counted: moving the
    /// refusal behind `fn refuse_x() -> SyncDecision` or
    /// `fn refuse_x(..) -> Option<SyncDecision>` changes nothing this test can
    /// see about `SyncDecision::error`, but it cannot avoid naming `refuse_x`
    /// here.
    ///
    /// WHAT THIS CANNOT CATCH. It reads this file's text, so a refusal added
    /// inside something it already permits is invisible to it: a new check
    /// inside `SyncPreGate::check`, or inside `sync_context`, or in another
    /// module entirely. The pre-gate is the sanctioned place for exactly that,
    /// and `every_pre_gate_class_has_a_rule_the_matrix_walks` forces every
    /// class it can answer with to have a `GateRule` beside it, which is what
    /// makes the matrix probe it. So this test is a fence around the one region
    /// the matrices cannot reach, not a proof that no free refusal exists
    /// anywhere. The load-bearing half of this section is the runtime parity
    /// matrix: `every_sync_pre_gate_rule_is_checked_on_every_sync_frontend`
    /// and the wire-level matrix above it, which probe behavior on every
    /// frontend instead of reading text.
    #[test]
    fn no_sync_dispatch_function_refuses_a_frame_before_it_reads_the_engine() {
        let src = include_str!("handler.rs");
        for name in [
            "dispatch_sync_status_decision",
            "dispatch_sync_pull_decision",
            "dispatch_sync_ack_decision",
        ] {
            let body = top_level_fn_body(src, name);
            let engine_read = body.find("sync_context(engine)").unwrap_or_else(|| {
                panic!("{name} no longer reads the engine through sync_context")
            });
            let pre_gate_check = body
                .find("pre_gate.check(credential_authenticated, principal)")
                .unwrap_or_else(|| {
                    panic!(
                        "{name} no longer delegates to `SyncPreGate::check`, so the pre-gate and \
                         the gated path can drift apart again"
                    )
                });
            assert!(
                pre_gate_check < engine_read,
                "{name} consults the pre-gate only after reading the engine"
            );
            let before_engine = &body[..engine_read];

            let early = before_engine.match_indices("SyncDecision::error(").count();
            assert_eq!(
                early, 1,
                "{name} refuses a frame {early} times before it reads the engine. Only the shared \
                 pre-gate may do that: any other refusal there needs no engine access, which \
                 means it is being decided while this frame holds the whole transaction gate. \
                 Move it into `SyncPreGate::check` and give it a `GateRule`, and the parity \
                 matrix will probe it on every frontend it reaches."
            );

            let returns = word_occurrences(before_engine, "return");
            assert_eq!(
                returns, 1,
                "{name} leaves itself {returns} times before it reads the engine. Exactly one \
                 early exit may live there, the pre-gate's; a second one is a refusal decided \
                 while this frame holds the whole transaction gate, even when the decision \
                 itself was made inside a helper."
            );

            let mut unexpected: Vec<String> = called_names(before_engine)
                .into_iter()
                .filter(|called| {
                    called != name
                        && !PRE_ENGINE_CALLS_ALLOWED_IN_SYNC_DISPATCH.contains(&&**called)
                })
                .collect();
            unexpected.sort();
            unexpected.dedup();
            assert!(
                unexpected.is_empty(),
                "{name} calls {unexpected:?} before it reads the engine. Whatever those decide is \
                 decided while this frame holds the whole transaction gate. If they can refuse \
                 the frame, the refusal belongs in `SyncPreGate::check` with a `GateRule` beside \
                 it; if they genuinely cannot, add them to \
                 PRE_ENGINE_CALLS_ALLOWED_IN_SYNC_DISPATCH and say why."
            );
        }
    }

    /// Occurrences of `word` in `src` that are whole tokens, so `return` does
    /// not match `returns` and `fn` does not match `fnord`.
    fn word_occurrences(src: &str, word: &str) -> usize {
        let bytes = src.as_bytes();
        let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        src.match_indices(word)
            .filter(|(idx, _)| {
                let before_ok = *idx == 0 || !is_ident(bytes[idx - 1]);
                let after = idx + word.len();
                let after_ok = after == bytes.len() || !is_ident(bytes[after]);
                before_ok && after_ok
            })
            .count()
    }

    /// Every name that is CALLED in `src`: `foo(` as `foo`, `Type::assoc(` as
    /// `Type::assoc`, `x.method(` as `method`, and `mac!(` as `mac!`.
    ///
    /// Deliberately syntactic. It is not trying to understand the code, only
    /// to make a function that appears in a region where nothing may decide
    /// anything impossible to add silently.
    fn called_names(src: &str) -> Vec<String> {
        let bytes = src.as_bytes();
        let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        let mut names = Vec::new();
        for (idx, byte) in bytes.iter().enumerate() {
            if *byte != b'(' {
                continue;
            }
            let mut end = idx;
            let macro_call = end > 0 && bytes[end - 1] == b'!';
            if macro_call {
                end -= 1;
            }
            let mut start = end;
            while start > 0 && is_ident(bytes[start - 1]) {
                start -= 1;
            }
            if start == end {
                // A `(` that opens a group, a tuple, or an argument list.
                continue;
            }
            // Keep any `Type::` qualification, so `SyncDecision::error` cannot
            // be permitted by declaring a bare `error`.
            let mut path = start;
            while path >= 2 && bytes[path - 1] == b':' && bytes[path - 2] == b':' {
                let mut segment = path - 2;
                while segment > 0 && is_ident(bytes[segment - 1]) {
                    segment -= 1;
                }
                if segment == path - 2 {
                    break;
                }
                path = segment;
            }
            let mut name = src[path..end].to_string();
            if macro_call {
                name.push('!');
            }
            names.push(name);
        }
        names
    }

    /// The body of an `impl` block, from its header to the closing brace in
    /// column 0.
    fn impl_block<'a>(src: &'a str, header: &str) -> &'a str {
        let start = src
            .find(header)
            .unwrap_or_else(|| panic!("`{header}` is not in handler.rs"));
        let rest = &src[start..];
        let end = rest
            .find("\n}\n")
            .unwrap_or_else(|| panic!("`{header}` has no closing brace in column 0"));
        &rest[..end]
    }

    /// Every class the pre-gate can answer with must have a `GateRule`, and
    /// every `GateRule` class must be one the pre-gate can answer with.
    ///
    /// Without this, a new pre-gate check would be safe from the gate but
    /// invisible to the matrix, so it could be applied to one sync frontend and
    /// forgotten on the other two: the exact partial application this section
    /// exists to make impossible.
    #[test]
    fn every_pre_gate_class_has_a_rule_the_matrix_walks() {
        let src = include_str!("handler.rs");
        let mut produced: BTreeSet<String> = BTreeSet::new();
        let mut scan = |body: &str| {
            for (idx, _) in body.match_indices("SyncErrorClass::") {
                let tail = &body[idx + "SyncErrorClass::".len()..];
                produced.insert(
                    tail.chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect(),
                );
            }
        };
        scan(impl_block(src, "\nimpl SyncPreGate {"));
        for name in [
            "check_sync_protocol_permitted",
            "check_sync_pull_bounds",
            "check_sync_ack_lsn_bounds",
        ] {
            scan(top_level_fn_body(src, name));
        }

        let declared: BTreeSet<String> = GateRule::ALL
            .iter()
            .filter_map(|rule| rule.pre_gate_class())
            .map(|class| format!("{class:?}"))
            .collect();
        assert_eq!(
            produced, declared,
            "the pre-gate and the rule enumeration disagree about what can be refused for free. \
             Every class on the left needs a `GateRule`, which is what makes the matrix probe it \
             on every frontend it reaches; every class on the right must actually be reachable \
             from the pre-gate."
        );
    }

    /// The body of a top-level function, from its signature to the closing
    /// brace in column 0.
    ///
    /// The name is matched as a whole token: `fn run_blocking_sync` must not
    /// resolve to `fn run_blocking_sync_preflight` declared earlier in the
    /// file. A prefix match there is not a near miss, it is a hole: the guards
    /// below would then count rejection sites in a decoy and never look at the
    /// function they name, and every one of them would pass while saying
    /// nothing.
    fn top_level_fn_body<'a>(src: &'a str, name: &str) -> &'a str {
        let bytes = src.as_bytes();
        let mut starts: Vec<usize> = Vec::new();
        for keyword in [
            "\nfn ",
            "\nasync fn ",
            "\npub fn ",
            "\npub async fn ",
            "\npub(crate) fn ",
            "\npub(crate) async fn ",
        ] {
            let needle = format!("{keyword}{name}");
            for (idx, _) in src.match_indices(&needle) {
                // The declaration ends the name here, rather than continuing
                // it: `(` for a plain function, `<` for a generic one.
                match bytes.get(idx + needle.len()) {
                    Some(b'(') | Some(b'<') => starts.push(idx),
                    _ => {}
                }
            }
        }
        starts.sort_unstable();
        starts.dedup();
        assert!(
            starts.len() <= 1,
            "`fn {name}` is declared {} times at the top level of handler.rs; the guards below \
             would inspect one of them and ignore the rest",
            starts.len()
        );
        let start = starts.first().copied().unwrap_or_else(|| {
            panic!("`fn {name}` is not in handler.rs; the rejection-site guard cannot see it")
        });
        let rest = &src[start + 1..];
        let end = rest
            .find("\n}\n")
            .unwrap_or_else(|| panic!("`fn {name}` has no closing brace in column 0"));
        &rest[..end]
    }

    #[test]
    fn every_gated_sync_rejection_site_is_declared() {
        let src = include_str!("handler.rs");
        let mut actual: BTreeMap<(&str, String), usize> = BTreeMap::new();
        for name in [
            "acquire_sync_permit",
            "classify_sync_ack_failure",
            "dispatch_sync_ack_decision",
            "dispatch_sync_pull_decision",
            "dispatch_sync_status_decision",
            "execute_gated_sync",
            "run_blocking_sync",
        ] {
            let body = top_level_fn_body(src, name);
            for (idx, _) in body.match_indices("SyncErrorClass::") {
                let tail = &body[idx + "SyncErrorClass::".len()..];
                let class: String = tail
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                *actual.entry((name, class)).or_insert(0) += 1;
            }
        }

        let declared: BTreeMap<(&str, String), usize> = DECLARED_SYNC_REJECTION_SITES
            .iter()
            .map(|(function, class, count)| ((*function, (*class).to_string()), *count))
            .collect();

        if actual != declared {
            let rendered = actual
                .iter()
                .map(|((function, class), count)| {
                    format!("        (\"{function}\", \"{class}\", {count}),")
                })
                .collect::<Vec<_>>()
                .join("\n");
            panic!(
                "the gated sync path gained, lost, or moved a way to refuse a frame.\n\n\
                 If the new refusal needs NO engine access it must move into \
                 `SyncPreGate::check` with a `GateRule` beside it, so the parity matrix probes it \
                 on every frontend it reaches; a pure refusal decided under the gate makes a \
                 frame wait out another connection's whole transaction before being told no.\n\
                 If it genuinely needs the engine, paste this over \
                 DECLARED_SYNC_REJECTION_SITES:\n\n{rendered}\n"
            );
        }
    }

    #[tokio::test]
    async fn forbidden_write_is_denied_without_acquiring_any_permit() {
        let (_dir, engine) = one_row_engine();
        // A single-permit gate whose only permit is already held: writer
        // admission takes the whole gate, so any acquisition would have to
        // wait out the full tx_wait_timeout below.
        let gate = new_tx_gate_with_permits(1);
        let metrics = Arc::new(Metrics::new());
        let held_reader = acquire_autocommit_permit(
            &gate,
            AdmissionMode::Reader,
            Duration::from_secs(1),
            &metrics,
        )
        .await
        .expect("held reader admission");
        assert_eq!(gate.available_permits(), 0);

        let (_client, server) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(server);
        let mut wire_read_buffer = Vec::new();
        let mut pending_messages = InFlightReadAhead::default();
        let mut tx_permit = None;
        let (message, ticket, termination) = tokio::time::timeout(
            Duration::from_millis(250),
            execute_wire_query(
                engine,
                gate.clone(),
                &mut tx_permit,
                "insert User { id := 2 }".into(),
                WireResultMode::Native,
                principal("readonly"),
                Duration::from_secs(2),
                Duration::from_secs(10),
                &metrics,
                &mut reader,
                &mut wire_read_buffer,
                &mut pending_messages,
            ),
        )
        .await
        .expect("a statement the principal may not run must never wait on the gate");

        match message {
            Message::ErrorWithClass { message, class } => {
                assert!(
                    message.contains("permission denied"),
                    "unexpected message: {message}"
                );
                assert_eq!(class, ErrorClass::Execution);
            }
            other => panic!("expected a typed permission error, got {other:?}"),
        }
        assert!(ticket.is_none());
        assert!(termination.is_none());
        assert!(tx_permit.is_none());
        assert_eq!(
            gate.available_permits(),
            0,
            "a statement the principal may not run must acquire nothing"
        );
        drop(held_reader);
        assert_eq!(gate.available_permits(), 1);
        assert!(metrics
            .render()
            .contains("powdb_queries_total{result=\"error\"} 1"));
    }

    #[tokio::test]
    async fn forbidden_write_is_denied_without_a_permit_on_every_frontend() {
        let (_dir, engine) = one_row_engine();
        let gate = new_tx_gate_with_permits(1);
        let metrics = Arc::new(Metrics::new());
        let _held_reader = acquire_autocommit_permit(
            &gate,
            AdmissionMode::Reader,
            Duration::from_secs(1),
            &metrics,
        )
        .await
        .expect("held reader admission");

        let (_sql_client, sql_server) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(sql_server);
        let mut wire_read_buffer = Vec::new();
        let mut pending_messages = InFlightReadAhead::default();
        let mut tx_permit = None;
        let (message, _, _) = tokio::time::timeout(
            Duration::from_millis(250),
            execute_wire_query_sql(
                Arc::clone(&engine),
                gate.clone(),
                &mut tx_permit,
                "INSERT INTO User (id) VALUES (2)".into(),
                WireResultMode::Native,
                principal("readonly"),
                Duration::from_secs(2),
                Duration::from_secs(10),
                &metrics,
                &mut reader,
                &mut wire_read_buffer,
                &mut pending_messages,
            ),
        )
        .await
        .expect("a forbidden SQL write must never wait on the gate");
        assert!(
            matches!(
                &message,
                Message::ErrorWithClass { message, class: ErrorClass::Execution }
                    if message.contains("permission denied")
            ),
            "unexpected SQL response: {message:?}"
        );

        let (_params_client, params_server) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(params_server);
        let mut wire_read_buffer = Vec::new();
        let mut pending_messages = InFlightReadAhead::default();
        let mut tx_permit = None;
        let (message, _, _) = tokio::time::timeout(
            Duration::from_millis(250),
            execute_wire_query_with_params(
                engine,
                gate.clone(),
                &mut tx_permit,
                "insert User { id := $1 }".into(),
                vec![WireParam::Int(2)],
                WireResultMode::Native,
                principal("readonly"),
                Duration::from_secs(2),
                Duration::from_secs(10),
                &metrics,
                &mut reader,
                &mut wire_read_buffer,
                &mut pending_messages,
            ),
        )
        .await
        .expect("a forbidden parameterized write must never wait on the gate");
        assert!(
            matches!(
                &message,
                Message::ErrorWithClass { message, class: ErrorClass::Execution }
                    if message.contains("permission denied")
            ),
            "unexpected parameterized response: {message:?}"
        );
        assert_eq!(gate.available_permits(), 0);
    }

    #[tokio::test]
    async fn unparsable_flood_cannot_starve_a_concurrent_reader() {
        let (_dir, engine) = one_row_engine();
        let gate = new_tx_gate_with_permits(2);
        let metrics = Arc::new(Metrics::new());
        // One connection is mid-read and holds reader admission, so writer
        // admission (the whole gate) stays unavailable for as long as it runs.
        let _held_reader = acquire_autocommit_permit(
            &gate,
            AdmissionMode::Reader,
            Duration::from_secs(1),
            &metrics,
        )
        .await
        .expect("held reader admission");

        let flood_engine = Arc::clone(&engine);
        let flood_gate = gate.clone();
        let flood_metrics = Arc::clone(&metrics);
        let flood = tokio::spawn(async move {
            let (_client, server) = tokio::io::duplex(1024);
            let mut reader = BufReader::new(server);
            let mut wire_read_buffer = Vec::new();
            let mut pending_messages = InFlightReadAhead::default();
            let mut tx_permit = None;
            for _ in 0..64 {
                let (message, _, _) = execute_wire_query(
                    Arc::clone(&flood_engine),
                    flood_gate.clone(),
                    &mut tx_permit,
                    ")))".into(),
                    WireResultMode::Native,
                    None,
                    Duration::from_secs(2),
                    Duration::from_secs(10),
                    &flood_metrics,
                    &mut reader,
                    &mut wire_read_buffer,
                    &mut pending_messages,
                )
                .await;
                assert!(
                    matches!(
                        message,
                        Message::ErrorWithClass {
                            class: ErrorClass::Parse,
                            ..
                        }
                    ),
                    "unexpected flood response: {message:?}"
                );
            }
        });
        // Give the flood time to queue for the gate if it takes admission.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let (_reader_client, reader_server) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(reader_server);
        let mut wire_read_buffer = Vec::new();
        let mut pending_messages = InFlightReadAhead::default();
        let mut tx_permit = None;
        let (message, _, _) = tokio::time::timeout(
            Duration::from_millis(500),
            execute_wire_query(
                engine,
                gate.clone(),
                &mut tx_permit,
                "User filter .id = 1".into(),
                WireResultMode::Native,
                None,
                Duration::from_secs(2),
                Duration::from_secs(10),
                &metrics,
                &mut reader,
                &mut wire_read_buffer,
                &mut pending_messages,
            ),
        )
        .await
        .expect("a real reader must complete while unparsable frames flood another connection");
        assert!(matches!(message, Message::ResultRowsNative { .. }));
        flood.await.expect("flood task");
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

    // ---- Wire classes for the typed storage refusals ----

    #[test]
    fn ddl_inside_a_transaction_reaches_clients_as_a_client_error() {
        let (_dir, engine) = one_row_engine();
        let (begin, _) = dispatch_query(&engine, "begin", None, true);
        begin.expect("begin");

        let (result, _) = dispatch_query(&engine, "drop User", None, true);
        let err = result.expect_err("DDL inside an explicit transaction must be refused");
        assert_eq!(
            classify_query_error(&err),
            ErrorClass::Execution,
            "refusing DDL because the connection is mid-transaction is the client's mistake; \
             ErrorClass::Internal tells the driver it hit a server bug it cannot act on"
        );
        // The class is only half of it: the guidance has to survive egress
        // sanitization or the client is told to act on nothing.
        assert!(
            sanitize_error(&err.to_string()).contains("DDL is not transactional"),
            "guidance was masked: {err}"
        );
    }

    #[test]
    fn transaction_over_the_dirty_page_budget_reaches_clients_as_a_limit() {
        // The refusal the heap raises (crates/storage/src/heap.rs) after the
        // query crate erases its type: `QueryError::StorageError` carries only
        // the rendered message, which is all the server ever sees.
        let raised = std::io::Error::new(
            std::io::ErrorKind::OutOfMemory,
            StorageError::TransactionTooLarge {
                pages: 65_536,
                limit_bytes: 268_435_456,
            },
        );
        let err = QueryError::StorageError(raised.to_string());
        assert_eq!(
            classify_query_error(&err),
            ErrorClass::LimitExceeded,
            "a transaction refused by the dirty-page budget is a resource limit, the same \
             class MemoryLimitExceeded already carries"
        );
        assert_eq!(
            sanitize_error(&err.to_string()),
            err.to_string(),
            "the budget message names the limit and the remedy; it must cross verbatim"
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

    /// A replica has exactly two "rebootstrap required" answers to branch on,
    /// one from the pull side and one from the ack side, and they must arrive
    /// as the SAME wire class. The ack one used to be reported as `Internal`,
    /// which a driver is told to treat as "server bug, nothing you can fix",
    /// so a replica whose cursor was gone or deactivated could not tell that
    /// answer apart from an unclassified server fault.
    #[test]
    fn a_refused_cursor_advance_is_classified_like_its_pull_side_twin() {
        for kind in [
            std::io::ErrorKind::NotFound,     // cursor not found
            std::io::ErrorKind::InvalidInput, // cursor inactive, or LSN behind
        ] {
            let err = std::io::Error::new(kind, "replica cursor not found; rebootstrap required");
            let class = classify_sync_ack_failure(&err);
            assert_eq!(class, SyncErrorClass::AckRejected, "{kind:?}");
            assert_eq!(
                class.wire_class(),
                SyncErrorClass::IdentityOrFormatMismatch.wire_class(),
                "the two rebootstrap answers must reach the replica as one class"
            );
            assert_ne!(class.wire_class(), ErrorClass::Internal);
        }
        // A real I/O failure is still the server's problem, not the replica's.
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "disk is read-only");
        assert_eq!(classify_sync_ack_failure(&io), SyncErrorClass::AckUpdate);
        assert_eq!(
            SyncErrorClass::AckUpdate.wire_class(),
            ErrorClass::Internal,
            "an I/O failure the replica cannot act on stays unclassified"
        );
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
            Message::ErrorWithClass { message, class } => {
                assert!(message.contains("requires authentication"));
                assert_eq!(class, ErrorClass::AuthFailed);
            }
            other => panic!("expected auth error, got {other:?}"),
        }

        let readonly = Principal {
            name: "reader".into(),
            role: "readonly".into(),
        };
        match dispatch_sync_status(&engine, "replica-a".into(), true, Some(&readonly)) {
            Message::ErrorWithClass { message, class } => {
                assert!(message.contains("permission denied"));
                // Same class the query frontends give a role refusal.
                assert_eq!(class, ErrorClass::Execution);
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
            Message::ErrorWithClass { message, .. } => {
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
            Message::ErrorWithClass { message, .. } => {
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
            Message::ErrorWithClass { message, .. } => {
                assert!(message.contains("cuts through transaction"))
            }
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
            Message::ErrorWithClass { message, .. } => {
                assert!(message.contains("cuts through transaction"))
            }
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
            Message::ErrorWithClass { message, .. } => {
                assert!(message.contains("cuts through transaction"))
            }
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
            Message::ErrorWithClass { message, .. } => assert!(message.contains("does not match")),
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
            Message::ErrorWithClass { message, .. } => {
                assert!(message.contains("rebootstrap required"))
            }
            other => panic!("expected format mismatch error, got {other:?}"),
        }
    }
}
