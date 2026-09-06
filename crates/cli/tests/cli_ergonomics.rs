//! The batch of CLI surface defects: a password on the command line, a
//! `--exec` failure that does not say which statement failed, `--help` that
//! does not work per subcommand, a REPL that rejects the `;` its own one-shot
//! mode requires, and user admin that says nothing about the server holding
//! the same data directory.

use std::io::Write;
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_powdb-cli")
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "powdb_ergo_{tag}_{}_{}",
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

/// Run with `input` on stdin.
fn run_with_stdin(args: &[&str], input: &str) -> std::process::Output {
    let mut child = Command::new(bin())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn powdb-cli");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait powdb-cli")
}

fn stdout_of(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn stderr_of(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

// ---- S8: the password on the command line ----

/// `--password` puts the secret in `ps` output for every user on the box.
/// `--password-stdin` is the way to avoid that, and the CLI has to have one.
#[test]
fn password_stdin_is_accepted_wherever_password_is() {
    let dir = tmp("pwstdin");
    let d = dir.to_str().unwrap();

    let out = run_with_stdin(
        &["--data-dir", d, "useradd", "alice", "--password-stdin"],
        "s3cret\n",
    );
    assert!(
        out.status.success(),
        "--password-stdin was not accepted: {}",
        stderr_of(&out)
    );

    let users = run(&["--data-dir", d, "users"]);
    assert!(
        stdout_of(&users).contains("alice"),
        "the user was not created: {}",
        stdout_of(&users)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The two ways of passing a password are mutually exclusive, so nobody has
/// to guess which one won.
#[test]
fn password_and_password_stdin_together_are_refused() {
    let dir = tmp("pwboth");
    let out = run_with_stdin(
        &[
            "--data-dir",
            dir.to_str().unwrap(),
            "useradd",
            "bob",
            "--password",
            "one",
            "--password-stdin",
        ],
        "two\n",
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "passing both must be a usage error: {}",
        stderr_of(&out)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// And using `--password` says why that is a bad idea, once.
#[test]
fn password_on_the_command_line_warns_about_ps() {
    let dir = tmp("pwwarn");
    let out = run(&[
        "--data-dir",
        dir.to_str().unwrap(),
        "useradd",
        "carol",
        "--password",
        "hunter2",
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("ps"),
        "nothing warned that the password is visible in ps: {stderr}"
    );
    assert!(
        !stderr.contains("hunter2"),
        "the warning printed the password it was warning about: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- S12: user admin against a running server ----

/// The offline user commands write `auth.json` under a data directory a
/// server may be holding. The server picks the change up on its next login
/// attempt, and the operator has no way to know that unless it is said.
#[test]
fn user_admin_says_the_server_will_reload() {
    let dir = tmp("livedir");
    let d = dir.to_str().unwrap();
    assert!(
        run(&["--data-dir", d, "useradd", "dave", "--password", "pw"])
            .status
            .success()
    );

    // Stand in for a live server by publishing the same LOCK file a live
    // writer publishes.
    std::fs::write(std::path::Path::new(d).join("LOCK"), "424242").unwrap();

    let out = run(&["--data-dir", d, "passwd", "dave", "--password", "pw2"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = format!("{}{}", stdout_of(&out), stderr_of(&out));
    assert!(
        text.contains("424242"),
        "nothing named the process holding the data dir: {text}"
    );
    assert!(
        text.contains("next login attempt"),
        "nothing said when the running server picks the change up: {text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- S18: which statement failed ----

/// `--exec 'a; b; c'` stopped at the first failure and printed only the
/// engine's error, which for a long dump does not say where to look.
#[test]
fn a_failing_statement_is_identified_by_position_and_text() {
    let dir = tmp("whichstmt");
    let d = dir.to_str().unwrap();
    let out = run(&[
        "--data-dir",
        d,
        "--exec",
        "type T { required id: int }; insert T { id := 1 }; insert Nope { id := 2 }; insert T { id := 3 }",
    ]);
    assert!(!out.status.success());
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("statement 3 of 4"),
        "the error does not say which statement failed: {stderr}"
    );
    assert!(
        stderr.contains("insert Nope"),
        "the error does not quote the failing statement: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- S18: per-subcommand help ----

/// `backup --help` used to try to back up ./powdb_data into a directory named
/// "--help"; `restore --help` and `sync-status --help` were "unknown
/// argument"; `sync-bootstrap --help` demanded a directory.
#[test]
fn every_subcommand_answers_its_own_help() {
    for (subcommand, expected) in [
        ("backup", "--base"),
        ("restore", "--apply"),
        ("sync-enable", "sync identity"),
        ("sync-bootstrap", "REPLICA_ID"),
        ("sync-status", "REPLICA_ID"),
        ("useradd", "--role"),
        ("userdel", "Delete a user"),
        ("passwd", "password"),
        ("users", "List users"),
        ("sweep", "overflow"),
    ] {
        let out = run(&[subcommand, "--help"]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "`{subcommand} --help` exited {:?}: {}",
            out.status.code(),
            stderr_of(&out)
        );
        let stdout = stdout_of(&out);
        assert!(
            stdout.contains(expected),
            "`{subcommand} --help` does not describe the subcommand ({expected} missing):\n{stdout}"
        );
        assert!(
            !std::path::Path::new("./powdb_data").exists(),
            "`{subcommand} --help` created a database"
        );
    }
}

// ---- S18 / S13: the REPL's own text ----

/// One-shot mode requires `;` between statements; the REPL rejected the same
/// `;` at the end of a line, so a statement copied out of a script failed.
#[test]
fn the_repl_accepts_one_trailing_semicolon() {
    let dir = tmp("semicolon");
    let d = dir.to_str().unwrap();
    let out = run_with_stdin(
        &["--data-dir", d],
        "type T { required id: int };\ninsert T { id := 1 };\ncount(T);\n",
    );
    let stderr = stderr_of(&out);
    assert!(
        !stderr.contains("Error:"),
        "a trailing `;` was rejected in the REPL: {stderr}"
    );
    assert!(
        stdout_of(&out).contains('1'),
        "the statements did not run: {}",
        stdout_of(&out)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `.help` and the `.schema` usage line disagreed about what the argument is
/// called.
#[test]
fn the_schema_usage_line_matches_the_help_text() {
    let dir = tmp("schemausage");
    let d = dir.to_str().unwrap();
    let out = run_with_stdin(&["--data-dir", d], ".help\n.schema\n");
    let text = format!("{}{}", stdout_of(&out), stderr_of(&out));
    assert!(
        text.contains(".schema <TABLE>"),
        ".help does not describe .schema as `<TABLE>`: {text}"
    );
    assert!(
        !text.contains("<TABLE_NAME>"),
        "the usage line still uses a different placeholder than .help: {text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The banner said "Type PowQL queries" in SQL mode.
#[test]
fn the_sql_repl_banner_says_sql() {
    let dir = tmp("sqlbanner");
    let d = dir.to_str().unwrap();
    let out = run_with_stdin(&["--data-dir", d, "--sql"], "");
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("Type SQL queries"),
        "the SQL banner still advertises PowQL: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
