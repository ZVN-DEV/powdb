//! PowDB's wire frontend: one connection's handshake, frame loop, and
//! shutdown, plus the modules each frame class is served by.

mod auth;
mod classify;
mod query;
mod sync;
mod transaction;
mod wire;

use crate::metrics::{Metrics, QueryOutcome, SyncOperation};
use crate::protocol::{
    frame_payload_len, negotiate_protocol, stated_client_hello, ErrorClass, Message,
    CLIENT_CATALOG_VERSION, MAX_SUPPORTED_PROTOCOL_VERSION, MIN_SUPPORTED_PROTOCOL_VERSION,
    SERVER_FEATURES,
};
use powdb_auth::UserStore;
use powdb_query::executor::{Engine, WalDurabilityTicket};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, BufReader, BufWriter};
use tokio::sync::{watch, OwnedSemaphorePermit};
use tracing::{debug, error, info, warn};
use zeroize::Zeroizing;

pub use self::auth::{
    authenticate_connect, new_rate_limiter, AuthOutcome, AuthRateLimiter, Principal,
};
pub use self::transaction::{
    new_tx_gate, new_tx_gate_with_max_tx_lifetime, new_tx_gate_with_permits, TxGate,
    DEFAULT_TX_GATE_READER_PERMITS, DEFAULT_TX_MAX_LIFETIME,
};

use self::auth::{
    check_db_name, clear_auth_failures, is_rate_limited, record_auth_failure, DEFAULT_DB_NAME,
};
use self::classify::error_response;
use self::query::{
    execute_wire_query, execute_wire_query_sql, execute_wire_query_with_params, log_received_query,
    log_received_query_with_params, settle_durability_ticket, DeferredQueryMetric, QueryContext,
    MAX_QUERY_LENGTH,
};
use self::sync::{
    dispatch_sync_ack_decision, dispatch_sync_pull_decision, dispatch_sync_status_decision,
    execute_gated_sync, SyncExecutionContext, SyncLogContext, SyncPreGate, SyncPullRequest,
};
use self::transaction::{
    reap_after_stalled_write, reap_expired_transaction, rollback_open_transaction,
    sync_tx_deadline, ReapContext, ReapNotice,
};
use self::wire::{
    flush_before_close, is_success_response, native_value_body_len, read_message_cancel_safe,
    write_msg, write_msg_within, ConnectionTermination, FrameStream, InFlightReadAhead,
    WireResultMode, MAX_WIRE_PAYLOAD_SIZE, WRITE_TIMEOUT,
};

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
                    ReapContext {
                        engine: &engine,
                        principal: &principal,
                        tx_permit: &mut tx_permit,
                        tx_deadline: &mut tx_deadline,
                        writer: &mut *writer,
                        peer: &peer,
                        metrics: &metrics,
                    },
                    max,
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
                match frame_payload_len(buf) {
                    // `Some` only when a whole 6-byte header is buffered.
                    Some(payload_len) => buf.len() - 6 >= payload_len as usize,
                    None => false,
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
                                QueryContext {
                                    engine: engine.clone(),
                                    tx_gate: tx_gate.clone(),
                                    tx_permit: &mut tx_permit,
                                    principal: principal.clone(),
                                    result_mode: WireResultMode::LegacyText,
                                    query_timeout,
                                    tx_wait_timeout,
                                    metrics: &metrics,
                                    stream: FrameStream {
                                        reader: &mut *reader,
                                        buffered: &mut wire_read_buffer,
                                        pending: &mut pending_messages,
                                    },
                                },
                                query,
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
                                QueryContext {
                                    engine: engine.clone(),
                                    tx_gate: tx_gate.clone(),
                                    tx_permit: &mut tx_permit,
                                    principal: principal.clone(),
                                    result_mode: WireResultMode::LegacyText,
                                    query_timeout,
                                    tx_wait_timeout,
                                    metrics: &metrics,
                                    stream: FrameStream {
                                        reader: &mut *reader,
                                        buffered: &mut wire_read_buffer,
                                        pending: &mut pending_messages,
                                    },
                                },
                                query,
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
                                QueryContext {
                                    engine: engine.clone(),
                                    tx_gate: tx_gate.clone(),
                                    tx_permit: &mut tx_permit,
                                    principal: principal.clone(),
                                    result_mode: WireResultMode::LegacyText,
                                    query_timeout,
                                    tx_wait_timeout,
                                    metrics: &metrics,
                                    stream: FrameStream {
                                        reader: &mut *reader,
                                        buffered: &mut wire_read_buffer,
                                        pending: &mut pending_messages,
                                    },
                                },
                                query,
                                params,
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
                                QueryContext {
                                    engine: engine.clone(),
                                    tx_gate: tx_gate.clone(),
                                    tx_permit: &mut tx_permit,
                                    principal: principal.clone(),
                                    result_mode: WireResultMode::Native,
                                    query_timeout,
                                    tx_wait_timeout,
                                    metrics: &metrics,
                                    stream: FrameStream {
                                        reader: &mut *reader,
                                        buffered: &mut wire_read_buffer,
                                        pending: &mut pending_messages,
                                    },
                                },
                                query,
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
                                QueryContext {
                                    engine: engine.clone(),
                                    tx_gate: tx_gate.clone(),
                                    tx_permit: &mut tx_permit,
                                    principal: principal.clone(),
                                    result_mode: WireResultMode::Native,
                                    query_timeout,
                                    tx_wait_timeout,
                                    metrics: &metrics,
                                    stream: FrameStream {
                                        reader: &mut *reader,
                                        buffered: &mut wire_read_buffer,
                                        pending: &mut pending_messages,
                                    },
                                },
                                query,
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
                                QueryContext {
                                    engine: engine.clone(),
                                    tx_gate: tx_gate.clone(),
                                    tx_permit: &mut tx_permit,
                                    principal: principal.clone(),
                                    result_mode: WireResultMode::Native,
                                    query_timeout,
                                    tx_wait_timeout,
                                    metrics: &metrics,
                                    stream: FrameStream {
                                        reader: &mut *reader,
                                        buffered: &mut wire_read_buffer,
                                        pending: &mut pending_messages,
                                    },
                                },
                                query,
                                params,
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
                        ReapContext {
                            engine: &engine,
                            principal: &principal,
                            tx_permit: &mut tx_permit,
                            tx_deadline: &mut tx_deadline,
                            writer: &mut *writer,
                            peer: &peer,
                            metrics: &metrics,
                        },
                        max_tx_lifetime,
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
                ReapContext {
                    engine: &engine,
                    principal: &principal,
                    tx_permit: &mut tx_permit,
                    tx_deadline: &mut tx_deadline,
                    writer: &mut *writer,
                    peer: &peer,
                    metrics: &metrics,
                },
                max_tx_lifetime,
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

#[cfg(test)]
mod tests;
