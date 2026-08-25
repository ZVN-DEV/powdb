use super::fast_paths::mission_a_engine;
use super::*;

// ---- Mission E2a: DISTINCT + IN-list + BETWEEN + LIKE -----------------

#[test]
fn test_distinct_deduplicates_rows() {
    let mut engine = test_engine();
    // Insert a second Alice to create a duplicate name.
    engine
        .execute_powql(r#"insert User { name := "Alice", email := "alice2@ex.com", age := 25 }"#)
        .unwrap();
    let result = engine.execute_powql("User distinct { .name }").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            let names: Vec<&Value> = rows.iter().map(|r| &r[0]).collect();
            // 4 rows in table (Alice×2, Bob, Charlie) but 3 distinct names.
            assert_eq!(names.len(), 3);
            let alice_count = names
                .iter()
                .filter(|v| matches!(v, Value::Str(s) if s == "Alice"))
                .count();
            assert_eq!(alice_count, 1);
            assert!(names
                .iter()
                .any(|v| matches!(v, Value::Str(s) if s == "Bob")));
            assert!(names
                .iter()
                .any(|v| matches!(v, Value::Str(s) if s == "Charlie")));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_in_list_filter() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User filter .name in ("Alice", "Bob") { .name }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_not_in_list_filter() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User filter .name not in ("Alice") { .name }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // Bob and Charlie survive.
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_between_filter() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql("User filter .age between 25 and 30 { .name, .age }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // Alice is 30 (inclusive), Bob is 25 (inclusive).
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_between_filter_float_column_int_literals() {
    // Regression for Value::Ord cross-type bug: BETWEEN on a Float column
    // with Int literals previously returned zero rows because Ord fell
    // through to TypeId discriminant comparison instead of promoting Int
    // to f64. Verifies the fix end-to-end through the query engine.
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "powdb_exec_between_float_{}_{}",
        std::process::id(),
        id
    ));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Product { required name: str, required price: float }")
        .unwrap();
    engine
        .execute_powql(r#"insert Product { name := "Cable",   price := 29.0 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Product { name := "Speaker", price := 175.5 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Product { name := "Monitor", price := 450.0 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Product { name := "Laptop",  price := 1299.0 }"#)
        .unwrap();

    let result = engine
        .execute_powql("Product filter .price between 100 and 500 { .name, .price }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(
                rows.len(),
                2,
                "expected 2 rows in [100, 500] range, got {}: {:?}",
                rows.len(),
                rows
            );
            // Sorted by insert order: Speaker (175.5), Monitor (450.0).
            let names: Vec<&str> = rows
                .iter()
                .map(|r| match &r[0] {
                    Value::Str(s) => s.as_str(),
                    _ => panic!("expected string name"),
                })
                .collect();
            assert!(names.contains(&"Speaker"));
            assert!(names.contains(&"Monitor"));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_not_between_filter() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql("User filter .age not between 26 and 29 { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // Alice (30), Bob (25), Charlie (35) all outside [26,29].
            assert_eq!(rows.len(), 3);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_like_prefix_match() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User filter .name like "Ali%" { .name }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert!(matches!(&rows[0][0], Value::Str(s) if s == "Alice"));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_like_wildcard_underscore() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User filter .name like "_ob" { .name }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert!(matches!(&rows[0][0], Value::Str(s) if s == "Bob"));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_not_like_filter() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User filter .name not like "A%" { .name }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // Bob and Charlie survive (don't start with A).
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_in_list_with_integers() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql("User filter .age in (25, 30) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_like_full_match() {
    let mut engine = test_engine();
    // Exact match (no wildcards).
    let result = engine
        .execute_powql(r#"User filter .name like "Alice" { .name }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
        }
        _ => panic!("expected rows"),
    }
}

// ─── Mission E2b: GROUP BY + HAVING ────────────────────────────────────

#[test]
fn test_group_by_count() {
    // All 3 users share the same "age bucket" when we group by a
    // derived column, but we can at least group by a column with
    // distinct values. test_engine has 3 distinct names.
    let mut engine = test_engine();
    let result = engine
        .execute_powql("User group .name { .name, n: count(.name) }")
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name", "n"]);
            assert_eq!(rows.len(), 3); // 3 distinct names
                                       // Each group has 1 row.
            for row in &rows {
                assert_eq!(row[1], Value::Int(1));
            }
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_group_by_sum_avg() {
    // Group all rows into one bucket by a constant column.
    // We'll use the mission_a_engine with a known shape.
    let mut engine = test_engine();
    // All 3 users: ages 30, 25, 35 → sum=90, avg=30.0
    let result = engine
        .execute_powql("User group .email { .email, total_age: sum(.age) }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // Each email is unique → 3 groups, each with sum of one age.
            assert_eq!(rows.len(), 3);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_group_by_with_filter() {
    let mut engine = test_engine();
    // Filter first, then group.
    let result = engine
        .execute_powql("User filter .age >= 30 group .name { .name, n: count(.name) }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // Alice (30) and Charlie (35) survive filter.
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_group_by_having() {
    // Use mission_a_engine so we have multiple rows per group.
    let mut engine = mission_a_engine(30);
    // 30 rows: statuses cycle active/inactive/pending → 10 each.
    // Group by status, HAVING count > 5.
    let result = engine
        .execute_powql("User group .status having count(.name) > 5 { .status, n: count(.name) }")
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["status", "n"]);
            // All 3 groups have 10 rows each, all > 5.
            assert_eq!(rows.len(), 3);
            for row in &rows {
                assert_eq!(row[1], Value::Int(10));
            }
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_group_by_having_filters_groups() {
    let mut engine = mission_a_engine(30);
    // HAVING count > 100 → no groups survive.
    let result = engine
        .execute_powql("User group .status having count(.name) > 100 { .status }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 0);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_group_by_having_with_aliased_projection_agg() {
    // Regression: TS client found that when the projection duplicates
    // the aggregate used by HAVING (with an alias), HAVING silently
    // failed to filter. This asserts the dedup path produces correct
    // filtering.
    let mut engine = mission_a_engine(30);
    // 3 statuses, 10 rows each. HAVING >= 11 should exclude all.
    let result = engine
        .execute_powql(
            "User group .status having count(.name) >= 11 { .status, cnt: count(.name) }",
        )
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 0, "HAVING >= 11 should filter all groups");
        }
        _ => panic!("expected rows"),
    }
    // HAVING >= 10 should include all three.
    let result = engine
        .execute_powql(
            "User group .status having count(.name) >= 10 { .status, cnt: count(.name) }",
        )
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
            for row in &rows {
                assert_eq!(row[1], Value::Int(10));
            }
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_group_by_having_post_projection() {
    // Regression: HAVING placed after the projection (`{ ... } having cnt >= N`,
    // referencing projection aliases) was silently dropped. This reproduces
    // the exact form the TS client used.
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_having_post_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Person { required name: str, required age: int, city: str }")
        .unwrap();
    for (name, age, city) in [
        ("Alice", 30, "NYC"),
        ("Bob", 24, "SF"),
        ("Carol", 41, "LA"),
        ("Dave", 28, "NYC"),
        ("Eve", 35, "Austin"),
    ] {
        engine
            .execute_powql(&format!(
                r#"insert Person {{ name := "{name}", age := {age}, city := "{city}" }}"#
            ))
            .unwrap();
    }
    let result = engine
        .execute_powql("Person group .city { .city, cnt: count(.name) } having cnt >= 2")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "only NYC has >= 2 people, got: {rows:?}");
            assert_eq!(rows[0][0], Value::Str("NYC".into()));
            assert_eq!(rows[0][1], Value::Int(2));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_having_without_group_by_errors() {
    let mut engine = test_engine();
    let err = engine.execute_powql("User { .name } having count(.name) > 1");
    assert!(
        err.is_err(),
        "HAVING without GROUP BY should be a parse error"
    );
}

#[test]
fn test_group_by_having_reproduces_ts_client_case() {
    // Exact reproduction of the TS client test that surfaced the bug:
    // 5 people across 4 cities, HAVING count >= 2 should keep only NYC.
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_having_ts_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Person { required name: str, required age: int, city: str }")
        .unwrap();
    for (name, age, city) in [
        ("Alice", 30, "NYC"),
        ("Bob", 24, "SF"),
        ("Carol", 41, "LA"),
        ("Dave", 28, "NYC"),
        ("Eve", 35, "Austin"),
    ] {
        engine
            .execute_powql(&format!(
                r#"insert Person {{ name := "{name}", age := {age}, city := "{city}" }}"#
            ))
            .unwrap();
    }
    let result = engine
        .execute_powql("Person group .city having count(.name) >= 2 { .city, cnt: count(.name) }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "only NYC has >= 2 people, got: {rows:?}");
            assert_eq!(rows[0][0], Value::Str("NYC".into()));
            assert_eq!(rows[0][1], Value::Int(2));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_group_by_having_filters_some_groups() {
    // Skewed distribution — some groups pass HAVING, some don't.
    let mut engine = test_engine();
    // test_engine has 3 rows, all distinct names. Add duplicates for Alice.
    engine
        .execute_powql(r#"insert User { name := "Alice", email := "a2@ex.com", age := 31 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert User { name := "Alice", email := "a3@ex.com", age := 32 }"#)
        .unwrap();
    // Now: Alice ×3, Bob ×1, Charlie ×1. HAVING count >= 2 → only Alice.
    let result = engine
        .execute_powql("User group .name having count(.name) >= 2 { .name, cnt: count(.name) }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Alice".into()));
            assert_eq!(rows[0][1], Value::Int(3));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_group_by_min_max() {
    let mut engine = mission_a_engine(30);
    // 30 rows, ages = 18 + (i % 60) for i in 0..30, so ages 18..47.
    // Group by status (3 groups of 10 each).
    // status=active: i=0,3,6,9,12,15,18,21,24,27 → ages 18,21,24,27,30,33,36,39,42,45
    // min=18, max=45
    let result = engine.execute_powql(
        r#"User filter .status = "active" group .status { .status, lo: min(.age), hi: max(.age) }"#,
    ).unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["status", "lo", "hi"]);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("active".into()));
            assert_eq!(rows[0][1], Value::Int(18));
            assert_eq!(rows[0][2], Value::Int(45));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_group_by_avg() {
    let mut engine = mission_a_engine(6);
    // 6 rows: i=0..5
    // active (i=0,3): ages 18,21 → avg=19.5
    // inactive (i=1,4): ages 19,22 → avg=20.5
    // pending (i=2,5): ages 20,23 → avg=21.5
    let result = engine
        .execute_powql(r#"User filter .status = "active" group .status { .status, a: avg(.age) }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            match &rows[0][1] {
                Value::Float(v) => assert!((v - 19.5).abs() < 0.001),
                other => panic!("expected float, got {other:?}"),
            }
        }
        _ => panic!("expected rows"),
    }
}

// ─── IS NULL / IS NOT NULL tests ─────────────────────────────────────

#[test]
fn test_is_null_filter() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"insert User { name := "Diana", email := "diana@ex.com" }"#)
        .unwrap();
    let result = engine
        .execute_powql("User filter .age is null { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Diana".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_is_not_null_filter() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"insert User { name := "Diana", email := "diana@ex.com" }"#)
        .unwrap();
    let result = engine
        .execute_powql("User filter .age is not null { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_is_null_count() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"insert User { name := "Diana", email := "diana@ex.com" }"#)
        .unwrap();
    let result = engine
        .execute_powql("count(User filter .age is null)")
        .unwrap();
    match result {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
        _ => panic!("expected scalar int"),
    }
}

#[test]
fn test_is_null_combined_with_and() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"insert User { name := "Diana", email := "diana@ex.com" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert User { name := "Eve", email := "eve@ex.com" }"#)
        .unwrap();
    let result = engine
        .execute_powql(r#"User filter .age is null and .name = "Diana" { .name }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Diana".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_eq_null_matches_is_null() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"insert User { name := "Diana", email := "diana@ex.com" }"#)
        .unwrap();
    let result = engine
        .execute_powql("User filter .age = null { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Diana".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_neq_null_matches_is_not_null() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"insert User { name := "Diana", email := "diana@ex.com" }"#)
        .unwrap();
    let result = engine
        .execute_powql("User filter .age != null { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
        }
        _ => panic!("expected rows"),
    }
}

// ─── String function tests ─────────────────────────────────────────────

#[test]
fn test_upper_in_filter() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User filter upper(.name) = "ALICE""#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Alice".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_lower_in_projection() {
    let mut engine = test_engine();
    let result = engine.execute_powql("User { low: lower(.email) }").unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["low"]);
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0][0], Value::Str("alice@ex.com".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_length_in_projection() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql("User { .name, len: length(.name) }")
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name", "len"]);
            assert_eq!(rows[0][1], Value::Int(5));
            assert_eq!(rows[1][1], Value::Int(3));
            assert_eq!(rows[2][1], Value::Int(7));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_substring_in_projection() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql("User { sub: substring(.name, 1, 3) }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Str("Ali".into()));
            assert_eq!(rows[1][0], Value::Str("Bob".into()));
            assert_eq!(rows[2][0], Value::Str("Cha".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_concat_in_projection() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User { full: concat(.name, " - ", .email) }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Str("Alice - alice@ex.com".into()));
            assert_eq!(rows[1][0], Value::Str("Bob - bob@ex.com".into()));
            assert_eq!(rows[2][0], Value::Str("Charlie - charlie@ex.com".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_concat_coerces_int() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User { info: concat(.name, " age=", .age) }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Str("Alice age=30".into()));
        }
        _ => panic!("expected rows"),
    }
}

// ─── CASE WHEN tests ───────────────────────────────────────────────

#[test]
fn test_case_in_projection() {
    let mut engine = test_engine();
    let result = engine.execute_powql(
        r#"User { .name, label: case when .age > 30 then "senior" when .age >= 30 then "exactly30" else "young" end }"#
    ).unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name", "label"]);
            assert_eq!(rows.len(), 3);
            for row in &rows {
                let name = &row[0];
                let label = &row[1];
                match name {
                    Value::Str(n) if n == "Alice" => {
                        assert_eq!(label, &Value::Str("exactly30".into()))
                    }
                    Value::Str(n) if n == "Bob" => {
                        assert_eq!(label, &Value::Str("young".into()))
                    }
                    Value::Str(n) if n == "Charlie" => {
                        assert_eq!(label, &Value::Str("senior".into()))
                    }
                    _ => panic!("unexpected name: {name:?}"),
                }
            }
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_case_in_filter() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User filter case when .age > 30 then true else false end"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Charlie".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_case_without_else_returns_empty() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User { .name, label: case when .age > 100 then "old" end }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            for row in &rows {
                assert_eq!(row[1], Value::Empty);
            }
        }
        _ => panic!("expected rows"),
    }
}

// ─── Mul/Div expression tests (E2f) ───────────────────────────────

#[test]
fn test_mul_in_projection() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql("User { .name, double_age: .age * 2 }")
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name", "double_age"]);
            // Alice age=30 → 60, Bob age=25 → 50, Charlie age=35 → 70
            let ages: Vec<_> = rows.iter().map(|r| &r[1]).collect();
            assert!(ages.contains(&&Value::Int(60)));
            assert!(ages.contains(&&Value::Int(50)));
            assert!(ages.contains(&&Value::Int(70)));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_div_in_filter() {
    let mut engine = test_engine();
    let result = engine.execute_powql("User filter .age / 10 > 2").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // 30/10=3>2 ✓, 25/10=2 ✗, 35/10=3>2 ✓
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("expected rows"),
    }
}

// ─── Multi-column ORDER BY tests (E2f) ────────────────────────────

#[test]
fn test_multi_order_by() {
    let mut engine = test_engine();
    // Insert another 30-year-old so we can test tiebreaker
    engine
        .execute_powql(r#"insert User { name := "Dave", email := "dave@ex.com", age := 30 }"#)
        .unwrap();
    let result = engine
        .execute_powql("User order .age asc, .name asc { .name, .age }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // Expected: Bob(25), Alice(30), Dave(30), Charlie(35)
            assert_eq!(rows[0][0], Value::Str("Bob".into()));
            assert_eq!(rows[1][0], Value::Str("Alice".into()));
            assert_eq!(rows[2][0], Value::Str("Dave".into()));
            assert_eq!(rows[3][0], Value::Str("Charlie".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_multi_order_mixed_direction() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"insert User { name := "Dave", email := "dave@ex.com", age := 30 }"#)
        .unwrap();
    let result = engine
        .execute_powql("User order .age asc, .name desc { .name, .age }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // Expected: Bob(25), Dave(30), Alice(30), Charlie(35)
            assert_eq!(rows[0][0], Value::Str("Bob".into()));
            assert_eq!(rows[1][0], Value::Str("Dave".into()));
            assert_eq!(rows[2][0], Value::Str("Alice".into()));
            assert_eq!(rows[3][0], Value::Str("Charlie".into()));
        }
        _ => panic!("expected rows"),
    }
}

// ─── ALTER TABLE / DROP TABLE tests (E2g) ─────────────────────────

#[test]
fn test_alter_add_column() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql("alter User add column status: str")
        .unwrap();
    match result {
        QueryResult::Executed { message } => {
            assert!(message.contains("status"));
            assert!(message.contains("User"));
        }
        other => panic!("expected Executed, got {other:?}"),
    }
    // Verify schema was updated — new inserts can use the new column
    engine.execute_powql(r#"insert User { name := "Eve", email := "eve@ex.com", age := 22, status := "active" }"#).unwrap();
    let result = engine
        .execute_powql(r#"User filter .name = "Eve" { .name, .status }"#)
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name", "status"]);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][1], Value::Str("active".into()));
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_alter_add_column_reads_old_rows() {
    // Regression: before the catalog rewrite path existed, rows
    // inserted before `alter ... add column` were left on disk
    // with the pre-alter variable-offset-table layout. A bare
    // `Type` scan then walked `decode_row` which read
    // `n_var + 1` offsets using the NEW schema and panicked with
    // "range end index X out of range for slice of length Y".
    //
    // This test reproduces that exactly: insert, alter, bare scan.
    // Any panic or wrong row count means the rewrite regressed.
    let mut engine = test_engine();
    engine
        .execute_powql("alter User add column country: str")
        .unwrap();
    // Bare scan: NO filter, so the planner cannot skip old rows.
    let result = engine.execute_powql("User").unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert!(columns.contains(&"country".to_string()));
            assert_eq!(rows.len(), 3, "three old rows must still be readable");
            let country_idx = columns
                .iter()
                .position(|c| c == "country")
                .expect("country column");
            for row in &rows {
                assert_eq!(
                    row[country_idx],
                    Value::Empty,
                    "backfilled column must be Empty"
                );
            }
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_alter_add_required_column_fails() {
    // Adding a required column to a non-empty table has no
    // default value to backfill with, so storing `Empty` would
    // silently violate the required invariant. The catalog must
    // reject it.
    let mut engine = test_engine();
    let err = engine
        .execute_powql("alter User add column required country: str")
        .expect_err("required-column add on non-empty table must fail");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("required") || msg.contains("backfill"),
        "error should mention required/backfill, got: {err}"
    );
    // And the schema must NOT have silently gained the column.
    let result = engine.execute_powql("User").unwrap();
    if let QueryResult::Rows { columns, .. } = result {
        assert!(
            !columns.contains(&"country".to_string()),
            "failed alter must not mutate the schema"
        );
    }
}

#[test]
fn test_alter_add_column_then_update_old_row() {
    // Regression-plus: after the rewrite path backfills Empty, an
    // UPDATE against an old row's new column must round-trip.
    // This exercises encode/decode with the new schema shape on a
    // row that was originally written with the old shape.
    let mut engine = test_engine();
    engine
        .execute_powql("alter User add column country: str")
        .unwrap();
    engine
        .execute_powql(r#"User filter .name = "Alice" update { country := "US" }"#)
        .unwrap();

    let result = engine
        .execute_powql(r#"User filter .name = "Alice" { .name, .country }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Alice".into()));
            assert_eq!(rows[0][1], Value::Str("US".into()));
        }
        other => panic!("expected rows, got {other:?}"),
    }

    // The other two rows should still decode cleanly with Empty.
    let result = engine.execute_powql("User").unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(rows.len(), 3);
            let country_idx = columns
                .iter()
                .position(|c| c == "country")
                .expect("country column");
            let empties = rows
                .iter()
                .filter(|r| r[country_idx] == Value::Empty)
                .count();
            assert_eq!(
                empties, 2,
                "two unchanged old rows must still read as Empty"
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_alter_drop_column() {
    let mut engine = test_engine();
    engine
        .execute_powql("alter User drop column email")
        .unwrap();
    let result = engine.execute_powql("User { .name, .age }").unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name", "age"]);
            assert_eq!(rows.len(), 3);
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_drop_table() {
    let mut engine = test_engine();
    let result = engine.execute_powql("drop User").unwrap();
    match result {
        QueryResult::Executed { message } => {
            assert!(message.contains("User"));
            assert!(message.contains("dropped"));
        }
        other => panic!("expected Executed, got {other:?}"),
    }
    // Querying the dropped table should fail
    assert!(engine.execute_powql("User").is_err());
}

#[test]
fn test_drop_nonexistent_table_errors() {
    let mut engine = test_engine();
    assert!(engine.execute_powql("drop NonExistent").is_err());
}

#[test]
fn test_alter_add_duplicate_column_errors() {
    let mut engine = test_engine();
    assert!(engine.execute_powql("alter User add name: str").is_err());
}

#[test]
fn test_alter_drop_nonexistent_column_errors() {
    let mut engine = test_engine();
    assert!(engine
        .execute_powql("alter User drop column nonexistent")
        .is_err());
}

#[test]
fn test_alter_add_index_creates_index() {
    let mut engine = test_engine();
    let result = engine.execute_powql("alter User add index .email").unwrap();
    match result {
        QueryResult::Executed { message } => {
            assert!(message.contains("User.email"), "message: {message}");
        }
        other => panic!("expected Executed, got {other:?}"),
    }
    // Equality lookup on the indexed column should still return results.
    let result = engine
        .execute_powql(r#"User filter .email = "alice@ex.com" { .name }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Alice".into()));
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_parse_rejects_trailing_tokens() {
    // Previously `User create_index .email` silently succeeded as
    // `User` (ignoring the trailing unknown tokens). Now it's a
    // parse error so users know the syntax isn't recognized.
    let mut engine = test_engine();
    assert!(engine.execute_powql("User create_index .email").is_err());
    assert!(engine.execute_powql("User add_column score: int").is_err());
    assert!(engine.execute_powql("User drop_column email").is_err());
}

// ─── IN subquery tests (E2h) ─────────────────────────────────────

#[test]
fn test_in_subquery_basic() {
    let mut engine = test_engine();
    // Create a second table with a subset of user names
    engine
        .execute_powql("type VIP { required name: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert VIP { name := "Alice" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert VIP { name := "Charlie" }"#)
        .unwrap();

    let result = engine
        .execute_powql("User filter .name in (VIP { .name }) { .name, .age }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
            let names: Vec<_> = rows.iter().map(|r| &r[0]).collect();
            assert!(names.contains(&&Value::Str("Alice".into())));
            assert!(names.contains(&&Value::Str("Charlie".into())));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_not_in_subquery() {
    let mut engine = test_engine();
    engine
        .execute_powql("type VIP { required name: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert VIP { name := "Alice" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert VIP { name := "Charlie" }"#)
        .unwrap();

    let result = engine
        .execute_powql("User filter .name not in (VIP { .name }) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Bob".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_in_subquery_with_filter() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Score { required name: str, required points: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert Score { name := "Alice", points := 100 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Score { name := "Bob", points := 50 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Score { name := "Charlie", points := 80 }"#)
        .unwrap();

    // Find users whose names are in the high-scorers list (points > 70)
    let result = engine
        .execute_powql("User filter .name in (Score filter .points > 70 { .name }) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
            let names: Vec<_> = rows.iter().map(|r| &r[0]).collect();
            assert!(names.contains(&&Value::Str("Alice".into())));
            assert!(names.contains(&&Value::Str("Charlie".into())));
        }
        _ => panic!("expected rows"),
    }
}

// ─── EXISTS subquery tests (uncorrelated) ───────────────────────────

#[test]
fn test_exists_subquery_uncorrelated_true() {
    let mut engine = test_engine();
    // A side table with at least one row → EXISTS(...) = true, so the
    // filter passes every User row through.
    engine
        .execute_powql("type VIP { required name: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert VIP { name := "Alice" }"#)
        .unwrap();

    let result = engine
        .execute_powql("User filter exists (VIP) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3, "all users should pass when EXISTS is true");
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_exists_subquery_uncorrelated_false() {
    let mut engine = test_engine();
    // An empty side table → EXISTS(...) = false, so no User rows pass.
    engine
        .execute_powql("type VIP { required name: str }")
        .unwrap();

    let result = engine
        .execute_powql("User filter exists (VIP) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 0, "no rows should pass when EXISTS is false");
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_not_exists_subquery() {
    let mut engine = test_engine();
    // NOT EXISTS over an empty table → true → all rows pass.
    engine
        .execute_powql("type VIP { required name: str }")
        .unwrap();

    let result = engine
        .execute_powql("User filter not exists (VIP) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
        }
        _ => panic!("expected rows"),
    }

    // Now add a row — NOT EXISTS becomes false → no rows pass.
    engine
        .execute_powql(r#"insert VIP { name := "Alice" }"#)
        .unwrap();
    let result = engine
        .execute_powql("User filter not exists (VIP) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 0);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_exists_subquery_with_inner_filter() {
    let mut engine = test_engine();
    // Subquery with its own filter: only rows matching the inner
    // predicate count toward EXISTS.
    engine
        .execute_powql("type Score { required name: str, required points: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert Score { name := "Alice", points := 100 }"#)
        .unwrap();

    // Inner filter matches → EXISTS true → all users pass.
    let result = engine
        .execute_powql("User filter exists (Score filter .points > 50) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 3),
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_exists_subquery_with_inner_filter_no_match() {
    // Fresh engine so the plan cache doesn't collide with the
    // `> 50` shape from the sibling test.
    let mut engine = test_engine();
    engine
        .execute_powql("type Score { required name: str, required points: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert Score { name := "Alice", points := 100 }"#)
        .unwrap();

    // Inner filter matches nothing → EXISTS false → no users pass.
    let result = engine
        .execute_powql("User filter exists (Score filter .points > 1000) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 0),
        _ => panic!("expected rows"),
    }
}
