use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::debug;

/// Process-wide WAL fsync accounting.
///
/// PowDB is a single-writer, fsync-bound engine: how long an fsync takes, and
/// how many are issued, is the single most useful signal an operator has for
/// write-path health. These are plain relaxed atomics updated at the two
/// places that call `sync_data` (the group-commit leader and the Normal-mode
/// background flusher), so a reader (the server's `/metrics` endpoint) can
/// sample them without taking any lock, including the engine lock.
///
/// Process-global rather than per-`Wal` because a PowDB server process serves
/// exactly one data directory, and the metrics endpoint must not reach through
/// the engine `RwLock` to read them.
static FSYNC_TOTAL: AtomicU64 = AtomicU64::new(0);
static FSYNC_NANOS_TOTAL: AtomicU64 = AtomicU64::new(0);
static FSYNC_FAILURES_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Snapshot of the process-wide WAL fsync counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WalFsyncStats {
    /// Successful `sync_data` calls issued against a WAL file.
    pub count: u64,
    /// Total nanoseconds spent inside those calls.
    pub nanos: u64,
    /// `sync_data` calls that returned an error.
    pub failures: u64,
}

/// Read the process-wide WAL fsync counters. Lock-free.
pub fn wal_fsync_stats() -> WalFsyncStats {
    WalFsyncStats {
        count: FSYNC_TOTAL.load(Ordering::Relaxed),
        nanos: FSYNC_NANOS_TOTAL.load(Ordering::Relaxed),
        failures: FSYNC_FAILURES_TOTAL.load(Ordering::Relaxed),
    }
}

/// Run `sync_data` on `file`, recording its duration in the process-wide
/// counters. Every WAL fsync goes through here so the accounting cannot drift
/// from the actual calls.
fn timed_sync_data(file: &File) -> io::Result<()> {
    let started = std::time::Instant::now();
    let result = file.sync_data();
    match &result {
        Ok(()) => {
            FSYNC_TOTAL.fetch_add(1, Ordering::Relaxed);
            FSYNC_NANOS_TOTAL.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        Err(_) => {
            FSYNC_FAILURES_TOTAL.fetch_add(1, Ordering::Relaxed);
        }
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WalRecordType {
    Insert = 1,
    Update = 2,
    Delete = 3,
    Commit = 4,
    Rollback = 5,
    DdlCreateTable = 6,
    DdlDropTable = 7,
    DdlAddColumn = 8,
    DdlDropColumn = 9,
    Begin = 10,
    /// Physical log of one overflow-chain chunk (door D4). Payload:
    /// `page_id u32 | next_page u32 | chunk_len u16 | chunk bytes`. Replayed
    /// by page id under the per-page LSN skip, so it is idempotent.
    OverflowWrite = 11,
    /// Batch of overflow pages returned to the free list (door D4). Payload:
    /// `count u32 | page_id u32 x count`. Idempotent on replay.
    OverflowFree = 12,
}

impl WalRecordType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(WalRecordType::Insert),
            2 => Some(WalRecordType::Update),
            3 => Some(WalRecordType::Delete),
            4 => Some(WalRecordType::Commit),
            5 => Some(WalRecordType::Rollback),
            6 => Some(WalRecordType::DdlCreateTable),
            7 => Some(WalRecordType::DdlDropTable),
            8 => Some(WalRecordType::DdlAddColumn),
            9 => Some(WalRecordType::DdlDropColumn),
            10 => Some(WalRecordType::Begin),
            11 => Some(WalRecordType::OverflowWrite),
            12 => Some(WalRecordType::OverflowFree),
            _ => None,
        }
    }
}

pub const WAL_MAGIC: &[u8; 4] = b"PWAL";
pub const WAL_FORMAT_VERSION: u16 = 1;
const WAL_FILE_HEADER_SIZE: u64 = 8;

/// WAL record header: len(4) + crc32(4) + tx_id(8) + type(1) + lsn(8) = 25 bytes
const WAL_HEADER_SIZE: usize = 25;

fn write_wal_file_header(file: &mut File) -> io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.write_all(WAL_MAGIC)?;
    file.write_all(&WAL_FORMAT_VERSION.to_le_bytes())?;
    file.write_all(&0u16.to_le_bytes())?;
    file.seek(SeekFrom::End(0))?;
    Ok(())
}

fn wal_records_start(file: &mut File) -> io::Result<u64> {
    let len = file.metadata()?.len();
    if len == 0 {
        write_wal_file_header(file)?;
        return Ok(WAL_FILE_HEADER_SIZE);
    }
    if len >= WAL_FILE_HEADER_SIZE {
        file.seek(SeekFrom::Start(0))?;
        let mut hdr = [0u8; WAL_FILE_HEADER_SIZE as usize];
        file.read_exact(&mut hdr)?;
        if &hdr[0..4] == WAL_MAGIC {
            let version = u16::from_le_bytes(hdr[4..6].try_into().expect("2-byte WAL version"));
            if version != WAL_FORMAT_VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported WAL format version: {version}"),
                ));
            }
            return Ok(WAL_FILE_HEADER_SIZE);
        }
    }
    // Legacy 0.4.x WAL: no file header; records start at byte 0.
    Ok(0)
}

/// Maximum allowed size for a single WAL record's data payload.
/// Records claiming more than 256 MB are treated as corruption and
/// stop replay — this prevents a crafted WAL from causing a
/// multi-gigabyte allocation before the CRC check can reject it.
const MAX_WAL_RECORD_SIZE: usize = 256 * 1024 * 1024;

#[derive(Debug)]
pub struct WalRecord {
    pub tx_id: u64,
    pub record_type: WalRecordType,
    /// Monotonic log sequence number assigned at append time. Used by
    /// the page-level idempotent replay: if a page's on-disk LSN is
    /// `>=` this record's LSN, the record has already been applied and
    /// replay skips it.
    pub lsn: u64,
    pub data: Vec<u8>,
}

/// Durability mode for the WAL — analogous to SQLite's `PRAGMA synchronous`
/// combined with `journal_mode=OFF`.
///
/// * `Full` — every mutation appends a record and `flush()` calls
///   `sync_data()` so the OS guarantees the bytes hit stable storage before
///   the call returns. This is the default and the only safe choice when
///   crash recovery must be perfect.
///
/// * `Off`  — every `append()` and `flush()` is a zero-work no-op. No CRC,
///   no BufWriter, no fsync, no recovery. This matches SQLite's `:memory:`
///   semantics and is the only way to compare apples-to-apples against
///   in-memory engines in benches. Never use this in production — a crash
///   loses every mutation since the last `Catalog::checkpoint()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WalSyncMode {
    #[default]
    Full,
    /// `Normal` — every commit buffers its record through to the OS
    /// (`BufWriter::flush`, so the bytes are file-visible) and returns
    /// WITHOUT an fsync; a background flusher fsyncs on a fixed interval
    /// (`NORMAL_FSYNC_INTERVAL`). A *process* crash loses nothing (replay
    /// reads the bytes already in the OS page cache); an *OS* crash / power
    /// loss can lose only the unsynced tail (≤ one interval of writes). This
    /// is SQLite `synchronous=NORMAL` / Postgres `synchronous_commit=off`
    /// semantics: opt-in, bounded-loss, and ~15–40× faster single-row writes
    /// because the fsync leaves the commit/lock path.
    Normal,
    Off,
}

/// How often the background flusher fsyncs in [`WalSyncMode::Normal`]. This is
/// the upper bound on the crash-loss window (OS-crash / power-loss only).
const NORMAL_FSYNC_INTERVAL: Duration = Duration::from_millis(10);

/// Fsync-coordination state shared between the `Wal`, the Normal-mode
/// background flusher, and any outstanding [`WalDurabilityTicket`]s.
///
/// This is the heart of Full-mode group commit: `dirty_gen` counts
/// flush-to-OS generations, `synced_gen` tracks the highest generation an
/// fsync has covered, and `sync_file` is both the fd fsyncs go through and
/// the leader election — whoever holds the mutex fsyncs on behalf of every
/// generation registered before the fsync started.
#[derive(Debug)]
struct WalSyncShared {
    /// Monotonic counter bumped on every durable-intent flush-to-OS (non-Off).
    /// A generation is only registered after its bytes reached the OS file
    /// (`BufWriter::flush`), so an fsync issued afterwards always covers it.
    dirty_gen: AtomicU64,
    /// Highest `dirty_gen` value known to be fsync-durable. Advanced by
    /// group-commit leaders and by the Normal background flusher.
    synced_gen: AtomicU64,
    /// Number of `sync_data` calls issued on the WAL file. Test/metrics hook:
    /// group-commit coalescing shows up as fewer fsyncs than commits.
    fsync_count: AtomicU64,
    /// The fd used for fsyncs, doubling as the group-commit leader lock.
    /// `None` only if cloning the writer's fd failed on (re)open.
    sync_file: Mutex<Option<File>>,
}

impl WalSyncShared {
    fn new(sync_file: Option<File>) -> Self {
        WalSyncShared {
            dirty_gen: AtomicU64::new(0),
            synced_gen: AtomicU64::new(0),
            fsync_count: AtomicU64::new(0),
            sync_file: Mutex::new(sync_file),
        }
    }

    /// Block until an fsync covering `gen` has completed (leader/follower
    /// group commit). The first caller to take the lock fsyncs once for every
    /// generation registered so far; callers queued behind it wake up already
    /// covered and return without an fsync of their own. A lone caller finds
    /// the lock free and fsyncs immediately — group commit never introduces a
    /// wait for company.
    fn sync_until(&self, gen: u64) -> io::Result<()> {
        if self.synced_gen.load(Ordering::Acquire) >= gen {
            return Ok(());
        }
        let guard = self
            .sync_file
            .lock()
            .map_err(|_| io::Error::other("WAL sync lock poisoned"))?;
        // A leader that ran while we were queued may already have covered us.
        if self.synced_gen.load(Ordering::Acquire) >= gen {
            return Ok(());
        }
        let file = guard
            .as_ref()
            .ok_or_else(|| io::Error::other("WAL sync fd unavailable"))?;
        // Snapshot BEFORE the fsync: every generation registered by now has
        // its bytes in the OS file already, so this one fsync covers them all.
        let cover = self.dirty_gen.load(Ordering::Acquire);
        timed_sync_data(file)?;
        self.fsync_count.fetch_add(1, Ordering::Relaxed);
        self.synced_gen.fetch_max(cover, Ordering::AcqRel);
        Ok(())
    }

    /// Swap the fsync fd and mark every generation registered so far as
    /// settled. Called when the WAL file is truncated or recreated: the bytes
    /// those generations covered are gone from the log — either already
    /// durable elsewhere (checkpoint flushed the heaps; the discard paths
    /// `sync_data` the truncated file) or intentionally discarded by rollback
    /// — so no ticket must ever block on them again.
    fn replace_file(&self, file: Option<File>) {
        // Take the leader lock so an in-flight fsync on the old fd finishes
        // before the swap. Poisoning is impossible in practice (the critical
        // section cannot panic) but recover anyway rather than propagate.
        let mut guard = match self.sync_file.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let d = self.dirty_gen.load(Ordering::Acquire);
        self.synced_gen.fetch_max(d, Ordering::AcqRel);
        *guard = file;
    }
}

/// A claim on WAL durability handed out by a deferred Full-mode flush: the
/// commit's records have reached the OS file but are not yet guaranteed on
/// stable storage. [`Self::wait`] blocks until an fsync covering them has
/// completed — the caller must not acknowledge the commit before `wait`
/// returns `Ok(())`.
///
/// Tickets are cumulative: generations are registered in order, so waiting on
/// a later ticket also makes every earlier generation durable. Waiting takes
/// no `Wal` lock, which is what lets a committer release the engine's write
/// lock first and other committers append while the fsync runs — the overlap
/// that lets one fsync cover many commits.
#[derive(Debug)]
#[must_use = "a commit must not be acknowledged until wait() returns Ok"]
pub struct WalDurabilityTicket {
    gen: u64,
    shared: Arc<WalSyncShared>,
}

impl WalDurabilityTicket {
    /// Block until an fsync covering this ticket's WAL records has completed.
    /// See [`WalSyncShared::sync_until`] for the leader/follower scheme.
    pub fn wait(self) -> io::Result<()> {
        self.shared.sync_until(self.gen)
    }
}

pub struct Wal {
    path: PathBuf,
    writer: Option<BufWriter<File>>,
    batch_size: usize,
    pending: usize,
    sync_mode: WalSyncMode,
    /// Monotonic LSN counter. Starts at 1 (0 means "no WAL record has
    /// ever touched this page") and increments by 1 on every `append`.
    next_lsn: u64,
    /// File length as of the last successful WAL sync/truncate/open.
    ///
    /// `BufWriter` may write large pending records through to the OS file
    /// before [`Self::flush`] is called. Those bytes are file-visible but
    /// not transaction-durable. Rollback truncates back to this boundary so
    /// a same-process reopen cannot replay uncommitted records.
    records_start: u64,
    synced_len: u64,
    /// Group-commit fsync coordination (see [`WalSyncShared`]).
    shared: Arc<WalSyncShared>,
    /// When `true`, a Full-mode `flush()` registers the generation it needs
    /// durable instead of fsyncing inline; [`Self::take_durability_ticket`]
    /// hands the claim to the caller, who must wait on it before
    /// acknowledging the commit. See [`Self::set_defer_sync`].
    defer_sync: bool,
    /// Highest generation registered by deferred flushes since the last
    /// `take_durability_ticket`. Cumulative — a later generation covers all
    /// earlier ones, so overwriting never loses coverage.
    deferred_gen: Option<u64>,
    /// Background fsync thread; present only while in `Normal` mode.
    flusher: Option<Flusher>,
}

/// Background fsync worker for [`WalSyncMode::Normal`]. Owns a cloned WAL file
/// descriptor and fsyncs it on [`NORMAL_FSYNC_INTERVAL`] whenever new bytes
/// have been buffered, keeping the fsync off the commit/lock path. fsync on the
/// cloned fd flushes the same underlying file (inode) the writer appends to.
struct Flusher {
    handle: Option<JoinHandle<()>>,
    /// `(stop, condvar)` — set `stop=true` + notify to wake the thread early.
    ctl: Arc<(Mutex<bool>, Condvar)>,
}

impl Flusher {
    fn spawn(file: File, shared: Arc<WalSyncShared>, interval: Duration) -> Flusher {
        let ctl: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));
        let ctl_thread = Arc::clone(&ctl);
        let handle = std::thread::Builder::new()
            .name("powdb-wal-flusher".into())
            .spawn(move || {
                let (lock, cvar) = &*ctl_thread;
                loop {
                    let stopping = {
                        let stop = lock.lock().expect("wal flusher lock");
                        if *stop {
                            true
                        } else {
                            let (stop, _timeout) =
                                cvar.wait_timeout(stop, interval).expect("wal flusher wait");
                            *stop
                        }
                    };
                    // fsync if the writer has buffered new bytes since last sync.
                    let d = shared.dirty_gen.load(Ordering::Acquire);
                    if d > shared.synced_gen.load(Ordering::Acquire) {
                        match timed_sync_data(&file) {
                            Ok(()) => {
                                shared.fsync_count.fetch_add(1, Ordering::Relaxed);
                                // fetch_max, not store: a Full-mode group
                                // commit may have advanced past `d` between
                                // the load and the fsync (mode switches).
                                shared.synced_gen.fetch_max(d, Ordering::AcqRel);
                            }
                            // In Normal mode this background fsync is the ONLY
                            // durability point. Swallowing the error (the old
                            // `&& .is_ok()`) meant an ENOSPC/EIO would keep the
                            // writer acking commits that never reached stable
                            // storage, with no signal. Surface it; synced_gen
                            // stays un-advanced so the next tick retries.
                            Err(e) => tracing::warn!(
                                error = %e,
                                "WAL background fsync failed; commits since the last \
                                 successful sync are not yet durable (will retry)"
                            ),
                        }
                    }
                    if stopping {
                        break;
                    }
                }
            })
            .expect("spawn wal flusher thread");
        Flusher {
            handle: Some(handle),
            ctl,
        }
    }

    fn stop(&mut self) {
        {
            let (lock, cvar) = &*self.ctl;
            let mut stop = lock.lock().expect("wal flusher lock");
            *stop = true;
            cvar.notify_all();
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Flusher {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Wal {
    pub fn create(path: &Path, batch_size: usize) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(true)
            .open(path)?;
        write_wal_file_header(&mut file)?;
        let sync_fd = file.try_clone()?;
        Ok(Wal {
            path: path.to_path_buf(),
            writer: Some(BufWriter::new(file)),
            batch_size,
            pending: 0,
            sync_mode: WalSyncMode::default(),
            next_lsn: 1,
            records_start: WAL_FILE_HEADER_SIZE,
            synced_len: WAL_FILE_HEADER_SIZE,
            shared: Arc::new(WalSyncShared::new(Some(sync_fd))),
            defer_sync: false,
            deferred_gen: None,
            flusher: None,
        })
    }

    pub fn open(path: &Path, batch_size: usize) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;
        let records_start = wal_records_start(&mut file)?;
        let synced_len = file.metadata()?.len();
        let sync_fd = file.try_clone()?;
        Ok(Wal {
            path: path.to_path_buf(),
            writer: Some(BufWriter::new(file)),
            batch_size,
            pending: 0,
            sync_mode: WalSyncMode::default(),
            next_lsn: 1,
            records_start,
            synced_len,
            shared: Arc::new(WalSyncShared::new(Some(sync_fd))),
            defer_sync: false,
            deferred_gen: None,
            flusher: None,
        })
    }

    /// Toggle the durability mode. See [`WalSyncMode`] for the contract.
    /// Starts the background flusher when entering `Normal`, and stops it when
    /// leaving `Normal`. The fsync-behavior change takes effect on the next
    /// `flush()`.
    pub fn set_sync_mode(&mut self, mode: WalSyncMode) {
        if mode == self.sync_mode {
            return;
        }
        self.sync_mode = mode;
        match mode {
            WalSyncMode::Normal => self.start_flusher(),
            WalSyncMode::Full | WalSyncMode::Off => self.stop_flusher(),
        }
    }

    /// Spawn the Normal-mode background flusher if not already running. The
    /// flusher fsyncs a cloned WAL fd, so it never contends on the writer.
    fn start_flusher(&mut self) {
        if self.flusher.is_some() {
            return;
        }
        if let Some(writer) = self.writer.as_ref() {
            if let Ok(file) = writer.get_ref().try_clone() {
                self.flusher = Some(Flusher::spawn(
                    file,
                    Arc::clone(&self.shared),
                    NORMAL_FSYNC_INTERVAL,
                ));
            }
        }
    }

    /// Stop the background flusher (final fsync + join), if running.
    fn stop_flusher(&mut self) {
        if let Some(mut f) = self.flusher.take() {
            f.stop();
        }
    }

    /// The highest dirty generation known to be fsync-durable. Advances on
    /// every Full commit and on each Normal background-flusher cycle. Exposed
    /// for tests and (future) metrics.
    pub fn synced_generation(&self) -> u64 {
        self.shared.synced_gen.load(Ordering::Acquire)
    }

    /// Number of fsyncs issued against the WAL file (group-commit leaders,
    /// inline Full-mode flushes, and the Normal background flusher). Exposed
    /// for tests and (future) metrics: group-commit coalescing shows up as
    /// fewer fsyncs than commits.
    pub fn fsync_count(&self) -> u64 {
        self.shared.fsync_count.load(Ordering::Relaxed)
    }

    /// Defer Full-mode commit fsyncs. While enabled, [`Self::flush`]
    /// registers the generation it needs durable instead of fsyncing inline;
    /// the pending claim is retrieved with [`Self::take_durability_ticket`]
    /// and the caller must wait on it before acknowledging the commit. This
    /// is how group commit lets the fsync leave the engine's exclusive-lock
    /// hold: append + register under the lock, wait after releasing it.
    ///
    /// `Normal` and `Off` modes are unaffected (they never fsync inline).
    pub fn set_defer_sync(&mut self, defer: bool) {
        self.defer_sync = defer;
    }

    /// Take the durability claim registered by deferred flushes since the
    /// last take, if any. Generations are cumulative, so one ticket covers
    /// every deferred flush that happened before it was taken.
    pub fn take_durability_ticket(&mut self) -> Option<WalDurabilityTicket> {
        self.deferred_gen.take().map(|gen| WalDurabilityTicket {
            gen,
            shared: Arc::clone(&self.shared),
        })
    }

    /// Returns the current sync mode (used by tests + introspection).
    pub fn sync_mode(&self) -> WalSyncMode {
        self.sync_mode
    }

    /// `true` when the WAL is in [`WalSyncMode::Off`] — i.e. every
    /// `append`/`flush` is a no-op. Catalog mutation hot paths check
    /// this BEFORE constructing WAL payloads so we don't pay
    /// `encode_row_into` + `encode_wal_payload` allocs only to throw
    /// the result away inside `append`. This is the difference between
    /// "no fsync" and "free" — the former is still 50–60% slower than
    /// the no-WAL baseline on `update_by_filter`/`delete_by_filter`,
    /// the latter matches the baseline.
    #[inline]
    pub fn is_off(&self) -> bool {
        matches!(self.sync_mode, WalSyncMode::Off)
    }

    /// LSN of the most recently appended record, or 0 if nothing has
    /// been appended yet (or the WAL is off).
    ///
    /// Used by schema-change paths to capture a "barrier LSN" that
    /// reflects the DDL record's position in the log; the heap can then
    /// stamp its pages with that LSN so replay skips every
    /// Insert/Update/Delete that pre-dates the schema change (those rows
    /// have already been migrated to the new layout in place).
    #[inline]
    pub fn last_appended_lsn(&self) -> u64 {
        if matches!(self.sync_mode, WalSyncMode::Off) {
            return 0;
        }
        self.next_lsn.saturating_sub(1)
    }

    /// Ensure the next LSN this WAL hands out is at least `lsn`. Called on
    /// open, after recovery, to restore monotonicity: heap pages carry the
    /// LSNs stamped during replay (and by DDL rewrites), but `Wal::open`
    /// always resets `next_lsn` to 1. Without this, writes taken after a
    /// crash-recovery would reuse LSNs at or below those stamped page LSNs,
    /// and the next crash's replay would skip them as already-applied —
    /// silent data loss. Never lowers the counter.
    pub fn set_next_lsn_at_least(&mut self, lsn: u64) {
        if lsn > self.next_lsn {
            self.next_lsn = lsn;
        }
    }

    /// Append a record to the WAL buffer. Auto-flushes when batch is full.
    ///
    /// In [`WalSyncMode::Off`] this is a zero-work no-op — see the enum's
    /// doc for the durability contract.
    pub fn append(
        &mut self,
        tx_id: u64,
        record_type: WalRecordType,
        data: &[u8],
    ) -> io::Result<()> {
        if matches!(self.sync_mode, WalSyncMode::Off) {
            return Ok(());
        }
        let lsn = self.next_lsn;
        self.next_lsn += 1;
        let total_len = (WAL_HEADER_SIZE + data.len()) as u32;

        // Compute CRC over tx_id + type + lsn + data
        let mut crc_input = Vec::with_capacity(17 + data.len());
        crc_input.extend_from_slice(&tx_id.to_le_bytes());
        crc_input.push(record_type as u8);
        crc_input.extend_from_slice(&lsn.to_le_bytes());
        crc_input.extend_from_slice(data);
        let crc = crc32fast::hash(&crc_input);

        // Write: len + crc + tx_id + type + lsn + data
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::other("WAL writer unavailable"))?;
        writer.write_all(&total_len.to_le_bytes())?;
        writer.write_all(&crc.to_le_bytes())?;
        writer.write_all(&tx_id.to_le_bytes())?;
        writer.write_all(&[record_type as u8])?;
        writer.write_all(&lsn.to_le_bytes())?;
        writer.write_all(data)?;

        self.pending += 1;
        if self.pending >= self.batch_size {
            self.flush()?;
        }
        Ok(())
    }

    /// Flush buffered records to disk (the group commit point).
    ///
    /// In `Full` mode the commit is durable when this returns: the buffered
    /// bytes are pushed to the OS and an fsync covering them completes before
    /// the call returns — unless durability deferral is active (see
    /// [`Self::set_defer_sync`]), in which case the fsync obligation is
    /// registered and handed to the caller via
    /// [`Self::take_durability_ticket`]. Either way, concurrent committers'
    /// fsyncs coalesce: one fsync covers every generation registered before
    /// it started, and a lone committer fsyncs immediately (no batching
    /// delay).
    ///
    /// No-op if nothing has been appended since the last flush. This makes
    /// it safe for the executor to unconditionally call `sync_wal` at the
    /// end of every statement — read queries pay zero fsync cost.
    pub fn flush(&mut self) -> io::Result<()> {
        let Some(gen) = self.flush_to_os()? else {
            return Ok(());
        };
        // SQLite-style synchronous knob: only the fsync is gated on the mode.
        // The flush-to-OS above always runs so a process crash still recovers
        // cleanly via `read_all`. In `Full` the fsync happens here (or via
        // the deferred ticket); in `Normal` the background flusher fsyncs off
        // this path.
        if matches!(self.sync_mode, WalSyncMode::Full) {
            if self.defer_sync {
                // Cumulative: the newest generation covers all earlier ones,
                // so overwriting an untaken claim never loses coverage.
                self.deferred_gen = Some(gen);
            } else {
                self.shared.sync_until(gen)?;
            }
        }
        Ok(())
    }

    /// Push buffered records through to the OS file (no fsync) and register
    /// the resulting dirty generation. Returns `Ok(None)` when there was
    /// nothing pending or the WAL is `Off`.
    fn flush_to_os(&mut self) -> io::Result<Option<u64>> {
        let batch = self.pending;
        if batch == 0 {
            return Ok(None);
        }
        // Borrow the writer only for the I/O, then drop it before touching the
        // generation counters (which borrow `self`).
        let new_len = {
            let writer = self
                .writer
                .as_mut()
                .ok_or_else(|| io::Error::other("WAL writer unavailable"))?;
            writer.flush()?;
            writer.get_ref().metadata()?.len()
        };
        self.synced_len = new_len;
        self.pending = 0;
        if matches!(self.sync_mode, WalSyncMode::Off) {
            return Ok(None);
        }
        // Registered only after the bytes are OS-visible, so any fsync issued
        // from here on covers this generation.
        let gen = self.shared.dirty_gen.fetch_add(1, Ordering::Release) + 1;
        debug!(records = batch, "wal group commit");
        Ok(Some(gen))
    }

    /// True when records have been appended to the in-memory WAL buffer
    /// since the last durable flush.
    #[inline]
    pub fn has_pending(&self) -> bool {
        self.pending > 0
    }

    /// Flush pending WAL bytes, then return the durable file length. Used as
    /// an explicit-transaction rollback boundary.
    pub fn synced_len(&mut self) -> io::Result<u64> {
        self.flush()?;
        Ok(self.synced_len)
    }

    /// Discard buffered (not-yet-flushed) WAL bytes and truncate the durable
    /// log back to `len`. This is intentionally not implemented by dropping
    /// the existing BufWriter: BufWriter's Drop attempts to flush buffered
    /// bytes, which would resurrect rolled-back records.
    pub fn discard_and_truncate_to(&mut self, len: u64) -> io::Result<()> {
        if matches!(self.sync_mode, WalSyncMode::Off) {
            self.pending = 0;
            self.synced_len = len;
            return Ok(());
        }

        if let Some(writer) = self.writer.take() {
            let (_file, _buffer_result) = writer.into_parts();
        }

        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&self.path)?;
        file.set_len(len)?;
        file.seek(SeekFrom::End(0))?;
        file.sync_data()?;
        let sync_fd = file.try_clone()?;
        self.writer = Some(BufWriter::new(file));
        // Everything that survived the truncation was just `sync_data`ed
        // above, and everything past `len` is intentionally discarded; settle
        // all registered generations and drop any deferred claim.
        self.deferred_gen = None;
        self.shared.replace_file(Some(sync_fd));
        self.pending = 0;
        self.synced_len = len;
        Ok(())
    }

    /// Read all valid records from the WAL file.
    pub fn read_all(&self) -> io::Result<Vec<WalRecord>> {
        self.read_through_len(u64::MAX)
    }

    /// Read valid records up to a byte length boundary in the WAL file.
    pub fn read_through_len(&self, max_len: u64) -> io::Result<Vec<WalRecord>> {
        parse_wal_records(&self.path, max_len)
    }
}

/// Parse valid WAL records from `path` up to `max_len` bytes, opening the file
/// **read-only**. This is the single record-decoding loop shared by
/// [`Wal::read_through_len`] and the read-only open path
/// ([`read_records_at_path`]); neither ever writes to the file, so it is safe on
/// a directory opened for read-only snapshot serving.
fn parse_wal_records(path: &Path, max_len: u64) -> io::Result<Vec<WalRecord>> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len().min(max_len);
    let mut file_for_header = File::open(path)?;
    let mut pos = wal_records_start_readonly(&mut file_for_header)?;
    let mut records = Vec::new();

    while let Some((record, next_pos)) = parse_wal_record_at(&mut file, pos, file_len)? {
        records.push(record);
        pos = next_pos;
    }

    Ok(records)
}

/// Parse the single record starting at `pos`, returning it together with the
/// position of the next record. Returns `Ok(None)` at end of file or at the
/// first corrupted/truncated record (the same stop-on-corruption semantics
/// replay has always had).
fn parse_wal_record_at(
    file: &mut File,
    pos: u64,
    file_len: u64,
) -> io::Result<Option<(WalRecord, u64)>> {
    if pos + WAL_HEADER_SIZE as u64 > file_len {
        return Ok(None);
    }
    {
        file.seek(SeekFrom::Start(pos))?;

        let mut header = [0u8; WAL_HEADER_SIZE];
        if file.read_exact(&mut header).is_err() {
            return Ok(None);
        }

        // These slice-to-array conversions are infallible (fixed-size
        // sub-slices of a 17-byte array) but we avoid `unwrap` to
        // satisfy the project-wide zero-panic policy.
        let total_len_bytes: [u8; 4] = match header[0..4].try_into() {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };
        let total_len = u32::from_le_bytes(total_len_bytes) as usize;
        let stored_crc_bytes: [u8; 4] = match header[4..8].try_into() {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };
        let stored_crc = u32::from_le_bytes(stored_crc_bytes);
        let tx_id_bytes: [u8; 8] = match header[8..16].try_into() {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };
        let tx_id = u64::from_le_bytes(tx_id_bytes);
        let record_type = match WalRecordType::from_u8(header[16]) {
            Some(rt) => rt,
            None => return Ok(None),
        };
        let lsn_bytes: [u8; 8] = match header[17..25].try_into() {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };
        let lsn = u64::from_le_bytes(lsn_bytes);

        // TASK-11: Verify the record fits within the file before
        // allocating. Catches truncated writes without any allocation.
        if pos + total_len as u64 > file_len {
            return Ok(None); // Record extends beyond file: truncated write
        }

        // TASK-09: Use checked_sub to prevent integer underflow when
        // a corrupted WAL has total_len < WAL_HEADER_SIZE.
        let data_len = match total_len.checked_sub(WAL_HEADER_SIZE) {
            Some(len) => len,
            None => return Ok(None), // Corrupted record: stop replay
        };

        // TASK-10: Cap allocation size before reading data. A crafted
        // WAL claiming a huge total_len would otherwise allocate
        // gigabytes before the CRC check rejects the record.
        if data_len > MAX_WAL_RECORD_SIZE {
            return Ok(None); // Unreasonably large record: treat as corruption
        }

        let mut data = vec![0u8; data_len];
        if data_len > 0 {
            file.read_exact(&mut data)?;
        }

        // Verify CRC (includes lsn in the hash input)
        let mut crc_input = Vec::with_capacity(17 + data.len());
        crc_input.extend_from_slice(&tx_id.to_le_bytes());
        crc_input.push(record_type as u8);
        crc_input.extend_from_slice(&lsn.to_le_bytes());
        crc_input.extend_from_slice(&data);
        let computed_crc = crc32fast::hash(&crc_input);

        if computed_crc != stored_crc {
            return Ok(None); // Corrupted record: stop here
        }

        Ok(Some((
            WalRecord {
                tx_id,
                record_type,
                lsn,
                data,
            },
            pos + total_len as u64,
        )))
    }
}

/// Report whether `path` holds at least one committed WAL record, without
/// ever opening a writable handle. Returns `false` if the file does not
/// exist. Used by the read-only catalog open path to decide whether a
/// directory is quiescent before serving it, without creating, appending
/// to, or truncating the WAL the way [`Wal::open`] would. Only the first
/// record's header and payload are read; a valid first record is proof
/// enough that the directory needs recovery.
pub fn wal_has_committed_records(path: &Path) -> io::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut file_for_header = File::open(path)?;
    let pos = wal_records_start_readonly(&mut file_for_header)?;
    Ok(parse_wal_record_at(&mut file, pos, file_len)?.is_some())
}

/// Locate the first record byte in a WAL file **without mutating it**. Mirrors
/// [`wal_records_start`] but never writes a header for an empty/headerless file
/// (the read-only path must not touch the directory): it returns the default
/// record start instead.
fn wal_records_start_readonly(file: &mut File) -> io::Result<u64> {
    let len = file.metadata()?.len();
    if len >= WAL_FILE_HEADER_SIZE {
        file.seek(SeekFrom::Start(0))?;
        let mut hdr = [0u8; WAL_FILE_HEADER_SIZE as usize];
        file.read_exact(&mut hdr)?;
        if &hdr[0..4] == WAL_MAGIC {
            let version = u16::from_le_bytes(hdr[4..6].try_into().expect("2-byte WAL version"));
            if version != WAL_FORMAT_VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported WAL format version: {version}"),
                ));
            }
            return Ok(WAL_FILE_HEADER_SIZE);
        }
        // Legacy 0.4.x WAL: no file header; records start at byte 0.
        return Ok(0);
    }
    // Empty or sub-header file: nothing to read, and we must not write a header.
    Ok(WAL_FILE_HEADER_SIZE.min(len))
}

impl Wal {
    /// Open a WAL handle that can never write, for the read-only snapshot-serving
    /// path. The file is opened read-only (no create, no append, no truncate) and
    /// the handle carries no writer; every append/flush/commit path is unreachable
    /// in read-only engine mode. Record reads still work because they reopen the
    /// file read-only on demand.
    pub fn open_read_only(path: &Path, batch_size: usize) -> io::Result<Self> {
        let records_start = if path.exists() {
            let mut f = File::open(path)?;
            wal_records_start_readonly(&mut f)?
        } else {
            WAL_FILE_HEADER_SIZE
        };
        Ok(Wal {
            path: path.to_path_buf(),
            writer: None,
            batch_size,
            pending: 0,
            sync_mode: WalSyncMode::Off,
            next_lsn: 1,
            records_start,
            synced_len: records_start,
            shared: Arc::new(WalSyncShared::new(None)),
            defer_sync: false,
            deferred_gen: None,
            flusher: None,
        })
    }

    /// Truncate the WAL (after checkpoint).
    pub fn truncate(&mut self) -> io::Result<()> {
        // Settle any deferred durability claim before destroying the records
        // it covers: this keeps the "WAL records are durable before truncate"
        // ordering airtight even if a caller checkpoints while deferral is
        // active.
        if let Some(gen) = self.deferred_gen.take() {
            self.shared.sync_until(gen)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .read(true)
            .truncate(true)
            .open(&self.path)?;
        write_wal_file_header(&mut file)?;
        let sync_fd = file.try_clone()?;
        self.writer = Some(BufWriter::new(file));
        // The old records are gone; settle their generations and swap the
        // fsync fd so outstanding tickets can never block on them.
        self.shared.replace_file(Some(sync_fd));
        self.records_start = WAL_FILE_HEADER_SIZE;
        self.pending = 0;
        self.synced_len = WAL_FILE_HEADER_SIZE;
        Ok(())
    }

    /// Discard records appended since the last successful [`Self::flush`].
    ///
    /// This is intentionally different from `flush`: it must not flush the
    /// current `BufWriter`, because rollback uses it to abandon uncommitted
    /// transaction records. `BufWriter::into_parts` lets us drop the buffered
    /// bytes without writing them, then we truncate any large records that
    /// had already spilled through to the file back to the last synced
    /// boundary.
    pub fn discard_pending(&mut self) -> io::Result<()> {
        if matches!(self.sync_mode, WalSyncMode::Off) {
            self.pending = 0;
            return Ok(());
        }

        if let Some(writer) = self.writer.take() {
            let (_file, _buffer) = writer.into_parts();
        }

        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .truncate(false)
            .open(&self.path)?;
        file.set_len(self.synced_len)?;
        file.sync_data()?;
        let sync_fd = file.try_clone()?;
        self.writer = Some(BufWriter::new(file));
        // The surviving prefix was just `sync_data`ed; settle all registered
        // generations and drop any deferred claim over discarded bytes.
        self.deferred_gen = None;
        self.shared.replace_file(Some(sync_fd));
        self.pending = 0;
        self.synced_len = self.records_start;
        Ok(())
    }
}

impl Drop for Wal {
    fn drop(&mut self) {
        // Clean shutdown must be durable regardless of mode: push any buffered
        // bytes to the OS and fsync, so a Normal-mode commit that hasn't yet
        // hit the background flusher's interval is still durable on a graceful
        // exit. (A process *crash* skips this — Normal's bounded-loss contract
        // only applies to OS-crash / power-loss, which this cannot help.)
        if !matches!(self.sync_mode, WalSyncMode::Off) {
            if let Some(writer) = self.writer.as_mut() {
                let _ = writer.flush();
                let _ = writer.get_ref().sync_data();
            }
        }
        self.stop_flusher();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_wal(name: &str) -> (Wal, PathBuf) {
        let path = std::env::temp_dir().join(format!("powdb_wal_{name}_{}", std::process::id()));
        let wal = Wal::create(&path, 4).unwrap();
        (wal, path)
    }

    #[test]
    fn test_append_and_flush() {
        let (mut wal, path) = temp_wal("basic");
        wal.append(1, WalRecordType::Insert, b"row data 1").unwrap();
        wal.append(1, WalRecordType::Insert, b"row data 2").unwrap();
        wal.flush().unwrap();

        let records = wal.read_all().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].tx_id, 1);
        assert_eq!(records[0].data, b"row data 1");
        assert_eq!(records[1].data, b"row data 2");
        drop(wal);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_group_commit_auto_flush() {
        let (mut wal, path) = temp_wal("group");
        // Batch size is 4 — after 4 appends, should auto-flush
        for i in 0..4 {
            wal.append(1, WalRecordType::Insert, format!("row {i}").as_bytes())
                .unwrap();
        }
        // Should have flushed automatically
        let records = wal.read_all().unwrap();
        assert_eq!(records.len(), 4);
        drop(wal);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_normal_mode_persists_records_across_reopen() {
        // NORMAL durability: commits are acked after the buffered bytes reach
        // the OS (BufWriter::flush) without a per-commit fsync; a background
        // flusher + clean shutdown make them durable. Data must survive a
        // clean close + reopen.
        let path =
            std::env::temp_dir().join(format!("powdb_wal_normal_reopen_{}", std::process::id()));
        std::fs::remove_file(&path).ok();
        {
            let mut wal = Wal::create(&path, 4).unwrap();
            wal.set_sync_mode(WalSyncMode::Normal);
            assert_eq!(wal.sync_mode(), WalSyncMode::Normal);
            wal.append(1, WalRecordType::Insert, b"n1").unwrap();
            wal.append(1, WalRecordType::Insert, b"n2").unwrap();
            wal.flush().unwrap();
        } // drop: stop flusher + final fsync
        let wal = Wal::open(&path, 4).unwrap();
        let records = wal.read_all().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].data, b"n1");
        assert_eq!(records[1].data, b"n2");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_normal_mode_background_flusher_syncs_off_commit_path() {
        // In NORMAL mode flush() must NOT fsync inline; the background flusher
        // fsyncs on its interval and advances the synced generation. Proves the
        // fsync is off the commit path (the latency win) yet still happens.
        let path = std::env::temp_dir().join(format!("powdb_wal_normal_bg_{}", std::process::id()));
        std::fs::remove_file(&path).ok();
        let mut wal = Wal::create(&path, 1000).unwrap(); // large batch: no auto-flush
        wal.set_sync_mode(WalSyncMode::Normal);
        wal.append(1, WalRecordType::Insert, b"bg1").unwrap();
        wal.flush().unwrap(); // buffers to OS + marks dirty; no inline fsync
                              // The background flusher fsyncs on its (~10 ms) interval. Poll
                              // rather than sleeping a fixed 80 ms: on loaded CI runners the
                              // flusher thread can be starved well past one interval, and the
                              // property under test is "it happens off the commit path", not
                              // "it happens within 80 ms".
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while wal.synced_generation() < 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            wal.synced_generation() >= 1,
            "background flusher did not sync within 2s (synced_generation = {})",
            wal.synced_generation()
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_lone_committer_fsyncs_immediately_per_commit() {
        // Group commit must never delay a lone committer: with no other
        // waiters, every flush fsyncs immediately — exactly one fsync per
        // commit, no timers, no batching window.
        let (mut wal, path) = temp_wal("lone_committer");
        let base = wal.fsync_count();
        for i in 0..10u32 {
            wal.append(1, WalRecordType::Insert, format!("c{i}").as_bytes())
                .unwrap();
            wal.flush().unwrap();
        }
        assert_eq!(
            wal.fsync_count() - base,
            10,
            "a lone sequential committer must fsync exactly once per commit"
        );
        drop(wal);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_deferred_tickets_coalesce_one_fsync_for_two_commits() {
        // Two commits registered before either waits: the first wait's fsync
        // covers both generations, the second wait returns without an fsync.
        let path = std::env::temp_dir().join(format!(
            "powdb_wal_gc_coalesce2_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut wal = Wal::create(&path, 1024).unwrap();
        wal.set_defer_sync(true);

        wal.append(1, WalRecordType::Insert, b"a").unwrap();
        wal.flush().unwrap();
        let t1 = wal.take_durability_ticket().expect("ticket for commit 1");

        wal.append(2, WalRecordType::Insert, b"b").unwrap();
        wal.flush().unwrap();
        let t2 = wal.take_durability_ticket().expect("ticket for commit 2");

        let base = wal.fsync_count();
        t2.wait().unwrap(); // leader — its fsync covers generation 1 too
        t1.wait().unwrap(); // already covered, no second fsync
        assert_eq!(
            wal.fsync_count() - base,
            1,
            "one fsync must cover both queued commits"
        );
        assert_eq!(wal.read_all().unwrap().len(), 2);
        drop(wal);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_concurrent_committers_share_one_fsync() {
        // Classic group commit: N committers append + register (serialized by
        // the writer lock), all reach the barrier before any of them waits,
        // then the first waiter's fsync covers every registered generation.
        use std::sync::Barrier;

        let path = std::env::temp_dir().join(format!(
            "powdb_wal_gc_concurrent_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let wal = Arc::new(Mutex::new(Wal::create(&path, 1024).unwrap()));
        wal.lock().unwrap().set_defer_sync(true);

        let n = 8;
        let barrier = Arc::new(Barrier::new(n));
        let mut handles = Vec::new();
        for t in 0..n {
            let wal = Arc::clone(&wal);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let ticket = {
                    let mut w = wal.lock().unwrap();
                    w.append(t as u64 + 1, WalRecordType::Insert, b"row")
                        .unwrap();
                    w.flush().unwrap();
                    w.take_durability_ticket().expect("deferred ticket")
                };
                barrier.wait();
                ticket.wait().unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let w = wal.lock().unwrap();
        assert_eq!(w.read_all().unwrap().len(), n);
        assert_eq!(
            w.fsync_count(),
            1,
            "all {n} overlapping commits must be covered by a single fsync"
        );
        drop(w);
        drop(wal);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_crc_integrity() {
        let (mut wal, path) = temp_wal("crc");
        wal.append(1, WalRecordType::Insert, b"important data")
            .unwrap();
        wal.flush().unwrap();

        let records = wal.read_all().unwrap();
        assert_eq!(records.len(), 1);
        // CRC was validated during read_all — if we get here, integrity is good
        drop(wal);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_multiple_transactions() {
        let (mut wal, path) = temp_wal("multi_tx");
        wal.append(1, WalRecordType::Insert, b"tx1 op1").unwrap();
        wal.append(2, WalRecordType::Insert, b"tx2 op1").unwrap();
        wal.append(1, WalRecordType::Commit, b"").unwrap();
        wal.append(2, WalRecordType::Commit, b"").unwrap();
        wal.flush().unwrap();

        let records = wal.read_all().unwrap();
        assert_eq!(records.len(), 4);
        assert_eq!(records[0].tx_id, 1);
        assert_eq!(records[2].tx_id, 1);
        assert_eq!(records[2].record_type, WalRecordType::Commit);
        drop(wal);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_overflow_record_types_roundtrip() {
        // Additive record types 11/12 append, flush, and read back with their
        // type + payload intact, alongside the existing Insert/Commit records.
        let (mut wal, path) = temp_wal("ovf_types");
        wal.append(1, WalRecordType::OverflowWrite, b"chunk-payload")
            .unwrap();
        wal.append(1, WalRecordType::OverflowFree, b"\x02\x00\x00\x00")
            .unwrap();
        wal.append(1, WalRecordType::Insert, b"stub-row").unwrap();
        wal.append(1, WalRecordType::Commit, b"").unwrap();
        wal.flush().unwrap();

        let records = wal.read_all().unwrap();
        assert_eq!(records.len(), 4);
        assert_eq!(records[0].record_type, WalRecordType::OverflowWrite);
        assert_eq!(records[0].data, b"chunk-payload");
        assert_eq!(records[1].record_type, WalRecordType::OverflowFree);
        assert_eq!(records[2].record_type, WalRecordType::Insert);
        assert_eq!(records[3].record_type, WalRecordType::Commit);
        assert_eq!(
            WalRecordType::from_u8(11),
            Some(WalRecordType::OverflowWrite)
        );
        assert_eq!(
            WalRecordType::from_u8(12),
            Some(WalRecordType::OverflowFree)
        );
        drop(wal);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_truncate() {
        let (mut wal, path) = temp_wal("trunc");
        for i in 0..8 {
            wal.append(1, WalRecordType::Insert, format!("data {i}").as_bytes())
                .unwrap();
        }
        wal.flush().unwrap();
        assert_eq!(wal.read_all().unwrap().len(), 8);

        wal.truncate().unwrap();
        assert_eq!(wal.read_all().unwrap().len(), 0);
        drop(wal);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_reopen_wal() {
        let path = std::env::temp_dir().join(format!("powdb_wal_reopen_{}", std::process::id()));
        {
            let mut wal = Wal::create(&path, 128).unwrap();
            wal.append(1, WalRecordType::Insert, b"persistent").unwrap();
            wal.append(1, WalRecordType::Commit, b"").unwrap();
            wal.flush().unwrap();
        }
        {
            let wal = Wal::open(&path, 128).unwrap();
            let records = wal.read_all().unwrap();
            assert_eq!(records.len(), 2);
            assert_eq!(records[0].data, b"persistent");
            assert_eq!(records[1].record_type, WalRecordType::Commit);
        }
        std::fs::remove_file(&path).ok();
    }
}
