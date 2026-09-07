//! Q29: a type whose rows can never fit a page is refused at DDL time.
//!
//! A row lives in one 4KB page and cannot be split across pages: overflow pages
//! hold an individual large VALUE, not a row's own header. A schema wide enough
//! that its null bitmap, fixed region and offset table alone exceed the page's
//! row budget therefore accepts every `type` statement and then refuses every
//! `insert`, including one that sets nothing at all. The table existed, the
//! catalog carried it, and it could never hold a row.

use powdb_query::executor::Engine;

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(dir.path()).unwrap();
    (dir, engine)
}

/// `type W { required unique id: int, c0: int, c1: int, ... }` with `n` int
/// columns after the key.
fn wide_type(name: &str, n: usize) -> String {
    let mut out = format!("type {name} {{ required unique id: int");
    for i in 0..n {
        out.push_str(&format!(", c{i}: int"));
    }
    out.push_str(" }");
    out
}

#[test]
fn a_type_whose_rows_can_never_fit_is_refused() {
    let (_dir, mut engine) = engine();
    let message = engine
        .execute_powql(&wide_type("W", 600))
        .map(|ok| panic!("a type that can hold no row must be refused, got {ok:?}"))
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("601 columns") && message.contains("4070"),
        "the refusal must name the width and the budget, got {message}"
    );
    // Nothing was created, so the name is still free.
    engine.execute_powql(&wide_type("W", 10)).unwrap();
}

#[test]
fn a_type_that_fits_still_works() {
    let (_dir, mut engine) = engine();
    engine.execute_powql(&wide_type("W", 400)).unwrap();
    engine.execute_powql("insert W { id := 1 }").unwrap();
    engine
        .execute_powql("insert W { id := 2, c0 := 7 }")
        .unwrap();
}

#[test]
fn adding_a_column_past_the_budget_is_refused() {
    let (_dir, mut engine) = engine();
    engine.execute_powql(&wide_type("W", 480)).unwrap();
    engine.execute_powql("insert W { id := 1 }").unwrap();
    let mut refusal = None;
    for i in 0..80 {
        if let Err(e) = engine.execute_powql(&format!("alter W add column x{i}: int")) {
            refusal = Some(e.to_string());
            break;
        }
    }
    let message = refusal.expect("adding columns forever must eventually be refused");
    assert!(
        message.contains("4070"),
        "the refusal must name the budget, got {message}"
    );
    // The refusal left the table exactly as it was: it still reads and still
    // takes a row.
    engine.execute_powql("count(W)").unwrap();
    engine.execute_powql("insert W { id := 2 }").unwrap();
}

#[test]
fn the_sql_frontend_refuses_it_too() {
    let (_dir, mut engine) = engine();
    let mut sql = String::from("create table W (id int primary key");
    for i in 0..600 {
        sql.push_str(&format!(", c{i} int"));
    }
    sql.push(')');
    assert!(engine.execute_sql(&sql).is_err());
}
