//! Top-N over a table that has spilled must rank raw rows and fetch an
//! overflow chain only for a projected column of a winner.
//!
//! `try_for_each_row_raw` rehydrates every spill and skips a value that
//! cannot re-inline into a v1 row (>= 64KB). These cases use a 70_000-byte
//! payload so that skip would drop the row. Happy paths are compared with
//! the generic reader and with the readonly surface.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::heap::HeapFile;
use powdb_storage::page::PAGE_SIZE;
use powdb_storage::row::{decode_column, raw_stub, RowLayout};
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

fn insert(engine: &mut Engine, id: i64, score: f64, payload: &[u8]) {
    engine
        .catalog_mut()
        .get_table_mut("T")
        .unwrap()
        .insert(&vec![
            Value::Int(id),
            Value::Float(score),
            Value::Bytes(payload.to_vec()),
        ])
        .unwrap();
}

/// id 1 is the smallest id and has a spilled payload.
/// id 2 has the greatest score and an inline payload.
/// id 3 is the largest id and has a spilled payload.
fn seed(dir: &Path) -> (Engine, Vec<u8>, Vec<u8>) {
    let mut engine = Engine::new(dir).unwrap();
    engine
        .execute_powql("type T { id: int, score: float, payload: bytes }")
        .unwrap();
    let low = vec![0x11; LARGE_LEN];
    let high = vec![0xAB; LARGE_LEN];
    insert(&mut engine, 1, 1.0, &low);
    insert(&mut engine, 2, 9.5, b"mid");
    insert(&mut engine, 3, -2.0, &high);
    assert!(
        engine.catalog().table_has_overflow("T"),
        "70_000-byte payload must spill"
    );
    (engine, low, high)
}

fn assert_fast_matches_generic(engine: &mut Engine, query: &str) {
    engine.set_force_generic_path(true);
    engine.reset_forced_generic_sites();
    let generic = rows(engine.execute_powql(query).unwrap());
    assert!(
        engine
            .forced_generic_sites()
            .contains(&"project-filter-sort-limit"),
        "{query} did not reach the top-N fast path"
    );
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
fn descending_id_keeps_the_spilled_winner() {
    let dir = tempfile::tempdir().unwrap();
    let (mut engine, _low, _high) = seed(dir.path());
    let query = "T order .id desc limit 1 { .id }";
    assert_eq!(
        rows(engine.execute_powql(query).unwrap()),
        vec![vec![Value::Int(3)]]
    );
    assert_fast_matches_generic(&mut engine, query);
}

#[test]
fn ascending_top_two_returns_real_payload_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let (mut engine, low, _high) = seed(dir.path());
    let query = "T order .id limit 2 { .id, .payload }";
    let expected = vec![
        vec![Value::Int(1), Value::Bytes(low)],
        vec![Value::Int(2), Value::Bytes(b"mid".to_vec())],
    ];
    assert_eq!(rows(engine.execute_powql(query).unwrap()), expected);
    assert!(expected.iter().all(|row| !row[1].is_empty()));
    assert_fast_matches_generic(&mut engine, query);
}

#[test]
fn descending_float_key_matches_generic() {
    let dir = tempfile::tempdir().unwrap();
    let (mut engine, _low, _high) = seed(dir.path());
    let query = "T order .score desc limit 1 { .id }";
    assert_eq!(
        rows(engine.execute_powql(query).unwrap()),
        vec![vec![Value::Int(2)]]
    );
    assert_fast_matches_generic(&mut engine, query);
}

#[test]
fn fixed_predicate_on_an_overflow_table_matches_generic() {
    let dir = tempfile::tempdir().unwrap();
    let (mut engine, _low, _high) = seed(dir.path());
    let query = "T filter .id >= 0 order .id limit 1 { .id }";
    assert_eq!(
        rows(engine.execute_powql(query).unwrap()),
        vec![vec![Value::Int(1)]]
    );
    assert_fast_matches_generic(&mut engine, query);
}

#[test]
fn variable_predicate_matches_generic() {
    // `.payload != null` compiles, but the v2 fallback decodes a stub as
    // Empty. The fast path must bail or it would drop both spilled rows.
    let dir = tempfile::tempdir().unwrap();
    let (mut engine, _low, _high) = seed(dir.path());
    let query = "T filter .payload != null order .id limit 1 { .id }";
    assert_eq!(
        rows(engine.execute_powql(query).unwrap()),
        vec![vec![Value::Int(1)]]
    );
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
                name: "score".into(),
                type_id: TypeId::Float,
                required: false,
                position: 1,
            },
            ColumnDef {
                name: "payload".into(),
                type_id: TypeId::Bytes,
                required: false,
                position: 2,
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
/// `corrupt_with_stale_crc` in the projected overflow scan test.
/// Offset 64 sits in the overflow payload (the chunk begins at byte 28).
fn corrupt_with_stale_crc(path: &Path, page_id: u32) {
    let mut page = read_page(path, page_id);
    page[64] ^= 0xFF;
    write_page(path, page_id, &page);
}

fn payload_page_for_id(path: &Path, id: i64) -> u32 {
    let schema = payload_schema();
    let layout = RowLayout::new(&schema);
    let heap = HeapFile::open(path).unwrap();
    let mut found = None;
    heap.try_for_each_row(|_rid, data| {
        if decode_column(&schema, &layout, data, 0) == Value::Int(id) {
            if let Some(stub) = raw_stub(&schema, &layout, data, 2) {
                found = Some(stub.first_page);
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    })
    .unwrap();
    drop(heap);
    let page_id = found.expect("row must have an overflow stub");
    assert_eq!(
        read_page(path, page_id)[4],
        3,
        "stub.first_page must be an overflow page"
    );
    page_id
}

fn reopen_with_corrupt_payload(id: i64) -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let dir_path: PathBuf = dir.path().to_path_buf();
    {
        let (mut engine, _low, _high) = seed(&dir_path);
        // Checkpoint flushes the heap and truncates the WAL. Without that,
        // reopen replays OverflowWrite and puts the original page back.
        engine.catalog_mut().checkpoint().unwrap();
    }
    let heap_path = dir_path.join("T.heap");
    let page_id = payload_page_for_id(&heap_path, id);
    corrupt_with_stale_crc(&heap_path, page_id);
    let engine = Engine::new(&dir_path).unwrap();
    (dir, engine)
}

#[test]
fn corrupt_chain_outside_the_top_n_does_not_change_the_winner() {
    let (_dir, mut engine) = reopen_with_corrupt_payload(1);
    let query = "T order .id desc limit 1 { .id }";
    let winner = vec![vec![Value::Int(3)]];
    assert_eq!(rows(engine.execute_powql(query).unwrap()), winner);
    assert_eq!(rows(engine.execute_powql_readonly(query).unwrap()), winner);

    // Heap scan verifies every page, including an overflow page this
    // projection never reads, so the generic path errors. The winner above
    // is the fast path, not that scan.
    engine.set_force_generic_path(true);
    engine.reset_forced_generic_sites();
    let error = engine
        .execute_powql(query)
        .expect_err("generic scan reads the corrupt overflow page");
    assert!(
        engine
            .forced_generic_sites()
            .contains(&"project-filter-sort-limit"),
        "{query} did not reach the top-N fast path"
    );
    let message = error.to_string();
    let lower = message.to_lowercase();
    assert!(
        lower.contains("corrupt") || lower.contains("checksum") || message.contains("CRC"),
        "generic error should name the corruption, got: {message}"
    );
}

#[test]
fn corrupt_winner_chain_errors_only_when_projected() {
    // The generic scan replaces a row whose chain will not reassemble with
    // Empty cells, which changes which id wins. This case checks the fast
    // path itself: the id is still the winner, and projecting the payload
    // is an error rather than Empty.
    let (_dir, mut engine) = reopen_with_corrupt_payload(3);
    let ids = vec![vec![Value::Int(3)]];
    assert_eq!(
        rows(
            engine
                .execute_powql("T order .id desc limit 1 { .id }")
                .unwrap()
        ),
        ids
    );
    assert_eq!(
        rows(
            engine
                .execute_powql_readonly("T order .id desc limit 1 { .id }")
                .unwrap()
        ),
        ids
    );

    for result in [
        engine.execute_powql("T order .id desc limit 1 { .payload }"),
        engine.execute_powql_readonly("T order .id desc limit 1 { .payload }"),
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
