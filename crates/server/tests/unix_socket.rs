//! Process-level test: the real `powdb-server` binary must accept connections
//! over a **Unix domain socket** when started with `--socket <path>`, serving
//! the same wire protocol as the TCP listener.
//!
//! UDS removes the TCP/IP stack from the same-host path (~2× lower round-trip
//! latency), which is the dominant cost for same-host clients — see
//! `docs/design/2026-06-27-beating-sqlite-latency-design.md`. The TCP listener
//! stays on unconditionally; the socket is additive.
#![cfg(unix)]

mod common;

use std::process::Child;
use std::time::Duration;

use common::{
    encode_connect, encode_query, free_port, read_response, spawn_server_bin, wait_for_socket,
};
use tokio::io::AsyncWriteExt;

fn spawn_server(port: u16, socket: &std::path::Path, data_dir: &std::path::Path) -> Child {
    spawn_server_bin(
        port,
        data_dir,
        &["--socket", socket.to_str().expect("utf-8 socket path")],
    )
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
