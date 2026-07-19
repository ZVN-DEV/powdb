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

mod common;

use std::process::Child;
use std::time::Duration;

use common::{
    encode_connect, encode_query, free_port, read_response, send_sigterm, spawn_server_bin,
    wait_for_bind, wait_with_timeout,
};
use tokio::io::AsyncWriteExt;

fn spawn_server(port: u16, data_dir: &std::path::Path) -> Child {
    spawn_server_bin(port, data_dir, &[])
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
