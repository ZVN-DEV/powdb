use super::*;

// ── Mission C Phase 5: prepared statements ────────────────────

#[test]
fn test_prepared_insert_reuses_template() {
    let mut engine = test_engine();
    let prep = engine
        .prepare(r#"insert User { name := "seed", email := "seed@ex.com", age := 0 }"#)
        .expect("prepare");
    // The template has 3 literal slots: name, email, age.
    assert_eq!(prep.param_count, 3);

    for i in 0..5 {
        engine
            .execute_prepared(
                &prep,
                &[
                    Literal::String(format!("user{i}")),
                    Literal::String(format!("u{i}@ex.com")),
                    Literal::Int(20 + i as i64),
                ],
            )
            .expect("execute_prepared");
    }

    // 3 seeded + 5 prepared inserts = 8 rows.
    let count = engine.execute_powql("count(User)").unwrap();
    match count {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 8),
        _ => panic!("expected scalar"),
    }

    // Reused scratch buffers must preserve each execution's values rather
    // than leaking the previous or final row across inserts.
    for i in 0..5 {
        let result = engine
            .execute_powql(&format!(
                r#"User filter .email = "u{i}@ex.com" {{ name, email, age }}"#
            ))
            .unwrap();
        match result {
            QueryResult::Rows { rows, .. } => assert_eq!(
                rows,
                vec![vec![
                    Value::Str(format!("user{i}")),
                    Value::Str(format!("u{i}@ex.com")),
                    Value::Int(20 + i as i64),
                ]]
            ),
            _ => panic!("expected rows"),
        }
    }
}

#[test]
fn prepared_insert_scratch_handles_large_values_shape_changes_and_errors() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "powdb_prepared_scratch_{}_{}",
        std::process::id(),
        id
    ));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Contact { required name: str, required unique email: str, note: str }")
        .unwrap();
    let full = engine
        .prepare(r#"insert Contact { name := "", email := "", note := "" }"#)
        .unwrap();

    let large = "x".repeat(powdb_storage::page::MAX_ROW_DATA_SIZE * 2);
    engine
        .execute_prepared(
            &full,
            &[
                Literal::String(large),
                Literal::String("large@example.com".into()),
                Literal::String("large".into()),
            ],
        )
        .unwrap();
    assert!(engine.insert_values_scratch.iter().all(|value| {
        !matches!(value, Value::Str(buffer) if buffer.capacity() > powdb_storage::page::MAX_ROW_DATA_SIZE)
    }));

    engine
        .execute_prepared(
            &full,
            &[
                Literal::String("short".into()),
                Literal::String("short@example.com".into()),
                Literal::String("ok".into()),
            ],
        )
        .unwrap();
    assert!(engine
        .execute_prepared(
            &full,
            &[
                Literal::String("duplicate".into()),
                Literal::String("short@example.com".into()),
                Literal::String("rejected".into()),
            ],
        )
        .is_err());
    engine
        .execute_prepared(
            &full,
            &[
                Literal::String("after-error".into()),
                Literal::String("after@example.com".into()),
                Literal::String("recovered".into()),
            ],
        )
        .unwrap();

    let partial = engine
        .prepare(r#"insert Contact { name := "", email := "" }"#)
        .unwrap();
    engine
        .execute_prepared(
            &partial,
            &[
                Literal::String("partial".into()),
                Literal::String("partial@example.com".into()),
            ],
        )
        .unwrap();
    let result = engine
        .execute_powql(r#"Contact filter .email = "partial@example.com" { name, email, note }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => assert_eq!(
            rows,
            vec![vec![
                Value::Str("partial".into()),
                Value::Str("partial@example.com".into()),
                Value::Empty,
            ]]
        ),
        _ => panic!("expected rows"),
    }
}

#[test]
fn prepared_insert_fast_path_preserves_schema_and_coercion_semantics() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "powdb_prepared_schema_semantics_{}_{}",
        std::process::id(),
        id
    ));
    let mut engine = Engine::new(&dir).unwrap();

    // Defaults and auto columns deliberately decline the direct slot fast
    // path. Prepared execution must retain the generic INSERT semantics.
    engine
        .execute_powql(
            r#"type Generated { unique auto id: int, required name: str, status: str default "new", score: float }"#,
        )
        .unwrap();
    let generated = engine
        .prepare(r#"insert Generated { name := "", score := 0 }"#)
        .unwrap();
    engine
        .execute_prepared(
            &generated,
            &[Literal::String("first".into()), Literal::Int(7)],
        )
        .unwrap();
    let QueryResult::Rows { rows, .. } = engine
        .execute_powql("Generated { .id, .name, .status, .score }")
        .unwrap()
    else {
        panic!("expected generated row");
    };
    assert_eq!(
        rows,
        vec![vec![
            Value::Int(1),
            Value::Str("first".into()),
            Value::Str("new".into()),
            Value::Float(7.0),
        ]]
    );

    // A simple all-literal insert remains eligible, but runtime values still
    // use generic coercion rules in both borrowed and take execution modes.
    engine
        .execute_powql("type Plain { required id: int, required score: float, note: str }")
        .unwrap();
    let plain = engine
        .prepare(r#"insert Plain { id := 0, score := 0, note := "" }"#)
        .unwrap();
    engine
        .execute_prepared(
            &plain,
            &[
                Literal::Int(1),
                Literal::Int(9),
                Literal::String("borrowed".into()),
            ],
        )
        .unwrap();
    let mut taken = [
        Literal::Int(2),
        Literal::Int(11),
        Literal::String("taken".into()),
    ];
    engine.execute_prepared_take(&plain, &mut taken).unwrap();

    let mismatch = engine.execute_prepared(
        &plain,
        &[
            Literal::String("not-an-int".into()),
            Literal::Int(1),
            Literal::String("bad".into()),
        ],
    );
    assert!(
        mismatch.unwrap_err().to_string().contains("expected Int"),
        "runtime prepared values must be type checked"
    );

    let QueryResult::Rows { rows, .. } = engine
        .execute_powql("Plain order .id { .id, .score, .note }")
        .unwrap()
    else {
        panic!("expected plain rows");
    };
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Int(1),
                Value::Float(9.0),
                Value::Str("borrowed".into()),
            ],
            vec![
                Value::Int(2),
                Value::Float(11.0),
                Value::Str("taken".into()),
            ],
        ]
    );

    // Recreating the same name and column schema in the same numeric slot can
    // still change defaults/auto metadata, which ColumnDef alone does not
    // encode. The catalog generation must force this stale prepared insert
    // through the live generic schema contract.
    engine
        .execute_powql("type Reused { required unique id: int, status: str }")
        .unwrap();
    let reused_insert = engine.prepare("insert Reused { id := 0 }").unwrap();
    engine.execute_powql("drop Reused").unwrap();
    engine
        .execute_powql(r#"type Reused { required unique id: int, status: str default "fresh" }"#)
        .unwrap();
    engine
        .execute_prepared(&reused_insert, &[Literal::Int(9)])
        .unwrap();
    let QueryResult::Rows { rows, .. } = engine.execute_powql("Reused { .id, .status }").unwrap()
    else {
        panic!("expected recreated row");
    };
    assert_eq!(rows, vec![vec![Value::Int(9), Value::Str("fresh".into())]]);
}

#[test]
fn prepared_fast_paths_revalidate_catalog_identity_schema_and_indexes() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "powdb_prepared_revalidation_{}_{}",
        std::process::id(),
        id
    ));
    let mut engine = Engine::new(&dir).unwrap();

    engine
        .execute_powql("type First { required unique id: int, score: int }")
        .unwrap();
    engine
        .execute_powql("type Survivor { required unique id: int, score: int }")
        .unwrap();
    engine
        .execute_powql("insert Survivor { id := 7, score := 70 }")
        .unwrap();
    let stale_insert = engine
        .prepare("insert First { id := 0, score := 0 }")
        .unwrap();
    let stale_update = engine
        .prepare("First filter .id = 0 update { score := 0 }")
        .unwrap();
    engine.execute_powql("drop First").unwrap();
    assert!(engine
        .execute_prepared(&stale_insert, &[Literal::Int(1), Literal::Int(10)])
        .is_err());
    assert!(engine
        .execute_prepared(&stale_update, &[Literal::Int(1), Literal::Int(10)])
        .is_err());
    match engine.execute_powql("count(Survivor)").unwrap() {
        QueryResult::Scalar(Value::Int(1)) => {}
        other => {
            panic!("a swap-moved table must never receive a stale prepared mutation: {other:?}")
        }
    }

    engine
        .execute_powql("type Altered { required unique id: int, score: int }")
        .unwrap();
    let altered_insert = engine
        .prepare("insert Altered { id := 0, score := 0 }")
        .unwrap();
    engine
        .execute_powql("alter Altered add column note: str")
        .unwrap();
    engine
        .execute_prepared(&altered_insert, &[Literal::Int(1), Literal::Int(5)])
        .unwrap();
    let QueryResult::Rows { rows, .. } = engine
        .execute_powql("Altered { .id, .score, .note }")
        .unwrap()
    else {
        panic!("expected altered row");
    };
    assert_eq!(rows, vec![vec![Value::Int(1), Value::Int(5), Value::Empty]]);

    engine
        .execute_powql("type IndexedTarget { required unique id: int, score: int }")
        .unwrap();
    engine
        .execute_powql("insert IndexedTarget { id := 1, score := 10 }, { id := 2, score := 20 }")
        .unwrap();
    let indexed_update = engine
        .prepare("IndexedTarget filter .id = 0 update { score := 0 }")
        .unwrap();
    engine
        .execute_powql("alter IndexedTarget add unique .score")
        .unwrap();
    assert!(
        engine
            .execute_prepared(&indexed_update, &[Literal::Int(2), Literal::Int(10)])
            .is_err(),
        "a post-prepare unique index must disable the byte-patch fast path"
    );
    let QueryResult::Rows { rows, .. } = engine
        .execute_powql("IndexedTarget order .id { .id, .score }")
        .unwrap()
    else {
        panic!("expected indexed rows");
    };
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(2), Value::Int(20)],
        ]
    );
}

#[test]
fn row_only_rollback_preserves_prepared_structure_generation() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "powdb_prepared_rollback_generation_{}_{}",
        std::process::id(),
        id
    ));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Stable { required id: int, value: str }")
        .unwrap();
    engine
        .catalog_mut()
        .get_table_mut("Stable")
        .unwrap()
        .insert(&vec![Value::Int(0), Value::Str("seed".into())])
        .unwrap();
    engine
        .catalog_mut()
        .create_index_unique("Stable", "id", true)
        .unwrap();
    engine.catalog_mut().checkpoint().unwrap();

    let prepared = engine
        .prepare(r#"insert Stable { id := 0, value := "" }"#)
        .unwrap();
    let before = engine.catalog().structure_generation();
    engine.execute_powql("begin").unwrap();
    engine
        .execute_prepared(
            &prepared,
            &[Literal::Int(1), Literal::String("rolled back".into())],
        )
        .unwrap();
    engine.execute_powql("rollback").unwrap();

    assert_eq!(
        engine.catalog().structure_generation(),
        before,
        "row-only rollback must keep prepared metadata valid"
    );
    engine
        .execute_prepared(
            &prepared,
            &[Literal::Int(2), Literal::String("committed".into())],
        )
        .unwrap();
    let QueryResult::Rows { rows, .. } = engine.execute_powql("Stable { .id, .value }").unwrap()
    else {
        panic!("expected stable row");
    };
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(0), Value::Str("seed".into())],
            vec![Value::Int(2), Value::Str("committed".into())],
        ]
    );
}

#[test]
fn execute_prepared_take_restores_strings_after_insert_error() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "powdb_prepared_take_restore_{}_{}",
        std::process::id(),
        id
    ));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Contact { required unique email: str, name: str }")
        .unwrap();
    let prepared = engine
        .prepare(r#"insert Contact { email := "", name := "" }"#)
        .unwrap();
    let mut first = [
        Literal::String("same@example.com".into()),
        Literal::String("first".into()),
    ];
    engine.execute_prepared_take(&prepared, &mut first).unwrap();
    assert!(matches!(&first[0], Literal::String(value) if value.is_empty()));

    let mut duplicate = [
        Literal::String("same@example.com".into()),
        Literal::String("duplicate".into()),
    ];
    assert!(engine
        .execute_prepared_take(&prepared, &mut duplicate)
        .is_err());
    assert_eq!(
        duplicate,
        [
            Literal::String("same@example.com".into()),
            Literal::String("duplicate".into()),
        ],
        "failed take execution must restore caller-owned strings"
    );
}

#[test]
fn test_prepared_update_by_pk() {
    let mut engine = test_engine();
    let prep = engine
        .prepare(r#"User filter .name = "seed" update { age := 0 }"#)
        .expect("prepare");
    // Two slots: filter literal "seed" + assignment literal 0.
    assert_eq!(prep.param_count, 2);

    engine
        .execute_prepared(&prep, &[Literal::String("Alice".into()), Literal::Int(99)])
        .expect("execute_prepared");

    let result = engine
        .execute_powql(r#"User filter .name = "Alice" { age }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Int(99));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_prepared_wrong_arity_errors() {
    let mut engine = test_engine();
    let prep = engine
        .prepare(r#"User filter .age > 0 { name }"#)
        .expect("prepare");
    assert_eq!(prep.param_count, 1);
    let err = engine.execute_prepared(&prep, &[]).unwrap_err();
    assert!(err.to_string().contains("expects 1 literal"));
}
