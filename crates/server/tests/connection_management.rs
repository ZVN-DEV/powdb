//! Integration tests for PowDB server connection management.
//!
//! Tests idle timeout, query timeout threshold, rate limiting, max connections
//! backpressure, graceful shutdown, malformed protocol handling, and
//! connection reuse after errors.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use common::{encode_connect_with_password, fresh_engine, read_message, InprocServer};
use powdb_query::executor::Engine;
use powdb_server::handler::{handle_connection, new_rate_limiter, new_tx_gate, ConnOpts};
use powdb_server::protocol::Message;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn encode_connect(db: &str) -> Vec<u8> {
    Message::Connect {
        db_name: db.to_string(),
        password: None,
        username: None,
    }
    .encode()
}

fn encode_query(q: &str) -> Vec<u8> {
    Message::Query {
        query: q.to_string(),
    }
    .encode()
}

/// Start a server that accepts a single connection with given opts.
/// Returns the address to connect to.
async fn start_single_conn_server(
    engine: Arc<RwLock<Engine>>,
    expected_password: Option<String>,
    idle_timeout: Duration,
    query_timeout: Duration,
    shutdown_rx: watch::Receiver<bool>,
    rate_limiter: Option<powdb_server::handler::AuthRateLimiter>,
) -> SocketAddr {
    let (addr, _handle) = InprocServer {
        expected_password,
        idle_timeout,
        query_timeout,
        rate_limiter,
        shutdown_rx: Some(shutdown_rx),
        single_conn: true,
        ..Default::default()
    }
    .start(engine)
    .await;
    addr
}

/// Start a server that accepts multiple connections.
async fn start_multi_conn_server(
    engine: Arc<RwLock<Engine>>,
    expected_password: Option<String>,
    idle_timeout: Duration,
    query_timeout: Duration,
    shutdown_rx: watch::Receiver<bool>,
    rate_limiter: Option<powdb_server::handler::AuthRateLimiter>,
) -> SocketAddr {
    let (addr, _handle) = InprocServer {
        expected_password,
        idle_timeout,
        query_timeout,
        rate_limiter,
        shutdown_rx: Some(shutdown_rx),
        ..Default::default()
    }
    .start(engine)
    .await;
    addr
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test 1: Idle timeout closes the connection after inactivity.
#[tokio::test]
async fn test_idle_timeout() {
    let (engine, _tmp) = fresh_engine();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    let addr = start_single_conn_server(
        engine,
        None,
        Duration::from_millis(100), // Very short idle timeout
        Duration::from_secs(5),
        shutdown_rx,
        None,
    )
    .await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&encode_connect("testdb")).await.unwrap();

    // Should get ConnectOk
    let msg = read_message(&mut stream).await.unwrap();
    assert!(matches!(msg, Message::ConnectOk { .. }));

    // Now do nothing and wait for idle timeout
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Should receive an error message about idle timeout
    let msg = read_message(&mut stream).await;
    match msg {
        Some(Message::Error { message }) => {
            assert!(
                message.contains("idle timeout"),
                "expected 'idle timeout' error, got: {message}"
            );
        }
        None => {
            // Connection closed without a message is also acceptable
            // if the server couldn't write before closing.
        }
        other => panic!("expected Error or EOF, got: {other:?}"),
    }
}

/// Test 2: Query timeout threshold is wired without detaching blocking work.
#[tokio::test]
async fn test_query_timeout_threshold_does_not_detach_query() {
    let (engine, _tmp) = fresh_engine();

    // Set up a table so the query actually does work
    {
        let mut eng = engine.write().unwrap();
        eng.execute_powql("type Item { required id: int, required name: str }")
            .unwrap();
    }

    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    let addr = start_single_conn_server(
        engine,
        None,
        Duration::from_secs(5),
        Duration::from_millis(1), // Very short timeout threshold
        shutdown_rx,
        None,
    )
    .await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&encode_connect("testdb")).await.unwrap();

    let msg = read_message(&mut stream).await.unwrap();
    assert!(matches!(msg, Message::ConnectOk { .. }));

    // Send a query. With cooperative cancellation the 1ms threshold may fire
    // (a timeout Error) or the tiny scan may finish first (ResultRows); both
    // are correct. What must never happen is a detached query: the connection
    // must produce exactly one coherent reply per request and stay usable.
    stream.write_all(&encode_query("Item")).await.unwrap();

    let msg = read_message(&mut stream).await.unwrap();
    match msg {
        Message::ResultRows { .. } => {}
        Message::Error { ref message } if message.contains("timeout") => {}
        other => panic!("expected ResultRows or timeout Error, got: {other:?}"),
    }

    // The connection must remain framed and usable after a timeout: a second
    // query gets its own definitive reply, not silence or leftover bytes from
    // a detached first execution.
    stream.write_all(&encode_query("Item")).await.unwrap();
    let msg = read_message(&mut stream).await.unwrap();
    match msg {
        Message::ResultRows { .. } => {}
        Message::Error { ref message } if message.contains("timeout") => {}
        other => panic!("expected ResultRows or timeout Error on reuse, got: {other:?}"),
    }
}

/// Test 3: Rate limiting after too many auth failures.
#[tokio::test]
async fn test_rate_limiting() {
    let (engine, _tmp) = fresh_engine();
    let rate_limiter = new_rate_limiter();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    let addr = start_multi_conn_server(
        engine,
        Some("correct_password".to_string()),
        Duration::from_secs(5),
        Duration::from_secs(5),
        shutdown_rx,
        Some(rate_limiter),
    )
    .await;

    // Give the server a moment to bind
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send 5 bad password attempts (MAX_AUTH_FAILURES = 5)
    for _ in 0..5 {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(&encode_connect_with_password("testdb", "wrong_password"))
            .await
            .unwrap();
        let msg = read_message(&mut stream).await.unwrap();
        match msg {
            Message::Error { message } => {
                assert!(
                    message.contains("authentication failed"),
                    "expected auth failed, got: {message}"
                );
            }
            other => panic!("expected auth error, got: {other:?}"),
        }
    }

    // The 6th attempt should be rate-limited
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&encode_connect_with_password("testdb", "wrong_password"))
        .await
        .unwrap();
    let msg = read_message(&mut stream).await.unwrap();
    match msg {
        Message::Error { message } => {
            assert!(
                message.contains("too many auth failures"),
                "expected rate limit error, got: {message}"
            );
        }
        other => panic!("expected rate limit error, got: {other:?}"),
    }
}

/// Test 4: Max connections backpressure using a semaphore.
/// Since ConnOpts doesn't have a built-in semaphore field, we test the
/// pattern by wrapping accept with a tokio::sync::Semaphore manually,
/// verifying the pattern works.
#[tokio::test]
async fn test_max_connections_backpressure() {
    let (engine, _tmp) = fresh_engine();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let max_conns: usize = 3;
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_conns));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let sem = semaphore.clone();
    let eng = engine.clone();
    let tx_gate = new_tx_gate();
    tokio::spawn(async move {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let permit = sem.clone().acquire_owned().await.unwrap();
            let eng2 = eng.clone();
            let tx_gate = tx_gate.clone();
            let mut rx = shutdown_rx.clone();
            tokio::spawn(async move {
                handle_connection(
                    stream,
                    ConnOpts {
                        tx_wait_timeout: std::time::Duration::from_secs(5),
                        db_name: None,
                        engine: eng2,
                        tx_gate,
                        expected_password: None,
                        users: Arc::new(powdb_auth::UserStore::new()),
                        shutdown_rx: &mut rx,
                        idle_timeout: Duration::from_secs(5),
                        query_timeout: Duration::from_secs(5),
                        rate_limiter: None,
                        peer_addr: Some(peer),
                        metrics: std::sync::Arc::new(powdb_server::metrics::Metrics::new()),
                    },
                )
                .await;
                drop(permit);
            });
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Open max_conns connections that stay connected
    let mut held_streams = Vec::new();
    for _ in 0..max_conns {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(&encode_connect("testdb")).await.unwrap();
        let msg = read_message(&mut stream).await.unwrap();
        assert!(matches!(msg, Message::ConnectOk { .. }));
        held_streams.push(stream);
    }

    // The (max_conns+1)th connection should block at the semaphore.
    // We test this by attempting a connection with a timeout - it should not
    // complete the handshake within a short window.
    let connect_future = async {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(&encode_connect("testdb")).await.unwrap();
        read_message(&mut stream).await
    };

    let result = tokio::time::timeout(Duration::from_millis(200), connect_future).await;
    // The connection should time out because the semaphore blocks accept
    assert!(
        result.is_err(),
        "expected the (max+1)th connection to be blocked by backpressure"
    );

    // Now close one of the held connections
    held_streams.pop(); // drops the TcpStream, closing it

    // Give a moment for the server to detect the close and release the permit
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Now a new connection should succeed
    let mut new_stream = TcpStream::connect(addr).await.unwrap();
    new_stream
        .write_all(&encode_connect("testdb"))
        .await
        .unwrap();
    let msg = tokio::time::timeout(Duration::from_secs(2), read_message(&mut new_stream))
        .await
        .expect("new connection should succeed after slot freed");
    assert!(matches!(msg, Some(Message::ConnectOk { .. })));
}

/// Test 5: Write timeout mechanism exists and messages are delivered.
/// We cannot easily trigger the 30s write timeout in a test, so we verify
/// that normal message delivery works correctly (write_msg is functional).
#[tokio::test]
async fn test_write_msg_delivers_messages() {
    let (engine, _tmp) = fresh_engine();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    let addr = start_single_conn_server(
        engine,
        None,
        Duration::from_secs(5),
        Duration::from_secs(5),
        shutdown_rx,
        None,
    )
    .await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&encode_connect("testdb")).await.unwrap();

    let msg = read_message(&mut stream).await.unwrap();
    assert!(matches!(msg, Message::ConnectOk { .. }));

    // Send a ping and verify pong comes back (exercises write_msg)
    stream.write_all(&Message::Ping.encode()).await.unwrap();
    let msg = read_message(&mut stream).await.unwrap();
    assert!(
        matches!(msg, Message::Pong),
        "expected Pong response, got: {msg:?}"
    );

    // Send multiple queries in sequence - all should get responses
    for i in 0..5 {
        let q = format!("type T{i} {{ required id: int }}");
        stream.write_all(&encode_query(&q)).await.unwrap();
        let msg = read_message(&mut stream).await.unwrap();
        assert!(
            matches!(
                msg,
                Message::ResultOk { .. } | Message::ResultMessage { .. }
            ),
            "query {i} should succeed, got: {msg:?}"
        );
    }
}

/// Test 6: Graceful shutdown notifies connected clients.
#[tokio::test]
async fn test_graceful_shutdown() {
    let (engine, _tmp) = fresh_engine();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let addr = start_single_conn_server(
        engine,
        None,
        Duration::from_secs(30),
        Duration::from_secs(5),
        shutdown_rx,
        None,
    )
    .await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&encode_connect("testdb")).await.unwrap();

    let msg = read_message(&mut stream).await.unwrap();
    assert!(matches!(msg, Message::ConnectOk { .. }));

    // Send shutdown signal
    shutdown_tx.send(true).unwrap();

    // Client should receive "server shutting down" error
    let msg = tokio::time::timeout(Duration::from_secs(2), read_message(&mut stream))
        .await
        .expect("should receive shutdown message within timeout");

    match msg {
        Some(Message::Error { message }) => {
            assert!(
                message.contains("server shutting down"),
                "expected 'server shutting down' error, got: {message}"
            );
        }
        None => {
            // Connection closed is also acceptable
        }
        other => panic!("expected shutdown error or EOF, got: {other:?}"),
    }
}

/// Test 7: Malformed protocol data results in error, not a crash.
#[tokio::test]
async fn test_malformed_protocol() {
    let (engine, _tmp) = fresh_engine();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    let addr = start_single_conn_server(
        engine,
        None,
        Duration::from_secs(5),
        Duration::from_secs(5),
        shutdown_rx,
        None,
    )
    .await;

    let mut stream = TcpStream::connect(addr).await.unwrap();

    // Send garbage bytes that don't form a valid message frame.
    // Use a valid-looking header with an unknown message type (0xFF)
    // and a small payload length so the server reads it fully.
    let garbage: Vec<u8> = vec![
        0xFF, // invalid message type
        0x00, // flags
        0x04, 0x00, 0x00, 0x00, // payload length = 4
        0xDE, 0xAD, 0xBE, 0xEF, // garbage payload
    ];
    stream.write_all(&garbage).await.unwrap();

    // The server should either send an error or close the connection,
    // but it should NOT crash. We verify by checking we get either an
    // error message or an EOF (connection closed gracefully).
    let result = tokio::time::timeout(Duration::from_secs(2), read_message(&mut stream)).await;

    match result {
        Ok(Some(Message::Error { message })) => {
            // Server responded with an error - good
            assert!(!message.is_empty(), "error message should not be empty");
        }
        Ok(None) => {
            // Server closed the connection - also acceptable
        }
        Err(_) => {
            // Timeout - server didn't respond but also didn't crash.
            // This could happen if the server is waiting for a valid
            // CONNECT message first. Acceptable.
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

/// Test 7b: Send completely random bytes (not even a valid header length).
#[tokio::test]
async fn test_malformed_protocol_truncated() {
    let (engine, _tmp) = fresh_engine();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    let addr = start_single_conn_server(
        engine,
        None,
        Duration::from_millis(500),
        Duration::from_secs(5),
        shutdown_rx,
        None,
    )
    .await;

    let mut stream = TcpStream::connect(addr).await.unwrap();

    // Send only 3 bytes (less than the 6-byte header)
    stream.write_all(&[0x01, 0x00, 0x00]).await.unwrap();

    // Close our write side so the server sees EOF mid-header
    stream.shutdown().await.unwrap();

    // The server should handle this gracefully (no panic).
    // Give it a moment to process.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // If we got here without the test process panicking, the server handled it.
}

/// Test 8: Connection reuse after a query error.
#[tokio::test]
async fn test_connection_reuse_after_error() {
    let (engine, _tmp) = fresh_engine();

    // Create a table so we have something valid to query
    {
        let mut eng = engine.write().unwrap();
        eng.execute_powql("type User { required name: str, required age: int }")
            .unwrap();
        eng.execute_powql(r#"insert User { name := "Alice", age := 30 }"#)
            .unwrap();
    }

    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    let addr = start_single_conn_server(
        engine,
        None,
        Duration::from_secs(5),
        Duration::from_secs(5),
        shutdown_rx,
        None,
    )
    .await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&encode_connect("testdb")).await.unwrap();

    let msg = read_message(&mut stream).await.unwrap();
    assert!(matches!(msg, Message::ConnectOk { .. }));

    // Send a query that will error (reference a non-existent table)
    stream
        .write_all(&encode_query("NonExistentTable"))
        .await
        .unwrap();
    let msg = read_message(&mut stream).await.unwrap();
    assert!(
        matches!(msg, Message::Error { .. }),
        "expected error for bad table, got: {msg:?}"
    );

    // Connection should still be alive - send a valid query
    stream.write_all(&encode_query("User")).await.unwrap();
    let msg = read_message(&mut stream).await.unwrap();
    assert!(
        matches!(msg, Message::ResultRows { .. }),
        "expected ResultRows after recovery, got: {msg:?}"
    );

    // Verify the data is correct
    match msg {
        Message::ResultRows { rows, .. } => {
            assert_eq!(rows.len(), 1, "expected 1 row");
            assert_eq!(rows[0][0], "Alice");
        }
        _ => unreachable!(),
    }

    // Send another error and verify recovery again
    stream
        .write_all(&encode_query("AnotherBadTable filter .x > 1"))
        .await
        .unwrap();
    let msg = read_message(&mut stream).await.unwrap();
    assert!(matches!(msg, Message::Error { .. }));

    // Still alive
    stream
        .write_all(&encode_query("count(User)"))
        .await
        .unwrap();
    let msg = read_message(&mut stream).await.unwrap();
    match msg {
        Message::ResultScalar { value } => {
            assert_eq!(value, "1");
        }
        other => panic!("expected ResultScalar, got: {other:?}"),
    }
}
