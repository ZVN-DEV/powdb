//! WS3: end-to-end heap page checksum tests.
//!
//! Verifies the full write-back -> on-disk-corruption -> verified-read path:
//! a row is inserted and flushed (stamping the CRC32), a byte in the page's
//! data region is flipped directly on disk, and the next read surfaces a
//! `StorageError::PageCorrupt` instead of silently returning garbage.

use powdb_storage::error::StorageError;
use powdb_storage::heap::HeapFile;
use powdb_storage::page::PAGE_SIZE;
use powdb_storage::row::encode_row;
use powdb_storage::types::{ColumnDef, RowId, Schema, TypeId, Value};
use std::io::{Read, Seek, SeekFrom, Write};

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

fn tmp_path(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("powdb_heap_crc_{name}_{}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn test_disk_corruption_detected_on_read() {
    let path = tmp_path("corrupt");
    let schema = one_col_schema();

    // Insert a row, flush to stamp the CRC, and close the heap so the file
    // holds a fully-written, checksummed page.
    let rid: RowId;
    {
        let mut heap = HeapFile::create(&path).unwrap();
        rid = heap
            .insert(&encode_row(&schema, &[Value::Str("important_data".into())]))
            .unwrap();
        heap.flush().unwrap();
        // drop flushes again, but the page is already stamped.
    }

    // Force a disk read of the corrupted page. `delete` routes through
    // `ensure_hot`, which reads + verifies the page from disk.
    //
    // The open path itself now verifies page CRCs, so the heap is
    // opened BEFORE the corruption is written (see `open_then_corrupt` below)
    // to keep this test focused on the read path rather than the open path.
    let mut heap = open_then_corrupt(&path, rid.page_id);
    let err = heap
        .delete(rid)
        .expect_err("reading a corrupted page must error");
    // The io::Error wraps StorageError::PageCorrupt's message.
    let msg = err.to_string();
    assert!(
        msg.contains("CRC32 mismatch") || msg.contains("page corrupt"),
        "expected a page-corruption error, got: {msg}"
    );

    drop(heap);
    let _ = std::fs::remove_file(&path);
}

/// Open the heap, then corrupt one byte of `page_id` on disk. The open must
/// happen first: the open path verifies every page CRC, so a file
/// corrupted beforehand no longer opens at all.
fn open_then_corrupt(path: &std::path::Path, page_id: u32) -> HeapFile {
    let heap = HeapFile::open(path).unwrap();
    corrupt_page_data_byte(path, page_id);
    heap
}

/// Flip one byte of page 0's data region directly on disk, bypassing the
/// heap. Returns nothing; panics on I/O failure.
fn corrupt_page_data_byte(path: &std::path::Path, page_id: u32) {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let corrupt_at = (page_id as u64) * PAGE_SIZE as u64 + 40;
    f.seek(SeekFrom::Start(corrupt_at)).unwrap();
    let mut byte = [0u8; 1];
    f.read_exact(&mut byte).unwrap();
    byte[0] ^= 0xFF;
    f.seek(SeekFrom::Start(corrupt_at)).unwrap();
    f.write_all(&byte).unwrap();
    f.flush().unwrap();
}

/// Proves the *gap*: when the mmap fast path is active, the zero-copy scan
/// reads the corrupted page bytes WITHOUT a CRC check, so on-disk bit-rot
/// goes UNDETECTED. This documents the deliberate read-path tradeoff — the
/// scan does not error, it just yields the garbled row.
///
/// NOTE: MAP_PRIVATE on macOS/Linux maps the on-disk contents at the time of
/// first access; this test corrupts the file *before* `enable_mmap()` so the
/// mapping observes the corrupted bytes deterministically.
#[test]
fn test_mmap_scan_does_not_detect_corruption_by_design() {
    let path = tmp_path("mmap_gap");
    let schema = one_col_schema();

    let rid;
    {
        let mut heap = HeapFile::create(&path).unwrap();
        rid = heap
            .insert(&encode_row(&schema, &[Value::Str("important_data".into())]))
            .unwrap();
        heap.flush().unwrap();
    }

    // Open first (the open path verifies CRCs), corrupt on disk, then
    // activate the mmap fast path so the mapping observes the corrupt bytes.
    let mut heap = open_then_corrupt(&path, rid.page_id);
    heap.enable_mmap();

    // The zero-copy scan reads through the mmap with no per-read CRC check.
    // It must NOT error — it silently yields the (corrupted) row. This is
    // the documented performance tradeoff, asserted here so the gap is
    // explicit rather than hidden.
    let mut rows_seen = 0usize;
    heap.try_for_each_row(|_rid, _data| {
        rows_seen += 1;
        std::ops::ControlFlow::Continue(())
    });
    assert_eq!(
        rows_seen, 1,
        "mmap scan yields the row without detecting corruption (by design)"
    );

    // ...but an explicit integrity scan DOES catch it, even with mmap active.
    let err = heap
        .verify_integrity()
        .expect_err("verify_integrity must detect on-disk corruption");
    assert!(
        matches!(err, StorageError::PageCorrupt(_)),
        "expected PageCorrupt, got: {err:?}"
    );

    drop(heap);
    let _ = std::fs::remove_file(&path);
}

/// `verify_integrity()` detects a corrupted page whether or not an mmap is
/// active, and returns `Ok(())` for a clean file.
#[test]
fn test_verify_integrity_detects_corruption() {
    let path = tmp_path("verify");
    let schema = one_col_schema();

    let rid;
    {
        let mut heap = HeapFile::create(&path).unwrap();
        rid = heap
            .insert(&encode_row(&schema, &[Value::Str("important_data".into())]))
            .unwrap();
        heap.flush().unwrap();
    }

    // Clean file: no mmap.
    {
        let heap = HeapFile::open(&path).unwrap();
        heap.verify_integrity()
            .expect("clean file must pass integrity check");
    }

    // Corrupt and detect, mmap NOT active.
    {
        let heap = open_then_corrupt(&path, rid.page_id);
        let err = heap
            .verify_integrity()
            .expect_err("corrupted page must fail integrity check (no mmap)");
        assert!(matches!(err, StorageError::PageCorrupt(_)));
    }

    // Corrupt and detect, mmap ACTIVE — verify_integrity reads off disk, not
    // the mmap snapshot, so it still catches it. A fresh file is used because
    // the one above is already corrupt and no longer opens.
    let mmap_path = tmp_path("verify_mmap");
    let mmap_rid;
    {
        let mut heap = HeapFile::create(&mmap_path).unwrap();
        mmap_rid = heap
            .insert(&encode_row(&schema, &[Value::Str("important_data".into())]))
            .unwrap();
        heap.flush().unwrap();
    }
    {
        let mut heap = open_then_corrupt(&mmap_path, mmap_rid.page_id);
        heap.enable_mmap();
        let err = heap
            .verify_integrity()
            .expect_err("corrupted page must fail integrity check (mmap active)");
        assert!(matches!(err, StorageError::PageCorrupt(_)));
    }

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&mmap_path);
}

#[test]
fn test_clean_page_reads_back_after_flush() {
    // Sanity: a stamped page round-trips through disk without false
    // positives.
    let path = tmp_path("clean");
    let schema = one_col_schema();

    let rid;
    {
        let mut heap = HeapFile::create(&path).unwrap();
        rid = heap
            .insert(&encode_row(&schema, &[Value::Str("hello".into())]))
            .unwrap();
        heap.flush().unwrap();
    }

    let heap = HeapFile::open(&path).unwrap();
    let data = heap.get(rid).expect("clean page row must read back");
    assert_eq!(
        powdb_storage::row::decode_row(&schema, &data)[0],
        Value::Str("hello".into())
    );
    drop(heap);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_legacy_unstamped_page_opens() {
    // A page whose checksum flag is clear (pre-WS3 format) must open and
    // read without verification. We forge such a page by writing raw bytes
    // with the flag byte cleared.
    let path = tmp_path("legacy");
    let schema = one_col_schema();

    let rid;
    {
        let mut heap = HeapFile::create(&path).unwrap();
        rid = heap
            .insert(&encode_row(&schema, &[Value::Str("legacy".into())]))
            .unwrap();
        heap.flush().unwrap();
    }

    // Clear the checksum flag (byte 5, bit 0) on page 0 directly on disk to
    // mimic a file written before checksums existed.
    {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let flag_at = (rid.page_id as u64) * PAGE_SIZE as u64 + 5;
        f.seek(SeekFrom::Start(flag_at)).unwrap();
        let mut byte = [0u8; 1];
        f.read_exact(&mut byte).unwrap();
        byte[0] &= !0b0000_0001; // clear FLAG_HAS_CHECKSUM
        f.seek(SeekFrom::Start(flag_at)).unwrap();
        f.write_all(&byte).unwrap();
        f.flush().unwrap();
    }

    // Even though the stored CRC bytes no longer match (flag is clear), the
    // page must read without a PageCorrupt error.
    let heap = HeapFile::open(&path).unwrap();
    match heap.get(rid) {
        Some(data) => assert_eq!(
            powdb_storage::row::decode_row(&schema, &data)[0],
            Value::Str("legacy".into())
        ),
        None => panic!("legacy page row should read back"),
    }
    drop(heap);
    let _ = std::fs::remove_file(&path);

    // Avoid an unused-import warning for StorageError if the corruption
    // test is ever cfg'd out.
    let _ = std::mem::size_of::<StorageError>();
}
