//! End-to-end proof that the CLI user-admin surface and `--user` authenticated
//! remote connect work together against a real `powdb-server`.

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
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "powdb_userauth_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn cli(args: &[&str]) -> std::process::Output {
    Command::new(cli_bin())
        .args(args)
        .output()
        .expect("failed to run powdb-cli")
}

/// Pick a likely-free port by binding to :0 and reading back the assigned port.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    l.local_addr().unwrap().port()
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

#[test]
fn cli_offline_user_admin_lifecycle() {
    let data = tmp("offline");
    let data_s = data.to_str().unwrap().to_string();
    std::fs::create_dir_all(&data).unwrap();

    // useradd with default role (readwrite) via POWDB_NEW_PASSWORD env.
    let out = Command::new(cli_bin())
        .args(["--data-dir", &data_s, "useradd", "bob"])
        .env("POWDB_NEW_PASSWORD", "s3cret")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "useradd via env failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("readwrite"));

    // Missing password (no flag, no env) is an error.
    let out = Command::new(cli_bin())
        .args(["--data-dir", &data_s, "useradd", "carol"])
        .env_remove("POWDB_NEW_PASSWORD")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "useradd without password should fail"
    );

    // Unknown role rejected.
    let out = cli(&[
        "--data-dir",
        &data_s,
        "useradd",
        "dave",
        "--role",
        "wizard",
        "--password",
        "x",
    ]);
    assert!(!out.status.success(), "unknown role should be rejected");

    // passwd changes the credential.
    let out = cli(&["--data-dir", &data_s, "passwd", "bob", "--password", "new"]);
    assert!(
        out.status.success(),
        "passwd failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // users lists bob + role, never a hash.
    let out = cli(&["--data-dir", &data_s, "users"]);
    assert!(out.status.success());
    let listing = String::from_utf8_lossy(&out.stdout);
    assert!(listing.contains("bob"));
    assert!(!listing.contains("$argon2"), "must not leak password hash");

    // userdel removes bob.
    let out = cli(&["--data-dir", &data_s, "userdel", "bob"]);
    assert!(
        out.status.success(),
        "userdel failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = cli(&["--data-dir", &data_s, "users"]);
    assert!(!String::from_utf8_lossy(&out.stdout).contains("bob"));
}

#[test]
fn cli_useradd_then_authenticated_connect() {
    let data = tmp("data");
    let data_s = data.to_str().unwrap().to_string();
    std::fs::create_dir_all(&data).unwrap();

    // 1. Create a user offline (no server running).
    let out = cli(&[
        "--data-dir",
        &data_s,
        "useradd",
        "alice",
        "--role",
        "readwrite",
        "--password",
        "pw",
    ]);
    assert!(
        out.status.success(),
        "useradd failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 2. `users` lists alice with her role (never a hash).
    let out = cli(&["--data-dir", &data_s, "users"]);
    assert!(out.status.success());
    let listing = String::from_utf8_lossy(&out.stdout);
    assert!(listing.contains("alice"), "users output: {listing:?}");
    assert!(listing.contains("readwrite"), "users output: {listing:?}");

    // 3. Start the server against that data dir (no POWDB_PASSWORD — users gate it).
    let Some(server) = server_bin() else {
        eprintln!("skipping: powdb-server binary not found next to powdb-cli");
        return;
    };
    let port = free_port();
    let child = Command::new(&server)
        .args(["--data-dir", &data_s, "--port", &port.to_string()])
        .env_remove("POWDB_PASSWORD")
        .env_remove("POWDB_ADMIN_USER")
        .env_remove("POWDB_ADMIN_PASSWORD")
        .spawn()
        .expect("failed to spawn powdb-server");
    let _guard = ServerGuard(child);
    wait_for_port(port);

    let addr = format!("localhost:{port}");

    // 4. Authenticated connect as alice with the right password succeeds.
    let mut ok = None;
    for _ in 0..5 {
        let out = cli(&[
            "--remote",
            &addr,
            "--user",
            "alice",
            "--password",
            "pw",
            "-c",
            "type T { required id: int }",
        ]);
        if out.status.success() {
            ok = Some(out);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let ok = ok.expect("authenticated connect as alice never succeeded");
    assert!(
        ok.status.success(),
        "alice connect failed: {}",
        String::from_utf8_lossy(&ok.stderr)
    );

    // 5. Wrong password is rejected.
    let bad = cli(&[
        "--remote",
        &addr,
        "--user",
        "alice",
        "--password",
        "WRONG",
        "-c",
        "count(T)",
    ]);
    assert!(
        !bad.status.success(),
        "wrong-password connect should fail; stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&bad.stdout),
        String::from_utf8_lossy(&bad.stderr)
    );

    // 6. Unknown user is rejected.
    let nobody = cli(&[
        "--remote",
        &addr,
        "--user",
        "nobody",
        "--password",
        "pw",
        "-c",
        "count(T)",
    ]);
    assert!(
        !nobody.status.success(),
        "unknown-user connect should fail; stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&nobody.stdout),
        String::from_utf8_lossy(&nobody.stderr)
    );
}
