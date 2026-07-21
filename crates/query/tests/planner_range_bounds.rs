//! Regression tests for planner range-bound extraction (v0.18.0 P0s).
//!
//! Bug F: an AND of two same-side bounds on one column (`.v > 1 and .v >= 9`)
//! silently dropped the second conjunct because `try_extract_range_index_keys`
//! kept only the first bound per side.
//!
//! Bug G: for the upper-bound-first spelling (`.v < B and .v > A`) the planner
//! built `RangeScan { start: A, end: B }` with the literals in reverse source
//! order, but the plan cache substitutes literals in source-text order while
//! the substitution walk visits start-then-end. Every warm execution of that
//! shape ran with the bounds swapped.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|error| panic!("failed `{query}`: {error}"))
}

fn ids(engine: &mut Engine, query: &str) -> Vec<i64> {
    let QueryResult::Rows { rows, columns } = exec(engine, query) else {
        panic!("expected rows for `{query}`");
    };
    let id_col = columns
        .iter()
        .position(|c| c == "id")
        .expect("id column present");
    let mut out: Vec<i64> = rows
        .into_iter()
        .map(|row| match &row[id_col] {
            Value::Int(id) => *id,
            other => panic!("expected int id, got {other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

fn scalar(engine: &mut Engine, query: &str) -> i64 {
    match exec(engine, query) {
        QueryResult::Scalar(Value::Int(n)) => n,
        QueryResult::Rows { rows, .. } if rows.len() == 1 && rows[0].len() == 1 => {
            match &rows[0][0] {
                Value::Int(n) => *n,
                other => panic!("non-int scalar {other:?}"),
            }
        }
        other => panic!("expected scalar for `{query}`, got {other:?}"),
    }
}

fn explain_text(engine: &mut Engine, query: &str) -> String {
    let QueryResult::Rows { rows, .. } = exec(engine, query) else {
        panic!("expected EXPLAIN rows");
    };
    rows.into_iter()
        .filter_map(|row| match row.into_iter().next() {
            Some(Value::Str(line)) => Some(line),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `T { id, v }` with rows (1,1), (2,5), (3,9).
fn fresh_small(with_index: bool) -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    exec(&mut engine, "type T { required id: int, required v: int }");
    exec(&mut engine, "insert T { id := 1, v := 1 }");
    exec(&mut engine, "insert T { id := 2, v := 5 }");
    exec(&mut engine, "insert T { id := 3, v := 9 }");
    if with_index {
        exec(&mut engine, "alter T add index .v");
    }
    (dir, engine)
}

/// `R { id, v }` with v in {-100, -60, 0, 40}.
fn fresh_signed(with_index: bool) -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    exec(&mut engine, "type R { required id: int, required v: int }");
    exec(&mut engine, "insert R { id := 1, v := -100 }");
    exec(&mut engine, "insert R { id := 2, v := -60 }");
    exec(&mut engine, "insert R { id := 3, v := 0 }");
    exec(&mut engine, "insert R { id := 4, v := 40 }");
    if with_index {
        exec(&mut engine, "alter R add index .v");
    }
    (dir, engine)
}

// ---------------------------------------------------------------------------
// Bug F: same-side bounds must both apply.
// ---------------------------------------------------------------------------

#[test]
fn same_side_lower_bounds_keep_both_conjuncts_unindexed() {
    let (_dir, mut engine) = fresh_small(false);
    // Cold, then warm (second run goes through the plan cache).
    for _ in 0..2 {
        assert_eq!(
            ids(&mut engine, "T filter .v > 1 and .v >= 9 { .id }"),
            vec![3],
            "two lower bounds: the tighter one (>= 9) must not be dropped"
        );
    }
}

#[test]
fn same_side_upper_bounds_keep_both_conjuncts_unindexed() {
    let (_dir, mut engine) = fresh_small(false);
    for _ in 0..2 {
        assert_eq!(
            ids(&mut engine, "T filter .v < 9 and .v <= 1 { .id }"),
            vec![1],
            "two upper bounds: the tighter one (<= 1) must not be dropped"
        );
    }
}

#[test]
fn same_side_bounds_keep_both_conjuncts_indexed() {
    let (_dir, mut engine) = fresh_small(true);
    for _ in 0..2 {
        assert_eq!(
            ids(&mut engine, "T filter .v > 1 and .v >= 9 { .id }"),
            vec![3],
        );
        assert_eq!(
            ids(&mut engine, "T filter .v < 9 and .v <= 1 { .id }"),
            vec![1],
        );
    }
}

#[test]
fn same_side_bounds_plan_keeps_a_residual_recheck() {
    // Without an index, the executed plan must be Filter over SeqScan with
    // the full predicate, never a lossy merged RangeScan.
    let (_dir, mut engine) = fresh_small(false);
    let plan = explain_text(&mut engine, "explain T filter .v > 1 and .v >= 9 { .id }");
    assert!(
        plan.contains("SeqScan") && plan.contains("Filter"),
        "same-side bounds without an index must run Filter(SeqScan): {plan}"
    );
    assert!(
        !plan.contains("RangeScan"),
        "same-side bounds must not silently merge into one RangeScan: {plan}"
    );

    // With an index, runtime lowering may drive one bound through the index,
    // but the other bound must survive as a residual Filter.
    let (_dir2, mut indexed) = fresh_small(true);
    let plan = explain_text(&mut indexed, "explain T filter .v > 1 and .v >= 9 { .id }");
    assert!(
        plan.contains("Filter"),
        "the second same-side bound must survive as a residual recheck: {plan}"
    );
}

#[test]
fn same_side_bounds_sql_frontend() {
    let (_dir, mut engine) = fresh_small(true);
    for _ in 0..2 {
        let result = engine
            .execute_sql("SELECT id FROM T WHERE v > 1 AND v >= 9")
            .unwrap();
        let QueryResult::Rows { rows, .. } = result else {
            panic!("expected rows");
        };
        assert_eq!(rows.len(), 1, "SQL same-side bounds must keep both");
        assert_eq!(rows[0][0], Value::Int(3));
    }
}

// ---------------------------------------------------------------------------
// Bug G: warm executions of the upper-bound-first spelling must not swap
// the bounds during plan-cache literal substitution.
// ---------------------------------------------------------------------------

#[test]
fn warm_reversed_bound_order_is_not_poisoned_indexed() {
    let (_dir, mut engine) = fresh_signed(true);
    // Cold: caches the shape. Empty range, so 0.
    assert_eq!(
        scalar(&mut engine, "count(R filter .v < -78 and .v > -48)"),
        0
    );
    // Warm: same shape, wide range. Poisoned cache swapped bounds -> 0.
    assert_eq!(
        scalar(&mut engine, "count(R filter .v < 100 and .v > -200)"),
        4,
        "warm execution of the reversed-bound shape must not swap bounds"
    );
    // Warm again with the original literals.
    assert_eq!(
        scalar(&mut engine, "count(R filter .v < -78 and .v > -48)"),
        0,
        "warm re-run of the cold literals must stay correct"
    );
}

#[test]
fn warm_reversed_bound_order_is_not_poisoned_unindexed() {
    // RangeScan is speculative (planner is catalog-pure), so the poisoning
    // is independent of whether the index exists.
    let (_dir, mut engine) = fresh_signed(false);
    assert_eq!(
        scalar(&mut engine, "count(R filter .v < -78 and .v > -48)"),
        0
    );
    assert_eq!(
        scalar(&mut engine, "count(R filter .v < 100 and .v > -200)"),
        4,
    );
    assert_eq!(
        scalar(&mut engine, "count(R filter .v < -78 and .v > -48)"),
        0
    );
}

#[test]
fn warm_reversed_bound_order_sql_frontend() {
    let (_dir, mut engine) = fresh_signed(true);
    let count = |engine: &mut Engine, sql: &str| -> i64 {
        match engine.execute_sql(sql).unwrap() {
            QueryResult::Scalar(Value::Int(n)) => n,
            QueryResult::Rows { rows, .. } if rows.len() == 1 && rows[0].len() == 1 => {
                match &rows[0][0] {
                    Value::Int(n) => *n,
                    other => panic!("non-int count {other:?}"),
                }
            }
            other => panic!("expected count, got {other:?}"),
        }
    };
    assert_eq!(
        count(
            &mut engine,
            "SELECT COUNT(*) FROM R WHERE v < -78 AND v > -48"
        ),
        0
    );
    assert_eq!(
        count(
            &mut engine,
            "SELECT COUNT(*) FROM R WHERE v < 100 AND v > -200"
        ),
        4,
        "SQL frontend shares the plan cache and must not swap bounds warm"
    );
    assert_eq!(
        count(
            &mut engine,
            "SELECT COUNT(*) FROM R WHERE v < -78 AND v > -48"
        ),
        0
    );
}

#[test]
fn reversed_bound_spelling_still_uses_the_index() {
    // Refusing to cache-poison must not cost the index: the executed plan for
    // `x < B and x > A` on an indexed column is still a two-sided RangeScan.
    let (_dir, mut engine) = fresh_signed(true);
    let plan = explain_text(
        &mut engine,
        "explain R filter .v < 100 and .v > -200 { .id }",
    );
    assert!(
        plan.contains("RangeScan"),
        "reversed spelling on an indexed column must still range-scan: {plan}"
    );
}

// ---------------------------------------------------------------------------
// The legitimate lower-then-upper range must be unaffected.
// ---------------------------------------------------------------------------

#[test]
fn normal_two_sided_range_still_index_backed_and_correct() {
    let (_dir, mut engine) = fresh_signed(true);
    let plan = explain_text(
        &mut engine,
        "explain R filter .v > -200 and .v < 100 { .id }",
    );
    assert!(
        plan.contains("RangeScan"),
        "canonical `x > A and x < B` must keep its RangeScan: {plan}"
    );
    assert!(
        !plan.contains("SeqScan"),
        "canonical range on an indexed column must not fall back to SeqScan: {plan}"
    );

    // Cold, then warm with different literals, then warm with the originals.
    assert_eq!(
        scalar(&mut engine, "count(R filter .v > -200 and .v < 100)"),
        4
    );
    assert_eq!(
        scalar(&mut engine, "count(R filter .v > -78 and .v < -48)"),
        1
    );
    assert_eq!(
        scalar(&mut engine, "count(R filter .v > -200 and .v < 100)"),
        4
    );
}
