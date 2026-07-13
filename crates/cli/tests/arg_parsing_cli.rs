//! Argument-parsing robustness for the built CLI binary.
//!
//! Regression (BUG 3): `powdb-cli --db $'\xff\xfe'` used to panic inside
//! `std::env::args()` (it unwraps the OsString→String conversion) before any
//! network or disk I/O. It must now exit cleanly with a non-panic error.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_powdb-cli")
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
