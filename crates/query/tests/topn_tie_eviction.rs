//! Descending top-N (`order .k desc limit N`) evicted the wrong row among ties.
//!
//! `project_filter_sort_limit_fast` keeps the N best rows in a bounded heap and
//! evicts whatever sits at the heap's top once it is full. The final order the
//! fast path then produces is key-descending, then scan-sequence **ascending**,
//! so the row that deserves eviction among rows tied on the boundary key is the
//! one with the *largest* sequence. The descending heap was ordered so that its
//! top was the *smallest* sequence, i.e. exactly the row that should have been
//! kept. `limit N` therefore returned a different row set, not merely a
//! different order, from the first N rows of the same unlimited query.
//!
//! The contract these tests pin: for any N, `order K limit N` must equal the
//! first N rows of `order K`. That is checked against the generic sort path
//! (which never enters the heap) and against the same query with fast paths
//! switched off entirely.

#![cfg(feature = "testing")]

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQUE_DIR: AtomicU64 = AtomicU64::new(0);

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "powdb_topn_{tag}_{}_{}",
        std::process::id(),
        UNIQUE_DIR.fetch_add(1, Ordering::Relaxed),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("failed to execute `{query}`: {e}"))
}

/// The single `.id` column of every result row, in result order.
fn ids(engine: &mut Engine, query: &str) -> Vec<i64> {
    match exec(engine, query) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|row| match row.first() {
                Some(Value::Int(n)) => *n,
                other => panic!("`{query}`: expected an Int id, got {other:?}"),
            })
            .collect(),
        other => panic!("`{query}`: expected rows, got {other:?}"),
    }
}

/// Three groups of three rows tied on `k` (and on `kf`), inserted in ascending
/// id order so scan sequence and id agree. Every limit from 1 to 9 therefore
/// lands either inside a tied group or exactly on its boundary.
fn tied_groups(dir: &std::path::Path) -> Engine {
    let mut engine = Engine::new(dir).unwrap();
    exec(&mut engine, "type S { id: int, k: int, kf: float }");
    for (id, k) in [
        (1, 10),
        (2, 10),
        (3, 10),
        (4, 20),
        (5, 20),
        (6, 20),
        (7, 30),
        (8, 30),
        (9, 30),
    ] {
        exec(
            &mut engine,
            &format!("insert S {{ id := {id}, k := {k}, kf := {k}.5 }}"),
        );
    }
    engine
}

/// `order K limit N` must be the first N rows of `order K`, for every N.
/// The unlimited query has no `Limit` node, so it never reaches the top-N heap:
/// it is the independent ground truth.
fn assert_limit_is_a_prefix_of_the_full_order(engine: &mut Engine, order: &str, rows: usize) {
    let full = ids(engine, &format!("S order {order} {{ .id }}"));
    assert_eq!(full.len(), rows, "fixture size for `order {order}`");
    for n in 1..=rows {
        let limited = ids(engine, &format!("S order {order} limit {n} {{ .id }}"));
        assert_eq!(
            limited,
            full[..n],
            "`order {order} limit {n}` must be the first {n} rows of `order {order}`"
        );
    }
}

#[test]
fn descending_top_n_over_an_int_key_is_a_prefix_of_the_full_order() {
    let dir = fresh_dir("desc_int");
    let mut engine = tied_groups(&dir);
    assert_limit_is_a_prefix_of_the_full_order(&mut engine, ".k desc", 9);
}

#[test]
fn descending_top_n_over_a_float_key_is_a_prefix_of_the_full_order() {
    let dir = fresh_dir("desc_float");
    let mut engine = tied_groups(&dir);
    assert_limit_is_a_prefix_of_the_full_order(&mut engine, ".kf desc", 9);
}

/// The ascending heap was already correct; keep it that way.
#[test]
fn ascending_top_n_is_a_prefix_of_the_full_order() {
    let dir = fresh_dir("asc");
    let mut engine = tied_groups(&dir);
    assert_limit_is_a_prefix_of_the_full_order(&mut engine, ".k", 9);
    assert_limit_is_a_prefix_of_the_full_order(&mut engine, ".kf", 9);
}

/// The exact shape reported: a boundary tie of three rows, one strictly larger
/// key. `limit 2` dropped id 1 and kept id 2; `limit 3` dropped id 1 entirely
/// and admitted id 3, so the returned *set* was wrong, not just its order.
#[test]
fn descending_top_n_evicts_the_last_tied_row_not_the_first() {
    let dir = fresh_dir("boundary");
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type S { id: int, k: int }");
    for (id, k) in [(10, 3), (11, 3), (12, 3), (13, 5)] {
        exec(&mut engine, &format!("insert S {{ id := {id}, k := {k} }}"));
    }

    assert_eq!(
        ids(&mut engine, "S order .k desc { .id }"),
        vec![13, 10, 11, 12]
    );
    assert_eq!(
        ids(&mut engine, "S order .k desc limit 2 { .id }"),
        vec![13, 10]
    );
    assert_eq!(
        ids(&mut engine, "S order .k desc limit 3 { .id }"),
        vec![13, 10, 11]
    );
    assert_eq!(
        ids(&mut engine, "S order .k desc limit 4 { .id }"),
        vec![13, 10, 11, 12]
    );
    // Larger than the table.
    assert_eq!(
        ids(&mut engine, "S order .k desc limit 99 { .id }"),
        vec![13, 10, 11, 12]
    );
}

/// `offset 0` is a no-op, but the `Offset` node it inserts blocks the top-N
/// fast path, so a query that answered correctly with `offset 0` answered
/// differently without it. Any divergence here is a fast-path bug by
/// construction.
#[test]
fn a_no_op_offset_does_not_change_the_answer() {
    let dir = fresh_dir("offset_zero");
    let mut engine = tied_groups(&dir);
    for order in [".k desc", ".kf desc", ".k", ".kf"] {
        for n in 1..=9 {
            assert_eq!(
                ids(&mut engine, &format!("S order {order} limit {n} {{ .id }}")),
                ids(
                    &mut engine,
                    &format!("S order {order} limit {n} offset 0 {{ .id }}")
                ),
                "`order {order} limit {n}` must not depend on a no-op `offset 0`"
            );
        }
    }
}

/// A filter under the sort reaches the same heap through the
/// `Sort(Filter(SeqScan))` arm.
#[test]
fn filtered_descending_top_n_is_a_prefix_of_the_full_order() {
    let dir = fresh_dir("filtered");
    let mut engine = tied_groups(&dir);
    let full = ids(&mut engine, "S filter .id > 2 order .k desc { .id }");
    assert_eq!(full.len(), 7);
    for n in 1..=7 {
        assert_eq!(
            ids(
                &mut engine,
                &format!("S filter .id > 2 order .k desc limit {n} {{ .id }}")
            ),
            full[..n],
            "filtered `order .k desc limit {n}` must be the first {n} rows"
        );
    }
}

/// The strongest form: run every limit with the fast paths on and again with
/// them forced off, and require identical answers. `forced_generic_sites`
/// proves the top-N fast path is really the code being diverted, a comparison
/// that never declined anywhere would be comparing one path with itself.
#[test]
fn top_n_agrees_with_the_generic_path() {
    let dir = fresh_dir("equiv");
    let mut engine = tied_groups(&dir);

    let mut saw_top_n_decline = false;
    for order in [".k desc", ".kf desc", ".k", ".kf"] {
        for n in 1..=9 {
            let query = format!("S order {order} limit {n} {{ .id }}");

            engine.set_force_generic_path(false);
            let fast = ids(&mut engine, &query);

            engine.set_force_generic_path(true);
            engine.reset_forced_generic_sites();
            let generic = ids(&mut engine, &query);
            saw_top_n_decline |= engine
                .forced_generic_sites()
                .contains(&"project-filter-sort-limit");
            engine.set_force_generic_path(false);

            assert_eq!(
                fast, generic,
                "fast and generic paths disagree on `{query}`"
            );
        }
    }
    assert!(
        saw_top_n_decline,
        "no query in this test reached the top-N fast path, so nothing was compared"
    );
}
