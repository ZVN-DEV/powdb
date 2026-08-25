//! Wire-protocol coverage for nested projection results: the assembled PJ1
//! JSON array column must round-trip intact on the native route, including
//! the empty array for a childless parent.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use powdb_query::executor::Engine;
use powdb_server::handler::{handle_connection, new_tx_gate, ConnOpts};
use powdb_server::metrics::Metrics;
use powdb_server::protocol::Message;
use powdb_storage::pj1::parse_json_text;
use powdb_storage::types::Value;
use tokio::net::{TcpListener, TcpStream};

static SERVER_ID: AtomicU64 = AtomicU64::new(0);

async fn start_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let unique = SERVER_ID.fetch_add(1, Ordering::Relaxed);
    let data_dir = std::env::temp_dir().join(format!(
        "powdb_nested_wire_{}_{}",
        std::process::id(),
        unique
    ));
    let _ = std::fs::remove_dir_all(&data_dir);
    std::fs::create_dir_all(&data_dir).expect("create data directory");

    let mut engine = Engine::new(&data_dir).expect("create engine");
    engine.set_wal_sync_mode(powdb_query::executor::WalSyncMode::Off);
    for statement in [
        "type User { required id: int, required name: str }",
        "type Order { required id: int, required user_id: int, required total: float }",
        r#"insert User { id := 1, name := "alice" }"#,
        r#"insert User { id := 2, name := "cara" }"#,
        "insert Order { id := 1, user_id := 1, total := 9.5 }",
        "insert Order { id := 2, user_id := 1, total := 20.25 }",
    ] {
        engine.execute_powql(statement).expect("seed fixture");
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind nested wire test server");
    let addr = listener.local_addr().expect("server address");
    let engine = Arc::new(RwLock::new(engine));
    let tx_gate = new_tx_gate();
    let handle = tokio::spawn(async move {
        loop {
            let (stream, peer) = listener.accept().await.expect("accept connection");
            let engine = engine.clone();
            let tx_gate = tx_gate.clone();
            tokio::spawn(async move {
                let (_shutdown, mut shutdown_rx) = tokio::sync::watch::channel(false);
                handle_connection(
                    stream,
                    ConnOpts {
                        engine,
                        tx_gate,
                        expected_password: None,
                        users: Arc::new(powdb_auth::UserStore::new()),
                        shutdown_rx: &mut shutdown_rx,
                        idle_timeout: Duration::from_secs(30),
                        preauth_deadline: powdb_server::handler::DEFAULT_PREAUTH_DEADLINE,
                        query_timeout: Duration::from_secs(5),
                        rate_limiter: None,
                        peer_addr: Some(peer),
                        metrics: Arc::new(Metrics::new()),
                        tx_wait_timeout: Duration::from_secs(5),
                        db_name: None,
                    },
                )
                .await;
            });
        }
    });
    (addr, handle)
}

async fn connect(addr: std::net::SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.expect("connect client");
    Message::Connect {
        db_name: "default".into(),
        password: None,
        username: None,
    }
    .write_to(&mut stream)
    .await
    .expect("write CONNECT");
    assert!(matches!(
        Message::read_from(&mut stream).await.expect("read CONNECT"),
        Some(Message::ConnectOk { .. })
    ));
    stream
}

async fn query(stream: &mut TcpStream, message: Message) -> Message {
    message.write_to(stream).await.expect("write query");
    Message::read_from(stream)
        .await
        .expect("read query response")
        .expect("server kept connection open")
}

#[tokio::test]
async fn nested_projection_json_column_round_trips_natively() {
    let (addr, server) = start_server().await;
    let mut stream = connect(addr).await;

    let response = query(
        &mut stream,
        Message::QueryNative {
            query: "User as u { u.name, orders: Order as o filter o.user_id = u.id \
                    order o.total desc { o.total } }"
                .into(),
        },
    )
    .await;
    let Message::ResultRowsNative { columns, rows } = response else {
        panic!("expected native rows, got {response:?}");
    };
    assert_eq!(columns, ["u.name", "orders"]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Str("alice".into()));
    let expected: Value = Value::Json(
        parse_json_text(r#"[{"total":20.25},{"total":9.5}]"#)
            .expect("parse expected JSON")
            .into(),
    );
    assert_eq!(rows[0][1], expected, "PJ1 array must arrive intact");
    // Childless parent: the JSON empty array, not NULL and not a dropped row.
    assert_eq!(rows[1][0], Value::Str("cara".into()));
    assert_eq!(
        rows[1][1],
        Value::Json(parse_json_text("[]").expect("parse empty array").into())
    );
    server.abort();
}

#[tokio::test]
async fn nested_projection_survives_text_route() {
    let (addr, server) = start_server().await;
    let mut stream = connect(addr).await;

    // The legacy text route stringifies values; the JSON column must still
    // carry the full array text.
    let response = query(
        &mut stream,
        Message::Query {
            query: "User as u filter u.id = 1 { u.name, orders: Order as o \
                    filter o.user_id = u.id order o.total asc { o.total } }"
                .into(),
        },
    )
    .await;
    let Message::ResultRows { columns, rows } = response else {
        panic!("expected text rows, got {response:?}");
    };
    assert_eq!(columns, ["u.name", "orders"]);
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0][1].contains("9.5") && rows[0][1].contains("20.25"),
        "text route must carry the array content, got: {}",
        rows[0][1]
    );
    server.abort();
}
