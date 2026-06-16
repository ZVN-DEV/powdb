use powdb_storage::heap::{HeapFile, HEAP_MAGIC};
use powdb_storage::page::{Page, PageType, PAGE_FORMAT_VERSION, PAGE_HEADER_SIZE};
use powdb_storage::row::{encode_row, validate_row_format, ROW_MAGIC, ROW_PREFIX_SIZE};
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};
use powdb_storage::wal::{Wal, WAL_FORMAT_VERSION, WAL_MAGIC};

fn temp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_format_{name}_{}_{:?}",
        std::process::id(),
        std::time::Instant::now()
    ))
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
