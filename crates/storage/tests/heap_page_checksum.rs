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

    // Corrupt one byte deep in page 0's data region, directly on disk,
    // bypassing the heap entirely. Offset 40 is past the 20-byte header and
    // CRC field but well before the bottom slot directory.
    {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let corrupt_at = (rid.page_id as u64) * PAGE_SIZE as u64 + 40;
        f.seek(SeekFrom::Start(corrupt_at)).unwrap();
        let mut byte = [0u8; 1];
        f.read_exact(&mut byte).unwrap();
        byte[0] ^= 0xFF;
        f.seek(SeekFrom::Start(corrupt_at)).unwrap();
        f.write_all(&byte).unwrap();
        f.flush().unwrap();
    }

    // Reopen and force a disk read of the corrupted page. `delete` routes
    // through `ensure_hot`, which reads + verifies the page from disk.
    let mut heap = HeapFile::open(&path).unwrap();
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
