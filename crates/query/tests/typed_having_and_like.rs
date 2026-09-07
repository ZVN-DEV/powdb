//! Q3: `having` and `like` refuse a mistyped operand, the way `filter` already
//! does.
//!
//! `filter .n = "abc"` on an int column is a type error before a row is read.
//! The same mistake written as `having count(.id) = "abc"` or `.n like "abc"`
//! evaluated to false on every row and returned nothing, so the three spellings
//! of one mistake gave two different answers and the silent one looked like an
//! empty table.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required unique id: int, n: int, s: str, b: bool }")
        .unwrap();
    engine
        .execute_powql("insert T { id := 1, n := 5, s := \"abc\", b := true }")
        .unwrap();
    engine
        .execute_powql("insert T { id := 2, n := 6, s := \"abd\", b := false }")
        .unwrap();
    (dir, engine)
}

fn err(engine: &mut Engine, query: &str) -> String {
    engine
        .execute_powql(query)
        .map(|ok| panic!("`{query}` should have been refused, got {ok:?}"))
        .unwrap_err()
        .to_string()
}

fn ids(engine: &mut Engine, query: &str) -> Vec<i64> {
    match engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("{query}: {e}"))
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|row| match row[0] {
                Value::Int(n) => n,
                ref other => panic!("expected an int, got {other:?}"),
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn like_against_a_non_text_column_is_a_type_error() {
    let (_dir, mut engine) = engine();
    for query in [
        "T filter .n like \"abc\" { .id }",
        "T filter .b like \"a%\" { .id }",
    ] {
        let message = err(&mut engine, query);
        assert!(
            message.contains("like"),
            "`{query}` was refused with {message}"
        );
    }
}

#[test]
fn like_with_a_non_text_pattern_is_a_type_error() {
    let (_dir, mut engine) = engine();
    for query in [
        "T filter .s like 5 { .id }",
        "T filter .s like true { .id }",
    ] {
        let message = err(&mut engine, query);
        assert!(
            message.contains("like"),
            "`{query}` was refused with {message}"
        );
    }
}

#[test]
fn like_between_two_text_operands_still_matches() {
    let (_dir, mut engine) = engine();
    assert_eq!(ids(&mut engine, "T filter .s like \"ab%\" { .id }"), [1, 2]);
    assert_eq!(ids(&mut engine, "T filter .s like \"abc\" { .id }"), [1]);
    assert_eq!(ids(&mut engine, "T filter .s like .s { .id }"), [1, 2]);
}

#[test]
fn having_on_an_aggregate_refuses_a_mistyped_literal() {
    let (_dir, mut engine) = engine();
    for query in [
        "T group .s having count(.id) = \"x\" { .s }",
        "T group .s having count(.id) > \"x\" { .s }",
        "T group .s having sum(.n) = true { .s }",
        "T group .s having avg(.n) = \"x\" { .s }",
    ] {
        let message = err(&mut engine, query);
        assert!(
            message.contains("type mismatch"),
            "`{query}` was refused with {message}"
        );
    }
}

#[test]
fn having_on_a_min_or_max_keeps_the_argument_type() {
    let (_dir, mut engine) = engine();
    let message = err(&mut engine, "T group .b having max(.s) = 5 { .b }");
    assert!(
        message.contains("type mismatch"),
        "max over a str column is a str: {message}"
    );
    assert_eq!(
        ids(
            &mut engine,
            "T group .b having max(.n) = 6 { c: count(.id) }"
        ),
        [1]
    );
}

#[test]
fn having_on_a_group_key_refuses_what_filter_refuses() {
    let (_dir, mut engine) = engine();
    let filtered = err(&mut engine, "T filter .s = 5 { .id }");
    let grouped = err(&mut engine, "T group .s having .s = 5 { .s }");
    assert_eq!(
        filtered, grouped,
        "the same mistake must read the same in both clauses"
    );
}

#[test]
fn a_well_typed_having_still_answers() {
    let (_dir, mut engine) = engine();
    assert_eq!(
        ids(
            &mut engine,
            "T group .b having count(.id) = 1 { c: count(.id) }"
        ),
        [1, 1]
    );
    assert_eq!(
        ids(
            &mut engine,
            "T group .b having count(.id) > 1 { c: count(.id) }"
        ),
        Vec::<i64>::new()
    );
}

#[test]
fn the_sql_frontend_refuses_the_same_two() {
    let (_dir, mut engine) = engine();
    for query in [
        "select s from T where n like 'abc'",
        "select s from T group by s having count(id) = 'x'",
    ] {
        engine
            .execute_sql(query)
            .map(|ok| panic!("`{query}` should have been refused, got {ok:?}"))
            .unwrap_err();
    }
}
