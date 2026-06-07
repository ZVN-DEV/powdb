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

#[test]
fn cli_incremental_backup_and_chain_restore() {
    let data = tmp("incdata");
    let data_s = data.to_str().unwrap();

    // seed schema + 20 rows
    assert!(
        run(&["--data-dir", data_s, "-c", "type T { required id: int }"])
            .status
            .success()
    );
    for i in 0..20 {
        let q = format!("insert T {{ id := {i} }}");
        assert!(run(&["--data-dir", data_s, "-c", &q]).status.success());
    }

    // full backup
    let full = tmp("incfull");
    let b = run(&["--data-dir", data_s, "backup", full.to_str().unwrap()]);
    assert!(
        b.status.success(),
        "full backup failed: {}",
        String::from_utf8_lossy(&b.stderr)
    );

    // insert 10 more rows after the full base
    for i in 20..30 {
        let q = format!("insert T {{ id := {i} }}");
        assert!(run(&["--data-dir", data_s, "-c", &q]).status.success());
    }

    // incremental backup against the full base
    let inc = tmp("incinc");
    let ib = run(&[
        "--data-dir",
        data_s,
        "backup",
        inc.to_str().unwrap(),
        "--base",
        full.to_str().unwrap(),
    ]);
    assert!(
        ib.status.success(),
        "incremental backup failed: {}",
        String::from_utf8_lossy(&ib.stderr)
    );

    // chain restore: full base + increment
    let restored = tmp("increstored");
    let r = run(&[
        "restore",
        full.to_str().unwrap(),
        restored.to_str().unwrap(),
        "--apply",
        inc.to_str().unwrap(),
    ]);
    assert!(
        r.status.success(),
        "chain restore failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );

    // verify restored data reflects all 30 rows
    let out = run(&["--data-dir", restored.to_str().unwrap(), "-c", "count(T)"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("30"),
        "expected count 30 in chain-restored DB, got stdout: {stdout:?}"
    );
}
