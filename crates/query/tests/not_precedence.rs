//! Regression tests for prefix `not` precedence (v0.18.0 finding A).
//!
//! docs/POWQL.md puts `not` at precedence level 4, looser than the
//! comparison operators at level 3, and the SQL frontend already parses
//! `NOT v > 0` as `NOT (v > 0)`. The PowQL parser instead consumed prefix
//! `not` in `parse_primary` (the tightest level), so `not .v > 0` parsed
//! as `(not .v) > 0` and returned zero rows.

use powdb_query::executor::Engine;
use powdb_query::parser::parse;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|error| panic!("failed `{query}`: {error}"))
}

fn ids(result: QueryResult) -> Vec<i64> {
    let QueryResult::Rows { rows, columns } = result else {
        panic!("expected rows");
    };
    let id_col = columns
        .iter()
        .position(|c| c == "id")
        .expect("id column present");
    let mut out: Vec<i64> = rows
        .into_iter()
        .map(|row| match &row[id_col] {
            Value::Int(id) => *id,
            other => panic!("expected int id, got {other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

fn fresh() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    exec(&mut engine, "type T { required id: int, required v: int }");
    exec(&mut engine, "insert T { id := 1, v := -5 }");
    exec(&mut engine, "insert T { id := 2, v := 3 }");
    exec(&mut engine, "insert T { id := 3, v := 0 }");
    (dir, engine)
}

#[test]
fn prefix_not_binds_looser_than_comparison() {
    // Per the documented precedence table, `not .v > 0` == `not (.v > 0)`.
    assert_eq!(
        parse("T filter not .v > 0").unwrap(),
        parse("T filter not (.v > 0)").unwrap(),
        "prefix `not` must bind looser than comparison"
    );
}

#[test]
fn explicit_parens_keep_the_tight_binding() {
    // `(not .v) > 0` must stay writable and must differ from `not (.v > 0)`.
    assert_ne!(
        parse("T filter (not .v) > 0").unwrap(),
        parse("T filter not (.v > 0)").unwrap(),
    );
}

#[test]
fn prefix_not_binds_tighter_than_and() {
    // Level 4 `not` vs level 5 `and`: `not A and B` == `(not A) and B`.
    assert_eq!(
        parse("T filter not .v = 1 and .id = 2").unwrap(),
        parse("T filter (not (.v = 1)) and .id = 2").unwrap(),
    );
}

#[test]
fn not_exists_still_parses() {
    assert!(parse("T filter not exists .v").is_ok());
    assert!(parse("User filter not exists (VIP)").is_ok());
}

#[test]
fn not_comparison_returns_complement_rows() {
    let (_dir, mut engine) = fresh();
    // v > 0 matches id 2 only; the complement is ids 1 and 3.
    assert_eq!(
        ids(exec(&mut engine, "T filter not .v > 0 { .id }")),
        vec![1, 3],
        "`not .v > 0` must negate the comparison, not the field"
    );
    assert_eq!(
        ids(exec(&mut engine, "T filter not (.v > 0) { .id }")),
        vec![1, 3],
    );
}

#[test]
fn powql_and_sql_frontends_agree_on_not() {
    let (_dir, mut engine) = fresh();
    let powql = ids(exec(&mut engine, "T filter not .v > 0 { .id }"));
    let sql = ids(engine
        .execute_sql("SELECT id FROM T WHERE NOT v > 0")
        .unwrap());
    assert_eq!(powql, sql, "the two frontends must agree on NOT precedence");
    assert_eq!(sql, vec![1, 3]);
}
