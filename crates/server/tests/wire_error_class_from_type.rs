//! The wire error class of a storage refusal must follow the refusal's TYPE.
//!
//! `docs/errors.md` calls the class byte stable wire contract, and drivers
//! branch on it: class 8 means "the write was rejected by a constraint, fix
//! the data", class 0 means "the server broke, there is nothing on your side
//! to fix". Getting that wrong sends a driver down the wrong recovery path.
//!
//! The classes below were historically recovered by substring-matching the
//! rendered message, because a storage refusal reached the server as
//! `QueryError::StorageError(String)` with its variant already discarded.
//! These tests pin each class end to end, over the real binary, so the move
//! to type-driven classification is provably behavior-preserving where the
//! class was already right, and provably fixes it where it was not.

mod common;

use std::time::Duration;

use common::{
    encode_connect, encode_query, free_port, read_response, spawn_server_bin, spawn_server_bin_env,
    wait_for_bind, wait_with_timeout,
};
use powdb_server::protocol::{decode_error_class, ErrorClass, Message};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// Eight 4 KiB pages, matching `dirty_page_budget_env.rs`: small enough that a
/// few hundred padded rows blow past it, large enough for the schema write.
const TINY_BUDGET_BYTES: &str = "32768";

async fn connect_ok(port: u16) -> TcpStream {
    let mut stream = wait_for_bind(port, Duration::from_secs(20)).await;
    stream.write_all(&encode_connect("testdb")).await.unwrap();
    assert_eq!(
        read_response(&mut stream).await[0],
        0x02,
        "expected CONNECT_OK"
    );
    stream
}

/// Run a statement that must succeed.
async fn query_ok(stream: &mut TcpStream, query: &str) {
    stream.write_all(&encode_query(query)).await.unwrap();
    let frame = read_response(stream).await;
    assert_ne!(
        frame[0],
        0x0A,
        "statement {query:?} unexpectedly failed: {}",
        error_message(&frame)
    );
}

/// Run a statement that must fail, returning its raw error frame.
async fn query_error_frame(stream: &mut TcpStream, query: &str) -> Vec<u8> {
    stream.write_all(&encode_query(query)).await.unwrap();
    let frame = read_response(stream).await;
    assert_eq!(
        frame[0], 0x0A,
        "expected MSG_ERROR for {query:?}, got 0x{:02X}",
        frame[0]
    );
    frame
}

fn error_message(frame: &[u8]) -> String {
    match Message::decode(frame).expect("an error frame must decode") {
        Message::Error { message } => message,
        other => panic!("expected an error frame, got {other:?}"),
    }
}

#[track_caller]
fn assert_class(frame: &[u8], expected: ErrorClass, what: &str) {
    assert_eq!(
        decode_error_class(frame),
        Some(expected.as_u8()),
        "{what}: expected class {} ({expected:?}) per docs/errors.md, message was {:?}",
        expected.as_u8(),
        error_message(frame)
    );
}

/// docs/errors.md class 8 (`constraint_violation`): "A constraint rejected the
/// write." A unique *expression* index is a constraint exactly as a unique
/// column is, and its refusal is raised by the same storage preflight. Because
/// the class was recovered by matching the literal substring
/// `unique constraint violation`, this refusal (`unique expression index
/// violation on ...`) missed the match and reported class 0 (`internal`),
/// telling drivers the server had broken when the caller had simply inserted a
/// duplicate.
#[tokio::test]
async fn unique_expression_index_violation_carries_constraint_class() {
    let tmp = tempfile::tempdir().unwrap();
    let port = free_port();
    let mut child = spawn_server_bin(port, tmp.path(), &[]);
    let mut stream = connect_ok(port).await;

    query_ok(&mut stream, "type Doc { required id: int, data: json }").await;
    query_ok(
        &mut stream,
        r#"insert Doc { id := 1, data := "{\"code\":\"a\"}" }"#,
    )
    .await;
    query_ok(&mut stream, "alter Doc add unique (.data->code)").await;

    let frame = query_error_frame(
        &mut stream,
        r#"insert Doc { id := 2, data := "{\"code\":\"a\"}" }"#,
    )
    .await;

    let class = decode_error_class(&frame);
    let _ = child.kill();
    wait_with_timeout(&mut child, Duration::from_secs(10));

    assert_eq!(
        class,
        Some(ErrorClass::ConstraintViolation.as_u8()),
        "a duplicate key on a unique expression index is a constraint \
         violation (class 8), not an internal server error (class 0)"
    );
}

/// The column-level twin of the test above, and the class that must not move.
#[tokio::test]
async fn unique_column_violation_keeps_constraint_class() {
    let tmp = tempfile::tempdir().unwrap();
    let port = free_port();
    let mut child = spawn_server_bin(port, tmp.path(), &[]);
    let mut stream = connect_ok(port).await;

    query_ok(&mut stream, "type Uniq { unique u: int }").await;
    query_ok(&mut stream, "insert Uniq { u := 1 }").await;
    let frame = query_error_frame(&mut stream, "insert Uniq { u := 1 }").await;

    let _ = child.kill();
    wait_with_timeout(&mut child, Duration::from_secs(10));

    assert_class(
        &frame,
        ErrorClass::ConstraintViolation,
        "a duplicate value in a unique column",
    );
    assert!(
        error_message(&frame).starts_with("unique constraint violation"),
        "the refusal text is on the egress allowlist and must cross verbatim"
    );
}

/// docs/errors.md class 2 (`execution`): "`cannot begin` while a transaction
/// is active" and friends. DDL inside an explicit transaction is refused by
/// the storage catalog, and the client can fix it by committing first, so it
/// must not read as an internal fault.
#[tokio::test]
async fn ddl_inside_a_transaction_keeps_execution_class() {
    let tmp = tempfile::tempdir().unwrap();
    let port = free_port();
    let mut child = spawn_server_bin(port, tmp.path(), &[]);
    let mut stream = connect_ok(port).await;

    query_ok(&mut stream, "type Doomed { required id: int }").await;
    query_ok(&mut stream, "begin").await;
    let frame = query_error_frame(&mut stream, "drop Doomed").await;

    let _ = child.kill();
    wait_with_timeout(&mut child, Duration::from_secs(10));

    assert_class(
        &frame,
        ErrorClass::Execution,
        "DDL refused inside an explicit transaction",
    );
    assert!(
        error_message(&frame).contains("DDL is not transactional"),
        "the refusal must still name the reason, got {:?}",
        error_message(&frame)
    );
}

/// docs/errors.md class 4 (`limit_exceeded`): "A memory or size limit was
/// exceeded". A transaction that outgrows the dirty-page budget is refused
/// rather than buffered until the process dies, and the client's fix is to
/// commit more often, so the class must say "limit", not "internal".
#[tokio::test]
async fn transaction_over_the_dirty_page_budget_keeps_limit_class() {
    let tmp = tempfile::tempdir().unwrap();
    let port = free_port();
    let mut child = spawn_server_bin_env(
        port,
        tmp.path(),
        &[],
        &[("POWDB_DIRTY_PAGE_BUDGET", TINY_BUDGET_BYTES)],
    );
    let mut stream = connect_ok(port).await;

    query_ok(
        &mut stream,
        "type Big { required id: int, required pad: str }",
    )
    .await;
    query_ok(&mut stream, "begin").await;

    let pad = "x".repeat(400);
    let rows: Vec<String> = (0..300)
        .map(|i| format!("{{ id := {i}, pad := \"{pad}\" }}"))
        .collect();
    let frame = query_error_frame(&mut stream, &format!("insert Big {}", rows.join(", "))).await;

    let _ = child.kill();
    wait_with_timeout(&mut child, Duration::from_secs(10));

    assert_class(
        &frame,
        ErrorClass::LimitExceeded,
        "a transaction over the dirty-page budget",
    );
    assert!(
        error_message(&frame).contains("dirty-page budget"),
        "the refusal must still name the budget, got {:?}",
        error_message(&frame)
    );
}
