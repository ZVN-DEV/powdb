//! Process-level test: the real `powdb-server` binary must shut down
//! *gracefully* — drain connections, checkpoint, exit 0 — when it receives
//! SIGTERM, the signal Docker (`docker stop`), Kubernetes (pod termination),
//! and systemd send on stop.
//!
//! Before SIGTERM handling was wired in, `main()` only awaited
//! `tokio::signal::ctrl_c()` (SIGINT), so SIGTERM fell through to the kernel's
//! default disposition and killed the process by signal — the graceful drain
//! and `catalog.checkpoint()` path never ran. This test pins the fix: under
//! SIGTERM the binary exits 0 and a row committed beforehand survives a
//! restart on the same data dir.
#![cfg(unix)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// Wire helpers mirror crates/server/tests/integration.rs so the test exercises
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

/// Grab an ephemeral port the OS just confirmed is free, then release it so the
/// server can bind it. Small TOCTOU window, standard for spawn-a-server tests.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn spawn_server(port: u16, data_dir: &std::path::Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_powdb-server"))
        .arg("--port")
        .arg(port.to_string())
        .arg("--bind")
        .arg("127.0.0.1")
        .arg("--data-dir")
        .arg(data_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn powdb-server binary")
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

fn send_sigterm(child: &Child) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("invoke kill");
    assert!(status.success(), "kill -TERM failed");
}

/// Poll for process exit with a hard timeout so a hang fails the test loudly
/// instead of wedging CI.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            panic!("server did not exit within {timeout:?} after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test]
async fn sigterm_triggers_graceful_drain_and_preserves_committed_data() {
    let tmp = tempfile::tempdir().unwrap();
    let port = free_port();

    // Boot the real server binary on a fresh data dir.
    let mut child = spawn_server(port, tmp.path());
    let mut stream = wait_for_bind(port, Duration::from_secs(20)).await;

    // Connect, define a type, insert + read the ack so the row is durably in
    // the WAL before we signal.
    stream.write_all(&encode_connect("testdb")).await.unwrap();
    assert_eq!(
        read_response(&mut stream).await[0],
        0x02,
        "expected CONNECT_OK"
    );

    stream
        .write_all(&encode_query("type Note { required body: str }"))
        .await
        .unwrap();
    let r = read_response(&mut stream).await;
    assert!(r[0] == 0x09 || r[0] == 0x0B, "expected create-type ack");

    stream
        .write_all(&encode_query(r#"insert Note { body := "survive" }"#))
        .await
        .unwrap();
    assert_eq!(
        read_response(&mut stream).await[0],
        0x09,
        "expected RESULT_OK for insert"
    );

    // Close the client so the drain has no in-flight work to wait on, then send
    // the signal orchestrators actually use.
    drop(stream);
    send_sigterm(&child);

    // Graceful shutdown must exit 0. A process killed by SIGTERM's default
    // disposition reports `success() == false` (terminated by signal 15).
    let status = wait_with_timeout(&mut child, Duration::from_secs(15));
    assert!(
        status.success(),
        "SIGTERM must trigger graceful shutdown with a clean exit (0); got {status:?}"
    );

    // The committed row must survive a restart on the same data dir.
    let mut child2 = spawn_server(port, tmp.path());
    let mut s2 = wait_for_bind(port, Duration::from_secs(20)).await;
    s2.write_all(&encode_connect("testdb")).await.unwrap();
    assert_eq!(
        read_response(&mut s2).await[0],
        0x02,
        "expected CONNECT_OK after restart"
    );

    s2.write_all(&encode_query("Note")).await.unwrap();
    let resp = read_response(&mut s2).await;
    assert_eq!(resp[0], 0x07, "expected RESULT_ROWS");
    match powdb_server::protocol::Message::decode(&resp).unwrap() {
        powdb_server::protocol::Message::ResultRows { rows, .. } => {
            assert_eq!(
                rows.len(),
                1,
                "the row committed before SIGTERM must survive the restart"
            );
        }
        other => panic!("expected ResultRows, got {other:?}"),
    }

    drop(s2);
    send_sigterm(&child2);
    let _ = wait_with_timeout(&mut child2, Duration::from_secs(15));
}
