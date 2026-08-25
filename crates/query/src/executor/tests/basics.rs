use super::*;

#[test]
fn test_scan_all() {
    let mut engine = test_engine();
    let result = engine.execute_powql("User").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 3),
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_filter() {
    let mut engine = test_engine();
    let result = engine.execute_powql("User filter .age > 28").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2); // Alice (30) and Charlie (35)
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_projection() {
    let mut engine = test_engine();
    let result = engine.execute_powql("User { name }").unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name"]);
            assert_eq!(rows.len(), 3);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_insert_and_count() {
    let mut engine = test_engine();
    let result = engine.execute_powql("count(User)").unwrap();
    match result {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 3),
        _ => panic!("expected scalar int"),
    }
}

#[test]
fn test_update() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"User filter .name = "Alice" update { age := 31 }"#)
        .unwrap();
    let result = engine
        .execute_powql(r#"User filter .name = "Alice" { name, age }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][1], Value::Int(31));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_delete() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"User filter .name = "Bob" delete"#)
        .unwrap();
    let result = engine.execute_powql("count(User)").unwrap();
    match result {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 2),
        _ => panic!("expected scalar int"),
    }
}

#[test]
fn test_order_limit() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql("User order .age desc limit 2 { name, age }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][0], Value::Str("Charlie".into())); // age 35
            assert_eq!(rows[1][0], Value::Str("Alice".into())); // age 30
        }
        _ => panic!("expected rows"),
    }
}

/// `order` by a non-existent column must surface an error, not panic.
/// Regression for executor.rs:998 / :2560 where the Sort node called
/// `unwrap_or_else(|| panic!(...))` — a malformed ORDER BY would crash
/// the server thread instead of returning an error to the client.
#[test]
fn test_order_by_missing_column_errors() {
    let mut engine = test_engine();
    let err = engine
        .execute_powql("User order .nonexistent desc")
        .expect_err("sort on missing column must error, not panic");
    assert!(
        err.to_string().contains("nonexistent"),
        "error should name the missing column, got: {err}"
    );
}

// ─── LIMIT / OFFSET combined semantics ──────────────────────────────────
//
// SQL/PowQL semantics: offset skips M rows first, then limit takes N rows.
// `limit 3 offset 1` on 5 rows must return rows 1..4 (three rows), not
// `N - M` rows. These regression tests pin the plan-shape ordering that
// previously had Offset wrapping Limit (so Limit capped at N rows and
// Offset then skipped M of those, yielding N - M).

/// 5-row Product fixture with an `id` column we can order on.
fn product_engine() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("powdb_limit_offset_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Product { required id: int, required name: str }")
        .unwrap();
    for i in 0..5i64 {
        let q = format!(r#"insert Product {{ id := {i}, name := "p{i}" }}"#);
        engine.execute_powql(&q).unwrap();
    }
    engine
}

#[test]
fn test_limit_offset_combined() {
    // 5 rows, `limit 3 offset 1` → exactly 3 rows, ids [1, 2, 3] when
    // ordered by id. We order by id to pin the row identity; without
    // an order by, insertion order is implementation-defined.
    let mut engine = product_engine();
    let result = engine
        .execute_powql("Product order .id limit 3 offset 1 { .id }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(
                rows.len(),
                3,
                "limit 3 offset 1 on 5 rows must return 3 rows"
            );
            assert_eq!(rows[0][0], Value::Int(1));
            assert_eq!(rows[1][0], Value::Int(2));
            assert_eq!(rows[2][0], Value::Int(3));
        }
        _ => panic!("expected rows"),
    }

    // `limit 2 offset 1` → exactly 2 rows, ids [1, 2].
    let result = engine
        .execute_powql("Product order .id limit 2 offset 1 { .id }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(
                rows.len(),
                2,
                "limit 2 offset 1 on 5 rows must return 2 rows"
            );
            assert_eq!(rows[0][0], Value::Int(1));
            assert_eq!(rows[1][0], Value::Int(2));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_limit_offset_combined_with_order() {
    // Same semantics but ordering on a string column. Names are p0..p4,
    // so sort order is identical to id order.
    let mut engine = product_engine();
    let result = engine
        .execute_powql("Product order .name limit 3 offset 1 { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0][0], Value::Str("p1".into()));
            assert_eq!(rows[1][0], Value::Str("p2".into()));
            assert_eq!(rows[2][0], Value::Str("p3".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_offset_then_limit_keyword_order() {
    // Parser accepts limit/offset in either order — verify the plan
    // semantics are identical regardless of keyword order.
    let mut engine = product_engine();
    let result = engine
        .execute_powql("Product order .id offset 1 limit 3 { .id }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0][0], Value::Int(1));
            assert_eq!(rows[1][0], Value::Int(2));
            assert_eq!(rows[2][0], Value::Int(3));
        }
        _ => panic!("expected rows"),
    }
}
