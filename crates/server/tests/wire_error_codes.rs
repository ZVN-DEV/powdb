//! Wire-level tests for the MSG_ERROR trailing error-class byte.
//!
//! Contract under test (see docs/errors.md):
//!   - a new server appends exactly one stable class byte after the
//!     length-prefixed error message inside the existing MSG_ERROR payload,
//!   - an old client that reads only the length-prefixed string still gets
//!     the full message (old-client + new-server direction),
//!   - a frame without the byte (old server) yields "no class"
//!     (new-client + old-server direction),
//!   - the `__POWDB_READONLY_NEEDS_WRITE__` sentinel never crosses the wire.
//!
//! Helpers are local to this file on purpose: shared test helpers are being
//! restructured in parallel, and these tests must exercise the raw framing
//! rather than a convenience layer.

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use powdb_server::protocol::{decode_error_class, ErrorClass, Message};

fn encode_connect(db: &str, password: Option<&str>) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(db.len() as u32).to_le_bytes());
    payload.extend_from_slice(db.as_bytes());
    match password {
        Some(pw) => {
            payload.extend_from_slice(&(pw.len() as u32).to_le_bytes());
            payload.extend_from_slice(pw.as_bytes());
        }
        // Empty password (len=0) means None.
        None => payload.extend_from_slice(&0u32.to_le_bytes()),
    }
    let mut frame = Vec::new();
    frame.push(0x01); // CONNECT
    frame.push(0); // flags
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    frame
}

fn encode_query(q: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(q.len() as u32).to_le_bytes());
    payload.extend_from_slice(q.as_bytes());
    let mut frame = Vec::new();
    frame.push(0x03); // QUERY
    frame.push(0);
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    frame
}

/// Read one full frame (header + payload) as raw bytes so tests can inspect
/// the exact wire layout, including the trailing class byte.
async fn read_response(stream: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 6];
    stream.read_exact(&mut header).await.unwrap();
    let payload_len = u32::from_le_bytes(header[2..6].try_into().unwrap()) as usize;
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream.read_exact(&mut payload).await.unwrap();
    }
    let mut full = Vec::new();
    full.extend_from_slice(&header);
    full.extend_from_slice(&payload);
    full
}

/// Decode a raw MSG_ERROR frame the way an OLD client does: through the
/// unchanged `Message::decode` path, which reads the message by its length
/// prefix and ignores the trailing class byte.
fn old_client_error_message(frame: &[u8]) -> String {
    assert_eq!(frame[0], 0x0A, "expected MSG_ERROR, got 0x{:02X}", frame[0]);
    match Message::decode(frame).expect("old-style decode must succeed") {
        Message::Error { message } => message,
        other => panic!("expected Error, got {other:?}"),
    }
}

/// Boot an in-process server on an OS-assigned port and return its address.
/// Mirrors the pattern in integration.rs, with an optional server password.
async fn spawn_server(data_dir: std::path::PathBuf, password: Option<&str>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let expected_password = password.map(|p| zeroize::Zeroizing::new(p.to_string()));
    tokio::spawn(async move {
        let engine = powdb_query::executor::Engine::new(&data_dir).unwrap();
        let engine = std::sync::Arc::new(std::sync::RwLock::new(engine));
        let tx_gate = powdb_server::handler::new_tx_gate();
        loop {
            let (stream, peer) = listener.accept().await.unwrap();
            let eng = engine.clone();
            let tx_gate = tx_gate.clone();
            let expected_password = expected_password.clone();
            let (_, mut rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                powdb_server::handler::handle_connection(
                    stream,
                    powdb_server::handler::ConnOpts {
                        tx_wait_timeout: Duration::from_secs(5),
                        db_name: None,
                        engine: eng,
                        tx_gate,
                        expected_password,
                        users: std::sync::Arc::new(powdb_auth::UserStore::new()),
                        shutdown_rx: &mut rx,
                        idle_timeout: Duration::from_secs(300),
                        preauth_deadline: powdb_server::handler::DEFAULT_PREAUTH_DEADLINE,
                        query_timeout: Duration::from_secs(30),
                        rate_limiter: None,
                        peer_addr: Some(peer),
                        metrics: std::sync::Arc::new(powdb_server::metrics::Metrics::new()),
                    },
                )
                .await;
            });
        }
    });
    addr
}

async fn connect_ok(addr: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&encode_connect("testdb", None))
        .await
        .unwrap();
    let resp = read_response(&mut stream).await;
    assert_eq!(resp[0], 0x02, "expected CONNECT_OK");
    stream
}

/// Send a query and return the raw error frame it produces.
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

#[tokio::test]
async fn server_errors_carry_stable_class_and_stay_old_client_decodable() {
    let data_dir = tempfile::tempdir().unwrap();
    let addr = spawn_server(data_dir.path().to_path_buf(), None).await;
    let mut stream = connect_ok(&addr).await;

    // Parse failure: class 1, message forwarded verbatim (safe prefix).
    let frame = query_error_frame(&mut stream, "type type type {{{").await;
    assert_eq!(
        decode_error_class(&frame),
        Some(ErrorClass::Parse.as_u8()),
        "parse errors must carry class byte 1"
    );
    let message = old_client_error_message(&frame);
    assert!(
        !message.is_empty(),
        "old-style decode must still yield the message"
    );

    // Execution failure (unknown table): class 2, message intact through an
    // old client's decoder even though the new frame has a trailing byte.
    let frame = query_error_frame(&mut stream, "NoSuchTable filter .x = 1").await;
    assert_eq!(
        decode_error_class(&frame),
        Some(ErrorClass::Execution.as_u8())
    );
    let message = old_client_error_message(&frame);
    assert!(
        message.contains("not found"),
        "unexpected message: {message}"
    );

    // The class byte is exactly one byte after the length-prefixed string.
    let payload = &frame[6..];
    let string_len = u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize;
    assert_eq!(
        payload.len(),
        4 + string_len + 1,
        "MSG_ERROR payload must be string + exactly one class byte"
    );
}

#[tokio::test]
async fn auth_failure_carries_auth_class() {
    let data_dir = tempfile::tempdir().unwrap();
    let addr = spawn_server(data_dir.path().to_path_buf(), Some("hunter2")).await;

    let mut stream = TcpStream::connect(&addr).await.unwrap();
    stream
        .write_all(&encode_connect("testdb", Some("wrong-password")))
        .await
        .unwrap();
    let frame = read_response(&mut stream).await;
    assert_eq!(frame[0], 0x0A, "expected MSG_ERROR for bad password");
    assert_eq!(
        decode_error_class(&frame),
        Some(ErrorClass::AuthFailed.as_u8())
    );
    assert_eq!(old_client_error_message(&frame), "authentication failed");
}

#[tokio::test]
async fn legacy_error_frame_without_class_byte_decodes_as_no_class() {
    // New-client + old-server direction: an old server sends only the
    // length-prefixed string. The class accessor must report "absent"
    // rather than misreading message bytes.
    let legacy_frame = Message::Error {
        message: "table 'users' not found".into(),
    }
    .encode();
    assert_eq!(decode_error_class(&legacy_frame), None);
    assert_eq!(
        old_client_error_message(&legacy_frame),
        "table 'users' not found"
    );
}

/// TASK-B5 regression guard: the internal readonly-escalation sentinel must
/// never cross the wire. A read-only server refuses a write with the
/// operator-facing readonly message and the ReadonlyRefused class.
#[tokio::test]
async fn readonly_refusal_never_leaks_sentinel_and_carries_readonly_class() {
    const SENTINEL: &str = "__POWDB_READONLY_NEEDS_WRITE__";

    // Seed a quiescent (checkpointed, WAL-clean) directory with the embedded
    // engine, mirroring read_only_serving.rs.
    let data_dir = tempfile::tempdir().unwrap();
    {
        let mut engine = powdb_query::executor::Engine::new(data_dir.path()).unwrap();
        engine
            .execute_powql("type User { required name: str, age: int }")
            .unwrap();
        engine
            .execute_powql(r#"insert User { name := "Ada", age := 36 }"#)
            .unwrap();
    }

    // Boot the real server binary in --readonly mode.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_powdb-server"))
        .arg("--port")
        .arg(port.to_string())
        .arg("--bind")
        .arg("127.0.0.1")
        .arg("--data-dir")
        .arg(data_dir.path())
        .arg("--readonly")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn powdb-server --readonly");

    let mut stream = {
        let start = std::time::Instant::now();
        loop {
            if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)).await {
                break stream;
            }
            assert!(
                start.elapsed() <= Duration::from_secs(10),
                "read-only server did not bind within 10s"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };

    stream
        .write_all(&encode_connect("testdb", None))
        .await
        .unwrap();
    let resp = read_response(&mut stream).await;
    assert_eq!(resp[0], 0x02, "expected CONNECT_OK");

    let frame = query_error_frame(&mut stream, r#"insert User { name := "Eve", age := 1 }"#).await;
    let message = old_client_error_message(&frame);
    assert!(
        !message.contains(SENTINEL),
        "readonly sentinel leaked over the wire: {message}"
    );
    assert!(
        message.starts_with("readonly mode"),
        "expected the operator-facing readonly message, got: {message}"
    );
    assert_eq!(
        decode_error_class(&frame),
        Some(ErrorClass::ReadonlyRefused.as_u8()),
        "readonly refusal must carry class byte 5"
    );

    // The connection stays usable after the refusal: a read still succeeds
    // and, being a success frame, never carries an error class.
    stream
        .write_all(&encode_query("User filter .age > 0"))
        .await
        .unwrap();
    let read_frame = read_response(&mut stream).await;
    assert_eq!(read_frame[0], 0x07, "expected RESULT_ROWS after refusal");
    assert_eq!(decode_error_class(&read_frame), None);

    child.kill().ok();
    child.wait().ok();
}

/// Send a query and assert it succeeded (any non-error response frame).
async fn query_ok(stream: &mut TcpStream, query: &str) {
    stream.write_all(&encode_query(query)).await.unwrap();
    let frame = read_response(stream).await;
    assert_ne!(
        frame[0],
        0x0A,
        "query {query:?} unexpectedly failed: {}",
        old_client_error_message(&frame)
    );
}

/// docs/errors.md: class 8 (`constraint_violation`) covers "Unique index
/// violation". A duplicate insert into a `unique` column must therefore
/// carry class byte 8, not 0 (`internal`).
///
/// Currently FAILING (found by the 2026-07-21 driver-writer DX pass, verified
/// over the wire against the released 0.18.0 binary): the violation is raised
/// in storage as an `io::Error`, surfaces as `QueryError::StorageError`, and
/// `classify_query_error` maps every `StorageError` to `ErrorClass::Internal`.
/// Its `unique constraint violation` prefix branch only inspects
/// `QueryError::Execution`, which this path never takes, so the wire class is
/// 0 while the message says "unique constraint violation on ...".
#[tokio::test]
async fn unique_violation_carries_constraint_class() {
    let data_dir = tempfile::tempdir().unwrap();
    let addr = spawn_server(data_dir.path().to_path_buf(), None).await;
    let mut stream = connect_ok(&addr).await;

    query_ok(&mut stream, "type UniqWire { unique u: int }").await;
    query_ok(&mut stream, "insert UniqWire { u := 1 }").await;

    let frame = query_error_frame(&mut stream, "insert UniqWire { u := 1 }").await;

    // The message itself is on the sanitizer allowlist and crosses verbatim.
    let message = old_client_error_message(&frame);
    assert!(
        message.starts_with("unique constraint violation"),
        "unexpected message: {message}"
    );

    // The contract under test: the trailing class byte is 8.
    assert_eq!(
        decode_error_class(&frame),
        Some(ErrorClass::ConstraintViolation.as_u8()),
        "unique constraint violations must carry class byte 8 (constraint_violation), \
         not 0 (internal); see docs/errors.md"
    );
}
