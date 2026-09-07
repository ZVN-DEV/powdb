//! Data-directory lock: a single data dir must not be opened by two *separate*
//! live processes at once (concurrent writers corrupt the heap/WAL). The lock is
//! PID-based rather than an held-fd `flock` on purpose: the durability suite
//! simulates crashes with `std::mem::forget(engine)` and then reopens the same
//! dir *in the same process*, so a fd-based lock would leak and wedge those
//! reopens. PID-based lets a same-PID (or dead-PID) owner take over while still
//! refusing a different, still-running process.

use powdb_query::executor::Engine;

const LOCK_FILE: &str = "LOCK";

/// Environment variable naming the directory the helper child should hold open.
const HOLDER_DIR_ENV: &str = "POWDB_TEST_LOCK_HOLDER_DIR";

/// A different, still-alive process holding the lock must block a second open.
///
/// The other process is a real one with a real engine open, re-invoking this
/// test binary. A `LOCK` file naming a live PID is no longer enough on its own:
/// a PID read in one namespace names a different process (or none) in another,
/// so an owner that holds no advisory lock on `LOCK` is treated as a ghost and
/// reclaimed. Only an engine that is actually open holds that lock.
#[test]
fn open_refused_when_another_live_process_holds_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    // Create a real database first, then release it cleanly.
    drop(Engine::new(dir.path()).unwrap());

    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["holds_the_lock_until_killed", "--exact", "--ignored"])
        .env(HOLDER_DIR_ENV, dir.path())
        .spawn()
        .unwrap();
    let held = wait_for_lock_owner(&dir.path().join(LOCK_FILE), child.id());

    let result = Engine::new(dir.path());
    let _ = child.kill();
    let _ = child.wait();

    assert!(held, "the helper process never took the lock");
    assert!(
        result.is_err(),
        "opening a dir locked by another live process must fail, got Ok"
    );
}

/// Poll `LOCK` until it names `pid`, for as long as a loaded machine plausibly
/// needs to start a process and open a database.
fn wait_for_lock_owner(lock_path: &std::path::Path, pid: u32) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if std::fs::read_to_string(lock_path)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
            == Some(pid)
        {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

/// The other process of [`open_refused_when_another_live_process_holds_the_lock`]:
/// holds a real engine open until its parent kills it. Ignored, so a normal run
/// of this binary never enters it; the parent names it explicitly.
#[test]
#[ignore = "helper process for open_refused_when_another_live_process_holds_the_lock"]
fn holds_the_lock_until_killed() {
    let Ok(dir) = std::env::var(HOLDER_DIR_ENV) else {
        return;
    };
    let _engine = Engine::new(std::path::Path::new(&dir)).unwrap();
    std::thread::sleep(std::time::Duration::from_secs(120));
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
