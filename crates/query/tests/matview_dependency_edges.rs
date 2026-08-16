//! Two ways a materialized view could keep answering after it stopped being
//! true, both of which the dirty flag alone did not cover.
//!
//! 1. **A view over a view.** `depends_on` records source *names*, and a view
//!    is a legal source, so `V2 as V1` depends on `V1`. Dirty propagation only
//!    walked one level, so mutating the base table marked `V1` dirty and left
//!    `V2` clean over `V1`'s pre-mutation rows. Nothing revisits a clean view,
//!    so `V2` was wrong permanently, in both directions, with no error.
//!
//! 2. **Dropping the source table.** `drop_table` never touched the view
//!    registry, so the view stayed clean over rows whose source no longer
//!    existed. Reading it served that orphaned copy while `refresh` on the very
//!    same view already failed with "table not found": the read and the refresh
//!    disagreed about whether the view was still valid, and the read was the
//!    one that lied.
//!
//! Both are the same class as the restart bug fixed in v0.24.0 (a view serving
//! rows it should not), so they are held to the same standard: refuse, or be
//! correct. Never silently serve.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use std::path::PathBuf;

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "powdb_matview_deps_{name}_{}_{:?}",
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

/// The `id` column of every row a query returned, ascending.
fn ids(result: &QueryResult) -> Vec<i64> {
    match result {
        QueryResult::Rows { rows, .. } => {
            let mut out: Vec<i64> = rows
                .iter()
                .filter_map(|row| match row.first() {
                    Some(Value::Int(i)) => Some(*i),
                    _ => None,
                })
                .collect();
            out.sort_unstable();
            out
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

fn seed_chain(engine: &mut Engine) {
    exec(engine, "type E { required unique id: int }");
    for id in [1i64, 2, 3] {
        exec(engine, &format!("insert E {{ id := {id} }}"));
    }
    exec(engine, "materialize V1 as E { .id }");
    exec(engine, "materialize V2 as V1 { .id }");
    // Read both once so each is clean over the seeded rows.
    assert_eq!(ids(&exec(engine, "V1")), vec![1, 2, 3]);
    assert_eq!(ids(&exec(engine, "V2")), vec![1, 2, 3]);
}

#[test]
fn a_view_over_a_view_sees_a_delete_in_the_base_table() {
    let dir = fresh_dir("chain_delete");
    let mut engine = Engine::new(&dir).unwrap();
    seed_chain(&mut engine);

    exec(&mut engine, "E filter .id = 1 delete");

    assert_eq!(ids(&exec(&mut engine, "E")), vec![2, 3]);
    assert_eq!(ids(&exec(&mut engine, "V1")), vec![2, 3]);
    assert_eq!(
        ids(&exec(&mut engine, "V2")),
        vec![2, 3],
        "V2 is built over V1, so a delete in the base table must reach it too"
    );

    drop(engine);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_view_over_a_view_sees_an_insert_in_the_base_table() {
    let dir = fresh_dir("chain_insert");
    let mut engine = Engine::new(&dir).unwrap();
    seed_chain(&mut engine);

    exec(&mut engine, "insert E { id := 4 }");

    assert_eq!(
        ids(&exec(&mut engine, "V2")),
        vec![1, 2, 3, 4],
        "the missing-row direction fails just as silently as the extra-row one"
    );

    drop(engine);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_view_over_a_view_survives_a_restart() {
    let dir = fresh_dir("chain_restart");
    {
        let mut engine = Engine::new(&dir).unwrap();
        seed_chain(&mut engine);
        exec(&mut engine, "E filter .id = 1 delete");
        // Exit without reading V2, so only the persisted flag can carry the
        // knowledge that a refresh is owed. This is the v0.24.0 failure mode
        // and the transitive walk has to persist through it as well.
    }

    let mut engine = Engine::new(&dir).unwrap();
    assert_eq!(
        ids(&exec(&mut engine, "V2")),
        vec![2, 3],
        "the transitive dirty mark must be on disk, not just in memory"
    );

    drop(engine);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dropping_the_source_table_makes_the_view_refuse_rather_than_serve() {
    let dir = fresh_dir("drop_source");
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type E { required unique id: int }");
    exec(&mut engine, "insert E { id := 1 }");
    exec(&mut engine, "materialize V as E { .id }");
    assert_eq!(ids(&exec(&mut engine, "V")), vec![1]);

    exec(&mut engine, "drop E");

    let err = engine
        .execute_powql("V")
        .expect_err("reading a view whose source is gone must fail, not serve the orphaned rows");
    let text = err.to_string();
    assert!(
        text.contains('E'),
        "the error should name the missing source table, got: {text}"
    );

    // The refresh path already failed before this fix; the point is that the
    // read now agrees with it instead of contradicting it.
    assert!(
        engine.execute_powql("refresh V").is_err(),
        "refresh must still fail for the same reason"
    );

    drop(engine);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dropping_the_source_table_reports_the_views_it_broke() {
    let dir = fresh_dir("drop_reports");
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type E { required unique id: int }");
    exec(&mut engine, "insert E { id := 1 }");
    exec(&mut engine, "materialize V1 as E { .id }");
    exec(&mut engine, "materialize V2 as V1 { .id }");
    exec(&mut engine, "V1");
    exec(&mut engine, "V2");

    match exec(&mut engine, "drop E") {
        QueryResult::Executed { message } => {
            // Both the direct dependent and the one behind it, so an operator
            // is not left to discover the second one by hitting the error.
            assert!(message.contains("'V1'"), "message must name V1: {message}");
            assert!(
                message.contains("'V2'"),
                "message must name the transitive dependent V2 too: {message}"
            );
        }
        other => panic!("expected an Executed message, got {other:?}"),
    }

    drop(engine);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dropping_an_unrelated_table_does_not_mention_views() {
    let dir = fresh_dir("drop_unrelated");
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type E { required unique id: int }");
    exec(&mut engine, "type Other { required unique id: int }");
    exec(&mut engine, "insert E { id := 1 }");
    exec(&mut engine, "materialize V as E { .id }");
    exec(&mut engine, "V");

    match exec(&mut engine, "drop Other") {
        QueryResult::Executed { message } => assert_eq!(
            message, "table 'Other' dropped",
            "a table with no dependent views must keep the plain message"
        ),
        other => panic!("expected an Executed message, got {other:?}"),
    }
    // And the untouched view must still answer.
    assert_eq!(ids(&exec(&mut engine, "V")), vec![1]);

    drop(engine);
    let _ = std::fs::remove_dir_all(&dir);
}
