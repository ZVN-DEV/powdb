//! Q2: a float written into an `int` column is refused unless it is exactly an
//! integer.
//!
//! `coerce_value` used `*v as i64`, which truncates toward zero and saturates
//! at the ends of the range, so `30.7` was stored as 30, `-0.5` as 0 and `1e300`
//! as `i64::MAX`. A string in the same position is refused, so the column
//! rejected the wrong value and silently rewrote the near-miss one, on insert
//! and on update alike.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required unique id: int, n: int }")
        .unwrap();
    (dir, engine)
}

fn stored_n(engine: &mut Engine) -> Vec<Value> {
    match engine.execute_powql("T { .n }").unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|row| row[0].clone()).collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn a_fractional_float_is_refused_on_insert() {
    let (_dir, mut engine) = engine();
    let message = engine
        .execute_powql("insert T { id := 1, n := 30.7 }")
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("'n'"),
        "expected a typed error naming the column, got {message}"
    );
    assert!(stored_n(&mut engine).is_empty(), "nothing was written");
}

#[test]
fn a_negative_fraction_is_refused_rather_than_stored_as_zero() {
    let (_dir, mut engine) = engine();
    assert!(engine
        .execute_powql("insert T { id := 1, n := -0.5 }")
        .is_err());
    assert!(stored_n(&mut engine).is_empty());
}

#[test]
fn an_out_of_range_float_is_refused_rather_than_saturated() {
    let (_dir, mut engine) = engine();
    assert!(engine
        .execute_powql("insert T { id := 1, n := 1e300 }")
        .is_err());
    assert!(stored_n(&mut engine).is_empty());
}

#[test]
fn an_exact_integral_float_is_still_accepted() {
    let (_dir, mut engine) = engine();
    engine
        .execute_powql("insert T { id := 1, n := 30.0 }")
        .unwrap();
    assert_eq!(stored_n(&mut engine), vec![Value::Int(30)]);
}

#[test]
fn an_update_refuses_the_same_value_the_insert_refuses() {
    let (_dir, mut engine) = engine();
    engine
        .execute_powql("insert T { id := 1, n := 1 }")
        .unwrap();
    assert!(engine
        .execute_powql("T filter .id = 1 update { n := 30.7 }")
        .is_err());
    assert_eq!(stored_n(&mut engine), vec![Value::Int(1)]);
}

#[test]
fn the_sql_frontend_refuses_it_too() {
    let (_dir, mut engine) = engine();
    assert!(engine
        .execute_sql("insert into T (id, n) values (1, 30.7)")
        .is_err());
    assert!(stored_n(&mut engine).is_empty());
}
