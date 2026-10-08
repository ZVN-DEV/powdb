//! Server transaction-abort routing regressions.
//!
//! A statement failure inside an explicit transaction aborts that transaction:
//! later reads and COMMIT are refused until the owning connection rolls back.
//! The server has several pre-dispatch refusal paths (wire parse, SQL parse,
//! parameter binding) that used to return before the engine saw the failure.
//! These tests keep those paths aligned with engine-executed failures without
//! changing wire classes or error text.

mod common;

use std::sync::{Arc, RwLock};

use common::{encode_connect, encode_query, fresh_engine, read_response_message, InprocServer};
use powdb_query::executor::Engine;
use powdb_server::protocol::{Message, WireParam};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

async fn start(engine: Arc<RwLock<Engine>>) -> std::net::SocketAddr {
    let (addr, _handle) = InprocServer::default().start(engine).await;
    addr
}

async fn connect(addr: std::net::SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&encode_connect("testdb")).await.unwrap();
    match read_response_message(&mut stream).await {
        Message::ConnectOk { .. } | Message::ConnectOkWithHello { .. } => stream,
        other => panic!("connect failed: {other:?}"),
    }
}

async fn send(stream: &mut TcpStream, message: Message) -> Message {
    stream.write_all(&message.encode()).await.unwrap();
    read_response_message(stream).await
}

async fn powql(stream: &mut TcpStream, query: &str) -> Message {
    stream.write_all(&encode_query(query)).await.unwrap();
    read_response_message(stream).await
}

fn assert_ok(message: Message, what: &str) {
    match message {
        Message::ResultOk { .. } | Message::ResultMessage { .. } => {}
        other => panic!("{what}: expected success, got {other:?}"),
    }
}

fn error_text(message: Message) -> String {
    match message {
        Message::Error { message } | Message::ErrorWithClass { message, .. } => message,
        other => panic!("expected error, got {other:?}"),
    }
}

fn assert_transaction_aborted(message: Message, what: &str) {
    let text = error_text(message);
    assert!(
        text.contains("explicit transaction is aborted"),
        "{what}: expected transaction-aborted error, got {text:?}"
    );
}

async fn assert_count(addr: std::net::SocketAddr, expected: &str) {
    let mut stream = connect(addr).await;
    match powql(&mut stream, "count(Item)").await {
        Message::ResultScalar { value } => assert_eq!(value, expected),
        other => panic!("expected count scalar, got {other:?}"),
    }
}

fn seeded_engine() -> (Arc<RwLock<Engine>>, tempfile::TempDir) {
    let (engine, tmp) = fresh_engine();
    // Keep the TempDir alive by letting the in-memory engine own open handles
    // for the duration of each test. The directory is not reopened here.
    engine
        .write()
        .unwrap()
        .execute_powql("type Item { required id: int }")
        .unwrap();
    (engine, tmp)
}

#[tokio::test]
async fn powql_parse_failure_aborts_only_the_owning_transaction_until_rollback() {
    let (engine, _tmp) = seeded_engine();
    let addr = start(engine).await;
    let mut owner = connect(addr).await;

    assert_ok(powql(&mut owner, "begin").await, "begin");
    assert_ok(powql(&mut owner, "insert Item { id := 1 }").await, "insert");

    let parse_error = error_text(powql(&mut owner, "Item filter").await);
    assert!(
        parse_error.contains("parse error") || parse_error.contains("unexpected"),
        "expected parse failure, got {parse_error:?}"
    );

    assert_transaction_aborted(
        powql(&mut owner, "count(Item)").await,
        "read after parse failure",
    );
    assert_transaction_aborted(
        powql(&mut owner, "commit").await,
        "commit after parse failure",
    );
    assert_ok(
        powql(&mut owner, "rollback").await,
        "rollback aborted transaction",
    );
    assert_count(addr, "0").await;
}

#[tokio::test]
async fn native_powql_parse_failure_aborts_the_owning_transaction() {
    let (engine, _tmp) = seeded_engine();
    let addr = start(engine).await;
    let mut owner = connect(addr).await;

    assert_ok(powql(&mut owner, "begin").await, "begin");
    assert_ok(powql(&mut owner, "insert Item { id := 1 }").await, "insert");
    let native_parse = send(
        &mut owner,
        Message::QueryNative {
            query: "Item filter".to_string(),
        },
    )
    .await;
    let text = error_text(native_parse);
    assert!(
        text.contains("parse error") || text.contains("unexpected"),
        "expected native parse failure, got {text:?}"
    );

    assert_transaction_aborted(
        powql(&mut owner, "commit").await,
        "commit after native parse failure",
    );
    assert_ok(
        powql(&mut owner, "rollback").await,
        "rollback aborted transaction",
    );
    assert_count(addr, "0").await;
}

#[tokio::test]
async fn sql_parse_failure_aborts_the_owning_transaction() {
    let (engine, _tmp) = seeded_engine();
    let addr = start(engine).await;
    let mut owner = connect(addr).await;

    assert_ok(powql(&mut owner, "begin").await, "begin");
    assert_ok(powql(&mut owner, "insert Item { id := 1 }").await, "insert");
    let sql_parse = send(
        &mut owner,
        Message::QuerySql {
            query: "select from".to_string(),
        },
    )
    .await;
    let text = error_text(sql_parse);
    assert!(
        text.contains("parse error") || text.contains("expected"),
        "expected SQL parse failure, got {text:?}"
    );

    assert_transaction_aborted(
        powql(&mut owner, "commit").await,
        "commit after SQL parse failure",
    );
    assert_ok(
        powql(&mut owner, "rollback").await,
        "rollback aborted transaction",
    );
    assert_count(addr, "0").await;
}

#[tokio::test]
async fn parameter_bind_failure_aborts_the_owning_transaction() {
    let (engine, _tmp) = seeded_engine();
    let addr = start(engine).await;
    let mut owner = connect(addr).await;

    assert_ok(powql(&mut owner, "begin").await, "begin");
    assert_ok(powql(&mut owner, "insert Item { id := 1 }").await, "insert");
    let bind_error = send(
        &mut owner,
        Message::QueryWithParams {
            query: "Item filter .id = $2".to_string(),
            params: vec![WireParam::Int(1)],
        },
    )
    .await;
    let text = error_text(bind_error);
    assert!(
        text.contains("parameter") || text.contains("$2"),
        "expected parameter binding failure, got {text:?}"
    );

    assert_transaction_aborted(
        powql(&mut owner, "count(Item)").await,
        "read after bind failure",
    );
    assert_transaction_aborted(
        powql(&mut owner, "commit").await,
        "commit after bind failure",
    );
    assert_ok(
        powql(&mut owner, "rollback").await,
        "rollback aborted transaction",
    );
    assert_count(addr, "0").await;
}

#[tokio::test]
async fn native_parameter_bind_failure_aborts_the_owning_transaction() {
    let (engine, _tmp) = seeded_engine();
    let addr = start(engine).await;
    let mut owner = connect(addr).await;

    assert_ok(powql(&mut owner, "begin").await, "begin");
    assert_ok(powql(&mut owner, "insert Item { id := 1 }").await, "insert");
    let bind_error = send(
        &mut owner,
        Message::QueryWithParamsNative {
            query: "Item filter .id = $2".to_string(),
            params: vec![WireParam::Int(1)],
        },
    )
    .await;
    let text = error_text(bind_error);
    assert!(
        text.contains("parameter") || text.contains("$2"),
        "expected native parameter binding failure, got {text:?}"
    );

    assert_transaction_aborted(
        powql(&mut owner, "count(Item)").await,
        "read after native bind failure",
    );
    assert_transaction_aborted(
        powql(&mut owner, "commit").await,
        "commit after native bind failure",
    );
    assert_ok(
        powql(&mut owner, "rollback").await,
        "rollback aborted transaction",
    );
    assert_count(addr, "0").await;
}

#[tokio::test]
async fn native_sql_parse_failure_aborts_the_owning_transaction() {
    let (engine, _tmp) = seeded_engine();
    let addr = start(engine).await;
    let mut owner = connect(addr).await;

    assert_ok(powql(&mut owner, "begin").await, "begin");
    assert_ok(powql(&mut owner, "insert Item { id := 1 }").await, "insert");
    let sql_parse = send(
        &mut owner,
        Message::QuerySqlNative {
            query: "select from".to_string(),
        },
    )
    .await;
    let text = error_text(sql_parse);
    assert!(
        text.contains("parse error") || text.contains("expected"),
        "expected native SQL parse failure, got {text:?}"
    );

    assert_transaction_aborted(
        powql(&mut owner, "count(Item)").await,
        "read after native SQL parse failure",
    );
    assert_transaction_aborted(
        powql(&mut owner, "commit").await,
        "commit after native SQL parse failure",
    );
    assert_ok(
        powql(&mut owner, "rollback").await,
        "rollback aborted transaction",
    );
    assert_count(addr, "0").await;
}

#[tokio::test]
async fn parse_failure_on_another_connection_does_not_abort_the_owner_transaction() {
    let (engine, _tmp) = seeded_engine();
    let addr = start(engine).await;
    let mut owner = connect(addr).await;
    let mut outsider = connect(addr).await;

    assert_ok(powql(&mut owner, "begin").await, "owner begin");
    assert_ok(
        powql(&mut owner, "insert Item { id := 1 }").await,
        "owner insert",
    );

    let outsider_error = error_text(powql(&mut outsider, "Item filter").await);
    assert!(
        outsider_error.contains("parse error") || outsider_error.contains("unexpected"),
        "expected outsider parse failure, got {outsider_error:?}"
    );

    assert_ok(
        powql(&mut owner, "commit").await,
        "owner commit after outsider rejection",
    );
    assert_count(addr, "1").await;
}

#[tokio::test]
async fn native_rejection_on_another_connection_does_not_abort_the_owner_transaction() {
    let (engine, _tmp) = seeded_engine();
    let addr = start(engine).await;
    let mut owner = connect(addr).await;
    let mut outsider = connect(addr).await;

    assert_ok(powql(&mut owner, "begin").await, "owner begin");
    assert_ok(
        powql(&mut owner, "insert Item { id := 1 }").await,
        "owner insert",
    );

    let outsider_sql = error_text(
        send(
            &mut outsider,
            Message::QuerySqlNative {
                query: "select from".to_string(),
            },
        )
        .await,
    );
    assert!(
        outsider_sql.contains("parse error") || outsider_sql.contains("expected"),
        "expected outsider native SQL parse failure, got {outsider_sql:?}"
    );

    let outsider_bind = error_text(
        send(
            &mut outsider,
            Message::QueryWithParamsNative {
                query: "Item filter .id = $2".to_string(),
                params: vec![WireParam::Int(1)],
            },
        )
        .await,
    );
    assert!(
        outsider_bind.contains("parameter") || outsider_bind.contains("$2"),
        "expected outsider native bind failure, got {outsider_bind:?}"
    );

    assert_ok(
        powql(&mut owner, "commit").await,
        "owner commit after outsider native rejections",
    );
    assert_count(addr, "1").await;
}
