use super::joins::join_engine;
use super::*;

// ─── EXPLAIN tests ─────────────────────────────────────────────────

#[test]
fn test_explain_simple_scan() {
    let mut engine = test_engine();
    let result = engine.execute_powql("explain User").unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["plan"]);
            assert!(!rows.is_empty());
            assert!(matches!(&rows[0][0], Value::Str(s) if s.contains("SeqScan")));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_explain_filter() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql("explain User filter .age > 30")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            let plan_text: String = rows
                .iter()
                .map(|r| match &r[0] {
                    Value::Str(s) => s.as_str(),
                    _ => "",
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                plan_text.contains("Filter"),
                "plan should show Filter(SeqScan) after lowering unindexed RangeScan"
            );
        }
        _ => panic!("expected rows"),
    }
}

fn explain_text(engine: &mut Engine, q: &str) -> String {
    match engine.execute_powql(q).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r[0] {
                Value::Str(s) => s.as_str(),
                _ => "",
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_explain_distinguishes_compound_hash_and_bounded_nested_join() {
    let mut engine = join_engine();
    let compound = explain_text(
        &mut engine,
        "explain User as u join Order as o on o.total > 75 and o.user_id = u.id",
    );
    assert!(compound.contains("strategy=hash+residual"), "{compound}");

    let non_equi = explain_text(
        &mut engine,
        "explain User as u join Order as o on u.id < o.user_id",
    );
    assert!(
        non_equi.contains("strategy=nested-loop-bounded"),
        "{non_equi}"
    );
}

#[test]
fn test_explain_eq_filter_unindexed_shows_seqscan_not_indexscan() {
    let mut engine = test_engine();
    // `email` has NO index in test_engine; the planner folds
    // `.email = lit` to IndexScan speculatively. EXPLAIN must show
    // what actually runs: Filter over SeqScan.
    let text = explain_text(
        &mut engine,
        r#"explain User filter .email = "alice@ex.com""#,
    );
    assert!(!text.contains("IndexScan"), "got: {text}");
    assert!(text.contains("Filter"), "got: {text}");
    assert!(text.contains("SeqScan"), "got: {text}");
}

#[test]
fn test_explain_eq_filter_indexed_shows_indexscan() {
    let mut engine = test_engine();
    engine.execute_powql("alter User add index .email").unwrap();
    let text = explain_text(
        &mut engine,
        r#"explain User filter .email = "alice@ex.com""#,
    );
    assert!(text.contains("IndexScan"), "got: {text}");
}

fn sorted_names(r: QueryResult) -> Vec<String> {
    match r {
        QueryResult::Rows { rows, .. } => {
            let mut v: Vec<String> = rows.iter().map(|r| format!("{:?}", r[0])).collect();
            v.sort();
            v
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_range_scan_uses_nonunique_index_same_results() {
    let mut engine = test_engine(); // Alice 30, Bob 25, Charlie 35
    let unindexed = engine
        .execute_powql("User filter .age > 26 and .age <= 35 { .name }")
        .unwrap();
    engine.execute_powql("alter User add index .age").unwrap();
    let indexed = engine
        .execute_powql("User filter .age > 26 and .age <= 35 { .name }")
        .unwrap();
    assert_eq!(sorted_names(unindexed), sorted_names(indexed)); // Alice, Charlie
}

#[test]
fn test_range_scan_between_uses_nonunique_index() {
    let mut engine = test_engine();
    let unindexed = engine
        .execute_powql("User filter .age between 25 and 30 { .name }")
        .unwrap();
    engine.execute_powql("alter User add index .age").unwrap();
    let indexed = engine
        .execute_powql("User filter .age between 25 and 30 { .name }")
        .unwrap();
    assert_eq!(sorted_names(unindexed), sorted_names(indexed)); // Alice, Bob
}

#[test]
fn test_range_scan_indexed_exclusive_bound_excludes_boundary() {
    let mut engine = test_engine();
    engine.execute_powql("alter User add index .age").unwrap();
    // Bob is exactly 25; `.age > 25` must exclude him.
    let names = sorted_names(
        engine
            .execute_powql("User filter .age > 25 { .name }")
            .unwrap(),
    );
    assert_eq!(names, vec!["Str(\"Alice\")", "Str(\"Charlie\")"]);
}

#[test]
fn test_range_scan_indexed_excludes_nulls() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"insert User { name := "Dana", email := "d@ex.com" }"#)
        .unwrap(); // age null
    engine.execute_powql("alter User add index .age").unwrap();
    match engine
        .execute_powql("User filter .age < 100 { .name }")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 3, "null age must not match"),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_explain_range_indexed_shows_rangescan() {
    let mut engine = test_engine();
    engine.execute_powql("alter User add index .age").unwrap();
    let text = explain_text(&mut engine, "explain User filter .age > 26");
    assert!(text.contains("RangeScan"), "got: {text}");
}

#[test]
fn test_explain_does_not_execute() {
    let mut engine = test_engine();
    // EXPLAIN should NOT actually insert a row.
    let result = engine
        .execute_powql(r#"explain insert User { name := "Zara", age := 99 }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            let plan_text: String = rows
                .iter()
                .map(|r| match &r[0] {
                    Value::Str(s) => s.as_str(),
                    _ => "",
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(plan_text.contains("Insert"));
        }
        _ => panic!("expected rows"),
    }
    // Verify no row was actually inserted.
    let result = engine.execute_powql("User { .name }").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3, "should still have original 3 users");
        }
        _ => panic!("expected rows"),
    }
}

// ─── Correlated subquery tests ──────────────────────────────────────

#[test]
fn test_correlated_in_subquery() {
    let mut engine = test_engine();
    // Create an orders table with user_name to correlate on.
    engine
        .execute_powql("type UserOrder { required user_name: str, required total: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert UserOrder { user_name := "Alice", total := 100 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert UserOrder { user_name := "Alice", total := 200 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert UserOrder { user_name := "Bob", total := 50 }"#)
        .unwrap();

    // Correlated: for each User row, find orders where user_name = outer .name
    // The subquery references .name which is a User column, not a UserOrder column.
    let result = engine
        .execute_powql(
            "User filter .name in (UserOrder filter .user_name = .name { .user_name }) { .name }",
        )
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2, "Alice and Bob have orders");
            let names: Vec<_> = rows.iter().map(|r| &r[0]).collect();
            assert!(names.contains(&&Value::Str("Alice".into())));
            assert!(names.contains(&&Value::Str("Bob".into())));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_correlated_exists_subquery() {
    let mut engine = test_engine();
    engine
        .execute_powql("type UserOrder { required user_name: str, required total: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert UserOrder { user_name := "Alice", total := 100 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert UserOrder { user_name := "Bob", total := 50 }"#)
        .unwrap();

    // Correlated EXISTS: only Users who have at least one order.
    // .name in the subquery filter refers to the outer User's name column.
    let result = engine
        .execute_powql("User filter exists (UserOrder filter .user_name = .name) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2, "Alice and Bob have orders");
            let names: Vec<_> = rows.iter().map(|r| &r[0]).collect();
            assert!(names.contains(&&Value::Str("Alice".into())));
            assert!(names.contains(&&Value::Str("Bob".into())));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_correlated_not_exists_subquery() {
    let mut engine = test_engine();
    engine
        .execute_powql("type UserOrder { required user_name: str, required total: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert UserOrder { user_name := "Alice", total := 100 }"#)
        .unwrap();

    // NOT EXISTS: Users without orders (Bob and Charlie).
    let result = engine
        .execute_powql("User filter not exists (UserOrder filter .user_name = .name) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2, "Bob and Charlie have no orders");
            let names: Vec<_> = rows.iter().map(|r| &r[0]).collect();
            assert!(names.contains(&&Value::Str("Bob".into())));
            assert!(names.contains(&&Value::Str("Charlie".into())));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_correlated_subquery_datetime_and_null() {
    // Regression: value_to_expr coerced DateTime (and NULL) outer values to
    // Int(0) during correlated-subquery substitution, so the comparison was
    // always false and matching rows were wrongly excluded.
    let mut engine = test_engine();
    engine
        .execute_powql("type Appt { required who: str, sched: datetime }")
        .unwrap();
    let ts = 1705321845000000_i64; // 2024-01-15 12:30:45 UTC
    engine
        .execute_powql(&format!(
            r#"insert Appt {{ who := "Alice", sched := {ts} }}"#
        ))
        .unwrap();
    // Bob's sched is NULL (column omitted).
    engine
        .execute_powql(r#"insert Appt { who := "Bob" }"#)
        .unwrap();
    engine
        .execute_powql("type Slot { required ts: datetime }")
        .unwrap();
    engine
        .execute_powql(&format!("insert Slot {{ ts := {ts} }}"))
        .unwrap();

    // Correlated EXISTS: an Appt matches when a Slot exists at its `sched`
    // time. The outer `.sched` (a DateTime, NULL for Bob) is substituted into
    // the subquery filter. Alice's slot exists → matches; Bob's sched is NULL
    // → no slot matches → excluded.
    let result = engine
        .execute_powql("Appt filter exists (Slot filter .ts = .sched) { .who }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "only Alice has a matching slot");
            assert_eq!(rows[0][0], Value::Str("Alice".into()));
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

// ─── CAST tests ───────────────────────────────────────────────────

#[test]
fn test_cast_int_to_str() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User { s: cast(.age, "str") }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Str("30".into()));
            assert_eq!(rows[1][0], Value::Str("25".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_cast_str_to_int() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"type Numbers { required val: str }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Numbers { val := "42" }"#)
        .unwrap();
    let result = engine
        .execute_powql(r#"Numbers { n: cast(.val, "int") }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Int(42));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_cast_float_to_int() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Floats { required val: float }")
        .unwrap();
    engine
        .execute_powql("insert Floats { val := 3.7 }")
        .unwrap();
    let result = engine
        .execute_powql(r#"Floats { n: cast(.val, "int") }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Int(3));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_cast_int_to_float() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User { f: cast(.age, "float") }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Float(30.0));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_cast_int_to_bool() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User { b: cast(.age, "bool") }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // age=30 -> true (non-zero)
            assert_eq!(rows[0][0], Value::Bool(true));
        }
        _ => panic!("expected rows"),
    }
}

// ─── Math function tests ──────────────────────────────────────────

#[test]
fn test_abs() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Nums { required val: int }")
        .unwrap();
    engine.execute_powql("insert Nums { val := -42 }").unwrap();
    let result = engine.execute_powql("Nums { a: abs(.val) }").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Int(42));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_round() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Floats { required val: float }")
        .unwrap();
    engine
        .execute_powql("insert Floats { val := 7.56789 }")
        .unwrap();
    let result = engine
        .execute_powql("Floats { r: round(.val, 2) }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Float(7.57));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_ceil_floor() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Floats { required val: float }")
        .unwrap();
    engine
        .execute_powql("insert Floats { val := 3.2 }")
        .unwrap();
    let c = engine.execute_powql("Floats { c: ceil(.val) }").unwrap();
    let f = engine.execute_powql("Floats { f: floor(.val) }").unwrap();
    match (c, f) {
        (QueryResult::Rows { rows: cr, .. }, QueryResult::Rows { rows: fr, .. }) => {
            assert_eq!(cr[0][0], Value::Float(4.0));
            assert_eq!(fr[0][0], Value::Float(3.0));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_sqrt() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Nums { required val: int }")
        .unwrap();
    engine.execute_powql("insert Nums { val := 144 }").unwrap();
    let result = engine.execute_powql("Nums { s: sqrt(.val) }").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Float(12.0));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_pow() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Nums { required val: int }")
        .unwrap();
    engine.execute_powql("insert Nums { val := 3 }").unwrap();
    let result = engine.execute_powql("Nums { p: pow(.val, 4) }").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Int(81));
        }
        _ => panic!("expected rows"),
    }
}

// ─── Date/time function tests ─────────────────────────────────────

#[test]
fn test_now_returns_datetime() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Events { required name: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert Events { name := "test" }"#)
        .unwrap();
    let result = engine.execute_powql("Events { ts: now() }").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => match &rows[0][0] {
            Value::DateTime(m) => assert!(*m > 0, "now() should return positive timestamp"),
            other => panic!("expected DateTime, got {other:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_extract_from_datetime() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Events { required ts: datetime }")
        .unwrap();
    // 2024-01-15 12:30:45 UTC in microseconds
    // 2024-01-15 = 19737 days since epoch
    // 19737 * 86400 = 1705276800 seconds + 12*3600 + 30*60 + 45 = 1705321845
    // * 1_000_000 = 1705321845000000
    engine
        .execute_powql("insert Events { ts := 1705321845000000 }")
        .unwrap();
    let result = engine.execute_powql(r#"Events { y: extract("year", .ts), m: extract("month", .ts), d: extract("day", .ts), h: extract("hour", .ts) }"#).unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Int(2024));
            assert_eq!(rows[0][1], Value::Int(1));
            assert_eq!(rows[0][2], Value::Int(15));
            assert_eq!(rows[0][3], Value::Int(12));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_date_add() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Events { required ts: datetime }")
        .unwrap();
    let base = 1705321845000000_i64; // 2024-01-15 12:30:45 UTC
    engine
        .execute_powql(&format!("insert Events {{ ts := {base} }}"))
        .unwrap();
    let result = engine
        .execute_powql(r#"Events { later: date_add(.ts, 2, "hours") }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::DateTime(base + 2 * 3_600_000_000));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_date_diff() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Events { required start_ts: datetime, required end_ts: datetime }")
        .unwrap();
    let t1 = 1705321845000000_i64; // 2024-01-15 12:30:45 UTC
    let t2 = t1 + 3 * 86_400_000_000; // +3 days
    engine
        .execute_powql(&format!(
            "insert Events {{ start_ts := {t1}, end_ts := {t2} }}"
        ))
        .unwrap();
    let result = engine
        .execute_powql(r#"Events { diff: date_diff(.end_ts, .start_ts, "days") }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Int(3));
        }
        _ => panic!("expected rows"),
    }
}
