use crate::disk::DiskManager;
use crate::error::StorageError;
use crate::page::{iter_page_slots, Page, PageType, UpdateFit, MAX_ROW_DATA_SIZE, PAGE_SIZE};
use crate::row::{row_is_v2, validate_row_format};
use crate::types::RowId;
use rustc_hash::FxHashMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

pub const HEAP_MAGIC: &[u8; 5] = b"PHEAP";
/// The heap superblock version a fresh heap is created with. A database that
/// never spills a value stays at v2 forever and remains readable by pre-v0.11
/// binaries (door D4 / section 3.8).
pub const HEAP_FORMAT_VERSION: u16 = 2;
/// Superblock version once the heap has stored at least one overflow chain.
/// Bumped lazily on the FIRST chain write; old binaries refuse v3 outright
/// (correct: they would misread Overflow pages mid-scan).
pub const HEAP_FORMAT_VERSION_WITH_OVERFLOW: u16 = 3;
const HEAP_SUPERBLOCK_OFFSET: usize = crate::page::PAGE_HEADER_SIZE;
const HEAP_SUPERBLOCK_FIRST_DATA_PAGE: u32 = 1;
const HEAP_SUPERBLOCK_VERSION_OFFSET: usize = HEAP_SUPERBLOCK_OFFSET + HEAP_MAGIC.len();

fn heap_superblock_page_versioned(version: u16) -> Page {
    let page = Page::new(0, PageType::Meta);
    let mut bytes = *page.as_bytes();
    let mut pos = HEAP_SUPERBLOCK_OFFSET;
    bytes[pos..pos + HEAP_MAGIC.len()].copy_from_slice(HEAP_MAGIC);
    pos += HEAP_MAGIC.len();
    bytes[pos..pos + 2].copy_from_slice(&version.to_le_bytes());
    pos += 2;
    bytes[pos..pos + 2].copy_from_slice(&0u16.to_le_bytes()); // flags
    pos += 2;
    bytes[pos..pos + 2].copy_from_slice(&(PAGE_SIZE as u16).to_le_bytes());
    pos += 2;
    bytes[pos..pos + 4].copy_from_slice(&HEAP_SUPERBLOCK_FIRST_DATA_PAGE.to_le_bytes());
    let mut page = Page::from_bytes(&bytes).expect("fresh heap superblock is a valid page");
    page.stamp_checksum();
    page
}

fn heap_superblock_page() -> Page {
    heap_superblock_page_versioned(HEAP_FORMAT_VERSION)
}

/// Returns `(first_data_page, heap_version)`. New code accepts versions 2 and
/// 3; a `first_data_page` of 0 signals a legacy pre-superblock (v1) heap.
fn heap_first_data_page(buf: &[u8; PAGE_SIZE]) -> io::Result<(u32, u16)> {
    if buf[4] != PageType::Meta as u8 {
        // Legacy heap v1 (pre-v0.5.0 writers): no page-0 superblock, data
        // pages start at page 0. Superseded by the v2 superblock in v0.5.0;
        // removable since v0.9.0 per the docs/FORMAT.md support policy, kept
        // deliberately.
        return Ok((0, 1));
    }
    let mut pos = HEAP_SUPERBLOCK_OFFSET;
    if &buf[pos..pos + HEAP_MAGIC.len()] != HEAP_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad heap superblock magic",
        ));
    }
    pos += HEAP_MAGIC.len();
    let version = u16::from_le_bytes(buf[pos..pos + 2].try_into().expect("2-byte heap version"));
    pos += 2;
    if version != HEAP_FORMAT_VERSION && version != HEAP_FORMAT_VERSION_WITH_OVERFLOW {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported heap format version: {version}"),
        ));
    }
    pos += 2; // flags
    let page_size = u16::from_le_bytes(buf[pos..pos + 2].try_into().expect("2-byte page size"));
    pos += 2;
    if page_size as usize != PAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported heap page size: {page_size}"),
        ));
    }
    let first_data_page = u32::from_le_bytes(
        buf[pos..pos + 4]
            .try_into()
            .expect("4-byte first data page"),
    );
    Ok((first_data_page, version))
}

#[cfg(test)]
thread_local! {
    /// Page id whose next flush write must fail. There is no portable way to
    /// make a real `pwrite` return ENOSPC/EIO on demand, and a mid-flush write
    /// error is precisely the case [`HeapFile::flush_all_dirty`] has to survive
    /// without stranding the pages it has not written yet.
    static HEAP_WRITE_FAILPOINT: std::cell::Cell<Option<u32>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn take_heap_write_failpoint(page_id: u32) -> bool {
    HEAP_WRITE_FAILPOINT.with(|failpoint| {
        if failpoint.get() == Some(page_id) {
            failpoint.set(None);
            true
        } else {
            false
        }
    })
}

/// `DiskManager::write_page` as the flush path calls it, with the test-only
/// failure injection point above. A free function so it can be called while a
/// page is borrowed out of `dirty_buffer`.
fn write_page_flushing(disk: &mut DiskManager, page_id: u32, data: &[u8]) -> io::Result<()> {
    #[cfg(test)]
    if take_heap_write_failpoint(page_id) {
        return Err(io::Error::other(format!(
            "injected write failure on page {page_id}"
        )));
    }
    disk.write_page(page_id, data)
}

/// Default ceiling on unflushed heap pages held in memory: 256 MiB, charged
/// across every table in a catalog. Mirrors the per-query byte budget in
/// `powdb-query`'s `mem_budget`.
///
/// A default, not a hard ceiling: operators raise or lower it with
/// `POWDB_DIRTY_PAGE_BUDGET` (the server reads that env var and applies it via
/// [`crate::catalog::Catalog::set_dirty_page_budget_bytes`]), the same way
/// `POWDB_QUERY_MEMORY_LIMIT` overrides the per-query budget. A host with the
/// RAM for a larger bulk-load transaction can raise it; a memory-capped host
/// should lower it.
pub const DEFAULT_DIRTY_PAGE_BUDGET: usize = 256 * 1024 * 1024;

/// Shared accounting for dirty (mutated but not yet written) heap pages.
///
/// `dirty_buffer` is otherwise uncapped, and its only drains
/// ([`HeapFile::flush_all_dirty`], [`HeapFile::discard_dirty`]) may not run
/// mid-transaction: a large enough transaction pins unbounded memory, and
/// under `panic = "abort"` an allocation failure kills the whole process
/// rather than the statement. One budget is shared by every heap a catalog
/// owns so the cap covers the engine, not a single table.
///
/// Outside an explicit transaction an over-budget heap simply writes its
/// buffer out (which is what scans and checkpoints already do). Inside one it
/// cannot: ROLLBACK works by *discarding* the unflushed pages, so spilling
/// them would make the transaction unrollbackable. There the mutation is
/// refused with [`StorageError::TransactionTooLarge`].
pub struct DirtyPageBudget {
    pages: AtomicUsize,
    limit_bytes: AtomicUsize,
    rollback_pinned: AtomicBool,
}

impl Default for DirtyPageBudget {
    fn default() -> Self {
        Self::new(DEFAULT_DIRTY_PAGE_BUDGET)
    }
}

impl DirtyPageBudget {
    pub fn new(limit_bytes: usize) -> Self {
        DirtyPageBudget {
            pages: AtomicUsize::new(0),
            limit_bytes: AtomicUsize::new(limit_bytes),
            rollback_pinned: AtomicBool::new(false),
        }
    }

    pub fn limit_bytes(&self) -> usize {
        self.limit_bytes.load(Ordering::Relaxed)
    }

    pub fn set_limit_bytes(&self, limit_bytes: usize) {
        self.limit_bytes.store(limit_bytes, Ordering::Relaxed);
    }

    /// Pages currently buffered across every heap sharing this budget.
    pub fn charged_pages(&self) -> usize {
        self.pages.load(Ordering::Relaxed)
    }

    /// Set while an explicit transaction is open: the buffered pages are the
    /// only copy of the pre-COMMIT state, so they must not be spilled to disk.
    pub fn set_rollback_pinned(&self, pinned: bool) {
        self.rollback_pinned.store(pinned, Ordering::Relaxed);
    }

    pub fn is_rollback_pinned(&self) -> bool {
        self.rollback_pinned.load(Ordering::Relaxed)
    }

    /// Reserve room for one more buffered page. `false` means the budget is
    /// full; nothing is charged in that case.
    fn try_charge_page(&self) -> bool {
        let limit_pages = self.limit_bytes.load(Ordering::Relaxed) / PAGE_SIZE;
        if self.pages.fetch_add(1, Ordering::Relaxed) >= limit_pages {
            self.pages.fetch_sub(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Charge `pages` without a limit check. Used only when an already
    /// populated heap joins a different budget.
    fn force_charge(&self, pages: usize) {
        self.pages.fetch_add(pages, Ordering::Relaxed);
    }

    /// Saturating on purpose: a wrapped counter would refuse every future
    /// page and turn this guard into the outage it exists to prevent.
    fn release(&self, pages: usize) {
        if pages == 0 {
            return;
        }
        let _ = self
            .pages
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |charged| {
                Some(charged.saturating_sub(pages))
            });
    }
}

/// A single dirty page pinned in memory for write-back coalescing.
///
/// Mission C Phase 1: the previous write path did `read_page + write_page`
/// for every insert/update/delete — two syscalls per row on `insert_batch_1k`
/// and two syscalls per row on `update_by_filter`. Keeping the last-touched
/// page live in memory collapses that to ~one read + one write per PAGE
/// instead of per ROW. For a 1000-row batch into ~40 pages, that's 2000
/// syscalls → 80 syscalls — 25x fewer trips to the OS.
struct HotPage {
    page_id: u32,
    page: Page,
    /// Set to true whenever `page` is mutated. The flush is skipped when
    /// `dirty == false` (e.g., we only read a page and never wrote).
    dirty: bool,
}

/// Manages a collection of data pages for storing rows.
/// Tracks which pages have free space for fast insertion.
pub struct HeapFile {
    disk: DiskManager,
    first_data_page: u32,
    /// Pages with known free space. Iteration order matters for the
    /// `insert` fallback path, so this is a `Vec`. Membership is tracked
    /// in `in_free_list` for O(1) `contains` checks.
    pages_with_space: Vec<u32>,
    /// Mission C Phase 8: sidecar bitmap parallel to `pages_with_space`
    /// that answers "is page N already on the free-space list?" in O(1).
    /// Indexed by page_id. Previously `delete` did a linear `contains`
    /// over `pages_with_space` on every call — for a scattered delete
    /// that walks every page, that's quadratic in the number of pages
    /// with free space and shows up as a ~30% overhead on
    /// `delete_by_filter`.
    in_free_list: Vec<bool>,
    /// Optional mmap for zero-syscall reads. Activated by `enable_mmap()`.
    ///
    /// # Safety invariant
    ///
    /// Valid only while the file size is stable. `disable_mmap()` must be
    /// called before any mutation that extends the file (e.g., `insert`
    /// allocating a new page). Without invalidation the pointer covers
    /// stale/incomplete data beyond the original mapped length.
    mmap_ptr: Option<(*const u8, usize)>,
    /// Mission C Phase 1: write-back cache for the most recently touched
    /// page. All insert/update/delete operations land here first and only
    /// hit disk when a different page is accessed, a scan runs, or the
    /// heap is dropped. Invariant: at most one dirty page lives in memory.
    hot_page: Option<HotPage>,
    /// Mission C Phase 9: deferred-write buffer for pages that were
    /// previously hot, got mutated, and have since been evicted from the
    /// `hot_page` slot. The previous design synchronously wrote the old
    /// hot page to disk on every page transition — for a scattered
    /// `delete_by_filter` that walks ~2000 pages, that's ~2000
    /// `write_page` syscalls on the critical path. Now the evicted dirty
    /// page gets parked here in memory, reclaimed on the next access to
    /// the same page, and only persisted via an explicit
    /// [`flush_all_dirty`] call (or `Drop`). Scan operations call
    /// `flush_all_dirty` first so their view is consistent with disk.
    dirty_buffer: FxHashMap<u32, Page>,
    /// Cap on how much `dirty_buffer` may hold, shared with every other heap
    /// in the same catalog. See [`DirtyPageBudget`].
    dirty_budget: Arc<DirtyPageBudget>,
    free_overflow_pages: Vec<u32>,
    heap_version: u16,
}

impl HeapFile {
    pub fn create(path: &Path) -> io::Result<Self> {
        let mut disk = DiskManager::create(path)?;
        let page_id = disk.allocate_page()?;
        debug_assert_eq!(page_id, 0);
        let superblock = heap_superblock_page();
        disk.write_page(0, superblock.as_bytes())?;
        disk.flush()?;
        Ok(HeapFile {
            disk,
            first_data_page: HEAP_SUPERBLOCK_FIRST_DATA_PAGE,
            pages_with_space: Vec::new(),
            in_free_list: Vec::new(),
            mmap_ptr: None,
            hot_page: None,
            dirty_buffer: FxHashMap::default(),
            dirty_budget: Arc::new(DirtyPageBudget::default()),
            free_overflow_pages: Vec::new(),
            heap_version: HEAP_FORMAT_VERSION,
        })
    }

    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_disk(DiskManager::open(path)?)
    }

    /// Reopen a heap file **read-only** for snapshot serving. The underlying
    /// [`DiskManager`] refuses every write, and the open-time free-space scan
    /// never reinitializes zero pages in place (that write path is skipped on a
    /// read-only handle), so opening never mutates the directory.
    pub fn open_read_only(path: &Path) -> io::Result<Self> {
        Self::open_disk(DiskManager::open_read_only(path)?)
    }

    fn open_disk(mut disk: DiskManager) -> io::Result<Self> {
        let read_only = disk.is_read_only();
        let num_pages = disk.num_pages();
        let (first_data_page, heap_version) = if num_pages == 0 {
            (0, HEAP_FORMAT_VERSION)
        } else {
            let page0 = disk.read_page(0)?;
            heap_first_data_page(&page0)?
        };
        let mut pages_with_space = Vec::new();
        let mut in_free_list = vec![false; num_pages as usize];
        for i in first_data_page..num_pages {
            if let Ok(buf) = disk.read_page(i) {
                // Overflow-chain pages are not data pages: they hold value
                // payload behind a chunk header, never a slot directory, so
                // they must never be offered to the row insert path nor
                // validated as rows. Their reuse is driven by the in-memory
                // free list (within a session) and by `sweep` (across
                // restarts), not by this open-time free-space scan.
                if buf[4] == PageType::Overflow as u8 {
                    continue;
                }
                // Mission 2: a page whose `page_type` byte is 0 was
                // allocated by [`DiskManager::allocate_page`] (which
                // writes an all-zero 4KB block) but never populated with
                // a real [`Page`] header. This happens when a crash
                // occurs between `allocate_page` extending the file and
                // the first `write_page` that lands actual row data —
                // which is exactly the state a WAL-replay-driven recovery
                // sees. Reinitialize these pages in place as fresh empty
                // Data pages so the insert path can treat them as normal
                // free-space candidates. Without this, `Page::insert`
                // takes `free_start = 0` as the write offset and stomps
                // on the page header with row bytes.
                if buf[4] == 0 {
                    // A read-only handle must never write. A quiescent
                    // (restored/checkpointed) directory has no half-allocated
                    // zero pages: those only appear after a crash mid-allocate,
                    // which read-only serving refuses to open via the WAL check ,
                    // so skipping the reinit here loses no reachable rows.
                    if read_only {
                        continue;
                    }
                    let mut fresh = Page::new(i, PageType::Data);
                    // WS3: this is a direct write that bypasses the flush
                    // path, so stamp the CRC here — otherwise the page's
                    // checksum flag would be set with a zero CRC and the
                    // next verified read would (correctly) reject it.
                    fresh.stamp_checksum();
                    let _ = disk.write_page(i, fresh.as_bytes());
                    pages_with_space.push(i);
                    in_free_list[i as usize] = true;
                    continue;
                }
                // Verify the CRC here (validate-if-present) instead
                // of trusting the bytes. The open path runs before WAL replay,
                // so a corrupt page that slips through and later panics in a
                // release build (`panic = "abort"`) would abort again on every
                // supervisor restart: a permanent crash loop. A typed
                // `PageCorrupt` error, by contrast, is reportable and
                // recoverable from a backup.
                let page = Page::from_bytes_verified(&buf).map_err(io::Error::from)?;
                for (_slot, row) in iter_page_slots(&buf) {
                    validate_row_format(row)?;
                }
                if page.free_space() > 64 {
                    pages_with_space.push(i);
                    in_free_list[i as usize] = true;
                }
            }
        }
        Ok(HeapFile {
            disk,
            first_data_page,
            pages_with_space,
            in_free_list,
            mmap_ptr: None,
            hot_page: None,
            dirty_buffer: FxHashMap::default(),
            dirty_budget: Arc::new(DirtyPageBudget::default()),
            free_overflow_pages: Vec::new(),
            heap_version,
        })
    }

    /// Join `budget`, the dirty-page cap shared by every heap in a catalog.
    /// Called right after the heap is created or opened.
    pub fn set_dirty_budget(&mut self, budget: Arc<DirtyPageBudget>) {
        let held = self.dirty_buffer.len();
        self.dirty_budget.release(held);
        budget.force_charge(held);
        self.dirty_budget = budget;
    }

    /// Pages this heap is currently holding unflushed (test/diagnostic hook).
    pub fn dirty_page_count(&self) -> usize {
        self.dirty_buffer.len()
    }

    pub fn format_version(&self) -> u16 {
        if self.first_data_page == 0 {
            1
        } else {
            self.heap_version
        }
    }

    pub fn first_data_page(&self) -> u32 {
        self.first_data_page
    }

    /// O(1) check: is `page_id` currently on the free-space list?
    #[inline]
    fn is_in_free_list(&self, page_id: u32) -> bool {
        self.in_free_list
            .get(page_id as usize)
            .copied()
            .unwrap_or(false)
    }

    /// Mark `page_id` as no-longer-free in the sidecar bitmap. Caller is
    /// responsible for removing it from `pages_with_space`.
    #[inline]
    fn mark_not_free(&mut self, page_id: u32) {
        if let Some(slot) = self.in_free_list.get_mut(page_id as usize) {
            *slot = false;
        }
    }

    /// Mark `page_id` as free in the sidecar bitmap, growing the vec if
    /// the id is beyond current capacity. Caller is responsible for
    /// pushing it onto `pages_with_space`.
    #[inline]
    fn mark_free(&mut self, page_id: u32) {
        let idx = page_id as usize;
        if idx >= self.in_free_list.len() {
            self.in_free_list.resize(idx + 1, false);
        }
        self.in_free_list[idx] = true;
    }

    /// Park the pinned hot page into the deferred-write buffer (or drop
    /// it if clean). Does not write to disk. Use [`flush_all_dirty`]
    /// to persist every buffered page.
    ///
    /// Mission C Phase 9: this was previously a write-through. Now the
    /// only callers that actually touch disk are `flush_all_dirty`,
    /// `enable_mmap`, `Drop`, and the over-budget relief path below.
    ///
    /// Parking is where `dirty_buffer` grows, so it is also where the shared
    /// [`DirtyPageBudget`] is charged. A full budget outside an explicit
    /// transaction is relieved by draining the buffer to disk; inside one the
    /// pages cannot be spilled without breaking ROLLBACK, so the mutation is
    /// refused with a typed error instead of growing until the process dies.
    fn park_hot_page(&mut self) -> io::Result<()> {
        if !self.hot_page.as_ref().is_some_and(|hot| hot.dirty) {
            self.hot_page = None;
            return Ok(());
        }
        if !self.dirty_budget.try_charge_page() {
            if self.dirty_budget.is_rollback_pinned() {
                return Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    StorageError::TransactionTooLarge {
                        pages: self.dirty_budget.charged_pages(),
                        limit_bytes: self.dirty_budget.limit_bytes(),
                    },
                ));
            }
            // Draining the whole buffer (rather than writing just this page)
            // keeps the relief an amortized batch eviction. The hot page is
            // written by the flush too, so it is clean afterwards and needs
            // no buffer slot.
            self.flush_all_dirty()?;
            self.hot_page = None;
            return Ok(());
        }
        if let Some(hot) = self.hot_page.take() {
            // `ensure_hot` uncharges whatever it pulls out of the buffer, so a
            // replacement here would mean the page was charged twice.
            if self.dirty_buffer.insert(hot.page_id, hot.page).is_some() {
                self.dirty_budget.release(1);
            }
        }
        Ok(())
    }

    /// Public API kept for compatibility: park the hot page AND flush
    /// every buffered dirty page to disk. Callers that need an on-disk
    /// consistent view (scans, mmap rebuilds) use this.
    pub fn flush_hot_page(&mut self) -> io::Result<()> {
        self.flush_all_dirty()
    }

    /// Write every buffered dirty page to disk, including the current
    /// hot page if dirty. Clears the buffer. Called by scans,
    /// `enable_mmap`, explicit flush requests, and `Drop`.
    ///
    /// A page leaves the buffer only once its own write has succeeded. An
    /// earlier version drained the whole buffer up front and then wrote the
    /// drained copies, so one ENOSPC/EIO partway through left every remaining
    /// page in neither memory nor on disk, and a later successful checkpoint
    /// would truncate the WAL and make that loss permanent. Keeping the
    /// unwritten pages buffered means the mutations are still there for the
    /// caller's retry, and still there for `Drop` and the next checkpoint.
    pub fn flush_all_dirty(&mut self) -> io::Result<()> {
        if let Some(hot) = self.hot_page.as_mut() {
            if hot.dirty {
                // WS3: stamp the CRC32 on the flush path (once per dirty
                // page), not per row, so the write-path regression stays
                // negligible.
                hot.page.stamp_checksum();
                write_page_flushing(&mut self.disk, hot.page_id, hot.page.as_bytes())?;
                hot.dirty = false;
            }
        }
        if !self.dirty_buffer.is_empty() {
            // Split the borrow so the write can take `&mut disk` while the
            // page it writes is still borrowed out of the buffer. Collecting
            // the ids (4 bytes each) is strictly cheaper than the drain this
            // replaced, which moved every 4KB page out of the map.
            let Self {
                disk,
                dirty_buffer,
                dirty_budget,
                ..
            } = self;
            let page_ids: Vec<u32> = dirty_buffer.keys().copied().collect();
            for page_id in page_ids {
                let Some(page) = dirty_buffer.get_mut(&page_id) else {
                    continue;
                };
                page.stamp_checksum();
                write_page_flushing(disk, page_id, page.as_bytes())?;
                dirty_buffer.remove(&page_id);
                dirty_budget.release(1);
            }
        }
        Ok(())
    }

    /// Make `page_id` the hot page. If a different page is currently hot,
    /// park it into the deferred-write buffer first. If `page_id` is
    /// already in the buffer, reclaim it instead of re-reading from disk.
    ///
    /// Mission C Phase 12: if `mmap_ptr` is set and covers the page, copy
    /// the bytes directly from the mapped region instead of issuing a
    /// `pread` syscall. On `delete_by_filter` this removes ~1.7ms of
    /// scattered syscall overhead (3000+ pages × ~500ns each).
    fn ensure_hot(&mut self, page_id: u32) -> io::Result<()> {
        if let Some(hot) = &self.hot_page {
            if hot.page_id == page_id {
                return Ok(());
            }
        }
        self.park_hot_page()?;

        // Mission C Phase 9: reclaim the page from the dirty buffer if
        // we've touched it before. This is the hot path for scattered
        // delete/update workloads — we re-visit the same pages via the
        // index lookups and don't want to re-read them from disk.
        if let Some(page) = self.dirty_buffer.remove(&page_id) {
            self.dirty_budget.release(1);
            self.hot_page = Some(HotPage {
                page_id,
                page,
                dirty: true,
            });
            return Ok(());
        }

        // Mission C Phase 12: zero-syscall read via mmap. The mmap was
        // created from a consistent on-disk snapshot after populate; as
        // long as the page hasn't been mutated since (i.e., not in hot/
        // dirty_buffer — already checked above), the bytes we see are
        // what `disk.read_page` would return without the syscall.
        if let Some((ptr, len)) = self.mmap_ptr {
            let offset = page_id as usize * PAGE_SIZE;
            if offset + PAGE_SIZE <= len {
                // SAFETY: `ptr` is a valid read-only mmap pointer covering
                // `len` bytes (set by `enable_mmap`). We verified
                // `offset + PAGE_SIZE <= len` above, so `ptr.add(offset)`
                // is within the mapped region and the resulting slice does
                // not exceed it.
                let page_bytes = unsafe { std::slice::from_raw_parts(ptr.add(offset), PAGE_SIZE) };
                if let Some(page) = Page::from_bytes(page_bytes) {
                    self.hot_page = Some(HotPage {
                        page_id,
                        page,
                        dirty: false,
                    });
                    return Ok(());
                }
            }
        }

        let buf = self.disk.read_page(page_id)?;
        // WS3: verify the page CRC32 on read (validate-if-present). A
        // checksum mismatch surfaces as `StorageError::PageCorrupt`, which
        // converts to an `io::Error` for this `io::Result` boundary.
        let page = Page::from_bytes_verified(&buf).map_err(io::Error::from)?;
        self.hot_page = Some(HotPage {
            page_id,
            page,
            dirty: false,
        });
        Ok(())
    }

    /// Install a freshly-allocated page (no disk read) as the hot page.
    /// The previous hot page, if any, is parked into the dirty buffer.
    fn install_fresh_hot(&mut self, page_id: u32, page: Page) -> io::Result<()> {
        self.park_hot_page()?;
        self.hot_page = Some(HotPage {
            page_id,
            page,
            dirty: true,
        });
        Ok(())
    }

    /// Activate mmap for zero-syscall reads. Call after all inserts are done.
    /// The mmap covers the current file size; new inserts will invalidate it.
    ///
    /// Mission C Phase 1: the hot page must be flushed before mmapping so
    /// the mapping sees the last dirty page's contents.
    pub fn enable_mmap(&mut self) {
        if self.mmap_ptr.is_some() {
            return;
        }
        // Flush every dirty page (hot + buffered) so the mmap sees the
        // same bytes the reader would expect. We log a warning on failure
        // rather than propagating the error, because `enable_mmap` does
        // not return `Result` and we don't want to fail the bench harness.
        // The fallback is per-call mmap which is still correct (if slower).
        if let Err(e) = self.flush_all_dirty() {
            tracing::warn!(error = %e, "flush failed before mmap enable");
        }

        let num_pages = self.disk.num_pages();
        if num_pages == 0 {
            return;
        }
        let file_len = num_pages as usize * PAGE_SIZE;
        use std::os::unix::io::AsRawFd;
        let fd = self.disk.file_ref().as_raw_fd();
        // SAFETY: `fd` is a valid open file descriptor obtained from
        // `self.disk.file_ref()`. `file_len` is computed from
        // `num_pages * PAGE_SIZE` which matches the actual file size
        // (we flushed all dirty pages above). The mapping is read-only
        // (`PROT_READ`) and private (`MAP_PRIVATE`), so no writes can
        // occur through this pointer. The returned pointer is only
        // stored if mmap succeeded (not `MAP_FAILED`).
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                file_len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                fd,
                0,
            )
        };
        if ptr != libc::MAP_FAILED {
            self.mmap_ptr = Some((ptr as *const u8, file_len));
        }
    }

    /// Tear down the persistent mmap, if active. Safe to call when no
    /// mapping exists (it's a no-op). Called on the file-growth path of
    /// `insert` (new-page allocation) so the mapping is invalidated before
    /// the file grows beyond the mapped length. WS1: it is deliberately
    /// *not* called on the hot-page insert fast path, which never grows
    /// the file.
    pub fn disable_mmap(&mut self) {
        if let Some((ptr, len)) = self.mmap_ptr.take() {
            // SAFETY: `ptr` and `len` were returned by a successful
            // `libc::mmap` call in `enable_mmap`. We only call `munmap`
            // once (the `take()` ensures the slot is cleared), so no
            // double-free is possible.
            unsafe {
                libc::munmap(ptr as *mut libc::c_void, len);
            }
        }
    }

    /// Reject rows that can never fit in a single page. This is the clean
    /// error boundary for user-supplied oversized values: every mutation
    /// entry point (`insert`, `update`) checks BEFORE touching any page so
    /// no partial state (e.g. the delete half of a delete+insert update)
    /// can land first. With `panic = "abort"` in release builds, the old
    /// `expect("row too large for empty page")` was a remote DoS.
    #[inline]
    fn check_row_size(row_data: &[u8]) -> io::Result<()> {
        if row_data.len() > MAX_ROW_DATA_SIZE {
            return Err(StorageError::RowTooLarge {
                size: row_data.len(),
                max: MAX_ROW_DATA_SIZE,
            }
            .into());
        }
        Ok(())
    }

    // ===================== Overflow chains (P-2) =====================

    /// Lazily bump the heap superblock from v2 to v3 on the first overflow
    /// chain write. A never-spilling database stays v2 and old-binary
    /// readable; once a chain exists, old binaries must refuse the file
    /// (they would misread Overflow pages), so the version advertises v3.
    /// v2 (since v0.5.0) and v3 (since v0.11.0) are both current writer
    /// versions per docs/FORMAT.md; neither is removal-eligible.
    fn ensure_heap_v3(&mut self) -> io::Result<()> {
        // Legacy pre-superblock heaps (first_data_page == 0) have no version
        // word to bump; already-v3 heaps are done.
        if self.first_data_page == 0 || self.heap_version >= HEAP_FORMAT_VERSION_WITH_OVERFLOW {
            return Ok(());
        }
        let mut buf = self.disk.read_page(0)?;
        buf[HEAP_SUPERBLOCK_VERSION_OFFSET..HEAP_SUPERBLOCK_VERSION_OFFSET + 2]
            .copy_from_slice(&HEAP_FORMAT_VERSION_WITH_OVERFLOW.to_le_bytes());
        let mut page = Page::from_bytes(&buf)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "corrupt heap superblock"))?;
        page.stamp_checksum();
        self.disk.write_page(0, page.as_bytes())?;
        self.heap_version = HEAP_FORMAT_VERSION_WITH_OVERFLOW;
        Ok(())
    }

    /// Allocate an overflow-chain page: reuse a freed one if the in-memory
    /// free list has any, otherwise extend the file. Extending invalidates
    /// the persistent mmap (the file grows past the mapped snapshot), so it
    /// is torn down first — exactly as on the data-page allocation path.
    pub fn allocate_overflow_page(&mut self) -> io::Result<u32> {
        if let Some(pid) = self.free_overflow_pages.pop() {
            return Ok(pid);
        }
        self.disable_mmap();
        self.disk.allocate_page()
    }

    /// Write one chunk of an overflow chain to `page_id` directly to disk
    /// (overflow pages bypass the data-page hot/dirty machinery). The page
    /// is stamped with `lsn` so WAL replay of the corresponding
    /// `OverflowWrite` record is idempotent under the per-page LSN skip, and
    /// with a CRC32 so the verified read path accepts it. Bumps the heap
    /// superblock to v3 on the first such write.
    pub fn write_overflow_page(
        &mut self,
        page_id: u32,
        next_page: u32,
        chunk: &[u8],
        lsn: u64,
    ) -> io::Result<()> {
        self.ensure_heap_v3()?;
        // A replay may target a page id past the current file end (the chain
        // page was allocated but never flushed before the crash). Grow the
        // file with zero pages up to it, mirroring `insert_at`.
        if page_id >= self.disk.num_pages() {
            self.disable_mmap();
            while self.disk.num_pages() <= page_id {
                self.disk.allocate_page()?;
            }
        }
        let mut page = Page::new_overflow(page_id);
        page.set_overflow_chunk(next_page, chunk);
        if lsn > 0 {
            page.set_lsn(lsn);
        }
        page.stamp_checksum();
        self.disk.write_page(page_id, page.as_bytes())
    }

    /// The current on-disk LSN of an overflow page (0 if past the file end or
    /// unstamped). Used by WAL replay to skip already-applied `OverflowWrite`
    /// records idempotently.
    pub fn overflow_page_lsn(&self, page_id: u32) -> u64 {
        if page_id < self.disk.num_pages() {
            if let Ok(buf) = self.disk.read_page(page_id) {
                return crate::page::page_lsn(&buf);
            }
        }
        0
    }

    /// Walk an overflow chain from `first_page`, collecting every page id in
    /// the chain (head-first order). Used to build `OverflowFree` payloads
    /// and to return a chain's pages to the free list. A cycle guard caps the
    /// walk at the current file length so a corrupt `next_page` can never
    /// loop forever.
    pub fn overflow_chain_pages(&self, first_page: u32) -> io::Result<Vec<u32>> {
        let mut pages = Vec::new();
        let mut pid = first_page;
        let max_steps = self.disk.num_pages() as usize + 1;
        while pid != crate::page::OVERFLOW_CHAIN_END && pages.len() < max_steps {
            if pid >= self.disk.num_pages() {
                break;
            }
            pages.push(pid);
            let buf = self.disk.read_page(pid)?;
            if buf[4] != PageType::Overflow as u8 {
                break;
            }
            pid = crate::page::overflow_next_from_bytes(&buf);
        }
        Ok(pages)
    }

    /// Reassemble the full out-of-line value a stub points at, verifying its
    /// length and whole-value CRC32 (typed `OverflowCorrupt` on mismatch —
    /// torn or cross-linked chain).
    pub fn read_overflow_value(
        &self,
        stub: &crate::row::OverflowStub,
    ) -> crate::error::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(stub.total_len as usize);
        let mut pid = stub.first_page;
        let max_steps = self.disk.num_pages() as usize + 1;
        let mut steps = 0usize;
        while pid != crate::page::OVERFLOW_CHAIN_END {
            if pid >= self.disk.num_pages() || steps > max_steps {
                return Err(StorageError::OverflowCorrupt(format!(
                    "overflow chain from page {} runs off the file at page {pid}",
                    stub.first_page
                )));
            }
            let buf = self.disk.read_page(pid).map_err(StorageError::Io)?;
            // Verify the page CRC so on-disk bit-rot in a chain page is caught.
            Page::from_bytes_verified(&buf)?;
            if buf[4] != PageType::Overflow as u8 {
                return Err(StorageError::OverflowCorrupt(format!(
                    "overflow chain page {pid} is not an Overflow page"
                )));
            }
            out.extend_from_slice(crate::page::overflow_chunk_from_bytes(&buf));
            pid = crate::page::overflow_next_from_bytes(&buf);
            steps += 1;
            if out.len() as u64 > stub.total_len {
                break;
            }
        }
        if out.len() as u64 != stub.total_len {
            return Err(StorageError::OverflowCorrupt(format!(
                "overflow value length {} != stub total_len {}",
                out.len(),
                stub.total_len
            )));
        }
        let crc = crc32fast::hash(&out);
        if crc != stub.value_crc32 {
            return Err(StorageError::OverflowCorrupt(format!(
                "overflow value CRC32 mismatch: computed {crc:#010x}, stub {:#010x}",
                stub.value_crc32
            )));
        }
        Ok(out)
    }

    /// Return a set of overflow pages to the in-memory free list for reuse by
    /// the next chain allocation within this session.
    pub fn release_overflow_pages(&mut self, pages: &[u32]) {
        self.free_overflow_pages.extend_from_slice(pages);
    }

    /// Snapshot of the current overflow free list (test/introspection).
    pub fn overflow_free_list_len(&self) -> usize {
        self.free_overflow_pages.len()
    }

    /// Total page count (including the superblock and any overflow pages).
    pub fn num_pages(&self) -> u32 {
        self.disk.num_pages()
    }

    /// Sweep phase of overflow reclamation (design 3.6): reclaim every
    /// Overflow-typed page in `[first_data_page, watermark)` that is NOT in the
    /// `referenced` set and not already free, returning them to the in-memory
    /// free list. The watermark is the current file length, so this runs under
    /// the table write lock where no concurrent allocation can race it. Returns
    /// the reclaimed page ids (for the caller's `OverflowFree` WAL record).
    pub fn sweep_unreferenced_overflow(
        &mut self,
        referenced: &std::collections::HashSet<u32>,
    ) -> io::Result<Vec<u32>> {
        let already_free: std::collections::HashSet<u32> =
            self.free_overflow_pages.iter().copied().collect();
        let watermark = self.disk.num_pages();
        let mut reclaimed = Vec::new();
        for pid in self.first_data_page..watermark {
            if referenced.contains(&pid) || already_free.contains(&pid) {
                continue;
            }
            let buf = self.disk.read_page(pid)?;
            if buf[4] == PageType::Overflow as u8 {
                reclaimed.push(pid);
            }
        }
        self.free_overflow_pages.extend_from_slice(&reclaimed);
        Ok(reclaimed)
    }

    /// Split `value` into `OVERFLOW_PAYLOAD_CAP`-sized chunks. The plan is a
    /// list of chunk byte-ranges; the caller allocates a page per chunk and
    /// links them head-first.
    pub fn overflow_chunk_count(value_len: usize) -> usize {
        value_len.div_ceil(crate::page::OVERFLOW_PAYLOAD_CAP).max(1)
    }

    /// Insert encoded row data. Returns RowId.
    ///
    /// Mission C Phase 1: uses the hot-page write-back cache. The common
    /// case — repeated inserts into the currently-hot page — does zero
    /// disk syscalls; the page stays pinned until a different page is
    /// touched or an explicit flush runs.
    pub fn insert(&mut self, row_data: &[u8]) -> io::Result<RowId> {
        Self::check_row_size(row_data)?;
        // WS1 invariant: the persistent mmap covers a snapshot of the file
        // taken at `enable_mmap()` time (length = num_pages * PAGE_SIZE).
        // It only becomes unsafe — covering a stale/short region relative
        // to the live file — when the file actually *grows*, i.e. when we
        // allocate a brand-new page below. Writes into already-mapped pages
        // are safe to leave the mapping in place because:
        //   1. The mapping is `MAP_PRIVATE` + `PROT_READ` (never written
        //      through), so it cannot tear from our writes here.
        //   2. `get()` / `scan()` consult the dirty hot page and the
        //      dirty buffer *before* the mmap, so unflushed writes to a
        //      mapped page are always read from memory, not the snapshot.
        // Therefore we defer `disable_mmap()` to the new-page path only,
        // instead of paying a `munmap` syscall on every hot-page insert.

        // Hot-path: the pinned page already has room. This is the bench's
        // insert_batch_1k / insert_single loop. No file growth happens here,
        // so the mmap stays mapped.
        if let Some(hot) = self.hot_page.as_mut() {
            if let Some(slot) = hot.page.insert(row_data) {
                hot.dirty = true;
                let page_id = hot.page_id;
                let became_full = hot.page.free_space() < 64;
                if became_full {
                    if let Some(pos) = self.pages_with_space.iter().position(|p| *p == page_id) {
                        self.pages_with_space.swap_remove(pos);
                    }
                    self.mark_not_free(page_id);
                }
                return Ok(RowId {
                    page_id,
                    slot_index: slot,
                });
            }
            // Hot page is full — fall through to pages_with_space. The
            // flush will happen inside `ensure_hot` when we load a
            // different page.
        }

        // Try existing pages with space.
        for idx in 0..self.pages_with_space.len() {
            let page_id = self.pages_with_space[idx];
            self.ensure_hot(page_id)?;
            let hot = self.hot_page.as_mut().expect("ensure_hot guarantees Some");
            if let Some(slot) = hot.page.insert(row_data) {
                hot.dirty = true;
                if hot.page.free_space() < 64 {
                    self.pages_with_space.swap_remove(idx);
                    self.mark_not_free(page_id);
                }
                return Ok(RowId {
                    page_id,
                    slot_index: slot,
                });
            }
            // Page doesn't fit this row; try the next one on the list.
        }

        // Allocate a new page. This *grows the file*, so the persistent
        // mmap snapshot must be invalidated first: after `allocate_page`,
        // `num_pages` increases and the mapping would cover a stale/short
        // region relative to the new file length. This is the only place
        // an `insert` extends the file, so it is the only place a `munmap`
        // is required (WS1: not on every insert).
        self.disable_mmap();
        let page_id = self.disk.allocate_page()?;
        // `allocate_page` zero-extends the file (no synchronous page write on
        // the insert hot path). The on-disk zeros are immediately shadowed by
        // this dirty hot page and overwritten on the next flush. The only
        // reader that can ever see an unflushed zero page is WAL replay after
        // a crash, and that path (`insert_at`) re-initialises a malformed
        // page before use — so the hot path pays nothing here.
        let mut page = Page::new(page_id, PageType::Data);
        // `check_row_size` at the top of `insert` guarantees this fits, but
        // keep a graceful error (never a panic) as defence in depth.
        let slot = page.insert(row_data).ok_or_else(|| {
            io::Error::from(StorageError::RowTooLarge {
                size: row_data.len(),
                max: MAX_ROW_DATA_SIZE,
            })
        })?;
        if page.free_space() >= 64 {
            self.pages_with_space.push(page_id);
            self.mark_free(page_id);
        }
        self.install_fresh_hot(page_id, page)?;
        Ok(RowId {
            page_id,
            slot_index: slot,
        })
    }

    /// Place a row at an exact RowId. Used only by WAL replay so that a
    /// re-applied Insert lands at the *same* (page, slot) it occupied before
    /// the crash — keeping every later Update/Delete record (which carry that
    /// RowId) correctly targeted. The normal `insert` self-assigns the next
    /// free slot, which can diverge from the logged RowId after a
    /// partial-flush crash (different page packing) and silently orphan those
    /// later records — one of the v0.4.x data-loss bugs.
    ///
    /// Grows the file with valid empty pages (never zero pages) up to the
    /// target page, then delegates to [`Page::insert_at_slot`]. Idempotent:
    /// re-placing an already-present slot is a no-op.
    pub fn insert_at(&mut self, rid: RowId, row_data: &[u8]) -> io::Result<()> {
        // Grow the file so the target page exists. Initialise each new page
        // as a proper empty page so a later read sees a valid layout.
        if rid.page_id >= self.disk.num_pages() {
            self.disable_mmap();
            while self.disk.num_pages() <= rid.page_id {
                let pid = self.disk.allocate_page()?;
                let mut empty = Page::new(pid, PageType::Data);
                empty.stamp_checksum();
                self.disk.write_page(pid, empty.as_bytes())?;
            }
        }
        self.ensure_hot(rid.page_id)?;
        let hot = self.hot_page.as_mut().expect("ensure_hot guarantees Some");
        // A page that was allocated (file grown) but never flushed with real
        // data before the crash reads back as a zero page — a malformed
        // header with `free_start = 0`. Re-initialise it as a fresh empty
        // page so the row lands in a valid layout. (Valid pages, including a
        // persisted prefix on this page, have `free_start >= PAGE_HEADER_SIZE`
        // and are left untouched.)
        if hot.page.is_blank() {
            hot.page = Page::new(rid.page_id, PageType::Data);
        }
        if !hot.page.insert_at_slot(rid.slot_index, row_data) {
            return Err(io::Error::other(format!(
                "replay: row does not fit at {rid:?} (page {} slot {})",
                rid.page_id, rid.slot_index
            )));
        }
        hot.dirty = true;
        Ok(())
    }

    /// Read row data by RowId.
    ///
    /// Mission F: `#[inline]` so the mmap-fast-path branch can fold into
    /// `Catalog::get → Table::get → HeapFile::get` callsites. The hot path
    /// is the mmap branch — inlining lets LTO collapse the whole chain.
    ///
    /// Mission C Phase 1: if the hot page holds `rid.page_id`, read from
    /// it directly. This is what keeps `update_by_pk` fast: the read for
    /// the old row lands on the hot page we're about to write back.
    #[inline]
    pub fn get(&self, rid: RowId) -> Option<Vec<u8>> {
        // Mission C: dirty hot page takes precedence over both mmap and
        // disk — it holds writes that haven't landed yet.
        if let Some(hot) = &self.hot_page {
            if hot.page_id == rid.page_id {
                return hot.page.get(rid.slot_index).map(|d| d.to_vec());
            }
        }

        // Mission C Phase 9: parked dirty page is also authoritative
        // over mmap/disk.
        if let Some(page) = self.dirty_buffer.get(&rid.page_id) {
            return page.get(rid.slot_index).map(|d| d.to_vec());
        }

        // Fast path: mmap — read directly from mapped memory
        if let Some((ptr, len)) = self.mmap_ptr {
            let offset = rid.page_id as usize * PAGE_SIZE;
            if offset + PAGE_SIZE <= len {
                // SAFETY: `ptr` points to a valid read-only mmap of `len`
                // bytes. We checked `offset + PAGE_SIZE <= len`, so the
                // slice is within bounds. The mmap is `MAP_PRIVATE` and
                // `PROT_READ`, and no concurrent writer can mutate the
                // underlying file region while the RwLock read guard is
                // held.
                let page_bytes = unsafe { std::slice::from_raw_parts(ptr.add(offset), PAGE_SIZE) };
                // Bounds check: validate slot_index against the page's
                // actual slot count to prevent OOB reads from stale/invalid
                // RowIds. `slot_count_from_page` clamps a corrupt count to
                // what the page can physically hold, and
                // `slot_bytes_from_page` refuses a slot entry whose
                // offset/length runs off the page, so neither a wild count
                // nor a wild entry can underflow into an out-of-range slice
                // (which in a release build with `panic = "abort"` would kill
                // the process). This path deliberately does NOT verify the
                // page CRC: that is the documented mmap performance tradeoff.
                if rid.slot_index >= crate::page::slot_count_from_page(page_bytes) {
                    return None;
                }
                return crate::page::slot_bytes_from_page(page_bytes, rid.slot_index)
                    .map(|row| row.to_vec());
            }
        }

        let buf = self.disk.read_page(rid.page_id).ok()?;
        // The disk point-lookup path consults the CRC
        // (validate-if-present) rather than trusting the bytes, so a corrupt
        // page reads as absent instead of yielding garbage or panicking.
        let page = Page::from_bytes_verified(&buf).ok()?;
        page.get(rid.slot_index).map(|d| d.to_vec())
    }

    /// Delete a row by marking its slot as deleted.
    ///
    /// Mission C Phase 1: land the change on the hot page so back-to-back
    /// deletes targeting the same page coalesce into one disk write.
    pub fn delete(&mut self, rid: RowId) -> io::Result<()> {
        self.ensure_hot(rid.page_id)?;
        let hot = self.hot_page.as_mut().expect("ensure_hot guarantees Some");
        hot.page.delete(rid.slot_index);
        hot.dirty = true;
        // Mission C Phase 8: O(1) membership check via sidecar bitmap.
        // On scattered `delete_by_filter` runs this used to be the single
        // biggest hot-loop cost — `Vec::contains` grows linearly as pages
        // get added to the free list.
        if !self.is_in_free_list(rid.page_id) {
            self.pages_with_space.push(rid.page_id);
            self.mark_free(rid.page_id);
        }
        Ok(())
    }

    /// Delete a row while giving the caller access to the old bytes in a
    /// single `ensure_hot` pass. The closure runs against the live row
    /// bytes *before* the slot is marked deleted — callers use this to
    /// pull out index keys for secondary-index maintenance without
    /// paying for a second `ensure_hot` round-trip.
    ///
    /// Mission C Phase 12: `Table::delete_many` threads the index-key
    /// extraction through this primitive, so a bulk delete does one
    /// ensure_hot per row instead of two. For a 20K-row
    /// `delete_by_filter` that saves ~800μs of redundant hot-slot lookups.
    ///
    /// Returns `Ok(true)` if the slot was found and deleted, `Ok(false)`
    /// if the slot was already missing (caller should treat as a no-op).
    #[inline]
    pub fn delete_with_hook<F>(&mut self, rid: RowId, hook: F) -> io::Result<bool>
    where
        F: FnOnce(&[u8]),
    {
        self.ensure_hot(rid.page_id)?;
        let found = {
            let hot = self.hot_page.as_mut().expect("ensure_hot guarantees Some");
            // Run the hook under a scoped immutable borrow of the page,
            // then drop that borrow before re-borrowing mutably for
            // `delete`.
            let has_slot = if let Some(bytes) = hot.page.get(rid.slot_index) {
                hook(bytes);
                true
            } else {
                false
            };
            if has_slot {
                hot.page.delete(rid.slot_index);
                hot.dirty = true;
            }
            has_slot
        };
        if found && !self.is_in_free_list(rid.page_id) {
            self.pages_with_space.push(rid.page_id);
            self.mark_free(rid.page_id);
        }
        Ok(found)
    }

    /// Apply an in-place mutation to a row's raw bytes. The closure
    /// receives a `&mut [u8]` of exactly the current row size and MUST NOT
    /// change the slice length. Returns `Ok(true)` if the mutation was
    /// applied, `Ok(false)` if the row is deleted or gone.
    ///
    /// Mission C Phase 4: lets the executor's update-by-pk fast path
    /// patch a fixed-width column (e.g. `age := 42`) directly on the hot
    /// page without allocating a `Vec<Value>`, calling `decode_row`, or
    /// re-running `encode_row_into`. For a 6-column User row that was
    /// ~700ns of work per update; this primitive replaces it with a
    /// single in-memory copy plus a branch.
    #[inline]
    pub fn with_row_bytes_mut<F>(&mut self, rid: RowId, f: F) -> io::Result<bool>
    where
        F: FnOnce(&mut [u8]),
    {
        self.ensure_hot(rid.page_id)?;
        let hot = self.hot_page.as_mut().expect("ensure_hot guarantees Some");
        if let Some(bytes) = hot.page.slot_bytes_mut(rid.slot_index) {
            // v0.11 overflow safety: byte-patch closures assume v1 layout math.
            // Refuse v2 rows so a caller can never corrupt a spilled row's
            // bytes through this primitive; the executor routes v2-capable
            // tables to the reassembling update path instead.
            if row_is_v2(bytes) {
                return Ok(false);
            }
            f(bytes);
            hot.dirty = true;
            return Ok(true);
        }
        Ok(false)
    }

    /// Apply an in-place mutation that may SHRINK a row. The closure
    /// receives `&mut [u8]` of the current row and returns `Some(new_len)`
    /// if the mutation succeeded (with `new_len <= current len`), or
    /// `None` to signal "doesn't fit in place, caller should fall back".
    /// On success the slot directory is updated so the row is now
    /// `new_len` bytes long.
    ///
    /// Mission C Phase 10: backs the var-column update fast path for
    /// `update_by_filter`. The closure uses
    /// [`crate::row::patch_var_column_in_place`] to rewrite the single
    /// changed var column in the row's raw bytes without invoking
    /// `decode_row` / `encode_row_into`.
    ///
    /// Returns `Ok(true)` if the patch landed, `Ok(false)` if the row is
    /// deleted/missing OR the closure returned `None`.
    #[inline]
    pub fn patch_row_shrink<F>(&mut self, rid: RowId, f: F) -> io::Result<bool>
    where
        F: FnOnce(&mut [u8]) -> Option<u16>,
    {
        self.ensure_hot(rid.page_id)?;
        let hot = self.hot_page.as_mut().expect("ensure_hot guarantees Some");
        let Some(bytes) = hot.page.slot_bytes_mut(rid.slot_index) else {
            return Ok(false);
        };
        // v0.11 overflow safety: refuse v2 rows (defence in depth — the patch
        // closure's `patch_var_column_in_place` also refuses them). A v2 row
        // patched with v1 offset math would corrupt.
        if row_is_v2(bytes) {
            return Ok(false);
        }
        let old_len = bytes.len();
        let Some(new_len) = f(bytes) else {
            return Ok(false);
        };
        // Defence in depth: the helper's contract says new_len <= old_len,
        // but if a bug upstream lies to us we don't want to silently corrupt
        // the slot directory.
        if (new_len as usize) > old_len {
            return Ok(false);
        }
        if (new_len as usize) != old_len {
            hot.page.shrink_slot(rid.slot_index, new_len);
        }
        hot.dirty = true;
        Ok(true)
    }

    /// Apply a borrowed read to a row's raw bytes. Like
    /// `with_row_bytes_mut` but without the mutable-access path — the
    /// closure sees the row slice, runs, and returns. No `Vec<u8>` is
    /// allocated, so callers that only want to decode a few columns
    /// (e.g. the index-maintenance side of `Table::delete`) can skip the
    /// per-row clone that `HeapFile::get` would otherwise force.
    ///
    /// Mission C Phase 7: this is the read-side counterpart to the
    /// write-side primitive that backs the Mission C Phase 4 update fast
    /// path. Same rationale — avoid allocating a whole row buffer just
    /// to read a handful of bytes out of it.
    #[inline]
    pub fn with_row_bytes<R, F>(&mut self, rid: RowId, f: F) -> io::Result<Option<R>>
    where
        F: FnOnce(&[u8]) -> R,
    {
        self.ensure_hot(rid.page_id)?;
        let hot = self.hot_page.as_ref().expect("ensure_hot guarantees Some");
        if let Some(bytes) = hot.page.get(rid.slot_index) {
            return Ok(Some(f(bytes)));
        }
        Ok(None)
    }

    /// Single-pass scan-and-delete. Walks every page in order, running
    /// `pred` on each live row's raw bytes. When `pred` returns `true`,
    /// `hook` is called with the same bytes (caller uses this to extract
    /// index keys before the slot is cleared) and the slot is marked
    /// deleted in place. Returns the total number of rows removed.
    ///
    /// Mission C Phase 16: fuses `collect_rids_for_mutation` +
    /// `delete_many` into one traversal. The old path did two walks over
    /// the heap — first building a `Vec<RowId>` via `for_each_row` (reads
    /// from mmap), then visiting each rid via `delete_with_hook` which
    /// called `ensure_hot(rid.page_id)` per row. Even when the rids were
    /// already sorted by page_id, every page boundary cost a
    /// `park_hot_page` + `Page::from_bytes` (4KB memcpy from the dirty
    /// buffer or mmap). For a 100K-row `delete_by_filter` with ~20K
    /// matches spread across ~3000 pages, that was ~3000 redundant page
    /// installs worth ~500-800ns each — meaningful slice of a ~1.9ms
    /// query. This primitive does exactly one `ensure_hot` per page and
    /// mutates in place under the single pinned borrow.
    #[inline]
    pub fn scan_delete_matching<P, H>(&mut self, mut pred: P, mut hook: H) -> io::Result<u64>
    where
        P: FnMut(&[u8]) -> bool,
        H: FnMut(RowId, &[u8]),
    {
        let num_pages = self.disk.num_pages();
        if num_pages == 0 {
            return Ok(0);
        }
        let mut count = 0u64;
        for page_id in 0..num_pages {
            self.ensure_hot(page_id)?;
            let mut any_deleted = false;
            {
                let hot = self.hot_page.as_mut().expect("ensure_hot guarantees Some");
                // Overflow-chain pages carry value payload, not a slot
                // directory — never walk them as rows.
                if hot.page.is_overflow() {
                    continue;
                }
                let slot_count = hot.page.slot_count();
                for slot in 0..slot_count {
                    // Scoped immutable borrow for the pred/hook invocation,
                    // then a separate mutable call to `delete`. The borrow
                    // checker is happy because each borrow ends inside the
                    // same iteration.
                    let should_delete = match hot.page.get(slot) {
                        Some(bytes) if pred(bytes) => {
                            hook(
                                RowId {
                                    page_id,
                                    slot_index: slot,
                                },
                                bytes,
                            );
                            true
                        }
                        _ => false,
                    };
                    if should_delete {
                        hot.page.delete(slot);
                        any_deleted = true;
                        count += 1;
                    }
                }
                if any_deleted {
                    hot.dirty = true;
                }
            }
            if any_deleted && !self.is_in_free_list(page_id) {
                self.pages_with_space.push(page_id);
                self.mark_free(page_id);
            }
        }
        Ok(count)
    }

    /// Single-pass scan that evaluates a predicate and applies an in-place
    /// mutation to every matching row. Returns `(patched_count, fallback_rids)`
    /// where `fallback_rids` contains rows that matched the predicate but
    /// where `try_mutate` returned `None` (e.g. a var-col patch that
    /// couldn't shrink in place).
    ///
    /// `try_mutate` receives the row's raw `&mut [u8]` and returns
    /// `Some(new_len)` on success (the slot is shrunk if `new_len < old_len`)
    /// or `None` to signal a fallback. For fixed-width patches that don't
    /// change row length, return `Some(bytes.len() as u16)`.
    ///
    /// The `hook` closure fires after every successful patch with the
    /// post-mutation bytes — used by the catalog layer for WAL logging.
    ///
    /// Perf sprint: mirrors `scan_delete_matching` but mutates instead of
    /// deleting. Eliminates the two-pass collect-RIDs-then-loop pattern
    /// that caused `update_by_filter` to pay one `ensure_hot` per matched
    /// row on the second pass.
    pub fn scan_patch_matching<P, M, H>(
        &mut self,
        mut pred: P,
        mut try_mutate: M,
        mut hook: H,
    ) -> io::Result<(u64, Vec<RowId>)>
    where
        P: FnMut(&[u8]) -> bool,
        M: FnMut(&mut [u8]) -> Option<u16>,
        H: FnMut(RowId, &[u8]),
    {
        let num_pages = self.disk.num_pages();
        if num_pages == 0 {
            return Ok((0, Vec::new()));
        }
        let mut count = 0u64;
        let mut fallback: Vec<RowId> = Vec::new();
        for page_id in 0..num_pages {
            self.ensure_hot(page_id)?;
            let hot = self.hot_page.as_mut().expect("ensure_hot guarantees Some");
            // Skip overflow-chain pages (no slot directory to patch).
            if hot.page.is_overflow() {
                continue;
            }
            let slot_count = hot.page.slot_count();
            let mut any_mutated = false;
            for slot in 0..slot_count {
                let matches = match hot.page.get(slot) {
                    Some(bytes) => pred(bytes),
                    None => false,
                };
                if matches {
                    let rid = RowId {
                        page_id,
                        slot_index: slot,
                    };
                    // v0.11 overflow safety: never byte-patch a v2 row here —
                    // `try_mutate` closures assume v1 layout and would corrupt a
                    // spilled row (and Path-1 fixed patches write unconditionally).
                    // Route it to the fallback list so the caller re-applies the
                    // mutation through the reassembling update path.
                    if hot.page.get(slot).map(row_is_v2).unwrap_or(false) {
                        fallback.push(rid);
                        continue;
                    }
                    if let Some(bytes) = hot.page.slot_bytes_mut(slot) {
                        let old_len = bytes.len() as u16;
                        if let Some(new_len) = try_mutate(bytes) {
                            if new_len < old_len {
                                hot.page.shrink_slot(slot, new_len);
                            }
                            // Re-read the (possibly shrunk) bytes for the hook.
                            if let Some(final_bytes) = hot.page.get(slot) {
                                hook(rid, final_bytes);
                            }
                            any_mutated = true;
                            count += 1;
                        } else {
                            fallback.push(rid);
                        }
                    }
                }
            }
            if any_mutated {
                hot.dirty = true;
            }
        }
        Ok((count, fallback))
    }

    /// Where an update of `rid` to `row_len` bytes would land, without touching
    /// the row. [`Self::update`] resolves the same question through the same
    /// [`Page::update_fit`] call, so the two cannot disagree.
    ///
    /// The WAL path needs the answer *before* it appends a record: a relocating
    /// update is not idempotent (its redo is delete + insert, and the insert
    /// self-assigns a slot), so it has to be logged as the two physical steps
    /// the heap is about to take rather than as one Update record. See
    /// `Catalog::update_logged`.
    ///
    /// Loads the page into the hot slot, which is where [`Self::update`] wants
    /// it anyway: the probe costs no extra I/O on the update path.
    pub fn update_fit(&mut self, rid: RowId, row_len: usize) -> io::Result<UpdateFit> {
        self.ensure_hot(rid.page_id)?;
        let hot = self.hot_page.as_ref().expect("ensure_hot guarantees Some");
        Ok(hot.page.update_fit(rid.slot_index, row_len))
    }

    /// Update a row. Returns new RowId (may change if row moves).
    ///
    /// Mission C Phase 1: in-place updates land on the hot page directly.
    /// `update_by_filter` and `update_by_pk` both route here.
    pub fn update(&mut self, rid: RowId, row_data: &[u8]) -> io::Result<RowId> {
        // Reject oversized rows BEFORE the delete+insert fallback below can
        // delete the old row — otherwise a failed oversized update would
        // destroy the existing data.
        Self::check_row_size(row_data)?;
        self.ensure_hot(rid.page_id)?;
        {
            let hot = self.hot_page.as_mut().expect("ensure_hot guarantees Some");
            if hot.page.update(rid.slot_index, row_data) {
                hot.dirty = true;
                return Ok(rid);
            }
        }
        // Doesn't fit in place — delete old, insert new. Both helpers also
        // go through the hot page, so the follow-up insert typically
        // lands on the same page and avoids another read.
        self.delete(rid)?;
        self.insert(row_data)
    }

    /// Scan all live rows across all pages.
    ///
    /// Mission C Phase 1: observes the pinned hot page so callers see
    /// unflushed writes. The iterator materialises the result list up front
    /// (same as before — the returned type was already an owned flat_map),
    /// so copying the hot page bytes into the result costs nothing extra.
    ///
    /// Fail-closed (scan-corruption fix): every page read off the disk goes
    /// through [`Page::from_bytes_verified`], the same gate `ensure_hot`
    /// applies to point reads. A page that cannot be read or fails
    /// verification surfaces as an `Err` item; it used to be silently
    /// mapped to zero rows, which turned mid-life corruption or an EIO into
    /// a plausible-but-partial result for index rebuilds, vacuum snapshots,
    /// and full-table queries. See `tests/scan_corruption.rs`.
    /// Whether the heap holds at least one live row. Fail-closed: an
    /// unreadable or unverifiable first page is an error, not "empty" —
    /// callers gate DDL decisions on this answer.
    pub fn has_rows(&self) -> io::Result<bool> {
        match self.scan().next() {
            Some(Err(e)) => Err(e),
            Some(Ok(_)) => Ok(true),
            None => Ok(false),
        }
    }

    pub fn scan(&self) -> impl Iterator<Item = io::Result<(RowId, Vec<u8>)>> + '_ {
        let hot_view = self
            .hot_page
            .as_ref()
            .map(|hot| (hot.page_id, *hot.page.as_bytes()));
        (0..self.disk.num_pages()).flat_map(move |page_id| {
            // Mission C Phase 9: parked dirty pages override disk.
            if let Some(page) = self.dirty_buffer.get(&page_id) {
                let entries: Vec<io::Result<(RowId, Vec<u8>)>> = page
                    .iter()
                    .map(|(slot, data)| {
                        Ok((
                            RowId {
                                page_id,
                                slot_index: slot,
                            },
                            data.to_vec(),
                        ))
                    })
                    .collect();
                return entries.into_iter();
            }
            let entries: Vec<io::Result<(RowId, Vec<u8>)>> = match &hot_view {
                Some((hid, hbytes)) if *hid == page_id => iter_page_slots(hbytes.as_slice())
                    .map(|(slot, data)| {
                        Ok((
                            RowId {
                                page_id,
                                slot_index: slot,
                            },
                            data.to_vec(),
                        ))
                    })
                    .collect(),
                _ => match self
                    .disk
                    .read_page(page_id)
                    .and_then(|buf| Page::from_bytes_verified(&buf).map_err(io::Error::from))
                {
                    Ok(page) => page
                        .iter()
                        .map(|(slot, data)| {
                            Ok((
                                RowId {
                                    page_id,
                                    slot_index: slot,
                                },
                                data.to_vec(),
                            ))
                        })
                        .collect(),
                    Err(e) => vec![Err(e)],
                },
            };
            entries.into_iter()
        })
    }

    /// Zero-copy scan with early termination. The callback returns
    /// `ControlFlow::Break(())` to stop iteration immediately.
    ///
    /// Mission D2: this is the load-bearing fix for `Project(Limit(...))`
    /// fast paths. Without it, `limit 100` on a 100K-row table still walked
    /// all 100K slots — the existing `done` flag in executor only short-
    /// circuited the *body* of the closure, not the iteration itself, so
    /// the inner loop kept paying decode_column / pred / call-frame cost
    /// for the trailing 99,900 rows.
    ///
    /// Mission D6: prefer the persistent mmap set by `enable_mmap()` instead
    /// of doing mmap+munmap on every call. The bench's per-query mmap pair
    /// was a syscall pair we paid on every read query.
    ///
    /// Fail-closed (scan-corruption fix): the per-page-read fallback
    /// propagates read errors and verifies pages via
    /// [`Page::from_bytes_verified`] instead of silently skipping a page it
    /// cannot read (see `tests/scan_corruption.rs`). The mmap paths carry a
    /// different, deliberate contract: every page was verified by
    /// `HeapFile::open`, an I/O error on a mapped page raises SIGBUS (loud,
    /// never a partial result), and re-verifying CRCs per scan would forfeit
    /// the zero-copy design, so mapped bytes are served as-is and post-open
    /// bit rot in a mapped page is bounded by the slot-arithmetic clamps.
    #[inline]
    pub fn try_for_each_row<F>(&self, mut f: F) -> io::Result<()>
    where
        F: FnMut(RowId, &[u8]) -> std::ops::ControlFlow<()>,
    {
        use std::ops::ControlFlow;

        let num_pages = self.disk.num_pages();
        if num_pages == 0 {
            return Ok(());
        }

        // Mission C Phase 1: if a hot page is pinned in memory, the scan
        // must observe its dirty contents — the mmap and the file both
        // still hold the stale version. We substitute the in-memory page
        // when the loop reaches its page_id.
        let hot_view: Option<(u32, &[u8; PAGE_SIZE])> = self
            .hot_page
            .as_ref()
            .map(|hot| (hot.page_id, hot.page.as_bytes()));

        // Fast path: persistent mmap activated by `enable_mmap()`. Zero
        // syscalls per query — we just slice the existing mapping.
        if let Some((ptr, len)) = self.mmap_ptr {
            // SAFETY: `ptr` is a valid read-only mmap pointer of `len`
            // bytes, established by `enable_mmap`. All dirty pages were
            // flushed before the mmap was created, and mutations
            // invalidate it via `disable_mmap`. The slice lifetime is
            // bounded by this method call.
            let mapped = unsafe { std::slice::from_raw_parts(ptr, len) };
            let pages_in_map = len / PAGE_SIZE;
            let limit = num_pages.min(pages_in_map as u32);
            'outer: for page_id in 0..limit {
                // Mission C Phase 9: dirty buffer > hot page > mmap.
                if let Some(page) = self.dirty_buffer.get(&page_id) {
                    for (slot, data) in iter_page_slots(page.as_bytes()) {
                        if let ControlFlow::Break(()) = f(
                            RowId {
                                page_id,
                                slot_index: slot,
                            },
                            data,
                        ) {
                            break 'outer;
                        }
                    }
                    continue;
                }
                let page_bytes: &[u8] = match hot_view {
                    Some((hid, hbytes)) if hid == page_id => hbytes.as_slice(),
                    _ => {
                        let offset = page_id as usize * PAGE_SIZE;
                        &mapped[offset..offset + PAGE_SIZE]
                    }
                };
                for (slot, data) in iter_page_slots(page_bytes) {
                    if let ControlFlow::Break(()) = f(
                        RowId {
                            page_id,
                            slot_index: slot,
                        },
                        data,
                    ) {
                        break 'outer;
                    }
                }
            }
            // The mmap may not have grown to cover pages allocated after
            // enable_mmap. If the hot page lives beyond that window, visit
            // it explicitly so inserts into fresh pages stay observable.
            if let Some((hid, hbytes)) = hot_view {
                if hid >= limit && hid < num_pages && !self.dirty_buffer.contains_key(&hid) {
                    for (slot, data) in iter_page_slots(hbytes) {
                        if let ControlFlow::Break(()) = f(
                            RowId {
                                page_id: hid,
                                slot_index: slot,
                            },
                            data,
                        ) {
                            return Ok(());
                        }
                    }
                }
            }
            // Visit any dirty-buffered pages that sit beyond the mmap
            // window (pages allocated after enable_mmap that we then
            // evicted from the hot slot).
            for page_id in limit..num_pages {
                if let Some(page) = self.dirty_buffer.get(&page_id) {
                    for (slot, data) in iter_page_slots(page.as_bytes()) {
                        if let ControlFlow::Break(()) = f(
                            RowId {
                                page_id,
                                slot_index: slot,
                            },
                            data,
                        ) {
                            return Ok(());
                        }
                    }
                }
            }
            return Ok(());
        }

        // No persistent mmap — try a per-call mmap as a one-shot best effort.
        use std::os::unix::io::AsRawFd;
        let fd = self.disk.file_ref().as_raw_fd();
        let file_len = (num_pages as usize) * PAGE_SIZE;
        // SAFETY: `fd` is a valid file descriptor from `self.disk`.
        // `file_len` matches the file's actual size (`num_pages *
        // PAGE_SIZE`). The mapping is read-only and private.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                file_len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                fd,
                0,
            )
        };

        if ptr != libc::MAP_FAILED {
            // SAFETY: mmap succeeded — `ptr` is valid for `file_len`
            // bytes. The mapping is read-only (`PROT_READ`) and private.
            let mapped = unsafe { std::slice::from_raw_parts(ptr as *const u8, file_len) };
            'outer: for page_id in 0..num_pages {
                // Mission C Phase 9: dirty buffer has priority.
                if let Some(page) = self.dirty_buffer.get(&page_id) {
                    for (slot, data) in iter_page_slots(page.as_bytes()) {
                        if let ControlFlow::Break(()) = f(
                            RowId {
                                page_id,
                                slot_index: slot,
                            },
                            data,
                        ) {
                            break 'outer;
                        }
                    }
                    continue;
                }
                let page_bytes: &[u8] = match hot_view {
                    Some((hid, hbytes)) if hid == page_id => hbytes.as_slice(),
                    _ => {
                        let offset = page_id as usize * PAGE_SIZE;
                        &mapped[offset..offset + PAGE_SIZE]
                    }
                };
                for (slot, data) in iter_page_slots(page_bytes) {
                    if let ControlFlow::Break(()) = f(
                        RowId {
                            page_id,
                            slot_index: slot,
                        },
                        data,
                    ) {
                        break 'outer;
                    }
                }
            }
            // SAFETY: `ptr` and `file_len` came from the successful
            // `libc::mmap` call above. We unmap exactly once.
            unsafe {
                libc::munmap(ptr, file_len);
            }
        } else {
            // Fallback: per-page read.
            'outer: for page_id in 0..num_pages {
                if let Some(page) = self.dirty_buffer.get(&page_id) {
                    for (slot, data) in iter_page_slots(page.as_bytes()) {
                        if let ControlFlow::Break(()) = f(
                            RowId {
                                page_id,
                                slot_index: slot,
                            },
                            data,
                        ) {
                            break 'outer;
                        }
                    }
                    continue;
                }
                if let Some((hid, hbytes)) = hot_view {
                    if hid == page_id {
                        for (slot, data) in iter_page_slots(hbytes) {
                            if let ControlFlow::Break(()) = f(
                                RowId {
                                    page_id,
                                    slot_index: slot,
                                },
                                data,
                            ) {
                                break 'outer;
                            }
                        }
                        continue;
                    }
                }
                // Fail closed, matching `ensure_hot`: a page that cannot be
                // read or fails verification is an error, never a skip.
                let buf = self.disk.read_page(page_id)?;
                let page = Page::from_bytes_verified(&buf).map_err(io::Error::from)?;
                for (slot, data) in page.iter() {
                    if let ControlFlow::Break(()) = f(
                        RowId {
                            page_id,
                            slot_index: slot,
                        },
                        data,
                    ) {
                        break 'outer;
                    }
                }
            }
        }
        Ok(())
    }

    /// Zero-copy scan: calls `f` for every live row without allocating a
    /// `Vec<u8>` per row. Uses the persistent mmap activated by
    /// `enable_mmap()` when available, otherwise falls back to a per-call
    /// mmap or page-by-page read.
    ///
    /// Mission D6: same persistent-mmap fix as `try_for_each_row`.
    ///
    /// Mission C Phase 1: same hot-page substitution as `try_for_each_row`.
    /// Scans of tables with unflushed writes see the latest bytes via the
    /// in-memory page rather than the stale disk page.
    #[inline]
    pub fn for_each_row<F>(&self, mut f: F) -> io::Result<()>
    where
        F: FnMut(RowId, &[u8]),
    {
        let num_pages = self.disk.num_pages();
        if num_pages == 0 {
            return Ok(());
        }

        let hot_view: Option<(u32, &[u8; PAGE_SIZE])> = self
            .hot_page
            .as_ref()
            .map(|hot| (hot.page_id, hot.page.as_bytes()));

        // Fast path: persistent mmap.
        if let Some((ptr, len)) = self.mmap_ptr {
            // SAFETY: `ptr` is a valid read-only mmap pointer of `len`
            // bytes, established by `enable_mmap`. See the SAFETY note
            // in `try_for_each_row` for the full argument.
            let mapped = unsafe { std::slice::from_raw_parts(ptr, len) };
            let pages_in_map = len / PAGE_SIZE;
            let limit = num_pages.min(pages_in_map as u32);
            for page_id in 0..limit {
                if let Some(page) = self.dirty_buffer.get(&page_id) {
                    for (slot, data) in iter_page_slots(page.as_bytes()) {
                        f(
                            RowId {
                                page_id,
                                slot_index: slot,
                            },
                            data,
                        );
                    }
                    continue;
                }
                let page_bytes: &[u8] = match hot_view {
                    Some((hid, hbytes)) if hid == page_id => hbytes.as_slice(),
                    _ => {
                        let offset = page_id as usize * PAGE_SIZE;
                        &mapped[offset..offset + PAGE_SIZE]
                    }
                };
                for (slot, data) in iter_page_slots(page_bytes) {
                    f(
                        RowId {
                            page_id,
                            slot_index: slot,
                        },
                        data,
                    );
                }
            }
            // Hot page allocated after enable_mmap — visit it explicitly.
            if let Some((hid, hbytes)) = hot_view {
                if hid >= limit && hid < num_pages && !self.dirty_buffer.contains_key(&hid) {
                    for (slot, data) in iter_page_slots(hbytes) {
                        f(
                            RowId {
                                page_id: hid,
                                slot_index: slot,
                            },
                            data,
                        );
                    }
                }
            }
            for page_id in limit..num_pages {
                if let Some(page) = self.dirty_buffer.get(&page_id) {
                    for (slot, data) in iter_page_slots(page.as_bytes()) {
                        f(
                            RowId {
                                page_id,
                                slot_index: slot,
                            },
                            data,
                        );
                    }
                }
            }
            return Ok(());
        }

        // No persistent mmap — try a per-call mmap as a one-shot best effort.
        use std::os::unix::io::AsRawFd;
        let fd = self.disk.file_ref().as_raw_fd();
        let file_len = (num_pages as usize) * PAGE_SIZE;
        // SAFETY: `fd` is a valid file descriptor. `file_len` matches
        // the actual file size. Mapping is read-only and private.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                file_len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                fd,
                0,
            )
        };

        if ptr != libc::MAP_FAILED {
            // SAFETY: mmap succeeded — `ptr` is valid for `file_len` bytes.
            let mapped = unsafe { std::slice::from_raw_parts(ptr as *const u8, file_len) };
            for page_id in 0..num_pages {
                if let Some(page) = self.dirty_buffer.get(&page_id) {
                    for (slot, data) in iter_page_slots(page.as_bytes()) {
                        f(
                            RowId {
                                page_id,
                                slot_index: slot,
                            },
                            data,
                        );
                    }
                    continue;
                }
                let page_bytes: &[u8] = match hot_view {
                    Some((hid, hbytes)) if hid == page_id => hbytes.as_slice(),
                    _ => {
                        let offset = page_id as usize * PAGE_SIZE;
                        &mapped[offset..offset + PAGE_SIZE]
                    }
                };
                for (slot, data) in iter_page_slots(page_bytes) {
                    f(
                        RowId {
                            page_id,
                            slot_index: slot,
                        },
                        data,
                    );
                }
            }
            // SAFETY: `ptr` and `file_len` from the successful mmap above.
            unsafe {
                libc::munmap(ptr, file_len);
            }
        } else {
            // Fallback: per-page read
            for page_id in 0..num_pages {
                if let Some(page) = self.dirty_buffer.get(&page_id) {
                    for (slot, data) in iter_page_slots(page.as_bytes()) {
                        f(
                            RowId {
                                page_id,
                                slot_index: slot,
                            },
                            data,
                        );
                    }
                    continue;
                }
                if let Some((hid, hbytes)) = hot_view {
                    if hid == page_id {
                        for (slot, data) in iter_page_slots(hbytes) {
                            f(
                                RowId {
                                    page_id,
                                    slot_index: slot,
                                },
                                data,
                            );
                        }
                        continue;
                    }
                }
                // Fail closed, matching `ensure_hot`: a page that cannot
                // be read or fails verification is an error, never a skip.
                let buf = self.disk.read_page(page_id)?;
                let page = Page::from_bytes_verified(&buf).map_err(io::Error::from)?;
                for (slot, data) in page.iter() {
                    f(
                        RowId {
                            page_id,
                            slot_index: slot,
                        },
                        data,
                    );
                }
            }
        }
        Ok(())
    }

    /// Return the maximum LSN across all pages in this heap file.
    /// Scans every page header (hot page, dirty buffer, and disk) to
    /// find the highest LSN. Used by WAL replay to determine which
    /// records have already been applied.
    pub fn max_page_lsn(&self) -> u64 {
        use crate::page::page_lsn;
        let mut max_lsn = 0u64;

        // Check hot page.
        if let Some(hot) = &self.hot_page {
            max_lsn = max_lsn.max(hot.page.lsn());
        }

        // Check dirty buffer.
        for page in self.dirty_buffer.values() {
            max_lsn = max_lsn.max(page.lsn());
        }

        // Check disk pages.
        for page_id in 0..self.disk.num_pages() {
            // Skip pages we already have in memory.
            if self.hot_page.as_ref().is_some_and(|h| h.page_id == page_id) {
                continue;
            }
            if self.dirty_buffer.contains_key(&page_id) {
                continue;
            }
            if let Ok(buf) = self.disk.read_page(page_id) {
                max_lsn = max_lsn.max(page_lsn(&buf));
            }
        }

        max_lsn
    }

    /// Read the LSN currently stamped on a single page, consulting the
    /// in-memory hot page and dirty buffer before falling back to the
    /// on-disk copy (mirrors [`Self::max_page_lsn`]'s precedence). Returns
    /// 0 for a page id past the end of the file or any page that has never
    /// been stamped. This is the per-page granularity WAL replay needs:
    /// a record is already durable iff its target page's LSN is >= the
    /// record's LSN. The table-wide max is too coarse — a low-LSN record on
    /// an unflushed page would be wrongly skipped because some *other*
    /// flushed page carries a higher LSN.
    pub fn page_lsn(&self, page_id: u32) -> u64 {
        use crate::page::page_lsn;
        if let Some(hot) = &self.hot_page {
            if hot.page_id == page_id {
                return hot.page.lsn();
            }
        }
        if let Some(page) = self.dirty_buffer.get(&page_id) {
            return page.lsn();
        }
        if page_id < self.disk.num_pages() {
            if let Ok(buf) = self.disk.read_page(page_id) {
                return page_lsn(&buf);
            }
        }
        0
    }

    /// Stamp every page (hot, dirty, on-disk) with at least `barrier_lsn`.
    /// Pages already at a higher LSN are left untouched (the inner
    /// [`Self::set_page_lsn`] enforces monotonicity).
    ///
    /// Used by schema-change paths: after `rewrite_rows_for_schema_change`
    /// converts every row to the new layout, the heap pages must
    /// advertise an LSN >= the DDL record's LSN so WAL replay knows the
    /// pre-DDL Insert/Update/Delete records have already been folded into
    /// the new on-disk layout and must be skipped (otherwise replay would
    /// re-inject rows in the OLD layout next to the rewritten rows and
    /// corrupt the heap).
    pub fn stamp_all_pages_min_lsn(&mut self, barrier_lsn: u64) -> io::Result<()> {
        if barrier_lsn == 0 {
            return Ok(());
        }
        let n = self.disk.num_pages();
        for page_id in 0..n {
            self.set_page_lsn(page_id, barrier_lsn)?;
        }
        Ok(())
    }

    /// The inverse of [`Self::stamp_all_pages_min_lsn`]: does every page
    /// already carry an LSN >= `barrier_lsn`? True means that barrier's stamp
    /// already ran and reached disk, so whatever heap rewrite it guards is
    /// already folded into these pages. Walks the same page range and reads the
    /// same header field the stamp writes, so the two cannot drift.
    pub fn all_pages_at_min_lsn(&self, barrier_lsn: u64) -> bool {
        if barrier_lsn == 0 {
            return false;
        }
        (0..self.disk.num_pages()).all(|page_id| self.page_lsn(page_id) >= barrier_lsn)
    }

    /// Set the LSN on a specific page. Loads the page into the hot
    /// slot if needed, stamps the LSN, and marks it dirty.
    pub fn set_page_lsn(&mut self, page_id: u32, lsn: u64) -> io::Result<()> {
        self.ensure_hot(page_id)?;
        if let Some(hot) = self.hot_page.as_mut() {
            if hot.page.lsn() < lsn {
                hot.page.set_lsn(lsn);
                hot.dirty = true;
            }
        }
        Ok(())
    }

    /// Scan every page on disk and verify its CRC32 checksum, returning the
    /// first `PageCorrupt` error encountered (or `Ok(())` if all pages are
    /// intact / legacy-unstamped).
    ///
    /// # Why this exists (the honest checksum contract)
    ///
    /// PowDB's hot read paths — `ensure_hot`'s mmap branch and the zero-copy
    /// `try_for_each_row*` scan path — deliberately read page bytes through
    /// the mmap **without** per-read CRC verification. Verifying every page on
    /// every scan would re-hash 4KB per page on the critical path and erase
    /// the scan and aggregate wins that are PowDB's headline numbers. So the
    /// checksum guarantee is scoped, not universal:
    ///
    /// * The **write path** stamps a CRC on every flushed page
    ///   ([`Page::stamp_checksum`]).
    /// * **Cold reads** (the `disk.read_page` fallback in `Self::ensure_hot`)
    ///   verify via [`Page::from_bytes_verified`].
    /// * The **hot mmap read path** trades per-read verification for speed.
    ///   On-disk bit-rot in a page that is only ever read through the mmap
    ///   fast path is NOT caught implicitly.
    ///
    /// `verify_integrity()` closes that gap explicitly: call it at startup or
    /// on demand (e.g. a `CHECK TABLE`-style operation, or a periodic scrub)
    /// to detect silent corruption across the entire file regardless of which
    /// read path serves a page. It reads each page directly off disk via
    /// [`Page::from_bytes_verified`], so it is correct whether or not an mmap
    /// is active — it does not consult the mmap snapshot, the hot page, or the
    /// dirty buffer (those in-memory copies are trusted; corruption we care
    /// about here is on-disk bit-rot).
    pub fn verify_integrity(&self) -> crate::error::Result<()> {
        for page_id in 0..self.disk.num_pages() {
            let buf = self.disk.read_page(page_id)?;
            // Returns PageCorrupt on a CRC mismatch for stamped pages;
            // legacy unstamped pages (flag clear) pass without verification.
            Page::from_bytes_verified(&buf)?;
        }
        Ok(())
    }

    /// Mission C Phase 1: flush the hot page (if dirty) before syncing the
    /// underlying file. A bare `disk.flush()` would otherwise miss the
    /// in-memory dirty buffer.
    pub fn flush(&mut self) -> io::Result<()> {
        self.flush_hot_page()?;
        self.disk.flush()
    }

    /// Discard all in-memory dirty state (hot page + dirty buffer) WITHOUT
    /// writing anything to disk. Used by ROLLBACK to throw away uncommitted
    /// mutations so the subsequent `Drop` doesn't accidentally persist them.
    ///
    /// After this call the HeapFile is in a "cold" state: all reads will go
    /// through disk or mmap, and the next mutation will start fresh.
    pub fn discard_dirty(&mut self) {
        // Drop the hot page without parking it into the dirty buffer.
        self.hot_page = None;
        // Clear every buffered dirty page — they contain uncommitted writes.
        self.dirty_budget.release(self.dirty_buffer.len());
        self.dirty_buffer.clear();
        // Tear down the mmap so stale mappings don't confuse the next reader.
        self.disable_mmap();
    }
}

impl Drop for HeapFile {
    fn drop(&mut self) {
        // Mission C Phase 1 / Phase 9: persist the hot page AND every
        // parked dirty page before the file handle goes away. Without
        // this, the final write-back of a bench's last batch (and of
        // any deferred-flush mutations) would be lost on close.
        let _ = self.flush_all_dirty();
        self.disable_mmap();
    }
}

// SAFETY: The mmap pointer is read-only and the file is not modified
// while the map is active. The HeapFile is not Send/Sync anyway (it
// contains DiskManager with File), so this is fine for single-threaded use.
unsafe impl Send for HeapFile {}
// SAFETY: Blocker B1 fix. `HeapFile` lives behind `Arc<RwLock<Engine>>`,
// so the standard `&self`/`&mut self` discipline applies: many readers
// or one writer, never both. The interesting question is whether the
// `&self` read path is itself thread-safe across multiple reader threads.
//
// The disk fallback (`DiskManager::read_page` / `write_page`) now uses
// `FileExt::read_exact_at` / `write_all_at`, which map to pread(2) /
// pwrite(2). POSIX guarantees these are atomic with respect to the kernel
// file offset, so concurrent `&self` callers sharing a single `&File`
// cannot race on a seek cursor the way a `seek + read_exact` pair would.
// Byte-level corruption under concurrent reads — the old bug — is gone.
//
// The `mmap_ptr` field is a `*const u8` into a read-only mmap. Read-only
// `&[u8]` views derived via `std::slice::from_raw_parts` are fine to
// alias across threads: no `&mut` can coexist with the readers because
// the RwLock write guard excludes them. Writers still take the write
// guard for higher-level consistency (catalog/header mutation); this
// SAFETY note is strictly about the read path not corrupting bytes.
//
// WS4-mmap: the mmap/write torn-read window is closed by THREE
// independent mechanisms working together. A torn read would require all
// three to fail simultaneously:
//
//   1. RwLock exclusion (caller contract). Reads take `&self` (a read
//      guard); writes take `&mut self` (the exclusive write guard). A
//      reader can therefore never hold a raw mmap-derived `&[u8]` slice
//      while a writer is mid-`insert`/`munmap`. Every reader API
//      (`get`, `scan`, `try_for_each_row`, `for_each_row`) confines its
//      mmap-derived borrow to the body of the call — no slice escapes the
//      guard (`get`/`scan` copy out owned `Vec<u8>`; the `*_each_row`
//      variants pass `&[u8]` into a closure that runs synchronously).
//   2. Read ordering: hot page + dirty buffer BEFORE mmap. In-place
//      mutations (`update`, `with_row_bytes_mut`, `patch_row_shrink`)
//      land on the in-memory hot page, never through the read-only mmap.
//      Because `get`/`scan` consult the dirty hot page and dirty buffer
//      first, a reader observing a page under mutation reads the live
//      in-memory bytes, not the mmap's stale snapshot — so there is no
//      half-written page to tear on.
//   3. Munmap only on growth (WS1). `disable_mmap` (the only `munmap`)
//      fires solely on the new-page allocation path, which holds the
//      write guard. It never runs concurrently with a reader.
//
// The `heap_mmap_race` integration test exercises (1)+(2)+(3) under
// concurrent reader/writer threads sharing an `Arc<RwLock<HeapFile>>`,
// asserting every scanned row decodes to its expected invariant.
unsafe impl Sync for HeapFile {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row::{decode_row, encode_row};
    use crate::types::*;

    fn user_schema() -> Schema {
        Schema {
            table_name: "users".into(),
            columns: vec![
                ColumnDef {
                    name: "name".into(),
                    type_id: TypeId::Str,
                    required: true,
                    position: 0,
                },
                ColumnDef {
                    name: "age".into(),
                    type_id: TypeId::Int,
                    required: false,
                    position: 1,
                },
            ],
        }
    }

    fn temp_heap(name: &str) -> (HeapFile, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("powdb_heap_{name}_{}", std::process::id()));
        let heap = HeapFile::create(&path).unwrap();
        (heap, path)
    }

    #[test]
    fn test_insert_and_get() {
        let (mut heap, path) = temp_heap("basic");
        let schema = user_schema();
        let row = vec![Value::Str("Alice".into()), Value::Int(30)];
        let encoded = encode_row(&schema, &row);
        let rid = heap.insert(&encoded).unwrap();
        let data = heap.get(rid).unwrap();
        let decoded = decode_row(&schema, &data);
        assert_eq!(decoded[0], Value::Str("Alice".into()));
        assert_eq!(decoded[1], Value::Int(30));
        drop(heap);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_scan_all_rows() {
        let (mut heap, path) = temp_heap("scan");
        let schema = user_schema();
        for i in 0..100 {
            let row = vec![Value::Str(format!("user_{i}")), Value::Int(i)];
            heap.insert(&encode_row(&schema, &row)).unwrap();
        }
        let all: Vec<_> = heap.scan().map(|r| r.unwrap()).collect();
        assert_eq!(all.len(), 100);
        drop(heap);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_delete_row() {
        let (mut heap, path) = temp_heap("del");
        let schema = user_schema();
        let r1 = heap
            .insert(&encode_row(
                &schema,
                &[Value::Str("A".into()), Value::Int(1)],
            ))
            .unwrap();
        let r2 = heap
            .insert(&encode_row(
                &schema,
                &[Value::Str("B".into()), Value::Int(2)],
            ))
            .unwrap();
        heap.delete(r1).unwrap();
        assert!(heap.get(r1).is_none());
        assert!(heap.get(r2).is_some());
        assert_eq!(
            heap.scan()
                .collect::<std::io::Result<Vec<_>>>()
                .unwrap()
                .len(),
            1
        );
        drop(heap);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_update_row() {
        let (mut heap, path) = temp_heap("upd");
        let schema = user_schema();
        let row = vec![Value::Str("Alice".into()), Value::Int(30)];
        let rid = heap.insert(&encode_row(&schema, &row)).unwrap();
        let new_row = vec![Value::Str("Alice".into()), Value::Int(31)];
        let new_rid = heap.update(rid, &encode_row(&schema, &new_row)).unwrap();
        let decoded = decode_row(&schema, &heap.get(new_rid).unwrap());
        assert_eq!(decoded[1], Value::Int(31));
        drop(heap);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_scan_delete_matching_basic() {
        let (mut heap, path) = temp_heap("sdm_basic");
        let schema = user_schema();
        // Insert enough rows to span multiple pages so the per-page
        // ensure_hot loop is actually exercised.
        let mut inserted = Vec::new();
        for i in 0..500 {
            let row = vec![Value::Str(format!("user_{i:04}")), Value::Int(i)];
            inserted.push(heap.insert(&encode_row(&schema, &row)).unwrap());
        }

        // Delete every row whose age is even via raw-bytes predicate.
        // The age column is at schema position 1 (after the name str).
        let layout = crate::row::RowLayout::new(&schema);
        let mut deleted_keys: Vec<i64> = Vec::new();
        let count = heap
            .scan_delete_matching(
                |data| match crate::row::decode_column(&schema, &layout, data, 1) {
                    Value::Int(i) => i % 2 == 0,
                    _ => false,
                },
                |_rid, data| {
                    if let Value::Int(i) = crate::row::decode_column(&schema, &layout, data, 1) {
                        deleted_keys.push(i);
                    }
                },
            )
            .unwrap();

        assert_eq!(count, 250); // half the rows
        assert_eq!(deleted_keys.len(), 250);
        deleted_keys.sort_unstable();
        let expected: Vec<i64> = (0..500).step_by(2).collect();
        assert_eq!(deleted_keys, expected);
        // Remaining rows should all be odd.
        let remaining: Vec<_> = heap.scan().map(|r| r.unwrap()).collect();
        assert_eq!(remaining.len(), 250);
        for (_, data) in &remaining {
            let row = decode_row(&schema, data);
            if let Value::Int(i) = &row[1] {
                assert_eq!(i % 2, 1);
            }
        }
        drop(heap);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_scan_delete_matching_all_or_none() {
        let (mut heap, path) = temp_heap("sdm_edge");
        let schema = user_schema();
        for i in 0..50 {
            let row = vec![Value::Str(format!("u{i}")), Value::Int(i)];
            heap.insert(&encode_row(&schema, &row)).unwrap();
        }
        // Predicate never matches — zero deletions, scan count unchanged.
        let c = heap.scan_delete_matching(|_| false, |_rid, _| {}).unwrap();
        assert_eq!(c, 0);
        assert_eq!(
            heap.scan()
                .collect::<std::io::Result<Vec<_>>>()
                .unwrap()
                .len(),
            50
        );

        // Predicate always matches — everything gone.
        let c = heap.scan_delete_matching(|_| true, |_rid, _| {}).unwrap();
        assert_eq!(c, 50);
        assert_eq!(
            heap.scan()
                .collect::<std::io::Result<Vec<_>>>()
                .unwrap()
                .len(),
            0
        );
        drop(heap);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_inserts_with_mmap_enabled_all_readable() {
        // Regression test for WS1: `insert` previously called
        // `disable_mmap()` unconditionally on every row, even on the
        // hot-page fast path where the file never grows. The fix only
        // tears down the mmap when a new page is actually allocated. This
        // test guards the correctness invariant: with mmap enabled,
        // interleaving reads and writes across page boundaries must never
        // observe stale/short mappings.
        let (mut heap, path) = temp_heap("mmap_inserts");
        let schema = user_schema();

        // Seed a few pages, then enable the persistent mmap so the file is
        // mapped at a known length.
        let mut rids = Vec::new();
        for i in 0..200 {
            let row = vec![Value::Str(format!("seed_{i:04}")), Value::Int(i)];
            rids.push((i, heap.insert(&encode_row(&schema, &row)).unwrap()));
        }
        heap.enable_mmap();

        // Now insert many more rows *with the mmap active*. These will grow
        // the file past the mapped length, which must invalidate the mmap
        // exactly when (and only when) a new page is allocated.
        for i in 200..2000 {
            let row = vec![Value::Str(format!("seed_{i:04}")), Value::Int(i)];
            rids.push((i, heap.insert(&encode_row(&schema, &row)).unwrap()));
        }

        // Every row — both pre- and post-mmap — must be readable with the
        // correct contents.
        for (i, rid) in &rids {
            let data = heap.get(*rid).unwrap_or_else(|| panic!("row {i} missing"));
            let decoded = decode_row(&schema, &data);
            assert_eq!(decoded[0], Value::Str(format!("seed_{i:04}")));
            assert_eq!(decoded[1], Value::Int(*i));
        }

        // A full scan must also see exactly the rows we inserted.
        assert_eq!(
            heap.scan()
                .collect::<std::io::Result<Vec<_>>>()
                .unwrap()
                .len(),
            2000
        );

        drop(heap);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_oversized_insert_returns_error_and_heap_survives() {
        let (mut heap, path) = temp_heap("oversized_insert");
        let schema = user_schema();

        // A row larger than any empty page can hold must be a clean error,
        // not a panic (panic = "abort" kills the whole server process).
        let big = vec![0xABu8; PAGE_SIZE];
        let err = heap.insert(&big).unwrap_err();
        assert!(
            err.to_string().contains("row too large"),
            "unexpected error: {err}"
        );

        // The heap must remain fully usable afterwards.
        let rid = heap
            .insert(&encode_row(
                &schema,
                &[Value::Str("ok".into()), Value::Int(1)],
            ))
            .unwrap();
        assert!(heap.get(rid).is_some());
        assert_eq!(
            heap.scan()
                .collect::<std::io::Result<Vec<_>>>()
                .unwrap()
                .len(),
            1
        );
        drop(heap);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_insert_at_max_row_size_succeeds() {
        use crate::page::MAX_ROW_DATA_SIZE;
        let (mut heap, path) = temp_heap("max_row");
        // Exactly the max must still fit on a fresh page.
        let exact = vec![0x42u8; MAX_ROW_DATA_SIZE];
        let rid = heap.insert(&exact).unwrap();
        assert_eq!(heap.get(rid).unwrap().len(), MAX_ROW_DATA_SIZE);
        // One byte more must be rejected.
        let over = vec![0x42u8; MAX_ROW_DATA_SIZE + 1];
        let err = heap.insert(&over).unwrap_err();
        assert!(
            err.to_string().contains("row too large"),
            "unexpected error: {err}"
        );
        drop(heap);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_oversized_update_returns_error_and_row_intact() {
        let (mut heap, path) = temp_heap("oversized_update");
        let schema = user_schema();
        let rid = heap
            .insert(&encode_row(
                &schema,
                &[Value::Str("Alice".into()), Value::Int(30)],
            ))
            .unwrap();
        let old_bytes = heap.get(rid).unwrap();

        // Updating to an oversized row must fail cleanly WITHOUT deleting
        // the old row (the delete+insert fallback must not fire).
        let big = vec![0xCDu8; PAGE_SIZE];
        let err = heap.update(rid, &big).unwrap_err();
        assert!(
            err.to_string().contains("row too large"),
            "unexpected error: {err}"
        );
        assert_eq!(heap.get(rid).unwrap(), old_bytes, "old row must survive");
        assert_eq!(
            heap.scan()
                .collect::<std::io::Result<Vec<_>>>()
                .unwrap()
                .len(),
            1
        );
        drop(heap);
        std::fs::remove_file(&path).ok();
    }

    fn write_chain(heap: &mut HeapFile, value: &[u8]) -> crate::row::OverflowStub {
        use crate::page::{OVERFLOW_CHAIN_END, OVERFLOW_PAYLOAD_CAP};
        let n = value.len().div_ceil(OVERFLOW_PAYLOAD_CAP).max(1);
        let mut pages = Vec::new();
        for _ in 0..n {
            pages.push(heap.allocate_overflow_page().unwrap());
        }
        for i in 0..n {
            let start = i * OVERFLOW_PAYLOAD_CAP;
            let end = (start + OVERFLOW_PAYLOAD_CAP).min(value.len());
            let next = if i + 1 < n {
                pages[i + 1]
            } else {
                OVERFLOW_CHAIN_END
            };
            heap.write_overflow_page(pages[i], next, &value[start..end], 0)
                .unwrap();
        }
        crate::row::OverflowStub::new(value.len() as u64, pages[0], crc32fast::hash(value))
    }

    #[test]
    fn test_overflow_chain_roundtrip_and_v3_bump() {
        let (mut heap, path) = temp_heap("ovf_chain");
        assert_eq!(heap.format_version(), HEAP_FORMAT_VERSION); // v2 before any chain
        let value = vec![0x7Eu8; 10_000]; // spans 3 chunks
        let stub = write_chain(&mut heap, &value);
        assert_eq!(
            heap.format_version(),
            HEAP_FORMAT_VERSION_WITH_OVERFLOW,
            "first chain write must lazily bump the heap to v3"
        );
        let got = heap.read_overflow_value(&stub).unwrap();
        assert_eq!(got, value);
        drop(heap);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_overflow_crc_and_length_faults_are_typed_errors() {
        let (mut heap, path) = temp_heap("ovf_crc");
        let value = b"the whole out-of-line value".to_vec();
        let mut stub = write_chain(&mut heap, &value);
        // Correct stub reads clean.
        assert_eq!(heap.read_overflow_value(&stub).unwrap(), value);
        // Wrong CRC -> typed OverflowCorrupt.
        stub.value_crc32 ^= 0xFFFF_FFFF;
        assert!(matches!(
            heap.read_overflow_value(&stub),
            Err(crate::error::StorageError::OverflowCorrupt(_))
        ));
        // Wrong length -> typed OverflowCorrupt.
        let mut bad_len = crate::row::OverflowStub::new(
            value.len() as u64 + 100,
            stub.first_page,
            crc32fast::hash(&value),
        );
        bad_len.value_crc32 = crc32fast::hash(&value);
        assert!(matches!(
            heap.read_overflow_value(&bad_len),
            Err(crate::error::StorageError::OverflowCorrupt(_))
        ));
        drop(heap);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_overflow_free_list_reuse() {
        let (mut heap, path) = temp_heap("ovf_reuse");
        let stub = write_chain(&mut heap, &vec![1u8; 9000]); // 3 pages
        let pages = heap.overflow_chain_pages(stub.first_page).unwrap();
        assert_eq!(pages.len(), 3);
        // Free the chain; the next allocations must reuse those exact pages.
        heap.release_overflow_pages(&pages);
        assert_eq!(heap.overflow_free_list_len(), 3);
        let reused: Vec<u32> = (0..3)
            .map(|_| heap.allocate_overflow_page().unwrap())
            .collect();
        // Pop order is LIFO, so reused is the freed set reversed — but as a set
        // they must be exactly the freed pages (no file growth).
        let mut a = reused.clone();
        a.sort_unstable();
        let mut b = pages.clone();
        b.sort_unstable();
        assert_eq!(
            a, b,
            "freed chain pages must be reused, not newly allocated"
        );
        assert_eq!(heap.overflow_free_list_len(), 0);
        drop(heap);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_multi_page_span() {
        let (mut heap, path) = temp_heap("multipage");
        let schema = user_schema();
        // Insert enough rows to span multiple pages
        for i in 0..500 {
            let row = vec![Value::Str(format!("user_{i:04}")), Value::Int(i)];
            heap.insert(&encode_row(&schema, &row)).unwrap();
        }
        assert_eq!(
            heap.scan()
                .collect::<std::io::Result<Vec<_>>>()
                .unwrap()
                .len(),
            500
        );
        drop(heap);
        std::fs::remove_file(&path).ok();
    }

    /// Fill `heap` until the shared budget refuses another buffered page,
    /// returning the error. `None` means the budget never tripped.
    fn fill_until_budget_trips(heap: &mut HeapFile, rows: usize) -> Option<io::Error> {
        let schema = user_schema();
        for i in 0..rows {
            let row = vec![Value::Str(format!("user_{i:06}")), Value::Int(i as i64)];
            if let Err(e) = heap.insert(&encode_row(&schema, &row)) {
                return Some(e);
            }
        }
        None
    }

    #[test]
    fn pinned_budget_refuses_growth_with_a_typed_error() {
        let (mut heap, path) = temp_heap("dirty_budget_pinned");
        let budget = Arc::new(DirtyPageBudget::new(8 * PAGE_SIZE));
        budget.set_rollback_pinned(true);
        heap.set_dirty_budget(Arc::clone(&budget));

        let err = fill_until_budget_trips(&mut heap, 100_000)
            .expect("an 8-page budget must refuse an unbounded pinned transaction");
        assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
        let typed = err
            .get_ref()
            .and_then(|source| source.downcast_ref::<StorageError>());
        assert!(
            matches!(typed, Some(StorageError::TransactionTooLarge { .. })),
            "expected a typed TransactionTooLarge, got: {err}"
        );
        assert!(
            err.to_string().starts_with("cannot "),
            "message must survive server-side sanitization: {err}"
        );
        // The refusal is a bound, not a leak: nothing grew past the budget.
        assert!(budget.charged_pages() <= 8);

        // Releasing the pin (as ROLLBACK does) makes the heap usable again.
        heap.discard_dirty();
        budget.set_rollback_pinned(false);
        assert_eq!(budget.charged_pages(), 0);
        assert!(fill_until_budget_trips(&mut heap, 2_000).is_none());

        drop(heap);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unpinned_budget_spills_instead_of_failing() {
        let (mut heap, path) = temp_heap("dirty_budget_unpinned");
        let budget = Arc::new(DirtyPageBudget::new(8 * PAGE_SIZE));
        heap.set_dirty_budget(Arc::clone(&budget));

        // No explicit transaction pins the buffer, so an over-budget heap
        // writes its pages out rather than refusing the insert. Autocommit
        // statements and WAL replay both depend on this.
        assert!(fill_until_budget_trips(&mut heap, 5_000).is_none());
        assert!(budget.charged_pages() <= 8);
        assert_eq!(
            heap.scan()
                .collect::<std::io::Result<Vec<_>>>()
                .unwrap()
                .len(),
            5_000
        );

        drop(heap);
        std::fs::remove_file(&path).ok();
    }

    /// A4: a write error partway through a flush must not consume the pages it
    /// never wrote. They stay buffered (and charged), so the retry that a
    /// checkpoint or `Drop` performs still has every mutation to write.
    #[test]
    fn failed_flush_keeps_the_unwritten_pages_buffered() {
        let (mut heap, path) = temp_heap("flush_write_error");
        let budget = Arc::new(DirtyPageBudget::default());
        heap.set_dirty_budget(Arc::clone(&budget));
        let schema = user_schema();

        // Enough rows to fill several pages, so a failure on one page leaves
        // others behind it in the buffer.
        for i in 0..2_000 {
            let row = vec![Value::Str(format!("user_{i:06}")), Value::Int(i as i64)];
            heap.insert(&encode_row(&schema, &row)).unwrap();
        }
        let buffered_before = heap.dirty_page_count();
        assert!(
            buffered_before > 2,
            "test needs several buffered pages, got {buffered_before}"
        );
        // Pick a page from the middle of the buffer so the flush has both
        // written pages behind it and unwritten pages ahead of it.
        let doomed = *heap
            .dirty_buffer
            .keys()
            .nth(buffered_before / 2)
            .expect("buffer has that many pages");

        HEAP_WRITE_FAILPOINT.with(|failpoint| failpoint.set(Some(doomed)));
        let err = heap
            .flush_all_dirty()
            .expect_err("the injected write failure must surface");
        assert!(err.to_string().contains("injected write failure"));

        // The failing page and everything after it are still in memory, and
        // still charged: losing the charge would let the buffer grow past the
        // budget by exactly the pages a failing disk keeps rejecting.
        assert!(
            heap.dirty_buffer.contains_key(&doomed),
            "the page whose write failed must stay buffered"
        );
        assert_eq!(budget.charged_pages(), heap.dirty_page_count());

        // The retry (what checkpoint and Drop do) writes everything, and every
        // row survives a reopen.
        heap.flush_all_dirty().unwrap();
        assert_eq!(heap.dirty_page_count(), 0);
        assert_eq!(budget.charged_pages(), 0);
        heap.flush().unwrap();
        drop(heap);

        let reopened = HeapFile::open(&path).unwrap();
        assert_eq!(
            reopened
                .scan()
                .collect::<std::io::Result<Vec<_>>>()
                .unwrap()
                .len(),
            2_000
        );
        drop(reopened);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn budget_accounting_returns_to_zero_after_flush() {
        let (mut heap, path) = temp_heap("dirty_budget_accounting");
        let budget = Arc::new(DirtyPageBudget::default());
        heap.set_dirty_budget(Arc::clone(&budget));

        assert!(fill_until_budget_trips(&mut heap, 2_000).is_none());
        assert!(budget.charged_pages() > 0, "inserts must charge the budget");
        assert_eq!(budget.charged_pages(), heap.dirty_page_count());

        heap.flush_all_dirty().unwrap();
        assert_eq!(budget.charged_pages(), 0);

        drop(heap);
        std::fs::remove_file(&path).ok();
    }
}
