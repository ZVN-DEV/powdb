use super::*;

// ─── Materialized view tests ────────────────────────────────────────────

#[test]
fn test_create_materialized_view() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"materialize OldUsers as User filter .age > 28"#)
        .unwrap();
    match result {
        QueryResult::Executed { message } => {
            assert!(message.contains("OldUsers"));
        }
        _ => panic!("expected Executed"),
    }
    // Query the view like a table.
    let result = engine.execute_powql("OldUsers").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2); // Alice (30) and Charlie (35)
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_view_auto_refresh_on_insert() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"materialize OldUsers as User filter .age > 28"#)
        .unwrap();
    // Insert a new qualifying row.
    engine
        .execute_powql(r#"insert User { name := "Dave", email := "dave@ex.com", age := 40 }"#)
        .unwrap();
    // The view should auto-refresh and include Dave.
    let result = engine.execute_powql("OldUsers").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3); // Alice, Charlie, Dave
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_view_auto_refresh_on_delete() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"materialize OldUsers as User filter .age > 28"#)
        .unwrap();
    // Delete Alice (age 30) from the base table.
    engine
        .execute_powql(r#"User filter .name = "Alice" delete"#)
        .unwrap();
    // View should auto-refresh: only Charlie remains.
    let result = engine.execute_powql("OldUsers").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_view_auto_refresh_on_update() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"materialize OldUsers as User filter .age > 28"#)
        .unwrap();
    // Update Bob's age to make him qualify.
    engine
        .execute_powql(r#"User filter .name = "Bob" update { age := 50 }"#)
        .unwrap();
    let result = engine.execute_powql("OldUsers").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3); // Alice, Charlie, Bob
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_explicit_refresh() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"materialize OldUsers as User filter .age > 28"#)
        .unwrap();
    engine
        .execute_powql(r#"insert User { name := "Eve", email := "eve@ex.com", age := 55 }"#)
        .unwrap();
    // Explicit refresh.
    let result = engine.execute_powql("refresh OldUsers").unwrap();
    match result {
        QueryResult::Executed { message } => {
            assert!(message.contains("refreshed"));
        }
        _ => panic!("expected Executed"),
    }
    // Now query — should include Eve.
    let result = engine.execute_powql("OldUsers").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_drop_view() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"materialize OldUsers as User filter .age > 28"#)
        .unwrap();
    let result = engine.execute_powql("drop view OldUsers").unwrap();
    match result {
        QueryResult::Executed { message } => {
            assert!(message.contains("dropped"));
        }
        _ => panic!("expected Executed"),
    }
    // Querying the dropped view should fail.
    let err = engine.execute_powql("OldUsers").unwrap_err();
    assert!(err.to_string().contains("not found"));
}

#[test]
fn test_view_with_projection() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"materialize UserNames as User { .name }"#)
        .unwrap();
    let result = engine.execute_powql("UserNames").unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name".to_string()]);
            assert_eq!(rows.len(), 3);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_view_no_stale_reads() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"materialize AllUsers as User"#)
        .unwrap();
    // Verify initial state.
    let result = engine.execute_powql("AllUsers").unwrap();
    match &result {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 3),
        _ => panic!("expected rows"),
    }
    // Insert two more.
    engine
        .execute_powql(r#"insert User { name := "D", email := "d@ex.com", age := 1 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert User { name := "E", email := "e@ex.com", age := 2 }"#)
        .unwrap();
    // First insert marks dirty, second stays dirty. Auto-refresh fires on read.
    let result = engine.execute_powql("AllUsers").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 5),
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_duplicate_view_creation_fails() {
    let mut engine = test_engine();
    engine.execute_powql(r#"materialize V as User"#).unwrap();
    let err = engine
        .execute_powql(r#"materialize V as User"#)
        .unwrap_err();
    assert!(err.to_string().contains("already exists"));
}

#[test]
fn test_drop_nonexistent_view_fails() {
    let mut engine = test_engine();
    let err = engine.execute_powql("drop view NoSuchView").unwrap_err();
    assert!(err.to_string().contains("not found"));
}

// ── UNION / UNION ALL tests ────────────────────────────────

#[test]
fn test_union_deduplicates() {
    let mut engine = test_engine();
    engine.execute_powql("type A { name: str }").unwrap();
    engine.execute_powql("type B { name: str }").unwrap();
    engine
        .execute_powql(r#"insert A { name := "alice" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert A { name := "bob" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert B { name := "bob" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert B { name := "carol" }"#)
        .unwrap();
    let result = engine.execute_powql("A union B").unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    // alice, bob, carol — bob deduped
    assert_eq!(rows.len(), 3);
}

#[test]
fn test_union_all_keeps_duplicates() {
    let mut engine = test_engine();
    engine.execute_powql("type X { val: int }").unwrap();
    engine.execute_powql("type Y { val: int }").unwrap();
    engine.execute_powql("insert X { val := 1 }").unwrap();
    engine.execute_powql("insert X { val := 2 }").unwrap();
    engine.execute_powql("insert Y { val := 2 }").unwrap();
    engine.execute_powql("insert Y { val := 3 }").unwrap();
    let result = engine.execute_powql("X union all Y").unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    // 1, 2, 2, 3 — no dedup
    assert_eq!(rows.len(), 4);
}

#[test]
fn test_union_with_filters() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Emp { name: str, dept: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert Emp { name := "alice", dept := "eng" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Emp { name := "bob", dept := "sales" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Emp { name := "carol", dept := "eng" }"#)
        .unwrap();
    let result = engine
        .execute_powql(r#"Emp filter .dept = "eng" union Emp filter .dept = "sales""#)
        .unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    assert_eq!(rows.len(), 3);
}

#[test]
fn test_union_chain_three_tables() {
    let mut engine = test_engine();
    engine.execute_powql("type T1 { v: int }").unwrap();
    engine.execute_powql("type T2 { v: int }").unwrap();
    engine.execute_powql("type T3 { v: int }").unwrap();
    engine.execute_powql("insert T1 { v := 1 }").unwrap();
    engine.execute_powql("insert T2 { v := 2 }").unwrap();
    engine.execute_powql("insert T3 { v := 3 }").unwrap();
    let result = engine.execute_powql("T1 union T2 union T3").unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    assert_eq!(rows.len(), 3);
}

#[test]
fn test_union_uses_left_side_columns() {
    let mut engine = test_engine();
    engine.execute_powql("type L { name: str }").unwrap();
    engine.execute_powql("type R { name: str }").unwrap();
    engine.execute_powql(r#"insert L { name := "a" }"#).unwrap();
    engine.execute_powql(r#"insert R { name := "b" }"#).unwrap();
    let result = engine.execute_powql("L union R").unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name".to_string()]);
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("expected rows"),
    }
}

// ── COUNT DISTINCT tests ───────────────────────────────────

#[test]
fn test_count_distinct_standalone() {
    let mut engine = test_engine();
    engine.execute_powql("type Color { name: str }").unwrap();
    engine
        .execute_powql(r#"insert Color { name := "red" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Color { name := "blue" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Color { name := "red" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Color { name := "green" }"#)
        .unwrap();
    let result = engine
        .execute_powql("count(distinct Color { .name })")
        .unwrap();
    match result {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 3), // red, blue, green
        _ => panic!("expected scalar int"),
    }
}

#[test]
fn test_count_distinct_in_group_by() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Sale { dept: str, item: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert Sale { dept := "eng", item := "laptop" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Sale { dept := "eng", item := "laptop" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Sale { dept := "eng", item := "monitor" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Sale { dept := "sales", item := "phone" }"#)
        .unwrap();
    let result = engine
        .execute_powql("Sale group .dept { .dept, count(distinct .item) }")
        .unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    // eng: 2 distinct items (laptop, monitor), sales: 1 (phone)
    let eng_row = rows
        .iter()
        .find(|r| r[0] == Value::Str("eng".into()))
        .unwrap();
    let sales_row = rows
        .iter()
        .find(|r| r[0] == Value::Str("sales".into()))
        .unwrap();
    assert_eq!(eng_row[1], Value::Int(2));
    assert_eq!(sales_row[1], Value::Int(1));
}

#[test]
fn test_count_distinct_with_filter() {
    let mut engine = test_engine();
    // Use test_engine which creates User with name, email, age
    engine
        .execute_powql(r#"insert User { name := "Dave", email := "d@e.com", age := 30 }"#)
        .unwrap();
    let result = engine
        .execute_powql("count(distinct User { .age })")
        .unwrap();
    match result {
        QueryResult::Scalar(Value::Int(n)) => {
            // 30(alice), 25(bob), 35(charlie), 30(dave) → 3 distinct
            assert_eq!(n, 3);
        }
        _ => panic!("expected scalar int"),
    }
}

// ── UPDATE with expressions tests ──────────────────────────

#[test]
fn test_update_with_arithmetic_expression() {
    let mut engine = test_engine();
    // Alice starts at age 30
    engine
        .execute_powql(r#"User filter .name = "Alice" update { age := .age + 5 }"#)
        .unwrap();
    let result = engine
        .execute_powql(r#"User filter .name = "Alice""#)
        .unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    assert_eq!(rows[0][2], Value::Int(35)); // 30 + 5 = 35
}

#[test]
fn test_update_with_multiply_expression() {
    let mut engine = test_engine();
    // Double everyone's age
    engine
        .execute_powql("User update { age := .age * 2 }")
        .unwrap();
    let result = engine.execute_powql("User").unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    let ages: Vec<i64> = rows
        .iter()
        .map(|r| match &r[2] {
            Value::Int(v) => *v,
            _ => 0,
        })
        .collect();
    assert!(ages.contains(&60)); // Alice: 30*2
    assert!(ages.contains(&50)); // Bob: 25*2
    assert!(ages.contains(&70)); // Charlie: 35*2
}

#[test]
fn test_update_expression_with_filter() {
    let mut engine = test_engine();
    // Increment age only for people over 28
    engine
        .execute_powql("User filter .age > 28 update { age := .age + 1 }")
        .unwrap();
    let result = engine
        .execute_powql(r#"User filter .name = "Alice""#)
        .unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    assert_eq!(rows[0][2], Value::Int(31)); // Alice was 30, now 31
    let result = engine
        .execute_powql(r#"User filter .name = "Bob""#)
        .unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    assert_eq!(rows[0][2], Value::Int(25)); // Bob was 25, unchanged
}

#[test]
fn test_update_literal_still_uses_fast_path() {
    // Verify the literal path still works after the refactor
    let mut engine = test_engine();
    engine
        .execute_powql(r#"User filter .name = "Alice" update { age := 99 }"#)
        .unwrap();
    let result = engine
        .execute_powql(r#"User filter .name = "Alice""#)
        .unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    assert_eq!(rows[0][2], Value::Int(99));
}

// ── COUNT(*) in GROUP BY tests ─────────────────────────────

#[test]
fn test_group_by_count_star() {
    let mut engine = test_engine();
    // test_engine has 3 users: Alice(30), Bob(25), Charlie(35)
    // Add another user with same age as Alice
    engine
        .execute_powql(r#"insert User { name := "Dave", email := "d@e.com", age := 30 }"#)
        .unwrap();
    let result = engine
        .execute_powql("User group .age { .age, count(*) }")
        .unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    let age30 = rows.iter().find(|r| r[0] == Value::Int(30)).unwrap();
    assert_eq!(age30[1], Value::Int(2)); // Alice + Dave
    let age25 = rows.iter().find(|r| r[0] == Value::Int(25)).unwrap();
    assert_eq!(age25[1], Value::Int(1)); // Bob only
}

#[test]
fn test_group_by_count_star_with_having() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"insert User { name := "Dave", email := "d@e.com", age := 30 }"#)
        .unwrap();
    let result = engine
        .execute_powql("User group .age having count(*) > 1 { .age, count(*) }")
        .unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(30)); // only age=30 has count > 1
}
