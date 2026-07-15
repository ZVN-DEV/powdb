//! PID-based advisory lock for a data directory.
//!
//! Stops two **separate** processes from opening the same database directory at
//! once — concurrent writers corrupt the heap and WAL. It is deliberately *not*
//! an held-fd `flock`: the engine's crash-recovery suite simulates a crash with
//! [`std::mem::forget`] and then reopens the same directory **in the same
//! process**, which would wedge on a leaked fd lock. A PID file instead lets a
//! same-PID owner (an in-process reopen after a forget-crash) or a dead-PID
//! owner (a real crash — the process is gone) take over, while still refusing a
//! different, still-running process.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Name of the exclusive writer lock file inside the data directory.
const LOCK_FILE: &str = "LOCK";

/// Subdirectory holding one PID file per live read-only reader. Reader and
/// writer admission is coordinated through it:
///
/// - A **writer** ([`DirLock::acquire`]) refuses to start while the `LOCK` file
///   is held by another live process **or** while any reader file names a live
///   process other than itself: two-process writes, and a writer racing a live
///   reader, both corrupt the shared heap/WAL.
/// - A **reader** ([`DirLock::acquire_reader`]) refuses to start while `LOCK` is
///   held by another live process (a live writer), then drops its own PID file
///   into this directory. Any number of readers coexist: reader files never
///   block another reader.
/// - Liveness uses the same dead-PID heuristic as the writer lock: a reader file
///   whose PID is gone (a crashed reader) is ignored and reclaimed, so a crash
///   never wedges the directory.
///
/// Each reader file is named `<pid>.<nonce>` so one process may hold several
/// reader locks without collision; the owning PID is read from the file
/// contents, not parsed from the name.
const READERS_DIR: &str = "readers";

/// Process-local nonce source, so two reader locks acquired in the same process
/// get distinct files (and each removes exactly its own on drop).
static READER_NONCE: AtomicU64 = AtomicU64::new(0);

/// The kind of lock a [`DirLock`] holds, so [`Drop`] cleans up the right file.
#[derive(Debug)]
enum LockKind {
    /// Exclusive writer lock: the `LOCK` file, keyed by our PID.
    Writer,
    /// Shared reader lock: our own file under `readers/`.
    Reader,
}

/// Held for the lifetime of an open database. A clean drop removes the PID file;
/// a `mem::forget` (crash simulation) skips that, leaving a stale file the next
/// open takes over.
#[derive(Debug)]
pub struct DirLock {
    path: PathBuf,
    pid: u32,
    kind: LockKind,
}

impl DirLock {
    /// Acquire the exclusive writer lock for `data_dir`. Fails when a
    /// *different, still alive* process holds the writer lock, or when a live
    /// reader (a different process) is currently serving the directory.
    pub fn acquire(data_dir: &Path) -> io::Result<DirLock> {
        let path = data_dir.join(LOCK_FILE);
        let me = std::process::id();

        // A readable, parseable PID that belongs to another live process is the
        // one case we refuse. Our own PID (in-process reopen) or a dead PID
        // (real crash) is a stale lock we take over; garbage is overwritten.
        if let Ok(contents) = fs::read_to_string(&path) {
            if let Ok(owner) = contents.trim().parse::<u32>() {
                if owner != me && pid_is_alive(owner) {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!(
                            "data directory {} is already open by process {owner}; \
                             close that instance first (or delete {} if it is gone)",
                            data_dir.display(),
                            path.display()
                        ),
                    ));
                }
            }
        }

        // A live reader (a different process) makes cross-process torn reads
        // possible the moment we start writing pages in place. Refuse rather
        // than race.
        if let Some(reader_pid) = live_reader_pid(data_dir, me) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "data directory {} is being served read-only by process {reader_pid}; \
                     stop the read-only reader(s) before opening it for writing",
                    data_dir.display()
                ),
            ));
        }

        let mut f = fs::File::create(&path)?;
        f.write_all(me.to_string().as_bytes())?;
        // Best-effort durability; the lock is advisory, not a data structure.
        let _ = f.sync_all();
        Ok(DirLock {
            path,
            pid: me,
            kind: LockKind::Writer,
        })
    }

    /// Acquire a **shared reader** lock for read-only snapshot serving. N readers
    /// may hold one concurrently; the acquire fails only when a live writer (a
    /// different process holding `LOCK`) is present.
    pub fn acquire_reader(data_dir: &Path) -> io::Result<DirLock> {
        let me = std::process::id();

        // Refuse to start while a live writer (a different process) holds the
        // exclusive lock: its in-place page writes and intra-process-only mmap
        // coherence would make our reads tear.
        let lock_path = data_dir.join(LOCK_FILE);
        if let Ok(contents) = fs::read_to_string(&lock_path) {
            if let Ok(owner) = contents.trim().parse::<u32>() {
                if owner != me && pid_is_alive(owner) {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!(
                            "data directory {} is open for writing by process {owner}; \
                             read-only serving requires a quiescent directory",
                            data_dir.display()
                        ),
                    ));
                }
            }
        }

        let readers_dir = data_dir.join(READERS_DIR);
        fs::create_dir_all(&readers_dir)?;
        let nonce = READER_NONCE.fetch_add(1, Ordering::Relaxed);
        let path = readers_dir.join(format!("{me}.{nonce}"));
        let mut f = fs::File::create(&path)?;
        f.write_all(me.to_string().as_bytes())?;
        let _ = f.sync_all();
        Ok(DirLock {
            path,
            pid: me,
            kind: LockKind::Reader,
        })
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        match self.kind {
            LockKind::Writer => {
                // Only remove the file if we still own it, so we never delete a
                // lock a later open (same PID) refreshed.
                if let Ok(contents) = fs::read_to_string(&self.path) {
                    if contents.trim().parse::<u32>().ok() == Some(self.pid) {
                        let _ = fs::remove_file(&self.path);
                    }
                }
            }
            LockKind::Reader => {
                // Remove exactly our own reader file. A crash (mem::forget or
                // kill -9) leaves it behind; the dead-PID heuristic reclaims it.
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

/// Return the PID of a live reader other than `me`, if any reader file under
/// `readers/` names a still-running process. Dead-PID (crashed) reader files are
/// ignored and best-effort removed so they never wedge a future writer.
fn live_reader_pid(data_dir: &Path, me: u32) -> Option<u32> {
    let readers_dir = data_dir.join(READERS_DIR);
    let entries = fs::read_dir(&readers_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(owner) = contents.trim().parse::<u32>() else {
            // Garbage file: reclaim it.
            let _ = fs::remove_file(&path);
            continue;
        };
        if owner == me {
            // Our own reader (same process holding both, or a stale file we may
            // reclaim on our next writer open): never blocks us.
            continue;
        }
        if pid_is_alive(owner) {
            return Some(owner);
        }
        // Dead reader: reclaim its file.
        let _ = fs::remove_file(&path);
    }
    None
}

/// Whether `pid` refers to a live process. `kill(pid, 0)` sends no signal but
/// runs the existence/permission check: success or `EPERM` ⇒ alive, `ESRCH` ⇒
/// dead.
#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: u32) -> bool {
    // Conservative: never auto-steal a lock when we can't check liveness.
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_then_drop_removes_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _lock = DirLock::acquire(dir.path()).unwrap();
            assert!(dir.path().join(LOCK_FILE).exists());
        }
        assert!(
            !dir.path().join(LOCK_FILE).exists(),
            "drop should remove it"
        );
    }

    #[test]
    fn same_process_reacquire_is_allowed() {
        // Models an in-process reopen after a forget-crash: the stale file holds
        // our own PID, so a second acquire takes over rather than refusing.
        let dir = tempfile::tempdir().unwrap();
        let first = DirLock::acquire(dir.path()).unwrap();
        std::mem::forget(first); // leave the file behind, as a crash would
        assert!(DirLock::acquire(dir.path()).is_ok());
    }

    #[test]
    fn dead_owner_pid_is_a_dead_pid() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!pid_is_alive(pid));
    }

    #[test]
    fn two_readers_coexist() {
        let dir = tempfile::tempdir().unwrap();
        let r1 = DirLock::acquire_reader(dir.path()).unwrap();
        let r2 = DirLock::acquire_reader(dir.path()).unwrap();
        // Both hold distinct reader files under readers/.
        let count = fs::read_dir(dir.path().join(READERS_DIR)).unwrap().count();
        assert_eq!(count, 2, "two readers should leave two reader files");
        drop(r1);
        drop(r2);
        // Each drop removes exactly its own file.
        let count = fs::read_dir(dir.path().join(READERS_DIR)).unwrap().count();
        assert_eq!(count, 0, "reader drops should remove their files");
    }

    #[test]
    fn reader_excludes_live_writer() {
        // Simulate a live writer from another process using PID 1 (init), which
        // is always alive and never us.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(LOCK_FILE), "1").unwrap();
        let err = DirLock::acquire_reader(dir.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    }

    #[test]
    fn writer_excludes_live_reader() {
        // Simulate a live reader from another process using PID 1.
        let dir = tempfile::tempdir().unwrap();
        let readers = dir.path().join(READERS_DIR);
        fs::create_dir_all(&readers).unwrap();
        fs::write(readers.join("1.0"), "1").unwrap();
        let err = DirLock::acquire(dir.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    }

    #[test]
    fn writer_reclaims_dead_reader_files() {
        // A crashed reader (dead PID) must not wedge a new writer.
        let dir = tempfile::tempdir().unwrap();
        let readers = dir.path().join(READERS_DIR);
        fs::create_dir_all(&readers).unwrap();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        fs::write(readers.join(format!("{dead_pid}.0")), dead_pid.to_string()).unwrap();

        // Writer starts (dead reader is reclaimed), and the stale file is gone.
        let _w = DirLock::acquire(dir.path()).unwrap();
        assert!(
            !readers.join(format!("{dead_pid}.0")).exists(),
            "dead reader file should be reclaimed"
        );
    }

    #[test]
    fn reader_reclaims_dead_writer_lock() {
        // A crashed writer (dead PID in LOCK) must not block read-only serving.
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        fs::write(dir.path().join(LOCK_FILE), dead_pid.to_string()).unwrap();
        // A dead writer lock does not block a reader.
        let _r = DirLock::acquire_reader(dir.path()).unwrap();
    }
}
