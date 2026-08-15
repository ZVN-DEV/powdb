//! Argument-parsing robustness for the built CLI binary.
//!
//! Regression (BUG 3): `powdb-cli --db $'\xff\xfe'` used to panic inside
//! `std::env::args()` (it unwraps the OsString→String conversion) before any
//! network or disk I/O. It must now exit cleanly with a non-panic error.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_powdb-cli")
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "powdb_argparse_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[cfg(unix)]
#[test]
fn non_utf8_db_arg_exits_cleanly_without_panic() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    // `\xff\xfe` is not valid UTF-8.
    let bad = OsStr::from_bytes(&[0xff, 0xfe]);
    let output = Command::new(bin())
        .arg("-r")
        .arg("localhost:5432")
        .arg("--db")
        .arg(bad)
        .output()
        .expect("spawn powdb-cli");

    // Exit code 2 (usage error), not a panic-driven exit.
    assert_eq!(
        output.status.code(),
        Some(2),
        "expected clean exit code 2, got {:?}",
        output.status.code()
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "must not panic on non-UTF-8 arg, stderr was: {stderr}"
    );
    assert!(
        stderr.contains("not valid UTF-8"),
        "expected a UTF-8 error message, stderr was: {stderr}"
    );
}

/// A mistyped subcommand must be an error, not a silent data directory.
///
/// `powdb-cli -d /var/lib/powdb usrs` used to ignore the explicit `-d`, treat
/// `usrs` as the data dir, create a fresh empty database there, and exit 0 —
/// the operator's real database was never opened and nothing said so.
#[test]
fn typoed_subcommand_is_rejected_instead_of_becoming_a_data_dir() {
    let cwd = tmp("typo");
    let real = cwd.join("db1");
    std::fs::create_dir_all(&real).unwrap();

    for (typo, suggestion) in [
        ("usrs", "users"),
        ("backupp", "backup"),
        ("sync-statuss", "sync-status"),
        ("useradds", "useradd"),
        ("restor", "restore"),
    ] {
        let out = Command::new(bin())
            .current_dir(&cwd)
            .args(["-d", "./db1", typo])
            .output()
            .expect("spawn powdb-cli");

        assert_eq!(
            out.status.code(),
            Some(2),
            "`{typo}` must be a usage error, got {:?}; stdout: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(typo) && stderr.contains(suggestion),
            "error must name '{typo}' and suggest '{suggestion}', got: {stderr}"
        );
        assert!(
            !cwd.join(typo).exists(),
            "`{typo}` must not create a data directory"
        );
    }
}

/// An explicit `-d` always wins: a stray positional after it is a usage error,
/// even when it is nothing like a subcommand.
#[test]
fn explicit_data_dir_is_never_overridden_by_a_positional() {
    let cwd = tmp("explicit");
    std::fs::create_dir_all(cwd.join("db1")).unwrap();

    let out = Command::new(bin())
        .current_dir(&cwd)
        .args(["-d", "./db1", "somewhere-else", "-c", "count(T)"])
        .output()
        .expect("spawn powdb-cli");

    assert_eq!(out.status.code(), Some(2), "expected a usage error");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("somewhere-else"),
        "error must name the offending value, got: {stderr}"
    );
    assert!(
        !cwd.join("somewhere-else").exists(),
        "must not create a data directory from the stray positional"
    );
}

/// The documented `powdb-cli [DATA_DIR]` form keeps working, including for a
/// directory that does not exist yet, and including a name that merely looks
/// a bit like a subcommand but is a real directory.
#[test]
fn documented_positional_data_dir_still_works() {
    let cwd = tmp("positional");

    // A fresh relative data dir, created on demand by the engine.
    let out = Command::new(bin())
        .current_dir(&cwd)
        .args(["./mydata", "-c", "type T { required id: int }"])
        .output()
        .expect("spawn powdb-cli");
    assert!(
        out.status.success(),
        "documented positional data dir failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(cwd.join("mydata").join("catalog.bin").exists());

    // A bare name one edit from `backup` is still honoured when it is an
    // existing directory: typo detection must not steal a real data dir.
    std::fs::create_dir_all(cwd.join("backups")).unwrap();
    let out = Command::new(bin())
        .current_dir(&cwd)
        .args(["backups", "-c", "type T { required id: int }"])
        .output()
        .expect("spawn powdb-cli");
    assert!(
        out.status.success(),
        "existing directory 'backups' must be usable as a data dir: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(cwd.join("backups").join("catalog.bin").exists());
}
