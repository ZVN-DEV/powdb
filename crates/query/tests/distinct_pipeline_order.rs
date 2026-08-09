//! `distinct` was applied *after* `limit` / `offset`, not before.
//!
//! The planner built one fixed pipeline for every read —
//! `Sort -> Offset -> Limit -> [Window] -> Project -> Distinct` — with
//! `Distinct` outermost. `U distinct limit 3 { .name }` therefore limited to
//! three rows and *then* de-duplicated them, so it could return fewer rows than
//! asked for while distinct rows the query should have reached were never
//! looked at. `distinct limit N` means "N distinct rows", not "the distinct
//! rows among the first N".
//!
//! The rule pinned here: for any N, `distinct limit N` must equal the first N
//! rows of the same query without the limit, and the same for `offset`.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQUE_DIR: AtomicU64 = AtomicU64::new(0);

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "powdb_distinct_{tag}_{}_{}",
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

/// Every result row rendered as a `|`-joined string, in result order.
fn rows(engine: &mut Engine, query: &str) -> Vec<String> {
    match exec(engine, query) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| match value {
                        Value::Str(s) => s.clone(),
                        Value::Int(n) => n.to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("`{query}`: expected rows, got {other:?}"),
    }
}

/// Four rows over three distinct names, with the duplicate pair first so that
/// truncating before de-duplication loses a value that the query asked for.
fn names(dir: &std::path::Path) -> Engine {
    let mut engine = Engine::new(dir).unwrap();
    exec(&mut engine, "type U { id: int, name: str }");
    for (id, name) in [(1, "a"), (2, "a"), (3, "b"), (4, "c")] {
        exec(
            &mut engine,
            &format!(r#"insert U {{ id := {id}, name := "{name}" }}"#),
        );
    }
    engine
}

/// `<head> limit N <tail>` must be the first N rows of `<head> <tail>`, and
/// `offset M` must be the rest after dropping M, for every N and M.
fn assert_slices_match_the_unsliced_query(
    engine: &mut Engine,
    head: &str,
    tail: &str,
    expected: &[&str],
) {
    let full = rows(engine, &format!("{head} {tail}"));
    assert_eq!(full, expected, "`{head} {tail}`");
    for n in 0..=full.len() + 1 {
        assert_eq!(
            rows(engine, &format!("{head} limit {n} {tail}")),
            full[..n.min(full.len())],
            "`{head} limit {n} {tail}` must be the first {n} rows of `{head} {tail}`"
        );
        assert_eq!(
            rows(engine, &format!("{head} offset {n} {tail}")),
            full[n.min(full.len())..],
            "`{head} offset {n} {tail}` must drop the first {n} rows of `{head} {tail}`"
        );
    }
}

#[test]
fn distinct_deduplicates_before_limit() {
    let dir = fresh_dir("limit");
    let mut engine = names(&dir);
    assert_slices_match_the_unsliced_query(
        &mut engine,
        "U distinct",
        "{ .name }",
        &["a", "b", "c"],
    );
}

/// The exact shape reported: `limit 3` over four rows with one duplicate pair
/// returned two rows instead of three.
#[test]
fn distinct_limit_returns_the_number_of_distinct_rows_requested() {
    let dir = fresh_dir("count");
    let mut engine = names(&dir);
    assert_eq!(rows(&mut engine, "U distinct { .name }").len(), 3);
    assert_eq!(rows(&mut engine, "U distinct limit 3 { .name }").len(), 3);
    assert_eq!(rows(&mut engine, "U distinct limit 2 { .name }").len(), 2);
    assert_eq!(rows(&mut engine, "U distinct limit 1 { .name }").len(), 1);
}

/// De-duplication applies to the *projected* output. `.id` is unique, so a
/// projection that carries it makes every row distinct even though `.name`
/// repeats; a projection that drops it collapses the pair.
#[test]
fn distinct_applies_to_the_projected_columns_not_the_source_row() {
    let dir = fresh_dir("projected");
    let mut engine = names(&dir);
    assert_eq!(
        rows(&mut engine, "U distinct { .id, .name }"),
        vec!["1|a", "2|a", "3|b", "4|c"],
        "distinct over a projection carrying the unique .id keeps all four rows"
    );
    assert_eq!(
        rows(&mut engine, "U distinct limit 3 { .id, .name }"),
        vec!["1|a", "2|a", "3|b"]
    );
    assert_eq!(
        rows(&mut engine, "U distinct { .name }"),
        vec!["a", "b", "c"],
        "distinct over .name alone collapses the duplicate pair"
    );
}

/// ORDER BY + DISTINCT + LIMIT together: order, then de-duplicate, then take.
#[test]
fn distinct_with_order_and_limit_follows_standard_semantics() {
    let dir = fresh_dir("ordered");
    let mut engine = names(&dir);
    assert_slices_match_the_unsliced_query(
        &mut engine,
        "U order .name desc distinct",
        "{ .name }",
        &["c", "b", "a"],
    );
    assert_slices_match_the_unsliced_query(
        &mut engine,
        "U order .name distinct",
        "{ .name }",
        &["a", "b", "c"],
    );
}

#[test]
fn distinct_over_a_join_deduplicates_before_limit() {
    let dir = fresh_dir("join");
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type P { pid: int, tag: str }");
    exec(&mut engine, "type Q { qid: int, pid: int }");
    for (pid, tag) in [(1, "x"), (2, "y"), (3, "z")] {
        exec(
            &mut engine,
            &format!(r#"insert P {{ pid := {pid}, tag := "{tag}" }}"#),
        );
    }
    // Two orders against P 1, so the joined output repeats tag "x".
    for (qid, pid) in [(10, 1), (11, 1), (12, 2), (13, 3)] {
        exec(
            &mut engine,
            &format!("insert Q {{ qid := {qid}, pid := {pid} }}"),
        );
    }

    assert_slices_match_the_unsliced_query(
        &mut engine,
        "P join Q on P.pid = Q.pid distinct",
        "{ P.tag }",
        &["x", "y", "z"],
    );
}

#[test]
fn distinct_over_a_grouped_query_deduplicates_before_limit() {
    let dir = fresh_dir("grouped");
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type G { k: int, bucket: int }");
    // Three groups whose counts are 2, 2 and 1: projecting only the count
    // leaves a duplicate that `distinct` must collapse before `limit` bites.
    // Grouping preserves first-seen key order, so the group sequence (and with
    // it the projected counts 2, 2, 1) is deterministic.
    for (k, bucket) in [(1, 7), (2, 7), (3, 8), (4, 8), (5, 9)] {
        exec(
            &mut engine,
            &format!("insert G {{ k := {k}, bucket := {bucket} }}"),
        );
    }
    assert_slices_match_the_unsliced_query(
        &mut engine,
        "G group .bucket distinct",
        "{ n: count(.k) }",
        &["2", "1"],
    );
}

/// A structural guard on the plan itself: `Distinct` must sit *under* the
/// slicing nodes. A future re-ordering of the pipeline that puts it back on top
/// would reintroduce the same wrong answer.
#[test]
fn explain_places_distinct_below_limit_and_offset() {
    let dir = fresh_dir("explain");
    let mut engine = names(&dir);
    let plan = match exec(&mut engine, "explain U distinct limit 3 offset 1 { .name }") {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|row| match &row[0] {
                Value::Str(s) => s.clone(),
                other => panic!("expected a plan line, got {other:?}"),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("expected rows, got {other:?}"),
    };
    let position = |needle: &str| {
        plan.find(needle)
            .unwrap_or_else(|| panic!("`{needle}` missing from plan:\n{plan}"))
    };
    // The tree prints outermost-first, so a smaller offset means further out.
    assert!(
        position("Limit") < position("Distinct"),
        "Limit must wrap Distinct, not the other way round:\n{plan}"
    );
    assert!(
        position("Offset") < position("Distinct"),
        "Offset must wrap Distinct:\n{plan}"
    );
    assert!(
        position("Distinct") < position("Project"),
        "Distinct must de-duplicate projected rows:\n{plan}"
    );
}
