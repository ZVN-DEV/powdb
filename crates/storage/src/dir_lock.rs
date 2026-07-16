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
use std::time::{SystemTime, UNIX_EPOCH};

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
/// Admission is TOCTOU-hardened by a **create-then-recheck** on both sides:
/// after publishing its own file, each side re-validates the opposing condition
/// and backs off (removing its own file) if the opposite party appeared in the
/// window between the initial scan and the publish. This makes a "both admitted"
/// interleaving impossible rather than merely unlikely.
///
/// Each reader file is named `<pid>.<entropy>`, where `<entropy>` is a
/// per-acquisition 128-bit value (wall-clock nanoseconds mixed with a
/// process-local counter). One process may hold several reader locks without
/// collision, and a recycled PID never collides with a **stale** reader file
/// left by a crashed earlier process that happened to share the PID: the file is
/// created with `create_new` (O_EXCL) and retried on the vanishingly rare
/// collision. The owning PID is read from the file contents, not parsed from the
/// name, so reclaim always keys on the embedded PID's liveness.
const READERS_DIR: &str = "readers";

/// Process-local counter mixed into each reader file's entropy so two reader
/// locks acquired in the same wall-clock nanosecond still get distinct names.
static READER_NONCE: AtomicU64 = AtomicU64::new(0);

/// The kind of lock a [`DirLock`] holds, so [`Drop`] cleans up the right file.
#[derive(Debug)]
enum LockKind {
    /// Exclusive writer lock: the `LOCK` file, keyed by our PID.
    Writer,
    /// Shared reader lock: our own file under `readers/`.
    Reader,
    /// Read-only fallback on a **non-writable** data directory (a `0o555`
    /// snapshot mount or a read-only filesystem): no PID file was written, so
    /// [`Drop`] has nothing to remove. Safe because a writer cannot start
    /// against such a directory either, its own `LOCK` write would fail, so
    /// there is no writer to exclude.
    ReaderLockless,
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
        Self::acquire_as(data_dir, std::process::id())
    }

    /// [`DirLock::acquire`] parameterized by the acquiring PID, so tests can
    /// simulate distinct processes (real live child PIDs) racing for admission.
    fn acquire_as(data_dir: &Path, me: u32) -> io::Result<DirLock> {
        let path = data_dir.join(LOCK_FILE);

        // A readable, parseable PID that belongs to another live process is the
        // one case we refuse. Our own PID (in-process reopen) or a dead PID
        // (real crash) is a stale lock we take over; garbage is overwritten.
        if let Some(owner) = live_writer_pid(&path, me) {
            return Err(writer_busy_err(data_dir, owner, &path));
        }

        // A live reader (a different process) makes cross-process torn reads
        // possible the moment we start writing pages in place. Refuse rather
        // than race.
        if let Some(reader_pid) = live_reader_pid(data_dir, me) {
            return Err(reader_present_err(data_dir, reader_pid));
        }

        // Publish LOCK atomically (write a temp, then rename it into place) so a
        // concurrent reader's recheck never observes a half-written (momentarily
        // empty) LOCK and wrongly concludes there is no writer.
        write_pid_atomically(&path, me)?;

        // Create-then-recheck (TOCTOU): a reader may have published its file in
        // the window between our scan above and this write. If so, back off:
        // remove our LOCK (only while it is still ours) and refuse, so the
        // reader and this writer are never both admitted.
        if let Some(reader_pid) = live_reader_pid(data_dir, me) {
            remove_lock_if_owned(&path, me);
            return Err(reader_present_err(data_dir, reader_pid));
        }
        // A second writer may likewise have overwritten LOCK after our write.
        // If it now names a different live process, it won our slot: back off
        // without deleting its file.
        if let Some(owner) = live_writer_pid(&path, me) {
            return Err(writer_busy_err(data_dir, owner, &path));
        }

        Ok(DirLock {
            path,
            pid: me,
            kind: LockKind::Writer,
        })
    }

    /// Acquire a **shared reader** lock for read-only snapshot serving. N readers
    /// may hold one concurrently; the acquire fails only when a live writer (a
    /// different process holding `LOCK`) is present.
    ///
    /// On a **non-writable** data directory (a `0o555` snapshot mount or a
    /// read-only filesystem) the reader lock cannot be written. Rather than fail
    /// the whole read-only open, this falls back to a **lock-less** reader and
    /// logs a warning: on such a directory a writer cannot start either (its own
    /// `LOCK` write would fail), so there is no writer to exclude and the
    /// fallback is safe. The trade-off is that writer exclusion is not enforced
    /// for the lifetime of this handle.
    pub fn acquire_reader(data_dir: &Path) -> io::Result<DirLock> {
        Self::acquire_reader_as(data_dir, std::process::id())
    }

    /// [`DirLock::acquire_reader`] parameterized by the acquiring PID (see
    /// [`DirLock::acquire_as`]).
    fn acquire_reader_as(data_dir: &Path, me: u32) -> io::Result<DirLock> {
        // Refuse to start while a live writer (a different process) holds the
        // exclusive lock: its in-place page writes and intra-process-only mmap
        // coherence would make our reads tear.
        let lock_path = data_dir.join(LOCK_FILE);
        if let Some(owner) = live_writer_pid(&lock_path, me) {
            return Err(writer_serving_err(data_dir, owner));
        }

        let readers_dir = data_dir.join(READERS_DIR);
        if let Err(e) = fs::create_dir_all(&readers_dir) {
            if is_readonly_dir_error(&e) {
                return Ok(lockless_reader_fallback(data_dir));
            }
            return Err(e);
        }

        // PID-reuse-proof, collision-proof, atomically-published reader file.
        // The final name `<pid>.<entropy>` carries fresh per-acquisition entropy,
        // so a recycled PID never collides with (or adopts) a stale reader file a
        // crashed predecessor with the same PID left behind. The content is
        // written into a temp under `readers/` and renamed into place, so the
        // writer's scan never sees a half-written (empty) reader file.
        let path = match publish_reader_file(&readers_dir, me) {
            Ok(path) => path,
            Err(e) if is_readonly_dir_error(&e) => {
                return Ok(lockless_reader_fallback(data_dir));
            }
            Err(e) => return Err(e),
        };

        // Create-then-recheck (TOCTOU): a writer may have published `LOCK` in
        // the window between our scan above and this write. If so, back off:
        // remove our reader file and refuse, so this reader and the writer are
        // never both admitted.
        if let Some(owner) = live_writer_pid(&lock_path, me) {
            let _ = fs::remove_file(&path);
            return Err(writer_serving_err(data_dir, owner));
        }

        Ok(DirLock {
            path,
            pid: me,
            kind: LockKind::Reader,
        })
    }
}

/// Cap on temp-name retries when a reader file name collides. A collision needs
/// two acquisitions to draw the same 128-bit entropy in the same process, so
/// this is only ever reached by a pathological clock/counter; erroring beats
/// looping forever.
const MAX_READER_NAME_ATTEMPTS: u32 = 32;

/// Write `pid` into `final_path` atomically: fill a sibling temp file, then
/// rename it over `final_path`. A reader/writer therefore only ever observes a
/// fully-written pid file, never a create-but-not-yet-written empty one.
fn write_pid_atomically(final_path: &Path, pid: u32) -> io::Result<()> {
    let dir = final_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = final_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let tmp = dir.join(format!(".{file_name}.tmp.{:032x}", reader_entropy()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(pid.to_string().as_bytes())?;
        // Best-effort durability; the lock is advisory, not a data structure.
        let _ = f.sync_all();
    }
    fs::rename(&tmp, final_path)
}

/// Publish this process's reader file under `readers/` and return its path.
/// The content is written to a `readers/.tmp.*` temp (skipped by reader scans,
/// which ignore dot-prefixed names) and renamed to the final `<pid>.<entropy>`
/// name, so the file only ever appears fully written.
fn publish_reader_file(readers_dir: &Path, me: u32) -> io::Result<PathBuf> {
    for _ in 0..MAX_READER_NAME_ATTEMPTS {
        let entropy = format!("{:032x}", reader_entropy());
        let tmp = readers_dir.join(format!(".tmp.{entropy}"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(mut f) => {
                f.write_all(me.to_string().as_bytes())?;
                let _ = f.sync_all();
                let final_path = readers_dir.join(format!("{me}.{entropy}"));
                fs::rename(&tmp, &final_path)?;
                return Ok(final_path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique reader lock file name",
    ))
}

/// A 128-bit, per-acquisition value for a reader file name: wall-clock
/// nanoseconds mixed with a process-local counter so distinct acquisitions in
/// the same nanosecond still differ.
fn reader_entropy() -> u128 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let counter = u128::from(READER_NONCE.fetch_add(1, Ordering::Relaxed));
    nanos ^ (counter << 96) ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// Build a lock-less reader handle for a non-writable directory, logging the
/// one warning an operator must see: writer exclusion is not enforced for this
/// process. (It need not be: a writer cannot start against this directory.)
fn lockless_reader_fallback(data_dir: &Path) -> DirLock {
    tracing::warn!(
        data_dir = %data_dir.display(),
        "data directory is not writable; reader lock skipped, writer exclusion is \
         not enforced for this process (safe: a writer cannot start against a \
         non-writable directory)"
    );
    DirLock {
        path: data_dir.to_path_buf(),
        pid: std::process::id(),
        kind: LockKind::ReaderLockless,
    }
}

/// Whether an error from creating a reader lock file/dir means the directory is
/// read-only (a `0o555` mount or a read-only filesystem) rather than a genuine
/// I/O fault. Both are the signal to fall back to a lock-less reader.
fn is_readonly_dir_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
    ) || e.raw_os_error() == Some(libc::EROFS)
        || e.raw_os_error() == Some(libc::EACCES)
}

/// The PID in `lock_path` if it names a live process other than `me`; `None`
/// when the lock is absent, garbage, ours, or held by a dead process.
fn live_writer_pid(lock_path: &Path, me: u32) -> Option<u32> {
    let contents = fs::read_to_string(lock_path).ok()?;
    let owner = contents.trim().parse::<u32>().ok()?;
    if owner != me && pid_is_alive(owner) {
        Some(owner)
    } else {
        None
    }
}

/// Remove the `LOCK` file only while it still names `me`, so a back-off never
/// deletes a lock a concurrent writer legitimately took over.
fn remove_lock_if_owned(lock_path: &Path, me: u32) {
    if let Ok(contents) = fs::read_to_string(lock_path) {
        if contents.trim().parse::<u32>().ok() == Some(me) {
            let _ = fs::remove_file(lock_path);
        }
    }
}

fn writer_busy_err(data_dir: &Path, owner: u32, lock_path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::AddrInUse,
        format!(
            "data directory {} is already open by process {owner}; \
             close that instance first (or delete {} if it is gone)",
            data_dir.display(),
            lock_path.display()
        ),
    )
}

fn reader_present_err(data_dir: &Path, reader_pid: u32) -> io::Error {
    io::Error::new(
        io::ErrorKind::AddrInUse,
        format!(
            "data directory {} is being served read-only by process {reader_pid}; \
             stop the read-only reader(s) before opening it for writing",
            data_dir.display()
        ),
    )
}

fn writer_serving_err(data_dir: &Path, owner: u32) -> io::Error {
    io::Error::new(
        io::ErrorKind::AddrInUse,
        format!(
            "data directory {} is open for writing by process {owner}; \
             read-only serving requires a quiescent directory",
            data_dir.display()
        ),
    )
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
            LockKind::ReaderLockless => {
                // No PID file was ever written (non-writable directory), so
                // there is nothing to clean up.
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
        // Skip in-flight temp files (dot-prefixed): they are being written by a
        // concurrent reader and renamed into place atomically. Reading or
        // reclaiming one would corrupt that reader's publish.
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with('.'))
        {
            continue;
        }
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

    #[cfg(unix)]
    #[test]
    fn reader_falls_back_lock_less_on_non_writable_dir() {
        // C3: a 0o555 snapshot mount must not fail a read-only open. The reader
        // lock is skipped (nothing is written into the directory), matching the
        // "a read-only open never mutates a data file" promise.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // Seed a plausible data file so the directory looks like a snapshot, then
        // freeze it read-only.
        fs::write(dir.path().join("catalog"), b"x").unwrap();
        let orig = fs::metadata(dir.path()).unwrap().permissions();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();

        let result = DirLock::acquire_reader(dir.path());

        // Restore permissions before asserting so a failure never leaks an
        // undeletable temp dir.
        fs::set_permissions(dir.path(), orig).unwrap();

        let lock = result.expect("read-only open must succeed on a non-writable dir");
        assert!(
            matches!(lock.kind, LockKind::ReaderLockless),
            "a non-writable dir must fall back to a lock-less reader"
        );
        // Nothing was written: no readers/ directory was created.
        assert!(
            !dir.path().join(READERS_DIR).exists(),
            "the lock-less fallback must not mutate the directory"
        );
    }

    #[test]
    fn reader_names_are_pid_reuse_proof() {
        // A crashed reader leaves a stale `<pid>.<entropy>` file. A later reader
        // that is reassigned the same PID must NOT collide with (or adopt) the
        // stale file: create_new + fresh entropy give it a distinct name, so the
        // stale file survives as its own reclaimable entry.
        let dir = tempfile::tempdir().unwrap();
        let readers = dir.path().join(READERS_DIR);
        fs::create_dir_all(&readers).unwrap();
        let recycled_pid = std::process::id();
        // A stale file for the recycled PID, using the OLD collision-prone name
        // shape (`<pid>.0`) a crash could have left behind.
        let stale = readers.join(format!("{recycled_pid}.0"));
        fs::write(&stale, recycled_pid.to_string()).unwrap();

        let lock = DirLock::acquire_reader_as(dir.path(), recycled_pid).unwrap();

        // The live reader got its own distinct file; the stale one is untouched.
        assert!(
            stale.exists(),
            "stale reader file must not be adopted/truncated"
        );
        assert_ne!(
            lock.path, stale,
            "a recycled PID must not reuse the stale file's name"
        );
        let count = fs::read_dir(&readers).unwrap().count();
        assert_eq!(
            count, 2,
            "stale + live reader files coexist without collision"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reader_and_writer_are_never_both_admitted() {
        // Best-effort concurrency stress: race a reader and a writer for the
        // same directory using two *distinct live PIDs* (real child processes),
        // so the cross-process `owner != me` exclusion actually engages. The
        // create-then-recheck invariant must make "both admitted" impossible.
        let writer_child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let reader_child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let writer_pid = writer_child.id();
        let reader_pid = reader_child.id();

        let mut both_admitted = 0u32;
        for _ in 0..200 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().to_path_buf();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

            // Hold both guards across the join: only a *simultaneous* double-hold
            // is a violation. (Returning `.is_ok()` would drop each lock the
            // instant it was acquired, letting an admitted writer's LOCK vanish
            // mid-race and masking the invariant.)
            let (w_res, r_res) = std::thread::scope(|scope| {
                let wb = barrier.clone();
                let wp = path.clone();
                let w = scope.spawn(move || {
                    wb.wait();
                    DirLock::acquire_as(&wp, writer_pid)
                });
                let rb = barrier.clone();
                let rp = path.clone();
                let r = scope.spawn(move || {
                    rb.wait();
                    DirLock::acquire_reader_as(&rp, reader_pid)
                });
                (w.join().unwrap(), r.join().unwrap())
            });

            if w_res.is_ok() && r_res.is_ok() {
                both_admitted += 1;
            }
            // Both guards drop here, releasing the directory for the next round.
        }

        // Clean up the stand-in "processes".
        let mut writer_child = writer_child;
        let mut reader_child = reader_child;
        let _ = writer_child.kill();
        let _ = reader_child.kill();
        let _ = writer_child.wait();
        let _ = reader_child.wait();

        assert_eq!(
            both_admitted, 0,
            "a reader and a writer were both admitted in {both_admitted} race(s)"
        );
    }
}
