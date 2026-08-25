//! A page that fails verification MID-SCAN must surface an error, never a
//! silently partial result set.
//!
//! `HeapFile::open` verifies every page up front, and point reads
//! (`ensure_hot`) verify on read, so both fail closed on corruption. The
//! scan paths did not: `scan()` preads every non-resident page and used to
//! map a failed read or an unparseable page to ZERO ROWS from that page
//! (`.ok() ... .unwrap_or_default()`), and the closure scans' pread
//! fallback used to `continue` past a failed read. On a long-running
//! server, bit rot or an EIO encountered mid-scan produced a silently
//! partial answer on the dominant full-scan path, while the same page hit
//! via a point lookup errored. "Silently missing rows" is the exact defect
//! class this project treats as P0, so scans now hold the same
//! fail-closed contract as `ensure_hot`: `read_page` errors and
//! `Page::from_bytes_verified` refusals surface as `io::Error`
//! (`StorageError::PageCorrupt`), never as a shorter result.
//!
//! The mmap fast paths are deliberately out of scope here: pages were
//! verified at open, an I/O error under mmap raises SIGBUS (loud), and
//! re-verifying every mapped page per scan would forfeit the zero-copy
//! design. See the doc comment on `try_for_each_row`.
//!
//! Corruption is injected AFTER a clean reopen (open verification passes),
//! which models the mid-life window: the page was valid at open and the
//! on-disk bytes changed later.

use powdb_storage::heap::HeapFile;
use powdb_storage::page::PAGE_SIZE;
use powdb_storage::row::encode_row;
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

fn one_col_schema() -> Schema {
    Schema {
        table_name: "t".into(),
        columns: vec![ColumnDef {
            name: "name".into(),
            type_id: TypeId::Str,
            required: true,
            position: 0,
        }],
    }
}

fn tmp_path(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let uniq = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "powdb_scan_corrupt_{name}_{}_{uniq}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

/// Seed enough rows to span several pages, flush, and reopen so the
/// in-memory state (hot page, dirty buffer) holds nothing and every page
/// must come back off the disk.
fn seeded_reopened_heap(path: &Path) -> (HeapFile, usize) {
    let schema = one_col_schema();
    let mut heap = HeapFile::create(path).unwrap();
    let mut rows = 0usize;
    // ~100 bytes per row, 4KB pages: 300 rows comfortably spans 8+ pages.
    for i in 0..300 {
        let payload = format!("row_{i}_{}", "x".repeat(80));
        heap.insert(&encode_row(&schema, &[Value::Str(payload)]))
            .unwrap();
        rows += 1;
    }
    heap.flush().unwrap();
    drop(heap);
    (HeapFile::open(path).unwrap(), rows)
}

fn read_page(path: &Path, page_id: u32) -> Vec<u8> {
    let mut f = std::fs::File::open(path).unwrap();
    f.seek(SeekFrom::Start(page_id as u64 * PAGE_SIZE as u64))
        .unwrap();
    let mut buf = vec![0u8; PAGE_SIZE];
    f.read_exact(&mut buf).unwrap();
    buf
}

fn write_page(path: &Path, page_id: u32, bytes: &[u8]) {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    f.seek(SeekFrom::Start(page_id as u64 * PAGE_SIZE as u64))
        .unwrap();
    f.write_all(bytes).unwrap();
    f.flush().unwrap();
}

/// Flip a data byte and leave the CRC stale: the checksum gate must catch it.
fn corrupt_with_stale_crc(path: &Path, page_id: u32) {
    let mut page = read_page(path, page_id);
    page[64] ^= 0xFF;
    write_page(path, page_id, &page);
}

/// The non-CRC refusal the point-read path also enforces: an unsupported
/// page-format version nibble, with the checksum flag cleared so the CRC
/// gate cannot be what catches it. (A wild slot count on an unchecksummed
/// legacy page is deliberately NOT refused anywhere — that shape is
/// clamped by the arithmetic hardening, see page_corruption.rs — so scans
/// hold exactly the `ensure_hot` standard, not a stricter one.)
fn corrupt_version_nibble_without_crc(path: &Path, page_id: u32) {
    let mut page = read_page(path, page_id);
    page[5] &= !0b0000_0001; // clear checksum flag
    page[5] |= 0b1111_0000; // version nibble -> 15, beyond any shipped version
    write_page(path, page_id, &page);
}

/// Collect a scan into rows, or the first error it surfaces.
fn collect_scan(heap: &HeapFile) -> Result<Vec<Vec<u8>>, std::io::Error> {
    heap.scan()
        .map(|item| item.map(|(_rid, data)| data))
        .collect()
}

#[test]
fn scan_over_a_crc_corrupt_page_errors_instead_of_dropping_its_rows() {
    let path = tmp_path("scan_stale_crc");
    let (heap, rows) = seeded_reopened_heap(&path);
    assert_eq!(
        collect_scan(&heap).unwrap().len(),
        rows,
        "sanity: clean scan"
    );

    corrupt_with_stale_crc(&path, 2);

    let err = collect_scan(&heap).expect_err("a page failing CRC mid-scan must error, not vanish");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("corrupt") || msg.contains("checksum"),
        "error should name the corruption, got: {msg}"
    );
}

#[test]
fn scan_over_an_unsupported_page_version_errors_instead_of_dropping_its_rows() {
    let path = tmp_path("scan_bad_version");
    let (heap, _rows) = seeded_reopened_heap(&path);

    corrupt_version_nibble_without_crc(&path, 2);

    let err = collect_scan(&heap)
        .expect_err("a page with an unsupported format version mid-scan must error, not vanish");
    assert!(
        err.to_string().to_lowercase().contains("version"),
        "error should name the version refusal, got: {err}"
    );
}

#[test]
fn scan_never_returns_a_silently_shorter_result_for_a_bad_page() {
    // The regression this pins by name: the old behavior mapped a bad page
    // to zero rows and kept going, so callers (index rebuild, vacuum
    // snapshots, full-table queries) saw a plausible but shorter result.
    let path = tmp_path("scan_no_partial");
    let (heap, rows) = seeded_reopened_heap(&path);

    corrupt_with_stale_crc(&path, 2);

    let mut seen = 0usize;
    let mut errored = false;
    for item in heap.scan() {
        match item {
            Ok(_) => seen += 1,
            Err(_) => {
                errored = true;
                break;
            }
        }
    }
    assert!(
        errored,
        "scan silently completed with {seen}/{rows} rows over a corrupt page"
    );
}
