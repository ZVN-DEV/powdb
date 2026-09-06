//! A remote client must be told exactly what an embedded caller is told.
//!
//! Egress rendering used to be a prefix allowlist over the message text, so
//! any diagnostic whose wording did not start with one of ~35 recognized
//! phrases reached the wire as the bare string `query execution error`. That
//! covered everyday mistakes: a mistyped column, `commit` outside a
//! transaction, a malformed uuid literal, a missing required column, an
//! upsert on a non-unique key. The same statement was diagnosable embedded
//! and undiagnosable over the wire, which is the difference between a driver
//! that can report a user error and one that cannot.
//!
//! The failures are enumerated by EXECUTING them through the real connection
//! handler and against an embedded `Engine` on an identically-built
//! directory, then comparing the two texts. Nothing here quotes a message, so
//! rewording a diagnostic keeps it covered and only a genuinely new masking
//! regresses.

mod common;

use common::{encode_connect, encode_query, read_response_message, InprocServer};
use powdb_query::executor::Engine;
use powdb_server::protocol::{decode_error_class, ErrorClass, Message, WireParam};
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Schema and rows both sides start from.
const FIXTURE: &[&str] = &[
    "type T { required id: int, s: str, u: uuid, y: bytes, d: datetime }",
    "insert T { id := 1, s := \"a\" }",
    "type P { required id: int, j: json }",
    "insert P { id := 1, j := \"{\\\"k\\\":[1,2]}\" }",
    "type Emp { required id: int, dept_id: int }",
    "alter Emp add index .dept_id",
];

/// Statements that must be refused, one per family the audit found masked.
fn probes() -> Vec<String> {
    [
        // column not found
        "T filter .nosuch = 1",
        // transaction control with no transaction
        "commit",
        "rollback",
        // typed-literal parse refusals on insert
        "insert T { id := 2, u := \"not-a-uuid\" }",
        "insert T { id := 3, y := \"zz\" }",
        "insert T { id := 4, d := \"not-a-datetime\" }",
        // missing required column / unknown field
        "insert T { s := \"x\" }",
        "insert T { id := 5, nosuchfield := 1 }",
        // negative limit
        "T order .id limit -1",
        // upsert on a non-unique key
        "upsert T on .s { id := 9, s := \"q\" }",
        // dropping a stored-column index
        "alter Emp drop index .dept_id",
        // non-scalar expression-index key
        "alter P add index (.j->k)",
        // refresh of an unknown view
        "refresh NoSuchView",
        // unknown table
        "drop NoSuchType",
        // type mismatch on update (already reached clients; kept as a control)
        "T filter .id = 1 update { id := \"x\" }",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// A fresh engine carrying [`FIXTURE`].
fn fixture_engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    for setup in FIXTURE {
        engine
            .execute_powql(setup)
            .unwrap_or_else(|e| panic!("fixture `{setup}` failed: {e}"));
    }
    (dir, engine)
}

/// The message an embedded caller is given for each probe.
fn embedded_messages(probes: &[String]) -> Vec<String> {
    let (_dir, mut engine) = fixture_engine();
    probes
        .iter()
        .map(|probe| {
            engine
                .execute_powql(probe)
                .err()
                .unwrap_or_else(|| panic!("`{probe}` was expected to be refused but succeeded"))
                .to_string()
        })
        .collect()
}

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

#[tokio::test]
async fn every_refusal_reaches_the_wire_with_the_embedded_message() {
    let probes = probes();
    let expected = embedded_messages(&probes);

    let (_dir, engine) = fixture_engine();
    let (addr, _server) = InprocServer::default()
        .start(Arc::new(RwLock::new(engine)))
        .await;
    let mut stream = connect(addr).await;

    let mut masked = Vec::new();
    for (probe, want) in probes.iter().zip(expected.iter()) {
        let got = match run(&mut stream, probe).await {
            Message::ErrorWithClass { message, .. } | Message::Error { message } => message,
            other => panic!("`{probe}` was expected to be refused, got {other:?}"),
        };
        if &got != want {
            masked.push(format!(
                "  {probe}\n    wire:     {got}\n    embedded: {want}"
            ));
        }
    }

    assert!(
        masked.is_empty(),
        "these refusals reach a remote client with a different message than an embedded \
         caller gets, so a driver cannot report the user's own mistake:\n{}",
        masked.join("\n")
    );
}

/// A parameter-count mismatch is the client's own framing error and must name
/// the counts, not be masked.
#[tokio::test]
async fn a_parameter_count_mismatch_reaches_the_wire_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine.execute_powql("type T { required id: int }").unwrap();
    let (addr, _server) = InprocServer::default()
        .start(Arc::new(RwLock::new(engine)))
        .await;
    let mut stream = connect(addr).await;

    let frame = Message::QueryWithParams {
        query: "T filter .id = $1 and .id = $2 { .id }".into(),
        params: vec![WireParam::Int(1)],
    }
    .encode();
    stream.write_all(&frame).await.unwrap();

    let mut header = [0u8; 6];
    stream.read_exact(&mut header).await.unwrap();
    let payload_len = u32::from_le_bytes(header[2..6].try_into().unwrap()) as usize;
    let mut payload = vec![0u8; payload_len];
    stream.read_exact(&mut payload).await.unwrap();
    let mut full = header.to_vec();
    full.extend_from_slice(&payload);

    let class = decode_error_class(&full).and_then(ErrorClass::from_u8);
    match Message::decode(&full).expect("an error frame must decode") {
        Message::Error { message } | Message::ErrorWithClass { message, .. } => {
            assert_ne!(
                message, "query execution error",
                "a parameter-count mismatch is the caller's own error and must name the counts"
            );
            assert!(
                message.contains("$2"),
                "the refusal must name the placeholder the caller left unbound: {message}"
            );
            assert_eq!(class, Some(ErrorClass::Parse));
        }
        other => panic!("expected an error frame, got {other:?}"),
    }
    drop(dir);
}
