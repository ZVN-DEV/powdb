use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_powdb-cli")
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "powdb_clibk_{tag}_{}_{}",
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

#[test]
fn cli_backup_then_restore_roundtrip() {
    let data = tmp("data");
    let data_s = data.to_str().unwrap();

    // seed
    assert!(
        run(&["--data-dir", data_s, "-c", "type T { required id: int }"])
            .status
            .success()
    );
    for i in 0..10 {
        let q = format!("insert T {{ id := {i} }}");
        assert!(run(&["--data-dir", data_s, "-c", &q]).status.success());
    }

    // backup
    let backup = tmp("bkp");
    let b = run(&["--data-dir", data_s, "backup", backup.to_str().unwrap()]);
    assert!(
        b.status.success(),
        "backup failed: {}",
        String::from_utf8_lossy(&b.stderr)
    );

    // restore
    let restored = tmp("restored");
    let r = run(&[
        "restore",
        backup.to_str().unwrap(),
        restored.to_str().unwrap(),
    ]);
    assert!(
        r.status.success(),
        "restore failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );

    // verify restored data: count(T) == 10
    let out = run(&["--data-dir", restored.to_str().unwrap(), "-c", "count(T)"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("10"),
        "expected count 10 in restored DB, got stdout: {stdout:?}"
    );
}
