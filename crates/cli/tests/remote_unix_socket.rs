//! `powdb-cli --remote /path/to/powdb.sock`.
//!
//! The server has had `--socket` for a long time, and the CLI could not use
//! it: `--remote` parsed the path as `host:port`, failed to resolve it, and
//! said "connection failed". A Unix socket is the way to reach a server that
//! binds no TCP port at all, so the only client that could talk to such a
//! deployment was the TypeScript one.

#![cfg(unix)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_powdb-cli")
}

/// Locate `powdb-server` next to the CLI binary. A missing binary is a hard
/// failure: skipping would report a green run for a path never exercised.
fn server_bin() -> std::path::PathBuf {
    let dir = std::path::Path::new(bin())
        .parent()
        .expect("the test binary has a parent directory");
    let ext = if cfg!(windows) { ".exe" } else { "" };
    let candidate = dir.join(format!("powdb-server{ext}"));
    assert!(
        candidate.exists(),
        "powdb-server is not built, so this test would not run at all. Build it with \
         `cargo build -p powdb-server`, or run `cargo test --workspace`. Looked for {}",
        candidate.display()
    );
    candidate
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "powdb_uds_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("failed to run powdb-cli")
}

struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start a server listening on `socket` (and on an OS-assigned TCP port, so
/// the port file still reports when it is up).
fn spawn_server_on_socket(data_dir: &std::path::Path, socket: &std::path::Path) -> ServerGuard {
    let port_file = data_dir.join("port");
    let mut child = Command::new(server_bin())
        .args(["--data-dir", data_dir.to_str().unwrap()])
        .args(["--bind", "127.0.0.1", "--port", "0"])
        .args(["--port-file", port_file.to_str().unwrap()])
        .args(["--socket", socket.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn powdb-server");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if socket.exists() && port_file.exists() {
            return ServerGuard(child);
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!("powdb-server exited before binding: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "powdb-server never bound its unix socket"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_cli_can_talk_to_a_server_over_a_unix_socket() {
    let dir = tmp("server");
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("powdb.sock");
    let _server = spawn_server_on_socket(&dir, &socket);
    let path = socket.to_str().unwrap();

    let created = run(&["--remote", path, "-c", "type T { required id: int }"]);
    assert!(
        created.status.success(),
        "creating a type over a unix socket failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );

    assert!(run(&["--remote", path, "-c", "insert T { id := 7 }"])
        .status
        .success());

    let counted = run(&["--remote", path, "-c", "count(T)"]);
    assert!(
        String::from_utf8_lossy(&counted.stdout).contains('1'),
        "the row inserted over the socket is missing: {}",
        String::from_utf8_lossy(&counted.stdout)
    );
}

/// A socket that is not there says so, instead of "connection failed" with no
/// hint that the path was read as a socket at all.
#[test]
fn a_missing_socket_path_names_the_path() {
    let missing = tmp("missing").join("powdb.sock");
    let out = run(&["--remote", missing.to_str().unwrap(), "-c", "schema"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(missing.to_str().unwrap()),
        "the error does not name the socket path: {stderr}"
    );
}

/// TLS is a TCP thing. Asking for it over a socket that is already local, and
/// carries no hostname to verify, is a mistake worth naming.
#[test]
fn tls_over_a_unix_socket_is_refused() {
    let socket = tmp("tls").join("powdb.sock");
    let out = run(&[
        "--remote",
        socket.to_str().unwrap(),
        "--tls",
        "-c",
        "schema",
    ]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "TLS over a unix socket must be a usage error: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
