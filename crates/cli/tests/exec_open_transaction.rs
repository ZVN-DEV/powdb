//! A `--exec` script that ends inside an explicit transaction discards every
//! write it made, so it must not report success.
//!
//! Both one-shot paths used to exit 0. Embedded, the engine was dropped with
//! the transaction still open and the checkpoint-on-drop logged an
//! internal-sounding ERROR; remote, the server rolled the transaction back
//! when the socket closed and said nothing. Either way a deploy script under
//! `set -e` carried on as though its inserts had landed.

use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_powdb-cli")
}

/// Locate the `powdb-server` binary next to the CLI's. A missing binary is a
/// hard failure: skipping would report a green run for a remote path that was
/// never exercised.
fn server_bin() -> std::path::PathBuf {
    let dir = std::path::Path::new(bin())
        .parent()
        .expect("the test binary has a parent directory");
    let ext = if cfg!(windows) { ".exe" } else { "" };
    let candidate = dir.join(format!("powdb-server{ext}"));
    assert!(
        candidate.exists(),
        "powdb-server is not built, so the remote half of this test would not run at all. \
         Build it with `cargo build -p powdb-server`, or run `cargo test --workspace`, \
         which builds every binary. Looked for {}",
        candidate.display()
    );
    candidate
}

/// Spawn `powdb-server` on an OS-assigned port and return it with the port it
/// bound. Probing a free port and re-binding it races every other server on
/// the machine; asking the server to report its port does not.
fn spawn_server_bound(mut cmd: Command) -> (Child, u16) {
    let port_file = std::env::temp_dir().join(format!(
        "powdb_open_tx_port_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    cmd.args(["--port", "0", "--port-file", port_file.to_str().unwrap()]);
    let mut child = cmd.spawn().expect("failed to spawn powdb-server");
    let deadline = Instant::now() + Duration::from_secs(60);
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
            "powdb-server did not publish its bound port within 60s"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

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

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "powdb_open_tx_{tag}_{}_{}",
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

fn stdout_of(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn stderr_of(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

#[test]
fn a_script_that_ends_inside_a_transaction_fails_and_says_the_writes_were_lost() {
    let dir = tmp("embedded");
    let d = dir.to_str().unwrap();
    let seed = run(&["--data-dir", d, "--exec", "type T { required id: int }"]);
    assert!(seed.status.success(), "seed failed: {}", stderr_of(&seed));

    let out = run(&["--data-dir", d, "--exec", "begin; insert T { id := 1 }"]);
    let stderr = stderr_of(&out);
    assert!(
        !out.status.success(),
        "a script that ended inside a transaction exited {:?}; every write it made was \
         discarded\nstdout: {}\nstderr: {stderr}",
        out.status.code(),
        stdout_of(&out)
    );
    assert!(
        stderr.contains("transaction still open at end of script; rolled back"),
        "the message does not say the transaction was rolled back: {stderr}"
    );

    // And the write really is gone, which is what the exit code now reports.
    let count = run(&["--data-dir", d, "--exec", "count(T)"]);
    assert!(
        count.status.success(),
        "count failed: {}",
        stderr_of(&count)
    );
    assert!(
        stdout_of(&count).contains('0'),
        "the discarded insert is still visible: {}",
        stdout_of(&count)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The complement: a script that closes its transaction is still a success.
#[test]
fn a_script_that_commits_still_succeeds() {
    let dir = tmp("committed");
    let d = dir.to_str().unwrap();
    assert!(
        run(&["--data-dir", d, "--exec", "type T { required id: int }"])
            .status
            .success()
    );

    let out = run(&[
        "--data-dir",
        d,
        "--exec",
        "begin; insert T { id := 1 }; commit",
    ]);
    assert!(
        out.status.success(),
        "a committed script must exit 0: {}",
        stderr_of(&out)
    );
    assert!(
        !stderr_of(&out).contains("transaction still open"),
        "a committed script must not be reported as leaving a transaction open: {}",
        stderr_of(&out)
    );

    let count = run(&["--data-dir", d, "--exec", "count(T)"]);
    assert!(
        stdout_of(&count).contains('1'),
        "the committed insert is missing: {}",
        stdout_of(&count)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A rollback the script asked for is not an error either.
#[test]
fn an_explicit_rollback_is_not_reported_as_an_open_transaction() {
    let dir = tmp("rolledback");
    let d = dir.to_str().unwrap();
    assert!(
        run(&["--data-dir", d, "--exec", "type T { required id: int }"])
            .status
            .success()
    );

    let out = run(&[
        "--data-dir",
        d,
        "--exec",
        "begin; insert T { id := 1 }; rollback",
    ]);
    assert!(
        out.status.success(),
        "an explicit rollback must exit 0: {}",
        stderr_of(&out)
    );
    assert!(!stderr_of(&out).contains("transaction still open"));

    let _ = std::fs::remove_dir_all(&dir);
}

/// The remote one-shot path had the same hole: the server rolls the
/// transaction back when the socket closes, and the CLI exited 0.
#[test]
fn a_remote_script_that_ends_inside_a_transaction_also_fails() {
    let dir = tmp("remote");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cmd = Command::new(server_bin());
    cmd.args(["--data-dir", dir.to_str().unwrap(), "--bind", "127.0.0.1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let (child, port) = spawn_server_bound(cmd);
    let _guard = ServerGuard(child);
    wait_for_port(port);
    let addr = format!("127.0.0.1:{port}");

    assert!(run(&["-r", &addr, "-c", "type T { required id: int }"])
        .status
        .success());

    let out = run(&["-r", &addr, "-c", "begin; insert T { id := 1 }"]);
    let stderr = stderr_of(&out);
    assert!(
        !out.status.success(),
        "a remote script that ended inside a transaction exited {:?}; the server discarded \
         every write it made\nstdout: {}\nstderr: {stderr}",
        out.status.code(),
        stdout_of(&out)
    );
    assert!(
        stderr.contains("transaction still open at end of script; rolled back"),
        "the message does not say the transaction was rolled back: {stderr}"
    );

    let count = run(&["-r", &addr, "-c", "count(T)"]);
    assert!(
        stdout_of(&count).contains('0'),
        "the discarded insert is still visible: {}",
        stdout_of(&count)
    );

    // A committed remote script is still a success.
    let ok = run(&["-r", &addr, "-c", "begin; insert T { id := 2 }; commit"]);
    assert!(
        ok.status.success(),
        "a committed remote script must exit 0: {}",
        stderr_of(&ok)
    );

    let _ = std::fs::remove_dir_all(&dir);
}
