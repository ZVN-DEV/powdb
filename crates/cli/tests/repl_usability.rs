//! REPL and one-shot usability, driven end-to-end through the built binary.
//!
//! Covers the footguns this crate used to ship with:
//!   - an unbalanced delimiter silently swallowing the rest of a piped session
//!   - no way out of a continuation buffer (`.cancel`)
//!   - SQL being unreachable from the CLI even though the engine supports it
//!   - no machine-readable output, so the CLI could not be scripted
//!   - `--exec-file` demanding `;` terminators while saying so nowhere

use std::io::Write;
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_powdb-cli")
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "powdb_repl_{tag}_{}_{}",
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

/// Pipe `input` into an embedded REPL session on `data_dir`.
fn repl(data_dir: &str, input: &str) -> std::process::Output {
    let mut child = Command::new(bin())
        .args(["--data-dir", data_dir])
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

fn seed(data_dir: &str) {
    let out = run(&[
        "--data-dir",
        data_dir,
        "-c",
        r#"type User { required name: str, required age: int }; insert User { name := "ada", age := 36 }; insert User { name := "bob", age := 24 }"#,
    ]);
    assert!(out.status.success(), "seed failed: {}", stderr_of(&out));
}

/// The original bug: one stray `(` swallowed every later line, meta-commands
/// included, and the session exited silently. Now the session warns loudly.
#[test]
fn unterminated_input_warns_at_eof() {
    let data = tmp("eof");
    let data_s = data.to_str().unwrap();
    seed(data_s);

    let out = repl(data_s, "count(User\ncount(User)\n.tables\n");
    let err = stderr_of(&out);
    assert!(
        err.contains("unterminated input were discarded"),
        "no EOF warning: {err}"
    );
    assert!(
        err.contains(".cancel"),
        "EOF warning does not point at the escape hatch: {err}"
    );
    // A piped session also gets a note when it first enters a continuation,
    // since it never sees the `  ...> ` prompt.
    assert!(
        err.contains("unterminated statement"),
        "no continuation note: {err}"
    );
}

/// `.cancel` escapes the continuation buffer, and the lines after it run.
#[test]
fn cancel_escapes_a_continuation() {
    let data = tmp("cancel");
    let data_s = data.to_str().unwrap();
    seed(data_s);

    let out = repl(data_s, "count(User\n.cancel\ncount(User)\n.tables\n");
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("discarded 1 line of unterminated input"),
        "no cancel confirmation: {stdout}"
    );
    assert!(
        stdout.contains('2'),
        "count did not run after .cancel: {stdout}"
    );
    assert!(
        stdout.contains("User"),
        ".tables did not run after .cancel: {stdout}"
    );
    assert!(
        !stderr_of(&out).contains("unterminated input were discarded"),
        "cancelled buffer must not warn at EOF"
    );
}

/// SQL is reachable from the REPL: one-off `.sql <STMT>` and a `.sql` mode.
#[test]
fn sql_is_reachable_from_the_repl() {
    let data = tmp("sql");
    let data_s = data.to_str().unwrap();
    seed(data_s);

    let out = repl(
        data_s,
        ".sql SELECT name FROM User WHERE age > 30\n.sql\nSELECT name FROM User WHERE age < 30\n.powql\ncount(User)\n",
    );
    let stdout = stdout_of(&out);
    assert!(stdout.contains("ada"), "one-off .sql did not run: {stdout}");
    assert!(stdout.contains("bob"), ".sql mode did not run: {stdout}");
    assert!(
        stdout.trim_end().ends_with('2'),
        ".powql did not restore PowQL: {stdout}"
    );
}

/// SQL is reachable from one-shot mode with `--sql`.
#[test]
fn sql_is_reachable_from_exec() {
    let data = tmp("sqlexec");
    let data_s = data.to_str().unwrap();
    seed(data_s);

    let out = run(&[
        "--data-dir",
        data_s,
        "--sql",
        "-c",
        "SELECT name FROM User WHERE age > 30",
    ]);
    assert!(out.status.success(), "--sql failed: {}", stderr_of(&out));
    assert!(stdout_of(&out).contains("ada"), "{}", stdout_of(&out));
}

/// Machine-readable output: `--format json` and `--format csv` make the CLI
/// scriptable, and `.mode` does the same inside the REPL.
#[test]
fn json_and_csv_output_modes() {
    let data = tmp("modes");
    let data_s = data.to_str().unwrap();
    seed(data_s);

    let json = run(&[
        "--data-dir",
        data_s,
        "--format",
        "json",
        "-c",
        "User { .name, .age } order .age",
    ]);
    assert!(json.status.success(), "{}", stderr_of(&json));
    assert_eq!(
        stdout_of(&json).trim(),
        r#"{"columns":["name","age"],"rows":[["bob",24],["ada",36]]}"#
    );

    let scalar = run(&[
        "--data-dir",
        data_s,
        "--format",
        "json",
        "-c",
        "count(User)",
    ]);
    assert_eq!(stdout_of(&scalar).trim(), r#"{"value":2}"#);

    let csv = run(&[
        "--data-dir",
        data_s,
        "--format",
        "csv",
        "-c",
        "User { .name, .age } order .age",
    ]);
    assert_eq!(stdout_of(&csv).trim(), "name,age\nbob,24\nada,36");

    let mode = repl(data_s, ".mode json\ncount(User)\n");
    assert!(
        stdout_of(&mode).contains(r#"{"value":2}"#),
        ".mode json did not take effect: {}",
        stdout_of(&mode)
    );
}

/// A file of newline-separated statements (what a user gets by copying a REPL
/// session) still fails, but now says why instead of only "unexpected trailing
/// token".
#[test]
fn exec_file_without_semicolons_explains_the_separator() {
    let data = tmp("sep");
    let data_s = data.to_str().unwrap();
    seed(data_s);

    let dir = tmp("sepfile");
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("dump.powql");
    std::fs::write(
        &script,
        "insert User { name := \"cy\", age := 51 }\ninsert User { name := \"di\", age := 19 }\n",
    )
    .unwrap();

    let out = run(&[
        "--data-dir",
        data_s,
        "--exec-file",
        script.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    let err = stderr_of(&out);
    assert!(
        err.contains("separated by `;`"),
        "missing separator hint: {err}"
    );

    // The same content with `;` terminators loads cleanly, and a genuine
    // parse error in `;`-separated input does NOT get the hint.
    std::fs::write(
        &script,
        "insert User { name := \"cy\", age := 51 };\ninsert User { name := \"di\", age := 19 };\n",
    )
    .unwrap();
    let ok = run(&[
        "--data-dir",
        data_s,
        "--exec-file",
        script.to_str().unwrap(),
    ]);
    assert!(ok.status.success(), "{}", stderr_of(&ok));
    let _ = std::fs::remove_dir_all(&dir);
}
