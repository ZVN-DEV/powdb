//! End-to-end tests for two internal dogfood fixes:
//!
//!   P-4  — overlapping explicit transactions QUEUE behind the transaction gate
//!          (they no longer fail fast), bounded by a configurable wait timeout
//!          that surfaces a clear, typed error instead of hanging forever.
//!   P-10 — the opt-in `--db-name` gate rejects a CONNECT that explicitly names
//!          a different database, while staying backward-compatible when unset.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use powdb_query::executor::Engine;
use powdb_server::handler::{self, ConnOpts};
use powdb_server::metrics::Metrics;
use powdb_server::protocol::Message;

static UNIQUE: AtomicU64 = AtomicU64::new(0);

struct ServerConfig {
    tx_wait_timeout: Duration,
    db_name: Option<String>,
    /// Shared metrics handle so a test can inspect counters the server bumps.
    /// Defaults to a fresh instance when a test does not care.
    metrics: Arc<Metrics>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            tx_wait_timeout: Duration::from_secs(5),
            db_name: None,
            metrics: Arc::new(Metrics::new()),
        }
    }
}

/// Spawn a real multi-connection server on an ephemeral port and return its
/// address. A single long-lived shutdown sender is kept alive for the server's
/// lifetime so idle connections park on the socket instead of spinning.
async fn start_server(cfg: ServerConfig) -> std::net::SocketAddr {
    let id = UNIQUE.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("powdb_txgate_{}_{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let engine = Arc::new(RwLock::new(Engine::new(&dir).unwrap()));
    let tx_gate = handler::new_tx_gate();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = Arc::new(cfg);

    tokio::spawn(async move {
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let engine = engine.clone();
            let tx_gate = tx_gate.clone();
            let cfg = cfg.clone();
            let mut rx = shutdown_rx.clone();
            tokio::spawn(async move {
                handler::handle_connection(
                    stream,
                    ConnOpts {
                        engine,
                        tx_gate,
                        expected_password: None,
                        users: Arc::new(powdb_auth::UserStore::new()),
                        shutdown_rx: &mut rx,
                        idle_timeout: Duration::from_secs(30),
                        query_timeout: Duration::from_secs(30),
                        rate_limiter: None,
                        peer_addr: Some(peer),
                        metrics: cfg.metrics.clone(),
                        tx_wait_timeout: cfg.tx_wait_timeout,
                        db_name: cfg.db_name.clone(),
                    },
                )
                .await;
            });
        }
    });

    // Let the listener spawn schedule before clients connect.
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

async fn send(stream: &mut TcpStream, msg: Message) {
    stream.write_all(&msg.encode()).await.unwrap();
}

async fn recv(stream: &mut TcpStream) -> Message {
    let mut header = [0u8; 6];
    stream.read_exact(&mut header).await.unwrap();
    let len = u32::from_le_bytes(header[2..6].try_into().unwrap()) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut payload).await.unwrap();
    }
    let mut full = header.to_vec();
    full.extend_from_slice(&payload);
    Message::decode(&full).unwrap()
}

/// CONNECT with the given db name, returning the raw server reply (so callers
/// can assert either ConnectOk or a rejection Error).
async fn connect_raw(addr: std::net::SocketAddr, db: &str) -> (TcpStream, Message) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    send(
        &mut stream,
        Message::Connect {
            db_name: db.to_string(),
            password: None,
            username: None,
        },
    )
    .await;
    let reply = recv(&mut stream).await;
    (stream, reply)
}

/// CONNECT and assert the handshake succeeded.
async fn connect(addr: std::net::SocketAddr, db: &str) -> TcpStream {
    let (stream, reply) = connect_raw(addr, db).await;
    assert!(
        matches!(reply, Message::ConnectOk { .. }),
        "expected ConnectOk, got {reply:?}"
    );
    stream
}

async fn query(stream: &mut TcpStream, q: &str) -> Message {
    send(
        stream,
        Message::Query {
            query: q.to_string(),
        },
    )
    .await;
    recv(stream).await
}

fn is_error(msg: &Message) -> bool {
    matches!(msg, Message::Error { .. })
}

// ---- P-4: transaction gate queuing + timeout ----------------------------

/// The dogfood repro: 10 concurrent connections each running begin/insert/commit
/// must ALL succeed. Before queuing, the second overlapping `begin` failed fast
/// (~54% failure under 10-way concurrency); now they serialize through the gate.
#[tokio::test]
async fn concurrent_explicit_transactions_all_succeed() {
    let addr = start_server(ServerConfig::default()).await;

    // Create the type once up front (bare DDL, not inside a transaction).
    let mut setup = connect(addr, "default").await;
    let created = query(&mut setup, "type Item { required n: int }").await;
    assert!(!is_error(&created), "type creation failed: {created:?}");

    let mut handles = Vec::new();
    for i in 0..10 {
        handles.push(tokio::spawn(async move {
            let mut s = connect(addr, "default").await;
            let begin = query(&mut s, "begin").await;
            assert!(!is_error(&begin), "conn {i} begin failed: {begin:?}");
            let insert = query(&mut s, &format!("insert Item {{ n := {i} }}")).await;
            assert!(!is_error(&insert), "conn {i} insert failed: {insert:?}");
            let commit = query(&mut s, "commit").await;
            assert!(!is_error(&commit), "conn {i} commit failed: {commit:?}");
        }));
    }
    for h in handles {
        h.await.expect("connection task panicked");
    }

    // All ten rows are durably visible.
    let count = query(&mut setup, "count(Item)").await;
    match count {
        Message::ResultScalar { value } => assert_eq!(value, "10"),
        other => panic!("expected scalar count, got {other:?}"),
    }
}

/// While one connection holds an explicit transaction open, another's `begin`
/// waits then fails with the clear, typed timeout error — not a generic
/// execution error and not an indefinite hang.
#[tokio::test]
async fn overlapping_begin_times_out_with_clear_error() {
    let addr = start_server(ServerConfig {
        tx_wait_timeout: Duration::from_millis(200),
        ..ServerConfig::default()
    })
    .await;

    // Connection A opens a transaction and holds it (never commits).
    let mut a = connect(addr, "default").await;
    let begin_a = query(&mut a, "begin").await;
    assert!(!is_error(&begin_a), "A begin failed: {begin_a:?}");

    // Connection B's begin must resolve (not hang) with the timeout error.
    let mut b = connect(addr, "default").await;
    let begin_b = tokio::time::timeout(Duration::from_secs(5), query(&mut b, "begin"))
        .await
        .expect("B begin must not hang past the wait timeout");
    match begin_b {
        Message::Error { message } => {
            assert!(
                message.contains("transaction gate timeout after 200ms"),
                "unexpected error: {message}"
            );
        }
        other => panic!("expected timeout Error, got {other:?}"),
    }

    // A is unaffected and can still commit.
    let commit_a = query(&mut a, "commit").await;
    assert!(!is_error(&commit_a), "A commit failed: {commit_a:?}");
}

/// A connection that dies mid-transaction must release the gate (via the
/// rollback-on-disconnect path) so the next connection's transaction proceeds.
#[tokio::test]
async fn disconnect_mid_transaction_releases_the_gate() {
    let addr = start_server(ServerConfig::default()).await;

    // A opens a transaction, then drops its socket without committing.
    let mut a = connect(addr, "default").await;
    let begin_a = query(&mut a, "begin").await;
    assert!(!is_error(&begin_a), "A begin failed: {begin_a:?}");
    drop(a);

    // Give the server time to observe EOF and run rollback-on-disconnect.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // B's begin now succeeds because the permit was released.
    let mut b = connect(addr, "default").await;
    let begin_b = tokio::time::timeout(Duration::from_secs(5), query(&mut b, "begin"))
        .await
        .expect("B begin must resolve");
    assert!(
        !is_error(&begin_b),
        "B begin should succeed after A disconnected: {begin_b:?}"
    );
    let commit_b = query(&mut b, "commit").await;
    assert!(!is_error(&commit_b), "B commit failed: {commit_b:?}");
}

/// A second `begin` on the SAME connection is rejected immediately — it must
/// never deadlock waiting on the permit the connection already holds.
#[tokio::test]
async fn same_connection_double_begin_errors_without_hanging() {
    let addr = start_server(ServerConfig::default()).await;
    let mut a = connect(addr, "default").await;

    let first = query(&mut a, "begin").await;
    assert!(!is_error(&first), "first begin failed: {first:?}");

    let second = tokio::time::timeout(Duration::from_secs(3), query(&mut a, "begin"))
        .await
        .expect("second begin must not hang on the already-held permit");
    assert!(
        is_error(&second),
        "second begin on the same connection must error, got {second:?}"
    );

    // The connection is still usable: commit closes the (single) transaction.
    let commit = query(&mut a, "commit").await;
    assert!(
        !is_error(&commit),
        "commit after double-begin failed: {commit:?}"
    );
}

// ---- Dogfood #4: bare autocommit writes are bounded by the same gate --------

/// The dogfood repro: connection A holds an explicit transaction open and
/// never commits; connection B issues a BARE autocommit insert (no `begin`).
/// Before this fix B waited on the gate UNBOUNDED (the dogfood run measured ~7.7s behind
/// an 8s holder, and a wedged holder wedged writers forever). Now B fails fast
/// with the clear, typed timeout error. After A commits, a retry from B works.
#[tokio::test]
async fn bare_autocommit_write_behind_held_txn_times_out_then_recovers() {
    let addr = start_server(ServerConfig {
        tx_wait_timeout: Duration::from_millis(200),
        ..ServerConfig::default()
    })
    .await;

    let mut setup = connect(addr, "default").await;
    let created = query(&mut setup, "type Item { required n: int }").await;
    assert!(!is_error(&created), "type creation failed: {created:?}");

    // A opens an explicit transaction and holds it (never commits).
    let mut a = connect(addr, "default").await;
    let begin_a = query(&mut a, "begin").await;
    assert!(!is_error(&begin_a), "A begin failed: {begin_a:?}");

    // B's BARE autocommit insert must resolve (not hang) with the timeout error.
    let mut b = connect(addr, "default").await;
    let insert_b = tokio::time::timeout(
        Duration::from_secs(5),
        query(&mut b, "insert Item { n := 1 }"),
    )
    .await
    .expect("B autocommit write must not hang past the wait timeout");
    match insert_b {
        Message::Error { message } => {
            assert!(
                message.contains("transaction gate timeout after 200ms"),
                "unexpected error: {message}"
            );
        }
        other => panic!("expected timeout Error, got {other:?}"),
    }

    // Once A commits and releases the gate, B's retry succeeds.
    let commit_a = query(&mut a, "commit").await;
    assert!(!is_error(&commit_a), "A commit failed: {commit_a:?}");

    let retry_b = tokio::time::timeout(
        Duration::from_secs(5),
        query(&mut b, "insert Item { n := 2 }"),
    )
    .await
    .expect("B retry must resolve now that the gate is free");
    assert!(!is_error(&retry_b), "B retry insert failed: {retry_b:?}");
}

/// A bare autocommit write that times out behind a held transaction bumps the
/// SAME `powdb_tx_gate_timeouts_total` counter as an explicit `begin` timeout,
/// so `powdb_queries_total{result="error"}` stays truthful for both paths.
#[tokio::test]
async fn bare_autocommit_timeout_increments_the_gate_metric() {
    let metrics = Arc::new(Metrics::new());
    let addr = start_server(ServerConfig {
        tx_wait_timeout: Duration::from_millis(150),
        metrics: metrics.clone(),
        ..ServerConfig::default()
    })
    .await;

    let mut setup = connect(addr, "default").await;
    let created = query(&mut setup, "type Item { required n: int }").await;
    assert!(!is_error(&created), "type creation failed: {created:?}");

    let mut a = connect(addr, "default").await;
    assert!(!is_error(&query(&mut a, "begin").await), "A begin failed");

    let mut b = connect(addr, "default").await;
    let insert_b = tokio::time::timeout(
        Duration::from_secs(5),
        query(&mut b, "insert Item { n := 9 }"),
    )
    .await
    .expect("B write must not hang");
    assert!(
        is_error(&insert_b),
        "expected a timeout error, got {insert_b:?}"
    );

    let rendered = metrics.render();
    assert!(
        rendered.contains("powdb_tx_gate_timeouts_total 1"),
        "autocommit timeout must bump the gate counter:\n{rendered}"
    );
    // The failed write must also register as an errored query.
    assert!(
        rendered.contains("powdb_queries_total{result=\"error\"} 1"),
        "autocommit timeout must count as an errored query:\n{rendered}"
    );
}

/// Regression / common path: a bare autocommit write with NO concurrent holder
/// must succeed immediately. The bounded acquire must add no latency and never
/// spuriously time out when the gate is free.
#[tokio::test]
async fn bare_autocommit_write_with_no_holder_succeeds_immediately() {
    // A deliberately tiny wait timeout: if the uncontended acquire were to wait
    // on it at all, this would flake. It must not.
    let addr = start_server(ServerConfig {
        tx_wait_timeout: Duration::from_millis(50),
        ..ServerConfig::default()
    })
    .await;

    let mut s = connect(addr, "default").await;
    let created = query(&mut s, "type Item { required n: int }").await;
    assert!(!is_error(&created), "type creation failed: {created:?}");

    for i in 0..5 {
        let insert = query(&mut s, &format!("insert Item {{ n := {i} }}")).await;
        assert!(!is_error(&insert), "bare insert {i} failed: {insert:?}");
    }

    let count = query(&mut s, "count(Item)").await;
    match count {
        Message::ResultScalar { value } => assert_eq!(value, "5"),
        other => panic!("expected scalar count, got {other:?}"),
    }
}

// ---- P-10: named-database gate ------------------------------------------

#[tokio::test]
async fn pinned_server_rejects_foreign_db_name() {
    let addr = start_server(ServerConfig {
        db_name: Some("prod".to_string()),
        ..ServerConfig::default()
    })
    .await;

    // A foreign explicit name is rejected with the actionable message.
    let (_s, reply) = connect_raw(addr, "staging").await;
    match reply {
        Message::Error { message } => {
            assert_eq!(
                message,
                "unknown database 'staging'; this server serves 'prod'"
            );
        }
        other => panic!("expected rejection Error, got {other:?}"),
    }

    // The configured name, the empty name, and the client default all connect.
    for accepted in ["prod", "", "default"] {
        let (_s, reply) = connect_raw(addr, accepted).await;
        assert!(
            matches!(reply, Message::ConnectOk { .. }),
            "db name {accepted:?} should connect, got {reply:?}"
        );
    }
}

#[tokio::test]
async fn unpinned_server_accepts_any_db_name() {
    let addr = start_server(ServerConfig::default()).await;
    for name in ["default", "", "whatever", "prod"] {
        let (_s, reply) = connect_raw(addr, name).await;
        assert!(
            matches!(reply, Message::ConnectOk { .. }),
            "db name {name:?} should connect on an unpinned server, got {reply:?}"
        );
    }
}

// ---- P-6/P-7: DX error messages must survive wire sanitization -----------

/// The reserved-word and duplicate-type guidance is only useful if it reaches
/// wire clients. These messages must stay on the server's safe-to-forward
/// allowlist (SAFE_ERROR_PREFIXES) — a prefix drift here silently masks them
/// back to "query execution error", which is exactly the dogfood complaint.
#[tokio::test]
async fn dx_error_messages_survive_wire_sanitization() {
    let addr = start_server(ServerConfig::default()).await;
    let mut s = connect(addr, "default").await;

    let reserved = query(&mut s, "type Post { type: str }").await;
    match reserved {
        Message::Error { message } => assert!(
            message.contains("reserved word") && message.contains("`type`"),
            "reserved-word guidance was sanitized away: {message:?}"
        ),
        other => panic!("expected Error, got {other:?}"),
    }

    let created = query(&mut s, "type Post { title: str }").await;
    assert!(!is_error(&created), "setup create failed: {created:?}");
    let duplicate = query(&mut s, "type Post { title: str }").await;
    match duplicate {
        Message::Error { message } => assert!(
            message.contains("cannot create type 'Post'"),
            "duplicate-type message was sanitized away: {message:?}"
        ),
        other => panic!("expected Error, got {other:?}"),
    }

    let begin = query(&mut s, "begin").await;
    assert!(!is_error(&begin), "begin failed: {begin:?}");
    let double_begin = query(&mut s, "begin").await;
    match double_begin {
        Message::Error { message } => assert!(
            message.contains("transaction is already active"),
            "double-begin message was sanitized away: {message:?}"
        ),
        other => panic!("expected Error, got {other:?}"),
    }
    let rollback = query(&mut s, "rollback").await;
    assert!(!is_error(&rollback), "rollback failed: {rollback:?}");
}

/// Lexer diagnostics and not-found errors must also reach remote clients
/// verbatim — the bug-hunt found `table 'X' not found` and backtick lexer
/// errors were masked because the allowlist prefixes didn't match the real
/// message shapes.
#[tokio::test]
async fn lookup_and_lexer_errors_survive_wire_sanitization() {
    let addr = start_server(ServerConfig::default()).await;
    let mut s = connect(addr, "default").await;

    let missing = query(&mut s, "count(Nope)").await;
    match missing {
        Message::Error { message } => assert!(
            message.contains("Nope"),
            "table-not-found message was sanitized away: {message:?}"
        ),
        other => panic!("expected Error, got {other:?}"),
    }

    let unterminated = query(&mut s, "type Post { `oops: str }").await;
    match unterminated {
        Message::Error { message } => assert!(
            message.contains("unterminated quoted identifier"),
            "lexer diagnostic was sanitized away: {message:?}"
        ),
        other => panic!("expected Error, got {other:?}"),
    }
}
