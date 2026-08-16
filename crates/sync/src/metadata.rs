use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use powdb_storage::create_data_dir_secure;
use serde::{Deserialize, Serialize};

use crate::segment::{read_units_since, SegmentIdentity};

pub const SYNC_STATE_DIR: &str = ".powdb-sync";
pub const IDENTITY_FILE: &str = "identity.json";
pub const REPLICA_CURSORS_FILE: &str = "replica-cursors.json";
pub const SYNC_METADATA_FORMAT_VERSION: u32 = 1;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const CURSOR_LOCK_FILE: &str = ".replica-cursors.lock";
/// How long a caller waits for the cursor lock before giving up.
///
/// Deliberately equal to [`CURSOR_LOCK_STALE_AFTER`]. The holder's critical
/// section is two fsyncs, so its cost rises with the number of contenders and
/// with whatever else is hitting the disk; the previous 5s could be exhausted
/// by ordinary contention on a busy machine, and the caller then got a
/// `WouldBlock` error for a lock that was simply in use rather than stuck. A
/// genuinely abandoned lock is not the reason to wait less: a dead owner is
/// detected and reclaimed by `reclaim_stale_cursor_lock` on every retry, and
/// that only becomes possible at `CURSOR_LOCK_STALE_AFTER`. Giving up sooner
/// than that means failing before the one mechanism that could have unblocked
/// us has had a chance to run.
const CURSOR_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
/// First backoff step. Grows to [`CURSOR_LOCK_RETRY_MAX`]; see
/// [`cursor_lock_backoff`] for why it grows and why it is jittered.
const CURSOR_LOCK_RETRY: Duration = Duration::from_millis(5);
const CURSOR_LOCK_RETRY_MAX: Duration = Duration::from_millis(50);
const CURSOR_LOCK_STALE_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatabaseIdentity {
    pub database_id: [u8; 16],
    pub primary_generation: u64,
}

impl DatabaseIdentity {
    pub fn segment_identity(self) -> SegmentIdentity {
        SegmentIdentity::current(self.database_id, self.primary_generation)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaCursor {
    pub replica_id: String,
    pub applied_lsn: u64,
    pub updated_unix_secs: u64,
    pub active: bool,
}

impl ReplicaCursor {
    pub fn active(replica_id: impl Into<String>, applied_lsn: u64) -> Self {
        Self {
            replica_id: replica_id.into(),
            applied_lsn,
            updated_unix_secs: now_unix_secs(),
            active: true,
        }
    }

    pub fn next_required_lsn(&self) -> io::Result<u64> {
        self.applied_lsn
            .checked_add(1)
            .ok_or_else(|| invalid_data("replica cursor LSN overflow"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentitySnapshot {
    pub format_version: u32,
    pub database_id: String,
    pub primary_generation: u64,
    pub created_unix_secs: u64,
}

impl IdentitySnapshot {
    pub fn from_identity(identity: DatabaseIdentity, created_unix_secs: u64) -> Self {
        Self {
            format_version: SYNC_METADATA_FORMAT_VERSION,
            database_id: encode_hex_16(identity.database_id),
            primary_generation: identity.primary_generation,
            created_unix_secs,
        }
    }

    pub fn identity(&self) -> io::Result<DatabaseIdentity> {
        if self.format_version != SYNC_METADATA_FORMAT_VERSION {
            return Err(invalid_data(format!(
                "unsupported sync identity format {}",
                self.format_version
            )));
        }
        let identity = DatabaseIdentity {
            database_id: decode_hex_16(&self.database_id)?,
            primary_generation: self.primary_generation,
        };
        validate_identity(identity)?;
        Ok(identity)
    }

    pub fn validate(&self) -> io::Result<()> {
        self.identity().map(|_| ())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CursorFile {
    format_version: u32,
    cursors: Vec<ReplicaCursor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CursorLockFile {
    format_version: u32,
    owner_pid: u32,
    created_unix_secs: u64,
}

pub fn sync_state_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(SYNC_STATE_DIR)
}

pub fn open_or_create_identity(data_dir: &Path) -> io::Result<DatabaseIdentity> {
    let state_dir = sync_state_dir(data_dir);
    create_data_dir_secure(&state_dir)?;

    match read_identity(data_dir) {
        Ok(identity) => return Ok(identity),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    let identity = DatabaseIdentity {
        database_id: generate_database_id()?,
        primary_generation: 1,
    };
    let file = IdentitySnapshot::from_identity(identity, now_unix_secs());

    match write_new_identity_file(&state_dir, &file) {
        Ok(()) => Ok(identity),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => read_identity(data_dir),
        Err(err) => Err(err),
    }
}

pub fn read_identity(data_dir: &Path) -> io::Result<DatabaseIdentity> {
    let Some(snapshot) = read_identity_snapshot(data_dir)? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "sync identity not found",
        ));
    };
    snapshot.identity()
}

pub fn read_identity_snapshot(data_dir: &Path) -> io::Result<Option<IdentitySnapshot>> {
    let bytes = fs::read(sync_state_dir(data_dir).join(IDENTITY_FILE))?;
    let snapshot: IdentitySnapshot = serde_json::from_slice(&bytes).map_err(invalid_data)?;
    snapshot.validate()?;
    Ok(Some(snapshot))
}

pub fn read_identity_snapshot_if_exists(data_dir: &Path) -> io::Result<Option<IdentitySnapshot>> {
    match read_identity_snapshot(data_dir) {
        Ok(snapshot) => Ok(snapshot),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

pub fn write_identity_snapshot(data_dir: &Path, snapshot: &IdentitySnapshot) -> io::Result<()> {
    snapshot.validate()?;
    let state_dir = sync_state_dir(data_dir);
    create_data_dir_secure(&state_dir)?;
    match write_new_identity_file(&state_dir, snapshot) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let Some(existing) = read_identity_snapshot(data_dir)? else {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "sync identity already exists but could not be read",
                ));
            };
            if existing == *snapshot {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "sync identity already exists with different database history",
                ))
            }
        }
        Err(err) => Err(err),
    }
}

pub fn read_replica_cursors(data_dir: &Path) -> io::Result<Vec<ReplicaCursor>> {
    read_replica_cursors_unlocked(data_dir)
}

pub(crate) fn read_replica_cursors_unlocked(data_dir: &Path) -> io::Result<Vec<ReplicaCursor>> {
    let path = sync_state_dir(data_dir).join(REPLICA_CURSORS_FILE);
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let file: CursorFile = serde_json::from_slice(&bytes).map_err(invalid_data)?;
    validate_cursor_file(&file)?;
    Ok(file.cursors)
}

pub fn write_replica_cursors(data_dir: &Path, mut cursors: Vec<ReplicaCursor>) -> io::Result<()> {
    validate_cursors_for_write(&cursors)?;
    cursors.sort_by(|a, b| a.replica_id.cmp(&b.replica_id));
    let state_dir = sync_state_dir(data_dir);
    create_data_dir_secure(&state_dir)?;
    let _lock = acquire_cursor_lock(&state_dir)?;
    validate_active_cursors_against_retained_tail(data_dir, &cursors)?;
    write_replica_cursors_unlocked(&state_dir, cursors)
}

fn write_replica_cursors_unlocked(state_dir: &Path, cursors: Vec<ReplicaCursor>) -> io::Result<()> {
    let file = CursorFile {
        format_version: SYNC_METADATA_FORMAT_VERSION,
        cursors,
    };
    atomic_replace_json(state_dir, REPLICA_CURSORS_FILE, &file)
}

pub(crate) fn replace_replica_cursors_unlocked(
    data_dir: &Path,
    mut cursors: Vec<ReplicaCursor>,
) -> io::Result<()> {
    validate_cursors_for_write(&cursors)?;
    cursors.sort_by(|a, b| a.replica_id.cmp(&b.replica_id));
    let state_dir = sync_state_dir(data_dir);
    create_data_dir_secure(&state_dir)?;
    write_replica_cursors_unlocked(&state_dir, cursors)
}

pub fn upsert_replica_cursor(data_dir: &Path, cursor: ReplicaCursor) -> io::Result<()> {
    validate_cursor_for_write(&cursor)?;
    let state_dir = sync_state_dir(data_dir);
    create_data_dir_secure(&state_dir)?;
    let _lock = acquire_cursor_lock(&state_dir)?;
    validate_active_cursor_against_retained_tail(data_dir, &cursor)?;
    let mut cursors = read_replica_cursors_unlocked(data_dir)?;
    if let Some(existing) = cursors
        .iter_mut()
        .find(|existing| existing.replica_id == cursor.replica_id)
    {
        *existing = cursor;
    } else {
        cursors.push(cursor);
    }
    validate_cursors_for_write(&cursors)?;
    cursors.sort_by(|a, b| a.replica_id.cmp(&b.replica_id));
    write_replica_cursors_unlocked(&state_dir, cursors)
}

pub fn register_bootstrap_cursor(
    data_dir: &Path,
    replica_id: &str,
    snapshot_lsn: u64,
    required_through_lsn: u64,
) -> io::Result<ReplicaCursor> {
    if required_through_lsn < snapshot_lsn {
        return Err(invalid_input(format!(
            "remote LSN {required_through_lsn} is behind snapshot LSN {snapshot_lsn}"
        )));
    }
    validate_replica_id(replica_id).map_err(invalid_input)?;
    let cursor = ReplicaCursor::active(replica_id, snapshot_lsn);
    validate_cursor_for_write(&cursor)?;

    let state_dir = sync_state_dir(data_dir);
    create_data_dir_secure(&state_dir)?;
    let _lock = acquire_cursor_lock(&state_dir)?;

    let identity = read_identity(data_dir)?;
    crate::validate_retained_tail_available(
        &crate::retained_segments_dir(data_dir),
        identity.segment_identity(),
        snapshot_lsn,
        required_through_lsn,
    )
    .map_err(|err| {
        if matches!(err.kind(), io::ErrorKind::InvalidData)
            && (err.to_string().contains("gap") || err.to_string().contains("missing"))
        {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("retained history is unavailable; rebootstrap required: {err}"),
            )
        } else {
            err
        }
    })?;

    let mut cursors = read_replica_cursors_unlocked(data_dir)?;
    if cursors
        .iter()
        .any(|existing| existing.replica_id == replica_id && existing.active)
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "replica cursor is already active; retire it before bootstrap",
        ));
    }
    if let Some(existing) = cursors
        .iter_mut()
        .find(|existing| existing.replica_id == replica_id)
    {
        *existing = cursor.clone();
    } else {
        cursors.push(cursor.clone());
    }
    validate_cursors_for_write(&cursors)?;
    cursors.sort_by(|a, b| a.replica_id.cmp(&b.replica_id));
    write_replica_cursors_unlocked(&state_dir, cursors)?;
    Ok(cursor)
}

pub(crate) fn with_cursor_metadata_lock<T>(
    data_dir: &Path,
    f: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    let state_dir = sync_state_dir(data_dir);
    create_data_dir_secure(&state_dir)?;
    let _lock = acquire_cursor_lock(&state_dir)?;
    f()
}

pub fn retire_replica_cursor(
    data_dir: &Path,
    replica_id: &str,
    updated_unix_secs: u64,
) -> io::Result<()> {
    validate_replica_id(replica_id).map_err(invalid_input)?;
    let state_dir = sync_state_dir(data_dir);
    create_data_dir_secure(&state_dir)?;
    let _lock = acquire_cursor_lock(&state_dir)?;
    let mut cursors = read_replica_cursors_unlocked(data_dir)?;
    let Some(cursor) = cursors
        .iter_mut()
        .find(|cursor| cursor.replica_id == replica_id)
    else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "replica cursor not found",
        ));
    };
    cursor.active = false;
    cursor.updated_unix_secs = updated_unix_secs;
    write_replica_cursors_unlocked(&state_dir, cursors)
}

/// Return the smallest next-required LSN across active replica cursors.
///
/// This is an advisory snapshot. Do not use it by itself as authority to delete
/// retained history because cursor publication can race an unlocked caller.
/// Use `prune_retained_segments_for_cursors` for deletion; it holds the cursor
/// metadata lock from cursor-floor read through segment removal.
pub fn minimum_retained_lsn(data_dir: &Path) -> io::Result<Option<u64>> {
    let mut min_lsn: Option<u64> = None;
    for cursor in read_replica_cursors(data_dir)? {
        if !cursor.active {
            continue;
        }
        let next_lsn = cursor.next_required_lsn()?;
        min_lsn = Some(match min_lsn {
            Some(current) => current.min(next_lsn),
            None => next_lsn,
        });
    }
    Ok(min_lsn)
}

fn write_new_identity_file(state_dir: &Path, file: &IdentitySnapshot) -> io::Result<()> {
    let final_path = state_dir.join(IDENTITY_FILE);
    let temp_path = temp_path(state_dir, IDENTITY_FILE);
    let bytes = serde_json::to_vec_pretty(file).map_err(io::Error::other)?;
    let result: io::Result<()> = (|| {
        let mut temp = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        temp.write_all(&bytes)?;
        temp.sync_all()?;
        drop(temp);
        fs::hard_link(&temp_path, &final_path)?;
        fsync_dir(state_dir)?;
        let _ = fs::remove_file(&temp_path);
        let _ = fsync_dir(state_dir);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

pub(crate) fn atomic_replace_json<T: Serialize>(
    state_dir: &Path,
    file_name: &str,
    value: &T,
) -> io::Result<()> {
    let final_path = state_dir.join(file_name);
    let temp_path = temp_path(state_dir, file_name);
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    let result: io::Result<()> = (|| {
        let mut temp = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        temp.write_all(&bytes)?;
        temp.sync_all()?;
        drop(temp);
        fs::rename(&temp_path, &final_path)?;
        fsync_dir(state_dir)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

struct CursorLock {
    path: PathBuf,
    dir: PathBuf,
}

impl Drop for CursorLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fsync_dir(&self.dir);
    }
}

/// How long to wait before the next attempt at the cursor lock.
///
/// Doubles from [`CURSOR_LOCK_RETRY`] to [`CURSOR_LOCK_RETRY_MAX`], then stays
/// there, and every result is jittered across the lower half of its step.
///
/// The jitter is the point. A fixed delay makes every waiter sleep the same
/// amount and therefore wake together, so N contenders re-collide on the same
/// `create_new` on every round and only the scheduler decides who wins. Nothing
/// in that arrangement guarantees a given waiter ever wins. Spreading the wakeups
/// turns the herd into a queue.
fn cursor_lock_backoff(attempt: u32) -> Duration {
    let step = CURSOR_LOCK_RETRY
        .saturating_mul(1u32 << attempt.min(5))
        .min(CURSOR_LOCK_RETRY_MAX);
    // Cheap decorrelation: the low bits of the clock differ between threads that
    // reached this line at slightly different times, which is exactly the set of
    // threads we are trying to separate. No RNG dependency for a sleep length.
    let spread = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    let half = step.as_nanos() as u64 / 2;
    let jitter = if half == 0 { 0 } else { spread % half };
    step - Duration::from_nanos(jitter)
}

fn acquire_cursor_lock(state_dir: &Path) -> io::Result<CursorLock> {
    let path = state_dir.join(CURSOR_LOCK_FILE);
    let start = Instant::now();
    let mut attempt: u32 = 0;
    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let result: io::Result<()> = (|| {
                    let owner = CursorLockFile {
                        format_version: SYNC_METADATA_FORMAT_VERSION,
                        owner_pid: std::process::id(),
                        created_unix_secs: now_unix_secs(),
                    };
                    let bytes = serde_json::to_vec_pretty(&owner).map_err(io::Error::other)?;
                    file.write_all(&bytes)?;
                    file.write_all(b"\n")?;
                    file.sync_all()?;
                    fsync_dir(state_dir)
                })();
                if let Err(err) = result {
                    let _ = fs::remove_file(&path);
                    let _ = fsync_dir(state_dir);
                    return Err(err);
                }
                return Ok(CursorLock {
                    path,
                    dir: state_dir.to_path_buf(),
                });
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                if reclaim_stale_cursor_lock(&path, state_dir)? {
                    continue;
                }
                if start.elapsed() >= CURSOR_LOCK_TIMEOUT {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "timed out waiting for sync cursor metadata lock",
                    ));
                }
                std::thread::sleep(cursor_lock_backoff(attempt));
                attempt = attempt.saturating_add(1);
            }
            Err(err) => return Err(err),
        }
    }
}

fn reclaim_stale_cursor_lock(path: &Path, state_dir: &Path) -> io::Result<bool> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(err) => return Err(err),
    };
    let stale = match serde_json::from_slice::<CursorLockFile>(&bytes) {
        Ok(owner) => cursor_lock_owner_stale(&owner),
        Err(_) => lock_file_age(path)?
            .map(|age| age >= CURSOR_LOCK_STALE_AFTER)
            .unwrap_or(false),
    };
    if !stale {
        return Ok(false);
    }
    match fs::remove_file(path) {
        Ok(()) => {
            fsync_dir(state_dir)?;
            Ok(true)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(err) => Err(err),
    }
}

fn cursor_lock_owner_stale(owner: &CursorLockFile) -> bool {
    if owner.format_version != SYNC_METADATA_FORMAT_VERSION {
        return lock_owner_age(owner)
            .map(|age| age >= CURSOR_LOCK_STALE_AFTER)
            .unwrap_or(false);
    }
    if owner.owner_pid == 0 {
        return true;
    }
    if !owner_process_alive(owner.owner_pid) {
        return true;
    }
    lock_owner_stale_without_process_probe(owner)
}

#[cfg(unix)]
fn lock_owner_stale_without_process_probe(_owner: &CursorLockFile) -> bool {
    false
}

#[cfg(not(unix))]
fn lock_owner_stale_without_process_probe(owner: &CursorLockFile) -> bool {
    lock_owner_age(owner)
        .map(|age| age >= CURSOR_LOCK_STALE_AFTER)
        .unwrap_or(false)
}

#[cfg(unix)]
fn owner_process_alive(pid: u32) -> bool {
    if pid > libc::pid_t::MAX as u32 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    let err = io::Error::last_os_error();
    err.raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn owner_process_alive(_pid: u32) -> bool {
    true
}

fn lock_owner_age(owner: &CursorLockFile) -> Option<Duration> {
    Some(Duration::from_secs(
        now_unix_secs().checked_sub(owner.created_unix_secs)?,
    ))
}

fn lock_file_age(path: &Path) -> io::Result<Option<Duration>> {
    let modified = fs::metadata(path)?.modified()?;
    Ok(SystemTime::now().duration_since(modified).ok())
}

fn validate_identity(identity: DatabaseIdentity) -> io::Result<()> {
    if identity.primary_generation == 0 {
        return Err(invalid_data("primary generation must be non-zero"));
    }
    if identity.database_id == [0; 16] {
        return Err(invalid_data("database id must be non-zero"));
    }
    Ok(())
}

fn validate_cursor_file(file: &CursorFile) -> io::Result<()> {
    if file.format_version != SYNC_METADATA_FORMAT_VERSION {
        return Err(invalid_data(format!(
            "unsupported sync cursor format {}",
            file.format_version
        )));
    }
    validate_cursors(&file.cursors, io::ErrorKind::InvalidData)
}

fn validate_cursors_for_write(cursors: &[ReplicaCursor]) -> io::Result<()> {
    validate_cursors(cursors, io::ErrorKind::InvalidInput)
}

fn validate_cursor_for_write(cursor: &ReplicaCursor) -> io::Result<()> {
    validate_replica_id(&cursor.replica_id).map_err(invalid_input)?;
    cursor
        .next_required_lsn()
        .map_err(|err| invalid_input(err.to_string()))?;
    Ok(())
}

fn validate_active_cursors_against_retained_tail(
    data_dir: &Path,
    cursors: &[ReplicaCursor],
) -> io::Result<()> {
    for cursor in cursors {
        validate_active_cursor_against_retained_tail(data_dir, cursor)?;
    }
    Ok(())
}

fn validate_active_cursor_against_retained_tail(
    data_dir: &Path,
    cursor: &ReplicaCursor,
) -> io::Result<()> {
    if !cursor.active {
        return Ok(());
    }
    let identity = match read_identity(data_dir) {
        Ok(identity) => identity,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    let segment_dir = crate::retained_segments_dir(data_dir);
    if crate::list_segment_files(&segment_dir)?.is_empty() {
        return Ok(());
    }
    match read_units_since(
        &segment_dir,
        identity.segment_identity(),
        cursor.applied_lsn,
        1,
    ) {
        Ok(_) => Ok(()),
        Err(err) if err.to_string().contains("gap") => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "replica cursor is behind retained history; rebootstrap required",
        )),
        Err(err) => Err(err),
    }
}

fn validate_cursors(cursors: &[ReplicaCursor], kind: io::ErrorKind) -> io::Result<()> {
    let mut seen = HashSet::new();
    for cursor in cursors {
        validate_replica_id(&cursor.replica_id)
            .map_err(|err| io::Error::new(kind, err.to_string()))?;
        cursor
            .next_required_lsn()
            .map_err(|err| io::Error::new(kind, err.to_string()))?;
        if !seen.insert(cursor.replica_id.as_str()) {
            return Err(io::Error::new(kind, "duplicate replica cursor id"));
        }
    }
    Ok(())
}

fn validate_replica_id(replica_id: &str) -> Result<(), &'static str> {
    if replica_id.is_empty() {
        return Err("replica id must be non-empty");
    }
    if replica_id.len() > 128 {
        return Err("replica id must be at most 128 bytes");
    }
    if !replica_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    {
        return Err("replica id contains unsupported characters");
    }
    Ok(())
}

fn generate_database_id() -> io::Result<[u8; 16]> {
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).map_err(|err| {
        io::Error::other(format!(
            "secure OS random source failed while creating sync database id: {err}"
        ))
    })?;
    if id == [0; 16] {
        return Err(io::Error::other(
            "secure OS random source returned an all-zero sync database id",
        ));
    }
    Ok(id)
}

fn encode_hex_16(bytes: [u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for byte in bytes {
        out.push(hex_digit(byte >> 4));
        out.push(hex_digit(byte & 0x0f));
    }
    out
}

fn decode_hex_16(s: &str) -> io::Result<[u8; 16]> {
    if s.len() != 32 {
        return Err(invalid_data("database id must be 32 hex characters"));
    }
    let mut out = [0u8; 16];
    let bytes = s.as_bytes();
    for i in 0..16 {
        let hi = hex_value(bytes[i * 2])?;
        let lo = hex_value(bytes[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'a' + (nibble - 10)) as char,
        _ => unreachable!("nibble is always four bits"),
    }
}

fn hex_value(byte: u8) -> io::Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(invalid_data("database id contains non-hex byte")),
    }
}

fn temp_path(dir: &Path, file_name: &str) -> PathBuf {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(
        ".{file_name}.tmp.{}.{}.{}",
        std::process::id(),
        now_nanos(),
        counter
    ))
}

pub(crate) fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

#[cfg(unix)]
fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

fn invalid_input(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_string())
}

fn invalid_data(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn identity_is_created_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let first = open_or_create_identity(dir.path()).unwrap();
        let second = open_or_create_identity(dir.path()).unwrap();

        assert_eq!(first, second);
        assert_ne!(first.database_id, [0; 16]);
        assert_eq!(first.primary_generation, 1);
        assert_eq!(read_identity(dir.path()).unwrap(), first);
        assert_eq!(
            first.segment_identity(),
            SegmentIdentity::current(first.database_id, 1)
        );
    }

    #[test]
    fn concurrent_identity_creation_uses_one_winner() {
        let dir = tempfile::tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();

        for _ in 0..8 {
            let data_dir = dir.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                open_or_create_identity(&data_dir)
            }));
        }

        let mut identities = Vec::new();
        for handle in handles {
            identities.push(handle.join().unwrap().unwrap());
        }

        for identity in &identities[1..] {
            assert_eq!(*identity, identities[0]);
        }
    }

    #[test]
    fn corrupt_identity_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = sync_state_dir(dir.path());
        create_data_dir_secure(&state_dir).unwrap();
        fs::write(
            state_dir.join(IDENTITY_FILE),
            br#"{"format_version":1,"database_id":"not-hex","primary_generation":1,"created_unix_secs":1}"#,
        )
        .unwrap();

        let err = read_identity(dir.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn cursors_round_trip_and_minimum_retained_lsn_uses_active_replicas() {
        let dir = tempfile::tempdir().unwrap();
        write_replica_cursors(
            dir.path(),
            vec![
                ReplicaCursor {
                    replica_id: "replica-b".into(),
                    applied_lsn: 40,
                    updated_unix_secs: 2,
                    active: true,
                },
                ReplicaCursor {
                    replica_id: "replica-a".into(),
                    applied_lsn: 10,
                    updated_unix_secs: 1,
                    active: true,
                },
                ReplicaCursor {
                    replica_id: "replica-retired".into(),
                    applied_lsn: 1,
                    updated_unix_secs: 3,
                    active: false,
                },
            ],
        )
        .unwrap();

        let cursors = read_replica_cursors(dir.path()).unwrap();
        assert_eq!(cursors[0].replica_id, "replica-a");
        assert_eq!(cursors[1].replica_id, "replica-b");
        assert_eq!(minimum_retained_lsn(dir.path()).unwrap(), Some(11));
    }

    #[test]
    fn upsert_and_retire_cursor_update_minimum_lsn() {
        let dir = tempfile::tempdir().unwrap();
        upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 5)).unwrap();
        upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-b", 9)).unwrap();
        assert_eq!(minimum_retained_lsn(dir.path()).unwrap(), Some(6));

        upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 12)).unwrap();
        assert_eq!(minimum_retained_lsn(dir.path()).unwrap(), Some(10));

        retire_replica_cursor(dir.path(), "replica-b", 100).unwrap();
        assert_eq!(minimum_retained_lsn(dir.path()).unwrap(), Some(13));
    }

    #[test]
    fn concurrent_upserts_keep_all_active_replicas() {
        let dir = tempfile::tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(16));
        let mut handles = Vec::new();

        for i in 0..16 {
            let data_dir = dir.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                upsert_replica_cursor(
                    &data_dir,
                    ReplicaCursor::active(format!("replica-{i:02}"), 100 + i),
                )
            }));
        }

        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        let cursors = read_replica_cursors(dir.path()).unwrap();
        assert_eq!(cursors.len(), 16);
        assert_eq!(minimum_retained_lsn(dir.path()).unwrap(), Some(101));
    }

    #[test]
    fn cursor_lock_backoff_grows_and_stays_within_its_step() {
        // Each step must sit in the upper half of its nominal delay: jitter
        // only ever shortens a wait, so backoff cannot collapse to a busy loop.
        for attempt in 0..8u32 {
            let nominal = CURSOR_LOCK_RETRY
                .saturating_mul(1u32 << attempt.min(5))
                .min(CURSOR_LOCK_RETRY_MAX);
            let waited = cursor_lock_backoff(attempt);
            assert!(
                waited <= nominal && waited >= nominal / 2,
                "attempt {attempt}: {waited:?} outside the half-open step below {nominal:?}"
            );
        }
        // And it must actually grow, or contention never spreads out.
        assert!(cursor_lock_backoff(4) > cursor_lock_backoff(0));
        // Capped, so a long wait never turns into a multi-second sleep that
        // overshoots the timeout it is supposed to be polling within.
        assert!(cursor_lock_backoff(31) <= CURSOR_LOCK_RETRY_MAX);
    }

    #[test]
    fn cursor_lock_timeout_is_not_shorter_than_the_staleness_window() {
        // Giving up before a dead owner can be declared stale means failing
        // before reclaim -- the only thing that could unblock us -- can run.
        assert!(
            CURSOR_LOCK_TIMEOUT >= CURSOR_LOCK_STALE_AFTER,
            "a caller must not give up before an abandoned lock becomes reclaimable"
        );
    }

    #[test]
    fn stale_cursor_lock_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = sync_state_dir(dir.path());
        create_data_dir_secure(&state_dir).unwrap();
        let stale_owner = CursorLockFile {
            format_version: SYNC_METADATA_FORMAT_VERSION,
            owner_pid: 0,
            created_unix_secs: now_unix_secs(),
        };
        fs::write(
            state_dir.join(CURSOR_LOCK_FILE),
            serde_json::to_vec_pretty(&stale_owner).unwrap(),
        )
        .unwrap();
        fsync_dir(&state_dir).unwrap();

        upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 7)).unwrap();

        let cursors = read_replica_cursors(dir.path()).unwrap();
        assert_eq!(cursors.len(), 1);
        assert_eq!(cursors[0].replica_id, "replica-a");
        assert!(!state_dir.join(CURSOR_LOCK_FILE).exists());
    }

    #[test]
    fn cursor_validation_rejects_duplicate_bad_and_overflowing_ids() {
        let dir = tempfile::tempdir().unwrap();
        let err = write_replica_cursors(
            dir.path(),
            vec![
                ReplicaCursor::active("replica-a", 1),
                ReplicaCursor::active("replica-a", 2),
            ],
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        let err =
            upsert_replica_cursor(dir.path(), ReplicaCursor::active("../bad", 1)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        let err = upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-c", u64::MAX))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn corrupt_cursor_file_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = sync_state_dir(dir.path());
        create_data_dir_secure(&state_dir).unwrap();
        fs::write(
            state_dir.join(REPLICA_CURSORS_FILE),
            br#"{"format_version":999,"cursors":[]}"#,
        )
        .unwrap();

        let err = read_replica_cursors(dir.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
