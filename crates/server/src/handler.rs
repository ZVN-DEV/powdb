use crate::protocol::Message;
use powdb_query::executor::{is_read_only_statement, Engine, READONLY_NEEDS_WRITE};
use powdb_query::parser;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

/// Tracks per-IP authentication failure counts for rate limiting.
pub type AuthRateLimiter = Arc<Mutex<HashMap<IpAddr, (u32, Instant)>>>;

/// Maximum query text length accepted from the wire (1 MB).
const MAX_QUERY_LENGTH: usize = 1024 * 1024;

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

/// Options for a single connection, bundled to keep `handle_connection`'s
/// argument list short.
pub struct ConnOpts<'a> {
    pub engine: Arc<RwLock<Engine>>,
    pub expected_password: Option<String>,
    pub shutdown_rx: &'a mut watch::Receiver<bool>,
    pub idle_timeout: Duration,
    pub query_timeout: Duration,
    pub rate_limiter: Option<&'a AuthRateLimiter>,
    pub peer_addr: Option<std::net::SocketAddr>,
}

/// Execute a query against the engine under the RwLock. Read-only
/// statements acquire `.read()` so concurrent SELECTs can scan in
/// parallel; mutations acquire `.write()`.
fn dispatch_query(engine: &Arc<RwLock<Engine>>, query: &str) -> Result<QueryResult, String> {
    let stmt_result = parser::parse(query).map_err(|e| e.to_string());

    let can_try_read = matches!(&stmt_result, Ok(s) if is_read_only_statement(s));
    if can_try_read {
        let res = {
            let eng = engine.read().map_err(|e| format!("lock poisoned: {e}"))?;
            eng.execute_powql_readonly(query)
        };
        match res {
            Ok(r) => return Ok(r),
            Err(e) if e == READONLY_NEEDS_WRITE => {
                // Escalate: fall through to the write path below.
            }
            Err(e) => return Err(e),
        }
    }

    let mut eng = engine.write().map_err(|e| format!("lock poisoned: {e}"))?;
    eng.execute_powql(query)
}

pub async fn handle_connection<S>(stream: S, opts: ConnOpts<'_>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let ConnOpts {
        engine,
        expected_password,
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
    let connect_msg = loop {
        match tokio::time::timeout(idle_timeout, Message::read_from(&mut reader)).await {
            Ok(Ok(Some(Message::Ping))) => {
                debug!(peer = %peer, "pre-auth ping");
                let pong = Message::Pong;
                if pong.write_to(&mut writer).await.is_err() {
                    return;
                }
                if writer.flush().await.is_err() {
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

    match connect_msg {
        Message::Connect { db_name, password } => {
            // Check rate limiting before verifying credentials.
            if let (Some(limiter), Some(ip)) = (rate_limiter, peer_ip) {
                if is_rate_limited(limiter, ip) {
                    warn!(peer = %peer, "rate limited: too many auth failures");
                    let err = Message::Error {
                        message: "too many auth failures, try again later".into(),
                    };
                    err.write_to(&mut writer).await.ok();
                    writer.flush().await.ok();
                    return;
                }
            }

            if let Some(expected) = &expected_password {
                if !password
                    .as_deref()
                    .is_some_and(|p| constant_time_eq(p.as_bytes(), expected.as_bytes()))
                {
                    warn!(peer = %peer, db = %db_name, "auth rejected: bad password");
                    // Record the failure for rate limiting.
                    if let (Some(limiter), Some(ip)) = (rate_limiter, peer_ip) {
                        record_auth_failure(limiter, ip);
                    }
                    let err = Message::Error {
                        message: "authentication failed".into(),
                    };
                    err.write_to(&mut writer).await.ok();
                    writer.flush().await.ok();
                    return;
                }
            }
            // Auth succeeded — clear any prior failure count.
            if let (Some(limiter), Some(ip)) = (rate_limiter, peer_ip) {
                clear_auth_failures(limiter, ip);
            }
            info!(peer = %peer, db = %db_name, "client connected");
            let ok = Message::ConnectOk {
                version: env!("CARGO_PKG_VERSION").into(),
            };
            if ok.write_to(&mut writer).await.is_err() {
                return;
            }
            if writer.flush().await.is_err() {
                return;
            }
        }
        _ => {
            warn!(peer = %peer, "first message was not CONNECT");
            let err = Message::Error {
                message: "expected CONNECT".into(),
            };
            err.write_to(&mut writer).await.ok();
            writer.flush().await.ok();
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
                        err.write_to(&mut writer).await.ok();
                        writer.flush().await.ok();
                        break;
                    }
                }
            }
            // If server is shutting down, notify client and close.
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!(peer = %peer, "server shutting down, closing connection");
                    let err = Message::Error { message: "server shutting down".into() };
                    err.write_to(&mut writer).await.ok();
                    writer.flush().await.ok();
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
                    let result = tokio::task::spawn_blocking({
                        let engine = engine.clone();
                        let query = query.clone();
                        move || dispatch_query(&engine, &query)
                    });
                    match tokio::time::timeout(query_timeout, result).await {
                        Ok(Ok(Ok(result))) => query_result_to_message(result),
                        Ok(Ok(Err(e))) => Message::Error {
                            message: sanitize_error(&e),
                        },
                        Ok(Err(e)) => Message::Error {
                            message: format!("internal error: {e}"),
                        },
                        Err(_) => {
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

        if response.write_to(&mut writer).await.is_err() {
            break;
        }
        if writer.flush().await.is_err() {
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
        QueryResult::Created(_name) => Message::ResultOk { affected: 0 },
        QueryResult::Executed { .. } => Message::ResultOk { affected: 0 },
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
        Value::Empty       => "{}".into(),
    }
}
