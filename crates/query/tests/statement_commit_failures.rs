#![cfg(feature = "testing")]

//! Fault coverage for statement commit durability boundaries.
//!
//! These tests use the `powdb-query/testing` feature, which propagates to the
//! storage crate's test-only WAL fsync failpoint. Production builds do not
//! expose the injection API.

use powdb_query::executor::Engine;
use powdb_query::result::{QueryError, QueryResult};
use powdb_storage::types::Value;

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|err| panic!("`{query}` failed: {err}"))
}

fn seed(dir: &std::path::Path) -> Engine {
    let mut engine = Engine::new(dir).unwrap();
    exec(&mut engine, "type T { required id: int, v: str }");
    exec(&mut engine, r#"insert T { id := 1, v := "old" }"#);
    engine
}

fn ids(engine: &mut Engine) -> Vec<i64> {
    match exec(engine, "T order .id { .id }") {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|row| match &row[0] {
                Value::Int(id) => *id,
                other => panic!("id projected as {other:?}"),
            })
            .collect(),
        other => panic!("expected row projection, got {other:?}"),
    }
}

#[test]
fn autocommit_fsync_failure_returns_unknown_commit_and_poisons_handle() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = seed(dir.path());

    engine.inject_next_fsync_failure_for_testing();
    let err = engine
        .execute_powql(
            r#"insert T
              { id := 2, v := "new-a" },
              { id := 3, v := "new-b" }"#,
        )
        .expect_err("the injected commit fsync failure must surface");
    assert!(matches!(err, QueryError::CommitOutcomeUnknown), "{err:?}");

    let read_after = engine
        .execute_powql("count(T)")
        .expect_err("poisoned handle must reject reads");
    assert!(
        matches!(read_after, QueryError::EnginePoisoned),
        "{read_after:?}"
    );
    let write_after = engine
        .execute_powql(r#"insert T { id := 4, v := "after" }"#)
        .expect_err("poisoned handle must reject writes");
    assert!(
        matches!(write_after, QueryError::EnginePoisoned),
        "{write_after:?}"
    );

    drop(engine);
    let mut reopened = Engine::new(dir.path()).unwrap();
    let recovered = ids(&mut reopened);
    assert!(
        recovered == vec![1] || recovered == vec![1, 2, 3],
        "reopen must reconcile to the whole old or whole new statement state, never partial: {recovered:?}"
    );
}

#[test]
fn deferred_fsync_failure_surfaces_on_ticket_and_poisons_future_reads() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = seed(dir.path());

    engine.inject_next_fsync_failure_for_testing();
    let (result, ticket) = engine.run_with_deferred_durability(|engine| {
        engine.execute_powql(r#"insert T { id := 2, v := "deferred" }"#)
    });
    result.expect("deferred commit registers durability but does not fsync inline");
    let ticket = ticket.expect("Full-mode deferred commit must return a durability ticket");
    let err = ticket
        .wait()
        .expect_err("the injected fsync failure must surface from the ticket");
    assert!(
        err.to_string().contains("injected WAL fsync failure"),
        "unexpected ticket error: {err}"
    );

    let read_after = engine
        .execute_powql("count(T)")
        .expect_err("WAL poison must make future readonly statements fail");
    assert!(
        matches!(read_after, QueryError::EnginePoisoned),
        "{read_after:?}"
    );
}
