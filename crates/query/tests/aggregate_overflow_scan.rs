//! `count(T)` and a numeric aggregate over a table that has spilled must
//! count every live row and read only the numeric column.
//!
//! `for_each_row_raw` rehydrates every spill and skips a value that cannot
//! re-inline into a v1 row (>= 64KB). These cases use a 70_000-byte payload
//! so that skip would drop the row, and compare the fast path with the
//! generic path. Healthy pages only: `HeapFile::scan` CRC-checks every page
//! and the raw slot walk does not.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

const LARGE_LEN: usize = 70_000;

fn scalar(result: QueryResult) -> Value {
    match result {
        QueryResult::Scalar(value) => value,
        other => panic!("expected scalar, got {other:?}"),
    }
}

/// Three rows. One payload spills. One id is null.
///
/// Catalog insert accepts a null id as `Value::Empty`. PowQL
/// `insert T { id := null, ... }` also parses; this seed matches the other
/// overflow scan tests.
fn seed() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { id: int, payload: bytes }")
        .unwrap();
    let rows = [
        vec![Value::Int(1), Value::Bytes(vec![0xAB; LARGE_LEN])],
        vec![Value::Int(2), Value::Bytes(b"hi".to_vec())],
        vec![Value::Empty, Value::Bytes(b"x".to_vec())],
    ];
    for row in rows {
        engine
            .catalog_mut()
            .get_table_mut("T")
            .unwrap()
            .insert(&row)
            .unwrap();
    }
    assert!(
        engine.catalog().table_has_overflow("T"),
        "70_000-byte payload must spill"
    );
    (dir, engine)
}

fn assert_matches_generic(engine: &mut Engine, query: &str, expected: Value) {
    engine.set_force_generic_path(true);
    engine.reset_forced_generic_sites();
    assert_eq!(
        scalar(engine.execute_powql(query).unwrap()),
        expected,
        "{query} generic"
    );
    engine.set_force_generic_path(false);
    assert_eq!(
        scalar(engine.execute_powql(query).unwrap()),
        expected,
        "{query}"
    );
    assert_eq!(
        scalar(engine.execute_powql_readonly(query).unwrap()),
        expected,
        "{query} readonly"
    );
}

#[test]
fn count_and_numeric_folds_include_the_spilled_row() {
    let (_dir, mut engine) = seed();
    assert_matches_generic(&mut engine, "count(T)", Value::Int(3));
    // `count(T { .id })` counts non-null ids and must stay off the row-count
    // path. The null id is the difference: 2, not 3.
    assert_matches_generic(&mut engine, "count(T { .id })", Value::Int(2));
    assert_matches_generic(&mut engine, "count(T filter .id >= 0)", Value::Int(2));
    assert_matches_generic(&mut engine, "sum(T { .id })", Value::Int(3));
    assert_matches_generic(&mut engine, "avg(T { .id })", Value::Float(1.5));
    assert_matches_generic(&mut engine, "min(T { .id })", Value::Int(1));
    assert_matches_generic(&mut engine, "max(T { .id })", Value::Int(2));
    assert_matches_generic(&mut engine, "sum(T filter .id >= 0 { .id })", Value::Int(3));
}

#[test]
fn variable_length_predicates_match_generic_by_bailing() {
    // `.payload != null` compiles, and the v2 fallback would see the spilled
    // stub as Empty. The fast path must bail. All three payloads are non-null,
    // so a wrong answer is 2.
    let (_dir, mut engine) = seed();
    assert_matches_generic(
        &mut engine,
        "count(T filter .payload != null)",
        Value::Int(3),
    );
    assert_matches_generic(
        &mut engine,
        "sum(T filter .payload != null { .id })",
        Value::Int(3),
    );
}
