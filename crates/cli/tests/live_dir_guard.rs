//! Offline CLI subcommands must refuse a data dir owned by a live writer.
//!
//! `powdb-cli --data-dir <dir> backup <dest>` and `powdb-cli --data-dir <dir>
//! sweep <table>` open the raw `Catalog` and mutate the directory (backup
//! checkpoints and truncates the shared `wal.log`; sweep rewrites heap pages).
//! The engine's `DirLock` exists exactly to stop a second process from doing
//! that, but these subcommands never acquire it. Run against a directory a
//! live `powdb-server` has open, `backup` silently truncates the server's WAL:
//! every write the server acknowledges after that point is destroyed by the
//! next crash recovery (reproduced: 66 of 200 acked rows lost under
//! `WalSyncMode::Full`, torture run 2026-07-21).
//!
//! These tests simulate the live server with a real, live foreign process (a
//! spawned `sleep`) whose PID is written to the `LOCK` file, exactly as
//! `DirLock::acquire` would have left it. The subcommands must fail and leave
//! the directory untouched.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_powdb-cli")
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "powdb_liveguard_{tag}_{}_{}",
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

/// Seed a data dir with a table and rows, then release it cleanly.
fn seed(data_s: &str) {
    assert!(
        run(&["--data-dir", data_s, "-c", "type T { required id: int }"])
            .status
            .success()
    );
    assert!(run(&[
        "--data-dir",
        data_s,
        "-c",
        "insert T { id := 1 }; insert T { id := 2 }; insert T { id := 3 }"
    ])
    .status
    .success());
}

/// Spawn a live foreign process and plant its PID in the data dir's LOCK
/// file, simulating a running `powdb-server` that owns the directory.
fn plant_live_writer(data_dir: &std::path::Path) -> std::process::Child {
    let child = Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("failed to spawn sleeper process");
    std::fs::write(data_dir.join("LOCK"), child.id().to_string()).expect("failed to write LOCK");
    child
}

#[test]
fn backup_refuses_data_dir_held_by_live_writer() {
    let data = tmp("bk_data");
    let dest = tmp("bk_dest");
    let data_s = data.to_str().unwrap();
    seed(data_s);

    let mut sleeper = plant_live_writer(&data);
    let wal_len_before = std::fs::metadata(data.join("wal.log"))
        .map(|m| m.len())
        .ok();

    let out = run(&["--data-dir", data_s, "backup", dest.to_str().unwrap()]);
    let _ = sleeper.kill();
    let _ = sleeper.wait();

    assert!(
        !out.status.success(),
        "backup against a data dir held by a live writer must be refused, \
         but it exited 0 with stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    // The refusal must leave the live directory untouched: in particular the
    // shared WAL must not have been checkpoint-truncated out from under the
    // (simulated) live server.
    let wal_len_after = std::fs::metadata(data.join("wal.log"))
        .map(|m| m.len())
        .ok();
    assert_eq!(
        wal_len_before, wal_len_after,
        "backup mutated the live directory's wal.log despite being refused"
    );
    assert!(
        !dest.join("manifest.json").exists(),
        "a refused backup must not leave a manifest at the destination"
    );
}

#[test]
fn sweep_refuses_data_dir_held_by_live_writer() {
    let data = tmp("sw_data");
    let data_s = data.to_str().unwrap();
    seed(data_s);

    let mut sleeper = plant_live_writer(&data);
    let out = run(&["--data-dir", data_s, "sweep", "T"]);
    let _ = sleeper.kill();
    let _ = sleeper.wait();

    assert!(
        !out.status.success(),
        "sweep against a data dir held by a live writer must be refused, \
         but it exited 0 with stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn backup_takes_over_stale_lock_from_dead_process() {
    // A crashed server leaves a stale LOCK naming a dead PID. Backup must
    // still work then (same dead-PID takeover the engine itself performs).
    let data = tmp("stale_data");
    let dest = tmp("stale_dest");
    let data_s = data.to_str().unwrap();
    seed(data_s);

    // A PID that is certainly dead: spawn a child and wait for it to exit.
    let mut child = Command::new("true").spawn().expect("spawn");
    let dead_pid = child.id();
    child.wait().expect("wait");
    std::fs::write(data.join("LOCK"), dead_pid.to_string()).expect("write LOCK");

    let out = run(&["--data-dir", data_s, "backup", dest.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "backup must take over a stale LOCK from a dead process: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dest.join("manifest.json").exists());
}
