//! A non-literal `limit` / `offset` count silently became "unlimited".
//!
//! The generic `Limit` / `Offset` executor arms reject any count that is not an
//! integer literal (`limit must be integer literal`). The projection fast paths
//! did not: they read the count with
//!
//! ```text
//! match limit_expr { Expr::Literal(Literal::Int(v)) if *v >= 0 => *v as usize, _ => usize::MAX }
//! ```
//!
//! so a count they could not read became `usize::MAX` and the query returned the
//! whole table with no error at all. Adding or removing a projection therefore
//! flipped the same query between a clean error and a silently wrong answer.
//!
//! `usize::MAX` also disabled the top-N heap's bound, so a rejected query heaped
//! every raw row of the table outside the sort-row cap and the row-byte budget.
//!
//! The rule pinned here: a count the engine cannot evaluate is an error, and it
//! is the *same* error whether or not the query carries a projection.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQUE_DIR: AtomicU64 = AtomicU64::new(0);

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "powdb_slicecount_{tag}_{}_{}",
        std::process::id(),
        UNIQUE_DIR.fetch_add(1, Ordering::Relaxed),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn fixture(dir: &std::path::Path) -> Engine {
    let mut engine = Engine::new(dir).unwrap();
    engine.execute_powql("type F { id: int, k: int }").unwrap();
    for id in 1..=5i64 {
        engine
            .execute_powql(&format!("insert F {{ id := {id}, k := {id} }}"))
            .unwrap();
    }
    engine
}

fn error_of(engine: &mut Engine, query: &str) -> String {
    match engine.execute_powql(query) {
        Err(e) => e.to_string(),
        Ok(QueryResult::Rows { rows, .. }) => panic!(
            "`{query}` should have been rejected, but returned {} rows",
            rows.len()
        ),
        Ok(other) => panic!("`{query}` should have been rejected, but returned {other:?}"),
    }
}

/// Every plan shape whose fast path read the count directly. The `{ .id }`
/// suffix is what put the query on the fast path; the bare form always went
/// through the generic arm and always errored.
const NON_LITERAL_SHAPES: &[&str] = &[
    // Project(Limit(SeqScan))
    "F limit 1 + 1",
    // Project(Limit(Filter(SeqScan)))
    "F filter .k > 0 limit 1 + 1",
    // Project(Limit(Sort(SeqScan))) — the top-N heap
    "F order .k limit 1 + 1",
    "F order .k desc limit 1 + 1",
    // Project(Limit(Sort(Filter(SeqScan))))
    "F filter .k > 0 order .k limit 1 + 1",
];

#[test]
fn a_non_literal_limit_is_rejected_with_or_without_a_projection() {
    let dir = fresh_dir("limit");
    let mut engine = fixture(&dir);
    for shape in NON_LITERAL_SHAPES {
        let bare = error_of(&mut engine, shape);
        let projected = error_of(&mut engine, &format!("{shape} {{ .id }}"));
        assert_eq!(
            bare, projected,
            "`{shape}` and `{shape} {{ .id }}` must fail identically"
        );
        assert!(
            bare.contains("limit must be integer literal"),
            "`{shape}`: expected the integer-literal error, got `{bare}`"
        );
    }
}

#[test]
fn a_non_literal_offset_is_rejected_with_or_without_a_projection() {
    let dir = fresh_dir("offset");
    let mut engine = fixture(&dir);
    for shape in [
        "F offset 1 + 1",
        "F limit 2 offset 1 + 1",
        "F order .k limit 2 offset 1 + 1",
    ] {
        let bare = error_of(&mut engine, shape);
        let projected = error_of(&mut engine, &format!("{shape} {{ .id }}"));
        assert_eq!(
            bare, projected,
            "`{shape}` and `{shape} {{ .id }}` must fail identically"
        );
        assert!(
            bare.contains("offset must be integer literal"),
            "`{shape}`: expected the integer-literal error, got `{bare}`"
        );
    }
}

/// A rejected count must not be reachable as "unlimited". This is the shape
/// that also bypassed the sort-row cap and the row-byte budget: the top-N heap
/// bound came from the same `usize::MAX`.
#[test]
fn a_rejected_limit_never_returns_the_whole_table() {
    let dir = fresh_dir("whole_table");
    let mut engine = fixture(&dir);
    for shape in NON_LITERAL_SHAPES {
        assert!(
            engine.execute_powql(&format!("{shape} {{ .id }}")).is_err(),
            "`{shape} {{ .id }}` returned rows instead of rejecting the count"
        );
    }
}

/// Literal counts must still take the fast paths and answer as before.
#[test]
fn literal_counts_are_unaffected() {
    let dir = fresh_dir("literal");
    let mut engine = fixture(&dir);
    let count = |engine: &mut Engine, query: &str| match engine.execute_powql(query) {
        Ok(QueryResult::Rows { rows, .. }) => rows.len(),
        other => panic!("`{query}`: {other:?}"),
    };
    assert_eq!(count(&mut engine, "F limit 2 { .id }"), 2);
    assert_eq!(count(&mut engine, "F filter .k > 0 limit 2 { .id }"), 2);
    assert_eq!(count(&mut engine, "F order .k limit 2 { .id }"), 2);
    assert_eq!(count(&mut engine, "F order .k desc limit 2 { .id }"), 2);
    assert_eq!(count(&mut engine, "F limit 2 offset 1 { .id }"), 2);
    assert_eq!(count(&mut engine, "F limit 0 { .id }"), 0);
}
