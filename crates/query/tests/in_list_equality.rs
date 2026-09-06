//! Q26: `x in (a, b)` evaluates exactly as `x = a or x = b`.
//!
//! The list membership test compared with `Value`'s strictly typed `PartialEq`
//! while `=` compares numerically across `Int` and `Float`, so the two operators
//! disagreed on the same pair of values: `.n = 28.0` matched a stored int 28
//! and `.n in (28.0)` did not, and `.f in (42)` missed a stored float 42.0.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn seeded(indexed: bool) -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required unique id: int, n: int, f: float, s: str }")
        .unwrap();
    if indexed {
        engine.execute_powql("alter T add index .n").unwrap();
        engine.execute_powql("alter T add index .f").unwrap();
    }
    for (id, n, f, s) in [(1i64, 28i64, 42.0f64, "a"), (2, 29, 43.5, "b")] {
        engine
            .execute_powql(&format!(
                "insert T {{ id := {id}, n := {n}, f := {f}, s := \"{s}\" }}"
            ))
            .unwrap();
    }
    (dir, engine)
}

fn count(engine: &mut Engine, query: &str) -> i64 {
    match engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("{query}: {e}"))
    {
        QueryResult::Scalar(Value::Int(n)) => n,
        QueryResult::Rows { rows, .. } => match rows[0][0] {
            Value::Int(n) => n,
            ref v => panic!("expected int, got {v:?}"),
        },
        other => panic!("expected a count, got {other:?}"),
    }
}

/// `in` and the `or` chain of `=` it stands for must answer the same, on every
/// access path.
#[test]
fn in_agrees_with_the_equality_chain_it_stands_for() {
    for indexed in [false, true] {
        let (_dir, mut engine) = seeded(indexed);
        for (list, chain) in [
            ("(28.0)", "(.n = 28.0)"),
            ("(28)", "(.n = 28)"),
            ("(28.5)", "(.n = 28.5)"),
            ("(28.0, 29)", "(.n = 28.0 or .n = 29)"),
        ] {
            let via_in = count(&mut engine, &format!("count(T filter .n in {list})"));
            let via_eq = count(&mut engine, &format!("count(T filter {chain})"));
            assert_eq!(
                via_in, via_eq,
                "`.n in {list}` vs `{chain}` (indexed={indexed})"
            );
        }
    }
}

#[test]
fn an_int_literal_matches_a_float_column_through_in() {
    for indexed in [false, true] {
        let (_dir, mut engine) = seeded(indexed);
        assert_eq!(
            count(&mut engine, "count(T filter .f in (42))"),
            1,
            "indexed={indexed}"
        );
    }
}

#[test]
fn a_float_literal_matches_an_int_column_through_in() {
    for indexed in [false, true] {
        let (_dir, mut engine) = seeded(indexed);
        assert_eq!(
            count(&mut engine, "count(T filter .n in (28.0))"),
            1,
            "indexed={indexed}"
        );
    }
}

#[test]
fn not_in_is_the_complement_of_in_over_present_values() {
    let (_dir, mut engine) = seeded(false);
    assert_eq!(count(&mut engine, "count(T filter .n in (28.0))"), 1);
    assert_eq!(count(&mut engine, "count(T filter .n not in (28.0))"), 1);
}

#[test]
fn a_string_list_still_matches_only_strings() {
    let (_dir, mut engine) = seeded(false);
    assert_eq!(count(&mut engine, "count(T filter .s in (\"a\"))"), 1);
    assert_eq!(count(&mut engine, "count(T filter .s in (\"z\"))"), 0);
}
