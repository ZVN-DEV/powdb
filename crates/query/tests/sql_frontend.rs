use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

#[test]
fn sql_select_matches_powql_and_shares_plan_cache() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type User { required id: int, required name: str, age: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert User { id := 1, name := "Ada", age := 37 }, { id := 2, name := "Grace", age := 31 }"#)
        .unwrap();

    let sql = engine
        .execute_sql("SELECT name, age FROM User WHERE age > 30 ORDER BY age DESC LIMIT 10")
        .unwrap();
    let powql = engine
        .execute_powql("User filter .age > 30 order .age desc limit 10 { .name, .age }")
        .unwrap();
    assert_eq!(format!("{sql:?}"), format!("{powql:?}"));
    let (hits, misses, len) = engine.plan_cache_stats();
    assert!(misses >= 1, "first SQL execution should populate cache");
    assert!(
        hits >= 1,
        "equivalent PowQL should reuse SQL-populated cache"
    );
    assert!(len >= 1);
}

#[test]
fn sql_mutations_execute_through_existing_engine() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_sql("CREATE TABLE User (id INTEGER NOT NULL UNIQUE, name TEXT, age INTEGER)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO User (id, name, age) VALUES (1, 'Ada', 37), (2, 'Grace', 31)")
        .unwrap();
    engine
        .execute_sql("UPDATE User SET age = 38 WHERE id = 1")
        .unwrap();
    engine.execute_sql("DELETE FROM User WHERE id = 2").unwrap();

    match engine
        .execute_sql("SELECT id, name, age FROM User")
        .unwrap()
    {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["id", "name", "age"]);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Int(1));
            assert_eq!(rows[0][1], Value::Str("Ada".into()));
            assert_eq!(rows[0][2], Value::Int(38));
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn sql_readonly_rejects_writes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(dir.path()).unwrap();
    let err = engine
        .execute_sql_readonly("CREATE TABLE T (id INTEGER)")
        .unwrap_err();
    assert_eq!(err.to_string(), "__POWDB_READONLY_NEEDS_WRITE__");
}

fn seeded_engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required id: int, required x: int }")
        .unwrap();
    engine
        .execute_powql("insert T { id := 1, x := 1 }, { id := 2, x := 5 }")
        .unwrap();
    (dir, engine)
}

/// Regression: a deeply-nested SQL expression must return a parse error, not
/// overflow the stack and abort the process. Reachable over the wire via
/// MSG_QUERY_SQL, so an unbounded-recursion overflow + panic=abort is a remote
/// DoS. The PowQL guard (MAX_NESTING_DEPTH) runs in the second stage and never
/// fires because the SQL pre-parser overflows first.
#[test]
fn sql_deeply_nested_expression_errors_instead_of_aborting() {
    let n = 50_000usize;
    let mut q = String::from("SELECT a FROM T WHERE ");
    q.push_str(&"(".repeat(n));
    q.push('1');
    q.push_str(&")".repeat(n));
    let r = powdb_query::sql::parse_sql(&q);
    assert!(r.is_err(), "deep nesting should error, not abort");
}

/// Regression: `NOT x = 1` is standard SQL `NOT (x = 1)`, not `(NOT x) = 1`.
/// Row id=2 has x=5, so the predicate is true for it.
#[test]
fn sql_not_binds_looser_than_comparison() {
    let (_dir, mut engine) = seeded_engine();
    match engine
        .execute_sql("SELECT id FROM T WHERE NOT x = 1")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "NOT (x = 1) should match the x=5 row");
            assert_eq!(rows[0][0], Value::Int(2));
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Regression: arithmetic in a bare (un-aliased) projection must parse and
/// evaluate. Previously PowQL's projection grammar only accepted a single
/// field/agg token in a bare slot, so `SELECT x - 1` (→ PowQL `T { .x - 1 }`)
/// failed with "expected field, got '-'".
#[test]
fn sql_subtraction_in_bare_projection() {
    let (_dir, mut engine) = seeded_engine();
    match engine
        .execute_sql("SELECT x - 1 FROM T WHERE id = 2")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Int(4), "5 - 1 = 4");
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The same computed-projection support must hold for native PowQL, not just
/// the SQL frontend — this is an engine-level grammar fix.
#[test]
fn powql_subtraction_in_bare_projection() {
    let (_dir, mut engine) = seeded_engine();
    match engine.execute_powql("T filter .id = 2 { .x - 1 }").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Int(4), "5 - 1 = 4");
        }
        other => panic!("expected rows, got {other:?}"),
    }
}
