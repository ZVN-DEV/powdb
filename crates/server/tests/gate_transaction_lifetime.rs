//! End-to-end proof that an explicit transaction cannot hold the write
//! admission gate forever.
//!
//! An idle `begin` takes the entire gate for its whole lifetime, and that part
//! is deliberate and documented (docs/POWQL.md: "every other connection that
//! needs the gate, readers included, waits"). What was NOT bounded is how long
//! "its whole lifetime" may be. The only wall-clock budget that touched an
//! open transaction was the CONNECTION idle timeout, which is re-armed by
//! every frame the client sends, and a bare six-byte `PING` is a frame. So one
//! unauthenticated peer on a default server could `begin`, ping once per idle
//! period, and take every other connection offline for as long as it liked.
//!
//! These tests drive real sockets through `handle_connection`, because that is
//! the only place the re-arm and the reaper meet.

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use powdb_query::executor::Engine;
use powdb_server::handler::{
    handle_connection, new_tx_gate_with_max_tx_lifetime, ConnOpts, DEFAULT_TX_MAX_LIFETIME,
};
use powdb_server::metrics::Metrics;
use powdb_server::protocol::{decode_error_class, ErrorClass, Message};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// Well below the idle timeout every server below runs with, so a failure to
/// reap shows up as an idle-timeout message (or a hang), never as a pass.
const MAX_TX_LIFETIME: Duration = Duration::from_millis(600);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

fn seeded_engine() -> (tempfile::TempDir, Arc<RwLock<Engine>>) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine.execute_powql("type T { required id: int }").unwrap();
    engine.execute_powql("insert T { id := 1 }").unwrap();
    (dir, Arc::new(RwLock::new(engine)))
}

/// Seed a table whose full scan encodes to far more bytes than any socket
/// buffer can hold, so a client that stops reading really does block the
/// server's write.
fn wide_engine(rows: usize, width: usize) -> (tempfile::TempDir, Arc<RwLock<Engine>>) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine.execute_powql("type T { required id: int }").unwrap();
    engine.execute_powql("insert T { id := 1 }").unwrap();
    engine
        .execute_powql("type Wide { required id: int, blob: str }")
        .unwrap();
    let payload = "x".repeat(width);
    engine.execute_powql("begin").unwrap();
    for id in 0..rows {
        engine
            .execute_powql(&format!(
                "insert Wide {{ id := {id}, blob := \"{payload}\" }}"
            ))
            .unwrap();
    }
    engine.execute_powql("commit").unwrap();
    (dir, Arc::new(RwLock::new(engine)))
}

async fn start_server(
    engine: Arc<RwLock<Engine>>,
    max_tx_lifetime: Option<Duration>,
) -> (SocketAddr, Arc<Metrics>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tx_gate = new_tx_gate_with_max_tx_lifetime(max_tx_lifetime);
    let metrics = Arc::new(Metrics::new());
    let observed_metrics = Arc::clone(&metrics);

    // The shutdown sender must outlive every connection. Dropping it makes
    // `shutdown_rx.changed()` resolve immediately and turns the connection
    // loop into a busy spin, which would keep the reap check running far more
    // often than a real server does and hide a missing wake-up.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    tokio::spawn(async move {
        let _shutdown_tx = shutdown_tx;
        loop {
            let (stream, peer) = listener.accept().await.unwrap();
            let engine = engine.clone();
            let tx_gate = tx_gate.clone();
            let metrics = metrics.clone();
            let mut shutdown_rx = shutdown_rx.clone();
            tokio::spawn(async move {
                handle_connection(
                    stream,
                    ConnOpts {
                        engine,
                        tx_gate,
                        expected_password: None,
                        users: Arc::new(powdb_auth::UserStore::new()),
                        shutdown_rx: &mut shutdown_rx,
                        idle_timeout: IDLE_TIMEOUT,
                        query_timeout: Duration::from_secs(30),
                        rate_limiter: None,
                        peer_addr: Some(peer),
                        metrics,
                        tx_wait_timeout: Duration::from_millis(300),
                        db_name: None,
                    },
                )
                .await;
            });
        }
    });

    (addr, observed_metrics)
}

async fn connect(addr: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    Message::Connect {
        db_name: "default".into(),
        password: None,
        username: None,
    }
    .write_to(&mut stream)
    .await
    .unwrap();
    match Message::read_from(&mut stream).await.unwrap().unwrap() {
        Message::ConnectOk { .. } | Message::ConnectOkWithHello { .. } => stream,
        other => panic!("expected CONNECT_OK, got {other:?}"),
    }
}

async fn request(stream: &mut TcpStream, msg: Message) -> Message {
    msg.write_to(stream).await.unwrap();
    Message::read_from(stream)
        .await
        .unwrap()
        .expect("server closed without answering")
}

async fn query(stream: &mut TcpStream, q: &str) -> Message {
    request(
        stream,
        Message::Query {
            query: q.to_string(),
        },
    )
    .await
}

fn error_message(msg: &Message) -> &str {
    match msg {
        Message::Error { message } | Message::ErrorWithClass { message, .. } => message,
        other => panic!("expected an error frame, got {other:?}"),
    }
}

/// Read one whole frame as bytes.
///
/// `Message::read_from` decodes `MSG_ERROR` into `Message::Error` and drops the
/// trailing class byte by design (that is what keeps old clients working), so
/// the only way to assert the class a client actually receives is to look at
/// the frame.
async fn read_raw_frame<R: tokio::io::AsyncRead + Unpin>(stream: &mut R) -> Vec<u8> {
    read_raw_frame_or_eof(stream)
        .await
        .expect("server closed without answering")
}

/// Like [`read_raw_frame`], but a connection close is `None` instead of a
/// panic.
///
/// Two closes qualify. A clean FIN at a frame boundary: the closing flush is
/// time-bounded and after a failed write the reap stays silent, so tests that
/// race a reap against a saturated socket have to accept it. And an RST
/// (`ConnectionReset`), anywhere: when the server closes with unread pings
/// still in its receive buffer, the kernel answers with a reset, and a reset
/// discards data in flight, so bytes missing on OUR side say nothing about
/// what the server wrote. The one outcome that stays a hard failure is a FIN
/// inside a frame: the server really did write a partial frame, which the
/// write-side reap contract forbids.
async fn read_raw_frame_or_eof<R: tokio::io::AsyncRead + Unpin>(stream: &mut R) -> Option<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut header = [0u8; 6];
    let mut filled = 0usize;
    while filled < header.len() {
        match stream.read(&mut header[filled..]).await {
            Ok(0) => {
                assert_eq!(
                    filled, 0,
                    "connection closed inside a frame header: a torn frame, not a clean close"
                );
                return None;
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return None,
            Err(e) => panic!("frame header read failed: {e:?}"),
        }
    }
    let payload_len = u32::from_le_bytes(header[2..6].try_into().unwrap()) as usize;
    let mut frame = header.to_vec();
    frame.resize(6 + payload_len, 0);
    if payload_len > 0 {
        match stream.read_exact(&mut frame[6..]).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return None,
            Err(e) => panic!("connection closed inside a frame payload: {e:?}"),
        }
    }
    Some(frame)
}

/// A `PING` loop must not extend an open transaction's hold on the gate.
///
/// This is the whole outage in one test: the pings keep the connection's idle
/// deadline permanently fresh, so before the lifetime bound existed the
/// transaction survived every one of them and the reader below waited until
/// the transaction connection chose to go away.
#[tokio::test]
async fn a_ping_loop_cannot_extend_an_open_transaction_past_its_lifetime() {
    let (_dir, engine) = seeded_engine();
    let (addr, metrics) = start_server(engine, Some(MAX_TX_LIFETIME)).await;

    let mut holder = connect(addr).await;
    assert!(matches!(
        query(&mut holder, "begin").await,
        Message::ResultMessage { .. }
    ));

    let started = Instant::now();
    let mut pings = 0u32;
    let reaped = loop {
        assert!(
            started.elapsed() < IDLE_TIMEOUT,
            "the transaction outlived its lifetime bound through {pings} pings"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        let response = request(&mut holder, Message::Ping).await;
        match response {
            Message::Pong => pings += 1,
            other => break other,
        }
    };

    assert!(
        pings >= 3,
        "the pings must actually have re-armed the idle deadline, got {pings}"
    );
    let message = error_message(&reaped);
    assert!(
        message.contains("maximum lifetime") && message.contains("rolled back"),
        "a reaped client must be told what happened: {message}"
    );
    assert!(
        message.contains("POWDB_TX_MAX_LIFETIME_MS"),
        "a reaped client must be told which budget to raise: {message}"
    );

    // A reap is an operator-visible event, not a silent rollback: it has its
    // own counter, separate from the gate-wait counter (this connection never
    // waited on the gate, it held it).
    let rendered = metrics.render();
    assert!(
        rendered.contains("powdb_tx_reaped_total 1"),
        "the reap left no trace on /metrics:\n{rendered}"
    );

    // The gate is genuinely free again, on a connection that never saw the
    // transaction: this is the property the outage denied.
    let mut reader = connect(addr).await;
    let response = tokio::time::timeout(Duration::from_secs(5), query(&mut reader, "count(T)"))
        .await
        .expect("the reaped transaction must have released the gate");
    assert!(
        matches!(
            response,
            Message::ResultScalar { .. } | Message::ResultScalarNative { .. }
        ),
        "expected a scalar count after the reap, got {response:?}"
    );
}

/// A client that never lets the server's read buffer run dry must not be able
/// to postpone the reap.
///
/// The shortened read wait cannot carry this on its own: a timeout only fires
/// when there is nothing to read, and a saturating flood means there always
/// is. The reap therefore has to be checked before each frame is served, not
/// only when the socket goes quiet, which is the difference between "bounded"
/// and "bounded unless the attacker keeps typing".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_saturating_ping_flood_cannot_postpone_the_reap() {
    use tokio::io::AsyncWriteExt;

    let (_dir, engine) = seeded_engine();
    let (addr, _metrics) = start_server(engine, Some(MAX_TX_LIFETIME)).await;

    let mut holder = connect(addr).await;
    assert!(matches!(
        query(&mut holder, "begin").await,
        Message::ResultMessage { .. }
    ));

    let (mut rx, mut tx) = holder.into_split();
    let flood_for = MAX_TX_LIFETIME * 8;
    let feeder = tokio::spawn(async move {
        let ping = Message::Ping.encode();
        let until = Instant::now() + flood_for;
        while Instant::now() < until {
            if tx.write_all(&ping).await.is_err() {
                break;
            }
        }
    });

    let started = Instant::now();
    let mut pongs = 0u64;
    loop {
        let frame = tokio::time::timeout(flood_for, read_raw_frame_or_eof(&mut rx))
            .await
            .expect("a saturating client must still be reaped");
        let Some(frame) = frame else {
            // The reap can close the connection without its notice reaching
            // us: the closing flush is time-bounded and a socket saturated
            // by this very flood can hold it past the bound. The close is
            // the reap (docs/errors.md tells clients to treat it as the
            // timeout), so it proves the point as well as the notice does.
            assert!(
                started.elapsed() < MAX_TX_LIFETIME * 4,
                "the flood postponed the reap by {:?}",
                started.elapsed()
            );
            break;
        };
        match Message::decode(&frame).expect("decodable frame") {
            Message::Pong => pongs += 1,
            other => {
                assert!(
                    error_message(&other).contains("maximum lifetime"),
                    "expected the reap, got {other:?}"
                );
                assert!(
                    started.elapsed() < MAX_TX_LIFETIME * 4,
                    "the flood postponed the reap by {:?}",
                    started.elapsed()
                );
                break;
            }
        }
    }
    // The reader consumes pongs for the whole lifetime of the transaction
    // (an RST can discard a tail of them, but not the ~600ms of traffic that
    // came before it), so zero here means the flood was never actually
    // served, with or without a reap notice.
    assert!(pongs > 0, "the flood must actually have been served");
    let _ = feeder.await;
}

/// The rollback is real: the reaped transaction's uncommitted write is gone,
/// not left half-applied for the next connection to read.
#[tokio::test]
async fn a_reaped_transaction_is_rolled_back_not_abandoned() {
    let (_dir, engine) = seeded_engine();
    let (addr, _metrics) = start_server(engine, Some(MAX_TX_LIFETIME)).await;

    let mut holder = connect(addr).await;
    assert!(matches!(
        query(&mut holder, "begin").await,
        Message::ResultMessage { .. }
    ));
    assert!(matches!(
        query(&mut holder, "insert T { id := 2 }").await,
        Message::ResultOk { .. }
    ));

    tokio::time::sleep(MAX_TX_LIFETIME + Duration::from_millis(400)).await;

    let mut reader = connect(addr).await;
    let response = tokio::time::timeout(Duration::from_secs(5), query(&mut reader, "count(T)"))
        .await
        .expect("the reaped transaction must have released the gate");
    let count = match response {
        Message::ResultScalar { value } => value,
        other => panic!("expected a scalar count, got {other:?}"),
    };
    assert_eq!(
        count, "1",
        "the uncommitted insert must have been rolled back"
    );
}

/// A silent holder is reaped on the transaction budget, not on the (much
/// longer) idle one: the bound is independent of the idle deadline, not a
/// rename of it.
#[tokio::test]
async fn a_silent_transaction_is_reaped_on_its_own_budget_not_the_idle_timeout() {
    let (_dir, engine) = seeded_engine();
    let (addr, _metrics) = start_server(engine, Some(MAX_TX_LIFETIME)).await;

    let mut holder = connect(addr).await;
    assert!(matches!(
        query(&mut holder, "begin").await,
        Message::ResultMessage { .. }
    ));

    let started = Instant::now();
    let frame = tokio::time::timeout(IDLE_TIMEOUT / 2, read_raw_frame(&mut holder))
        .await
        .expect("a silent open transaction must be reaped");
    assert!(
        started.elapsed() < MAX_TX_LIFETIME * 4,
        "reaped on the idle timeout ({IDLE_TIMEOUT:?}), not on the transaction lifetime: took {:?}",
        started.elapsed()
    );
    let reaped = Message::decode(&frame).expect("decodable error frame");
    assert!(
        error_message(&reaped).contains("maximum lifetime"),
        "expected the lifetime message, got {reaped:?}"
    );
    // The class the client actually receives, read off the wire: a reaped
    // transaction is a time budget, never an unclassified failure.
    assert_eq!(
        decode_error_class(&frame),
        Some(ErrorClass::Timeout.as_u8()),
        "the reap error reached the wire without a real class"
    );
}

/// Committing well inside the budget must not be disturbed, and the next
/// transaction on the same connection starts a fresh budget rather than
/// inheriting the first one's deadline.
#[tokio::test]
async fn transactions_inside_the_budget_are_untouched_and_re_arm_per_transaction() {
    let (_dir, engine) = seeded_engine();
    let (addr, _metrics) = start_server(engine, Some(MAX_TX_LIFETIME)).await;

    let mut client = connect(addr).await;
    for id in 2..=4 {
        assert!(matches!(
            query(&mut client, "begin").await,
            Message::ResultMessage { .. }
        ));
        assert!(
            matches!(
                query(&mut client, &format!("insert T {{ id := {id} }}")).await,
                Message::ResultOk { .. }
            ),
            "insert {id} inside the budget must succeed"
        );
        // Long enough that a deadline carried over from the first transaction
        // would have expired by the third.
        tokio::time::sleep(MAX_TX_LIFETIME / 2).await;
        assert!(
            matches!(
                query(&mut client, "commit").await,
                Message::ResultMessage { .. }
            ),
            "commit {id} inside the budget must succeed"
        );
    }

    let response = query(&mut client, "count(T)").await;
    match response {
        Message::ResultScalar { value } => assert_eq!(value, "4"),
        other => panic!("expected a scalar count, got {other:?}"),
    }
}

/// The bound is an opt-out, and turning it off restores the old behavior
/// exactly: this is what `POWDB_TX_MAX_LIFETIME_MS=0` buys, and what it costs.
#[tokio::test]
async fn the_bound_can_be_disabled_and_then_nothing_reaps() {
    let (_dir, engine) = seeded_engine();
    let (addr, _metrics) = start_server(engine, None).await;

    let mut holder = connect(addr).await;
    assert!(matches!(
        query(&mut holder, "begin").await,
        Message::ResultMessage { .. }
    ));
    tokio::time::sleep(MAX_TX_LIFETIME * 3).await;
    assert!(
        matches!(request(&mut holder, Message::Ping).await, Message::Pong),
        "with the bound disabled the transaction must survive"
    );
    assert!(matches!(
        query(&mut holder, "rollback").await,
        Message::ResultMessage { .. }
    ));
}

/// A client that stops READING must not be able to hold the gate either.
///
/// The reap only runs between frames, so bounding the read side is only half
/// the budget. A client that opens a transaction, asks for a reply larger than
/// the socket buffers, and then stops reading parks the handler inside the
/// response write, where the only budget was the hard-coded 30s `WRITE_TIMEOUT`
/// (`handler.rs`), with the transaction still open and the gate still held. The
/// server therefore advertised a 300000ms guarantee it did not keep, and the
/// reap never fired and never logged.
///
/// The reader below has to get through in a small multiple of the transaction
/// budget. The old behavior misses that by an order of magnitude, so this test
/// separates them without depending on precise timing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_stops_reading_cannot_hold_the_gate_for_the_write_timeout() {
    /// Longer than MAX_TX_LIFETIME so the (debug-build) scan and encode of the
    /// reply below comfortably finish inside the budget, and still far below
    /// the 30s write timeout this test exists to distinguish it from.
    const STALLED_WRITE_LIFETIME: Duration = Duration::from_millis(2000);
    /// Well past any socket buffer on any platform CI runs on.
    const REPLY_ROWS: usize = 4000;
    const REPLY_WIDTH: usize = 2000;

    let (_dir, engine) = wide_engine(REPLY_ROWS, REPLY_WIDTH);
    let (addr, metrics) = start_server(engine, Some(STALLED_WRITE_LIFETIME)).await;

    let mut holder = connect(addr).await;
    assert!(matches!(
        query(&mut holder, "begin").await,
        Message::ResultMessage { .. }
    ));

    // Ask for roughly 8 MB and then never read a byte of it.
    Message::Query {
        query: "Wide".to_string(),
    }
    .write_to(&mut holder)
    .await
    .unwrap();

    let started = Instant::now();
    let mut reader = connect(addr).await;
    // The server's tx_wait_timeout is 300ms, so the first attempts are refused
    // with a gate timeout while the holder still has the gate. Retry until the
    // gate is actually free, bounded well below the 30s write timeout.
    let deadline = started + Duration::from_secs(12);
    let count = loop {
        assert!(
            Instant::now() < deadline,
            "the stalled reader held the transaction gate past its {}ms budget; \
             it is being bounded by the 30s response-write timeout instead",
            STALLED_WRITE_LIFETIME.as_millis()
        );
        match query(&mut reader, "count(T)").await {
            Message::ResultScalar { value } => break value,
            Message::ResultScalarNative { .. } => break "1".to_string(),
            other => {
                let message = error_message(&other);
                assert!(
                    message.contains("transaction gate timeout"),
                    "unexpected answer while the holder still had the gate: {message}"
                );
            }
        }
    };
    assert_eq!(count, "1");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(12),
        "the gate came back only after {elapsed:?}"
    );

    // And the reap was a real reap: rolled back, logged, and counted, not a
    // write failure that quietly dropped the connection.
    let rendered = metrics.render();
    assert!(
        rendered.contains("powdb_tx_reaped_total 1"),
        "a transaction reaped through the write side left no trace on /metrics:\n{rendered}"
    );

    drop(holder);
}

/// What the client got: the frames that arrived whole, and the frame the
/// stream stopped inside, if it stopped inside one.
struct WireStream<'a> {
    whole: Vec<Vec<u8>>,
    torn: Option<TornFrame<'a>>,
}

/// A frame the connection stopped in the middle of: what its header promised,
/// and the bytes that actually arrived for it.
struct TornFrame<'a> {
    declared: usize,
    received: &'a [u8],
}

/// Walk a received byte stream the way a client does: length-prefixed frames,
/// one after another.
fn walk_frames(bytes: &[u8]) -> WireStream<'_> {
    let mut whole = Vec::new();
    let mut idx = 0usize;
    while idx < bytes.len() {
        // A tail too short to hold a header is a frame that never got its
        // length across: torn with nothing declared.
        let declared = if bytes.len() - idx < 6 {
            0
        } else {
            u32::from_le_bytes(bytes[idx + 2..idx + 6].try_into().unwrap()) as usize
        };
        if bytes.len() - idx < 6 + declared {
            return WireStream {
                whole,
                torn: Some(TornFrame {
                    declared,
                    received: &bytes[idx..],
                }),
            };
        }
        whole.push(bytes[idx..idx + 6 + declared].to_vec());
        idx += 6 + declared;
    }
    WireStream { whole, torn: None }
}

/// A reap that fires because a reply write could not finish must not write
/// anything else on that connection.
///
/// `write_msg_with_budget` wraps `write_all` in a timeout, and a frame larger
/// than the `BufWriter` buffer goes straight to the socket, so cancelling that
/// write leaves a partial frame on the wire with no way to resume it. The reap
/// then wrote its typed `Timeout` error next: those bytes land INSIDE the
/// unfinished frame's declared payload, where a client counting that length
/// down reads them as payload. The notification therefore never arrives, and
/// the client's framing is corrupted on the way to a connection reset it would
/// have seen anyway.
///
/// The client here drains slowly instead of stopping dead: that is the shape
/// that makes the bug visible, because the socket always has room for the
/// error frame the server must not send.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_side_reap_never_writes_after_the_frame_it_could_not_finish() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Long enough for a debug build to scan and encode the reply inside the
    /// budget, short enough to keep the test quick.
    const STALLED_WRITE_LIFETIME: Duration = Duration::from_millis(2000);
    const REPLY_ROWS: usize = 4000;
    const REPLY_WIDTH: usize = 2000;
    /// ~1.6 MB/s: far too slow to take the ~8 MB reply inside the budget, and
    /// far too fast to ever leave the socket without room for a 200-byte error
    /// frame.
    const DRAIN_CHUNK: usize = 8 * 1024;
    const DRAIN_GAP: Duration = Duration::from_millis(5);

    let (_dir, engine) = wide_engine(REPLY_ROWS, REPLY_WIDTH);
    let (addr, metrics) = start_server(engine, Some(STALLED_WRITE_LIFETIME)).await;

    let holder = connect(addr).await;
    let (mut rx, mut tx) = holder.into_split();
    tx.write_all(
        &Message::Query {
            query: "begin".to_string(),
        }
        .encode(),
    )
    .await
    .unwrap();
    let begin = read_raw_frame(&mut rx).await;
    assert!(
        matches!(
            Message::decode(&begin).expect("decodable begin reply"),
            Message::ResultMessage { .. }
        ),
        "begin must open the transaction"
    );

    // Ask for roughly 8 MB and then read it far slower than the server can
    // write it.
    tx.write_all(
        &Message::Query {
            query: "Wide".to_string(),
        }
        .encode(),
    )
    .await
    .unwrap();

    let drain = tokio::spawn(async move {
        let mut got = Vec::new();
        let mut chunk = vec![0u8; DRAIN_CHUNK];
        loop {
            match rx.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
            tokio::time::sleep(DRAIN_GAP).await;
        }
        got
    });

    let got = tokio::time::timeout(Duration::from_secs(30), drain)
        .await
        .expect("the reap must close the connection, not leave it open")
        .unwrap();

    // The reap really happened on the write side: without this the assertions
    // below could pass on a connection that was never reaped at all.
    let rendered = metrics.render();
    assert!(
        rendered.contains("powdb_tx_reaped_total 1"),
        "the write-side reap left no trace on /metrics:\n{rendered}"
    );

    let stream = walk_frames(&got);
    for frame in &stream.whole {
        Message::decode(frame).unwrap_or_else(|err| {
            panic!("the client received a frame it cannot decode: {err}");
        });
    }
    let torn = stream.torn.expect(
        "the reply must have been cut short; if it arrived whole the drain was too fast and \
         this test proves nothing",
    );
    assert!(
        torn.declared > torn.received.len(),
        "the unfinished frame must really be unfinished"
    );

    // The contract, stated as bytes: nothing follows the frame the server
    // could not finish. The reap's notification is the only thing that ever
    // tried to, and its text is what proves it did.
    let needle = b"maximum lifetime";
    let leaked = torn
        .received
        .windows(needle.len())
        .any(|window| window == needle);
    assert!(
        !leaked,
        "the reap wrote its notification after a frame it could not finish: those {} bytes \
         land inside the unfinished frame's declared {}-byte payload, so the client reads them \
         as payload and the notification is lost either way",
        torn.received.len(),
        torn.declared
    );

    drop(tx);
}

/// The default is a real bound, not `None` dressed up as one.
#[test]
fn the_default_lifetime_is_finite() {
    assert!(DEFAULT_TX_MAX_LIFETIME > Duration::ZERO);
    assert!(DEFAULT_TX_MAX_LIFETIME <= Duration::from_secs(300));
}
