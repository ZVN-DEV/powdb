//! Q1 and Q4: integer overflow and a runtime zero divisor are errors, not a
//! silent NULL.
//!
//! The expression evaluator had no error channel, so an unrepresentable result
//! became the empty set: `.big * 2` produced NULL in a projection, filtered
//! nothing out in a predicate, and an `update` wrote that NULL into a required
//! column. `sum` over the same values refused the same overflow outright and
//! `/ 0` written as a literal was refused at validation, so three spellings of
//! one question gave three different answers.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required unique id: int, required n: int, d: int }")
        .unwrap();
    engine
        .execute_powql("insert T { id := 1, n := 9223372036854775807, d := 0 }")
        .unwrap();
    engine
        .execute_powql("insert T { id := 2, n := 2, d := 2 }")
        .unwrap();
    (dir, engine)
}

fn err(engine: &mut Engine, query: &str) -> String {
    engine
        .execute_powql(query)
        .map(|ok| panic!("`{query}` should have failed, got {ok:?}"))
        .unwrap_err()
        .to_string()
}

fn stored_n(engine: &mut Engine) -> Vec<Value> {
    match engine.execute_powql("T { .n } order .id").unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|row| row[0].clone()).collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn overflow_in_a_projection_is_an_error() {
    let (_dir, mut engine) = engine();
    let message = err(&mut engine, "T { x: .n * 2 }");
    assert!(
        message.contains("overflow"),
        "expected an overflow error, got {message}"
    );
}

#[test]
fn overflow_in_a_filter_is_an_error() {
    let (_dir, mut engine) = engine();
    let message = err(&mut engine, "count(T filter .n + 1 > 0)");
    assert!(
        message.contains("overflow"),
        "expected an overflow error, got {message}"
    );
}

#[test]
fn overflow_reports_the_same_way_a_sum_overflow_does() {
    let (_dir, mut engine) = engine();
    engine
        .execute_powql("insert T { id := 3, n := 9223372036854775807, d := 1 }")
        .unwrap();
    let projected = err(&mut engine, "T { x: .n + .n }");
    let summed = err(&mut engine, "sum(T { .n })");
    assert!(projected.contains("overflow"), "{projected}");
    assert!(summed.contains("overflow"), "{summed}");
}

#[test]
fn an_update_does_not_write_an_overflowed_value() {
    let (_dir, mut engine) = engine();
    assert!(engine
        .execute_powql("T filter .id = 1 update { n := .n * 2 }")
        .is_err());
    assert_eq!(
        stored_n(&mut engine),
        vec![Value::Int(i64::MAX), Value::Int(2)],
        "the required column must not have been set to null"
    );
}

#[test]
fn a_runtime_zero_divisor_errors_like_a_literal_one() {
    let (_dir, mut engine) = engine();
    let literal = err(&mut engine, "T { x: .n / 0 }");
    let runtime = err(&mut engine, "T { x: .n / .d }");
    assert!(literal.contains("divide by zero"), "{literal}");
    assert!(runtime.contains("divide by zero"), "{runtime}");
}

#[test]
fn an_update_does_not_write_a_division_by_zero() {
    let (_dir, mut engine) = engine();
    assert!(engine
        .execute_powql("T filter .id = 1 update { n := .n / .d }")
        .is_err());
    assert_eq!(
        stored_n(&mut engine),
        vec![Value::Int(i64::MAX), Value::Int(2)]
    );
}

#[test]
fn arithmetic_that_fits_is_unaffected() {
    let (_dir, mut engine) = engine();
    match engine.execute_powql("T filter .id = 2 { x: .n * 2, y: .n / .d }") {
        Ok(QueryResult::Rows { rows, .. }) => {
            assert_eq!(rows[0][0], Value::Int(4));
            assert_eq!(rows[0][1], Value::Int(1));
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn a_fault_does_not_leak_into_the_next_statement() {
    let (_dir, mut engine) = engine();
    assert!(engine.execute_powql("T { x: .n * 2 }").is_err());
    assert!(
        engine.execute_powql("count(T)").is_ok(),
        "the next statement must not inherit the fault"
    );
}

#[test]
fn the_read_only_path_reports_it_too() {
    let (_dir, engine) = engine();
    let message = engine
        .execute_powql_readonly("T { x: .n * 2 }")
        .unwrap_err()
        .to_string();
    assert!(message.contains("overflow"), "{message}");
}
