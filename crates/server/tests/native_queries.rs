//! End-to-end coverage for typed query request routes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use powdb_query::executor::Engine;
use powdb_server::handler::{handle_connection, new_tx_gate, ConnOpts};
use powdb_server::metrics::Metrics;
use powdb_server::protocol::{Message, WireParam};
use powdb_storage::pj1::parse_json_text;
use powdb_storage::types::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

static SERVER_ID: AtomicU64 = AtomicU64::new(0);

async fn start_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let unique = SERVER_ID.fetch_add(1, Ordering::Relaxed);
    let data_dir = std::env::temp_dir().join(format!(
        "powdb_native_query_{}_{}",
        std::process::id(),
        unique
    ));
    let _ = std::fs::remove_dir_all(&data_dir);
    std::fs::create_dir_all(&data_dir).expect("create data directory");

    let mut engine = Engine::new(&data_dir).expect("create engine");
    engine.set_wal_sync_mode(powdb_query::executor::WalSyncMode::Off);
    engine
        .execute_powql(
            "type NativeRow { required unique id: int, data: json, blob: bytes, note: str }",
        )
        .expect("create native fixture table");
    let document = parse_json_text(r#"{"kind":"fixture","n":7}"#).expect("parse JSON");
    engine
        .catalog_mut()
        .get_table_mut("NativeRow")
        .expect("native fixture table")
        .insert(&vec![
            Value::Int(1),
            Value::Json(document.into_boxed_slice()),
            Value::Bytes(vec![0x00, 0x7f, 0x80, 0xff]),
            Value::Empty,
        ])
        .expect("insert native fixture row");

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind native query test server");
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

fn assert_native_fixture(message: Message) {
    let Message::ResultRowsNative { columns, rows } = message else {
        panic!("expected native rows");
    };
    assert_eq!(columns, ["data", "blob", "note"]);
    assert_eq!(rows.len(), 1);
    assert!(matches!(&rows[0][0], Value::Json(value) if !value.is_empty()));
    assert_eq!(rows[0][1], Value::Bytes(vec![0x00, 0x7f, 0x80, 0xff]));
    assert_eq!(rows[0][2], Value::Empty);
}

#[tokio::test]
async fn native_powql_sql_and_params_preserve_values() {
    let (addr, server) = start_server().await;
    let mut stream = connect(addr).await;

    assert_native_fixture(
        query(
            &mut stream,
            Message::QueryNative {
                query: "NativeRow filter .id = 1 { .data, .blob, .note }".into(),
            },
        )
        .await,
    );
    assert_native_fixture(
        query(
            &mut stream,
            Message::QuerySqlNative {
                query: "SELECT data, blob, note FROM NativeRow WHERE id = 1".into(),
            },
        )
        .await,
    );
    assert_native_fixture(
        query(
            &mut stream,
            Message::QueryWithParamsNative {
                query: "NativeRow filter .id = $1 { .data, .blob, .note }".into(),
                params: vec![WireParam::Int(1)],
            },
        )
        .await,
    );

    let scalar = query(
        &mut stream,
        Message::QueryNative {
            query: "count(NativeRow)".into(),
        },
    )
    .await;
    assert!(matches!(
        scalar,
        Message::ResultScalarNative {
            value: Value::Int(1)
        }
    ));
    server.abort();
}

#[tokio::test]
async fn pipelined_native_routes_keep_request_order() {
    let (addr, server) = start_server().await;
    let mut stream = connect(addr).await;
    let requests = [
        Message::QueryNative {
            query: "NativeRow filter .id = 1 { .data, .blob, .note }".into(),
        },
        Message::QuerySqlNative {
            query: "SELECT data, blob, note FROM NativeRow WHERE id = 1".into(),
        },
        Message::QueryWithParamsNative {
            query: "NativeRow filter .id = $1 { .data, .blob, .note }".into(),
            params: vec![WireParam::Int(1)],
        },
    ];
    let burst = requests
        .iter()
        .flat_map(Message::encode)
        .collect::<Vec<_>>();
    stream.write_all(&burst).await.expect("write native burst");

    for _ in requests {
        let response = Message::read_from(&mut stream)
            .await
            .expect("read pipelined response")
            .expect("server kept connection open");
        assert_native_fixture(response);
    }
    server.abort();
}
