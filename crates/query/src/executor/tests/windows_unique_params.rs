use super::*;

// ---------------------------------------------------------------------------
// Window aggregates without `order`: frame must be the ENTIRE partition,
// not a running prefix. (`avg(.sal) over (partition .dept)` used to return
// 10/15/20 for salaries 10/20/30 instead of 20/20/20.)
// ---------------------------------------------------------------------------

fn window_engine() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_win_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Emp { required name: str, required dept: str, required sal: int }")
        .unwrap();
    for (name, dept, sal) in [
        ("a", "eng", 10),
        ("b", "eng", 20),
        ("c", "eng", 30),
        ("d", "ops", 100),
        ("e", "ops", 300),
    ] {
        engine
            .execute_powql(&format!(
                r#"insert Emp {{ name := "{name}", dept := "{dept}", sal := {sal} }}"#
            ))
            .unwrap();
    }
    engine
}

/// Extract (name → window value) pairs from a two-column result.
fn window_col_by_name(result: QueryResult) -> std::collections::HashMap<String, Value> {
    match result {
        QueryResult::Rows { columns, rows } => {
            let name_idx = columns.iter().position(|c| c == "name").unwrap();
            let win_idx = columns.len() - 1;
            rows.into_iter()
                .map(|r| {
                    let name = match &r[name_idx] {
                        Value::Str(s) => s.clone(),
                        other => panic!("expected name string, got {other:?}"),
                    };
                    (name, r[win_idx].clone())
                })
                .collect()
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_window_agg_without_order_uses_whole_partition() {
    let mut engine = window_engine();
    let result = engine
        .execute_powql("Emp { .name, davg: avg(.sal) over (partition .dept) }")
        .unwrap();
    let by_name = window_col_by_name(result);
    // eng partition avg = (10+20+30)/3 = 20 for EVERY row.
    for n in ["a", "b", "c"] {
        assert_eq!(by_name[n], Value::Float(20.0), "row {n}");
    }
    // ops partition avg = (100+300)/2 = 200 for both rows.
    for n in ["d", "e"] {
        assert_eq!(by_name[n], Value::Float(200.0), "row {n}");
    }
}

#[test]
fn test_window_sum_count_min_max_without_order_whole_partition() {
    let mut engine = window_engine();

    let by_name = window_col_by_name(
        engine
            .execute_powql("Emp { .name, dsum: sum(.sal) over (partition .dept) }")
            .unwrap(),
    );
    for n in ["a", "b", "c"] {
        assert_eq!(by_name[n], Value::Int(60), "sum row {n}");
    }
    for n in ["d", "e"] {
        assert_eq!(by_name[n], Value::Int(400), "sum row {n}");
    }

    let by_name = window_col_by_name(
        engine
            .execute_powql("Emp { .name, dcnt: count(.sal) over (partition .dept) }")
            .unwrap(),
    );
    for n in ["a", "b", "c"] {
        assert_eq!(by_name[n], Value::Int(3), "count row {n}");
    }

    let by_name = window_col_by_name(
        engine
            .execute_powql("Emp { .name, dmin: min(.sal) over (partition .dept) }")
            .unwrap(),
    );
    for n in ["a", "b", "c"] {
        assert_eq!(by_name[n], Value::Int(10), "min row {n}");
    }

    let by_name = window_col_by_name(
        engine
            .execute_powql("Emp { .name, dmax: max(.sal) over (partition .dept) }")
            .unwrap(),
    );
    for n in ["a", "b", "c"] {
        assert_eq!(by_name[n], Value::Int(30), "max row {n}");
    }
    for n in ["d", "e"] {
        assert_eq!(by_name[n], Value::Int(300), "max row {n}");
    }
}

#[test]
fn test_window_agg_with_order_keeps_running_frame() {
    // WITH an explicit `order`, the running (rows-so-far) frame is the
    // existing documented behavior — it must not change.
    let mut engine = window_engine();
    let by_name = window_col_by_name(
        engine
            .execute_powql("Emp { .name, ravg: avg(.sal) over (partition .dept order .sal) }")
            .unwrap(),
    );
    assert_eq!(by_name["a"], Value::Float(10.0));
    assert_eq!(by_name["b"], Value::Float(15.0));
    assert_eq!(by_name["c"], Value::Float(20.0));
}

// ---------------------------------------------------------------------------
// UNIQUE constraint enforcement (Task 3)
// ---------------------------------------------------------------------------

fn unique_engine() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_uniq_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Acct { required unique email: str, id: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert Acct { email := "a@x.com", id := 1 }"#)
        .unwrap();
    engine
}

#[test]
fn test_unique_dup_insert_rejected() {
    let mut engine = unique_engine();
    let err = engine
        .execute_powql(r#"insert Acct { email := "a@x.com", id := 2 }"#)
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("unique constraint violation on Acct.email"),
        "{err}"
    );
    match engine.execute_powql("count(Acct)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
        other => panic!("expected scalar, got {other:?}"),
    }
}

#[test]
fn test_unique_update_into_dup_rejected() {
    let mut engine = unique_engine();
    engine
        .execute_powql(r#"insert Acct { email := "b@x.com", id := 2 }"#)
        .unwrap();
    let err = engine
        .execute_powql(r#"Acct filter .id = 2 update { email := "a@x.com" }"#)
        .unwrap_err();
    assert!(
        err.to_string().contains("unique constraint violation"),
        "{err}"
    );
    // The losing row keeps its own value (rolled back / never applied).
    match engine
        .execute_powql(r#"Acct filter .email = "b@x.com" { .id }"#)
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_unique_update_to_same_value_allowed() {
    let mut engine = unique_engine();
    // Updating a unique column to its own current value must NOT trip the
    // constraint (existing rid == self).
    engine
        .execute_powql(r#"Acct filter .id = 1 update { email := "a@x.com", id := 9 }"#)
        .unwrap();
    match engine.execute_powql("count(Acct)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
        other => panic!("expected scalar, got {other:?}"),
    }
}

#[test]
fn test_upsert_requires_unique_and_no_dup_ids() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_ups_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type W { unique id: int, v: str }")
        .unwrap();
    engine
        .execute_powql(r#"upsert W on .id { id := 1, v := "first" }"#)
        .unwrap();
    // Known bug regression: a plain insert of the same id must now fail
    // instead of silently creating a second id=1 row.
    assert!(engine
        .execute_powql(r#"insert W { id := 1, v := "second" }"#)
        .is_err());
    engine
        .execute_powql(r#"upsert W on .id { id := 1, v := "third" }"#)
        .unwrap();
    match engine.execute_powql("count(W)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
        other => panic!("expected scalar, got {other:?}"),
    }
    // upsert on a NON-unique column is a clean error.
    engine.execute_powql("type W2 { id: int }").unwrap();
    let err = engine
        .execute_powql("upsert W2 on .id { id := 1 }")
        .unwrap_err();
    assert!(
        err.to_string().contains("requires a unique column"),
        "{err}"
    );
}

#[test]
fn test_alter_add_unique_fails_on_existing_dups() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_audup_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine.execute_powql("type L { e: str }").unwrap();
    engine.execute_powql(r#"insert L { e := "x" }"#).unwrap();
    engine.execute_powql(r#"insert L { e := "x" }"#).unwrap();
    assert!(engine.execute_powql("alter L add unique .e").is_err());
}

#[test]
fn test_alter_add_unique_succeeds_then_enforces() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_au_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine.execute_powql("type L { e: str }").unwrap();
    engine.execute_powql(r#"insert L { e := "x" }"#).unwrap();
    engine.execute_powql(r#"insert L { e := "y" }"#).unwrap();
    engine.execute_powql("alter L add unique .e").unwrap();
    // Now enforced on subsequent inserts.
    assert!(engine.execute_powql(r#"insert L { e := "x" }"#).is_err());
    // Adding unique on an already-indexed column is a clean error.
    let err = engine.execute_powql("alter L add unique .e").unwrap_err();
    assert!(err.to_string().contains("already indexed"), "{err}");
}

#[test]
fn test_unique_constraint_survives_reopen() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_uniq_re_{}_{}", std::process::id(), id));
    {
        let mut engine = Engine::new(&dir).unwrap();
        engine
            .execute_powql("type Acct { required unique email: str }")
            .unwrap();
        engine
            .execute_powql(r#"insert Acct { email := "a@x.com" }"#)
            .unwrap();
        // Dropped here without explicit checkpoint — recovery path must
        // restore the unique flag from catalog.bin + WAL replay.
    }
    let mut engine = Engine::new(&dir).unwrap();
    assert!(engine
        .execute_powql(r#"insert Acct { email := "a@x.com" }"#)
        .is_err());
}

// ---------------------------------------------------------------------------
// Task 4: wire parameter binding ($1..$N), token-level substitution.
// ---------------------------------------------------------------------------

#[test]
fn test_params_bind_injection_shaped_strings_byte_faithfully() {
    use crate::ast::ParamValue;
    let mut engine = test_engine();
    let evil = r#"x"; drop User; filter .age > "0"#;
    engine
        .execute_powql_with_params(
            "insert User { name := $1, email := $2, age := $3 }",
            &[
                ParamValue::Str(evil.to_string()),
                ParamValue::Str("e@x.com".into()),
                ParamValue::Int(40),
            ],
        )
        .unwrap();
    let r = engine
        .execute_powql_with_params(
            "User filter .email = $1 { .name }",
            &[ParamValue::Str("e@x.com".into())],
        )
        .unwrap();
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str(evil.to_string()));
        }
        other => panic!("expected rows, got {other:?}"),
    }
    // Table survived; 4 rows total.
    match engine.execute_powql("count(User)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 4),
        other => panic!("{other:?}"),
    }
}

#[test]
fn test_params_errors() {
    use crate::ast::ParamValue;
    let mut engine = test_engine();
    // Out-of-range placeholder is a clean error.
    assert!(engine
        .execute_powql_with_params("User filter .age > $2", &[ParamValue::Int(1)])
        .is_err());
    // The no-params API rejects an unbound placeholder.
    assert!(engine.execute_powql("User filter .age > $1").is_err());
    // Null param round-trips as PowQL null.
    engine
        .execute_powql_with_params(
            "insert User { name := $1, email := $2, age := $3 }",
            &[
                ParamValue::Str("N".into()),
                ParamValue::Str("n@x.com".into()),
                ParamValue::Null,
            ],
        )
        .unwrap();
    match engine
        .execute_powql("User filter .age = null { .name }")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("{other:?}"),
    }
}

#[test]
fn test_params_all_types_round_trip() {
    use crate::ast::ParamValue;
    let mut engine = test_engine();
    engine
        .execute_powql("type Mix { required name: str, n: int, f: float, ok: bool }")
        .unwrap();
    engine
        .execute_powql_with_params(
            "insert Mix { name := $1, n := $2, f := $3, ok := $4 }",
            &[
                ParamValue::Str("row".into()),
                ParamValue::Int(-7),
                ParamValue::Float(2.5),
                ParamValue::Bool(true),
            ],
        )
        .unwrap();
    match engine
        .execute_powql("Mix filter .n = -7 { .name }")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("{other:?}"),
    }
}

#[test]
fn test_params_readonly_path() {
    use crate::ast::ParamValue;
    let engine = {
        let mut e = test_engine();
        // mutate up front, then exercise the readonly param path on &self.
        e.execute_powql_with_params(
            "insert User { name := $1, email := $2, age := $3 }",
            &[
                ParamValue::Str("Zed".into()),
                ParamValue::Str("z@x.com".into()),
                ParamValue::Int(99),
            ],
        )
        .unwrap();
        e
    };
    let r = engine
        .execute_powql_readonly_with_params(
            "User filter .name = $1 { .age }",
            &[ParamValue::Str("Zed".into())],
        )
        .unwrap();
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Int(99));
        }
        other => panic!("{other:?}"),
    }
    // A write statement through the readonly param path escalates.
    assert!(matches!(
        engine.execute_powql_readonly_with_params(
            "insert User { name := $1, email := $2 }",
            &[ParamValue::Str("a".into()), ParamValue::Str("b".into())],
        ),
        Err(crate::result::QueryError::ReadonlyNeedsWrite)
    ));
}

#[test]
fn test_no_params_regression_path_unchanged() {
    let mut engine = test_engine();
    // Plain queries with no placeholders still work identically.
    match engine
        .execute_powql("User filter .age > 26 { .name }")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 2),
        other => panic!("{other:?}"),
    }
}
