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

/// Name of the lock file inside the data directory.
const LOCK_FILE: &str = "LOCK";

/// Held for the lifetime of an open database. A clean drop removes the PID file;
/// a `mem::forget` (crash simulation) skips that, leaving a stale file the next
/// open takes over.
#[derive(Debug)]
pub struct DirLock {
    path: PathBuf,
    pid: u32,
}

impl DirLock {
    /// Acquire the lock for `data_dir`. Fails only when a *different, still
    /// alive* process currently holds it.
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

        let mut f = fs::File::create(&path)?;
        f.write_all(me.to_string().as_bytes())?;
        // Best-effort durability; the lock is advisory, not a data structure.
        let _ = f.sync_all();
        Ok(DirLock { path, pid: me })
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        // Only remove the file if we still own it, so we never delete a lock a
        // later open (same PID) refreshed.
        if let Ok(contents) = fs::read_to_string(&self.path) {
            if contents.trim().parse::<u32>().ok() == Some(self.pid) {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
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
}
