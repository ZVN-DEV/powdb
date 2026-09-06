//! A damaged data directory must refuse to open, not come up looking healthy.
//!
//! Every case here used to open cleanly and answer queries: a WAL whose file
//! header is gone read as a pre-v0.5.0 log and dropped every un-checkpointed
//! row; a heap truncated mid-page read as a shorter table; heap files with no
//! `catalog.bin` read as a brand new database. All three answer "your data is
//! not there" with a success status, which is the shape this project treats as
//! worse than a crash.

use powdb_storage::catalog::Catalog;
use powdb_storage::error::{StorageError, StorageErrorKind};
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "powdb_damaged_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn users_schema() -> Schema {
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
        ],
    }
}

/// A catalog with `rows` rows, cleanly checkpointed and closed.
fn seed(dir: &std::path::Path, rows: i64) {
    let mut cat = Catalog::create(dir).unwrap();
    cat.create_table(users_schema()).unwrap();
    for i in 0..rows {
        cat.insert("users", &vec![Value::Int(i), Value::Str(format!("u{i}"))])
            .unwrap();
    }
    cat.checkpoint().unwrap();
}

fn open_err(dir: &std::path::Path, what: &str) -> std::io::Error {
    match Catalog::open(dir) {
        Ok(_) => panic!("{what} must refuse the open"),
        Err(error) => error,
    }
}

// ── E6: a WAL with no PWAL file header ───────────────────────────────

/// A headerless WAL is read as a pre-v0.5.0 log whose records start at byte 0.
/// On a directory no pre-v0.5.0 binary could have written, that reading turns
/// every un-checkpointed row into nothing at all, silently.
#[test]
fn a_wal_with_no_pwal_header_refuses_the_open() {
    let dir = temp_dir("wal_header");
    seed(&dir, 3);
    {
        let mut cat = Catalog::open(&dir).unwrap();
        cat.insert("users", &vec![Value::Int(99), Value::Str("late".into())])
            .unwrap();
        cat.sync_wal().unwrap();
        std::mem::forget(cat); // crash: recovery depends on wal.log
    }

    // Overwrite the four magic bytes, leaving the records behind them intact.
    let wal_path = dir.join("wal.log");
    let mut bytes = std::fs::read(&wal_path).unwrap();
    bytes[0..4].copy_from_slice(b"XXXX");
    std::fs::write(&wal_path, &bytes).unwrap();

    let error = open_err(&dir, "a WAL with no PWAL header");
    assert_eq!(
        StorageError::kind_of_io_error(&error),
        Some(StorageErrorKind::WalReplay),
        "expected a WAL replay refusal, got: {error}"
    );
    assert!(
        error.to_string().contains("PWAL"),
        "the refusal must name the missing header, got: {error}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ── E14: heap and catalog files that do not add up ───────────────────

/// `truncate -s 100 users.heap` used to round the partial page away, so the
/// table opened clean and answered zero rows.
#[test]
fn a_heap_truncated_mid_page_refuses_the_open() {
    let dir = temp_dir("heap_mid_page");
    seed(&dir, 500);

    let heap = dir.join("users.heap");
    let bytes = std::fs::read(&heap).unwrap();
    assert!(bytes.len() > 4096, "the seed must span more than one page");
    std::fs::write(&heap, &bytes[..bytes.len() - 100]).unwrap();

    let error = open_err(&dir, "a heap truncated mid-page");
    assert_eq!(
        StorageError::kind_of_io_error(&error),
        Some(StorageErrorKind::CorruptData),
        "expected a corrupt-data refusal, got: {error}"
    );
    assert!(
        error.to_string().contains("whole number"),
        "the refusal must say the length is not a whole page count, got: {error}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A heap truncated to nothing is the same defect with a length that happens
/// to divide evenly.
#[test]
fn a_heap_truncated_to_zero_refuses_the_open() {
    let dir = temp_dir("heap_zero");
    seed(&dir, 20);

    std::fs::write(dir.join("users.heap"), []).unwrap();

    let error = open_err(&dir, "a heap truncated to zero");
    assert_eq!(
        StorageError::kind_of_io_error(&error),
        Some(StorageErrorKind::CorruptData),
        "expected a corrupt-data refusal, got: {error}"
    );
    assert!(
        error.to_string().contains("users"),
        "the refusal must name the table, got: {error}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// `rm catalog.bin` used to look exactly like an empty directory, so the
/// engine created a fresh catalog beside the surviving heap and index files
/// and every table was gone.
#[test]
fn table_files_without_a_catalog_refuse_the_open() {
    let dir = temp_dir("no_catalog");
    seed(&dir, 20);

    std::fs::remove_file(dir.join("catalog.bin")).unwrap();

    let error = open_err(&dir, "table files with no catalog");
    assert_ne!(
        error.kind(),
        std::io::ErrorKind::NotFound,
        "NotFound is what callers read as 'fresh directory'; got: {error}"
    );
    assert_eq!(
        StorageError::kind_of_io_error(&error),
        Some(StorageErrorKind::CatalogCorrupt),
        "expected a catalog-corrupt refusal, got: {error}"
    );
    assert!(
        error.to_string().contains("catalog.bin"),
        "the refusal must name catalog.bin, got: {error}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The fresh-directory path must still work: an empty directory is not
/// damage, and `Catalog::open` must keep reporting NotFound for it so callers
/// can create one.
#[test]
fn an_empty_directory_still_reports_not_found() {
    let dir = temp_dir("empty");
    let error = open_err(&dir, "an empty directory");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    std::fs::remove_dir_all(&dir).ok();
}
