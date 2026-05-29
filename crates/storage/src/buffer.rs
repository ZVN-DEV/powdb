use crate::disk::DiskManager;
use crate::page::{Page, PageType};
use rustc_hash::FxHashMap;
use std::io;
use std::path::Path;

struct Frame {
    page: Page,
    dirty: bool,
    pin_count: u32,
    ref_bit: bool,
}

/// In-memory page cache with clock-sweep eviction.
///
/// Keeps up to `capacity` pages in memory. When full, clock-sweep finds
/// an unpinned frame with a cleared ref bit to evict. Dirty pages are
/// flushed to disk before eviction.
///
/// # WS3 checksum integrity
///
/// Every `write_page` call in this file (`new_page`, eviction in
/// `find_or_evict_frame`, `flush_page`, `flush_all`) calls
/// [`Page::stamp_checksum`] immediately before writing, and `ensure_loaded`
/// reads back through [`Page::from_bytes_verified`]. A page that is corrupt
/// or truncated on disk therefore surfaces a `PageCorrupt` error on read
/// rather than returning garbage or panicking the server. Keep this
/// stamp-on-write / verify-on-read pairing intact: dropping the stamp on any
/// write path would make the verified read reject the page as corrupt
/// (stamped flag + stale CRC).
pub struct BufferPool {
    disk: DiskManager,
    frames: Vec<Option<Frame>>,
    // Mission F: FxHashMap for u32 page-id keys (SipHash on a 4-byte int is
    // pure overhead — Fx is dramatically cheaper).
    page_table: FxHashMap<u32, usize>, // page_id -> frame_index
    capacity: usize,
    clock_hand: usize,
}

impl BufferPool {
    pub fn new(path: &Path, capacity: usize) -> io::Result<Self> {
        let disk = if path.exists() {
            DiskManager::open(path)?
        } else {
            DiskManager::create(path)?
        };
        let frames = (0..capacity).map(|_| None).collect();
        Ok(BufferPool {
            disk,
            frames,
            page_table: FxHashMap::default(),
            capacity,
            clock_hand: 0,
        })
    }

    /// Allocate a new page on disk and load it into the buffer pool.
    pub fn new_page(&mut self, page_type: PageType) -> io::Result<u32> {
        let page_id = self.disk.allocate_page()?;
        let mut page = Page::new(page_id, page_type);
        // Stamp the CRC32 before writing so the page can be read back through
        // the verifying constructor (see `ensure_loaded`). Without this the
        // checksum flag would be set with a stale (zero) CRC and the verified
        // read would reject the page as corrupt.
        page.stamp_checksum();
        let frame_idx = self.find_or_evict_frame()?;
        self.page_table.insert(page_id, frame_idx);
        self.frames[frame_idx] = Some(Frame {
            page,
            dirty: true,
            pin_count: 0,
            ref_bit: true,
        });
        // Write to disk immediately so it exists for read-back
        self.disk.write_page(
            page_id,
            self.frames[frame_idx]
                .as_ref()
                .expect("invariant: frame just assigned Some on the line above")
                .page
                .as_bytes(),
        )?;
        Ok(page_id)
    }

    /// Get an immutable reference to a page, loading from disk if needed.
    pub fn get_page(&mut self, page_id: u32) -> io::Result<&Page> {
        self.ensure_loaded(page_id)?;
        let frame_idx = self.page_table[&page_id];
        let frame = self.frames[frame_idx]
            .as_mut()
            .expect("invariant: ensure_loaded guarantees this frame slot is Some");
        frame.ref_bit = true;
        Ok(&frame.page)
    }

    /// Get a mutable reference to a page, loading from disk if needed.
    pub fn get_page_mut(&mut self, page_id: u32) -> io::Result<&mut Page> {
        self.ensure_loaded(page_id)?;
        let frame_idx = self.page_table[&page_id];
        let frame = self.frames[frame_idx]
            .as_mut()
            .expect("invariant: ensure_loaded guarantees this frame slot is Some");
        frame.ref_bit = true;
        Ok(&mut frame.page)
    }

    /// Mark a page as dirty so it will be flushed on eviction or flush_all.
    pub fn mark_dirty(&mut self, page_id: u32) {
        if let Some(&frame_idx) = self.page_table.get(&page_id) {
            if let Some(frame) = &mut self.frames[frame_idx] {
                frame.dirty = true;
            }
        }
    }

    /// Pin a page to prevent eviction.
    pub fn pin(&mut self, page_id: u32) {
        if let Some(&frame_idx) = self.page_table.get(&page_id) {
            if let Some(frame) = &mut self.frames[frame_idx] {
                frame.pin_count += 1;
            }
        }
    }

    /// Unpin a page, allowing eviction.
    pub fn unpin(&mut self, page_id: u32) {
        if let Some(&frame_idx) = self.page_table.get(&page_id) {
            if let Some(frame) = &mut self.frames[frame_idx] {
                frame.pin_count = frame.pin_count.saturating_sub(1);
            }
        }
    }

    fn ensure_loaded(&mut self, page_id: u32) -> io::Result<()> {
        if self.page_table.contains_key(&page_id) {
            return Ok(());
        }
        // Load from disk. Use the checksum-verifying constructor so a corrupt
        // or truncated page surfaces as PageCorrupt (mapped to an io error)
        // instead of panicking the server on a malformed on-disk page.
        let buf = self.disk.read_page(page_id)?;
        let page = Page::from_bytes_verified(&buf)?;
        let frame_idx = self.find_or_evict_frame()?;
        self.page_table.insert(page_id, frame_idx);
        self.frames[frame_idx] = Some(Frame {
            page,
            dirty: false,
            pin_count: 0,
            ref_bit: true,
        });
        Ok(())
    }

    fn find_or_evict_frame(&mut self) -> io::Result<usize> {
        // Find an empty frame first
        for i in 0..self.capacity {
            if self.frames[i].is_none() {
                return Ok(i);
            }
        }
        // Clock-sweep eviction
        let mut attempts = 0;
        loop {
            let idx = self.clock_hand;
            self.clock_hand = (self.clock_hand + 1) % self.capacity;
            if let Some(frame) = &mut self.frames[idx] {
                if frame.pin_count > 0 {
                    attempts += 1;
                    if attempts > self.capacity * 2 {
                        return Err(io::Error::other("buffer pool full — all pages pinned"));
                    }
                    continue;
                }
                if frame.ref_bit {
                    frame.ref_bit = false;
                    continue;
                }
                // Evict this frame
                if frame.dirty {
                    let page_id = frame.page.page_id();
                    frame.page.stamp_checksum();
                    self.disk.write_page(page_id, frame.page.as_bytes())?;
                }
                let old_page_id = frame.page.page_id();
                self.page_table.remove(&old_page_id);
                self.frames[idx] = None;
                return Ok(idx);
            }
            attempts += 1;
            if attempts > self.capacity * 2 {
                return Err(io::Error::other("buffer pool full"));
            }
        }
    }

    /// Flush a single page to disk.
    pub fn flush_page(&mut self, page_id: u32) -> io::Result<()> {
        if let Some(&frame_idx) = self.page_table.get(&page_id) {
            if let Some(frame) = &mut self.frames[frame_idx] {
                if frame.dirty {
                    frame.page.stamp_checksum();
                    self.disk.write_page(page_id, frame.page.as_bytes())?;
                    frame.dirty = false;
                }
            }
        }
        Ok(())
    }

    /// Flush all dirty pages to disk.
    pub fn flush_all(&mut self) -> io::Result<()> {
        for i in 0..self.capacity {
            if let Some(frame) = &mut self.frames[i] {
                if frame.dirty {
                    let page_id = frame.page.page_id();
                    frame.page.stamp_checksum();
                    self.disk.write_page(page_id, frame.page.as_bytes())?;
                    frame.dirty = false;
                }
            }
        }
        self.disk.flush()?;
        Ok(())
    }

    /// Access the underlying disk manager (for heap/btree that need direct I/O).
    pub fn disk(&self) -> &DiskManager {
        &self.disk
    }

    pub fn disk_mut(&mut self) -> &mut DiskManager {
        &mut self.disk
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PageType;

    fn temp_pool(name: &str, capacity: usize) -> (BufferPool, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("powdb_bp_{name}_{}", std::process::id()));
        let pool = BufferPool::new(&path, capacity).unwrap();
        (pool, path)
    }

    #[test]
    fn test_create_and_fetch_page() {
        let (mut pool, path) = temp_pool("basic", 10);
        let page_id = pool.new_page(PageType::Data).unwrap();
        {
            let page = pool.get_page_mut(page_id).unwrap();
            page.insert(b"buffered");
        }
        pool.mark_dirty(page_id);
        pool.flush_all().unwrap();

        let page = pool.get_page(page_id).unwrap();
        assert_eq!(page.get(0).unwrap(), b"buffered");
        drop(pool);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_corrupt_page_on_read_errors_not_panic() {
        use crate::page::Page;
        use std::os::unix::fs::FileExt;

        let (pool, path) = temp_pool("corrupt_read", 4);

        // Build a CRC-stamped page, corrupt one body byte after the CRC was
        // finalized, and write it straight to disk under a fresh page id.
        let mut page = Page::new(0, PageType::Data);
        page.insert(b"trust me");
        page.stamp_checksum();
        let mut raw = page.as_bytes().to_vec();
        // Flip a byte well past the header so the stored CRC no longer matches.
        let last = raw.len() - 1;
        raw[last] ^= 0xFF;

        // Append it as a new on-disk page beyond what the pool has allocated.
        {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            let offset = pool.disk().num_pages() as u64 * crate::page::PAGE_SIZE as u64;
            f.write_all_at(&raw, offset).unwrap();
        }
        // Re-open the pool so it picks up the new page count.
        drop(pool);
        let mut pool = BufferPool::new(&path, 4).unwrap();
        let corrupt_id = pool.disk().num_pages() - 1;

        // Reading the corrupt page must return Err, never panic the server.
        let result = pool.get_page(corrupt_id);
        assert!(
            result.is_err(),
            "expected an error reading a CRC-corrupt page, got Ok"
        );
        drop(pool);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_eviction_under_pressure() {
        let (mut pool, path) = temp_pool("evict", 4);
        let mut ids = Vec::new();
        for i in 0..8 {
            let pid = pool.new_page(PageType::Data).unwrap();
            {
                let page = pool.get_page_mut(pid).unwrap();
                page.insert(format!("page {i}").as_bytes());
            }
            pool.mark_dirty(pid);
            ids.push(pid);
        }
        pool.flush_all().unwrap();
        // All 8 pages created, but only 4 fit in buffer
        // Accessing old pages should load them from disk
        let page = pool.get_page(ids[0]).unwrap();
        assert_eq!(page.page_id(), ids[0]);
        assert_eq!(page.get(0).unwrap(), b"page 0");
        drop(pool);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_dirty_page_persists_after_eviction() {
        let (mut pool, path) = temp_pool("dirty_evict", 2);
        // Create 2 pages, fill buffer
        let p0 = pool.new_page(PageType::Data).unwrap();
        let _p1 = pool.new_page(PageType::Data).unwrap();
        {
            let page = pool.get_page_mut(p0).unwrap();
            page.insert(b"dirty data");
        }
        pool.mark_dirty(p0);
        // Create a 3rd page — forces eviction of p0 or p1
        let _p2 = pool.new_page(PageType::Data).unwrap();
        // p0 was dirty, should have been flushed during eviction
        // Re-load p0 from disk
        let page = pool.get_page(p0).unwrap();
        assert_eq!(page.get(0).unwrap(), b"dirty data");
        drop(pool);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_pin_prevents_eviction() {
        let (mut pool, path) = temp_pool("pin", 2);
        let p0 = pool.new_page(PageType::Data).unwrap();
        let p1 = pool.new_page(PageType::Data).unwrap();
        pool.pin(p0);
        pool.pin(p1);
        // Both pinned, no room — should fail
        let result = pool.new_page(PageType::Data);
        assert!(result.is_err());
        // Unpin one — should succeed
        pool.unpin(p0);
        let p2 = pool.new_page(PageType::Data);
        assert!(p2.is_ok());
        drop(pool);
        std::fs::remove_file(&path).ok();
    }
}
