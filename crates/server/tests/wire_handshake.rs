//! End-to-end tests for wire protocol version negotiation.
//!
//! Contract under test (see docs/integrations/powql-for-drivers.md):
//!   - a pre-v0.22.0 client (no hello block) still connects, and gets a
//!     CONNECT_OK whose payload is byte-identical to the legacy one,
//!   - a client that sends a hello gets the negotiated version and the
//!     agreed feature set back,
//!   - a client whose version range does not overlap the server's is refused
//!     DURING the handshake with a typed `ProtocolVersion` error, in both
//!     directions (too old and too new), and the connection is closed so no
//!     mismatch can surface on a later frame.
//!
//! Helpers are local to this file to keep the raw framing visible, matching
//! the style of wire_error_codes.rs.

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use powdb_server::protocol::{
    decode_error_class, ClientHello, ErrorClass, Message, ServerHello,
    MAX_SUPPORTED_PROTOCOL_VERSION, MIN_SUPPORTED_PROTOCOL_VERSION, SERVER_FEATURES,
};

/// Boot an in-process server on an OS-assigned port and return its address.
async fn spawn_server(data_dir: std::path::PathBuf) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let engine = powdb_query::executor::Engine::new(&data_dir).unwrap();
        let engine = std::sync::Arc::new(std::sync::RwLock::new(engine));
        let tx_gate = powdb_server::handler::new_tx_gate();
        loop {
            let (stream, peer) = listener.accept().await.unwrap();
            let eng = engine.clone();
            let tx_gate = tx_gate.clone();
            let (_, mut rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                powdb_server::handler::handle_connection(
                    stream,
                    powdb_server::handler::ConnOpts {
                        tx_wait_timeout: Duration::from_secs(5),
                        db_name: None,
                        engine: eng,
                        tx_gate,
                        expected_password: None,
                        users: std::sync::Arc::new(powdb_auth::UserStore::new()),
                        shutdown_rx: &mut rx,
                        idle_timeout: Duration::from_secs(30),
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

/// Read one full frame (header + payload) as raw bytes.
async fn read_frame(stream: &mut TcpStream) -> Vec<u8> {
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

/// The exact CONNECT bytes a v0.21.0 client writes: db name and password,
/// no username and no hello block.
fn legacy_connect_frame(db: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(db.len() as u32).to_le_bytes());
    payload.extend_from_slice(db.as_bytes());
    payload.extend_from_slice(&0u32.to_le_bytes());
    let mut frame = vec![0x01, 0x00];
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    frame
}

fn hello_connect_frame(db: &str, min_protocol: u16, max_protocol: u16) -> Vec<u8> {
    Message::ConnectWithHello {
        db_name: db.into(),
        password: None,
        username: None,
        hello: ClientHello {
            min_protocol,
            max_protocol,
            catalog_version: 0,
            features: SERVER_FEATURES.iter().map(|f| (*f).to_owned()).collect(),
        },
    }
    .encode()
}

fn query_frame(query: &str) -> Vec<u8> {
    Message::Query {
        query: query.into(),
    }
    .encode()
}

/// Prove the connection is usable for real work after the handshake.
async fn assert_session_works(stream: &mut TcpStream) {
    stream
        .write_all(&query_frame("type T { a: int }"))
        .await
        .unwrap();
    let reply = read_frame(stream).await;
    match Message::decode(&reply).unwrap() {
        Message::ResultMessage { .. } => {}
        other => panic!("expected the session to serve DDL, got {other:?}"),
    }
}

/// After a refused handshake, prove the server hung up: a follow-up query
/// gets no reply at all, so a version mismatch cannot surface mid-session.
async fn assert_closed_after_refusal(stream: &mut TcpStream) {
    // The write may or may not land depending on when the peer's FIN arrives;
    // what matters is that no reply ever comes back.
    let _ = stream.write_all(&query_frame("1 + 1")).await;
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte))
        .await
        .expect("server must close rather than hang after refusing the handshake");
    assert!(
        matches!(read, Ok(0) | Err(_)),
        "expected EOF after a refused handshake, got a further frame byte {read:?}"
    );
}

#[tokio::test]
async fn legacy_client_connects_and_gets_a_byte_identical_connect_ok() {
    let data_dir = tempfile::tempdir().unwrap();
    let addr = spawn_server(data_dir.path().to_path_buf()).await;
    let mut stream = TcpStream::connect(&addr).await.unwrap();
    stream
        .write_all(&legacy_connect_frame("testdb"))
        .await
        .unwrap();

    let frame = read_frame(&mut stream).await;
    assert_eq!(frame[0], 0x02, "expected CONNECT_OK");
    match Message::decode(&frame).unwrap() {
        Message::ConnectOk { version } => {
            // Byte-identical: the payload is the version string and nothing
            // else, exactly as a v0.21.0 server would answer.
            assert_eq!(frame, Message::ConnectOk { version }.encode());
        }
        other => panic!("a hello-less CONNECT must get a hello-less CONNECT_OK, got {other:?}"),
    }

    // And the session works normally afterwards.
    assert_session_works(&mut stream).await;
}

#[tokio::test]
async fn hello_client_gets_the_negotiated_version_and_agreed_features() {
    let data_dir = tempfile::tempdir().unwrap();
    let addr = spawn_server(data_dir.path().to_path_buf()).await;
    let mut stream = TcpStream::connect(&addr).await.unwrap();
    stream
        .write_all(&hello_connect_frame(
            "testdb",
            MIN_SUPPORTED_PROTOCOL_VERSION,
            MAX_SUPPORTED_PROTOCOL_VERSION,
        ))
        .await
        .unwrap();

    let frame = read_frame(&mut stream).await;
    match Message::decode(&frame).unwrap() {
        Message::ConnectOkWithHello { version, hello } => {
            assert!(!version.is_empty());
            assert_eq!(hello.protocol, MAX_SUPPORTED_PROTOCOL_VERSION);
            assert_eq!(hello.min_protocol, MIN_SUPPORTED_PROTOCOL_VERSION);
            assert_eq!(hello.max_protocol, MAX_SUPPORTED_PROTOCOL_VERSION);
            assert!(
                hello.catalog_version > 0,
                "server must state its catalog format"
            );
            assert_eq!(hello.features, SERVER_FEATURES);
        }
        other => panic!("expected ConnectOkWithHello, got {other:?}"),
    }

    assert_session_works(&mut stream).await;
}

#[tokio::test]
async fn a_client_older_than_the_server_floor_is_refused_during_the_handshake() {
    let data_dir = tempfile::tempdir().unwrap();
    let addr = spawn_server(data_dir.path().to_path_buf()).await;
    let mut stream = TcpStream::connect(&addr).await.unwrap();
    // Ceiling below this server's floor: the shape a client takes once a
    // future server raises its minimum past a shipped release.
    stream
        .write_all(&hello_connect_frame("testdb", 0, 0))
        .await
        .unwrap();

    let frame = read_frame(&mut stream).await;
    assert_eq!(frame[0], 0x0A, "expected an Error frame, not CONNECT_OK");
    assert_eq!(
        decode_error_class(&frame),
        Some(ErrorClass::ProtocolVersion.as_u8()),
        "a version refusal must carry the ProtocolVersion class"
    );
    match Message::decode(&frame).unwrap() {
        Message::Error { message } => {
            assert!(message.contains("wire protocol"), "{message}");
            assert!(message.contains("upgrade the client"), "{message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
    assert_closed_after_refusal(&mut stream).await;
}

#[tokio::test]
async fn a_client_newer_than_the_server_ceiling_is_refused_during_the_handshake() {
    let data_dir = tempfile::tempdir().unwrap();
    let addr = spawn_server(data_dir.path().to_path_buf()).await;
    let mut stream = TcpStream::connect(&addr).await.unwrap();
    stream
        .write_all(&hello_connect_frame("testdb", 9000, 9001))
        .await
        .unwrap();

    let frame = read_frame(&mut stream).await;
    assert_eq!(frame[0], 0x0A, "expected an Error frame, not CONNECT_OK");
    assert_eq!(
        decode_error_class(&frame),
        Some(ErrorClass::ProtocolVersion.as_u8()),
    );
    match Message::decode(&frame).unwrap() {
        Message::Error { message } => {
            assert!(message.contains("upgrade the server"), "{message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
    assert_closed_after_refusal(&mut stream).await;
}

#[tokio::test]
async fn a_new_client_against_a_pre_negotiation_server_fails_at_handshake_time() {
    // The mirror direction, which cannot be produced by this server: replay
    // the exact CONNECT_OK a v0.21.0 server writes and run the client-side
    // capability check on it. A client demanding protocol v2 must reject it
    // there and then, before it ever sends a frame that server cannot parse.
    let legacy_ok = Message::ConnectOk {
        version: "0.21.0".into(),
    }
    .encode();
    let server_hello = match Message::decode(&legacy_ok).unwrap() {
        Message::ConnectOk { .. } => ServerHello::legacy(),
        Message::ConnectOkWithHello { hello, .. } => hello,
        other => panic!("expected ConnectOk, got {other:?}"),
    };
    let refusal = powdb_server::protocol::require_server_capabilities(
        &server_hello,
        MAX_SUPPORTED_PROTOCOL_VERSION,
        &[],
        0,
    )
    .expect_err("a v1 server must not satisfy a v2 requirement");
    assert!(
        refusal.contains("wire protocol") && refusal.contains("upgrade the server"),
        "the refusal must name the protocol version: {refusal}"
    );

    // And the same client stays backward compatible when it does not demand
    // v2: a legacy server is still usable at protocol v1.
    powdb_server::protocol::require_server_capabilities(
        &server_hello,
        MIN_SUPPORTED_PROTOCOL_VERSION,
        &[],
        0,
    )
    .expect("a client that does not require v2 must still accept a v1 server");
}

/// Stand up a listener that answers CONNECT the way a pre-v0.22.0 server does:
/// a bare `ConnectOk` with no hello block, then nothing. Returns its address.
///
/// The check above is pure; this one puts the same reply through the real
/// framing path, because "the server sent no hello" has to be recognised from
/// bytes off a socket, not from a value handed to a function.
async fn spawn_pre_negotiation_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                // Consume the CONNECT frame, hello block and all: a v0.21.0
                // server reads the fields it knows and ignores the rest.
                let mut header = [0u8; 6];
                if stream.read_exact(&mut header).await.is_err() {
                    return;
                }
                let payload_len = u32::from_le_bytes(header[2..6].try_into().unwrap()) as usize;
                let mut payload = vec![0u8; payload_len];
                if payload_len > 0 && stream.read_exact(&mut payload).await.is_err() {
                    return;
                }
                let ok = Message::ConnectOk {
                    version: "0.21.0".into(),
                }
                .encode();
                let _ = stream.write_all(&ok).await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn a_pre_negotiation_server_is_recognised_as_protocol_one_over_a_real_socket() {
    let addr = spawn_pre_negotiation_server().await;
    let mut stream = TcpStream::connect(&addr).await.unwrap();
    stream
        .write_all(&hello_connect_frame(
            "testdb",
            MIN_SUPPORTED_PROTOCOL_VERSION,
            MAX_SUPPORTED_PROTOCOL_VERSION,
        ))
        .await
        .unwrap();

    let reply = read_frame(&mut stream).await;
    let hello = match Message::decode(&reply).unwrap() {
        // The old server ignored the hello it did not understand and answered
        // the frame it has always answered.
        Message::ConnectOk { version } => {
            assert_eq!(version, "0.21.0");
            ServerHello::legacy()
        }
        other => panic!("a v0.21.0 server must answer a bare ConnectOk, got {other:?}"),
    };
    assert_eq!(hello.protocol, MIN_SUPPORTED_PROTOCOL_VERSION);
    assert!(
        hello.features.is_empty(),
        "a server that named no features must not be credited with any"
    );

    // A client that needs nothing new connects anyway: this is the backward
    // compatibility promise, and there is no forced upgrade order.
    powdb_server::protocol::require_server_capabilities(
        &hello,
        MIN_SUPPORTED_PROTOCOL_VERSION,
        &[],
        powdb_server::protocol::CLIENT_CATALOG_VERSION,
    )
    .expect("a legacy server must still serve a client that requires nothing new");

    // A client that needs a negotiated protocol refuses here, during the
    // handshake, rather than on the first frame this server cannot parse.
    //
    // Requiring no features at the same time is deliberate. Asking for both at
    // once passes even when the version check is removed entirely, because the
    // feature check then produces its own "upgrade the server" message: the
    // assertion would be satisfied by the wrong code path. Each requirement
    // gets its own case, and each names what it is actually about.
    let refusal = powdb_server::protocol::require_server_capabilities(
        &hello,
        MAX_SUPPORTED_PROTOCOL_VERSION,
        &[],
        powdb_server::protocol::CLIENT_CATALOG_VERSION,
    )
    .expect_err("a v1 server cannot satisfy a client that requires v2");
    assert!(
        refusal.contains("wire protocol") && refusal.contains("upgrade the server"),
        "the refusal must name the protocol version: {refusal}"
    );

    // And separately: a feature this server never named is refused by name.
    let missing = SERVER_FEATURES.first().expect("at least one wire feature");
    let refusal = powdb_server::protocol::require_server_capabilities(
        &hello,
        MIN_SUPPORTED_PROTOCOL_VERSION,
        &[missing],
        powdb_server::protocol::CLIENT_CATALOG_VERSION,
    )
    .expect_err("a server that named no features cannot satisfy a feature requirement");
    assert!(
        refusal.contains(missing),
        "the refusal must name the missing feature: {refusal}"
    );
}

#[tokio::test]
async fn a_server_catalog_from_the_future_is_refused_during_the_handshake() {
    // The other client-side refusal: the server speaks a protocol this client
    // understands but writes a catalog format it cannot read. Driven off a
    // real CONNECT_OK frame so the ceiling is read from the wire.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let ok = Message::ConnectOkWithHello {
                version: "9.9.9".into(),
                hello: ServerHello {
                    protocol: MAX_SUPPORTED_PROTOCOL_VERSION,
                    min_protocol: MIN_SUPPORTED_PROTOCOL_VERSION,
                    max_protocol: MAX_SUPPORTED_PROTOCOL_VERSION,
                    catalog_version: powdb_server::protocol::CLIENT_CATALOG_VERSION + 1,
                    features: SERVER_FEATURES.iter().map(|f| (*f).to_owned()).collect(),
                },
            }
            .encode();
            let _ = stream.write_all(&ok).await;
        }
    });

    let mut stream = TcpStream::connect(&addr).await.unwrap();
    stream
        .write_all(&hello_connect_frame(
            "testdb",
            MIN_SUPPORTED_PROTOCOL_VERSION,
            MAX_SUPPORTED_PROTOCOL_VERSION,
        ))
        .await
        .unwrap();
    let reply = read_frame(&mut stream).await;
    let hello = match Message::decode(&reply).unwrap() {
        Message::ConnectOkWithHello { hello, .. } => hello,
        other => panic!("expected ConnectOkWithHello, got {other:?}"),
    };
    let refusal = powdb_server::protocol::require_server_capabilities(
        &hello,
        MIN_SUPPORTED_PROTOCOL_VERSION,
        &[],
        powdb_server::protocol::CLIENT_CATALOG_VERSION,
    )
    .expect_err("a catalog format from the future must be refused");
    assert!(refusal.contains("upgrade the client"), "{refusal}");
}
