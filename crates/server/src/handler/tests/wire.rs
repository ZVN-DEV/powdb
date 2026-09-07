//! Frame reading, bounded reply writes, and response encoding.

use super::*;

// ---- Wire NULL rendering (Fix: remote protocol rendered NULL as `{}`) ----

#[test]
fn null_serializes_as_null_bareword_on_wire() {
    assert_eq!(value_to_display(&Value::Empty), "null");
}

// ---- Wire frame header reads must never panic (panic = "abort") ----

/// A connection sits with fewer than six buffered bytes every time it
/// waits for the next frame, and a peer can pin it there by sending a
/// partial header and stopping. The read loop must treat that as "no
/// header yet" rather than reading the four-byte length field out of it.
/// The release profile aborts on panic, so one byte past the end of this
/// buffer is not a failed request: it disconnects every other client on
/// the server and forces a WAL replay on restart.
#[tokio::test]
async fn a_partial_frame_header_never_panics_the_read_loop() {
    for prefix in 0..6usize {
        let mut buffered = vec![0xFFu8; prefix];
        let empty: &[u8] = &[];
        let mut reader = BufReader::new(empty);
        let got =
            read_message_cancel_safe(&mut reader, &mut buffered, MAX_IN_FLIGHT_READ_AHEAD_BYTES)
                .await;
        match got {
            Ok(None) => {}
            Ok(Some(_)) => panic!(
                "the read loop decoded a whole frame out of a {prefix}-byte \
                 partial header"
            ),
            Err(error) => panic!(
                "a {prefix}-byte partial header followed by EOF must close the \
                 connection cleanly, got error: {error}"
            ),
        }
    }
}

// ---- The write side of the transaction budget ----

/// A socket that accepts a fixed amount of data and then never accepts
/// another byte, recording everything the server offers it afterwards.
///
/// `seal()` marks the boundary between the write that failed and whatever
/// the server does next: bytes offered after the seal are bytes written
/// AFTER a write had already failed, which is the thing that tears a frame
/// in half on a real socket.
#[derive(Clone)]
struct StalledSocket(Arc<Mutex<StalledSocketState>>);

struct StalledSocketState {
    room: usize,
    accepted: usize,
    sealed: bool,
    offered_after_seal: usize,
}

impl StalledSocket {
    fn with_room(room: usize) -> Self {
        Self(Arc::new(Mutex::new(StalledSocketState {
            room,
            accepted: 0,
            sealed: false,
            offered_after_seal: 0,
        })))
    }

    fn accepted(&self) -> usize {
        self.0.lock().unwrap().accepted
    }

    fn seal(&self) {
        self.0.lock().unwrap().sealed = true;
    }

    fn offered_after_seal(&self) -> usize {
        self.0.lock().unwrap().offered_after_seal
    }

    /// The client started reading again.
    fn drain(&self, more: usize) {
        let mut state = self.0.lock().unwrap();
        state.room = state.room.saturating_add(more);
    }
}

impl AsyncWrite for StalledSocket {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let mut state = self.0.lock().unwrap();
        if state.sealed {
            state.offered_after_seal += buf.len();
        }
        let room = state.room.saturating_sub(state.accepted);
        if room == 0 {
            // Deliberately no waker: the write budget is the only thing
            // that ends this write, which is the stall being modelled.
            return std::task::Poll::Pending;
        }
        let taken = buf.len().min(room);
        state.accepted += taken;
        std::task::Poll::Ready(Ok(taken))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// A reply larger than any buffer, so the write goes straight to the
/// socket and a cancelled write leaves the frame half delivered.
fn unwritable_reply() -> Message {
    error_response("x".repeat(4 << 20), ErrorClass::Internal)
}

/// The reap that fires because a reply write failed must write NOTHING
/// more on that connection.
///
/// The frame it could not finish is already partly on the wire with its
/// declared length announced, and there is no resume: a client counting
/// that length down reads whatever comes next as the dead frame's payload.
/// So the notification cannot arrive, and the bytes that carry it corrupt
/// the client's framing on the way to a reset it would have seen anyway.
/// Rolling back, releasing the gate, logging, and counting still happen;
/// only the wire goes quiet.
#[tokio::test]
async fn a_reap_after_a_stalled_write_writes_nothing_more() {
    let (_dir, engine) = one_row_engine();
    let metrics = Arc::new(Metrics::new());
    let socket = StalledSocket::with_room(64 * 1024);
    let mut writer = BufWriter::new(socket.clone());

    let reply = unwritable_reply();
    assert!(
        !write_msg_with_budget(&mut writer, &reply, Duration::from_millis(50)).await,
        "a socket that stops accepting must fail the write"
    );
    assert_eq!(
        socket.accepted(),
        64 * 1024,
        "the frame must be torn on the wire, which is what makes the reap unable to speak"
    );
    socket.seal();

    let principal: Option<Principal> = None;
    let mut tx_permit = None;
    let mut tx_deadline = Some(Instant::now() - Duration::from_millis(1));
    reap_after_stalled_write(
        ReapContext {
            engine: &engine,
            principal: &principal,
            tx_permit: &mut tx_permit,
            tx_deadline: &mut tx_deadline,
            writer: &mut writer,
            peer: "peer",
            metrics: &metrics,
        },
        Some(Duration::from_millis(600)),
    )
    .await;

    assert_eq!(
        socket.offered_after_seal(),
        0,
        "the reap wrote {} bytes after a frame it could not finish; they land inside that \
         frame's declared payload",
        socket.offered_after_seal()
    );
    assert!(
        tx_deadline.is_none(),
        "the transaction must still be reaped, not merely left unwritten to"
    );
    let rendered = metrics.render();
    assert!(
        rendered.contains("powdb_tx_reaped_total 1"),
        "a silent reap is still an operator-visible reap:\n{rendered}"
    );
}

/// The other half of the same rule: on a connection whose last write
/// COMPLETED, the next byte starts a frame, so the reap still says why.
/// Suppressing the notification everywhere would trade one lie for another.
#[tokio::test]
async fn a_reap_on_a_quiet_connection_still_tells_the_client_why() {
    let (_dir, engine) = one_row_engine();
    let metrics = Arc::new(Metrics::new());
    let mut writer = BufWriter::new(Vec::new());

    let principal: Option<Principal> = None;
    let mut tx_permit = None;
    let mut tx_deadline = Some(Instant::now());
    reap_expired_transaction(
        ReapContext {
            engine: &engine,
            principal: &principal,
            tx_permit: &mut tx_permit,
            tx_deadline: &mut tx_deadline,
            writer: &mut writer,
            peer: "peer",
            metrics: &metrics,
        },
        Duration::from_millis(600),
        ReapNotice::Speak(WRITE_TIMEOUT),
    )
    .await;

    let frame = writer.into_inner();
    let decoded = Message::decode(&frame).expect("the reap notification must be a whole frame");
    match &decoded {
        Message::Error { message } | Message::ErrorWithClass { message, .. } => assert!(
            message.contains("maximum lifetime") && message.contains("POWDB_TX_MAX_LIFETIME_MS"),
            "the reaped client must be told what happened and which budget to raise: {message}"
        ),
        other => panic!("expected a typed error frame, got {other:?}"),
    }
    assert_eq!(
        crate::protocol::decode_error_class(&frame),
        Some(ErrorClass::Timeout.as_u8()),
        "a reap is a time budget, never an unclassified failure"
    );
}

/// The budget a reply is written under, at both ends of its range.
#[test]
fn the_write_budget_is_the_smaller_of_the_write_timeout_and_the_remaining_lifetime() {
    assert_eq!(
        write_budget(None),
        WRITE_TIMEOUT,
        "with no transaction there is no lifetime to cap the write"
    );
    assert_eq!(
        write_budget(Some(Instant::now() + WRITE_TIMEOUT * 2)),
        WRITE_TIMEOUT,
        "a distant deadline must not RAISE the write timeout"
    );

    let budget = write_budget(Some(Instant::now() + Duration::from_millis(500)));
    assert!(
        budget <= Duration::from_millis(500) && budget > Duration::from_millis(400),
        "a transaction with 500ms left may write for at most 500ms, got {budget:?}"
    );

    // The edge the reap depends on: a deadline that has already passed is
    // ZERO, never a wrapped-around eternity.
    assert_eq!(
        write_budget(Some(Instant::now() - Duration::from_secs(1))),
        Duration::ZERO,
        "an expired transaction must not be handed a budget at all"
    );
}

/// `Duration::ZERO` means "no WAITING", not "no write": a reply the socket
/// can take immediately still goes out, and one that would block fails at
/// once instead of parking the handler with the gate still held.
#[tokio::test]
async fn a_zero_write_budget_delivers_what_never_blocks_and_nothing_else() {
    let reply = error_response("small", ErrorClass::Internal);
    let mut ready = BufWriter::new(Vec::new());
    assert!(
        write_msg_with_budget(&mut ready, &reply, Duration::ZERO).await,
        "a write that never has to wait must still be delivered on a spent budget"
    );
    assert_eq!(ready.into_inner(), reply.encode());

    let socket = StalledSocket::with_room(0);
    let mut blocked = BufWriter::new(socket.clone());
    assert!(
        !write_msg_with_budget(&mut blocked, &unwritable_reply(), Duration::ZERO).await,
        "a write that would have to wait must fail immediately on a spent budget"
    );
    assert_eq!(socket.accepted(), 0);
}

/// A frame the connection already handed to the writer must not be thrown
/// away because the connection is ending: `BufWriter` has no `Drop` that
/// flushes, so without a closing flush a complete reply left in the buffer
/// is silently discarded. The flush is bounded, because a client that
/// stopped reading must not be able to park the teardown either.
#[tokio::test]
async fn the_closing_flush_delivers_buffered_frames_without_parking_teardown() {
    let reply = error_response("buffered", ErrorClass::Internal);
    let socket = StalledSocket::with_room(0);
    let mut writer = BufWriter::new(socket.clone());

    // Small enough to sit entirely in the BufWriter: the socket never sees
    // it, so nothing is torn and the frame is still whole and deliverable.
    assert!(!write_msg_with_budget(&mut writer, &reply, Duration::from_millis(50)).await);
    assert_eq!(
        socket.accepted(),
        0,
        "the frame is buffered, not on the wire"
    );

    let started = Instant::now();
    assert!(
        !flush_before_close(&mut writer).await,
        "a client that is still not reading cannot be flushed to"
    );
    assert!(
        started.elapsed() < FINAL_FLUSH_BUDGET * 4,
        "the closing flush parked teardown for {:?}",
        started.elapsed()
    );

    socket.drain(usize::MAX / 2);
    assert!(
        flush_before_close(&mut writer).await,
        "once the client reads again the buffered frame must go out"
    );
    assert_eq!(
        socket.accepted(),
        reply.encode().len(),
        "the whole frame, exactly once"
    );
}

// ---- The in-flight read-ahead byte budget ----

/// A reader that answers a 6-byte header declaring `payload_len` bytes and
/// then hands out payload bytes forever, the way a client streaming a large
/// frame does. It counts what it served, so a test can see how much the
/// server was willing to buffer.
struct EndlessFrame {
    header: Vec<u8>,
    header_sent: usize,
    served: Arc<std::sync::atomic::AtomicUsize>,
}

impl EndlessFrame {
    fn declaring(payload_len: usize) -> (Self, Arc<std::sync::atomic::AtomicUsize>) {
        let mut header = vec![0x03u8, 0]; // MSG_QUERY, no flags
        header.extend_from_slice(&(payload_len as u32).to_le_bytes());
        let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (
            EndlessFrame {
                header,
                header_sent: 0,
                served: Arc::clone(&served),
            },
            served,
        )
    }
}

impl tokio::io::AsyncRead for EndlessFrame {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let remaining_header = self.header.len() - self.header_sent;
        let n = if remaining_header > 0 {
            let n = remaining_header.min(buf.remaining());
            let from = self.header_sent;
            let bytes = self.header[from..from + n].to_vec();
            buf.put_slice(&bytes);
            self.header_sent += n;
            n
        } else {
            let n = buf.remaining().min(8192);
            buf.put_slice(&vec![b'x'; n]);
            n
        };
        self.served
            .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
        std::task::Poll::Ready(Ok(()))
    }
}

/// A frame that declares more payload than the read-ahead budget has left
/// must not be buffered while a statement runs.
///
/// The budget exists to bound what one connection can hold in memory ahead of
/// the statement it is executing. `has_room()` only decides whether a read may
/// START; if the frame being read is not bounded too, a peer that opens a
/// statement and then announces a 64 MiB frame parks 64 MiB of server heap per
/// connection for as long as the statement runs, and `Vec::drain` never gives
/// the capacity back afterwards.
///
/// Nothing is lost by refusing to read it here: the frame's header stays
/// buffered and the ordinary main loop reads the whole frame, at the full wire
/// limit, once the statement finishes.
#[tokio::test]
async fn a_frame_over_the_read_ahead_budget_is_not_buffered_while_a_query_runs() {
    let (dir, engine) = one_row_engine();
    let metrics = Arc::new(Metrics::new());
    let (reader, served) = EndlessFrame::declaring(MAX_WIRE_PAYLOAD_SIZE);
    let mut reader = BufReader::new(reader);
    let mut buffered = Vec::new();
    let mut pending = InFlightReadAhead::default();

    let (message, _durability, termination, _) = run_blocking_query(
        BlockingQuery {
            engine: Arc::clone(&engine),
            principal: None,
            result_mode: WireResultMode::LegacyText,
            query_timeout: Duration::from_secs(5),
            query_deadline: Instant::now() + Duration::from_secs(5),
            metrics: &metrics,
            stream: FrameStream {
                reader: &mut reader,
                buffered: &mut buffered,
                pending: &mut pending,
            },
        },
        (),
        |_engine, (), _principal| {
            // Long enough that the read arm is polled to completion first,
            // short enough to keep the test quick.
            std::thread::sleep(Duration::from_millis(400));
            (
                Ok(QueryResult::Executed {
                    message: "done".into(),
                }),
                None,
            )
        },
    )
    .await;

    assert!(
        buffered.len() <= MAX_IN_FLIGHT_READ_AHEAD_BYTES,
        "the connection buffered {} bytes of one frame against a \
         {MAX_IN_FLIGHT_READ_AHEAD_BYTES}-byte read-ahead budget",
        buffered.len()
    );
    assert!(
        served.load(std::sync::atomic::Ordering::Relaxed) <= MAX_IN_FLIGHT_READ_AHEAD_BYTES,
        "the connection read {} bytes off the socket against a \
         {MAX_IN_FLIGHT_READ_AHEAD_BYTES}-byte read-ahead budget",
        served.load(std::sync::atomic::Ordering::Relaxed)
    );
    // The statement still ran to completion and the connection stays open:
    // the oversized frame is deferred, not refused, and not a disconnect.
    assert!(
        matches!(message, Message::ResultMessage { .. }),
        "the statement must still answer, got {message:?}"
    );
    assert!(
        termination.is_none(),
        "an oversized read-ahead frame must not end the connection, got {termination:?}"
    );
    drop(dir);
}

/// Small pipelined frames are still read ahead, so the bound above is not a
/// bound that works by never reading anything.
#[tokio::test]
async fn small_pipelined_frames_are_still_read_ahead() {
    let (dir, engine) = one_row_engine();
    let metrics = Arc::new(Metrics::new());
    let mut frames = Vec::new();
    for _ in 0..3 {
        frames.extend_from_slice(
            &Message::Query {
                query: "User".into(),
            }
            .encode(),
        );
    }
    let mut reader = BufReader::new(std::io::Cursor::new(frames));
    let mut buffered = Vec::new();
    let mut pending = InFlightReadAhead::default();

    run_blocking_query(
        BlockingQuery {
            engine: Arc::clone(&engine),
            principal: None,
            result_mode: WireResultMode::LegacyText,
            query_timeout: Duration::from_secs(5),
            query_deadline: Instant::now() + Duration::from_secs(5),
            metrics: &metrics,
            stream: FrameStream {
                reader: &mut reader,
                buffered: &mut buffered,
                pending: &mut pending,
            },
        },
        (),
        |_engine, (), _principal| {
            std::thread::sleep(Duration::from_millis(200));
            (
                Ok(QueryResult::Executed {
                    message: "done".into(),
                }),
                None,
            )
        },
    )
    .await;

    assert!(
        !pending.is_empty(),
        "ordinary pipelined frames must still be read while a statement runs"
    );
    drop(dir);
}
