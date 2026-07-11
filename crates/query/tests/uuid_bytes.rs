//! uuid/bytes fidelity (#151): string coercion, `uuid()`/`bytes()` sugar,
//! cache-hit correctness, prepared-insert fidelity, SQL frontend, and WAL
//! durability. Uuid/Bytes are already in the on-disk format, so these tests
//! exercise the query frontend and coercion, never a storage change.

use powdb_query::ast::Literal;
use powdb_query::executor::Engine;
use powdb_query::result::{QueryError, QueryResult};
use powdb_storage::types::Value;

const UUID1: &str = "550e8400-e29b-41d4-a716-446655440000";
const UUID1_BYTES: [u8; 16] = [
    0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44, 0x00, 0x00,
];
const UUID2: &str = "00000000-0000-0000-0000-000000000002";

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn count(engine: &mut Engine, table: &str) -> i64 {
    match engine.execute_powql(&format!("count({table})")).unwrap() {
        QueryResult::Scalar(Value::Int(n)) => n,
        QueryResult::Rows { rows, .. } => match rows[0][0] {
            Value::Int(n) => n,
            ref v => panic!("expected int count, got {v:?}"),
        },
        other => panic!("expected count, got {other:?}"),
    }
}

/// A plain string literal into a uuid/bytes column coerces to the real typed
/// value at execution time (the cache-friendly primary path).
#[test]
fn string_coercion_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { id: uuid, blob: bytes }")
        .unwrap();
    engine
        .execute_powql(&format!(
            r#"insert T {{ id := "{UUID1}", blob := "\\xdeadbeef" }}"#
        ))
        .unwrap();

    let r = rows(engine.execute_powql("T { .id, .blob }").unwrap());
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Uuid(UUID1_BYTES));
    assert_eq!(r[0][1], Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
}

/// An invalid uuid string aborts the whole statement — no partial write.
#[test]
fn invalid_uuid_errors_all_or_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine.execute_powql("type T { id: uuid }").unwrap();

    let err = engine
        .execute_powql(&format!(
            r#"insert T {{ id := "{UUID1}" }}, {{ id := "not-a-uuid" }}"#
        ))
        .unwrap_err();
    assert!(
        matches!(&err, QueryError::Execution(m) if m.contains("uuid")),
        "expected a uuid coercion error, got {err:?}"
    );
    assert_eq!(count(&mut engine, "T"), 0, "no row may be written");
}

/// Bad bytea hex (odd length / missing prefix) errors.
#[test]
fn bad_hex_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine.execute_powql("type T { blob: bytes }").unwrap();

    // Odd number of hex digits.
    assert!(engine
        .execute_powql(r#"insert T { blob := "\\xabc" }"#)
        .is_err());
    // Missing `\x` prefix.
    assert!(engine
        .execute_powql(r#"insert T { blob := "deadbeef" }"#)
        .is_err());
    assert_eq!(count(&mut engine, "T"), 0);
}

/// `uuid("…")` / `bytes("…")` sugar and two-arg `cast(…, "uuid")` all land the
/// same typed value in insert position.
#[test]
fn sugar_and_cast_forms_in_insert_position() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { id: uuid, blob: bytes }")
        .unwrap();
    engine
        .execute_powql(&format!(
            r#"insert T {{ id := uuid("{UUID1}"), blob := bytes("\\x0102") }}"#
        ))
        .unwrap();
    engine
        .execute_powql(&format!(r#"insert T {{ id := cast("{UUID2}", "uuid") }}"#))
        .unwrap();

    let r = rows(engine.execute_powql("T order .id { .id, .blob }").unwrap());
    assert_eq!(r.len(), 2);
    // UUID2 (all zeros but ...02) sorts before UUID1.
    assert_eq!(
        r[0][0],
        Value::Uuid([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2])
    );
    assert_eq!(r[1][0], Value::Uuid(UUID1_BYTES));
    assert_eq!(r[1][1], Value::Bytes(vec![0x01, 0x02]));
}

/// `cast(<int>, "datetime")` const-folds in insert position.
#[test]
fn datetime_cast_in_insert_position() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine.execute_powql("type T { at: datetime }").unwrap();
    engine
        .execute_powql(r#"insert T { at := cast(1718000000, "datetime") }"#)
        .unwrap();

    let r = rows(engine.execute_powql("T { .at }").unwrap());
    assert_eq!(r[0][0], Value::DateTime(1718000000));
}

/// `filter .id = uuid(…)` run twice with different uuids proves the cached
/// plan rebinds the inner string on the second (cache-hit) call.
#[test]
fn filter_uuid_cache_hit_correctness() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine.execute_powql("type T { id: uuid }").unwrap();
    engine
        .execute_powql(&format!(
            r#"insert T {{ id := "{UUID1}" }}, {{ id := "{UUID2}" }}"#
        ))
        .unwrap();

    let r1 = rows(
        engine
            .execute_powql(&format!(r#"T filter .id = uuid("{UUID1}") {{ .id }}"#))
            .unwrap(),
    );
    assert_eq!(r1.len(), 1);
    assert_eq!(r1[0][0], Value::Uuid(UUID1_BYTES));

    // Second call, same shape → cache hit → must match the *second* uuid.
    let r2 = rows(
        engine
            .execute_powql(&format!(r#"T filter .id = uuid("{UUID2}") {{ .id }}"#))
            .unwrap(),
    );
    assert_eq!(r2.len(), 1);
    assert_eq!(
        r2[0][0],
        Value::Uuid([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2])
    );
}

/// A prepared insert into a uuid column stores a real `Value::Uuid` (the
/// InsertFast guard forces the coercing generic path).
#[test]
fn prepared_insert_stores_real_uuid() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine.execute_powql("type T { id: uuid }").unwrap();

    let prep = engine
        .prepare(r#"insert T { id := "00000000-0000-0000-0000-000000000000" }"#)
        .unwrap();
    engine
        .execute_prepared(&prep, &[Literal::String(UUID1.into())])
        .unwrap();

    let r = rows(engine.execute_powql("T { .id }").unwrap());
    assert_eq!(r.len(), 1);
    assert_eq!(
        r[0][0],
        Value::Uuid(UUID1_BYTES),
        "prepared insert must store a typed uuid, not a raw string"
    );
}

/// SQL frontend: `CREATE TABLE` with uuid + bytea, insert via SQL string
/// literals, then a repeated query to confirm the cache path stays correct.
#[test]
fn sql_frontend_uuid_bytea_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_sql("CREATE TABLE t (id uuid, blob bytea)")
        .unwrap();
    // The SQL lexer treats a single backslash as a C-style escape, so a
    // bytea literal doubles it: '\\x…' lexes to the value `\x…`.
    engine
        .execute_sql(&format!(
            r#"INSERT INTO t (id, blob) VALUES ('{UUID1}', '\\xdeadbeef')"#
        ))
        .unwrap();
    engine
        .execute_sql(&format!(
            r#"INSERT INTO t (id, blob) VALUES ('{UUID2}', '\\x00')"#
        ))
        .unwrap();

    for _ in 0..2 {
        let r = rows(engine.execute_sql("SELECT id, blob FROM t").unwrap());
        assert_eq!(r.len(), 2);
    }
    let r = rows(
        engine
            .execute_powql(&format!(r#"t filter .id = uuid("{UUID1}") {{ .blob }}"#))
            .unwrap(),
    );
    assert_eq!(r[0][0], Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
}

/// uuid/bytes rows survive a crash + WAL replay (reopen without checkpoint).
#[test]
fn wal_replay_durability_of_uuid_bytes() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type T { id: uuid, blob: bytes }")
            .unwrap();
        engine
            .execute_powql(&format!(
                r#"insert T {{ id := "{UUID1}", blob := "\\xcafe" }}"#
            ))
            .unwrap();
        // Drop the engine without an explicit checkpoint — recovery must
        // replay the WAL.
    }

    let mut engine = Engine::new(dir.path()).unwrap();
    let r = rows(engine.execute_powql("T { .id, .blob }").unwrap());
    assert_eq!(r.len(), 1, "uuid/bytes row must survive WAL replay");
    assert_eq!(r[0][0], Value::Uuid(UUID1_BYTES));
    assert_eq!(r[0][1], Value::Bytes(vec![0xca, 0xfe]));
}
