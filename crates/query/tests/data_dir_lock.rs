//! Data-directory lock: a single data dir must not be opened by two *separate*
//! live processes at once (concurrent writers corrupt the heap/WAL). The lock is
//! PID-based rather than an held-fd `flock` on purpose: the durability suite
//! simulates crashes with `std::mem::forget(engine)` and then reopens the same
//! dir *in the same process*, so a fd-based lock would leak and wedge those
//! reopens. PID-based lets a same-PID (or dead-PID) owner take over while still
//! refusing a different, still-running process.

use powdb_query::executor::Engine;

const LOCK_FILE: &str = "LOCK";

/// A different, still-alive process holding the lock must block a second open.
#[test]
fn open_refused_when_another_live_process_holds_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    // Create a real database first, then release it cleanly.
    drop(Engine::new(dir.path()).unwrap());

    // Simulate another live process owning the lock: a real sleeping child.
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    std::fs::write(dir.path().join(LOCK_FILE), child.id().to_string()).unwrap();

    let result = Engine::new(dir.path());
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        result.is_err(),
        "opening a dir locked by another live process must fail, got Ok"
    );
}

/// A stale lock left by a dead process must be taken over, not treated as fatal.
#[test]
fn open_takes_over_stale_lock_from_dead_process() {
    let dir = tempfile::tempdir().unwrap();
    drop(Engine::new(dir.path()).unwrap());

    // A reaped child's PID is no longer alive.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = child.id();
    child.wait().unwrap();
    std::fs::write(dir.path().join(LOCK_FILE), dead_pid.to_string()).unwrap();

    assert!(
        Engine::new(dir.path()).is_ok(),
        "a stale lock from a dead process must be taken over"
    );
}

/// A clean close releases the lock so the same dir can be reopened.
#[test]
fn clean_close_releases_lock_for_reopen() {
    let dir = tempfile::tempdir().unwrap();
    drop(Engine::new(dir.path()).unwrap());
    assert!(
        Engine::new(dir.path()).is_ok(),
        "reopen after a clean close must succeed"
    );
}
