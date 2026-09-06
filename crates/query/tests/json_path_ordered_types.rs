//! Q25: an ordered comparison between a JSON scalar and a value of another
//! type is false, not decided by type rank.
//!
//! `Value::Ord`'s tail arm compares the two type discriminants when it has no
//! rule for the pair, and both the generic evaluator and the compiled JSON leaf
//! took that answer, so `.j->v > 99.5` returned the rows whose value was the
//! string "deep" and the bool true. `>` therefore answered a question about
//! numbers with rows that hold no number, on every access path, with no error.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

/// One row per JSON scalar type, so every cross-type pair is present.
fn seeded(indexed: bool) -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type D { required unique id: int, j: json }")
        .unwrap();
    for (id, doc) in [
        (1, r#"{ \"v\": 1 }"#),
        (2, r#"{ \"v\": 100.5 }"#),
        (3, r#"{ \"v\": \"deep\" }"#),
        (4, r#"{ \"v\": true }"#),
        (5, r#"{ \"v\": null }"#),
    ] {
        engine
            .execute_powql(&format!("insert D {{ id := {id}, j := \"{doc}\" }}"))
            .unwrap();
    }
    if indexed {
        engine.execute_powql("alter D add index (.j->v)").unwrap();
    }
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
                    ref v => panic!("expected int id, got {v:?}"),
                })
                .collect();
            out.sort_unstable();
            out
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn an_ordered_comparison_against_a_number_matches_only_numbers() {
    for indexed in [false, true] {
        let (_dir, mut engine) = seeded(indexed);
        assert_eq!(
            ids(&mut engine, "D filter .j->v > 99.5 { .id }"),
            vec![2],
            "indexed={indexed}"
        );
        assert_eq!(
            ids(&mut engine, "D filter .j->v < 99.5 { .id }"),
            vec![1],
            "indexed={indexed}"
        );
    }
}

#[test]
fn an_ordered_comparison_against_a_string_matches_only_strings() {
    for indexed in [false, true] {
        let (_dir, mut engine) = seeded(indexed);
        assert_eq!(
            ids(&mut engine, "D filter .j->v > \"a\" { .id }"),
            vec![3],
            "indexed={indexed}"
        );
    }
}

#[test]
fn the_generic_evaluator_answers_the_same_as_the_compiled_leaf() {
    let (_dir, mut engine) = seeded(false);
    let compiled = ids(&mut engine, "D filter .j->v >= 1 { .id }");
    engine.set_force_generic_path(true);
    let generic = ids(&mut engine, "D filter .j->v >= 1 { .id }");
    assert_eq!(compiled, generic);
    assert_eq!(compiled, vec![1, 2]);
}

#[test]
fn equality_across_types_is_still_simply_false() {
    let (_dir, mut engine) = seeded(false);
    assert_eq!(
        ids(&mut engine, "D filter .j->v = 99.5 { .id }"),
        Vec::new()
    );
    assert_eq!(ids(&mut engine, "D filter .j->v = 1 { .id }"), vec![1]);
}
