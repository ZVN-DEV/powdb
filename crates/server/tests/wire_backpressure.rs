//! Pipelining and frame-level refusals must never end as a silent close.
//!
//! Two failures shared one shape: the server stopped talking. A client that
//! pipelined more than the 128-frame in-flight read-ahead cap had its
//! connection dropped mid-burst with no Error frame (22 of 200 `Promise.all`
//! queries answered, then ECONNRESET), and a frame the protocol reader
//! refused outright (an oversized payload, more parameters than the cap) was
//! answered the same way. In both cases the client sees a transport error and
//! cannot tell a server fault from its own mistake, while the client README
//! promises every in-flight query still gets its result.

mod common;

use common::{encode_connect, encode_query, read_message, read_response_message, InprocServer};
use powdb_query::executor::Engine;
use powdb_server::protocol::{decode_error_class, ErrorClass, Message};
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn connect(addr: std::net::SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&encode_connect("test")).await.unwrap();
    match read_response_message(&mut stream).await {
        Message::ConnectOk { .. } | Message::ConnectOkWithHello { .. } => {}
        other => panic!("expected CONNECT_OK, got {other:?}"),
    }
    stream
}

/// A burst far past both the read-ahead cap (128 frames) and the pipeline
/// batch cap (128 responses) must be answered in full and in order.
#[tokio::test]
async fn five_hundred_pipelined_statements_all_get_replies() {
    const BURST: usize = 500;

    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine.execute_powql("type T { required id: int }").unwrap();
    let (addr, _server) = InprocServer::default()
        .start(Arc::new(RwLock::new(engine)))
        .await;
    let mut stream = connect(addr).await;

    let mut burst = Vec::new();
    for id in 0..BURST {
        burst.extend_from_slice(&encode_query(&format!("insert T {{ id := {id} }}")));
    }
    stream.write_all(&burst).await.unwrap();
    stream.flush().await.unwrap();

    let mut answered = 0usize;
    let mut first_failure = None;
    for _ in 0..BURST {
        match read_message(&mut stream).await {
            Some(Message::ResultOk { .. }) => answered += 1,
            Some(other) => {
                first_failure = Some(format!("{other:?}"));
                break;
            }
            None => {
                first_failure = Some("connection closed".to_string());
                break;
            }
        }
    }
    assert_eq!(
        answered, BURST,
        "only {answered} of {BURST} pipelined statements were answered; first non-result: {first_failure:?}"
    );

    let count = run(&mut stream, "count(T)").await;
    match count {
        Message::ResultScalar { value } => assert_eq!(value, BURST.to_string()),
        Message::ResultScalarNative { value } => {
            assert_eq!(value.to_wire_string(), BURST.to_string())
        }
        other => panic!("expected a scalar count, got {other:?}"),
    }
}

async fn run(stream: &mut TcpStream, query: &str) -> Message {
    stream.write_all(&encode_query(query)).await.unwrap();
    read_response_message(stream).await
}

/// Read one raw frame, or `None` when the connection closed without sending
/// one. The class byte lives past the length-prefixed message, so the refusal
/// has to be inspected as bytes rather than through `Message::decode`.
async fn read_raw_frame(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut header = [0u8; 6];
    stream.read_exact(&mut header).await.ok()?;
    let payload_len = u32::from_le_bytes(header[2..6].try_into().unwrap()) as usize;
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream.read_exact(&mut payload).await.ok()?;
    }
    let mut full = Vec::with_capacity(6 + payload_len);
    full.extend_from_slice(&header);
    full.extend_from_slice(&payload);
    Some(full)
}

/// The message and class of a refusal frame, failing loudly on a bare close.
fn refusal(frame: Option<Vec<u8>>, what: &str) -> (String, ErrorClass) {
    let frame =
        frame.unwrap_or_else(|| panic!("{what}: the connection closed with no Error frame at all"));
    let raw = decode_error_class(&frame)
        .unwrap_or_else(|| panic!("{what}: the Error frame carries no class byte"));
    let class = ErrorClass::from_u8(raw)
        .unwrap_or_else(|| panic!("{what}: unknown error class byte {raw}"));
    let message = match Message::decode(&frame).expect("an error frame must decode") {
        Message::Error { message } | Message::ErrorWithClass { message, .. } => message,
        other => panic!("{what}: expected an Error frame, got {other:?}"),
    };
    (message, class)
}

/// A frame header that declares more payload than the protocol accepts is a
/// client mistake, and the client has to be told which cap it hit.
#[tokio::test]
async fn an_oversized_frame_is_refused_with_an_error_frame() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(dir.path()).unwrap();
    let (addr, _server) = InprocServer::default()
        .start(Arc::new(RwLock::new(engine)))
        .await;
    let mut stream = connect(addr).await;

    // 70 MiB declared payload, past the 64 MiB wire limit. Only the header is
    // sent: the reader refuses on the declared length alone.
    let mut header = vec![0x03u8, 0];
    header.extend_from_slice(&(70u32 * 1024 * 1024).to_le_bytes());
    stream.write_all(&header).await.unwrap();
    stream.flush().await.unwrap();

    let (message, class) = refusal(read_raw_frame(&mut stream).await, "oversized frame");
    assert_eq!(class, ErrorClass::LimitExceeded);
    assert!(
        message.contains("payload too large"),
        "the refusal must name the cap: {message}"
    );
}

/// The same for a frame the decoder refuses: more parameters than the cap.
#[tokio::test]
async fn too_many_parameters_is_refused_with_an_error_frame() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(dir.path()).unwrap();
    let (addr, _server) = InprocServer::default()
        .start(Arc::new(RwLock::new(engine)))
        .await;
    let mut stream = connect(addr).await;

    let query = "T filter .id = $1 { .id }";
    let mut payload = Vec::new();
    payload.extend_from_slice(&(query.len() as u32).to_le_bytes());
    payload.extend_from_slice(query.as_bytes());
    // One past MAX_PARAMS (4096); no parameter bodies follow, the count alone
    // is refused.
    payload.extend_from_slice(&4097u16.to_le_bytes());
    let mut frame = vec![0x04u8, 0];
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    stream.write_all(&frame).await.unwrap();
    stream.flush().await.unwrap();

    let (message, class) = refusal(read_raw_frame(&mut stream).await, "too many parameters");
    assert_eq!(class, ErrorClass::LimitExceeded);
    assert!(
        message.contains("parameters"),
        "the refusal must name what was refused: {message}"
    );
}
