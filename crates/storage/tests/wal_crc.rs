//! WAL CRC32 rejection tests.
//!
//! These tests verify that the WAL reader (`Wal::read_all`) correctly
//! detects and rejects corrupted records via CRC32 validation, rather
//! than silently processing corrupt data.
//!
//! WAL record layout (from wal.rs):
//!   len:    4 bytes (u32 LE) — total record size including header
//!   crc32:  4 bytes (u32 LE) — CRC of (tx_id + type + data)
//!   tx_id:  8 bytes (u64 LE)
//!   type:   1 byte
//!   data:   (len - 17) bytes

use powdb_storage::wal::{Wal, WalRecordType};
use std::io::Write;

const WAL_HEADER_SIZE: usize = 17;

fn temp_wal(name: &str) -> (Wal, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "powdb_wal_crc_{name}_{}_{:?}",
        std::process::id(),
        std::time::Instant::now()
    ));
    let wal = Wal::create(&path, 128).unwrap();
    (wal, path)
}

/// Write some valid records, flush, and return the WAL file path.
fn write_valid_records(name: &str, count: usize) -> std::path::PathBuf {
    let (mut wal, path) = temp_wal(name);
    for i in 0..count {
        wal.append(
            (i + 1) as u64,
            WalRecordType::Insert,
            format!("record-{i}").as_bytes(),
        )
        .unwrap();
    }
    wal.flush().unwrap();
    drop(wal);
    path
}

// ── CRC bit-flip detection ────────────────────────────────────────────

/// Flip bits in the CRC32 field of the first record. replay should
/// detect the mismatch and return zero records (stops at first bad record).
#[test]
fn test_crc_bit_flip_in_first_record() {
    let path = write_valid_records("crc_flip_first", 3);

    // Read the raw WAL file and flip the CRC field (bytes 4..8).
    let mut data = std::fs::read(&path).unwrap();
    assert!(data.len() >= 8, "WAL file too short");
    data[4] ^= 0xFF;
    data[5] ^= 0xFF;
    std::fs::write(&path, &data).unwrap();

    let wal = Wal::open(&path, 128).unwrap();
    let records = wal.read_all().unwrap();
    assert_eq!(
        records.len(),
        0,
        "corrupted first record should stop replay immediately"
    );
    std::fs::remove_file(&path).ok();
}

/// Flip bits in the CRC32 field of the second record. The first record
/// should be returned successfully; the second and third should be dropped.
#[test]
fn test_crc_bit_flip_in_second_record() {
    let path = write_valid_records("crc_flip_second", 3);

    let mut data = std::fs::read(&path).unwrap();
    // Find the start of the second record by reading the first record's length.
    let first_len = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    let second_start = first_len;
    assert!(
        data.len() > second_start + 8,
        "WAL file too short for second record"
    );
    // Flip CRC of the second record.
    data[second_start + 4] ^= 0xFF;
    std::fs::write(&path, &data).unwrap();

    let wal = Wal::open(&path, 128).unwrap();
    let records = wal.read_all().unwrap();
    assert_eq!(
        records.len(),
        1,
        "only the first (uncorrupted) record should survive"
    );
    assert_eq!(records[0].tx_id, 1);
    std::fs::remove_file(&path).ok();
}

// ── Data payload corruption ───────────────────────────────────────────

/// Corrupt the data payload (not the CRC field) — the computed CRC
/// should no longer match the stored CRC.
#[test]
fn test_data_payload_corruption() {
    let path = write_valid_records("data_corrupt", 2);

    let mut data = std::fs::read(&path).unwrap();
    // Corrupt a byte in the data payload of the first record.
    // Data starts at offset WAL_HEADER_SIZE (17).
    if data.len() > WAL_HEADER_SIZE + 2 {
        data[WAL_HEADER_SIZE + 2] ^= 0xFF;
    }
    std::fs::write(&path, &data).unwrap();

    let wal = Wal::open(&path, 128).unwrap();
    let records = wal.read_all().unwrap();
    assert_eq!(
        records.len(),
        0,
        "corrupted data payload should fail CRC check"
    );
    std::fs::remove_file(&path).ok();
}

/// Corrupt the tx_id field — CRC is computed over tx_id+type+data,
/// so changing tx_id should also fail the CRC check.
#[test]
fn test_tx_id_corruption() {
    let path = write_valid_records("txid_corrupt", 2);

    let mut data = std::fs::read(&path).unwrap();
    // tx_id is at offset 8..16 in the first record.
    data[8] ^= 0xFF;
    std::fs::write(&path, &data).unwrap();

    let wal = Wal::open(&path, 128).unwrap();
    let records = wal.read_all().unwrap();
    assert_eq!(records.len(), 0, "corrupted tx_id should fail CRC check");
    std::fs::remove_file(&path).ok();
}

/// Corrupt the record type byte — also covered by CRC.
#[test]
fn test_record_type_corruption() {
    let path = write_valid_records("type_corrupt", 2);

    let mut data = std::fs::read(&path).unwrap();
    // Record type is at offset 16 in the first record.
    data[16] ^= 0xFF;
    std::fs::write(&path, &data).unwrap();

    let wal = Wal::open(&path, 128).unwrap();
    let records = wal.read_all().unwrap();
    assert_eq!(
        records.len(),
        0,
        "corrupted record type should fail CRC or type parse"
    );
    std::fs::remove_file(&path).ok();
}

// ── Truncated WAL records ─────────────────────────────────────────────

/// Truncate the WAL file in the middle of the first record.
#[test]
fn test_truncated_mid_first_record() {
    let path = write_valid_records("trunc_mid_first", 3);

    let data = std::fs::read(&path).unwrap();
    let first_len = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    // Truncate to half of the first record.
    std::fs::write(&path, &data[..first_len / 2]).unwrap();

    let wal = Wal::open(&path, 128).unwrap();
    let records = wal.read_all().unwrap();
    assert_eq!(
        records.len(),
        0,
        "truncated first record should yield zero records"
    );
    std::fs::remove_file(&path).ok();
}

/// Truncate the WAL mid-header (fewer than 17 bytes).
#[test]
fn test_truncated_mid_header() {
    let path = write_valid_records("trunc_header", 1);

    // Truncate to just 10 bytes (less than WAL_HEADER_SIZE).
    let data = std::fs::read(&path).unwrap();
    std::fs::write(&path, &data[..10.min(data.len())]).unwrap();

    let wal = Wal::open(&path, 128).unwrap();
    let records = wal.read_all().unwrap();
    assert_eq!(records.len(), 0);
    std::fs::remove_file(&path).ok();
}

/// Truncate between the first and second record — first should survive.
#[test]
fn test_truncated_between_records() {
    let path = write_valid_records("trunc_between", 3);

    let data = std::fs::read(&path).unwrap();
    let first_len = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    // Keep the full first record plus a few extra bytes (partial second header).
    let truncate_at = first_len + 5;
    std::fs::write(&path, &data[..truncate_at.min(data.len())]).unwrap();

    let wal = Wal::open(&path, 128).unwrap();
    let records = wal.read_all().unwrap();
    assert_eq!(records.len(), 1, "only the first complete record should survive");
    assert_eq!(records[0].tx_id, 1);
    std::fs::remove_file(&path).ok();
}

// ── Partially written records (crash mid-write) ──────────────────────

/// Simulate a crash mid-write: write a valid length field but incomplete data.
#[test]
fn test_partial_write_valid_header_no_data() {
    let path = std::env::temp_dir().join(format!(
        "powdb_wal_crc_partial_{}_{:?}",
        std::process::id(),
        std::time::Instant::now()
    ));

    // Write one valid record first.
    {
        let mut wal = Wal::create(&path, 128).unwrap();
        wal.append(1, WalRecordType::Insert, b"valid record")
            .unwrap();
        wal.flush().unwrap();
    }

    // Append a partial record: write the header claiming 100 bytes total
    // but only write a few data bytes.
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        let total_len: u32 = 100;
        f.write_all(&total_len.to_le_bytes()).unwrap();
        f.write_all(&[0u8; 4]).unwrap(); // fake CRC
        f.write_all(&2u64.to_le_bytes()).unwrap(); // tx_id
        f.write_all(&[1u8]).unwrap(); // type = Insert
        f.write_all(b"partial").unwrap(); // only 7 bytes of data, not 83
        f.flush().unwrap();
    }

    let wal = Wal::open(&path, 128).unwrap();
    let records = wal.read_all().unwrap();
    assert_eq!(
        records.len(),
        1,
        "the partial second record should be skipped; first should survive"
    );
    assert_eq!(records[0].data, b"valid record");
    std::fs::remove_file(&path).ok();
}

// ── Empty WAL file ────────────────────────────────────────────────────

#[test]
fn test_empty_wal_file() {
    let (wal, path) = temp_wal("empty");
    let records = wal.read_all().unwrap();
    assert!(records.is_empty());
    drop(wal);
    std::fs::remove_file(&path).ok();
}

// ── Corrupted length field ────────────────────────────────────────────

/// Set the length field to zero (less than WAL_HEADER_SIZE). The reader
/// should treat this as corruption and stop.
#[test]
fn test_zero_length_field() {
    let path = write_valid_records("zero_len", 2);

    let mut data = std::fs::read(&path).unwrap();
    // Set the first record's length to 0.
    data[0] = 0;
    data[1] = 0;
    data[2] = 0;
    data[3] = 0;
    std::fs::write(&path, &data).unwrap();

    let wal = Wal::open(&path, 128).unwrap();
    let records = wal.read_all().unwrap();
    assert_eq!(
        records.len(),
        0,
        "zero-length record should stop replay"
    );
    std::fs::remove_file(&path).ok();
}

/// Set the length field to a value smaller than WAL_HEADER_SIZE (e.g. 5).
/// The checked_sub in read_all should catch this.
#[test]
fn test_length_smaller_than_header() {
    let path = write_valid_records("len_small", 2);

    let mut data = std::fs::read(&path).unwrap();
    // Set length to 5 (less than WAL_HEADER_SIZE=17).
    data[0] = 5;
    data[1] = 0;
    data[2] = 0;
    data[3] = 0;
    std::fs::write(&path, &data).unwrap();

    let wal = Wal::open(&path, 128).unwrap();
    let records = wal.read_all().unwrap();
    assert_eq!(records.len(), 0);
    std::fs::remove_file(&path).ok();
}

/// Set the length field to a huge value (>256MB). The MAX_WAL_RECORD_SIZE
/// guard should stop replay without allocating.
#[test]
fn test_huge_length_field() {
    let path = write_valid_records("huge_len", 1);

    let mut data = std::fs::read(&path).unwrap();
    // Set the first record's length to 0x20000000 (512MB).
    data[0] = 0x00;
    data[1] = 0x00;
    data[2] = 0x00;
    data[3] = 0x20;
    std::fs::write(&path, &data).unwrap();

    let wal = Wal::open(&path, 128).unwrap();
    let records = wal.read_all().unwrap();
    assert_eq!(
        records.len(),
        0,
        "huge length field should stop replay"
    );
    std::fs::remove_file(&path).ok();
}

// ── Valid records survive alongside corruption ────────────────────────

/// Write 5 valid records, corrupt only the 4th. Records 1-3 should
/// survive, 4-5 should be dropped.
#[test]
fn test_corruption_mid_stream() {
    let path = write_valid_records("mid_stream", 5);

    let data = std::fs::read(&path).unwrap();
    // Walk through records to find the start of the 4th.
    let mut offset = 0usize;
    for _ in 0..3 {
        let len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += len;
    }
    // Corrupt the CRC of the 4th record.
    let mut corrupted = data.clone();
    corrupted[offset + 4] ^= 0xFF;
    std::fs::write(&path, &corrupted).unwrap();

    let wal = Wal::open(&path, 128).unwrap();
    let records = wal.read_all().unwrap();
    assert_eq!(records.len(), 3, "records 1-3 should survive, 4-5 dropped");
    for (i, rec) in records.iter().enumerate() {
        assert_eq!(rec.tx_id, (i + 1) as u64);
    }
    std::fs::remove_file(&path).ok();
}
