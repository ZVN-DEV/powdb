//! Process-level tests for `powdb-server --readonly` (snapshot serving).
//!
//! These boot the real server binary against a *quiescent* (checkpointed,
//! WAL-clean) data directory and assert the read-only contract end to end:
//!   - reads are served over the wire,
//!   - a write returns a terminal error and the connection stays usable,
//!   - two read-only server processes serve the SAME directory concurrently,
//!   - a `kill -9` mid-serve never mutates the directory (verified by hash).
#![cfg(unix)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// Wire helpers mirror crates/server/tests/integration.rs so the tests exercise
// the real framing.
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

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn spawn_readonly_server(port: u16, data_dir: &std::path::Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_powdb-server"))
        .arg("--port")
        .arg(port.to_string())
        .arg("--bind")
        .arg("127.0.0.1")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--readonly")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn powdb-server --readonly")
}

async fn wait_for_bind(port: u16, within: Duration) -> TcpStream {
    let start = Instant::now();
    loop {
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)).await {
            return stream;
        }
        assert!(
            start.elapsed() <= within,
            "server did not accept connections on port {port} within {within:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Seed a quiescent (checkpointed, WAL-clean) data directory with a table and a
/// row using the embedded engine directly, so the read-only open is
/// deterministic and independent of server shutdown timing.
fn seed_quiescent_dir(dir: &std::path::Path) {
    let mut engine = powdb_query::executor::Engine::new(dir).unwrap();
    engine
        .execute_powql("type User { required name: str, age: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert User { name := "Ada", age := 36 }"#)
        .unwrap();
    // Drop checkpoints: flush heaps + truncate the WAL, leaving a quiescent dir.
    drop(engine);
}

/// Hash every data file's path + bytes under `dir`, excluding the advisory lock
/// artifacts (`LOCK` and the `readers/` directory), which the reader lock
/// legitimately creates and removes. Data files must be byte-identical across a
/// read-only open, including a `kill -9`.
fn hash_data_files(dir: &std::path::Path) -> String {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let mut items: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
        items.sort_by_key(std::fs::DirEntry::path);
        for item in items {
            let path = item.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "LOCK" || name == "readers" {
                continue;
            }
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    fn mix(hash: &mut u64, bytes: &[u8]) {
        for &b in bytes {
            *hash ^= b as u64;
            *hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    let mut files = Vec::new();
    walk(dir, &mut files);
    // FNV-1a over each file's path + bytes. Inlined to avoid a new dependency.
    let mut hash: u64 = 0xcbf29ce484222325;
    for path in &files {
        mix(&mut hash, path.to_string_lossy().as_bytes());
        mix(&mut hash, &std::fs::read(path).unwrap());
    }
    format!("{hash:016x}")
}

#[tokio::test]
async fn two_readonly_servers_serve_same_dir_and_reject_writes() {
    let tmp = tempfile::tempdir().unwrap();
    seed_quiescent_dir(tmp.path());
    let before = hash_data_files(tmp.path());

    let port_a = free_port();
    let port_b = free_port();
    let mut srv_a = spawn_readonly_server(port_a, tmp.path());
    let mut srv_b = spawn_readonly_server(port_b, tmp.path());

    let mut a = wait_for_bind(port_a, Duration::from_secs(20)).await;
    let mut b = wait_for_bind(port_b, Duration::from_secs(20)).await;

    for stream in [&mut a, &mut b] {
        stream.write_all(&encode_connect("testdb")).await.unwrap();
        let resp = read_response(stream).await;
        assert_eq!(resp[0], 0x02, "expected CONNECT_OK");
        // A read is served: count(User) => scalar 1.
        stream
            .write_all(&encode_query("count(User)"))
            .await
            .unwrap();
        let resp = read_response(stream).await;
        assert_eq!(resp[0], 0x08, "expected RESULT_SCALAR for count");
    }

    // A write returns the terminal read-only error (frame 0x0A).
    a.write_all(&encode_query(r#"insert User { name := "Bo", age := 20 }"#))
        .await
        .unwrap();
    let resp = read_response(&mut a).await;
    assert_eq!(
        resp[0], 0x0A,
        "expected RESULT_ERROR for a write in readonly mode"
    );
    let decoded = powdb_server::protocol::Message::decode(&resp).unwrap();
    match decoded {
        powdb_server::protocol::Message::Error { message } => {
            assert!(
                message.contains("readonly mode"),
                "expected a readonly-mode message, got {message:?}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }

    // The connection stays usable after the rejected write.
    a.write_all(&encode_query("count(User)")).await.unwrap();
    let resp = read_response(&mut a).await;
    assert_eq!(
        resp[0], 0x08,
        "connection must stay usable after a rejected write"
    );

    let _ = srv_a.kill();
    let _ = srv_b.kill();
    let _ = srv_a.wait();
    let _ = srv_b.wait();

    let after = hash_data_files(tmp.path());
    assert_eq!(
        before, after,
        "two read-only servers + reads + a rejected write must not mutate the data files"
    );
}

#[tokio::test]
async fn kill9_during_readonly_serving_never_mutates_dir() {
    let tmp = tempfile::tempdir().unwrap();
    seed_quiescent_dir(tmp.path());
    let before = hash_data_files(tmp.path());

    let port = free_port();
    let mut srv = spawn_readonly_server(port, tmp.path());
    let mut stream = wait_for_bind(port, Duration::from_secs(20)).await;
    stream.write_all(&encode_connect("testdb")).await.unwrap();
    let resp = read_response(&mut stream).await;
    assert_eq!(resp[0], 0x02, "expected CONNECT_OK");

    // Issue read load, then SIGKILL the server mid-flight.
    stream.write_all(&encode_query("User")).await.unwrap();
    let _ = read_response(&mut stream).await;
    stream
        .write_all(&encode_query("count(User)"))
        .await
        .unwrap();

    // kill -9: no drop handlers run, so this proves the directory is untouched
    // by the running read-only server, not just by a clean shutdown.
    let _ = srv.kill(); // SIGKILL on unix
    let _ = srv.wait();

    let after = hash_data_files(tmp.path());
    assert_eq!(
        before, after,
        "kill -9 during read-only serving must leave the data files byte-identical"
    );
}
