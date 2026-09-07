//! Q18: a float literal can be written with an exponent.
//!
//! `1e308` was not a float literal at all: the lexer read `1` and then the
//! identifier `e308`, so the query failed with a parse error about a stray
//! name. Every magnitude past what fits in plain decimal notation, and every
//! small one, had to be written out in full, and a value copied out of a result
//! set could not be pasted back into a query.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required unique id: int, f: float }")
        .unwrap();
    (dir, engine)
}

fn stored(engine: &mut Engine) -> Vec<Value> {
    match engine.execute_powql("T order .id { .f }").unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|row| row[0].clone()).collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn an_exponent_literal_is_a_float() {
    let (_dir, mut engine) = engine();
    for (id, literal, expected) in [
        (1, "1e308", 1e308_f64),
        (2, "1.5E-7", 1.5E-7_f64),
        (3, "2e+10", 2e10_f64),
        (4, "-3.25e2", -3.25e2_f64),
        (5, "1e0", 1.0_f64),
    ] {
        engine
            .execute_powql(&format!("insert T {{ id := {id}, f := {literal} }}"))
            .unwrap_or_else(|e| panic!("insert of {literal}: {e}"));
        let _ = expected;
    }
    assert_eq!(
        stored(&mut engine),
        vec![
            Value::Float(1e308),
            Value::Float(1.5E-7),
            Value::Float(2e10),
            Value::Float(-3.25e2),
            Value::Float(1.0),
        ]
    );
}

#[test]
fn an_exponent_literal_compares_as_a_number() {
    let (_dir, mut engine) = engine();
    engine
        .execute_powql("insert T { id := 1, f := 1e10 }")
        .unwrap();
    engine
        .execute_powql("insert T { id := 2, f := 1e-10 }")
        .unwrap();
    match engine.execute_powql("count(T filter .f > 1e5)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
        other => panic!("expected a count, got {other:?}"),
    }
}

#[test]
fn a_literal_with_no_finite_value_is_refused() {
    let (_dir, mut engine) = engine();
    let message = engine
        .execute_powql("insert T { id := 1, f := 1e400 }")
        .unwrap_err()
        .to_string();
    assert!(message.contains("out of range"), "got {message}");
}

#[test]
fn a_bare_e_is_still_not_part_of_the_number() {
    let (_dir, mut engine) = engine();
    // `1e` is an int followed by a name, and the parser says so rather than
    // reading a float that was never written.
    assert!(engine
        .execute_powql("insert T { id := 1, f := 1e }")
        .is_err());
}

#[test]
fn plain_decimal_literals_are_unchanged() {
    let (_dir, mut engine) = engine();
    engine
        .execute_powql("insert T { id := 1, f := 3.25 }")
        .unwrap();
    engine
        .execute_powql("insert T { id := 2, f := -0.5 }")
        .unwrap();
    assert_eq!(
        stored(&mut engine),
        vec![Value::Float(3.25), Value::Float(-0.5)]
    );
}

#[test]
fn the_sql_frontend_takes_it_too() {
    let (_dir, mut engine) = engine();
    engine
        .execute_sql("insert into T (id, f) values (1, 2.5e3)")
        .unwrap();
    assert_eq!(stored(&mut engine), vec![Value::Float(2500.0)]);
}
