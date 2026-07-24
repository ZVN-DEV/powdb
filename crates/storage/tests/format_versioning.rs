use powdb_storage::heap::{
    HeapFile, HEAP_FORMAT_VERSION, HEAP_FORMAT_VERSION_WITH_OVERFLOW, HEAP_MAGIC,
};
use powdb_storage::page::{
    Page, PageType, OVERFLOW_CHAIN_END, PAGE_FORMAT_VERSION, PAGE_HEADER_SIZE,
};
use powdb_storage::row::{
    encode_row, encode_row_v2_into, row_format_version, validate_row_format, RowLayout, ROW_MAGIC,
    ROW_PREFIX_SIZE,
};
use powdb_storage::types::{ColumnDef, RowId, Schema, TypeId, Value};
use powdb_storage::wal::{Wal, WAL_FORMAT_VERSION, WAL_MAGIC};

fn temp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_format_{name}_{}_{:?}",
        std::process::id(),
        std::time::Instant::now()
    ))
}

/// Recompute and write back the CRC of page `page_id` inside a raw heap-file
/// image, so a test that tampers with page contents still presents a
/// checksum-valid page to the open path.
fn restamp_page_crc(bytes: &mut [u8], page_id: u32) {
    use powdb_storage::page::PAGE_SIZE;
    let start = page_id as usize * PAGE_SIZE;
    let mut page = Page::from_bytes(&bytes[start..start + PAGE_SIZE]).expect("page image");
    page.stamp_checksum();
    bytes[start..start + PAGE_SIZE].copy_from_slice(page.as_bytes());
}

fn schema() -> Schema {
    Schema {
        table_name: "T".into(),
        columns: vec![ColumnDef {
            name: "name".into(),
            type_id: TypeId::Str,
            required: true,
            position: 0,
        }],
    }
}

#[test]
fn current_row_encoding_has_magic_and_rejects_unknown_version() {
    let row = encode_row(&schema(), &[Value::Str("alice".into())]);
    assert_eq!(&row[0..4], ROW_MAGIC);
    assert_eq!(ROW_PREFIX_SIZE, 6);

    let mut bad = row;
    bad[4..6].copy_from_slice(&(u16::MAX).to_le_bytes());
    let err = validate_row_format(&bad).unwrap_err();
    assert!(err.to_string().contains("unsupported row format version"));
}

#[test]
fn page_header_rejects_unknown_format_version() {
    let page = Page::new(7, PageType::Data);
    let mut bytes = *page.as_bytes();
    bytes[5] = (PAGE_FORMAT_VERSION + 1) << 4;
    let err = match Page::from_bytes_verified(&bytes) {
        Ok(_) => panic!("page should reject unknown version"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("unsupported page format version"));
}

#[test]
fn wal_file_header_rejects_unknown_version() {
    let path = temp_path("wal_unknown");
    {
        let mut wal = Wal::create(&path, 4).unwrap();
        wal.append(1, powdb_storage::wal::WalRecordType::Commit, b"")
            .unwrap();
        wal.flush().unwrap();
    }
    let mut bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[0..4], WAL_MAGIC);
    assert_eq!(
        u16::from_le_bytes(bytes[4..6].try_into().unwrap()),
        WAL_FORMAT_VERSION
    );
    bytes[4..6].copy_from_slice(&(WAL_FORMAT_VERSION + 1).to_le_bytes());
    std::fs::write(&path, bytes).unwrap();
    let err = match Wal::open(&path, 4) {
        Ok(_) => panic!("WAL should reject unknown version"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("unsupported WAL format version"));
    std::fs::remove_file(path).ok();
}

#[test]
fn heap_superblock_rejects_unknown_version() {
    let path = temp_path("heap_unknown");
    {
        let mut heap = HeapFile::create(&path).unwrap();
        heap.flush_hot_page().unwrap();
    }
    let mut bytes = std::fs::read(&path).unwrap();
    let version_offset = PAGE_HEADER_SIZE + HEAP_MAGIC.len();
    bytes[version_offset..version_offset + 2].copy_from_slice(&u16::MAX.to_le_bytes());
    std::fs::write(&path, bytes).unwrap();
    let err = match HeapFile::open(&path) {
        Ok(_) => panic!("heap should reject unknown version"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("unsupported heap format version"));
    std::fs::remove_file(path).ok();
}

#[test]
fn heap_open_rejects_unknown_row_version_before_decode() {
    let path = temp_path("row_unknown_in_heap");
    let sch = schema();
    {
        let mut heap = HeapFile::create(&path).unwrap();
        let row = encode_row(&sch, &[Value::Str("alice".into())]);
        heap.insert(&row).unwrap();
        heap.flush_hot_page().unwrap();
    }
    let mut bytes = std::fs::read(&path).unwrap();
    let pos = bytes
        .windows(ROW_MAGIC.len())
        .position(|w| w == ROW_MAGIC)
        .expect("row magic written into heap");
    bytes[pos + 4..pos + 6].copy_from_slice(&u16::MAX.to_le_bytes());
    // TASK-08: the open path now verifies the page CRC before walking rows, so
    // re-stamp the tampered page. Without this the CRC gate fires first and
    // the row-version gate under test is never reached.
    restamp_page_crc(&mut bytes, (pos / powdb_storage::page::PAGE_SIZE) as u32);
    std::fs::write(&path, bytes).unwrap();
    let err = match HeapFile::open(&path) {
        Ok(_) => panic!("heap should reject unknown row version"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("unsupported row format version"));
    std::fs::remove_file(path).ok();
}

#[test]
fn new_heap_reserves_page_zero_for_superblock() {
    let path = temp_path("heap_superblock");
    let mut heap = HeapFile::create(&path).unwrap();
    assert_eq!(heap.format_version(), 2);
    assert_eq!(heap.first_data_page(), 1);
    let row = encode_row(&schema(), &[Value::Str("first".into())]);
    let rid = heap.insert(&row).unwrap();
    assert_eq!(rid.page_id, 1, "data pages start after superblock");
    drop(heap);
    std::fs::remove_file(path).ok();
}

#[test]
fn legacy_heap_without_superblock_and_legacy_rows_still_opens() {
    let path = temp_path("legacy_heap");
    let sch = schema();
    let encoded = encode_row(&sch, &[Value::Str("legacy".into())]);
    let legacy_body = &encoded[ROW_PREFIX_SIZE..];
    let mut page = Page::new(0, PageType::Data);
    let slot = page.insert(legacy_body).unwrap();
    page.stamp_checksum();
    std::fs::write(&path, page.as_bytes()).unwrap();

    let heap = HeapFile::open(&path).unwrap();
    assert_eq!(heap.format_version(), 1);
    let bytes = heap
        .get(powdb_storage::types::RowId {
            page_id: 0,
            slot_index: slot,
        })
        .unwrap();
    assert_eq!(
        powdb_storage::row::decode_row(&sch, &bytes)[0],
        Value::Str("legacy".into())
    );
    std::fs::remove_file(path).ok();
}

#[test]
fn row_gate_accepts_v2_and_rejects_v3() {
    // A v2 row (overflow bitmap + no actual spill) passes the gate.
    let sch = schema();
    let layout = RowLayout::new(&sch);
    let mut v2 = Vec::new();
    encode_row_v2_into(
        &sch,
        &layout,
        &[Value::Str("alice".into())],
        &vec![None; layout.n_var()],
        &mut v2,
    );
    assert_eq!(&v2[0..4], ROW_MAGIC);
    assert_eq!(row_format_version(&v2).unwrap(), 2);
    validate_row_format(&v2).expect("v2 row must be accepted");

    // Bumping the version byte to 3 (a future format) is refused by this
    // build's gate — the "old gate refuses a newer row" back-compat contract.
    let mut v3 = v2;
    v3[4..6].copy_from_slice(&3u16.to_le_bytes());
    assert!(validate_row_format(&v3).is_err());
}

#[test]
fn heap_stays_v2_until_first_chain_then_bumps_to_v3() {
    let path = temp_path("heap_lazy_v3");
    let value = vec![0xABu8; 9000]; // 3 chunks
    {
        let mut heap = HeapFile::create(&path).unwrap();
        // A never-spilling insert keeps the heap at v2.
        heap.insert(&encode_row(&schema(), &[Value::Str("small".into())]))
            .unwrap();
        assert_eq!(heap.format_version(), HEAP_FORMAT_VERSION);
        heap.flush_hot_page().unwrap();
    }
    // On-disk version byte is still 2.
    let bytes = std::fs::read(&path).unwrap();
    let voff = PAGE_HEADER_SIZE + HEAP_MAGIC.len();
    assert_eq!(
        u16::from_le_bytes(bytes[voff..voff + 2].try_into().unwrap()),
        HEAP_FORMAT_VERSION
    );

    // Writing a chain lazily bumps the superblock to v3, on disk.
    {
        let mut heap = HeapFile::open(&path).unwrap();
        let n = value.len().div_ceil(4068).max(1);
        let mut pages = Vec::new();
        for _ in 0..n {
            pages.push(heap.allocate_overflow_page().unwrap());
        }
        for i in 0..n {
            let start = i * 4068;
            let end = (start + 4068).min(value.len());
            let next = if i + 1 < n {
                pages[i + 1]
            } else {
                OVERFLOW_CHAIN_END
            };
            heap.write_overflow_page(pages[i], next, &value[start..end], 0)
                .unwrap();
        }
        assert_eq!(heap.format_version(), HEAP_FORMAT_VERSION_WITH_OVERFLOW);
        heap.flush_hot_page().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        u16::from_le_bytes(bytes[voff..voff + 2].try_into().unwrap()),
        HEAP_FORMAT_VERSION_WITH_OVERFLOW,
        "first chain write must persist a v3 superblock"
    );

    // Reopening a v3 heap works (new code accepts 2 and 3).
    let heap = HeapFile::open(&path).unwrap();
    assert_eq!(heap.format_version(), HEAP_FORMAT_VERSION_WITH_OVERFLOW);
    let _ = RowId {
        page_id: 1,
        slot_index: 0,
    };
    drop(heap);
    std::fs::remove_file(path).ok();
}

#[test]
fn old_gate_refuses_heap_v3() {
    // Simulate a pre-v0.11 binary whose heap gate only knew v2: patch a v3
    // superblock down-check by asserting the gate rejects an unknown (here,
    // a future v4) version, mirroring how v3 looks to an old build.
    let path = temp_path("heap_v3_oldgate");
    {
        let mut heap = HeapFile::create(&path).unwrap();
        heap.flush_hot_page().unwrap();
    }
    let mut bytes = std::fs::read(&path).unwrap();
    let voff = PAGE_HEADER_SIZE + HEAP_MAGIC.len();
    // A version this build does not know (v4) must be refused — the same
    // failure an old v2-only build produces when handed a v3 file.
    bytes[voff..voff + 2].copy_from_slice(&4u16.to_le_bytes());
    std::fs::write(&path, bytes).unwrap();
    assert!(HeapFile::open(&path).is_err());
    std::fs::remove_file(path).ok();
}

#[test]
fn legacy_wal_without_file_header_still_reads() {
    use std::io::Write;

    let path = temp_path("legacy_wal");
    let payload = b"legacy";
    let tx_id = 7u64;
    let record_type = powdb_storage::wal::WalRecordType::Commit;
    let lsn = 1u64;
    let total_len = (25 + payload.len()) as u32;
    let mut crc_input = Vec::new();
    crc_input.extend_from_slice(&tx_id.to_le_bytes());
    crc_input.push(record_type as u8);
    crc_input.extend_from_slice(&lsn.to_le_bytes());
    crc_input.extend_from_slice(payload);
    let crc = crc32fast::hash(&crc_input);

    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(&total_len.to_le_bytes()).unwrap();
    f.write_all(&crc.to_le_bytes()).unwrap();
    f.write_all(&tx_id.to_le_bytes()).unwrap();
    f.write_all(&[record_type as u8]).unwrap();
    f.write_all(&lsn.to_le_bytes()).unwrap();
    f.write_all(payload).unwrap();
    drop(f);

    let wal = Wal::open(&path, 4).unwrap();
    let records = wal.read_all().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].tx_id, tx_id);
    assert_eq!(records[0].data, payload);
    std::fs::remove_file(path).ok();
}
