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

/// SQL `INSERT ... RETURNING *` returns the inserted row, so an ORM gets the
/// row back in one round-trip instead of a write followed by a reselect. PowQL
/// already supports `insert ... returning`; this wires the SQL surface to it.
#[test]
fn sql_insert_returning_star_yields_inserted_row() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_sql("CREATE TABLE User (id INTEGER NOT NULL UNIQUE, name TEXT)")
        .unwrap();
    match engine
        .execute_sql("INSERT INTO User (id, name) VALUES (1, 'Ada') RETURNING *")
        .unwrap()
    {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["id", "name"]);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Int(1));
            assert_eq!(rows[0][1], Value::Str("Ada".into()));
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The createMany case: a multi-row `INSERT ... VALUES (..),(..) RETURNING *`
/// returns every inserted row in one statement (one round-trip, one fsync).
#[test]
fn sql_multi_row_insert_returning_star_yields_all_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_sql("CREATE TABLE User (id INTEGER NOT NULL UNIQUE, name TEXT)")
        .unwrap();
    match engine
        .execute_sql(
            "INSERT INTO User (id, name) VALUES (1, 'Ada'), (2, 'Grace'), (3, 'Mary') RETURNING *",
        )
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0][0], Value::Int(1));
            assert_eq!(rows[1][1], Value::Str("Grace".into()));
            assert_eq!(rows[2][0], Value::Int(3));
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// A plain `INSERT` (no RETURNING) still reports an affected-row count rather
/// than materializing rows — RETURNING stays opt-in.
#[test]
fn sql_insert_without_returning_reports_modified() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_sql("CREATE TABLE User (id INTEGER NOT NULL UNIQUE, name TEXT)")
        .unwrap();
    match engine
        .execute_sql("INSERT INTO User (id, name) VALUES (1, 'Ada')")
        .unwrap()
    {
        QueryResult::Modified(n) => assert_eq!(n, 1),
        other => panic!("expected Modified, got {other:?}"),
    }
}

/// `UPDATE ... SET ... WHERE ... RETURNING *` returns the post-image of the
/// updated rows.
#[test]
fn sql_update_returning_star_yields_post_image() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_sql("CREATE TABLE User (id INTEGER NOT NULL UNIQUE, age INTEGER)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO User (id, age) VALUES (1, 37)")
        .unwrap();
    match engine
        .execute_sql("UPDATE User SET age = 38 WHERE id = 1 RETURNING *")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Int(1));
            assert_eq!(rows[0][1], Value::Int(38), "post-image: the new age");
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// `UPDATE` with no `WHERE` still parses a trailing `RETURNING *` (the clause
/// must not be swallowed into the SET assignments).
#[test]
fn sql_update_returning_without_where() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_sql("CREATE TABLE User (id INTEGER NOT NULL UNIQUE, age INTEGER)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO User (id, age) VALUES (1, 37), (2, 40)")
        .unwrap();
    match engine
        .execute_sql("UPDATE User SET age = 50 RETURNING *")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert!(rows.iter().all(|r| r[1] == Value::Int(50)));
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// `DELETE ... WHERE ... RETURNING *` returns the pre-image of the deleted rows.
#[test]
fn sql_delete_returning_star_yields_pre_image() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_sql("CREATE TABLE User (id INTEGER NOT NULL UNIQUE, name TEXT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO User (id, name) VALUES (1, 'Ada'), (2, 'Grace')")
        .unwrap();
    match engine
        .execute_sql("DELETE FROM User WHERE id = 2 RETURNING *")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Int(2));
            assert_eq!(rows[0][1], Value::Str("Grace".into()), "pre-image");
        }
        other => panic!("expected rows, got {other:?}"),
    }
    // The row is actually gone.
    match engine.execute_sql("SELECT id FROM User").unwrap() {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("expected rows, got {other:?}"),
    }
}

/// PowQL's `returning` is all-columns, so a projected `RETURNING id, name` is
/// rejected with a clear error rather than silently returning every column.
#[test]
fn sql_returning_column_list_is_unsupported() {
    let err = powdb_query::sql::parse_sql(
        "INSERT INTO User (id, name) VALUES (1, 'Ada') RETURNING id, name",
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("column projection"),
        "error should explain the column-projection limitation, got: {err}"
    );
}

// --- Ungrouped aggregates (regression: `SELECT count(*)` must aggregate) ------
//
// Before the fix the SQL frontend lowered `SELECT count(*) FROM T` as a *row
// projection* `T { count(*) }`, so it returned one null row per source row
// instead of a scalar count. `count(*)` is documented (docs/SQL.md) and is the
// README's headline SQL example, so this was a silent wrong-answer bug across
// both the server `QuerySql` path and the embedded addon.

/// `SELECT count(*) FROM T` must lower to PowQL's aggregate form `count(T)`.
#[test]
fn sql_count_star_lowers_to_powql_aggregate() {
    let parsed = powdb_query::sql::parse_sql_with_canonical("SELECT count(*) FROM T").unwrap();
    assert_eq!(parsed.canonical_powql, "count(T)");
}

#[test]
fn sql_count_star_aggregates_to_scalar() {
    let (_dir, mut engine) = seeded_engine();
    match engine.execute_sql("SELECT count(*) FROM T").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 2),
        other => panic!("expected Scalar(2), got {other:?}"),
    }
}

#[test]
fn sql_count_star_with_where_filters_then_counts() {
    let (_dir, mut engine) = seeded_engine();
    match engine
        .execute_sql("SELECT count(*) FROM T WHERE x > 3")
        .unwrap()
    {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
        other => panic!("expected Scalar(1), got {other:?}"),
    }
}

#[test]
fn sql_count_star_alias_still_aggregates() {
    let (_dir, mut engine) = seeded_engine();
    match engine.execute_sql("SELECT count(*) AS n FROM T").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 2),
        other => panic!("expected Scalar(2), got {other:?}"),
    }
}

#[test]
fn sql_sum_aggregates_to_scalar() {
    let (_dir, mut engine) = seeded_engine();
    match engine.execute_sql("SELECT sum(x) FROM T").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 6),
        other => panic!("expected Scalar(6), got {other:?}"),
    }
}

#[test]
fn sql_max_aggregates_to_scalar() {
    let (_dir, mut engine) = seeded_engine();
    match engine.execute_sql("SELECT max(x) FROM T").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 5),
        other => panic!("expected Scalar(5), got {other:?}"),
    }
}

/// PowQL has no single-statement form for several ungrouped aggregates at once,
/// so the SQL frontend must reject it with a clear error rather than silently
/// returning garbage rows.
#[test]
fn sql_multiple_ungrouped_aggregates_error_not_garbage() {
    let (_dir, mut engine) = seeded_engine();
    let r = engine.execute_sql("SELECT count(*), sum(x) FROM T");
    assert!(
        r.is_err(),
        "multiple ungrouped aggregates should error, got: {r:?}"
    );
}

/// Grouped `count(*)` was already correct (the planner extracts it per group);
/// the ungrouped-aggregate fix must not regress it. `x=5` appears twice.
#[test]
fn sql_grouped_count_star_counts_per_group() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required id: int, required x: int }")
        .unwrap();
    engine
        .execute_powql("insert T { id := 1, x := 1 }, { id := 2, x := 5 }, { id := 3, x := 5 }")
        .unwrap();
    match engine
        .execute_sql("SELECT x, count(*) FROM T GROUP BY x ORDER BY x")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0], vec![Value::Int(1), Value::Int(1)]);
            assert_eq!(rows[1], vec![Value::Int(5), Value::Int(2)]);
        }
        other => panic!("expected grouped rows, got {other:?}"),
    }
}
