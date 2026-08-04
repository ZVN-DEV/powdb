//! Numeric edges of the ROW evaluator, which is a different evaluator from the
//! aggregate accumulator and has repeatedly been fixed one of the two.
//!
//! v0.21.0 made an integer total that leaves `i64` a query-killing error in the
//! aggregate accumulator and left the row evaluator clamping to `i64::MAX`, so
//! `sum(A { .v })` refused to answer while `A { x: .v + 1 }` answered
//! `9223372036854775807` and called it a result. The row evaluator already had
//! the right convention for arithmetic with no representable answer, in
//! `BinOp::Div`, which returns missing rather than a number: division applied
//! it, addition, subtraction and multiplication did not.
//!
//! The scalar functions in the same evaluator narrow an `i64` argument to `i32`
//! with `as`, which is not a narrowing check but a silent truncation. That
//! produced an infinite `powi` factor inside `round`, and the resulting NaN was
//! not confined to one answer: an `update` wrote it to the column, after which
//! it compared inconsistently on every access path and `max()` reported NaN
//! while `min()` ignored the row.
//!
//! Each test below states the value that must never come back, so a regression
//! names the defect rather than a number.

#![cfg(feature = "testing")]

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQUE_DIR: AtomicU64 = AtomicU64::new(0);

fn fresh_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "powdb_xtype_arith_{}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the unix epoch")
            .as_nanos(),
        UNIQUE_DIR.fetch_add(1, Ordering::Relaxed),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn exec(engine: &mut Engine, statement: &str) {
    engine
        .execute_powql(statement)
        .unwrap_or_else(|err| panic!("statement `{statement}` failed: {err}"));
}

/// One row holding `i64::MAX`, `i64::MIN + 1` and a float, plus the row id.
fn engine_with_edges() -> Engine {
    let mut engine = Engine::new(&fresh_dir()).expect("engine opens over a fresh temp dir");
    exec(
        &mut engine,
        "type A { required unique id: int, big: int, small: int, f: float, s: str }",
    );
    exec(
        &mut engine,
        "insert A { id := 1, big := 9223372036854775807, small := -9223372036854775807, \
         f := 1.5, s := \"abcdef\" }",
    );
    engine
}

/// The single projected value, whatever it is.
fn projected(engine: &mut Engine, query: &str) -> Value {
    match engine.execute_powql(query) {
        Ok(QueryResult::Rows { rows, .. }) => match rows.first().and_then(|row| row.first()) {
            Some(value) => value.clone(),
            None => panic!("`{query}` returned no rows"),
        },
        Ok(other) => panic!("`{query}` returned {other:?}"),
        Err(err) => panic!("`{query}` failed: {err}"),
    }
}

/// Both paths, because a projection over one row is exactly the shape that
/// reaches the generic evaluator while the surrounding query may not.
fn projected_on_both_paths(query: &str) -> Value {
    let mut fast = engine_with_edges();
    let first = projected(&mut fast, query);
    let mut generic = engine_with_edges();
    generic.set_force_generic_path(true);
    let second = projected(&mut generic, query);
    assert_eq!(
        format!("{first:?}"),
        format!("{second:?}"),
        "`{query}` answered differently with fast paths on and off"
    );
    first
}

// ─── integer overflow in the row evaluator ──────────────────────────────────

#[test]
fn integer_overflow_in_a_projection_is_missing_not_a_clamped_number() {
    // `saturating_add` reported `i64::MAX` for a sum that is not `i64::MAX`.
    // Missing is the convention the same evaluator already uses for `Div` with
    // no representable answer, for `abs(i64::MIN)`, and for a `date_add` whose
    // unit multiply overflows.
    for query in [
        "A { x: .big + 1 }",
        "A { x: 1 + .big }",
        "A { x: .small - 2 }",
        "A { x: .big * 2 }",
        "A { x: 2 * .big }",
        "A { x: .big - .small }",
        "A { x: .big * .big }",
    ] {
        assert_eq!(
            projected_on_both_paths(query),
            Value::Empty,
            "`{query}` produced a number for an arithmetic result that has none"
        );
    }
}

#[test]
fn integer_arithmetic_that_fits_is_untouched() {
    // The guard must only catch the results that do not exist, not change the
    // ones that do.
    for (query, expected) in [
        ("A { x: .big - 1 }", Value::Int(9_223_372_036_854_775_806)),
        (
            "A { x: .small + 1 }",
            Value::Int(-9_223_372_036_854_775_806),
        ),
        ("A { x: .big * 1 }", Value::Int(9_223_372_036_854_775_807)),
        ("A { x: 2 * 3 }", Value::Int(6)),
        ("A { x: .f + 1 }", Value::Float(2.5)),
        ("A { x: .big + 0.0 }", Value::Float(9.223372036854776e18)),
    ] {
        assert_eq!(
            projected_on_both_paths(query),
            expected,
            "`{query}` changed a representable answer"
        );
    }
}

// ─── scalar functions that narrowed i64 to i32 with `as` ────────────────────

#[test]
fn round_never_produces_a_non_finite_number() {
    // `*d as i32` truncated the decimal count, so 2147483647 raised 10 to an
    // infinite power, `f * inf` was inf, and `inf / inf` was NaN.
    for query in [
        "A { x: round(.f, 2147483647) }",
        "A { x: round(.f, 9223372036854775807) }",
        "A { x: round(.f, -2147483648) }",
        "A { x: round(.f, -9223372036854775807) }",
        "A { x: round(.f, 400) }",
        "A { x: round(.f, -400) }",
    ] {
        match projected_on_both_paths(query) {
            Value::Float(f) => assert!(
                f.is_finite(),
                "`{query}` produced {f}, which is not a number a column can hold"
            ),
            other => panic!("`{query}` produced {other:?}"),
        }
    }
}

#[test]
fn round_still_rounds() {
    for (query, expected) in [
        ("A { x: round(.f, 0) }", 2.0),
        ("A { x: round(.f, 1) }", 1.5),
        ("A { x: round(.f, 2) }", 1.5),
        // Rounding to more decimals than an f64 carries is the identity, and
        // rounding to a magnitude larger than the value is zero.
        ("A { x: round(.f, 100) }", 1.5),
        ("A { x: round(.f, -100) }", 0.0),
        // A truncating `as i32` does not only produce infinite factors, which a
        // finiteness guard would catch on its own. It also maps a huge count
        // onto a SMALL one, which produces a perfectly finite wrong answer:
        // `4294967296 as i32` is 0, so this rounded to whole numbers and
        // answered 2.0. These cells are why the range check has to be a check
        // and not a rescue.
        ("A { x: round(.f, 4294967296) }", 1.5),
        ("A { x: round(.f, 4294967298) }", 1.5),
        ("A { x: round(.f, -4294967296) }", 0.0),
    ] {
        match projected_on_both_paths(query) {
            Value::Float(f) => assert_eq!(f, expected, "`{query}` rounded wrongly"),
            other => panic!("`{query}` produced {other:?}"),
        }
    }
}

#[test]
fn a_non_finite_round_result_never_reaches_durable_storage() {
    // The NaN did not stay inside one answer: an `update` wrote it to the
    // column, and from then on `filter .f > -99999.0` returned the row on every
    // access path while `min()` skipped it, so the table itself was poisoned.
    let mut engine = engine_with_edges();
    exec(&mut engine, "A update { f := round(.f, 2147483647) }");
    let stored = projected(&mut engine, "A { x: .f }");
    match stored {
        Value::Float(f) => assert!(f.is_finite(), "a NaN was stored in a float column"),
        // Missing is acceptable; a NaN is not.
        Value::Empty => {}
        other => panic!("unexpected stored value {other:?}"),
    }
}

#[test]
fn pow_does_not_truncate_its_exponent() {
    // `*exp as i32` wrapped 4294967298 to 2, so `pow(2.0, 4294967298)` answered
    // 4.0: not an overflow, a different question silently answered.
    match projected_on_both_paths("A { x: pow(2.0, 4294967298) }") {
        Value::Float(f) => assert!(
            f.is_infinite() && f.is_sign_positive(),
            "pow answered {f} for an exponent past i32"
        ),
        other => panic!("pow produced {other:?}"),
    }
    match projected_on_both_paths("A { x: pow(2.0, -4294967298) }") {
        Value::Float(f) => assert_eq!(f, 0.0, "pow answered {f} for an exponent past i32"),
        other => panic!("pow produced {other:?}"),
    }
    // The ordinary cases keep their answers.
    assert_eq!(
        projected_on_both_paths("A { x: pow(2.0, 10) }"),
        Value::Float(1024.0)
    );
    assert_eq!(
        projected_on_both_paths("A { x: pow(2, 10) }"),
        Value::Int(1024)
    );
}

#[test]
fn substring_rejects_a_negative_length() {
    // `*len as usize` turned -1 into `usize::MAX`, so a negative length
    // returned the whole rest of the string instead of nothing.
    assert_eq!(
        projected_on_both_paths("A { x: substring(.s, 2, -1) }"),
        Value::Empty,
        "a negative substring length returned characters"
    );
    assert_eq!(
        projected_on_both_paths("A { x: substring(.s, 2, -9223372036854775807) }"),
        Value::Empty,
        "a negative substring length returned characters"
    );
    // The ordinary cases keep their answers.
    assert_eq!(
        projected_on_both_paths("A { x: substring(.s, 2, 3) }"),
        Value::Str("bcd".to_string())
    );
    assert_eq!(
        projected_on_both_paths("A { x: substring(.s, 1, 0) }"),
        Value::Str(String::new())
    );
    assert_eq!(
        projected_on_both_paths("A { x: substring(.s, 2, 9223372036854775807) }"),
        Value::Str("bcdef".to_string())
    );
}
