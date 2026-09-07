//! `powdb-cli --readonly`: opening an embedded data directory for reading.
//!
//! The server has `--readonly` and `POWDB_READONLY`, and the engine has
//! `Engine::open_read_only`, but the CLI had no way to reach it: the only way
//! to look at a snapshot or a restored backup without risking a write was to
//! start a server in front of it. The same command that opens a directory also
//! has to say something usable when the path is missing or is not a directory,
//! which it did not: those arrived as bare OS errors.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_powdb-cli")
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "powdb_ro_{tag}_{}_{}",
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

/// A read-only session serves reads and refuses writes, and the directory is
/// unchanged afterwards.
#[test]
fn readonly_serves_reads_and_refuses_writes() {
    let dir = tmp("serve");
    let d = dir.to_str().unwrap();
    let seed = run(&[
        "--data-dir",
        d,
        "--exec",
        "type T { required id: int }; insert T { id := 1 }",
    ]);
    assert!(seed.status.success(), "seed failed: {}", stderr_of(&seed));

    let read = run(&["--data-dir", d, "--readonly", "--exec", "count(T)"]);
    assert!(
        read.status.success(),
        "a read-only session must serve reads: {}",
        stderr_of(&read)
    );
    assert!(
        stdout_of(&read).contains('1'),
        "the seeded row is missing: {}",
        stdout_of(&read)
    );

    let write = run(&[
        "--data-dir",
        d,
        "--readonly",
        "--exec",
        "insert T { id := 2 }",
    ]);
    assert!(
        !write.status.success(),
        "a read-only session must refuse a write: {}",
        stdout_of(&write)
    );
    assert!(
        stderr_of(&write).contains("readonly"),
        "the refusal does not say the session is read-only: {}",
        stderr_of(&write)
    );

    // The refused write really did not land.
    let after = run(&["--data-dir", d, "--exec", "count(T)"]);
    assert!(
        stdout_of(&after).contains('1'),
        "a refused write changed the data: {}",
        stdout_of(&after)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A missing directory is named, and not created. Creating one would hand
/// back an empty database that looks like data loss.
#[test]
fn readonly_on_a_missing_directory_names_the_path() {
    let dir = tmp("missing");
    let d = dir.to_str().unwrap();

    let out = run(&["--data-dir", d, "--readonly", "--exec", "schema"]);
    assert!(!out.status.success(), "a missing directory must be refused");
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains(d),
        "the error does not name the path: {stderr}"
    );
    assert!(
        stderr.contains("does not exist"),
        "the error does not say the directory is missing: {stderr}"
    );
    assert!(
        !dir.exists(),
        "a read-only open created the directory it was asked to read"
    );
}

/// A data dir that is a file is a different mistake and says so.
#[test]
fn a_data_dir_that_is_a_file_says_it_is_not_a_directory() {
    let path = tmp("isfile");
    std::fs::write(&path, b"not a database").unwrap();
    let p = path.to_str().unwrap();

    for args in [
        vec!["--data-dir", p, "--readonly", "--exec", "schema"],
        vec!["--data-dir", p, "--exec", "schema"],
    ] {
        let out = run(&args);
        assert!(!out.status.success(), "a file is not a data directory");
        let stderr = stderr_of(&out);
        assert!(
            stderr.contains(p) && stderr.contains("not a directory"),
            "the error does not say the path is not a directory: {stderr}"
        );
    }

    let _ = std::fs::remove_file(&path);
}

/// Read-only is a property of the open, so it is the server's call in remote
/// mode. Accepting the flag there would suggest the CLI was enforcing
/// something it cannot enforce.
#[test]
fn readonly_with_remote_is_refused_and_says_where_it_belongs() {
    let out = run(&["--remote", "127.0.0.1:1", "--readonly", "--exec", "schema"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "--readonly with --remote must be a usage error: {}",
        stderr_of(&out)
    );
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("POWDB_READONLY") || stderr.contains("server"),
        "the error does not say the server decides: {stderr}"
    );
}

/// The flag is discoverable.
#[test]
fn help_documents_readonly() {
    let out = run(&["--help"]);
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("--readonly"),
        "--help does not mention --readonly:\n{stdout}"
    );
}
