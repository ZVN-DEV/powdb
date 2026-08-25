//! End-to-end proof that the CLI's remote TLS support (`--tls`, `--tls-ca`,
//! `--tls-server-name`) works against a real TLS-enabled `powdb-server`, and
//! that the failure paths (plaintext client, untrusted cert, bad CA path)
//! fail cleanly instead of hanging or panicking.
//!
//! Certificate generation copies the rcgen technique from
//! `crates/server/tests/tls_connection.rs`.

use std::net::TcpStream;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

fn cli_bin() -> &'static str {
    env!("CARGO_BIN_EXE_powdb-cli")
}

/// Locate the `powdb-server` binary. It is not in this crate, so
/// `CARGO_BIN_EXE_powdb-server` is unavailable; instead derive it from the CLI
/// binary's directory (workspace binaries share a target dir). If it cannot be
/// found, the test is skipped rather than failing (e.g. when only the CLI was
/// built in isolation).
fn server_bin() -> Option<std::path::PathBuf> {
    let cli = std::path::Path::new(cli_bin());
    let dir = cli.parent()?;
    let ext = if cfg!(windows) { ".exe" } else { "" };
    let candidate = dir.join(format!("powdb-server{ext}"));
    candidate.exists().then_some(candidate)
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "powdb_tlscli_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// Run the CLI hermetically: scrub every TLS/auth env fallback so the flags
/// under test are the only inputs.
fn cli(args: &[&str]) -> std::process::Output {
    Command::new(cli_bin())
        .args(args)
        .env_remove("POWDB_TLS")
        .env_remove("POWDB_TLS_CA")
        .env_remove("POWDB_TLS_SERVER_NAME")
        .env_remove("POWDB_PASSWORD")
        .output()
        .expect("failed to run powdb-cli")
}

/// Spawn `powdb-server` on an OS-assigned port (`--port 0` + `--port-file`)
/// and return the child with the port it actually bound. Probing a free port
/// and re-binding it races every other concurrently spawned server on the
/// machine; asking the server to report its port does not.
fn spawn_server_bound(mut cmd: Command) -> (Child, u16) {
    let port_file = std::env::temp_dir().join(format!(
        "powdb_cli_ports_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    cmd.args(["--port", "0", "--port-file", port_file.to_str().unwrap()]);
    let mut child = cmd.spawn().expect("failed to spawn powdb-server");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(text) = std::fs::read_to_string(&port_file) {
            if let Some(port) = text
                .lines()
                .find_map(|l| l.strip_prefix("port=")?.parse::<u16>().ok())
            {
                let _ = std::fs::remove_file(&port_file);
                return (child, port);
            }
        }
        if let Ok(Some(status)) = child.try_wait() {
            let _ = std::fs::remove_file(&port_file);
            panic!("powdb-server exited before publishing its bound port: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "powdb-server did not publish its bound port within 30s"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Wait until the server is accepting connections (or panic after a timeout).
fn wait_for_port(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("server did not start listening on port {port}");
}

struct ServerGuard(Child);
impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Generate a self-signed certificate for `localhost` and write the PEM pair
/// into `dir`, returning (cert_path, key_path). Same rcgen technique as
/// `crates/server/tests/tls_connection.rs`.
fn write_localhost_certs(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    std::fs::create_dir_all(dir).unwrap();
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, signing_key.serialize_pem()).unwrap();
    (cert_path, key_path)
}

fn stderr_str(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

#[test]
fn cli_tls_against_tls_required_server() {
    let Some(server) = server_bin() else {
        eprintln!("skipping: powdb-server binary not found next to powdb-cli");
        return;
    };

    let data = tmp("data");
    let data_s = data.to_str().unwrap().to_string();
    let cert_dir = tmp("certs");
    let (cert_path, key_path) = write_localhost_certs(&cert_dir);
    let cert_s = cert_path.to_str().unwrap().to_string();

    // Spawn the real server with TLS configured through the documented env
    // vars. With a cert + key set, every connection must complete a TLS
    // handshake, so this server is unreachable for a plaintext client.
    let mut cmd = Command::new(&server);
    cmd.args(["--data-dir", &data_s])
        .env("POWDB_TLS_CERT", &cert_s)
        .env("POWDB_TLS_KEY", key_path.to_str().unwrap())
        .env_remove("POWDB_PASSWORD")
        .env_remove("POWDB_ADMIN_USER")
        .env_remove("POWDB_ADMIN_PASSWORD");
    let (child, port) = spawn_server_bound(cmd);
    let _guard = ServerGuard(child);
    wait_for_port(port);

    let addr = format!("localhost:{port}");

    // 1. TLS CLI with the self-signed cert as trust root: full round trip.
    // Retried briefly because the port can accept before the engine is ready.
    let mut ok = None;
    for _ in 0..5 {
        let out = cli(&[
            "--remote",
            &addr,
            "--tls",
            "--tls-ca",
            &cert_s,
            "-c",
            r#"type TlsT { required id: int }; insert TlsT { id := 7 }; count(TlsT)"#,
        ]);
        if out.status.success() {
            ok = Some(out);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = ok.expect("TLS CLI round trip never succeeded");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains('1'),
        "expected count(TlsT) == 1 in output: {stdout}"
    );

    // 2. The certificate names `localhost`, so connecting by IP works only
    // with the --tls-server-name override (the classic IP-vs-hostname case).
    let ip_addr = format!("127.0.0.1:{port}");
    let out = cli(&[
        "--remote",
        &ip_addr,
        "--tls",
        "--tls-ca",
        &cert_s,
        "--tls-server-name",
        "localhost",
        "-c",
        "count(TlsT)",
    ]);
    assert!(
        out.status.success(),
        "--tls-server-name override failed: {}",
        stderr_str(&out)
    );

    // ...and without the override the handshake is refused (name mismatch),
    // with a clear error instead of a hang or a protocol-level failure.
    let out = cli(&[
        "--remote",
        &ip_addr,
        "--tls",
        "--tls-ca",
        &cert_s,
        "-c",
        "count(TlsT)",
    ]);
    assert!(!out.status.success(), "hostname mismatch must fail");
    assert!(
        stderr_str(&out).contains("TLS handshake"),
        "expected a TLS handshake error, got: {}",
        stderr_str(&out)
    );

    // 3. TLS without --tls-ca: the self-signed cert is not in the webpki
    // root store, so verification fails cleanly.
    let out = cli(&["--remote", &addr, "--tls", "-c", "count(TlsT)"]);
    assert!(!out.status.success(), "untrusted cert must fail");
    assert!(
        stderr_str(&out).contains("TLS handshake"),
        "expected a TLS handshake error, got: {}",
        stderr_str(&out)
    );

    // 4. A bad CA path fails with a clear error before any protocol traffic.
    let out = cli(&[
        "--remote",
        &addr,
        "--tls",
        "--tls-ca",
        "/nonexistent/ca.pem",
        "-c",
        "count(TlsT)",
    ]);
    assert!(!out.status.success(), "bad CA path must fail");
    assert!(
        stderr_str(&out).contains("failed to open TLS CA file"),
        "expected a CA file error, got: {}",
        stderr_str(&out)
    );

    // 5. A plaintext CLI (no --tls) against the TLS-required server fails
    // cleanly: the server expects a TLS handshake, so the CONNECT handshake
    // cannot complete.
    let out = cli(&["--remote", &addr, "-c", "count(TlsT)"]);
    assert!(
        !out.status.success(),
        "plaintext CLI against a TLS server must fail"
    );
    assert!(
        stderr_str(&out).contains("Error:"),
        "expected a clean error message, got: {}",
        stderr_str(&out)
    );

    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&cert_dir);
}
