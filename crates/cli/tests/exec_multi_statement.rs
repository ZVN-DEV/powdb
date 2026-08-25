//! Statement-aware `--exec` / `--exec-file` loading (#150). These drive the
//! built binary end-to-end: a `;` inside a string literal or `#` comment must
//! not break a statement, and `--exec-file` must load a whole PowQL dump.

use std::io::Write;
use std::net::TcpStream;
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

/// A dump whose last line is a comment must exit 0.
///
/// The writes committed either way; the one-shot path then sent the trailing
/// comment-only segment to the engine, got "expected statement, got end of
/// input", and exited 1. Any `set -e` deploy or CI step loading a schema dump
/// aborted (or paged someone) on a run that had in fact fully succeeded, and
/// ending a dump with a comment is completely normal.
#[test]
fn exec_file_ending_in_a_comment_exits_zero() {
    let data = tmp("trailcomment");
    let data_s = data.to_str().unwrap();

    let dump = concat!(
        "# schema\n",
        "type X { unique auto id: int, required a: str };\n",
        "insert X { a := \"one\" };\n",
        "insert X { a := \"two\" };\n",
        "# end of dump\n",
    );
    let dump_path = tmp("trailcomment_dump").with_extension("powql");
    std::fs::create_dir_all(dump_path.parent().unwrap()).unwrap();
    std::fs::write(&dump_path, dump).unwrap();

    let out = run(&[
        "--data-dir",
        data_s,
        "--exec-file",
        dump_path.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a dump ending in a comment must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Every real statement still ran.
    let got = run(&["--data-dir", data_s, "-c", "count(X)"]);
    assert_eq!(out_str(&got).trim(), "2");
}

/// The SQL dialect has its own comment syntax, so the blank check has to use
/// the SQL lexer. `--` opens a comment in SQL and is subtraction in PowQL, so
/// asking the PowQL lexer about a SQL dump ending in `-- done` reports "not
/// blank" and the segment reaches the engine as a syntax error, exiting 1 after
/// every real statement has already committed.
#[test]
fn sql_exec_file_ending_in_a_dash_comment_exits_zero() {
    let data = tmp("sqltrailcomment");
    let data_s = data.to_str().unwrap();

    let dump = concat!(
        "-- schema\n",
        "CREATE TABLE S (id INT, k TEXT);\n",
        "INSERT INTO S (id, k) VALUES (1, 'a');\n",
        "-- end of dump\n",
    );
    let dump_path = tmp("sqltrailcomment_dump").with_extension("sql");
    std::fs::create_dir_all(dump_path.parent().unwrap()).unwrap();
    std::fs::write(&dump_path, dump).unwrap();

    let out = run(&[
        "--data-dir",
        data_s,
        "--sql",
        "--exec-file",
        dump_path.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a SQL dump ending in a -- comment must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Every real statement still ran.
    let got = run(&[
        "--data-dir",
        data_s,
        "--sql",
        "-c",
        "SELECT count(*) FROM S",
    ]);
    assert_eq!(out_str(&got).trim(), "1");

    // A `--` inside a string literal is not a comment, so this is a real
    // statement and must still execute rather than being skipped as blank.
    let lit = run(&[
        "--data-dir",
        data_s,
        "--sql",
        "-c",
        "SELECT '-- not a comment' AS lit FROM S",
    ]);
    assert_eq!(lit.status.code(), Some(0));
    assert!(
        out_str(&lit).contains("-- not a comment"),
        "a -- inside a string literal must survive as data; got: {}",
        out_str(&lit)
    );

    // A genuine SQL syntax error must still fail.
    let bad = run(&["--data-dir", data_s, "--sql", "-c", "SELEKT bogus"]);
    assert_eq!(
        bad.status.code(),
        Some(1),
        "a real SQL syntax error must still exit non-zero"
    );
}

/// A comment-only `--exec` is a no-op that exits 0, not an error. Same for a
/// leading comment: the statement after it still runs.
#[test]
fn comment_only_exec_is_a_successful_noop() {
    let data = tmp("commentonly");
    let data_s = data.to_str().unwrap();

    let out = run(&["--data-dir", data_s, "-c", "# just a comment"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "comment-only --exec must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out_str(&out).trim().is_empty(),
        "comment-only --exec must print nothing, got: {:?}",
        out_str(&out)
    );

    // A comment between two statements is skipped without eating either.
    let out = run(&[
        "--data-dir",
        data_s,
        "-c",
        "type T { required id: int };\n# a comment on its own\n; insert T { id := 1 };",
    ]);
    assert!(
        out.status.success(),
        "load with an interior comment segment failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let got = run(&["--data-dir", data_s, "-c", "count(T)"]);
    assert_eq!(out_str(&got).trim(), "1");
}

/// Skipping blank segments must not weaken real error reporting: a genuine
/// syntax error still exits non-zero, and the `;`-separator hint still fires.
#[test]
fn genuine_syntax_error_still_exits_nonzero() {
    let data = tmp("stillerrors");
    let data_s = data.to_str().unwrap();

    let out = run(&[
        "--data-dir",
        data_s,
        "-c",
        "# leading comment\nthis is not powql",
    ]);
    assert!(
        !out.status.success(),
        "a real syntax error must still fail; stdout: {}",
        out_str(&out)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Error:"),
        "expected an Error: line, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The missing-`;` hint is still produced for the shape it was written for.
    let out = run(&[
        "--data-dir",
        data_s,
        "-c",
        "type T { required id: int }\ninsert T { id := 1 }",
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("separated by `;`"),
        "missing-separator hint disappeared: {stderr}"
    );
}

/// Remote one-shot skips comment-only segments too: the guard belongs to the
/// CLI's statement splitter, not to one transport.
#[test]
fn remote_exec_ending_in_a_comment_exits_zero() {
    let Some(server) = server_bin() else {
        eprintln!("powdb-server binary not found; skipping remote test");
        return;
    };
    let data = tmp("remotecomment");
    std::fs::create_dir_all(&data).unwrap();
    let mut cmd = Command::new(server);
    cmd.args(["--data-dir", data.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let (child, port) = spawn_server_bound(cmd);
    let _guard = ServerGuard(child);
    wait_for_port(port);
    let addr = format!("127.0.0.1:{port}");

    let out = run(&[
        "-r",
        &addr,
        "-c",
        "# schema\ntype R { required id: int };\ninsert R { id := 1 };\n# end of dump",
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "remote load ending in a comment must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let got = run(&["-r", &addr, "-c", "count(R)"]);
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
    let mut cmd = Command::new(server);
    cmd.args(["--data-dir", data.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let (child, port) = spawn_server_bound(cmd);
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

/// A `;` inside a SQL comment must not end the statement. This is the dangerous
/// direction of the dialect split: `-- cleanup; DELETE FROM t` is ONE comment,
/// but a PowQL-only splitter cuts it in two and hands `DELETE FROM t` to the
/// engine as a live statement the user believed was commented out. Combined
/// with skipping blank segments, that silently destroyed data and exited 0.
#[test]
fn a_semicolon_inside_a_sql_comment_does_not_execute_its_tail() {
    let data = tmp("sqlcommentsemi");
    let data_s = data.to_str().unwrap();

    for sql in [
        "CREATE TABLE t (id INT);",
        "INSERT INTO t (id) VALUES (1);",
        "INSERT INTO t (id) VALUES (2);",
    ] {
        let out = run(&["--data-dir", data_s, "--sql", "-c", sql]);
        assert_eq!(out.status.code(), Some(0), "setup failed for `{sql}`");
    }

    // The whole line is a comment. Nothing may run.
    let out = run(&[
        "--data-dir",
        data_s,
        "--sql",
        "-c",
        "-- cleanup; DELETE FROM t",
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a comment-only line must exit 0"
    );

    let got = run(&[
        "--data-dir",
        data_s,
        "--sql",
        "-c",
        "SELECT count(*) FROM t",
    ]);
    assert_eq!(
        out_str(&got).trim(),
        "2",
        "the DELETE was inside a comment and must not have run"
    );

    // Same shape in a file, which is the migration-script case.
    let dump = concat!(
        "-- migration 003\n",
        "INSERT INTO t (id) VALUES (10);\n",
        "-- cleanup temp rows; DELETE FROM t\n",
    );
    let dump_path = tmp("sqlcommentsemi_dump").with_extension("sql");
    std::fs::create_dir_all(dump_path.parent().unwrap()).unwrap();
    std::fs::write(&dump_path, dump).unwrap();
    let out = run(&[
        "--data-dir",
        data_s,
        "--sql",
        "--exec-file",
        dump_path.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0));

    let got = run(&[
        "--data-dir",
        data_s,
        "--sql",
        "-c",
        "SELECT count(*) FROM t",
    ]);
    assert_eq!(
        out_str(&got).trim(),
        "3",
        "only the real INSERT may run; the commented DELETE must not"
    );

    // A `;` inside a string literal is data, not a boundary.
    let out = run(&[
        "--data-dir",
        data_s,
        "--sql",
        "-c",
        "INSERT INTO t (id) VALUES (4); SELECT count(*) FROM t",
    ]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out_str(&out).trim().lines().last().unwrap().trim(), "4");
}
