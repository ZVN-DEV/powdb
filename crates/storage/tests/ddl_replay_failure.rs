//! A DDL record whose replay cannot finish must fail the open.
//!
//! `alter table add column` flushes its WAL record *before* it rewrites the
//! rows, so a rewrite that fails leaves a durable DDL record describing a
//! migration that never happened. Replay used to discard the result of the
//! retry with `let _ = ...`: the table came up half-migrated, reported no
//! error, and every later open repeated the same silent half-migration.

use powdb_storage::catalog::Catalog;
use powdb_storage::error::{StorageError, StorageErrorKind};
use powdb_storage::page::OVERFLOW_CHAIN_END;
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "powdb_ddl_replay_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn spill_schema() -> Schema {
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

/// Seed one spilled row, checkpoint it, and return the chain head of `v`.
fn seed_spilled_row(dir: &std::path::Path) -> u32 {
    let mut cat = Catalog::create(dir).unwrap();
    cat.create_table(spill_schema()).unwrap();
    cat.insert("t", &vec![Value::Int(1), Value::Str("x".repeat(16_384))])
        .unwrap();
    cat.checkpoint().unwrap();

    let tbl = cat.get_table("t").unwrap();
    let rid = tbl.scan().map(|r| r.unwrap()).next().unwrap().0;
    let raw = tbl.heap.get(rid).unwrap().expect("row on disk");
    let stub = powdb_storage::row::raw_stub(tbl.schema(), tbl.row_layout(), &raw, 1)
        .expect("v spilled into a chain");
    stub.first_page
}

#[test]
fn add_column_replay_that_cannot_rewrite_refuses_the_open() {
    let dir = temp_dir("add");
    let head = seed_spilled_row(&dir);

    {
        let mut cat = Catalog::open(&dir).unwrap();
        // Break the chain the rewrite has to read. The page CRC stays valid,
        // so only the whole-value check catches it.
        cat.get_table_mut("t")
            .unwrap()
            .heap
            .write_overflow_page(head, OVERFLOW_CHAIN_END, b"rot", 0)
            .unwrap();
        // The DDL record is flushed before the rewrite, so this leaves a
        // durable record for a migration that failed.
        cat.alter_table_add_column(
            "t",
            ColumnDef {
                name: "extra".into(),
                type_id: TypeId::Str,
                required: false,
                position: 2,
            },
        )
        .expect_err("the live rewrite must fail on the broken chain");
        std::mem::forget(cat); // crash before any checkpoint
    }

    let err = match Catalog::open(&dir) {
        Ok(_) => panic!("replay of the failed migration must refuse the open"),
        Err(e) => e,
    };
    assert_eq!(
        StorageError::kind_of_io_error(&err),
        Some(StorageErrorKind::WalReplay),
        "expected a WAL replay refusal, got: {err}"
    );
    assert!(
        err.to_string().contains("table 't'"),
        "the refusal must name the table, got: {err}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn drop_column_replay_that_cannot_rewrite_refuses_the_open() {
    let dir = temp_dir("drop");
    let head = seed_spilled_row(&dir);

    {
        let mut cat = Catalog::open(&dir).unwrap();
        cat.get_table_mut("t")
            .unwrap()
            .heap
            .write_overflow_page(head, OVERFLOW_CHAIN_END, b"rot", 0)
            .unwrap();
        cat.alter_table_drop_column("t", "v")
            .expect_err("the live rewrite must fail on the broken chain");
        std::mem::forget(cat);
    }

    let err = match Catalog::open(&dir) {
        Ok(_) => panic!("replay of the failed migration must refuse the open"),
        Err(e) => e,
    };
    assert_eq!(
        StorageError::kind_of_io_error(&err),
        Some(StorageErrorKind::WalReplay),
        "expected a WAL replay refusal, got: {err}"
    );
    assert!(
        err.to_string().contains("table 't'"),
        "the refusal must name the table, got: {err}"
    );

    std::fs::remove_dir_all(&dir).ok();
}
