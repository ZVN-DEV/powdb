//! End-to-end cooperative query cancellation over the TCP wire.
//!
//! The important distinction in these tests is between a query deadline and a
//! dead client. Timeout coverage uses a short configured deadline. Disconnect
//! coverage uses a 30-second deadline but requires recovery within two seconds,
//! so passing cannot be explained by eventual deadline cancellation.

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use common::{encode_connect, InprocServer};
use powdb_query::executor::Engine;
use powdb_server::metrics::Metrics;
use powdb_server::protocol::{Message, WireParam};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

static SERVER_ID: AtomicU64 = AtomicU64::new(0);

/// Arithmetic around the equality key prevents hash-key extraction and forces
/// the cancellation-polled nested-loop path. The fixture remains below the
/// measured nested-loop pair cap.
const SLOW_POWQL: &str =
    r#"Ver as ver join Grp as g on ver.id + 0 = g.version_id and g.field_ns = "f1""#;
const SLOW_POWQL_PARAM: &str =
    "Ver as ver join Grp as g on ver.id + 0 = g.version_id and g.field_ns = $1";
const SLOW_SQL: &str = "SELECT * FROM Ver AS ver JOIN Grp AS g \
    ON ver.id + 0 = g.version_id AND g.field_ns = 'f1'";
const SLOW_ROWS: usize = 2_500;
const SLOW_ROWS_TEXT: &str = "2500";

#[derive(Clone, Copy, Debug)]
enum QueryRoute {
    PowQl,
    Sql,
    Params,
    NativePowQl,
    NativeSql,
    NativeParams,
}

impl QueryRoute {
    const ALL: [Self; 6] = [
        Self::PowQl,
        Self::Sql,
        Self::Params,
        Self::NativePowQl,
        Self::NativeSql,
        Self::NativeParams,
    ];

    fn slow_frame(self) -> Vec<u8> {
        match self {
            Self::PowQl => Message::Query {
                query: SLOW_POWQL.into(),
            },
            Self::Sql => Message::QuerySql {
                query: SLOW_SQL.into(),
            },
            Self::Params => Message::QueryWithParams {
                query: SLOW_POWQL_PARAM.into(),
                params: vec![WireParam::Str("f1".into())],
            },
            Self::NativePowQl => Message::QueryNative {
                query: SLOW_POWQL.into(),
            },
            Self::NativeSql => Message::QuerySqlNative {
                query: SLOW_SQL.into(),
            },
            Self::NativeParams => Message::QueryWithParamsNative {
                query: SLOW_POWQL_PARAM.into(),
                params: vec![WireParam::Str("f1".into())],
            },
        }
        .encode()
    }
}

async fn read_response(stream: &mut TcpStream) -> Message {
    common::read_response_message(stream).await
}

async fn start_join_server(
    n: usize,
    query_timeout: Duration,
    tx_wait_timeout: Duration,
) -> (String, tokio::task::JoinHandle<()>, Arc<Metrics>) {
    let unique = SERVER_ID.fetch_add(1, Ordering::Relaxed);
    let data_dir = std::env::temp_dir().join(format!(
        "powdb_cancel_wire_{}_{}",
        std::process::id(),
        unique
    ));
    let _ = std::fs::remove_dir_all(&data_dir);
    std::fs::create_dir_all(&data_dir).unwrap();

    let mut engine = Engine::new(Path::new(&data_dir)).unwrap();
    engine.set_wal_sync_mode(powdb_query::executor::WalSyncMode::Off);
    engine
        .execute_powql("type Ver { required id: int }")
        .unwrap();
    engine
        .execute_powql("type Grp { required version_id: int, required field_ns: str }")
        .unwrap();
    engine
        .execute_powql("type TxProbe { required id: int }")
        .unwrap();
    for i in 0..n {
        engine
            .execute_powql(&format!("insert Ver {{ id := {i} }}"))
            .unwrap();
        engine
            .execute_powql(&format!(
                r#"insert Grp {{ version_id := {i}, field_ns := "f1" }}"#
            ))
            .unwrap();
    }

    let engine = Arc::new(RwLock::new(engine));
    let metrics = Arc::new(Metrics::new());

    let (addr, handle) = InprocServer {
        tx_wait_timeout,
        query_timeout,
        metrics: metrics.clone(),
        ..Default::default()
    }
    .start(engine)
    .await;

    (addr.to_string(), handle, metrics)
}

async fn connect(addr: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&encode_connect("testdb")).await.unwrap();
    assert!(matches!(
        read_response(&mut stream).await,
        Message::ConnectOk { .. }
    ));
    stream
}

async fn query(stream: &mut TcpStream, text: &str) -> Message {
    stream
        .write_all(&Message::Query { query: text.into() }.encode())
        .await
        .unwrap();
    read_response(stream).await
}

async fn wait_for_in_flight(metrics: &Metrics) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if !metrics.render().contains("powdb_queries_in_flight 0") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "query never reached the blocking executor"
        );
        tokio::task::yield_now().await;
    }
}

async fn assert_scalar_within(addr: &str, text: &str, expected: &str, bound: Duration) {
    let mut client = connect(addr).await;
    let started = Instant::now();
    let result = tokio::time::timeout(bound, query(&mut client, text))
        .await
        .expect("fresh query remained blocked after cancellation");
    assert!(started.elapsed() < bound);
    match result {
        Message::ResultScalar { value } => assert_eq!(value, expected),
        other => panic!("expected scalar {expected}, got {other:?}"),
    }
}

#[tokio::test]
async fn timeout_is_typed_and_counted_for_all_query_routes() {
    let timeout = Duration::from_millis(75);
    let (addr, handle, metrics) =
        start_join_server(SLOW_ROWS, timeout, Duration::from_secs(5)).await;

    for route in QueryRoute::ALL {
        let mut client = connect(&addr).await;
        let started = Instant::now();
        client.write_all(&route.slow_frame()).await.unwrap();
        let response = tokio::time::timeout(Duration::from_secs(3), read_response(&mut client))
            .await
            .unwrap_or_else(|_| panic!("{route:?} did not honor its query timeout"));
        assert!(started.elapsed() < Duration::from_secs(3));
        match response {
            Message::Error { message } => assert_eq!(message, "query timeout after 75ms"),
            other => panic!("{route:?} expected timeout error, got {other:?}"),
        }
    }

    let rendered = metrics.render();
    assert!(
        rendered.contains("powdb_query_timeouts_total 6"),
        "{rendered}"
    );
    assert!(rendered.contains("powdb_queries_in_flight 0"), "{rendered}");
    handle.abort();
}

#[tokio::test]
async fn independent_read_only_queries_overlap_instead_of_queueing() {
    let timeout = Duration::from_millis(300);
    let (addr, handle, _metrics) =
        start_join_server(SLOW_ROWS, timeout, Duration::from_secs(2)).await;
    let mut first = connect(&addr).await;
    let mut second = connect(&addr).await;
    first
        .write_all(&QueryRoute::PowQl.slow_frame())
        .await
        .unwrap();
    second
        .write_all(&QueryRoute::PowQl.slow_frame())
        .await
        .unwrap();

    let started = Instant::now();
    let (first_result, second_result) =
        tokio::join!(read_response(&mut first), read_response(&mut second));
    let elapsed = started.elapsed();
    for result in [first_result, second_result] {
        match result {
            Message::Error { message } => assert_eq!(message, "query timeout after 300ms"),
            other => panic!("expected overlapping read timeout, got {other:?}"),
        }
    }
    assert!(
        elapsed < Duration::from_millis(500),
        "two 300ms read queries queued instead of overlapping: {elapsed:?}"
    );
    handle.abort();
}

#[tokio::test]
async fn pipelined_frames_keep_order_after_a_query_times_out() {
    let (addr, handle, _metrics) =
        start_join_server(SLOW_ROWS, Duration::from_millis(75), Duration::from_secs(5)).await;
    let mut client = connect(&addr).await;
    let mut burst = QueryRoute::PowQl.slow_frame();
    burst.extend_from_slice(
        &Message::Query {
            query: "count(Ver)".into(),
        }
        .encode(),
    );
    burst.extend_from_slice(
        &Message::Query {
            query: "count(Grp)".into(),
        }
        .encode(),
    );
    client.write_all(&burst).await.unwrap();

    match read_response(&mut client).await {
        Message::Error { message } => assert_eq!(message, "query timeout after 75ms"),
        other => panic!("expected the first pipelined response to time out, got {other:?}"),
    }
    match read_response(&mut client).await {
        Message::ResultScalar { value } => assert_eq!(value, SLOW_ROWS_TEXT),
        other => panic!("expected the second pipelined response, got {other:?}"),
    }
    match read_response(&mut client).await {
        Message::ResultScalar { value } => assert_eq!(value, SLOW_ROWS_TEXT),
        other => panic!("expected the third pipelined response, got {other:?}"),
    }
    handle.abort();
}

#[tokio::test]
async fn eof_cancels_each_query_route_before_the_long_deadline() {
    let (addr, handle, metrics) =
        start_join_server(SLOW_ROWS, Duration::from_secs(30), Duration::from_secs(5)).await;

    for route in QueryRoute::ALL {
        let mut doomed = connect(&addr).await;
        doomed.write_all(&route.slow_frame()).await.unwrap();
        wait_for_in_flight(&metrics).await;
        drop(doomed);

        assert_scalar_within(&addr, "count(Grp)", SLOW_ROWS_TEXT, Duration::from_secs(2)).await;
    }

    let rendered = metrics.render();
    assert!(
        rendered.contains("powdb_query_timeouts_total 0"),
        "disconnect must not be counted as a deadline timeout:\n{rendered}"
    );
    handle.abort();
}

#[tokio::test]
async fn explicit_disconnect_frame_cancels_the_in_flight_query() {
    let (addr, handle, metrics) =
        start_join_server(SLOW_ROWS, Duration::from_secs(30), Duration::from_secs(5)).await;
    let mut doomed = connect(&addr).await;
    doomed
        .write_all(&QueryRoute::PowQl.slow_frame())
        .await
        .unwrap();
    wait_for_in_flight(&metrics).await;
    doomed
        .write_all(&Message::Disconnect.encode())
        .await
        .unwrap();

    assert_scalar_within(&addr, "count(Ver)", SLOW_ROWS_TEXT, Duration::from_secs(2)).await;
    handle.abort();
}

#[tokio::test]
async fn in_flight_read_ahead_byte_cap_cancels_before_the_long_deadline() {
    let (addr, handle, metrics) =
        start_join_server(SLOW_ROWS, Duration::from_secs(30), Duration::from_secs(5)).await;
    let mut doomed = connect(&addr).await;
    doomed
        .write_all(&QueryRoute::PowQl.slow_frame())
        .await
        .unwrap();
    wait_for_in_flight(&metrics).await;

    // Only the next frame's header is needed. Its declared 2 MiB payload is
    // valid under the ordinary 64 MiB protocol limit but exceeds the much
    // smaller in-flight read-ahead budget, so the server must cancel without
    // buffering the payload or waiting for the 30-second query deadline.
    let mut oversized_header = [0u8; 6];
    oversized_header[0] = 0x03;
    oversized_header[2..6].copy_from_slice(&(2u32 * 1024 * 1024).to_le_bytes());
    doomed.write_all(&oversized_header).await.unwrap();

    assert_scalar_within(&addr, "count(Ver)", SLOW_ROWS_TEXT, Duration::from_secs(2)).await;
    let rendered = metrics.render();
    assert!(
        rendered.contains("powdb_query_timeouts_total 0"),
        "read-ahead cancellation must beat, not consume, the deadline:\n{rendered}"
    );
    handle.abort();
}

#[tokio::test]
async fn in_flight_read_ahead_frame_cap_cancels_before_the_long_deadline() {
    let (addr, handle, metrics) =
        start_join_server(SLOW_ROWS, Duration::from_secs(30), Duration::from_secs(5)).await;
    let mut doomed = connect(&addr).await;
    doomed
        .write_all(&QueryRoute::PowQl.slow_frame())
        .await
        .unwrap();
    wait_for_in_flight(&metrics).await;

    let mut burst = Vec::new();
    for _ in 0..128 {
        burst.extend_from_slice(&Message::Ping.encode());
    }
    doomed.write_all(&burst).await.unwrap();

    assert_scalar_within(&addr, "count(Grp)", SLOW_ROWS_TEXT, Duration::from_secs(2)).await;
    let rendered = metrics.render();
    assert!(
        rendered.contains("powdb_query_timeouts_total 0"),
        "frame-cap cancellation must beat, not consume, the deadline:\n{rendered}"
    );
    handle.abort();
}

#[tokio::test]
async fn disconnect_rolls_back_an_explicit_transaction_before_gate_release() {
    let (addr, handle, metrics) =
        start_join_server(SLOW_ROWS, Duration::from_secs(30), Duration::from_secs(5)).await;
    let mut doomed = connect(&addr).await;
    assert!(matches!(
        query(&mut doomed, "begin").await,
        Message::ResultMessage { .. }
    ));
    assert!(matches!(
        query(&mut doomed, "insert TxProbe { id := 1 }").await,
        Message::ResultOk { .. }
    ));
    doomed
        .write_all(&QueryRoute::PowQl.slow_frame())
        .await
        .unwrap();
    wait_for_in_flight(&metrics).await;
    drop(doomed);

    assert_scalar_within(&addr, "count(TxProbe)", "0", Duration::from_secs(2)).await;
    handle.abort();
}

#[tokio::test]
async fn cancelled_begin_closes_connection_and_frees_the_gate() {
    // A zero query timeout makes every statement trip the statement-boundary
    // cancellation checkpoint the instant it enters the executor (the deadline
    // is already in the past), before a begin can open a transaction. This
    // drives the begin arm's cancellation path deterministically, with no
    // scheduler race.
    let (addr, handle, _metrics) =
        start_join_server(0, Duration::ZERO, Duration::from_secs(2)).await;
    let mut client = connect(&addr).await;

    // The begin is cancelled at the statement boundary and returns a typed
    // timeout error rather than opening a transaction.
    match query(&mut client, "begin").await {
        Message::Error { message } => assert_eq!(message, "query timeout after 0ms"),
        other => panic!("expected the cancelled begin to time out, got {other:?}"),
    }

    // Parity fix: a cancelled begin closes the connection (like the
    // commit/rollback and in-transaction arms) instead of leaving it open in
    // an ambiguous state, so the next frame reads EOF. Before the fix the
    // begin arm only checked `is_success_response`, so a cancelled begin left
    // the connection open and this second frame would draw another response.
    let _ = client
        .write_all(
            &Message::Query {
                query: "begin".into(),
            }
            .encode(),
        )
        .await;
    let mut header = [0u8; 6];
    match tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut header)).await {
        Ok(Err(_)) => {}
        Ok(Ok(_)) => panic!("server kept the connection open after a cancelled begin"),
        Err(_) => panic!("connection neither closed nor answered after a cancelled begin"),
    }

    // The transaction gate was fully released, not wedged by the cancelled
    // begin: a fresh connection is admitted and reaches the same statement
    // boundary. A leaked permit would instead surface as a transaction gate
    // timeout after tx_wait_timeout.
    let mut fresh = connect(&addr).await;
    match query(&mut fresh, "begin").await {
        Message::Error { message } => assert_eq!(
            message, "query timeout after 0ms",
            "a wedged gate would surface as a transaction gate timeout"
        ),
        other => panic!("expected the fresh begin to reach the executor, got {other:?}"),
    }

    handle.abort();
}

#[tokio::test]
async fn timeout_rolls_back_an_explicit_transaction_before_gate_release() {
    let (addr, handle, _metrics) =
        start_join_server(SLOW_ROWS, Duration::from_millis(75), Duration::from_secs(5)).await;
    let mut timed_out = connect(&addr).await;
    assert!(matches!(
        query(&mut timed_out, "begin").await,
        Message::ResultMessage { .. }
    ));
    assert!(matches!(
        query(&mut timed_out, "insert TxProbe { id := 1 }").await,
        Message::ResultOk { .. }
    ));
    timed_out
        .write_all(&QueryRoute::PowQl.slow_frame())
        .await
        .unwrap();
    match read_response(&mut timed_out).await {
        Message::Error { message } => assert_eq!(message, "query timeout after 75ms"),
        other => panic!("expected timeout error, got {other:?}"),
    }

    assert_scalar_within(&addr, "count(TxProbe)", "0", Duration::from_secs(2)).await;
    handle.abort();
}
