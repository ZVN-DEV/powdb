//! End-to-end test of the `--metrics-addr` Prometheus endpoint against the real
//! `powdb-server` binary: real queries over the wire protocol must show up in
//! `/metrics`, and the `connections_active` gauge must return to 0 once a client
//! disconnects (the RAII guard wiring in `main.rs`).
#![cfg(unix)]

mod common;

use std::process::Child;
use std::time::Duration;

use common::{
    encode_connect, encode_query, read_response, spawn_server_bound_with_metrics, wait_for_bind,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn spawn_server(data_dir: &std::path::Path) -> (Child, u16, u16) {
    spawn_server_bound_with_metrics(data_dir, &[], &[])
}

/// One-shot HTTP GET against the metrics endpoint; returns the full response.
async fn scrape(port: u16, path: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).await.unwrap();
    resp
}

#[tokio::test]
async fn metrics_endpoint_reflects_real_queries_and_gauge_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, port, metrics_port) = spawn_server(tmp.path());
    let mut wire = wait_for_bind(port, Duration::from_secs(20)).await;
    let _ = wait_for_bind(metrics_port, Duration::from_secs(20)).await;

    // Run real traffic over the wire protocol.
    wire.write_all(&encode_connect("testdb")).await.unwrap();
    assert_eq!(read_response(&mut wire).await[0], 0x02, "CONNECT_OK");
    wire.write_all(&encode_query("type Note { required body: str }"))
        .await
        .unwrap();
    let _ = read_response(&mut wire).await;
    wire.write_all(&encode_query(r#"insert Note { body := "x" }"#))
        .await
        .unwrap();
    assert_eq!(read_response(&mut wire).await[0], 0x09, "insert RESULT_OK");

    // Unknown path → 404; /metrics → 200 with the expected families.
    let four04 = scrape(metrics_port, "/nope").await;
    assert!(four04.starts_with("HTTP/1.1 404"), "resp: {four04}");

    let body = scrape(metrics_port, "/metrics").await;
    assert!(body.starts_with("HTTP/1.1 200 OK"), "resp head: {body}");
    assert!(body.contains("Content-Type: text/plain; version=0.0.4"));
    assert!(body.contains("powdb_build_info{version=\""));
    assert!(body.contains("powdb_query_duration_seconds_bucket{le=\"+Inf\"}"));
    // The create + insert both succeeded → at least 2 ok queries recorded.
    let ok_line = body
        .lines()
        .find(|l| l.starts_with("powdb_queries_total{result=\"ok\"}"))
        .expect("queries_total ok line");
    let ok_count: u64 = ok_line.rsplit(' ').next().unwrap().parse().unwrap();
    assert!(ok_count >= 2, "expected >=2 ok queries, got {ok_count}");
    // One wire connection is currently open → active >= 1.
    assert!(
        body.contains("powdb_connections_active 1") || body.contains("powdb_connections_active 2"),
        "active gauge missing/odd: {body}"
    );

    // Drop the wire client; the active gauge must return to 0 (RAII dec).
    drop(wire);
    let mut returned_to_zero = false;
    for _ in 0..40 {
        if scrape(metrics_port, "/metrics")
            .await
            .contains("powdb_connections_active 0")
        {
            returned_to_zero = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(returned_to_zero, "connections_active never returned to 0");

    let _ = child.kill();
    let _ = child.wait();
}
