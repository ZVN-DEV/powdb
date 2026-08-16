//! A materialized view's dirty flag has to survive the process that set it.
//!
//! The registry already had an on-disk format carrying the flag, and
//! `register`/`unregister` wrote it, but the three functions that *change* it
//! (`mark_dirty`, `mark_clean`, `mark_dependents_dirty`) only touched memory.
//! So the marker was correct for exactly as long as the process lived:
//!
//! ```text
//! powdb-cli -c 'type E { required id: int }'
//! powdb-cli -c 'insert E { id := 1 }'
//! powdb-cli -c 'materialize V as E { .id }'
//! powdb-cli -c 'E filter .id = 1 delete'
//! powdb-cli --format json -c 'count(E)'   # {"value":0}  correct
//! powdb-cli --format json -c 'count(V)'   # {"value":1}  WRONG, forever
//! ```
//!
//! Each of those is a separate process. The delete dirtied V in the deleting
//! process and that process then exited; the next one loaded a CLEAN V and
//! served the deleted row. No error, no warning, and no mutation could ever
//! fix it because every mutation lost the marker the same way. Only an
//! explicit `refresh V` recovered, and nothing told the user it was owed.
//!
//! Every test below drops the engine WITHOUT reading the view in between, so
//! the marker has to have reached disk to be there on reopen. Against the
//! unfixed implementation `view_serving_a_deleted_row_after_restart` sees
//! `count(V) == 1` while `count(E) == 0`, and the insert-side test sees
//! `count(V) == 1` while `count(E) == 2`.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use powdb_storage::view::ViewRegistry;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_matview_restart_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("failed to execute `{query}`: {e}"))
}

fn count(engine: &mut Engine, query: &str) -> i64 {
    match exec(engine, query) {
        QueryResult::Scalar(Value::Int(n)) => n,
        QueryResult::Rows { rows, .. } if rows.len() == 1 && rows[0].len() == 1 => {
            match &rows[0][0] {
                Value::Int(n) => *n,
                other => panic!("count returned non-int {other:?}"),
            }
        }
        other => panic!("expected scalar count, got {other:?}"),
    }
}

fn ints(engine: &mut Engine, query: &str) -> Vec<i64> {
    match exec(engine, query) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r[0] {
                Value::Int(n) => *n,
                other => panic!("expected int, got {other:?}"),
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Set up `E` with `rows`, materialize `V` over it, and read `V` once so it is
/// registered CLEAN — the state the bug needs in order to stay clean.
fn seed(dir: &std::path::Path, rows: &[i64]) {
    let mut engine = Engine::new(dir).unwrap();
    exec(&mut engine, "type E { required unique id: int }");
    for id in rows {
        exec(&mut engine, &format!("insert E {{ id := {id} }}"));
    }
    exec(&mut engine, "materialize V as E { .id }");
    assert_eq!(
        count(&mut engine, "count(V)"),
        rows.len() as i64,
        "the fresh view must reflect the seeded rows"
    );
}

/// Whether the on-disk registry says the view needs a refresh. Read straight
/// from `views.bin` with no engine, so it observes what a new process would.
fn dirty_on_disk(dir: &std::path::Path, view: &str) -> bool {
    ViewRegistry::open(dir)
        .expect("the view registry is readable")
        .is_dirty(view)
}

/// The reported repro, in one process boundary: delete the only row, exit
/// without touching the view, reopen, and the view must not still be serving
/// the deleted row.
#[test]
fn view_serving_a_deleted_row_after_restart() {
    let dir = temp_dir("delete");
    seed(&dir, &[1]);

    {
        let mut engine = Engine::new(&dir).unwrap();
        exec(&mut engine, "E filter .id = 1 delete");
        assert_eq!(count(&mut engine, "count(E)"), 0);
        // Deliberately no read of V here: the whole bug is that the marker
        // set by the line above does not outlive this scope.
    }

    // Assert the user-visible answer before the mechanism, so a regression
    // reports the wrong answer itself rather than a missing marker.
    let mut engine = Engine::new(&dir).unwrap();
    assert_eq!(count(&mut engine, "count(E)"), 0);
    assert_eq!(
        count(&mut engine, "count(V)"),
        0,
        "V served the deleted row after a restart"
    );
    assert_eq!(
        ints(&mut engine, "V { .id }"),
        Vec::<i64>::new(),
        "V returned the deleted row after a restart"
    );
    drop(engine);
    assert!(
        !dirty_on_disk(&dir, "V"),
        "the read above refreshed V, so no refresh is still owed"
    );
}

/// The other direction, which the audit also found: a row inserted after the
/// view was built is missing from it forever.
#[test]
fn view_omitting_a_live_row_after_restart() {
    let dir = temp_dir("insert");
    seed(&dir, &[1]);

    {
        let mut engine = Engine::new(&dir).unwrap();
        exec(&mut engine, "insert E { id := 2 }");
        assert_eq!(count(&mut engine, "count(E)"), 2);
    }

    let mut engine = Engine::new(&dir).unwrap();
    assert_eq!(
        count(&mut engine, "count(V)"),
        2,
        "V omitted a row inserted before the restart"
    );
    let mut got = ints(&mut engine, "V { .id }");
    got.sort_unstable();
    assert_eq!(got, vec![1, 2]);
}

/// An UPDATE dirties the view the same way a delete or insert does, and the
/// marker has to survive the same boundary.
#[test]
fn view_serving_a_pre_update_value_after_restart() {
    let dir = temp_dir("update");
    seed(&dir, &[1]);

    {
        let mut engine = Engine::new(&dir).unwrap();
        exec(&mut engine, "E filter .id = 1 update { id := 9 }");
    }

    let mut engine = Engine::new(&dir).unwrap();
    assert_eq!(
        ints(&mut engine, "V { .id }"),
        vec![9],
        "V served the pre-update value after a restart"
    );
}

/// `refresh V` was the only recovery the user had. It still has to work, and
/// it has to leave the view clean on disk rather than clean only in memory —
/// otherwise the fix would trade a permanently stale view for a view that
/// refreshes on every restart forever.
#[test]
fn explicit_refresh_after_restart_clears_the_marker_durably() {
    let dir = temp_dir("refresh");
    seed(&dir, &[1, 2]);

    {
        let mut engine = Engine::new(&dir).unwrap();
        exec(&mut engine, "E filter .id = 1 delete");
    }

    {
        let mut engine = Engine::new(&dir).unwrap();
        assert!(dirty_on_disk(&dir, "V"));
        exec(&mut engine, "refresh V");
        assert_eq!(count(&mut engine, "count(V)"), 1);
    }

    assert!(
        !dirty_on_disk(&dir, "V"),
        "a completed refresh must record that the view is current"
    );

    let mut engine = Engine::new(&dir).unwrap();
    assert_eq!(ints(&mut engine, "V { .id }"), vec![2]);
}

/// A read is a refresh too (`refresh_dirty_views_read_by` runs before the
/// plan), so reading the view after a restart must also clear the marker
/// durably. Without this, the previous test would pass on an implementation
/// that simply never cleans the flag.
#[test]
fn reading_a_stale_view_clears_the_marker_durably() {
    let dir = temp_dir("read_clears");
    seed(&dir, &[1, 2]);

    {
        let mut engine = Engine::new(&dir).unwrap();
        exec(&mut engine, "E filter .id = 1 delete");
    }
    assert!(dirty_on_disk(&dir, "V"));

    {
        let mut engine = Engine::new(&dir).unwrap();
        assert_eq!(count(&mut engine, "count(V)"), 1);
    }

    assert!(
        !dirty_on_disk(&dir, "V"),
        "a read that refreshed the view must record it as current"
    );
}

/// A database nobody mutated must not come back dirty. This is what keeps the
/// fix from being "mark everything dirty always", which would answer every
/// assertion above while making each restart re-run every view's source query.
#[test]
fn an_untouched_view_stays_clean_across_restarts() {
    let dir = temp_dir("untouched");
    seed(&dir, &[1, 2, 3]);

    for _ in 0..3 {
        assert!(
            !dirty_on_disk(&dir, "V"),
            "no mutation happened, so no refresh is owed"
        );
        let mut engine = Engine::new(&dir).unwrap();
        assert_eq!(count(&mut engine, "count(V)"), 3);
    }
}

/// The marker must be on disk before the mutation that caused it is durable,
/// not after: a crash in between otherwise recovers the write and loses the
/// marker, which is the original bug through a narrower window.
/// `std::mem::forget` is the hard-crash simulator used by `durability.rs` —
/// no Drop, so no checkpoint and no final flush.
#[test]
fn marker_survives_a_hard_crash_that_the_write_also_survives() {
    let dir = temp_dir("hard_crash");
    seed(&dir, &[1, 2]);

    {
        let mut engine = Engine::new(&dir).unwrap();
        exec(&mut engine, "insert E { id := 3 }");
        std::mem::forget(engine); // hard process crash: no Drop, no flush
    }

    let mut engine = Engine::new(&dir).unwrap();
    let base = count(&mut engine, "count(E)");
    assert_eq!(base, 3, "the write itself must survive via WAL replay");
    assert_eq!(
        count(&mut engine, "count(V)"),
        base,
        "the view must not disagree with a base table the same crash preserved"
    );
}

/// Repeated mutation between refreshes is the write-path cost question: only
/// the clean → dirty transition may write `views.bin`. Deleting the file after
/// the first mutation makes any further write unmissable.
#[test]
fn only_the_first_mutation_after_a_refresh_writes_the_registry() {
    let dir = temp_dir("transition_only");
    seed(&dir, &[1]);

    let mut engine = Engine::new(&dir).unwrap();
    // Remove the file `materialize` wrote, so the assertion below observes the
    // mutation's own write and nothing else.
    let registry_file = dir.join("views.bin");
    std::fs::remove_file(&registry_file).unwrap();

    exec(&mut engine, "insert E { id := 100 }");
    assert!(
        registry_file.exists(),
        "the first mutation after a refresh records the marker"
    );
    std::fs::remove_file(&registry_file).unwrap();

    for id in 101..200 {
        exec(&mut engine, &format!("insert E {{ id := {id} }}"));
    }
    assert!(
        !registry_file.exists(),
        "mutations after the view is already dirty must not rewrite views.bin"
    );

    // And the view is still correct once it is read.
    assert_eq!(count(&mut engine, "count(V)"), 101);
}
