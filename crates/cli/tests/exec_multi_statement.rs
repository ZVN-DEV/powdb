//! Statement-aware `--exec` / `--exec-file` loading (#150). These drive the
//! built binary end-to-end: a `;` inside a string literal or `#` comment must
//! not break a statement, and `--exec-file` must load a whole PowQL dump.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_powdb-cli")
}

/// Locate `powdb-server` next to the CLI binary (workspace binaries share a
/// target dir); skip the remote test rather than fail if it is absent.
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
        "powdb_execmulti_{tag}_{}_{}",
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

fn out_str(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

/// The exact #150 repro: a string value containing `;` must load intact, not
/// break the statement with an "unterminated string" error.
#[test]
fn semicolon_in_string_loads_intact() {
    let data = tmp("semi");
    let data_s = data.to_str().unwrap();

    let out = run(&[
        "--data-dir",
        data_s,
        "-c",
        r#"type Note { required id: int, required body: str }; insert Note { id := 1, body := "hello; world" }"#,
    ]);
    assert!(
        out.status.success(),
        "multi-statement load failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let got = run(&["--data-dir", data_s, "-c", "Note { .body }"]);
    assert!(got.status.success());
    assert!(
        out_str(&got).contains("hello; world"),
        "body not stored intact: {}",
        out_str(&got)
    );
}

/// Multi-statement `--exec` with a `#` comment (containing a `;`) and a
/// trailing `;` — all statements run, the trailing empty segment is dropped.
#[test]
fn multi_statement_with_comment_and_trailing_semicolon() {
    let data = tmp("comment");
    let data_s = data.to_str().unwrap();

    let script = "type T { required id: int } # setup; done\n; \
                  insert T { id := 1 }; insert T { id := 2 };";
    let out = run(&["--data-dir", data_s, "-c", script]);
    assert!(
        out.status.success(),
        "load failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let got = run(&["--data-dir", data_s, "-c", "count(T)"]);
    assert_eq!(out_str(&got).trim(), "2");
}

/// A document-shaped dump loaded through `--exec-file`: uuid PK, prose with
/// `;`/newlines, JSON-in-text, `\x` bytes, `# WARN:` comments, trailing `;`.
#[test]
fn exec_file_document_shaped_dump() {
    let data = tmp("file");
    let data_s = data.to_str().unwrap();

    let dump = concat!(
        "type Doc { required id: uuid, required title: str, required meta: str, required blob: bytes };\n",
        "# WARN: prose below contains semicolons; newlines; and JSON braces\n",
        "insert Doc {\n",
        "  id := uuid(\"550e8400-e29b-41d4-a716-446655440000\"),\n",
        "  title := \"Report; Q3\",\n",
        "  meta := \"{\\\"tags\\\": [\\\"a;b\\\", \\\"c\\\"]}\",\n",
        "  blob := \"\\\\xdeadbeef\"\n",
        "};\n",
        "insert Doc {\n",
        "  id := uuid(\"00000000-0000-0000-0000-000000000002\"),\n",
        "  title := \"Second\",\n",
        "  meta := \"line1;\nline2\",\n",
        "  blob := \"\\\\xcafe\"\n",
        "};\n",
    );

    let dump_path = tmp("dump").with_extension("powql");
    std::fs::create_dir_all(dump_path.parent().unwrap()).unwrap();
    std::fs::write(&dump_path, dump).unwrap();

    let out = run(&[
        "--data-dir",
        data_s,
        "--exec-file",
        dump_path.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "exec-file load failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let got = run(&["--data-dir", data_s, "-c", "count(Doc)"]);
    assert_eq!(out_str(&got).trim(), "2", "both rows must load");

    let lookup = run(&[
        "--data-dir",
        data_s,
        "-c",
        r#"Doc filter .id = uuid("550e8400-e29b-41d4-a716-446655440000") { .title }"#,
    ]);
    assert!(lookup.status.success());
    assert!(
        out_str(&lookup).contains("Report; Q3"),
        "uuid lookup missed the prose row: {}",
        out_str(&lookup)
    );
}

/// `--exec-file -` reads PowQL from stdin.
#[test]
fn exec_file_stdin() {
    let data = tmp("stdin");
    let data_s = data.to_str().unwrap();

    let mut child = Command::new(bin())
        .args(["--data-dir", data_s, "--exec-file", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"type T { required id: int }; insert T { id := 7 }")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "stdin load failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let got = run(&["--data-dir", data_s, "-c", "count(T)"]);
    assert_eq!(out_str(&got).trim(), "1");
}

/// A missing `--exec-file` path exits non-zero with a clean error.
#[test]
fn exec_file_missing_path_errors() {
    let data = tmp("missing");
    let out = run(&[
        "--data-dir",
        data.to_str().unwrap(),
        "--exec-file",
        "/no/such/file.powql",
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("failed to read"));
}

/// `--exec` and `--exec-file` are mutually exclusive.
#[test]
fn exec_and_exec_file_mutually_exclusive() {
    let data = tmp("mutex");
    let out = run(&[
        "--data-dir",
        data.to_str().unwrap(),
        "-c",
        "count(T)",
        "--exec-file",
        "-",
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("mutually exclusive"));
}

/// The interactive REPL must fail cleanly (clean "Error:" line + exit 1) when
/// the engine cannot open its data dir — not with a raw Rust panic. Regression
/// for the `.expect("failed to initialize engine")` in `run_embedded`.
#[test]
fn repl_engine_open_failure_exits_cleanly() {
    // A regular file where a directory component must be: create_dir_all fails
    // with NotADirectory, exercising the engine-open error path.
    let file = tmp("replfail");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"x").unwrap();
    let data_dir = file.join("sub");

    let child = Command::new(bin())
        .args(["--data-dir", data_dir.to_str().unwrap()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let out = child.wait_with_output().unwrap();

    assert_eq!(out.status.code(), Some(1), "expected clean exit code 1");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Error: failed to initialize engine"),
        "expected clean engine-open error, got: {stderr}"
    );
    assert!(!stderr.contains("panicked"), "must not panic: {stderr}");
}

/// Remote `--exec` splits client-side and sends one `Query` message per
/// statement: a `;`-in-string multi-statement script loads intact over the
/// wire, and a bad statement stops the run before later statements execute.
#[test]
fn remote_exec_multi_statement_and_stop_on_error() {
    let Some(server) = server_bin() else {
        eprintln!("powdb-server binary not found; skipping remote test");
        return;
    };
    let data = tmp("remote");
    std::fs::create_dir_all(&data).unwrap();
    let port = free_port();
    let child = Command::new(server)
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "--port",
            &port.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn powdb-server");
    let _guard = ServerGuard(child);
    wait_for_port(port);
    let addr = format!("127.0.0.1:{port}");

    let out = run(&[
        "-r",
        &addr,
        "-c",
        r#"type Note { required id: int, required body: str }; insert Note { id := 1, body := "hello; world" }; Note { .body }"#,
    ]);
    assert!(
        out.status.success(),
        "remote multi-statement load failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out_str(&out).contains("hello; world"),
        "body not stored intact: {}",
        out_str(&out)
    );

    let out = run(&[
        "-r",
        &addr,
        "-c",
        r#"insert Note { id := "bad" }; insert Note { id := 2, body := "x" }"#,
    ]);
    assert!(!out.status.success(), "run must fail on the bad statement");

    let got = run(&["-r", &addr, "-c", "count(Note)"]);
    assert_eq!(
        out_str(&got).trim(),
        "1",
        "statement after the remote error must not run"
    );
}

/// Stop-on-first-error: a bad middle statement aborts the run and later
/// statements do not execute.
#[test]
fn stop_on_first_error() {
    let data = tmp("stop");
    let data_s = data.to_str().unwrap();

    let out = run(&[
        "--data-dir",
        data_s,
        "-c",
        "type T { required id: int }; insert T { id := 1 }; insert T { id := \"bad\" }; insert T { id := 2 }",
    ]);
    assert!(!out.status.success(), "run must fail on the bad statement");

    // Only the first insert committed; the statement after the error never ran.
    let got = run(&["--data-dir", data_s, "-c", "count(T)"]);
    assert_eq!(out_str(&got).trim(), "1");
}
