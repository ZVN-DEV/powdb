//! A statement inside an explicit transaction does not fsync.
//!
//! Full mode's promise is that nothing is acknowledged before an fsync
//! covering it has returned, and inside `begin`/`commit` the acknowledgement
//! the client waits on is the `commit`. Flushing at every statement boundary
//! in between buys no durability (an unfinished transaction is rolled back on
//! replay) and costs one fsync per row. The text path has skipped it since
//! group commit landed; the prepared paths had not, so a 5000-row load through
//! a prepared insert paid 5000 fsyncs where the same load written as PowQL
//! text paid 78.
//!
//! 78 rather than 1 because the WAL also flushes whenever its append buffer
//! fills, and in Full mode that flush fsyncs as well. That one is inside
//! `Wal::append` (crates/storage/src/wal.rs) and is a separate lane's change;
//! these tests therefore pin the property this lane owns: a statement boundary
//! inside a transaction costs nothing, so a prepared write and the identical
//! write in text pay exactly the same.

use powdb_query::ast::Literal;
use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_tx_fsync_{name}_{}_{}",
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
        other => panic!("expected scalar count, got {other:?}"),
    }
}

fn open(name: &str) -> (std::path::PathBuf, Engine) {
    let dir = temp_dir(name);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(&dir).unwrap();
    (dir, engine)
}

#[test]
fn five_thousand_prepared_inserts_cost_the_same_fsyncs_as_five_thousand_text_inserts() {
    const ROWS: i64 = 5000;

    let (text_dir, mut engine) = open("text_insert");
    exec(&mut engine, "type T { required id: int, v: int }");
    exec(&mut engine, "begin");
    let base = engine.wal_fsync_count();
    for i in 0..ROWS {
        exec(&mut engine, &format!("insert T {{ id := {i}, v := {i} }}"));
    }
    let text_inside = engine.wal_fsync_count() - base;
    exec(&mut engine, "commit");
    assert_eq!(count(&mut engine, "count(T)"), ROWS);
    std::fs::remove_dir_all(&text_dir).ok();

    let (dir, mut engine) = open("prepared_insert");
    exec(&mut engine, "type T { required id: int, v: int }");
    let prep = engine
        .prepare("insert T { id := 1, v := 2 }")
        .expect("prepare");
    exec(&mut engine, "begin");
    let base = engine.wal_fsync_count();
    for i in 0..ROWS {
        engine
            .execute_prepared(&prep, &[Literal::Int(i), Literal::Int(i * 2)])
            .expect("prepared insert");
    }
    let prepared_inside = engine.wal_fsync_count() - base;
    exec(&mut engine, "commit");

    assert_eq!(
        prepared_inside, text_inside,
        "the same {ROWS} rows must cost the same fsyncs whether written \
         prepared or as text"
    );
    assert!(
        prepared_inside * 8 < ROWS as u64,
        "fsyncs inside a transaction must not scale with the statement count, \
         saw {prepared_inside} for {ROWS} statements"
    );
    assert_eq!(count(&mut engine, "count(T)"), ROWS);
    std::fs::remove_dir_all(&dir).ok();
}

/// Short enough that the WAL's own append buffer never fills, so the only
/// fsync left to observe is the one the `commit` owes.
#[test]
fn a_short_transaction_of_prepared_inserts_costs_exactly_one_fsync() {
    let (dir, mut engine) = open("prepared_short");
    exec(&mut engine, "type S { required id: int, v: int }");
    let prep = engine
        .prepare("insert S { id := 1, v := 2 }")
        .expect("prepare");

    exec(&mut engine, "begin");
    let base = engine.wal_fsync_count();
    for i in 0..50i64 {
        engine
            .execute_prepared(&prep, &[Literal::Int(i), Literal::Int(i)])
            .expect("prepared insert");
    }
    let inside = engine.wal_fsync_count() - base;
    exec(&mut engine, "commit");
    let total = engine.wal_fsync_count() - base;

    assert_eq!(
        inside, 0,
        "no statement boundary inside a transaction may fsync, saw {inside}"
    );
    assert_eq!(total, 1, "one transaction is one fsync, saw {total}");
    assert_eq!(count(&mut engine, "count(S)"), 50);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_prepared_take_insert_in_a_transaction_does_not_fsync() {
    let (dir, mut engine) = open("prepared_take");
    exec(&mut engine, "type T { required id: int, name: str }");
    let prep = engine
        .prepare("insert T { id := 1, name := \"x\" }")
        .expect("prepare");

    exec(&mut engine, "begin");
    let base = engine.wal_fsync_count();
    for i in 0..50i64 {
        let mut literals = [Literal::Int(i), Literal::String(format!("row {i}"))];
        engine
            .execute_prepared_take(&prep, &mut literals)
            .expect("prepared insert");
    }
    let inside = engine.wal_fsync_count() - base;
    exec(&mut engine, "commit");

    assert_eq!(
        inside, 0,
        "no statement inside a transaction may fsync, saw {inside}"
    );
    assert_eq!(count(&mut engine, "count(T)"), 50);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_prepared_point_update_in_a_transaction_does_not_fsync() {
    let (dir, mut engine) = open("prepared_update_pk");
    exec(
        &mut engine,
        "type U { required id: int, required name: str, age: int }",
    );
    engine
        .catalog_mut()
        .create_index_unique("U", "id", true)
        .unwrap();
    for i in 0..50i64 {
        exec(
            &mut engine,
            &format!("insert U {{ id := {i}, name := \"u{i}\", age := {i} }}"),
        );
    }
    let prep = engine
        .prepare("U filter .id = 1 update { age := 2 }")
        .expect("prepare");

    exec(&mut engine, "begin");
    let base = engine.wal_fsync_count();
    for i in 0..50i64 {
        engine
            .execute_prepared(&prep, &[Literal::Int(i), Literal::Int(900 + i)])
            .expect("prepared update");
    }
    let inside = engine.wal_fsync_count() - base;
    exec(&mut engine, "commit");

    assert_eq!(
        inside, 0,
        "no statement inside a transaction may fsync, saw {inside}"
    );
    assert_eq!(count(&mut engine, "count(U filter .age >= 900)"), 50);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_prepared_delete_in_a_transaction_does_not_fsync() {
    let (dir, mut engine) = open("prepared_generic");
    exec(&mut engine, "type D { required id: int, v: int }");
    for i in 0..50i64 {
        exec(&mut engine, &format!("insert D {{ id := {i}, v := {i} }}"));
    }
    let prep = engine.prepare("D filter .id = 1 delete").expect("prepare");

    exec(&mut engine, "begin");
    let base = engine.wal_fsync_count();
    for i in 0..50i64 {
        engine
            .execute_prepared(&prep, &[Literal::Int(i)])
            .expect("prepared delete");
    }
    let inside = engine.wal_fsync_count() - base;
    exec(&mut engine, "commit");

    assert_eq!(
        inside, 0,
        "no statement inside a transaction may fsync, saw {inside}"
    );
    assert_eq!(count(&mut engine, "count(D)"), 0);
    std::fs::remove_dir_all(&dir).ok();
}

/// A prepared write outside a transaction keeps paying its own fsync: the
/// guard must be about the transaction, not about the prepared path.
#[test]
fn a_prepared_insert_outside_a_transaction_still_fsyncs() {
    let (dir, mut engine) = open("prepared_autocommit");
    exec(&mut engine, "type A { required id: int, v: int }");
    let prep = engine
        .prepare("insert A { id := 1, v := 2 }")
        .expect("prepare");

    let base = engine.wal_fsync_count();
    for i in 0..10i64 {
        engine
            .execute_prepared(&prep, &[Literal::Int(i), Literal::Int(i)])
            .expect("prepared insert");
    }
    assert_eq!(
        engine.wal_fsync_count() - base,
        10,
        "an autocommit statement must still be durable before it returns"
    );
    std::fs::remove_dir_all(&dir).ok();
}
