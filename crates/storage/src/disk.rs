use crate::page::{Page, PAGE_SIZE};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

// Positioned I/O helpers. Unix uses pread(2)/pwrite(2); Windows uses
// FileExt::seek_read/seek_write with explicit offsets. Both avoid mutating the
// shared file cursor, which is required for concurrent page reads.
#[cfg(unix)]
use std::os::unix::fs::FileExt as _;
#[cfg(windows)]
use std::os::windows::fs::FileExt as _;

#[cfg(unix)]
fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = file.write_at(buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write full page at offset",
            ));
        }
        offset += n as u64;
        buf = &buf[n..];
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = file.seek_write(buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write full page at offset",
            ));
        }
        offset += n as u64;
        buf = &buf[n..];
    }
    Ok(())
}

#[cfg(unix)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = file.read_at(buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "failed to read full page at offset",
            ));
        }
        offset += n as u64;
        let tmp = buf;
        buf = &mut tmp[n..];
    }
    Ok(())
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = file.seek_read(buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "failed to read full page at offset",
            ));
        }
        offset += n as u64;
        let tmp = buf;
        buf = &mut tmp[n..];
    }
    Ok(())
}

/// Page count of a data file, refusing a length that is not a whole number of
/// pages.
///
/// Every writer extends a heap one whole page at a time, so a partial trailing
/// page is damage: a truncation, a torn extend, or a bad copy. The old
/// `len / PAGE_SIZE` rounded it away, which turned `truncate -s 100` on a heap
/// into a table that opened clean and answered zero rows.
fn whole_pages(path: &Path, file: &File) -> io::Result<u32> {
    let len = file.metadata()?.len();
    let remainder = len % PAGE_SIZE as u64;
    if remainder != 0 {
        return Err(crate::error::StorageError::CorruptData(format!(
            "{}: file length {len} is not a whole number of {PAGE_SIZE}-byte pages ({remainder} trailing bytes)",
            path.display()
        ))
        .into());
    }
    Ok((len / PAGE_SIZE as u64) as u32)
}

/// Manages page-level I/O to a single data file.
/// Each page is PAGE_SIZE bytes at offset = page_id * PAGE_SIZE.
pub struct DiskManager {
    file: File,
    num_pages: u32,
    /// True when the file was opened read-only (snapshot serving). Every write
    /// entry point refuses in this mode, so a stray mutation surfaces as a clean
    /// error instead of an OS-level EBADF on a read-only fd.
    read_only: bool,
}

impl DiskManager {
    pub fn create(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(DiskManager {
            file,
            num_pages: 0,
            read_only: false,
        })
    }

    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let num_pages = whole_pages(path, &file)?;
        Ok(DiskManager {
            file,
            num_pages,
            read_only: false,
        })
    }

    /// Open a data file **read-only** for snapshot serving. The file is opened
    /// with `.read(true)` only, so the OS refuses any write to the descriptor;
    /// the write entry points also refuse in software, so a logic error surfaces
    /// as a clear message instead of a raw `EBADF`.
    pub fn open_read_only(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        let num_pages = whole_pages(path, &file)?;
        Ok(DiskManager {
            file,
            num_pages,
            read_only: true,
        })
    }

    /// Whether this file was opened read-only.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    #[cold]
    fn read_only_write_error() -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cannot write: data file was opened read-only for snapshot serving",
        )
    }

    /// Allocate a new page and extend the file. Returns the new page_id.
    pub fn allocate_page(&mut self) -> io::Result<u32> {
        if self.read_only {
            return Err(Self::read_only_write_error());
        }
        let id = self.num_pages;
        let zeros = [0u8; PAGE_SIZE];
        let offset = id as u64 * PAGE_SIZE as u64;
        write_all_at(&self.file, &zeros, offset)?;
        self.num_pages += 1;
        Ok(id)
    }

    pub fn write_page(&mut self, page_id: u32, data: &[u8]) -> io::Result<()> {
        if self.read_only {
            return Err(Self::read_only_write_error());
        }
        debug_assert_eq!(data.len(), PAGE_SIZE);
        let offset = page_id as u64 * PAGE_SIZE as u64;
        // pwrite(2): atomic w.r.t. the kernel file offset, so this is safe
        // on a shared `&File` across threads.
        write_all_at(&self.file, data, offset)?;
        Ok(())
    }

    pub fn read_page(&self, page_id: u32) -> io::Result<[u8; PAGE_SIZE]> {
        let mut buf = [0u8; PAGE_SIZE];
        let offset = page_id as u64 * PAGE_SIZE as u64;
        // pread(2): atomic w.r.t. the kernel file offset per POSIX. Multiple
        // reader threads can call this concurrently through a shared `&self`
        // (and thus a shared `&File`) without interfering with each other.
        // This is the correctness foundation for `unsafe impl Sync for HeapFile`.
        read_exact_at(&self.file, &mut buf, offset)?;
        Ok(buf)
    }

    /// Read `pages.len()` contiguous pages starting at `start_id` in one
    /// positioned read.
    ///
    /// The offset is `start_id * PAGE_SIZE`, the same formula as
    /// [`Self::read_page`]. The slots are contiguous `[u8; PAGE_SIZE]` values,
    /// so the read is one `pages.len() * PAGE_SIZE` byte buffer which is
    /// already split into pages. A short read is `UnexpectedEof`, from the
    /// same helper `read_page` uses.
    fn read_contiguous_into(&self, start_id: u32, pages: &mut [[u8; PAGE_SIZE]]) -> io::Result<()> {
        let offset = start_id as u64 * PAGE_SIZE as u64;
        read_exact_at(&self.file, pages.as_flattened_mut(), offset)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if self.read_only {
            // Nothing was written, so there is nothing to sync. Returning Ok
            // keeps a defensive `flush` on a read-only handle a harmless no-op.
            return Ok(());
        }
        self.file.sync_data()
    }

    pub fn num_pages(&self) -> u32 {
        self.num_pages
    }

    /// Truncate the data file to exactly `pages` pages.
    pub fn truncate_pages(&mut self, pages: u32) -> io::Result<()> {
        if self.read_only {
            return Err(Self::read_only_write_error());
        }
        self.file.set_len(pages as u64 * PAGE_SIZE as u64)?;
        self.num_pages = pages;
        Ok(())
    }

    /// Borrow the underlying file (for mmap-based scans).
    pub fn file_ref(&self) -> &File {
        &self.file
    }
}

/// Read and verify every requested page, preserving request order and duplicates.
///
/// `depth` must be nonzero. Depth one is a serial loop; higher depths use at
/// most `depth` scoped threads sharing the positioned-I/O disk handle. Each
/// thread groups its slice into maximal runs of ids where each id is exactly
/// one greater than the previous. A run longer than 32 pages is two preads:
/// the second read overlaps the checksum of the first half. Every shorter run
/// is one pread, [`DiskManager::read_page`] for a single page and one
/// contiguous read otherwise. A gap, a duplicate, or a decreasing id is not
/// coalesced. This is opt-in and does not change
/// [`DiskManager::read_page`] or heap point reads. Checksums follow
/// [`Page::verify_bytes`]'s validate-if-present policy: the check does not
/// copy the page. Any read, verification, or worker error fails the whole
/// batch. No partial output is returned. Callers must keep the underlying file
/// quiescent if they need the batch to represent one consistent snapshot.
pub fn read_pages_verified(
    disk: &DiskManager,
    page_ids: &[u32],
    depth: usize,
) -> io::Result<Vec<[u8; PAGE_SIZE]>> {
    read_pages_in_chunks(page_ids, depth, &|ids, output| {
        verify_contiguous_runs(disk, ids, output)
    })
}

// The reader seam lets tests count actual calls, including repeated ids, without
// adding counters or branches to DiskManager's one-page path. It stays one call
// per id. Coalescing lives in `verify_contiguous_runs`, not here.
#[cfg(test)]
fn read_pages_with(
    page_ids: &[u32],
    depth: usize,
    read: &(impl Fn(u32) -> io::Result<[u8; PAGE_SIZE]> + Sync),
) -> io::Result<Vec<[u8; PAGE_SIZE]>> {
    read_pages_in_chunks(page_ids, depth, &|ids, output| {
        for (&page_id, slot) in ids.iter().zip(output) {
            *slot = read(page_id)?;
        }
        Ok(())
    })
}

fn read_pages_in_chunks(
    page_ids: &[u32],
    depth: usize,
    fill: &(impl Fn(&[u32], &mut [[u8; PAGE_SIZE]]) -> io::Result<()> + Sync),
) -> io::Result<Vec<[u8; PAGE_SIZE]>> {
    if depth == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "page read depth must be nonzero",
        ));
    }
    if page_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut pages = vec![[0; PAGE_SIZE]; page_ids.len()];
    if depth == 1 {
        fill(page_ids, &mut pages)?;
        return Ok(pages);
    }
    let chunk_size = page_ids.len().div_ceil(depth.min(page_ids.len()));
    std::thread::scope(|scope| {
        let mut joins = Vec::new();
        for (ids, output) in page_ids
            .chunks(chunk_size)
            .zip(pages.chunks_mut(chunk_size))
        {
            joins.push(scope.spawn(move || -> io::Result<()> { fill(ids, output) }));
        }
        // Join every worker even on error so no unjoined panic escapes scope.
        let mut result = Ok(());
        for join in joins {
            let worker = join
                .join()
                .map_err(|_| io::Error::other("page reader thread panicked"))
                .and_then(|result| result);
            if result.is_ok() {
                result = worker;
            }
        }
        result
    })?;
    Ok(pages)
}

// 32 pages is 128 KiB: one positioned read. A longer run is exactly two reads,
// not a window loop. The second pread overlaps the first half's checksum.
const ONE_TRANSFER_PAGES: usize = 32;

fn verify_page_bytes(pages: &[[u8; PAGE_SIZE]]) -> io::Result<()> {
    for page in pages {
        Page::verify_bytes(page).map_err(io::Error::from)?;
    }
    Ok(())
}

// Two preads, both halves nonempty (`mid = len / 2`). The caller reads the
// first half, then one scoped helper reads the second half while the caller
// checksums the first. Join before touching the second half. `checked_add`
// is a backstop: `chunk_by` already refuses a wrapping run.
fn verify_long_run(
    disk: &DiskManager,
    start_id: u32,
    slots: &mut [[u8; PAGE_SIZE]],
) -> io::Result<()> {
    let mid = slots.len() / 2;
    debug_assert!(mid > 0 && mid < slots.len());
    let (first, second) = slots.split_at_mut(mid);
    disk.read_contiguous_into(start_id, first)?;
    let second_start = u32::try_from(mid)
        .ok()
        .and_then(|half| start_id.checked_add(half))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "contiguous page run overflows a page id",
            )
        })?;
    std::thread::scope(|scope| {
        let reader =
            scope.spawn(|| -> io::Result<()> { disk.read_contiguous_into(second_start, second) });
        let verified = verify_page_bytes(first);
        let read = reader
            .join()
            .map_err(|_| io::Error::other("page reader thread panicked"))
            .and_then(|result| result);
        verified?;
        read
    })?;
    verify_page_bytes(second)
}

// Maximal runs: each id is exactly one greater than the previous. A gap, a
// duplicate, or a decrease starts a new run. `checked_add` refuses a wrap
// from `u32::MAX` to 0, which is not contiguous on disk.
fn verify_contiguous_runs(
    disk: &DiskManager,
    ids: &[u32],
    output: &mut [[u8; PAGE_SIZE]],
) -> io::Result<()> {
    debug_assert_eq!(ids.len(), output.len());
    let mut filled = 0;
    for run in ids.chunk_by(|prev, next| prev.checked_add(1) == Some(*next)) {
        debug_assert!(!run.is_empty());
        let start_id = run[0];
        let slots = &mut output[filled..filled + run.len()];
        filled += run.len();
        if run.len() == 1 {
            let bytes = disk.read_page(start_id)?;
            Page::verify_bytes(&bytes).map_err(io::Error::from)?;
            slots[0] = bytes;
        } else if run.len() <= ONE_TRANSFER_PAGES {
            disk.read_contiguous_into(start_id, slots)?;
            verify_page_bytes(slots)?;
        } else {
            verify_long_run(disk, start_id, slots)?;
        }
    }
    debug_assert_eq!(filled, ids.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{Page, PageType};
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("powdb_test_{name}_{}", std::process::id()))
    }

    fn batch_fixture_n(pages: u32) -> (tempfile::TempDir, DiskManager) {
        let dir = tempfile::tempdir().unwrap();
        let mut disk = DiskManager::create(&dir.path().join("batch.heap")).unwrap();
        for _ in 0..pages {
            let id = disk.allocate_page().unwrap();
            let mut page = Page::new(id, PageType::Data);
            page.insert(&id.to_le_bytes()).unwrap();
            page.stamp_checksum();
            disk.write_page(id, page.as_bytes()).unwrap();
        }
        (dir, disk)
    }

    fn batch_fixture() -> (tempfile::TempDir, DiskManager) {
        batch_fixture_n(9)
    }

    fn flip_page_byte(disk: &mut DiskManager, page_id: u32) {
        let mut bytes = disk.read_page(page_id).unwrap();
        bytes[100] ^= 1;
        disk.write_page(page_id, &bytes).unwrap();
    }

    #[test]
    fn batch_preserves_serial_order_at_depth_one_and_four() {
        let (_dir, disk) = batch_fixture();
        let ids = [8, 0, 5, 2, 1, 7, 3, 6, 4, 0, 8];
        let expected: Vec<_> = ids.iter().map(|&id| disk.read_page(id).unwrap()).collect();
        for depth in [1, 4, 100] {
            assert_eq!(read_pages_verified(&disk, &ids, depth).unwrap(), expected);
            assert!(read_pages_verified(&disk, &[], depth).unwrap().is_empty());
        }
        assert_eq!(
            read_pages_verified(&disk, &ids, 0).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn batch_contiguous_and_gapped_match_serial_reads() {
        let (_dir, disk) = batch_fixture();
        let contiguous = [0, 1, 2, 3, 4, 5, 6, 7, 8];
        // Gap before 4, duplicate 5, then a decreasing id. None of those join.
        let gapped = [0, 1, 2, 4, 5, 5, 8, 3];
        for ids in [&contiguous[..], &gapped[..]] {
            let expected: Vec<_> = ids.iter().map(|&id| disk.read_page(id).unwrap()).collect();
            for depth in [1, 4] {
                assert_eq!(read_pages_verified(&disk, ids, depth).unwrap(), expected);
            }
        }
    }

    #[test]
    fn batch_refuses_corrupt_or_missing_pages() {
        let (_dir, mut disk) = batch_fixture();
        let mut bytes = disk.read_page(2).unwrap();
        bytes[100] ^= 1;
        disk.write_page(2, &bytes).unwrap();
        for depth in [1, 4] {
            assert!(read_pages_verified(&disk, &[0, 2, 1], depth).is_err());
            // Page 2 sits in the middle of one contiguous run, not a one-page read.
            assert!(read_pages_verified(&disk, &[0, 1, 2, 3], depth).is_err());
            assert_eq!(
                read_pages_verified(&disk, &[0, 99, 1], depth)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
    }

    #[test]
    fn batch_long_run_matches_serial_reads() {
        // 40 pages is past ONE_TRANSFER_PAGES (32), so depth 1 is two preads.
        let (_dir, disk) = batch_fixture_n(40);
        let ids: Vec<u32> = (0..40).collect();
        let expected: Vec<_> = ids.iter().map(|&id| disk.read_page(id).unwrap()).collect();
        for depth in [1, 4] {
            assert_eq!(read_pages_verified(&disk, &ids, depth).unwrap(), expected);
        }
    }

    #[test]
    fn batch_long_run_refuses_corrupt_first_half() {
        // Page 3 sits in the first half of a 40-page run (mid = 20).
        let (_dir, mut disk) = batch_fixture_n(40);
        flip_page_byte(&mut disk, 3);
        let ids: Vec<u32> = (0..40).collect();
        for depth in [1, 4] {
            assert!(read_pages_verified(&disk, &ids, depth).is_err());
        }
    }

    #[test]
    fn batch_long_run_refuses_corrupt_second_half() {
        // Page 30 sits in the second half of a 40-page run (mid = 20).
        let (_dir, mut disk) = batch_fixture_n(40);
        flip_page_byte(&mut disk, 30);
        let ids: Vec<u32> = (0..40).collect();
        for depth in [1, 4] {
            assert!(read_pages_verified(&disk, &ids, depth).is_err());
        }
    }

    #[test]
    fn batch_reads_duplicate_ids_for_every_request() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (_dir, disk) = batch_fixture();
        let ids = [2, 2, 0, 2, 0, 2, 0];
        for depth in [1, 4] {
            let calls: [_; 9] = std::array::from_fn(|_| AtomicUsize::new(0));
            let pages = read_pages_with(&ids, depth, &|id| {
                calls[id as usize].fetch_add(1, Ordering::Relaxed);
                let bytes = disk.read_page(id)?;
                Page::from_bytes_verified(&bytes).map_err(io::Error::from)?;
                Ok(bytes)
            })
            .unwrap();
            assert_eq!(pages.len(), ids.len());
            assert_eq!(calls[2].load(Ordering::Relaxed), 4);
            assert_eq!(calls[0].load(Ordering::Relaxed), 3);
        }
    }

    #[test]
    fn test_create_and_read_page() {
        let path = temp_path("disk_basic");
        let mut dm = DiskManager::create(&path).unwrap();
        let page_id = dm.allocate_page().unwrap();
        assert_eq!(page_id, 0);

        let mut page = Page::new(page_id, PageType::Data);
        page.insert(b"hello disk");
        dm.write_page(page_id, page.as_bytes()).unwrap();

        let buf = dm.read_page(page_id).unwrap();
        let loaded = Page::from_bytes(&buf).unwrap();
        assert_eq!(loaded.get(0).unwrap(), b"hello disk");

        drop(dm);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_allocate_multiple_pages() {
        let path = temp_path("disk_multi");
        let mut dm = DiskManager::create(&path).unwrap();
        let p0 = dm.allocate_page().unwrap();
        let p1 = dm.allocate_page().unwrap();
        let p2 = dm.allocate_page().unwrap();
        assert_eq!(p0, 0);
        assert_eq!(p1, 1);
        assert_eq!(p2, 2);
        assert_eq!(dm.num_pages(), 3);

        drop(dm);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_reopen_file() {
        let path = temp_path("disk_reopen");
        {
            let mut dm = DiskManager::create(&path).unwrap();
            let id = dm.allocate_page().unwrap();
            let mut page = Page::new(id, PageType::Data);
            page.insert(b"persistent");
            dm.write_page(id, page.as_bytes()).unwrap();
            dm.flush().unwrap();
        }
        {
            let dm = DiskManager::open(&path).unwrap();
            assert_eq!(dm.num_pages(), 1);
            let buf = dm.read_page(0).unwrap();
            let page = Page::from_bytes(&buf).unwrap();
            assert_eq!(page.get(0).unwrap(), b"persistent");
        }
        std::fs::remove_file(&path).ok();
    }
}
