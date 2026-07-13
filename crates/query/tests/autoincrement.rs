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

/// Regression (BUG 1): rolling back the *same* reused auto-id twice must not
/// leave a phantom entry in the unique index. The first rollback used to flush
/// the uncommitted index mutation to the on-disk `.idx` (via the dropped
/// catalog's Drop→checkpoint), and the second rollback of the same reused id
/// reloaded that poisoned `.idx` back into the live tree — permanently
/// poisoning the id. See rollback_to_last_sync_inner.
#[test]
fn double_rollback_of_reused_auto_id_does_not_poison_index() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type D { unique auto id: int, required v: int }")
        .unwrap();

    // id=1 committed
    engine.execute_powql("insert D { v := 1 }").unwrap();

    // begin/insert(id=2)/rollback — id 2 goes back to the sequence
    engine.execute_powql("begin").unwrap();
    engine.execute_powql("insert D { v := 2 }").unwrap();
    engine.execute_powql("rollback").unwrap();

    // reuse id=2, committed (must succeed)
    engine.execute_powql("insert D { v := 3 }").unwrap();

    // begin/insert(id=3)/rollback — first rollback of id 3
    engine.execute_powql("begin").unwrap();
    engine.execute_powql("insert D { v := 4 }").unwrap();
    engine.execute_powql("rollback").unwrap();

    // begin/insert(id=3)/rollback — SECOND rollback of the same reused id 3
    engine.execute_powql("begin").unwrap();
    engine.execute_powql("insert D { v := 5 }").unwrap();
    engine.execute_powql("rollback").unwrap();

    // reuse id=3, committed — this used to fail with a phantom unique violation
    engine
        .execute_powql("insert D { v := 6 }")
        .expect("reusing rolled-back auto id 3 must not hit a phantom unique violation");

    // The live table holds exactly ids {1, 2, 3}, all with the committed values.
    match engine.execute_powql("D order .id { .id, .v }").unwrap() {
        QueryResult::Rows { rows, .. } => {
            let got: Vec<(i64, i64)> = rows
                .iter()
                .map(|r| match (&r[0], &r[1]) {
                    (Value::Int(id), Value::Int(v)) => (*id, *v),
                    other => panic!("expected (int,int), got {other:?}"),
                })
                .collect();
            assert_eq!(
                got,
                vec![(1, 1), (2, 3), (3, 6)],
                "unexpected surviving rows"
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }

    // The unique index on .id must point at the committed row, not a phantom.
    let looked_up = one_int(engine.execute_powql("D filter .id = 3 { .v }").unwrap());
    assert_eq!(looked_up, 6, "index lookup on .id returned the wrong row");
}

/// Regression (BUG 1): the poison must not survive a reopen either — the
/// on-disk `.idx` file itself must be clean after the fix.
#[test]
fn double_rollback_poison_absent_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type D { unique auto id: int, required v: int }")
            .unwrap();
        engine.execute_powql("insert D { v := 1 }").unwrap();
        engine.execute_powql("begin").unwrap();
        engine.execute_powql("insert D { v := 2 }").unwrap();
        engine.execute_powql("rollback").unwrap();
        engine.execute_powql("insert D { v := 3 }").unwrap();
        engine.execute_powql("begin").unwrap();
        engine.execute_powql("insert D { v := 4 }").unwrap();
        engine.execute_powql("rollback").unwrap();
        engine.execute_powql("begin").unwrap();
        engine.execute_powql("insert D { v := 5 }").unwrap();
        engine.execute_powql("rollback").unwrap();
    }
    // Reopen: the .idx on disk must not carry a phantom key 3.
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("insert D { v := 6 }")
        .expect("phantom unique entry survived reopen");
    let looked_up = one_int(engine.execute_powql("D filter .id = 3 { .v }").unwrap());
    assert_eq!(
        looked_up, 6,
        "index lookup after reopen returned the wrong row"
    );
}

/// A single rollback of a fresh auto-id must still cleanly remove its index
/// entry (guards against an over-broad fix that leaks the id forward).
#[test]
fn single_rollback_cleanly_removes_index_entry() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type D { unique auto id: int, required v: int }")
        .unwrap();
    engine.execute_powql("insert D { v := 1 }").unwrap();

    engine.execute_powql("begin").unwrap();
    engine.execute_powql("insert D { v := 2 }").unwrap();
    engine.execute_powql("rollback").unwrap();

    // Reused id=2 must insert cleanly and the index must find only it.
    engine
        .execute_powql("insert D { v := 3 }")
        .expect("single rollback left a phantom index entry");
    let looked_up = one_int(engine.execute_powql("D filter .id = 2 { .v }").unwrap());
    assert_eq!(looked_up, 3);
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
