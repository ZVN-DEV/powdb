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
