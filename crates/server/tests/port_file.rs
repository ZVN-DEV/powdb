//! `--port 0` + `--port-file`: ask the OS for the port and have the server
//! report what it actually bound.
//!
//! Every spawn-a-server test used to probe a free port, release it, and race
//! the whole machine to rebind it (`free_port`'s TOCTOU). Ephemeral ports are
//! machine-global, so with many test binaries spawning servers concurrently a
//! collision hands one test's connection to another test's dying server — the
//! `wire_error_class_from_type` ConnectionReset flake on CI. Binding port 0
//! inside the server and reading the result back removes the race entirely.

mod common;

use std::time::{Duration, Instant};

use common::{
    encode_connect, read_response, spawn_server_bin_env, wait_for_bind, wait_with_timeout,
};
use tokio::io::AsyncWriteExt;

/// Poll `path` until the server has published its ports, then parse them.
/// Returns (port, metrics_port).
fn read_port_file(path: &std::path::Path, within: Duration) -> (u16, Option<u16>) {
    let start = Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if text.ends_with('\n') {
                let mut port = None;
                let mut metrics = None;
                for line in text.lines() {
                    if let Some(v) = line.strip_prefix("port=") {
                        port = v.parse::<u16>().ok();
                    } else if let Some(v) = line.strip_prefix("metrics=") {
                        metrics = v.parse::<u16>().ok();
                    }
                }
                if let Some(port) = port {
                    return (port, metrics);
                }
            }
        }
        assert!(
            start.elapsed() <= within,
            "server did not publish its bound port to {path:?} within {within:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test]
async fn port_zero_with_port_file_reports_the_bound_port() {
    let tmp = tempfile::tempdir().unwrap();
    let port_file = tmp.path().join("ports");
    let mut child = spawn_server_bin_env(
        0,
        tmp.path(),
        &["--port-file", port_file.to_str().unwrap()],
        &[],
    );

    let (port, metrics) = read_port_file(&port_file, Duration::from_secs(20));
    assert_ne!(port, 0, "the published port must be the real bound port");
    assert_eq!(metrics, None, "no metrics endpoint was requested");

    let mut stream = wait_for_bind(port, Duration::from_secs(20)).await;
    stream.write_all(&encode_connect("testdb")).await.unwrap();
    assert_eq!(
        read_response(&mut stream).await[0],
        0x02,
        "expected CONNECT_OK on the published port"
    );

    let _ = child.kill();
    wait_with_timeout(&mut child, Duration::from_secs(10));
}

#[tokio::test]
async fn port_file_reports_the_metrics_port_too() {
    let tmp = tempfile::tempdir().unwrap();
    let port_file = tmp.path().join("ports");
    let mut child = spawn_server_bin_env(
        0,
        tmp.path(),
        &[
            "--port-file",
            port_file.to_str().unwrap(),
            "--metrics-addr",
            "127.0.0.1:0",
        ],
        &[],
    );

    let (port, metrics) = read_port_file(&port_file, Duration::from_secs(20));
    let metrics = metrics.expect("metrics endpoint was requested, port must be published");
    assert_ne!(port, 0);
    assert_ne!(metrics, 0);
    assert_ne!(port, metrics, "two distinct listeners, two distinct ports");

    // The published metrics port really is the metrics endpoint.
    let mut mstream = wait_for_bind(metrics, Duration::from_secs(20)).await;
    mstream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut body = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut mstream, &mut body)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("powdb_"),
        "expected Prometheus metrics on the published metrics port, got: {text}"
    );

    let _ = child.kill();
    wait_with_timeout(&mut child, Duration::from_secs(10));
}
