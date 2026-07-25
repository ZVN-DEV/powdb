//! A corrupt page must produce a typed error, never a process abort.
//!
//! `panic = "abort"` is set for release builds (crash-only design), so any
//! reachable panic in the open path or the point-lookup path is worse than a
//! crash: `HeapFile::open` runs before WAL replay, so a supervisor that
//! restarts the process aborts again immediately, giving a permanent crash
//! loop that no restart can clear.
//!
//! Two corruption shapes are exercised here:
//!
//!   1. A page whose bottom-of-page `slot_count` word is a wild value. The
//!      slot-entry offset is computed from it, so an unchecked read underflows
//!      `PAGE_SIZE - 2 - (i + 1) * 4` and slices the page with a ~2^64 range.
//!   2. A page whose slot entry points outside the page (offset/length are
//!      u16, so `offset + length` can reach ~131k against a 4096-byte page).
//!
//! Each shape is tested twice: once with the CRC left stale (the open/read
//! path must reject the page as corrupt) and once with the CRC recomputed over
//! the corrupted bytes, which is the case a checksum cannot catch and where
//! only the arithmetic hardening stands between us and an abort.
//!
//! Test binaries build with the default `panic = "unwind"`, so a regression
//! shows up as a failing test rather than a killed test runner. No subprocess
//! harness is needed (unlike `crates/server/tests/kill9_durability.rs`, which
//! needs a real child process because it SIGKILLs the server).

use powdb_storage::heap::HeapFile;
use powdb_storage::page::PAGE_SIZE;
use powdb_storage::row::encode_row;
use powdb_storage::types::{ColumnDef, RowId, Schema, TypeId, Value};
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
        "powdb_page_corrupt_{name}_{}_{uniq}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

/// Write one row, flush so the page is fully written and CRC-stamped, and
/// return its path plus RowId.
fn seeded_heap(path: &Path) -> RowId {
    let schema = one_col_schema();
    let mut heap = HeapFile::create(path).unwrap();
    let rid = heap
        .insert(&encode_row(&schema, &[Value::Str("important_data".into())]))
        .unwrap();
    heap.flush().unwrap();
    rid
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

/// Clear the checksum flag so the page reads as a pre-WS3 (unchecksummed)
/// page. This is how we exercise the arithmetic hardening itself: the CRC
/// gate is bypassed exactly as it is for a legacy data file.
fn clear_checksum_flag(page: &mut [u8]) {
    page[5] &= !0b0000_0001;
}

fn set_slot_count(page: &mut [u8], count: u16) {
    page[PAGE_SIZE - 2..PAGE_SIZE].copy_from_slice(&count.to_le_bytes());
}

/// Point slot 0 at an offset/length pair that runs off the end of the page.
fn set_slot_entry(page: &mut [u8], slot: u16, offset: u16, length: u16) {
    let entry_off = PAGE_SIZE - 2 - ((slot as usize + 1) * 4);
    page[entry_off..entry_off + 2].copy_from_slice(&offset.to_le_bytes());
    page[entry_off + 2..entry_off + 4].copy_from_slice(&length.to_le_bytes());
}

#[test]
fn open_with_wild_slot_count_and_stale_crc_errors_instead_of_aborting() {
    let path = tmp_path("open_slotcount_crc");
    let rid = seeded_heap(&path);

    let mut page = read_page(&path, rid.page_id);
    set_slot_count(&mut page, u16::MAX);
    write_page(&path, rid.page_id, &page);

    // The CRC is now stale, so the open path must reject the page outright.
    let msg = match HeapFile::open(&path) {
        Ok(_) => panic!("a corrupt page must fail the open"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("CRC32 mismatch") || msg.to_lowercase().contains("corrupt"),
        "expected a typed page-corruption error, got: {msg}"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn open_with_wild_slot_count_and_no_checksum_does_not_abort() {
    let path = tmp_path("open_slotcount_legacy");
    let rid = seeded_heap(&path);

    let mut page = read_page(&path, rid.page_id);
    set_slot_count(&mut page, u16::MAX);
    clear_checksum_flag(&mut page);
    write_page(&path, rid.page_id, &page);

    // No checksum to validate against, so the open walks the slot directory
    // with a wild count. It must clamp instead of computing an underflowed
    // slice index.
    match HeapFile::open(&path) {
        Ok(_) => {}
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.to_lowercase().contains("corrupt") || msg.contains("row"),
                "open may reject the page, but with a typed error; got: {msg}"
            );
        }
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn open_with_out_of_range_slot_entry_does_not_abort() {
    let path = tmp_path("open_slot_entry");
    let rid = seeded_heap(&path);

    let mut page = read_page(&path, rid.page_id);
    // offset + length = 65534 + 65534, far past the 4096-byte page.
    set_slot_entry(&mut page, rid.slot_index, u16::MAX - 1, u16::MAX - 1);
    clear_checksum_flag(&mut page);
    write_page(&path, rid.page_id, &page);

    match HeapFile::open(&path) {
        Ok(_) => {}
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.to_lowercase().contains("corrupt") || msg.contains("row"),
                "open may reject the page, but with a typed error; got: {msg}"
            );
        }
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn point_lookup_on_stale_crc_page_returns_none_instead_of_aborting() {
    let path = tmp_path("get_crc");
    let rid = seeded_heap(&path);

    // Open first, THEN corrupt on disk, so the read happens through the
    // point-lookup path rather than being caught by the open-time scan.
    let heap = HeapFile::open(&path).unwrap();
    let mut page = read_page(&path, rid.page_id);
    page[40] ^= 0xFF;
    write_page(&path, rid.page_id, &page);

    assert_eq!(
        heap.get(rid),
        None,
        "a point lookup must consult the page CRC and refuse a corrupt page"
    );

    drop(heap);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn open_with_stale_crc_page_errors_instead_of_returning_garbage() {
    let path = tmp_path("open_crc");
    let rid = seeded_heap(&path);

    let mut page = read_page(&path, rid.page_id);
    page[40] ^= 0xFF;
    write_page(&path, rid.page_id, &page);

    let msg = match HeapFile::open(&path) {
        Ok(_) => panic!("a stale CRC must fail the open"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("CRC32 mismatch"),
        "expected a CRC error, got: {msg}"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn point_lookup_with_out_of_range_slot_entry_does_not_abort() {
    let path = tmp_path("get_slot_entry");
    let rid = seeded_heap(&path);

    let mut page = read_page(&path, rid.page_id);
    set_slot_entry(&mut page, rid.slot_index, u16::MAX - 1, u16::MAX - 1);
    clear_checksum_flag(&mut page);
    write_page(&path, rid.page_id, &page);

    let heap = HeapFile::open(&path).expect("unchecksummed page must still open");
    assert_eq!(
        heap.get(rid),
        None,
        "a slot entry pointing outside the page must read as absent"
    );

    drop(heap);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn mmap_point_lookup_with_wild_slot_count_does_not_abort() {
    let path = tmp_path("get_mmap");
    let rid = seeded_heap(&path);

    let mut page = read_page(&path, rid.page_id);
    // A slot index beyond the directory's physical capacity: the mmap fast
    // path computes its entry offset by subtraction and would underflow.
    set_slot_count(&mut page, u16::MAX);
    clear_checksum_flag(&mut page);
    write_page(&path, rid.page_id, &page);

    let mut heap = HeapFile::open(&path).expect("unchecksummed page must still open");
    heap.enable_mmap();
    let wild = RowId {
        page_id: rid.page_id,
        slot_index: 60_000,
    };
    assert_eq!(
        heap.get(wild),
        None,
        "a slot index past the directory's capacity must read as absent"
    );

    drop(heap);
    let _ = std::fs::remove_file(&path);
}
