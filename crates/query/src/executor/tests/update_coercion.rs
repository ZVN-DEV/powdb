use super::*;

// ── #117 / #118: the in-place UPDATE fast path must coerce the assigned
// value to the target column's declared type before writing fixed bytes ──
//
// `Acct` has a fixed-size `balance: float` (→ byte-patch fast path), an
// indexed `id` (forces the IndexScan update path / plan_exec.rs:913 site) and
// a non-indexed `tag` (forces the fused Filter(SeqScan) path /
// try_fused_scan_update's :2679 site).
fn acct_engine() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_acct_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Acct { required unique id: str, balance: float, tag: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert Acct { id := "a", balance := 1.5, tag := 1 }"#)
        .unwrap();
    engine
}

fn acct_balance(engine: &mut Engine) -> Value {
    match engine
        .execute_powql(r#"Acct filter .id = "a" { .balance }"#)
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => rows[0][0].clone(),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_update_int_into_float_column_coerces_indexed_path() {
    // #118: int assigned to a float column on the indexed fast path must be
    // coerced to f64, not stored as the raw i64 bit pattern (which read back
    // as the denormal ~5e-323).
    let mut engine = acct_engine();
    engine
        .execute_powql(r#"Acct filter .id = "a" update { balance := 10 }"#)
        .unwrap();
    assert_eq!(acct_balance(&mut engine), Value::Float(10.0));
}

#[test]
fn test_update_int_into_float_column_coerces_seqscan_path() {
    // #118 via the fused Filter(SeqScan) path (filter on the non-indexed tag).
    let mut engine = acct_engine();
    engine
        .execute_powql("Acct filter .tag = 1 update { balance := 10 }")
        .unwrap();
    assert_eq!(acct_balance(&mut engine), Value::Float(10.0));
}

#[test]
fn test_update_str_into_float_column_errors_not_panic_indexed_path() {
    // #117: a str assigned to a fixed-size (float) column on the indexed fast
    // path must return a typed error, NOT hit `unreachable!` and abort.
    let mut engine = acct_engine();
    let result = engine.execute_powql(r#"Acct filter .id = "a" update { balance := "oops" }"#);
    assert!(
        result.is_err(),
        "type-mismatched UPDATE must return Err, got {result:?}"
    );
    // The row must be untouched.
    assert_eq!(acct_balance(&mut engine), Value::Float(1.5));
}

#[test]
fn test_update_str_into_float_column_errors_not_panic_seqscan_path() {
    // #117 via the fused Filter(SeqScan) path (try_fused_scan_update).
    let mut engine = acct_engine();
    let result = engine.execute_powql(r#"Acct filter .tag = 1 update { balance := "oops" }"#);
    assert!(
        result.is_err(),
        "type-mismatched UPDATE via seqscan must return Err, got {result:?}"
    );
    assert_eq!(acct_balance(&mut engine), Value::Float(1.5));
}

// ── #117 / #118 redux: the EXPRESSION update path (non-literal RHS) must
// coerce too. #119 only fixed the literal fast path; a non-literal RHS such as
// `balance := .tag + 9` falls through to the per-row eval_expr loop, which wrote
// the raw eval result into the row with no coercion — re-opening both the #118
// (silent float corruption) and #117 (str → fixed-col `unreachable!` abort)
// holes on any computed assignment. These mirror the literal tests above but
// drive the expr path via a column reference. ──

#[test]
fn test_update_int_expr_into_float_column_coerces_plain_path() {
    // #118 via the expression path: `.tag` (int 1) + 9 = int 10 assigned to the
    // float `balance` must be coerced to f64, not stored as the raw i64 bits.
    let mut engine = acct_engine();
    engine
        .execute_powql("Acct filter .id = \"a\" update { balance := .tag + 9 }")
        .unwrap();
    assert_eq!(acct_balance(&mut engine), Value::Float(10.0));
}

#[test]
fn test_update_int_expr_into_float_column_coerces_returning_path() {
    // Same #118 hole on the separate RETURNING expr write site: the returned
    // post-image AND the persisted row must both be the coerced f64.
    let mut engine = acct_engine();
    let result = engine
        .execute_powql("Acct filter .id = \"a\" update { balance := .tag + 9 } returning")
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            let bidx = columns.iter().position(|c| c == "balance").unwrap();
            assert_eq!(
                rows[0][bidx],
                Value::Float(10.0),
                "RETURNING post-image must carry the coerced float, got {:?}",
                rows[0][bidx]
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
    assert_eq!(acct_balance(&mut engine), Value::Float(10.0));
}

#[test]
fn test_update_str_expr_into_float_column_errors_not_panic_plain_path() {
    // #117 via the expression path: `.id` (str "a") assigned to the float
    // `balance` must return a typed error, NOT hit `unreachable!` and abort.
    let mut engine = acct_engine();
    let result = engine.execute_powql("Acct filter .id = \"a\" update { balance := .id }");
    assert!(
        result.is_err(),
        "type-mismatched expr UPDATE must return Err, got {result:?}"
    );
    assert_eq!(acct_balance(&mut engine), Value::Float(1.5));
}

#[test]
fn test_update_str_expr_into_float_column_errors_not_panic_returning_path() {
    // #117 on the RETURNING expr write site.
    let mut engine = acct_engine();
    let result =
        engine.execute_powql("Acct filter .id = \"a\" update { balance := .id } returning");
    assert!(
        result.is_err(),
        "type-mismatched expr UPDATE (returning) must return Err, got {result:?}"
    );
    assert_eq!(acct_balance(&mut engine), Value::Float(1.5));
}

// ── #117 / #118 redux: the UPSERT on-conflict path applies its assignments
// with the same raw write #119 fixed for UPDATE. A type-mismatched literal on
// conflict (`upsert Acct on .id { id := "a", balance := 10 / "oops" }`) must be
// coerced too — otherwise int→float silently corrupts and str→float aborts. ──

#[test]
fn test_upsert_conflict_int_into_float_column_coerces() {
    // #118 on the upsert conflict-update path: int literal 10 applied to the
    // float `balance` must coerce to f64, not store the raw i64 bit pattern.
    let mut engine = acct_engine();
    engine
        .execute_powql(r#"upsert Acct on .id { id := "a", balance := 10 }"#)
        .unwrap();
    assert_eq!(acct_balance(&mut engine), Value::Float(10.0));
}

#[test]
fn test_upsert_conflict_str_into_float_column_errors_not_panic() {
    // #117 on the upsert conflict-update path: str applied to the float
    // `balance` must return a typed error, NOT hit `unreachable!` and abort.
    let mut engine = acct_engine();
    let result = engine.execute_powql(r#"upsert Acct on .id { id := "a", balance := "oops" }"#);
    assert!(
        result.is_err(),
        "type-mismatched upsert must return Err, got {result:?}"
    );
    assert_eq!(acct_balance(&mut engine), Value::Float(1.5));
}

#[test]
fn test_engine_normal_sync_mode_persists_across_reopen() {
    // Phase 1: the engine honors WalSyncMode::Normal end-to-end. In Normal,
    // commits don't fsync per statement (the latency win); a clean shutdown is
    // still durable, so rows survive a reopen.
    use super::WalSyncMode;
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("powdb_engine_normal_{}_{}", std::process::id(), id));
    {
        let mut engine = Engine::new(&dir).unwrap();
        engine.set_wal_sync_mode(WalSyncMode::Normal);
        engine
            .execute_powql("type T { required id: int, required v: int }")
            .unwrap();
        engine
            .execute_powql("insert T { id := 1, v := 100 }")
            .unwrap();
        engine
            .execute_powql("insert T { id := 2, v := 200 }")
            .unwrap();
    } // clean drop → durable
    let mut engine = Engine::new(&dir).unwrap();
    match engine.execute_powql("count(T)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 2),
        other => panic!("expected 2 rows after Normal-mode reopen, got {other:?}"),
    }
}

#[test]
fn test_insert_returning_yields_inserted_row() {
    // RETURNING: `insert ... returning` returns the inserted row(s) as Rows
    // (kills the ORM reselect round-trip).
    let mut engine = test_engine();
    match engine
        .execute_powql(r#"insert User { name := "Dana", email := "d@x.com", age := 40 } returning"#)
        .unwrap()
    {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(rows.len(), 1);
            let name_idx = columns.iter().position(|c| c == "name").expect("name col");
            let age_idx = columns.iter().position(|c| c == "age").expect("age col");
            assert_eq!(rows[0][name_idx], Value::Str("Dana".into()));
            assert_eq!(rows[0][age_idx], Value::Int(40));
        }
        other => panic!("expected Rows from insert ... returning, got {other:?}"),
    }
    // The row is actually persisted (3 seed rows + 1).
    match engine.execute_powql("count(User)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 4),
        other => panic!("{other:?}"),
    }
}

#[test]
fn test_insert_multi_row_returning_yields_all_rows() {
    let mut engine = test_engine();
    match engine
        .execute_powql(
            r#"insert User { name := "A", email := "a2@x.com", age := 1 }, { name := "B", email := "b2@x.com", age := 2 } returning"#,
        )
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 2),
        other => panic!("expected 2 Rows, got {other:?}"),
    }
}

#[test]
fn test_insert_without_returning_still_modified() {
    // Additive: a plain insert (no `returning`) is unchanged.
    let mut engine = test_engine();
    match engine
        .execute_powql(r#"insert User { name := "Eve", email := "e@x.com", age := 50 }"#)
        .unwrap()
    {
        QueryResult::Modified(n) => assert_eq!(n, 1),
        other => panic!("expected Modified, got {other:?}"),
    }
}

#[test]
fn test_update_returning_yields_updated_rows() {
    // RETURNING: `... update { .. } returning` returns the POST-update row(s).
    let mut engine = test_engine();
    match engine
        .execute_powql(r#"User filter .name = "Alice" update { age := 99 } returning"#)
        .unwrap()
    {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(rows.len(), 1);
            let name_idx = columns.iter().position(|c| c == "name").expect("name col");
            let age_idx = columns.iter().position(|c| c == "age").expect("age col");
            assert_eq!(rows[0][name_idx], Value::Str("Alice".into()));
            // Post-image: the new value, not the old 30.
            assert_eq!(rows[0][age_idx], Value::Int(99));
        }
        other => panic!("expected Rows from update ... returning, got {other:?}"),
    }
}

#[test]
fn test_update_returning_expression_path() {
    // The expression-update path (`age := .age + 5`) also returns post-image.
    let mut engine = test_engine();
    match engine
        .execute_powql(r#"User filter .name = "Bob" update { age := .age + 5 } returning"#)
        .unwrap()
    {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(rows.len(), 1);
            let age_idx = columns.iter().position(|c| c == "age").expect("age col");
            assert_eq!(rows[0][age_idx], Value::Int(30)); // Bob was 25
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn test_update_returning_coerces_int_into_float() {
    // #118 guard on the returning path: int → float column must coerce, and the
    // returned post-image must show the coerced float, not raw i64 bits.
    let mut engine = acct_engine();
    match engine
        .execute_powql(r#"Acct filter .id = "a" update { balance := 10 } returning"#)
        .unwrap()
    {
        QueryResult::Rows { columns, rows } => {
            let bal_idx = columns
                .iter()
                .position(|c| c == "balance")
                .expect("balance col");
            assert_eq!(rows[0][bal_idx], Value::Float(10.0));
        }
        other => panic!("expected Rows, got {other:?}"),
    }
    assert_eq!(acct_balance(&mut engine), Value::Float(10.0));
}

#[test]
fn test_update_without_returning_still_modified() {
    // Additive: a plain update (no `returning`) is unchanged.
    let mut engine = test_engine();
    match engine
        .execute_powql(r#"User filter .name = "Alice" update { age := 99 }"#)
        .unwrap()
    {
        QueryResult::Modified(n) => assert_eq!(n, 1),
        other => panic!("expected Modified, got {other:?}"),
    }
}

#[test]
fn test_delete_returning_yields_deleted_rows() {
    // RETURNING: `... delete returning` returns the PRE-delete row(s).
    let mut engine = test_engine();
    match engine
        .execute_powql(r#"User filter .name = "Alice" delete returning"#)
        .unwrap()
    {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(rows.len(), 1);
            let name_idx = columns.iter().position(|c| c == "name").expect("name col");
            assert_eq!(rows[0][name_idx], Value::Str("Alice".into()));
        }
        other => panic!("expected Rows from delete ... returning, got {other:?}"),
    }
    // The row is actually gone (3 seed rows - 1).
    match engine.execute_powql("count(User)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 2),
        other => panic!("{other:?}"),
    }
}

#[test]
fn test_delete_returning_multi_row() {
    // Multiple matches: every deleted row comes back.
    let mut engine = test_engine();
    match engine
        .execute_powql("User filter .age > 28 delete returning")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 2), // Alice(30), Charlie(35)
        other => panic!("expected 2 Rows, got {other:?}"),
    }
    match engine.execute_powql("count(User)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
        other => panic!("{other:?}"),
    }
}

#[test]
fn test_delete_without_returning_still_modified() {
    // Additive: a plain delete (no `returning`) is unchanged.
    let mut engine = test_engine();
    match engine
        .execute_powql(r#"User filter .name = "Alice" delete"#)
        .unwrap()
    {
        QueryResult::Modified(n) => assert_eq!(n, 1),
        other => panic!("expected Modified, got {other:?}"),
    }
}
