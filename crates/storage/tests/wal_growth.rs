//! E18: the WAL must not grow without bound while a process stays up.
//!
//! The log used to be truncated only when the catalog was closed, so a
//! long-running writer carried every record it had ever written and a crash
//! replayed all of them.

use powdb_storage::catalog::Catalog;
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_wal_growth_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn one_str_schema() -> Schema {
    Schema {
        table_name: "t".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            },
            ColumnDef {
                name: "v".into(),
                type_id: TypeId::Str,
                required: true,
                position: 1,
            },
        ],
    }
}

fn wal_len(dir: &std::path::Path) -> u64 {
    std::fs::metadata(dir.join("wal.log")).map_or(0, |m| m.len())
}

#[test]
fn a_long_run_of_mutations_does_not_grow_the_wal_without_bound() {
    let dir = temp_dir("bounded");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    // A small threshold keeps the test quick; the shipped default is 64 MiB.
    cat.set_wal_checkpoint_bytes(64 * 1024);
    cat.create_table(one_str_schema()).unwrap();

    let body = "x".repeat(200);
    let mut peak = 0u64;
    for i in 0..10_000i64 {
        cat.insert("t", &vec![Value::Int(i), Value::Str(body.clone())])
            .unwrap();
        // What the executor does at the end of every mutating statement.
        cat.commit_autocommit().unwrap();
        if i % 100 == 0 {
            peak = peak.max(wal_len(&dir));
        }
    }
    peak = peak.max(wal_len(&dir));

    // Every row must still be there once the run ends.
    cat.checkpoint().unwrap();
    let rows = cat.scan("t").unwrap().count();
    drop(cat);
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(rows, 10_000, "auto-checkpointing lost rows");
    assert!(
        peak < 1024 * 1024,
        "WAL peaked at {peak} bytes with a 64 KiB checkpoint threshold"
    );
}

#[test]
fn a_zero_threshold_leaves_the_wal_to_grow_until_close() {
    let dir = temp_dir("unbounded");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.set_wal_checkpoint_bytes(0);
    cat.create_table(one_str_schema()).unwrap();

    let body = "x".repeat(200);
    for i in 0..4_000i64 {
        cat.insert("t", &vec![Value::Int(i), Value::Str(body.clone())])
            .unwrap();
        cat.commit_autocommit().unwrap();
    }
    let grown = wal_len(&dir);
    drop(cat);
    std::fs::remove_dir_all(&dir).ok();

    assert!(
        grown > 1024 * 1024,
        "opting out should keep the whole log, got {grown} bytes"
    );
}

/// The threshold is settable but was write-only, so nothing could report what
/// a running catalog had been configured with. A server that wants to log or
/// expose its own effective setting needs to read it back.
#[test]
fn the_checkpoint_threshold_reads_back_what_was_set() {
    let dir = temp_dir("readback");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();

    assert_eq!(
        cat.wal_checkpoint_bytes(),
        powdb_storage::catalog::DEFAULT_WAL_CHECKPOINT_BYTES,
        "a fresh catalog starts on the documented default"
    );

    cat.set_wal_checkpoint_bytes(64 * 1024);
    assert_eq!(cat.wal_checkpoint_bytes(), 64 * 1024);

    cat.set_wal_checkpoint_bytes(0);
    assert_eq!(
        cat.wal_checkpoint_bytes(),
        0,
        "0 is a real setting (automatic checkpoints off), not 'unset'"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
