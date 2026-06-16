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
