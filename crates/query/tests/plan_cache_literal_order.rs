//! The plan cache re-binds a repeated query's literals to the wrong slots.
//!
//! `canonicalize` collects literals in SOURCE order and `substitute_plan`
//! consumes them in PLAN-WALK order, and the two disagree wherever the planner
//! reorders clauses. `{ ... } limit N` is the everyday case: the projection is
//! written first but the planner puts `Project` ON TOP of `Limit`, so the walk
//! reached the limit first and bound the projection's literal to it. The first
//! execution of a query was right and every later one silently answered a
//! different question: `D { x: .n + 1 } limit 5` returned `.n + 5`, and
//! `filter .n > 1 { x: .n + 1 } limit 5 offset 0` returned no rows at all
//! because the offset became 5.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type D { required unique id: int, n: int }")
        .unwrap();
    for (id, n) in [(1, 3), (2, 10), (3, 20)] {
        engine
            .execute_powql(&format!("insert D {{ id := {id}, n := {n} }}"))
            .unwrap();
    }
    (dir, engine)
}

fn rows(engine: &mut Engine, query: &str) -> Vec<Vec<Value>> {
    match engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("{query}: {e}"))
    {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Running the same query three times must answer the same thing three times.
fn stable(engine: &mut Engine, query: &str) -> Vec<Vec<Value>> {
    let first = rows(engine, query);
    for run in 2..=3 {
        assert_eq!(
            rows(engine, query),
            first,
            "run {run} of `{query}` answered differently from the first"
        );
    }
    first
}

#[test]
fn a_projection_literal_survives_a_limit() {
    let (_dir, mut engine) = engine();
    assert_eq!(
        stable(&mut engine, "D { x: .n + 1 } limit 5"),
        vec![
            vec![Value::Int(4)],
            vec![Value::Int(11)],
            vec![Value::Int(21)]
        ]
    );
}

#[test]
fn a_projection_literal_survives_a_limit_and_an_offset() {
    let (_dir, mut engine) = engine();
    assert_eq!(
        stable(
            &mut engine,
            "D filter .n > 1 { x: .n + 1 } limit 5 offset 0"
        ),
        vec![
            vec![Value::Int(4)],
            vec![Value::Int(11)],
            vec![Value::Int(21)]
        ]
    );
    assert_eq!(
        stable(
            &mut engine,
            "D filter .n > 1 { x: .n + 1 } limit 1 offset 1"
        ),
        vec![vec![Value::Int(11)]]
    );
}

#[test]
fn a_projection_literal_survives_a_bare_offset() {
    let (_dir, mut engine) = engine();
    assert_eq!(
        stable(&mut engine, "D { x: .n + 1 } offset 2"),
        vec![vec![Value::Int(21)]]
    );
}

#[test]
fn the_same_shape_with_other_literals_gets_its_own() {
    let (_dir, mut engine) = engine();
    assert_eq!(
        stable(&mut engine, "D { x: .n + 1 } limit 5"),
        vec![
            vec![Value::Int(4)],
            vec![Value::Int(11)],
            vec![Value::Int(21)]
        ]
    );
    assert_eq!(
        stable(&mut engine, "D { x: .n + 100 } limit 2"),
        vec![vec![Value::Int(103)], vec![Value::Int(110)]]
    );
    assert_eq!(
        stable(&mut engine, "D { x: .n + 1 } limit 5"),
        vec![
            vec![Value::Int(4)],
            vec![Value::Int(11)],
            vec![Value::Int(21)]
        ]
    );
}

/// The projection may be written before or after `order`, and the plan keeps
/// no record of which; the cache must not guess.
#[test]
fn a_projection_literal_and_a_sort_literal_do_not_swap() {
    let (_dir, mut engine) = engine();
    assert_eq!(
        stable(&mut engine, "D { x: .n + 1 } order .n + 2"),
        vec![
            vec![Value::Int(4)],
            vec![Value::Int(11)],
            vec![Value::Int(21)]
        ]
    );
    assert_eq!(
        stable(&mut engine, "D order .n + 2 { x: .n + 1 }"),
        vec![
            vec![Value::Int(4)],
            vec![Value::Int(11)],
            vec![Value::Int(21)]
        ]
    );
}

#[test]
fn the_shapes_that_were_already_right_stay_right() {
    let (_dir, mut engine) = engine();
    assert_eq!(
        stable(&mut engine, "D filter .n > 5 { .id } limit 1"),
        vec![vec![Value::Int(2)]]
    );
    assert_eq!(stable(&mut engine, "D filter .n = 10 { .id }").len(), 1);
    assert_eq!(stable(&mut engine, "D { .id } limit 2 offset 1").len(), 2);
}
