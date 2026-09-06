//! What the server binary does with its configuration, its signals, and its
//! socket, checked against the real process rather than the parsers.
//!
//! Every case here failed silently before: a malformed `POWDB_*` value bound
//! the default instead of refusing to start, SIGHUP killed the process with
//! no drain and no checkpoint, a password rotated by `powdb-cli` against a
//! running server kept the old one working, a server at its connection
//! ceiling ignored SIGTERM, and the Unix socket was world-connectable.

#![cfg(unix)]

mod common;

use common::{
    encode_connect_user, read_response_message, send_sigterm, spawn_server_bound,
    spawn_server_bound_env, wait_for_bind, wait_with_timeout,
};
use powdb_auth::UserStore;
use powdb_server::protocol::Message;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// Run the server binary to completion with `env` set, returning
/// `(exit code, stdout, stderr)`. Used for the paths that must refuse to
/// start, which return promptly on their own.
fn run_to_completion(args: &[&str], env: &[(&str, &str)]) -> (Option<i32>, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_powdb-server"));
    // `output()` waits, so the platform's first-exec scan is paid inline here
    // rather than against a deadline.
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run powdb-server");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Every `POWDB_*` value that used to be silently defaulted must refuse
/// startup, naming the variable and the value.
#[test]
fn a_malformed_env_value_refuses_startup() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().to_str().unwrap();
    let cases = [
        ("POWDB_PORT", "abc"),
        ("POWDB_IDLE_TIMEOUT", "abc"),
        ("POWDB_IDLE_TIMEOUT", "0"),
        ("POWDB_QUERY_TIMEOUT", "abc"),
        ("POWDB_QUERY_TIMEOUT", "0"),
        ("POWDB_TX_WAIT_TIMEOUT_MS", "abc"),
        ("POWDB_TX_MAX_LIFETIME_MS", "abc"),
        ("POWDB_SYNC_MODE", "bogus"),
        ("POWDB_QUERY_MEMORY_LIMIT", "64MiB"),
        ("POWDB_QUERY_MEMORY_LIMIT", "-1"),
        ("POWDB_MAX_NESTED_LOOP_PAIRS", "lots"),
        ("POWDB_DIRTY_PAGE_BUDGET", "1G"),
        ("POWDB_READONLY", "ture"),
        ("POWDB_REQUIRE_TLS", "ture"),
        ("POWDB_MAX_CONNECTIONS", "0"),
        ("POWDB_SHUTDOWN_TIMEOUT", "abc"),
    ];
    let mut accepted = Vec::new();
    for (name, value) in cases {
        let (code, _, stderr) = run_to_completion(
            &["--data-dir", data_dir, "--port", "0", "--bind", "127.0.0.1"],
            &[(name, value)],
        );
        if code != Some(2) || !stderr.contains(name) || !stderr.contains(value) {
            accepted.push(format!(
                "  {name}={value} exited {code:?} with stderr: {}",
                stderr.trim()
            ));
        }
    }
    assert!(
        accepted.is_empty(),
        "these malformed values did not refuse startup with a message naming the \
         variable and the value:\n{}",
        accepted.join("\n")
    );
}

/// `--help` has to list the settings an operator has to know about, including
/// the two that create a user out of the environment.
#[test]
fn help_lists_the_undocumented_settings() {
    let (code, stdout, _) = run_to_completion(&["--help"], &[]);
    assert_eq!(code, Some(0));
    for expected in [
        "POWDB_ADMIN_USER",
        "POWDB_ADMIN_PASSWORD",
        "POWDB_MAX_CONNECTIONS",
        "--max-connections",
        "--shutdown-timeout",
        "POWDB_SHUTDOWN_TIMEOUT",
        "NO_COLOR",
        "SIGHUP",
    ] {
        assert!(
            stdout.contains(expected),
            "--help does not mention {expected}:\n{stdout}"
        );
    }
    assert!(
        stdout.contains("0 is not a way to disable"),
        "--help must say what 0 means for the timeouts:\n{stdout}"
    );
}

/// The refusal has to name the right unit. Every count budget shared one
/// message that said "bytes", so an operator who mistyped a CONNECTION ceiling
/// was told to give a byte count.
#[test]
fn a_refusal_names_the_unit_the_setting_is_actually_in() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().to_str().unwrap();
    let args = ["--data-dir", data_dir, "--port", "0", "--bind", "127.0.0.1"];

    let (code, _, stderr) = run_to_completion(&args, &[("POWDB_MAX_CONNECTIONS", "64MiB")]);
    assert_eq!(code, Some(2));
    assert!(
        !stderr.contains("bytes"),
        "a connection ceiling is not a byte count: {stderr}"
    );
    assert!(
        stderr.contains("connections"),
        "the message does not say what the value counts: {stderr}"
    );

    // The budgets that really are in bytes still say so.
    let (code, _, stderr) = run_to_completion(&args, &[("POWDB_QUERY_MEMORY_LIMIT", "64MiB")]);
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("bytes"),
        "a memory budget must still be described in bytes: {stderr}"
    );
}

/// A log that nobody is reading as a terminal must carry no ANSI escapes.
#[test]
fn no_ansi_escapes_when_stdout_is_not_a_tty() {
    let dir = tempfile::tempdir().unwrap();
    let (_, stdout, stderr) = run_to_completion(
        &[
            "--data-dir",
            dir.path().to_str().unwrap(),
            "--port",
            "abc",
            "--bind",
            "127.0.0.1",
        ],
        &[],
    );
    // A refusal is enough output to see the formatting; a successful start
    // would have to be killed, and the point is the escape bytes either way.
    assert!(
        !stdout.contains('\u{1b}') && !stderr.contains('\u{1b}'),
        "ANSI escapes reached a piped stdout/stderr"
    );
}

fn store_with(dir: &std::path::Path, name: &str, password: &str) -> UserStore {
    let mut store = UserStore::new();
    store.create_user(name, password, "admin").unwrap();
    store.save(dir).unwrap();
    store
}

/// Attempt a CONNECT and report whether the server accepted it.
async fn login_succeeds(port: u16, user: &str, password: &str) -> bool {
    let mut stream = wait_for_bind(port, Duration::from_secs(30)).await;
    stream
        .write_all(&encode_connect_user("test", password, user))
        .await
        .unwrap();
    matches!(
        read_response_message(&mut stream).await,
        Message::ConnectOk { .. } | Message::ConnectOkWithHello { .. }
    )
}

/// A password rotated against a running server takes effect on the next login
/// attempt, with no restart and no signal.
#[tokio::test]
async fn a_rotated_password_takes_effect_without_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store_with(dir.path(), "alice", "one");
    let (mut child, port) = spawn_server_bound(dir.path(), &[]);

    assert!(login_succeeds(port, "alice", "one").await, "initial login");

    store.set_password("alice", "two").unwrap();
    store.save(dir.path()).unwrap();

    assert!(
        login_succeeds(port, "alice", "two").await,
        "the rotated password must work against the running server"
    );
    assert!(
        !login_succeeds(port, "alice", "one").await,
        "the old password must stop working against the running server"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// A deleted user cannot log in again, with no restart.
#[tokio::test]
async fn a_deleted_user_cannot_log_in_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store_with(dir.path(), "bob", "pw");
    store.create_user("carol", "pw", "admin").unwrap();
    store.save(dir.path()).unwrap();
    let (mut child, port) = spawn_server_bound(dir.path(), &[]);

    assert!(login_succeeds(port, "bob", "pw").await, "initial login");

    store.delete_user("bob").unwrap();
    store.save(dir.path()).unwrap();

    assert!(
        !login_succeeds(port, "bob", "pw").await,
        "a user deleted against the running server must be refused"
    );
    assert!(
        login_succeeds(port, "carol", "pw").await,
        "deleting one user must not lock out the others"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// SIGHUP reloads `auth.json` and leaves the server serving; it used to kill
/// the process outright, with no drain and no checkpoint.
#[tokio::test]
async fn sighup_reloads_users_and_keeps_the_server_running() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store_with(dir.path(), "dave", "one");
    let (mut child, port) = spawn_server_bound(dir.path(), &[]);
    assert!(login_succeeds(port, "dave", "one").await, "initial login");

    store.set_password("dave", "two").unwrap();
    store.save(dir.path()).unwrap();

    let status = Command::new("kill")
        .arg("-HUP")
        .arg(child.id().to_string())
        .status()
        .expect("invoke kill");
    assert!(status.success(), "kill -HUP failed");

    // Give the signal handler a moment, then prove the process is still alive
    // and serving the reloaded credential.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "SIGHUP terminated the server instead of reloading"
    );
    assert!(
        login_succeeds(port, "dave", "two").await,
        "the reloaded password must work after SIGHUP"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// A server at its connection ceiling must still honour SIGTERM: the accept
/// loop used to park on the connection semaphore outside the shutdown select,
/// so a saturated server ignored the signal entirely.
#[tokio::test]
async fn shutdown_drains_a_saturated_server() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, port) = spawn_server_bound(dir.path(), &["--max-connections", "1"]);

    // One connection takes the only slot and stays open.
    let mut held = wait_for_bind(port, Duration::from_secs(30)).await;
    held.write_all(&common::encode_connect("test"))
        .await
        .unwrap();
    let _ = read_response_message(&mut held).await;

    // A second peer connects; the accept loop is now parked waiting for a slot.
    let _queued = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("second connect");

    send_sigterm(&child);
    // The held connection is told the server is going away and closes, so the
    // drain completes well inside the budget.
    let status = wait_with_timeout(&mut child, Duration::from_secs(20));
    assert!(
        status.success(),
        "a saturated server must still drain on SIGTERM, exited with {status:?}"
    );
    drop(held);
}

/// The Unix socket carries the same access as the data directory.
#[tokio::test]
async fn the_unix_socket_is_owner_and_group_only() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("powdb.sock");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let (mut child, port) =
        spawn_server_bound_env(&data, &["--socket", socket.to_str().unwrap()], &[]);
    let _ = wait_for_bind(port, Duration::from_secs(30)).await;
    // The socket is bound before the port file is written, so it exists here.
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(
        mode, 0o660,
        "the unix socket is {mode:o}; any local user could connect to a server whose \
         data directory is 0700"
    );
}
