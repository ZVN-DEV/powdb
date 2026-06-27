//! Process-level test: the real `powdb-server` binary must accept connections
//! over a **Unix domain socket** when started with `--socket <path>`, serving
//! the same wire protocol as the TCP listener.
//!
//! UDS removes the TCP/IP stack from the same-host path (~2× lower round-trip
//! latency), which is the dominant cost for same-host clients — see
//! `docs/design/2026-06-27-beating-sqlite-latency-design.md`. The TCP listener
//! stays on unconditionally; the socket is additive.
#![cfg(unix)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

// Wire helpers mirror crates/server/tests/integration.rs so the test exercises
// the real framing, just over a UnixStream instead of a TcpStream.
fn encode_connect(db: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(db.len() as u32).to_le_bytes());
    payload.extend_from_slice(db.as_bytes());
    payload.extend_from_slice(&0u32.to_le_bytes()); // empty password => None
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

async fn read_response(stream: &mut UnixStream) -> Vec<u8> {
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

/// Grab an ephemeral TCP port the OS just confirmed is free, then release it so
/// the server can bind it. The server always binds TCP; the socket is extra.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn spawn_server(port: u16, socket: &std::path::Path, data_dir: &std::path::Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_powdb-server"))
        .arg("--port")
        .arg(port.to_string())
        .arg("--bind")
        .arg("127.0.0.1")
        .arg("--socket")
        .arg(socket)
        .arg("--data-dir")
        .arg(data_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn powdb-server binary")
}

async fn wait_for_socket(path: &std::path::Path, within: Duration) -> UnixStream {
    let start = Instant::now();
    loop {
        if let Ok(stream) = UnixStream::connect(path).await {
            return stream;
        }
        assert!(
            start.elapsed() <= within,
            "server did not accept connections on socket {path:?} within {within:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn server_serves_over_unix_socket() {
    let test_id = std::process::id();
    let socket_path = std::env::temp_dir().join(format!("powdb_uds_{test_id}.sock"));
    let data_dir = std::env::temp_dir().join(format!("powdb_uds_data_{test_id}"));
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_dir_all(&data_dir);
    std::fs::create_dir_all(&data_dir).unwrap();

    let mut child = spawn_server(free_port(), &socket_path, &data_dir);

    let mut stream = wait_for_socket(&socket_path, Duration::from_secs(10)).await;

    // Handshake.
    stream.write_all(&encode_connect("testdb")).await.unwrap();
    let resp = read_response(&mut stream).await;
    assert_eq!(resp[0], 0x02, "expected CONNECT_OK over UDS");

    // Create + insert + count — proves the full read/write path works over UDS.
    stream
        .write_all(&encode_query("type User { required name: str, age: int }"))
        .await
        .unwrap();
    let resp = read_response(&mut stream).await;
    assert!(
        resp[0] == 0x09 || resp[0] == 0x0B,
        "expected RESULT_OK/RESULT_MSG for create, got 0x{:02X}",
        resp[0]
    );

    stream
        .write_all(&encode_query(r#"insert User { name := "Ada", age := 36 }"#))
        .await
        .unwrap();
    let resp = read_response(&mut stream).await;
    assert_eq!(
        resp[0], 0x09,
        "expected RESULT_OK for insert, got 0x{:02X}",
        resp[0]
    );

    stream
        .write_all(&encode_query("count(User)"))
        .await
        .unwrap();
    let resp = read_response(&mut stream).await;
    assert_eq!(
        resp[0], 0x08,
        "expected RESULT_SCALAR for count, got 0x{:02X}",
        resp[0]
    );
    // The scalar payload is a length-prefixed string "1".
    let scalar = String::from_utf8(resp[10..].to_vec()).unwrap();
    assert_eq!(scalar, "1", "count(User) should be 1 over UDS");

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_dir_all(&data_dir);
}
