//! Q28: `not` over a missing value is the plain complement, like every other
//! `not`.
//!
//! docs/POWQL.md states one rule: filter logic is two-valued, so `not (p)`
//! matches whenever `p` is false, and a comparison against a missing value is
//! false. `not (.b = true)` followed it and included the rows with no `b`, but
//! `not .b` (the same question, one word shorter) evaluated the missing value
//! to the empty set and excluded them. The two spellings of one predicate gave
//! two answers.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required unique id: int, b: bool, n: int }")
        .unwrap();
    engine
        .execute_powql("insert T { id := 1, b := true, n := 1 }")
        .unwrap();
    engine
        .execute_powql("insert T { id := 2, b := false, n := 2 }")
        .unwrap();
    engine.execute_powql("insert T { id := 3 }").unwrap();
    (dir, engine)
}

fn ids(engine: &mut Engine, query: &str) -> Vec<i64> {
    match engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("{query}: {e}"))
    {
        QueryResult::Rows { rows, .. } => {
            let mut out: Vec<i64> = rows
                .iter()
                .map(|row| match row[0] {
                    Value::Int(n) => n,
                    ref other => panic!("expected an int id, got {other:?}"),
                })
                .collect();
            out.sort_unstable();
            out
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn the_two_spellings_of_one_complement_agree() {
    let (_dir, mut engine) = engine();
    assert_eq!(
        ids(&mut engine, "T filter not .b { .id }"),
        ids(&mut engine, "T filter not (.b = true) { .id }"),
        "`not .b` and `not (.b = true)` are the same predicate"
    );
    assert_eq!(ids(&mut engine, "T filter not .b { .id }"), [2, 3]);
}

#[test]
fn a_bare_bool_predicate_is_unchanged() {
    let (_dir, mut engine) = engine();
    assert_eq!(ids(&mut engine, "T filter .b { .id }"), [1]);
    assert_eq!(ids(&mut engine, "T filter .b = true { .id }"), [1]);
}

#[test]
fn presence_guards_still_narrow_it() {
    let (_dir, mut engine) = engine();
    assert_eq!(
        ids(&mut engine, "T filter exists .b and not .b { .id }"),
        [2],
        "guarding presence excludes the missing row again"
    );
}

#[test]
fn the_operator_forms_keep_their_own_rule() {
    let (_dir, mut engine) = engine();
    // `!=` is an operator, not a complement: a missing value never matches it.
    assert_eq!(ids(&mut engine, "T filter .b != true { .id }"), [2]);
    assert_eq!(ids(&mut engine, "T filter .n != 1 { .id }"), [2]);
    assert_eq!(ids(&mut engine, "T filter .b is null { .id }"), [3]);
}

#[test]
fn every_access_path_agrees() {
    let (_dir, mut engine) = engine();
    let compiled = ids(&mut engine, "T filter not .b { .id }");
    engine.set_force_generic_path(true);
    assert_eq!(compiled, ids(&mut engine, "T filter not .b { .id }"));
    engine.set_force_generic_path(false);
    engine.execute_powql("alter T add index .b").unwrap();
    assert_eq!(compiled, ids(&mut engine, "T filter not .b { .id }"));
}

#[test]
fn the_sql_frontend_agrees() {
    let (_dir, mut engine) = engine();
    let powql = ids(&mut engine, "T filter not .b { .id }");
    match engine.execute_sql("select id from T where not b").unwrap() {
        QueryResult::Rows { rows, .. } => {
            let mut sql: Vec<i64> = rows
                .iter()
                .map(|row| match row[0] {
                    Value::Int(n) => n,
                    ref other => panic!("{other:?}"),
                })
                .collect();
            sql.sort_unstable();
            assert_eq!(powql, sql);
        }
        other => panic!("expected rows, got {other:?}"),
    }
}
