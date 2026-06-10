use crate::protocol::Message;
use powdb_auth::{Permission, Role, UserStore};
use powdb_query::executor::{is_read_only_statement, Engine};
use powdb_query::parser;
use powdb_query::result::{QueryError, QueryResult};
use powdb_storage::types::Value;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};
use zeroize::Zeroizing;

/// Tracks per-IP authentication failure counts for rate limiting.
pub type AuthRateLimiter = Arc<Mutex<HashMap<IpAddr, (u32, Instant)>>>;

/// Maximum query text length accepted from the wire (1 MB).
const MAX_QUERY_LENGTH: usize = 1024 * 1024;

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

/// Whether `role` grants the `Write` permission. Unknown role names fail
/// closed (no write). Shared-password / open / embedded modes never construct
/// a [`Principal`], so they are unaffected by this gate.
fn role_can_write(role: &str) -> bool {
    Role::builtin(role).is_some_and(|r| r.allows(Permission::Write))
}

/// Enforce the principal's role against a parsed statement. Returns an error
/// for any non-read statement (insert/update/delete/upsert/DDL/view ops/
/// transaction control) when the role does not grant `Write`.
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
    if is_read_only_statement(stmt) || role_can_write(&p.role) {
        return Ok(());
    }
    Err(QueryError::Execution(format!(
        "permission denied: role '{}' cannot execute write statements",
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

pub async fn handle_connection<S>(stream: S, opts: ConnOpts<'_>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let ConnOpts {
        engine,
        expected_password,
        users,
        shutdown_rx,
        idle_timeout,
        query_timeout,
        rate_limiter,
        peer_addr,
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
                    let handle = tokio::task::spawn_blocking({
                        let engine = engine.clone();
                        let query = query.clone();
                        let principal = principal.clone();
                        move || dispatch_query(&engine, &query, principal.as_ref())
                    });
                    let abort_handle = handle.abort_handle();
                    match tokio::time::timeout(query_timeout, handle).await {
                        Ok(Ok(Ok(result))) => query_result_to_message(result),
                        Ok(Ok(Err(e))) => Message::Error {
                            message: sanitize_error(&e.to_string()),
                        },
                        Ok(Err(e)) => Message::Error {
                            message: format!("internal error: {e}"),
                        },
                        Err(_) => {
                            abort_handle.abort();
                            warn!(peer = %peer, query = %query, "query timeout exceeded");
                            Message::Error {
                                message: "query timeout exceeded".into(),
                            }
                        }
                    }
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

    info!(peer = %peer, "client disconnected");
}

fn query_result_to_message(result: QueryResult) -> Message {
    match result {
        QueryResult::Rows { columns, rows } => {
            let str_rows: Vec<Vec<String>> = rows
                .iter()
                .map(|row| row.iter().map(value_to_display).collect())
                .collect();
            Message::ResultRows {
                columns,
                rows: str_rows,
            }
        }
        QueryResult::Scalar(val) => Message::ResultScalar {
            value: value_to_display(&val),
        },
        QueryResult::Modified(n) => Message::ResultOk { affected: n },
        QueryResult::Created(name) => Message::ResultMessage {
            message: format!("type {name} created"),
        },
        QueryResult::Executed { message } => Message::ResultMessage { message },
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
