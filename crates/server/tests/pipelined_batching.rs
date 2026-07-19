//! Read-ahead batching + group-commit contract over the real wire protocol.
//!
//! A pipelining client writes many query frames back-to-back without waiting
//! for replies. The server executes them one at a time but settles the WAL
//! durability obligation ONCE per burst (the newest ticket covers every
//! earlier generation), so N pipelined durable writes share far fewer than N
//! fsyncs — while every reply still arrives in statement order and full-mode
//! durability semantics are preserved.

mod common;

use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use common::{encode_connect, encode_ping, encode_query, read_response, InprocServer};

const MSG_CONNECT_OK: u8 = 0x02;
const MSG_RESULT_ROWS: u8 = 0x07;
const MSG_RESULT_SCALAR: u8 = 0x08;
const MSG_RESULT_OK: u8 = 0x09;
const MSG_ERROR: u8 = 0x0A;
const MSG_RESULT_MSG: u8 = 0x0B;
const MSG_PONG: u8 = 0x12;

fn assert_success(resp: &[u8], what: &str) {
    assert_ne!(
        resp[0],
        MSG_ERROR,
        "{what} failed: {:?}",
        powdb_server::protocol::Message::decode(resp)
    );
}

/// Spawn a real server on an OS-assigned port, sharing the engine Arc with
/// the test so fsync counters can be observed. Returns (addr, engine).
async fn spawn_server(
    data_dir: &std::path::Path,
) -> (String, Arc<RwLock<powdb_query::executor::Engine>>) {
    let engine = powdb_query::executor::Engine::new(data_dir).unwrap();
    let engine = Arc::new(RwLock::new(engine));
    let (addr, _handle) = InprocServer::default().start(engine.clone()).await;
    (addr.to_string(), engine)
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    common::unique_temp_dir(&format!("pipeline_{name}"))
}

/// The core contract: a pipelined burst of durable writes gets every reply,
/// in order, and the whole burst shares fsyncs instead of paying one each.
#[tokio::test]
async fn pipelined_burst_answers_in_order_and_shares_fsyncs() {
    let dir = temp_dir("burst");
    std::fs::create_dir_all(&dir).unwrap();
    let (addr, engine) = spawn_server(&dir).await;

    let mut stream = TcpStream::connect(&addr).await.unwrap();
    stream.write_all(&encode_connect("testdb")).await.unwrap();
    assert_eq!(read_response(&mut stream).await[0], MSG_CONNECT_OK);

    stream
        .write_all(&encode_query("type B { required id: int }"))
        .await
        .unwrap();
    assert_success(&read_response(&mut stream).await, "create type");

    let base = engine.read().unwrap().wal_fsync_count();

    // Write the whole burst back-to-back — no reply waits.
    const N: usize = 50;
    let mut burst = Vec::new();
    for i in 0..N {
        burst.extend_from_slice(&encode_query(&format!("insert B {{ id := {i} }}")));
    }
    burst.extend_from_slice(&encode_query("count(B)"));
    stream.write_all(&burst).await.unwrap();

    // Every reply arrives, in statement order.
    for i in 0..N {
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], MSG_RESULT_OK, "insert {i} reply");
    }
    let count_resp = read_response(&mut stream).await;
    assert_eq!(count_resp[0], MSG_RESULT_SCALAR, "count reply");
    match powdb_server::protocol::Message::decode(&count_resp).unwrap() {
        powdb_server::protocol::Message::ResultScalar { value } => {
            assert_eq!(value, N.to_string(), "all pipelined inserts applied");
        }
        other => panic!("expected scalar, got {other:?}"),
    }

    // The batching win: far fewer fsyncs than durable statements. The exact
    // number depends on TCP segmentation (each batch = one fsync), so pin
    // only the direction — strictly fewer than one per insert.
    let fsyncs = engine.read().unwrap().wal_fsync_count() - base;
    assert!(
        fsyncs < N as u64,
        "pipelined burst should share fsyncs: {fsyncs} fsyncs for {N} inserts"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A non-query frame in the middle of a burst flushes the batch and is then
/// handled normally — replies stay in frame order (result, PONG, result).
#[tokio::test]
async fn non_query_frame_mid_burst_carries_over() {
    let dir = temp_dir("carry");
    std::fs::create_dir_all(&dir).unwrap();
    let (addr, _engine) = spawn_server(&dir).await;

    let mut stream = TcpStream::connect(&addr).await.unwrap();
    stream.write_all(&encode_connect("testdb")).await.unwrap();
    assert_eq!(read_response(&mut stream).await[0], MSG_CONNECT_OK);

    stream
        .write_all(&encode_query("type C { required id: int }"))
        .await
        .unwrap();
    assert_success(&read_response(&mut stream).await, "create type");

    let mut burst = Vec::new();
    burst.extend_from_slice(&encode_query("insert C { id := 1 }"));
    burst.extend_from_slice(&encode_ping());
    burst.extend_from_slice(&encode_query("insert C { id := 2 }"));
    burst.extend_from_slice(&encode_query("count(C)"));
    stream.write_all(&burst).await.unwrap();

    assert_eq!(read_response(&mut stream).await[0], MSG_RESULT_OK);
    assert_eq!(read_response(&mut stream).await[0], MSG_PONG);
    assert_eq!(read_response(&mut stream).await[0], MSG_RESULT_OK);
    let count_resp = read_response(&mut stream).await;
    assert_eq!(count_resp[0], MSG_RESULT_SCALAR);

    std::fs::remove_dir_all(&dir).ok();
}

/// An explicit transaction inside a pipelined burst keeps its semantics:
/// begin stops read-ahead (the connection holds the TxGate), the statements
/// run inside the transaction, and commit makes them durable.
#[tokio::test]
async fn pipelined_transaction_burst_preserves_semantics() {
    let dir = temp_dir("txburst");
    std::fs::create_dir_all(&dir).unwrap();
    let (addr, _engine) = spawn_server(&dir).await;

    let mut stream = TcpStream::connect(&addr).await.unwrap();
    stream.write_all(&encode_connect("testdb")).await.unwrap();
    assert_eq!(read_response(&mut stream).await[0], MSG_CONNECT_OK);

    stream
        .write_all(&encode_query("type T { required id: int }"))
        .await
        .unwrap();
    assert_success(&read_response(&mut stream).await, "create type");

    let mut burst = Vec::new();
    for stmt in [
        "begin",
        "insert T { id := 1 }",
        "insert T { id := 2 }",
        "commit",
        "count(T)",
    ] {
        burst.extend_from_slice(&encode_query(stmt));
    }
    stream.write_all(&burst).await.unwrap();

    for stmt in ["begin", "insert 1", "insert 2", "commit"] {
        let resp = read_response(&mut stream).await;
        assert_success(&resp, stmt);
    }
    let count_resp = read_response(&mut stream).await;
    assert_eq!(count_resp[0], MSG_RESULT_SCALAR);
    match powdb_server::protocol::Message::decode(&count_resp).unwrap() {
        powdb_server::protocol::Message::ResultScalar { value } => {
            assert_eq!(value, "2", "both transactional inserts committed");
        }
        other => panic!("expected scalar, got {other:?}"),
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// Concurrent connections issuing awaited durable writes: correctness under
/// the released-before-wait TxGate ordering — every statement acknowledged,
/// nothing lost, no deadlock. (Fsync sharing across connections is timing-
/// dependent, so it is pinned deterministically in the engine-level suite,
/// not here.)
#[tokio::test]
async fn concurrent_connections_commit_durably() {
    let dir = temp_dir("concurrent");
    std::fs::create_dir_all(&dir).unwrap();
    let (addr, _engine) = spawn_server(&dir).await;

    let mut setup = TcpStream::connect(&addr).await.unwrap();
    setup.write_all(&encode_connect("testdb")).await.unwrap();
    assert_eq!(read_response(&mut setup).await[0], MSG_CONNECT_OK);
    setup
        .write_all(&encode_query("type W { required id: int }"))
        .await
        .unwrap();
    assert_success(&read_response(&mut setup).await, "create type");

    const CONNS: usize = 4;
    const PER_CONN: usize = 10;
    let mut tasks = Vec::new();
    for c in 0..CONNS {
        let addr = addr.clone();
        tasks.push(tokio::spawn(async move {
            let mut stream = TcpStream::connect(&addr).await.unwrap();
            stream.write_all(&encode_connect("testdb")).await.unwrap();
            assert_eq!(read_response(&mut stream).await[0], MSG_CONNECT_OK);
            for i in 0..PER_CONN {
                let id = c * PER_CONN + i;
                stream
                    .write_all(&encode_query(&format!("insert W {{ id := {id} }}")))
                    .await
                    .unwrap();
                let resp = read_response(&mut stream).await;
                assert_eq!(resp[0], MSG_RESULT_OK, "conn {c} insert {i}");
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }

    setup.write_all(&encode_query("count(W)")).await.unwrap();
    let count_resp = read_response(&mut setup).await;
    match powdb_server::protocol::Message::decode(&count_resp).unwrap() {
        powdb_server::protocol::Message::ResultScalar { value } => {
            assert_eq!(
                value,
                (CONNS * PER_CONN).to_string(),
                "every acknowledged concurrent insert applied"
            );
        }
        other => panic!("expected scalar, got {other:?}"),
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// Rows-returning queries interleave correctly inside a burst (read-only
/// statements produce no ticket; mixing them with writes must not confuse
/// the batch's single durability wait or reply ordering).
#[tokio::test]
async fn mixed_read_write_burst_keeps_order() {
    let dir = temp_dir("mixed");
    std::fs::create_dir_all(&dir).unwrap();
    let (addr, _engine) = spawn_server(&dir).await;

    let mut stream = TcpStream::connect(&addr).await.unwrap();
    stream.write_all(&encode_connect("testdb")).await.unwrap();
    assert_eq!(read_response(&mut stream).await[0], MSG_CONNECT_OK);

    stream
        .write_all(&encode_query("type M { required id: int }"))
        .await
        .unwrap();
    assert_success(&read_response(&mut stream).await, "create type");

    let mut burst = Vec::new();
    burst.extend_from_slice(&encode_query("insert M { id := 1 }"));
    burst.extend_from_slice(&encode_query("count(M)"));
    burst.extend_from_slice(&encode_query("insert M { id := 2 }"));
    burst.extend_from_slice(&encode_query("M"));
    stream.write_all(&burst).await.unwrap();

    assert_eq!(read_response(&mut stream).await[0], MSG_RESULT_OK);
    let mid_count = read_response(&mut stream).await;
    assert_eq!(mid_count[0], MSG_RESULT_SCALAR);
    match powdb_server::protocol::Message::decode(&mid_count).unwrap() {
        powdb_server::protocol::Message::ResultScalar { value } => {
            assert_eq!(value, "1", "count sees exactly the first insert");
        }
        other => panic!("expected scalar, got {other:?}"),
    }
    assert_eq!(read_response(&mut stream).await[0], MSG_RESULT_OK);
    assert_eq!(read_response(&mut stream).await[0], MSG_RESULT_ROWS);

    std::fs::remove_dir_all(&dir).ok();
}

/// A partial next frame must NOT hold the current statement's reply hostage:
/// read-ahead only proceeds on a fully-buffered frame, so the batch flushes
/// and the first reply arrives even though the second frame is incomplete.
#[tokio::test]
async fn partial_next_frame_does_not_delay_replies() {
    let dir = temp_dir("partial");
    std::fs::create_dir_all(&dir).unwrap();
    let (addr, _engine) = spawn_server(&dir).await;

    let mut stream = TcpStream::connect(&addr).await.unwrap();
    stream.write_all(&encode_connect("testdb")).await.unwrap();
    assert_eq!(read_response(&mut stream).await[0], MSG_CONNECT_OK);

    stream
        .write_all(&encode_query("type P { required id: int }"))
        .await
        .unwrap();
    assert_success(&read_response(&mut stream).await, "create type");

    // One complete frame + the first 3 bytes of the next, in a single write.
    let first = encode_query("insert P { id := 1 }");
    let second = encode_query("insert P { id := 2 }");
    let mut chunk = first.clone();
    chunk.extend_from_slice(&second[..3]);
    stream.write_all(&chunk).await.unwrap();

    // The first reply must arrive promptly — well before the idle timeout —
    // even though the tail of the next frame never arrived yet.
    let resp = tokio::time::timeout(Duration::from_secs(5), read_response(&mut stream))
        .await
        .expect("first reply must not wait for the partial second frame");
    assert_eq!(resp[0], MSG_RESULT_OK);

    // Complete the second frame; its reply follows normally.
    stream.write_all(&second[3..]).await.unwrap();
    let resp = read_response(&mut stream).await;
    assert_eq!(resp[0], MSG_RESULT_OK);

    std::fs::remove_dir_all(&dir).ok();
}

// Silence dead-code warnings for constants only some tests use.
#[allow(dead_code)]
const _: (u8, u8) = (MSG_RESULT_MSG, MSG_RESULT_ROWS);
