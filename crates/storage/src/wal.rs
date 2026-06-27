use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::debug;

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
}

impl WalRecordType {
    fn from_u8(v: u8) -> Option<Self> {
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
    /// ([`NORMAL_FSYNC_INTERVAL`]). A *process* crash loses nothing (replay
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
    /// Monotonic counter bumped on every durable-intent `flush()` (non-Off).
    /// The Normal background flusher fsyncs whenever `dirty_gen > synced_gen`.
    dirty_gen: Arc<AtomicU64>,
    /// Highest `dirty_gen` value known to be fsync-durable. Advanced by Full's
    /// inline fsync and by the Normal background flusher.
    synced_gen: Arc<AtomicU64>,
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
    fn spawn(
        file: File,
        dirty_gen: Arc<AtomicU64>,
        synced_gen: Arc<AtomicU64>,
        interval: Duration,
    ) -> Flusher {
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
                    let d = dirty_gen.load(Ordering::Acquire);
                    if d > synced_gen.load(Ordering::Acquire) && file.sync_data().is_ok() {
                        synced_gen.store(d, Ordering::Release);
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
        Ok(Wal {
            path: path.to_path_buf(),
            writer: Some(BufWriter::new(file)),
            batch_size,
            pending: 0,
            sync_mode: WalSyncMode::default(),
            next_lsn: 1,
            records_start: WAL_FILE_HEADER_SIZE,
            synced_len: WAL_FILE_HEADER_SIZE,
            dirty_gen: Arc::new(AtomicU64::new(0)),
            synced_gen: Arc::new(AtomicU64::new(0)),
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
        Ok(Wal {
            path: path.to_path_buf(),
            writer: Some(BufWriter::new(file)),
            batch_size,
            pending: 0,
            sync_mode: WalSyncMode::default(),
            next_lsn: 1,
            records_start,
            synced_len,
            dirty_gen: Arc::new(AtomicU64::new(0)),
            synced_gen: Arc::new(AtomicU64::new(0)),
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
                    Arc::clone(&self.dirty_gen),
                    Arc::clone(&self.synced_gen),
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
        self.synced_gen.load(Ordering::Acquire)
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

    /// Flush buffered records to disk with fsync (the group commit point).
    ///
    /// No-op if nothing has been appended since the last flush. This makes
    /// it safe for the executor to unconditionally call `sync_wal` at the
    /// end of every statement — read queries pay zero fsync cost.
    pub fn flush(&mut self) -> io::Result<()> {
        let batch = self.pending;
        if batch == 0 {
            return Ok(());
        }
        let is_full = matches!(self.sync_mode, WalSyncMode::Full);
        let is_off = matches!(self.sync_mode, WalSyncMode::Off);
        // Borrow the writer only for the I/O, then drop it before touching the
        // generation counters (which borrow `self`).
        let new_len = {
            let writer = self
                .writer
                .as_mut()
                .ok_or_else(|| io::Error::other("WAL writer unavailable"))?;
            writer.flush()?;
            // SQLite-style synchronous knob: only the explicit fsync is gated.
            // The BufWriter::flush above always runs so a process crash still
            // recovers cleanly via `read_all`. In `Full` we fsync inline here;
            // in `Normal` the background flusher fsyncs off this path.
            if is_full {
                writer.get_ref().sync_data()?;
            }
            writer.get_ref().metadata()?.len()
        };
        if !is_off {
            let gen = self.dirty_gen.fetch_add(1, Ordering::Release) + 1;
            if is_full {
                self.synced_gen.store(gen, Ordering::Release);
            }
        }
        self.synced_len = new_len;
        self.pending = 0;
        debug!(records = batch, "wal group commit");
        Ok(())
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
        self.writer = Some(BufWriter::new(file));
        self.pending = 0;
        self.synced_len = len;
        Ok(())
    }

    /// Read all valid records from the WAL file.
    pub fn read_all(&self) -> io::Result<Vec<WalRecord>> {
        let mut file = File::open(&self.path)?;
        let file_len = file.metadata()?.len();
        let mut file_for_header = File::open(&self.path)?;
        let mut pos = wal_records_start(&mut file_for_header)?;
        let mut records = Vec::new();

        while pos + WAL_HEADER_SIZE as u64 <= file_len {
            file.seek(SeekFrom::Start(pos))?;

            let mut header = [0u8; WAL_HEADER_SIZE];
            if file.read_exact(&mut header).is_err() {
                break;
            }

            // These slice-to-array conversions are infallible (fixed-size
            // sub-slices of a 17-byte array) but we avoid `unwrap` to
            // satisfy the project-wide zero-panic policy.
            let total_len_bytes: [u8; 4] = match header[0..4].try_into() {
                Ok(b) => b,
                Err(_) => break,
            };
            let total_len = u32::from_le_bytes(total_len_bytes) as usize;
            let stored_crc_bytes: [u8; 4] = match header[4..8].try_into() {
                Ok(b) => b,
                Err(_) => break,
            };
            let stored_crc = u32::from_le_bytes(stored_crc_bytes);
            let tx_id_bytes: [u8; 8] = match header[8..16].try_into() {
                Ok(b) => b,
                Err(_) => break,
            };
            let tx_id = u64::from_le_bytes(tx_id_bytes);
            let record_type = match WalRecordType::from_u8(header[16]) {
                Some(rt) => rt,
                None => break,
            };
            let lsn_bytes: [u8; 8] = match header[17..25].try_into() {
                Ok(b) => b,
                Err(_) => break,
            };
            let lsn = u64::from_le_bytes(lsn_bytes);

            // TASK-11: Verify the record fits within the file before
            // allocating. Catches truncated writes without any allocation.
            if pos + total_len as u64 > file_len {
                break; // Record extends beyond file — truncated write
            }

            // TASK-09: Use checked_sub to prevent integer underflow when
            // a corrupted WAL has total_len < WAL_HEADER_SIZE.
            let data_len = match total_len.checked_sub(WAL_HEADER_SIZE) {
                Some(len) => len,
                None => break, // Corrupted record — stop replay
            };

            // TASK-10: Cap allocation size before reading data. A crafted
            // WAL claiming a huge total_len would otherwise allocate
            // gigabytes before the CRC check rejects the record.
            if data_len > MAX_WAL_RECORD_SIZE {
                break; // Unreasonably large record — treat as corruption
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
                break; // Corrupted record — stop here
            }

            records.push(WalRecord {
                tx_id,
                record_type,
                lsn,
                data,
            });
            pos += total_len as u64;
        }

        Ok(records)
    }

    /// Truncate the WAL (after checkpoint).
    pub fn truncate(&mut self) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .read(true)
            .truncate(true)
            .open(&self.path)?;
        write_wal_file_header(&mut file)?;
        self.writer = Some(BufWriter::new(file));
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
        self.writer = Some(BufWriter::new(file));
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
                              // The background flusher should fsync within its (~10 ms) interval.
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(
            wal.synced_generation() >= 1,
            "background flusher did not sync (synced_generation = {})",
            wal.synced_generation()
        );
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
