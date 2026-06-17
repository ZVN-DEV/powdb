use crate::page::PAGE_SIZE;
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

/// Manages page-level I/O to a single data file.
/// Each page is PAGE_SIZE bytes at offset = page_id * PAGE_SIZE.
pub struct DiskManager {
    file: File,
    num_pages: u32,
}

impl DiskManager {
    pub fn create(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(DiskManager { file, num_pages: 0 })
    }

    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let len = file.metadata()?.len();
        let num_pages = (len / PAGE_SIZE as u64) as u32;
        Ok(DiskManager { file, num_pages })
    }

    /// Allocate a new page and extend the file. Returns the new page_id.
    pub fn allocate_page(&mut self) -> io::Result<u32> {
        let id = self.num_pages;
        let zeros = [0u8; PAGE_SIZE];
        let offset = id as u64 * PAGE_SIZE as u64;
        write_all_at(&self.file, &zeros, offset)?;
        self.num_pages += 1;
        Ok(id)
    }

    pub fn write_page(&mut self, page_id: u32, data: &[u8]) -> io::Result<()> {
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

    pub fn flush(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }

    pub fn num_pages(&self) -> u32 {
        self.num_pages
    }

    /// Borrow the underlying file (for mmap-based scans).
    pub fn file_ref(&self) -> &File {
        &self.file
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{Page, PageType};
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("powdb_test_{name}_{}", std::process::id()))
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
