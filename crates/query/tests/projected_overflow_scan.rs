//! Projected scans over a table that has spilled must not fetch overflow
//! chains the query does not project, and must not turn a projected spill
//! into Empty.
//!
//! `try_for_each_row_raw` rehydrates every spill and skips a value that
//! cannot re-inline into a v1 row (>= 64KB). These cases use a 70_000-byte
//! payload so that skip would drop the row, and compare the fast path with
//! the generic path.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::heap::HeapFile;
use powdb_storage::page::PAGE_SIZE;
use powdb_storage::row::{raw_stub, RowLayout};
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

const LARGE_LEN: usize = 70_000;

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn seed() -> (tempfile::TempDir, Engine, Vec<u8>, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { id: int, payload: bytes }")
        .unwrap();
    let large = vec![0xAB; LARGE_LEN];
    let short = b"hi".to_vec();
    engine
        .catalog_mut()
        .get_table_mut("T")
        .unwrap()
        .insert(&vec![Value::Int(1), Value::Bytes(large.clone())])
        .unwrap();
    engine
        .catalog_mut()
        .get_table_mut("T")
        .unwrap()
        .insert(&vec![Value::Int(2), Value::Bytes(short.clone())])
        .unwrap();
    assert!(
        engine.catalog().table_has_overflow("T"),
        "70_000-byte payload must spill"
    );
    (dir, engine, large, short)
}

fn assert_fast_matches_generic(engine: &mut Engine, query: &str) {
    engine.set_force_generic_path(true);
    engine.reset_forced_generic_sites();
    let generic = rows(engine.execute_powql(query).unwrap());
    engine.set_force_generic_path(false);
    assert_eq!(
        rows(engine.execute_powql(query).unwrap()),
        generic,
        "{query}"
    );
    assert_eq!(
        rows(engine.execute_powql_readonly(query).unwrap()),
        generic,
        "{query} readonly"
    );
}

#[test]
fn projecting_the_id_returns_both_rows_without_the_spilled_payload() {
    let (_dir, mut engine, _large, _short) = seed();
    let expected = vec![vec![Value::Int(1)], vec![Value::Int(2)]];
    assert_eq!(rows(engine.execute_powql("T { .id }").unwrap()), expected);
    assert_fast_matches_generic(&mut engine, "T { .id }");
    assert_fast_matches_generic(&mut engine, "T limit 1 { .id }");
    assert_eq!(
        rows(engine.execute_powql("T limit 1 { .id }").unwrap()),
        vec![vec![Value::Int(1)]]
    );
}

#[test]
fn fixed_column_predicate_on_an_overflow_table_matches_generic() {
    let (_dir, mut engine, _large, _short) = seed();
    let query = "T filter .id >= 0 { .id }";
    assert_eq!(
        rows(engine.execute_powql(query).unwrap()),
        vec![vec![Value::Int(1)], vec![Value::Int(2)]]
    );
    assert_fast_matches_generic(&mut engine, query);
    assert_fast_matches_generic(&mut engine, "T filter .id >= 0 limit 1 { .id }");
}

#[test]
fn projecting_the_spilled_payload_returns_the_bytes() {
    let (_dir, mut engine, large, short) = seed();
    let expected = vec![vec![Value::Bytes(large)], vec![Value::Bytes(short)]];
    assert_eq!(
        rows(engine.execute_powql("T { .payload }").unwrap()),
        expected
    );
    assert!(expected.iter().all(|row| !row[0].is_empty()));
    assert_fast_matches_generic(&mut engine, "T { .payload }");
    assert_fast_matches_generic(&mut engine, "T limit 1 { .payload }");
}

#[test]
fn predicate_on_the_spilled_column_matches_generic() {
    // `.payload != null` compiles to an IsNotNull leaf. On a v2 row that
    // leaf's fallback decodes the stub as Empty, so the fast path must bail
    // to the generic reader instead of dropping the spilled row.
    let (_dir, mut engine, large, short) = seed();
    let query = "T filter .payload != null { .payload }";
    let expected = vec![vec![Value::Bytes(large)], vec![Value::Bytes(short)]];
    assert_eq!(rows(engine.execute_powql(query).unwrap()), expected);
    assert_fast_matches_generic(&mut engine, query);
}

fn payload_schema() -> Schema {
    Schema {
        table_name: "T".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                type_id: TypeId::Int,
                required: false,
                position: 0,
            },
            ColumnDef {
                name: "payload".into(),
                type_id: TypeId::Bytes,
                required: false,
                position: 1,
            },
        ],
    }
}

fn read_page(path: &Path, page_id: u32) -> Vec<u8> {
    let mut file = std::fs::File::open(path).unwrap();
    file.seek(SeekFrom::Start(page_id as u64 * PAGE_SIZE as u64))
        .unwrap();
    let mut buf = vec![0u8; PAGE_SIZE];
    file.read_exact(&mut buf).unwrap();
    buf
}

fn write_page(path: &Path, page_id: u32, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.seek(SeekFrom::Start(page_id as u64 * PAGE_SIZE as u64))
        .unwrap();
    file.write_all(bytes).unwrap();
    file.flush().unwrap();
}

/// Flip one payload byte and leave the page CRC stale, same as
/// `corrupt_with_stale_crc` in `crates/storage/tests/scan_corruption.rs`.
/// Offset 64 sits in the overflow payload (the chunk begins at byte 28).
fn corrupt_with_stale_crc(path: &Path, page_id: u32) {
    let mut page = read_page(path, page_id);
    page[64] ^= 0xFF;
    write_page(path, page_id, &page);
}

fn spilled_payload_page(path: &Path) -> u32 {
    let schema = payload_schema();
    let layout = RowLayout::new(&schema);
    let heap = HeapFile::open(path).unwrap();
    let mut found = None;
    heap.try_for_each_row(|_rid, data| {
        if let Some(stub) = raw_stub(&schema, &layout, data, 1) {
            if stub.total_len == LARGE_LEN as u64 {
                found = Some(stub.first_page);
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    })
    .unwrap();
    drop(heap);
    let page_id = found.expect("large payload must have an overflow stub");
    assert_eq!(
        read_page(path, page_id)[4],
        3,
        "stub.first_page must be an overflow page"
    );
    page_id
}

#[test]
fn corrupt_chain_is_unread_unless_the_column_is_projected() {
    let dir = tempfile::tempdir().unwrap();
    let dir_path: PathBuf = dir.path().to_path_buf();
    {
        let mut engine = Engine::new(&dir_path).unwrap();
        engine
            .execute_powql("type T { id: int, payload: bytes }")
            .unwrap();
        engine
            .catalog_mut()
            .get_table_mut("T")
            .unwrap()
            .insert(&vec![Value::Int(1), Value::Bytes(vec![0xAB; LARGE_LEN])])
            .unwrap();
        engine
            .catalog_mut()
            .get_table_mut("T")
            .unwrap()
            .insert(&vec![Value::Int(2), Value::Bytes(b"hi".to_vec())])
            .unwrap();
        // Checkpoint flushes the heap and truncates the WAL. Without that,
        // reopen replays OverflowWrite and puts the original page back,
        // hiding the byte flipped below.
        engine.catalog_mut().checkpoint().unwrap();
    }

    let heap_path = dir_path.join("T.heap");
    let page_id = spilled_payload_page(&heap_path);
    corrupt_with_stale_crc(&heap_path, page_id);

    let mut engine = Engine::new(&dir_path).unwrap();
    let ids = vec![vec![Value::Int(1)], vec![Value::Int(2)]];
    assert_eq!(rows(engine.execute_powql("T { .id }").unwrap()), ids);
    assert_eq!(
        rows(engine.execute_powql_readonly("T { .id }").unwrap()),
        ids
    );

    for result in [
        engine.execute_powql("T { .payload }"),
        engine.execute_powql_readonly("T { .payload }"),
    ] {
        let error = result.expect_err("a projected corrupt chain must error, not return Empty");
        let message = error.to_string();
        let lower = message.to_lowercase();
        assert!(
            lower.contains("corrupt") || lower.contains("checksum") || message.contains("CRC"),
            "error should name the corruption, got: {message}"
        );
    }
}
