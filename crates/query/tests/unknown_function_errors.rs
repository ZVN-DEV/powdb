//! Q9: calling a function PowQL does not have says so.
//!
//! An identifier followed by `(` was parsed as a bare column and the `(` was
//! left for whatever came next, so `T { x: foo(.id) }` reported
//! `column 'foo' not found`, `T filter foo(.id) = 1` reported an unexpected
//! `(`, and a two-argument call like `date_trunc(.t, "day")` reported
//! `expected ')', got ','`. Three unrelated messages for one mistake, none of
//! them saying that the function does not exist.

use powdb_query::executor::Engine;

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required unique id: int, s: str }")
        .unwrap();
    engine
        .execute_powql("insert T { id := 1, s := \"a\" }")
        .unwrap();
    (dir, engine)
}

fn message(engine: &mut Engine, query: &str) -> String {
    engine
        .execute_powql(query)
        .map(|ok| panic!("`{query}` should have been refused, got {ok:?}"))
        .unwrap_err()
        .to_string()
}

#[test]
fn an_unknown_function_is_named_wherever_it_is_called() {
    let (_dir, mut engine) = engine();
    for query in [
        "T { x: foo(.id) }",
        "T filter foo(.id) = 1 { .id }",
        "T order foo(.id) { .id }",
        "T { x: date_trunc(.id, \"day\") }",
    ] {
        let message = message(&mut engine, query);
        assert!(
            message.contains("unknown function"),
            "`{query}` was refused with {message}"
        );
    }
}

#[test]
fn a_near_miss_gets_a_suggestion() {
    let (_dir, mut engine) = engine();
    let message = message(&mut engine, "T { x: uppr(.s) }");
    assert!(
        message.contains("upper"),
        "expected a suggestion, got {message}"
    );
}

#[test]
fn the_functions_that_exist_still_run() {
    let (_dir, mut engine) = engine();
    for query in [
        "T { x: upper(.s) }",
        "T { x: length(.s) }",
        "T { x: substring(.s, 1, 1) }",
        "T { x: abs(.id) }",
        "T { x: uuid(\"00000000-0000-0000-0000-000000000000\") }",
        "count(T)",
    ] {
        engine
            .execute_powql(query)
            .unwrap_or_else(|e| panic!("{query}: {e}"));
    }
}

#[test]
fn a_bare_identifier_is_still_a_column_reference() {
    let (_dir, mut engine) = engine();
    // `t.s` (a qualified reference) and a bare name are untouched: only a name
    // immediately followed by `(` is read as a call.
    engine.execute_powql("T as t { t.s }").unwrap();
}

#[test]
fn the_sql_frontend_names_it_too() {
    let (_dir, mut engine) = engine();
    let message = engine
        .execute_sql("select foo(id) from T")
        .map(|ok| panic!("should have been refused, got {ok:?}"))
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("foo"),
        "the refusal must name the function, got {message}"
    );
}
