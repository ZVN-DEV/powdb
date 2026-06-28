use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn one_int(result: QueryResult) -> i64 {
    match result {
        QueryResult::Rows { rows, .. } => match rows[0][0] {
            Value::Int(n) => n,
            ref v => panic!("expected int, got {v:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

/// An `auto` int column assigns sequential ids when omitted, the assigned id
/// comes back via `returning`, and the sequence keeps climbing after a restart.
#[test]
fn powql_auto_assigns_sequential_ids_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type T { unique auto id: int, required name: str }")
            .unwrap();
        let id1 = one_int(
            engine
                .execute_powql(r#"insert T { name := "a" } returning"#)
                .unwrap(),
        );
        let id2 = one_int(
            engine
                .execute_powql(r#"insert T { name := "b" } returning"#)
                .unwrap(),
        );
        assert_eq!(id1, 1, "first auto id");
        assert_eq!(id2, 2, "second auto id");
    }

    // Reopen: the sequence must resume above the highest persisted id, not
    // restart at 1 (which would collide with the unique constraint).
    let mut engine = Engine::new(dir.path()).unwrap();
    let id3 = one_int(
        engine
            .execute_powql(r#"insert T { name := "c" } returning"#)
            .unwrap(),
    );
    assert_eq!(id3, 3, "sequence resumes after restart");

    match engine.execute_powql("T order .id { .id, .name }").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0], vec![Value::Int(1), Value::Str("a".into())]);
            assert_eq!(rows[1], vec![Value::Int(2), Value::Str("b".into())]);
            assert_eq!(rows[2], vec![Value::Int(3), Value::Str("c".into())]);
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// After a hard crash (no checkpoint), the auto sequence resumes above the
/// rows recovered from the WAL — not from a stale persisted counter (there is
/// none; it is recomputed from the recovered data).
#[test]
fn auto_sequence_recovers_after_crash() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type T { unique auto id: int, required n: str }")
            .unwrap();
        // ids 1 and 2, committed to the WAL but not checkpointed.
        engine
            .execute_powql(r#"insert T { n := "a" }, { n := "b" }"#)
            .unwrap();
        std::mem::forget(engine); // hard crash: skip the clean checkpoint
    }
    let mut engine = Engine::new(dir.path()).unwrap();
    let id = one_int(
        engine
            .execute_powql(r#"insert T { n := "c" } returning"#)
            .unwrap(),
    );
    assert_eq!(id, 3, "sequence resumes above the WAL-replayed rows");
}

/// SQL `AUTOINCREMENT` column modifier lowers to the same auto mechanism.
#[test]
fn sql_autoincrement_assigns_ids() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_sql("CREATE TABLE T (id INTEGER UNIQUE AUTOINCREMENT, name TEXT NOT NULL)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO T (name) VALUES ('a')")
        .unwrap();
    engine
        .execute_sql("INSERT INTO T (name) VALUES ('b')")
        .unwrap();
    match engine.execute_sql("SELECT id FROM T ORDER BY id").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Int(1));
            assert_eq!(rows[1][0], Value::Int(2));
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// A multi-row insert assigns a distinct, sequential id to each row.
#[test]
fn multi_row_insert_assigns_distinct_ids() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { unique auto id: int, required n: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert T { n := "a" }, { n := "b" }, { n := "c" }"#)
        .unwrap();
    match engine.execute_powql("T order .id { .id }").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0][0], Value::Int(1));
            assert_eq!(rows[1][0], Value::Int(2));
            assert_eq!(rows[2][0], Value::Int(3));
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// An explicitly-provided value pushes the sequence past it, so a later
/// auto-assigned id never collides with the chosen one.
#[test]
fn explicit_value_advances_the_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { unique auto id: int, required n: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert T { id := 100, n := "explicit" }"#)
        .unwrap();
    let next = one_int(
        engine
            .execute_powql(r#"insert T { n := "auto" } returning"#)
            .unwrap(),
    );
    assert_eq!(next, 101, "sequence resumes above the explicit value");
}

/// `auto` on a non-integer column is rejected at table-creation time.
#[test]
fn auto_on_non_int_column_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    let err = engine
        .execute_powql("type T { auto name: str }")
        .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("int"),
        "auto-on-str should mention the int requirement, got: {err}"
    );
}

/// A column cannot be both `auto` and declare a literal `default`.
#[test]
fn auto_with_default_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    let err = engine
        .execute_powql("type T { auto id: int default 5 }")
        .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("default"),
        "auto+default should be rejected, got: {err}"
    );
}
