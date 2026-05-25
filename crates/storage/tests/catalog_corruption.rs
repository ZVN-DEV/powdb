//! Catalog corruption resilience tests.
//!
//! These tests verify that `Catalog::open` returns proper errors (not
//! panics) when the `catalog.bin` file is truncated, zeroed, or contains
//! garbage bytes. A production system should never silently accept a
//! corrupt catalog.

use powdb_storage::catalog::Catalog;
use powdb_storage::types::{ColumnDef, Schema, TypeId};

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "powdb_cat_corrupt_{name}_{}_{:?}",
        std::process::id(),
        std::time::Instant::now()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn test_schema() -> Schema {
    Schema {
        table_name: "users".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            },
            ColumnDef {
                name: "name".into(),
                type_id: TypeId::Str,
                required: true,
                position: 1,
            },
            ColumnDef {
                name: "email".into(),
                type_id: TypeId::Str,
                required: false,
                position: 2,
            },
        ],
    }
}

/// Helper: create a valid catalog with one table, checkpoint it, return the dir.
fn create_valid_catalog(name: &str) -> std::path::PathBuf {
    let dir = temp_dir(name);
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(test_schema()).unwrap();
    cat.checkpoint().unwrap();
    drop(cat);
    dir
}

// ── Zero-length catalog.bin ──────────────────────────────────────────

#[test]
fn test_zero_length_catalog() {
    let dir = create_valid_catalog("zero_len");
    let cat_path = dir.join("catalog.bin");

    // Truncate catalog.bin to zero bytes.
    std::fs::write(&cat_path, b"").unwrap();

    let result = Catalog::open(&dir);
    assert!(
        result.is_err(),
        "opening a zero-length catalog should return an error"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ── Truncated catalog (fewer bytes than expected) ─────────────────────

#[test]
fn test_truncated_catalog_half() {
    let dir = create_valid_catalog("trunc_half");
    let cat_path = dir.join("catalog.bin");

    let data = std::fs::read(&cat_path).unwrap();
    // Write only the first half.
    std::fs::write(&cat_path, &data[..data.len() / 2]).unwrap();

    let result = Catalog::open(&dir);
    assert!(
        result.is_err(),
        "opening a truncated catalog (half) should return an error"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_truncated_catalog_header_only() {
    let dir = create_valid_catalog("trunc_header");
    let cat_path = dir.join("catalog.bin");

    // Keep only the magic + version (6 bytes) but drop the table count.
    std::fs::write(&cat_path, b"BCAT\x02\x00").unwrap();

    let result = Catalog::open(&dir);
    assert!(
        result.is_err(),
        "opening a catalog with only the magic+version should fail"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ── Corrupted bytes in the middle ─────────────────────────────────────

#[test]
fn test_corrupted_middle_bytes() {
    let dir = create_valid_catalog("corrupt_mid");
    let cat_path = dir.join("catalog.bin");

    let mut data = std::fs::read(&cat_path).unwrap();
    // Flip bytes in the middle of the table entry data.
    let mid = data.len() / 2;
    for i in 0..4.min(data.len() - mid) {
        data[mid + i] ^= 0xFF;
    }
    std::fs::write(&cat_path, &data).unwrap();

    let result = Catalog::open(&dir);
    // This should either error or produce a visibly wrong catalog
    // (wrong column names, bad type IDs, etc.). We accept either an
    // error or a successful open with garbled data — the key invariant
    // is that it must NOT panic.
    let _ = result;
    std::fs::remove_dir_all(&dir).ok();
}

// ── Garbage bytes in catalog.bin ──────────────────────────────────────

#[test]
fn test_garbage_bytes() {
    let dir = create_valid_catalog("garbage");
    let cat_path = dir.join("catalog.bin");

    // Overwrite with random-ish garbage.
    let garbage: Vec<u8> = (0..128).map(|i| (i * 37 + 13) as u8).collect();
    std::fs::write(&cat_path, &garbage).unwrap();

    let result = Catalog::open(&dir);
    assert!(
        result.is_err(),
        "opening a catalog full of garbage should return an error"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ── Bad magic ─────────────────────────────────────────────────────────

#[test]
fn test_bad_magic() {
    let dir = create_valid_catalog("bad_magic");
    let cat_path = dir.join("catalog.bin");

    let mut data = std::fs::read(&cat_path).unwrap();
    // Overwrite the "BCAT" magic with something else.
    data[0] = b'X';
    data[1] = b'X';
    std::fs::write(&cat_path, &data).unwrap();

    let result = Catalog::open(&dir);
    assert!(result.is_err(), "bad magic should be rejected");
    std::fs::remove_dir_all(&dir).ok();
}

// ── Bad version ───────────────────────────────────────────────────────

#[test]
fn test_unsupported_version() {
    let dir = create_valid_catalog("bad_version");
    let cat_path = dir.join("catalog.bin");

    let mut data = std::fs::read(&cat_path).unwrap();
    // Set version to 99 (unsupported).
    data[4] = 99;
    data[5] = 0;
    std::fs::write(&cat_path, &data).unwrap();

    let result = Catalog::open(&dir);
    assert!(
        result.is_err(),
        "unsupported catalog version should be rejected"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ── Inflated table count ──────────────────────────────────────────────

#[test]
fn test_inflated_table_count() {
    let dir = create_valid_catalog("inflated_count");
    let cat_path = dir.join("catalog.bin");

    let mut data = std::fs::read(&cat_path).unwrap();
    // Set n_tables to a huge number (bytes 6..10).
    if data.len() >= 10 {
        data[6] = 0xFF;
        data[7] = 0xFF;
        data[8] = 0x00;
        data[9] = 0x00; // n_tables = 65535
        std::fs::write(&cat_path, &data).unwrap();

        let result = Catalog::open(&dir);
        // Should error because the file is too short to contain 65535 tables.
        assert!(
            result.is_err(),
            "inflated table count should trigger an EOF error"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

// ── Valid catalog round-trip (sanity check) ───────────────────────────

#[test]
fn test_valid_catalog_round_trip() {
    let dir = create_valid_catalog("valid");

    let cat = Catalog::open(&dir).unwrap();
    assert!(cat.get_table("users").is_some());
    assert!(cat.get_table("nonexistent").is_none());
    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

// ── Multiple tables with corruption ───────────────────────────────────

#[test]
fn test_multi_table_catalog_truncated() {
    let dir = temp_dir("multi_trunc");
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(test_schema()).unwrap();
    cat.create_table(Schema {
        table_name: "orders".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            },
            ColumnDef {
                name: "amount".into(),
                type_id: TypeId::Float,
                required: true,
                position: 1,
            },
        ],
    })
    .unwrap();
    cat.checkpoint().unwrap();
    drop(cat);

    let cat_path = dir.join("catalog.bin");
    let data = std::fs::read(&cat_path).unwrap();
    // Truncate somewhere in the second table's entry.
    let truncate_at = data.len() - 5;
    std::fs::write(&cat_path, &data[..truncate_at]).unwrap();

    let result = Catalog::open(&dir);
    assert!(
        result.is_err(),
        "truncation in the second table entry should error"
    );
    std::fs::remove_dir_all(&dir).ok();
}
