//! Q21: refreshing a materialized view reclaims the previous materialization
//! instead of appending a fresh copy beside it.
//!
//! A refresh deleted every backing row and re-inserted the new ones. A slotted
//! page marks a deleted slot but never moves `free_start` back, so the bytes of
//! every previous copy stayed in the file: in a 30 minute mixed run a 1 MB base
//! table produced a 113 MB view heap, write p50 went from 67 ms to 700 ms and
//! throughput from 117 to 12 writes per second, with no error anywhere.
//!
//! The property held here is that the backing heap stays proportional to the
//! view's own contents across repeated read-after-write cycles, and that a read
//! with no intervening write does no work at all.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use std::path::{Path, PathBuf};

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "powdb_matview_space_{name}_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("{query}: {e}"))
}

fn heap_len(dir: &Path, table: &str) -> u64 {
    std::fs::metadata(dir.join(format!("{table}.heap")))
        .map(|meta| meta.len())
        .unwrap_or(0)
}

/// 60 rows, a view over all of them, and one read so the view is clean.
fn seed(engine: &mut Engine) {
    exec(engine, "type T { required unique id: int, n: int }");
    for id in 0..60i64 {
        exec(engine, &format!("insert T {{ id := {id}, n := {id} }}"));
    }
    exec(engine, "materialize V as T { .id, .n }");
    exec(engine, "V");
}

#[test]
fn repeated_read_after_write_cycles_leave_the_view_heap_flat() {
    let dir = fresh_dir("flat");
    let mut engine = Engine::new(&dir).unwrap();
    seed(&mut engine);
    let baseline = heap_len(&dir, "V");
    assert!(
        baseline > 0,
        "the view heap should exist after the first read"
    );

    // The base table keeps a constant row count, so every refresh writes the
    // same number of view rows. Only unreclaimed previous copies can make the
    // file grow.
    for cycle in 0..25i64 {
        exec(&mut engine, &format!("T filter .id = {cycle} delete"));
        exec(
            &mut engine,
            &format!("insert T {{ id := {}, n := {cycle} }}", 1_000 + cycle),
        );
        exec(&mut engine, "V");
    }

    let after = heap_len(&dir, "V");
    assert!(
        after <= baseline * 2,
        "25 refreshes of a constant-size view grew its heap from {baseline} to \
         {after} bytes; the previous materialization is not being reclaimed"
    );
}

#[test]
fn a_read_with_no_intervening_write_does_not_rewrite_the_view() {
    let dir = fresh_dir("noop");
    let mut engine = Engine::new(&dir).unwrap();
    seed(&mut engine);
    let baseline = heap_len(&dir, "V");
    for _ in 0..10 {
        exec(&mut engine, "V");
    }
    assert_eq!(
        heap_len(&dir, "V"),
        baseline,
        "reading a clean view must not rewrite its backing table"
    );
}

#[test]
fn a_refreshed_view_still_answers_what_its_source_answers() {
    let dir = fresh_dir("answers");
    let mut engine = Engine::new(&dir).unwrap();
    seed(&mut engine);
    for cycle in 0..5i64 {
        exec(&mut engine, &format!("T filter .id = {cycle} delete"));
        exec(
            &mut engine,
            &format!("insert T {{ id := {}, n := {cycle} }}", 1_000 + cycle),
        );
        let view = exec(&mut engine, "V { .id, .n } order .id");
        let source = exec(&mut engine, "T { .id, .n } order .id");
        assert_eq!(
            format!("{view:?}"),
            format!("{source:?}"),
            "the view disagrees with its source after cycle {cycle}"
        );
    }
}

#[test]
fn the_reclaimed_view_survives_a_restart() {
    let dir = fresh_dir("restart");
    {
        let mut engine = Engine::new(&dir).unwrap();
        seed(&mut engine);
        for cycle in 0..5i64 {
            exec(&mut engine, &format!("T filter .id = {cycle} delete"));
            exec(&mut engine, "V");
        }
    }
    let mut engine = Engine::new(&dir).unwrap();
    let view = exec(&mut engine, "V { .id, .n } order .id");
    let source = exec(&mut engine, "T { .id, .n } order .id");
    assert_eq!(format!("{view:?}"), format!("{source:?}"));
}
