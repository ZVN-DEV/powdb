//! An open transaction that has not written must not stop other connections
//! from reading.
//!
//! The write-admission gate is documented as serializing WRITERS, and the
//! README advertises parallel reads. In practice a bare `begin` took the whole
//! gate, so every statement on every other connection, reads on unrelated
//! tables and `schema` included, waited out the holder or timed out after the
//! gate budget. A transaction that has written nothing has nothing uncommitted
//! to hide, so that wait bought no isolation.
//!
//! Once the holder HAS written, readers must still wait: there is no MVCC, so
//! letting them through would serve uncommitted rows, which is worse than
//! waiting.

mod common;

use common::{encode_connect, encode_query, read_response_message, InprocServer};
use powdb_query::executor::Engine;
use powdb_server::protocol::Message;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// Short enough that a blocked read is unmistakable, long enough that a
/// loaded machine does not trip the "fast" assertion.
const GATE_WAIT: Duration = Duration::from_millis(800);

async fn connect(addr: std::net::SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&encode_connect("test")).await.unwrap();
    match read_response_message(&mut stream).await {
        Message::ConnectOk { .. } | Message::ConnectOkWithHello { .. } => {}
        other => panic!("expected CONNECT_OK, got {other:?}"),
    }
    stream
}

async fn run(stream: &mut TcpStream, query: &str) -> Message {
    stream.write_all(&encode_query(query)).await.unwrap();
    read_response_message(stream).await
}

fn is_error(message: &Message) -> bool {
    matches!(
        message,
        Message::Error { .. } | Message::ErrorWithClass { .. }
    )
}

async fn seeded_server() -> (tempfile::TempDir, std::net::SocketAddr) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine.execute_powql("type T { required id: int }").unwrap();
    engine
        .execute_powql("type Other { required id: int }")
        .unwrap();
    engine.execute_powql("insert T { id := 1 }").unwrap();
    let (addr, _server) = InprocServer {
        tx_wait_timeout: GATE_WAIT,
        ..Default::default()
    }
    .start(Arc::new(RwLock::new(engine)))
    .await;
    (dir, addr)
}

#[tokio::test]
async fn a_begin_only_transaction_does_not_block_reads_on_other_connections() {
    let (_dir, addr) = seeded_server().await;
    let mut holder = connect(addr).await;
    let mut reader = connect(addr).await;

    let begun = run(&mut holder, "begin").await;
    assert!(!is_error(&begun), "begin was refused: {begun:?}");

    for probe in ["count(T)", "Other { .id }", "schema"] {
        let start = Instant::now();
        let reply = run(&mut reader, probe).await;
        let elapsed = start.elapsed();
        assert!(
            !is_error(&reply),
            "`{probe}` was refused while another connection held a begin-only transaction: {reply:?}"
        );
        assert!(
            elapsed < GATE_WAIT / 2,
            "`{probe}` took {elapsed:?} while another connection held a begin-only \
             transaction; it should not have queued behind the gate at all"
        );
    }

    let rolled = run(&mut holder, "rollback").await;
    assert!(!is_error(&rolled), "rollback was refused: {rolled:?}");
}

#[tokio::test]
async fn a_transaction_with_uncommitted_writes_still_blocks_readers() {
    let (_dir, addr) = seeded_server().await;
    let mut holder = connect(addr).await;
    let mut reader = connect(addr).await;

    assert!(!is_error(&run(&mut holder, "begin").await));
    let written = run(&mut holder, "insert T { id := 2 }").await;
    assert!(
        !is_error(&written),
        "insert in transaction failed: {written:?}"
    );

    let start = Instant::now();
    let reply = run(&mut reader, "count(T)").await;
    let elapsed = start.elapsed();
    assert!(
        is_error(&reply),
        "a read must not be served while another connection holds uncommitted writes; \
         it would see them. Got {reply:?}"
    );
    assert!(
        elapsed >= GATE_WAIT,
        "the read returned after {elapsed:?}, before the gate budget elapsed; it did not \
         actually wait for the writer"
    );

    let rolled = run(&mut holder, "rollback").await;
    assert!(!is_error(&rolled), "rollback was refused: {rolled:?}");

    // With the transaction gone the same read is served immediately, and the
    // rolled-back row is not there.
    let reply = run(&mut reader, "count(T)").await;
    match reply {
        Message::ResultScalar { value } => assert_eq!(value, "1"),
        Message::ResultScalarNative { value } => assert_eq!(value.to_wire_string(), "1"),
        other => panic!("expected a scalar count after rollback, got {other:?}"),
    }
}

/// Two explicit transactions must still not overlap: the engine has one
/// transaction, so a second `begin` waits for the first to finish.
#[tokio::test]
async fn a_second_begin_still_waits_for_the_first() {
    let (_dir, addr) = seeded_server().await;
    let mut first = connect(addr).await;
    let mut second = connect(addr).await;

    assert!(!is_error(&run(&mut first, "begin").await));
    let start = Instant::now();
    let reply = run(&mut second, "begin").await;
    let elapsed = start.elapsed();
    assert!(
        is_error(&reply),
        "two explicit transactions must not be open at once, got {reply:?}"
    );
    assert!(
        elapsed >= GATE_WAIT,
        "the second begin returned after {elapsed:?} instead of waiting out the gate budget"
    );

    assert!(!is_error(&run(&mut first, "rollback").await));
}

/// An autocommit write on another connection must still not run inside the
/// holder's transaction.
#[tokio::test]
async fn an_autocommit_write_still_waits_for_a_begin_only_transaction() {
    let (_dir, addr) = seeded_server().await;
    let mut holder = connect(addr).await;
    let mut writer = connect(addr).await;

    assert!(!is_error(&run(&mut holder, "begin").await));
    let start = Instant::now();
    let reply = run(&mut writer, "insert T { id := 3 }").await;
    let elapsed = start.elapsed();
    assert!(
        is_error(&reply),
        "an autocommit write must not join another connection's open transaction, got {reply:?}"
    );
    assert!(
        elapsed >= GATE_WAIT,
        "the write returned after {elapsed:?} instead of waiting out the gate budget"
    );

    assert!(!is_error(&run(&mut holder, "rollback").await));
}
