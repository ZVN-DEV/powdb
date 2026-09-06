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

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

/// Spawn a real live writer: a `powdb-cli` REPL that holds the directory's
/// writer lock for as long as its stdin stays open.
///
/// Writing a live process's PID into `LOCK` is no longer enough to stand in
/// for one. The storage lock is settled by an `flock` on the `LOCK` file, so a
/// directory whose `LOCK` names a live process that holds no flock is
/// correctly read as a ghost and reclaimed; a `sleep` never took the lock, so
/// it stopped representing a running server. Only a process that really
/// acquired the lock does.
fn plant_live_writer(data_dir: &std::path::Path) -> std::process::Child {
    let mut child = Command::new(bin())
        .args(["--data-dir", data_dir.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn a live powdb-cli writer");
    let want = child.id().to_string();
    let deadline = Instant::now() + Duration::from_secs(60);
    while std::fs::read_to_string(data_dir.join("LOCK"))
        .map(|text| text.trim() != want)
        .unwrap_or(true)
    {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the live writer never took the data dir lock");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
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
