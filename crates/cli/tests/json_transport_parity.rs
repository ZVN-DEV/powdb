//! `--format json` must not depend on the transport.
//!
//! The CLI sells `--format json` as the scriptable output ("json and csv make
//! the CLI scriptable") with no transport caveat. Remote results used to come
//! back on the legacy stringly-typed `Query` frame, so the same query over the
//! same data rendered `[[1,1.5,true,"x"]]` embedded and `[["1","1.5","true",
//! "x"]]` remotely: a `jq` numeric comparison written against an embedded run
//! silently stopped matching the moment it was pointed at a server.
//!
//! These tests run the same queries over the same data directory through both
//! transports and demand byte-identical JSON.

use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_powdb-cli")
}

/// Locate `powdb-server` next to the CLI binary (workspace binaries share a
/// target dir); skip rather than fail if it is absent.
fn server_bin() -> Option<std::path::PathBuf> {
    let dir = std::path::Path::new(bin()).parent()?;
    let ext = if cfg!(windows) { ".exe" } else { "" };
    let candidate = dir.join(format!("powdb-server{ext}"));
    candidate.exists().then_some(candidate)
}

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    l.local_addr().unwrap().port()
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
        "powdb_jsonparity_{tag}_{}_{}",
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

/// stdout of a successful run, trimmed. Panics with stderr on failure so a
/// broken query never silently compares as an empty string.
fn stdout_of(args: &[&str]) -> String {
    let out = run(args);
    assert!(
        out.status.success(),
        "powdb-cli {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Every JSON-visible type in one row: int, float, bool, str, NULL, and an
/// i64 past 2^53 (which must keep every digit, not round-trip through a
/// double).
const SEED: &str = concat!(
    "type J { unique auto id: int, required f: float, required b: bool, ",
    "required s: str, required big: int, n: str };",
    "insert J { f := 1.5, b := true, s := \"x\", big := 9007199254740993 }"
);
const PROJECTION: &str = "J { .id, .f, .b, .s, .big, .n }";

#[test]
fn embedded_and_remote_json_are_identical() {
    let Some(server) = server_bin() else {
        eprintln!("powdb-server binary not found; skipping transport parity test");
        return;
    };

    let data = tmp("parity");
    let data_s = data.to_str().unwrap().to_string();
    let _ = stdout_of(&["--data-dir", &data_s, "-c", SEED]);

    // Embedded first: the CLI takes an exclusive lock on the data dir and
    // releases it on exit, so the server can take it over afterwards.
    let embedded_rows = stdout_of(&["--data-dir", &data_s, "--format", "json", "-c", PROJECTION]);
    let embedded_scalar = stdout_of(&["--data-dir", &data_s, "--format", "json", "-c", "count(J)"]);
    let embedded_csv = stdout_of(&["--data-dir", &data_s, "--format", "csv", "-c", PROJECTION]);
    let embedded_sql = stdout_of(&[
        "--data-dir",
        &data_s,
        "--sql",
        "--format",
        "json",
        "-c",
        "SELECT id, f, b FROM J",
    ]);

    // The embedded baseline is the contract the docs describe: ints and floats
    // are JSON numbers, bools are JSON bools, NULL is JSON null.
    assert_eq!(
        embedded_rows,
        r#"{"columns":["id","f","b","s","big","n"],"rows":[[1,1.5,true,"x",9007199254740993,null]]}"#,
        "embedded JSON is not the typed shape the docs promise"
    );
    assert_eq!(embedded_scalar, r#"{"value":1}"#);

    let port = free_port();
    let child = Command::new(server)
        .args(["--data-dir", &data_s, "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn powdb-server");
    let _guard = ServerGuard(child);
    wait_for_port(port);
    let addr = format!("127.0.0.1:{port}");

    let remote_rows = stdout_of(&["-r", &addr, "--format", "json", "-c", PROJECTION]);
    let remote_scalar = stdout_of(&["-r", &addr, "--format", "json", "-c", "count(J)"]);
    let remote_csv = stdout_of(&["-r", &addr, "--format", "csv", "-c", PROJECTION]);
    let remote_sql = stdout_of(&[
        "-r",
        &addr,
        "--sql",
        "--format",
        "json",
        "-c",
        "SELECT id, f, b FROM J",
    ]);

    assert_eq!(
        remote_rows, embedded_rows,
        "remote --format json must match embedded byte for byte"
    );
    assert_eq!(
        remote_scalar, embedded_scalar,
        "remote scalar JSON must match embedded byte for byte"
    );
    assert_eq!(
        remote_sql, embedded_sql,
        "remote --sql --format json must match embedded byte for byte"
    );

    // csv (and, by the same code path, table) must keep the legacy frame and
    // the rendering it produced before typed JSON existed.
    assert_eq!(
        embedded_csv.lines().last().unwrap(),
        "1,1.5,true,x,9007199254740993,NULL"
    );
    assert_eq!(
        remote_csv, embedded_csv,
        "csv rendering must be unchanged on both transports"
    );

    // A remote write and a remote DDL keep their non-row JSON shapes.
    assert_eq!(
        stdout_of(&[
            "-r",
            &addr,
            "--format",
            "json",
            "-c",
            "insert J { f := 2.5, b := false, s := \"y\", big := 3 }"
        ]),
        r#"{"affected":1}"#
    );
    assert_eq!(
        stdout_of(&[
            "-r",
            &addr,
            "--format",
            "json",
            "-c",
            "type Z { required q: int }"
        ]),
        r#"{"message":"type Z created"}"#
    );
}
