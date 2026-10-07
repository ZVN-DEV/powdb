//! Autocommit statements must not be treated like one long rollback-pinned
//! transaction by the dirty-page budget.
//!
//! A tiny dirty-page budget is valid in production: explicit transactions that
//! exceed it are refused because rollback needs their dirty pages pinned, while
//! small autocommit statements can spill earlier committed pages and keep going.
//! Each individual statement still needs to fit its rollback budget. These regressions
//! keep that split true through the query engine's statement boundary, not just
//! through the lower-level catalog API.

use powdb_query::ast::Literal;
use powdb_query::executor::Engine;
use powdb_query::result::{QueryError, QueryResult};
use powdb_storage::error::StorageErrorKind;
use powdb_storage::page::PAGE_SIZE;
use powdb_storage::types::Value;
use powdb_storage::wal::{WalDurabilityTicket, WalSyncMode};
use std::path::Path;

const MODES: [WalSyncMode; 3] = [WalSyncMode::Full, WalSyncMode::Normal, WalSyncMode::Off];
const DIRTY_BUDGET_BYTES: usize = 8 * PAGE_SIZE;
const ROWS: i64 = 100;
const PAYLOAD_LEN: usize = 2_000;

fn open(mode: WalSyncMode) -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine.set_wal_sync_mode(mode);
    engine
        .catalog_mut()
        .set_dirty_page_budget_bytes(DIRTY_BUDGET_BYTES);
    (dir, engine)
}

fn wal_len(dir: &Path) -> u64 {
    std::fs::metadata(dir.join("wal.log")).map_or(0, |metadata| metadata.len())
}

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|error| panic!("failed to execute `{query}`: {error}"))
}

fn prepared_insert(engine: &mut Engine) -> powdb_query::executor::PreparedQuery {
    engine
        .prepare(r#"insert Bulk { id := 1, payload := "x" }"#)
        .unwrap()
}

fn execute_prepared_insert(
    engine: &mut Engine,
    insert: &powdb_query::executor::PreparedQuery,
    id: i64,
) -> Result<QueryResult, QueryError> {
    engine.execute_prepared(insert, &[Literal::Int(id), Literal::String(payload(id))])
}

fn count(engine: &mut Engine) -> i64 {
    match exec(engine, "count(Bulk)") {
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

fn rows(engine: &mut Engine) -> Vec<Vec<Value>> {
    match exec(engine, "Bulk order .id { .id, .payload }") {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn payload(id: i64) -> String {
    let mut value = format!("row-{id:04}-");
    value.push_str(&"x".repeat(PAYLOAD_LEN - value.len()));
    value
}

fn expected_rows(count: i64) -> Vec<Vec<Value>> {
    (0..count)
        .map(|id| vec![Value::Int(id), Value::Str(payload(id))])
        .collect()
}

fn create_bulk(engine: &mut Engine) {
    exec(
        engine,
        "type Bulk { required unique id: int, required payload: str }",
    );
}

fn assert_transaction_too_large(error: QueryError) {
    assert!(
        matches!(
            error,
            QueryError::Storage {
                kind: StorageErrorKind::TransactionTooLarge,
                ..
            }
        ),
        "expected typed TransactionTooLarge, got: {error:?}"
    );
}

#[test]
fn prepared_autocommit_inserts_spill_instead_of_exhausting_the_dirty_page_budget() {
    for mode in MODES {
        let (dir, mut engine) = open(mode);
        create_bulk(&mut engine);
        let insert = prepared_insert(&mut engine);

        // Do not scan or count inside this loop: read paths flush dirty pages
        // and can mask the regression. Each statement must commit and release
        // its rollback pin on its own.
        for id in 0..ROWS {
            let result = execute_prepared_insert(&mut engine, &insert, id);
            assert!(
                result.is_ok(),
                "prepared autocommit insert {id} failed in {mode:?}: {result:?}"
            );
        }

        assert_eq!(count(&mut engine), ROWS, "{mode:?}");
        assert_eq!(rows(&mut engine), expected_rows(ROWS), "{mode:?}");
        drop(engine);

        let mut reopened = Engine::new(dir.path()).unwrap();
        assert_eq!(count(&mut reopened), ROWS, "reopen count: {mode:?}");
        assert_eq!(
            rows(&mut reopened),
            expected_rows(ROWS),
            "reopen rows: {mode:?}"
        );
    }
}

#[test]
fn powql_text_autocommit_inserts_spill_instead_of_exhausting_the_dirty_page_budget() {
    let (_dir, mut engine) = open(WalSyncMode::Full);
    create_bulk(&mut engine);

    for id in 0..ROWS {
        let query = format!(
            r#"insert Bulk {{ id := {id}, payload := "{}" }}"#,
            payload(id)
        );
        let result = engine.execute_powql(&query);
        assert!(
            result.is_ok(),
            "text autocommit insert {id} failed: {result:?}"
        );
    }

    assert_eq!(count(&mut engine), ROWS);
    assert_eq!(rows(&mut engine), expected_rows(ROWS));
}

#[test]
fn failed_autocommit_statement_after_budget_pressure_preserves_prior_rows() {
    for mode in MODES {
        let (dir, mut engine) = open(mode);
        create_bulk(&mut engine);
        let insert = prepared_insert(&mut engine);

        for id in 0..ROWS {
            execute_prepared_insert(&mut engine, &insert, id).unwrap_or_else(|error| {
                panic!("prepared autocommit insert {id} failed in {mode:?}: {error}")
            });
        }
        let before = rows(&mut engine);

        let duplicate = engine
            .execute_prepared(
                &insert,
                &[Literal::Int(ROWS - 1), Literal::String(payload(ROWS))],
            )
            .expect_err("duplicate unique id must fail");
        assert!(
            duplicate.to_string().contains("unique constraint"),
            "unexpected duplicate error in {mode:?}: {duplicate}"
        );
        assert_eq!(rows(&mut engine), before, "{mode:?}");

        engine
            .execute_prepared(
                &insert,
                &[Literal::Int(ROWS), Literal::String(payload(ROWS))],
            )
            .unwrap_or_else(|error| {
                panic!("insert after failed statement failed in {mode:?}: {error}")
            });
        assert_eq!(count(&mut engine), ROWS + 1, "{mode:?}");
        drop(engine);

        let mut reopened = Engine::new(dir.path()).unwrap();
        assert_eq!(count(&mut reopened), ROWS + 1, "reopen: {mode:?}");
    }
}

#[test]
fn explicit_oversized_transaction_is_refused_and_rolls_back_cleanly() {
    for mode in MODES {
        let (_dir, mut engine) = open(mode);
        create_bulk(&mut engine);
        let insert = prepared_insert(&mut engine);

        exec(&mut engine, "begin");
        let mut refusal = None;
        for id in 0..ROWS {
            let result = execute_prepared_insert(&mut engine, &insert, id);
            if let Err(error) = result {
                refusal = Some(error);
                break;
            }
        }
        assert_transaction_too_large(refusal.unwrap_or_else(|| {
            panic!("explicit transaction should exceed the 8-page budget in {mode:?}")
        }));
        assert_eq!(
            engine.execute_powql("Bulk").unwrap_err(),
            QueryError::TransactionAborted,
            "{mode:?}"
        );

        exec(&mut engine, "rollback");
        assert_eq!(count(&mut engine), 0, "{mode:?}");
        engine
            .execute_prepared(&insert, &[Literal::Int(0), Literal::String(payload(0))])
            .unwrap_or_else(|error| panic!("insert after rollback failed in {mode:?}: {error}"));
        assert_eq!(rows(&mut engine), expected_rows(1), "{mode:?}");
    }
}

#[test]
fn pressure_relief_preserves_existing_wal_history_when_checkpointing_is_disabled() {
    let (dir, mut engine) = open(WalSyncMode::Full);
    engine.catalog_mut().set_wal_checkpoint_bytes(0);
    create_bulk(&mut engine);
    let insert = prepared_insert(&mut engine);

    let mut previous_len = wal_len(dir.path());
    assert!(previous_len > 0, "table creation should leave WAL history");
    for id in 0..ROWS {
        execute_prepared_insert(&mut engine, &insert, id)
            .unwrap_or_else(|error| panic!("insert {id} failed under pressure: {error}"));
        let current_len = wal_len(dir.path());
        assert!(
            current_len >= previous_len,
            "pressure relief must not truncate WAL history: {current_len} < {previous_len} at row {id}"
        );
        previous_len = current_len;
    }

    assert_eq!(count(&mut engine), ROWS);
    let final_len = wal_len(dir.path());
    assert!(
        final_len >= previous_len,
        "post-pressure read must not truncate WAL history: {final_len} < {previous_len}"
    );
    std::mem::forget(engine);

    let mut reopened = Engine::new(dir.path()).unwrap();
    assert_eq!(
        count(&mut reopened),
        ROWS,
        "all uncheckpointed WAL history must replay after a hard crash"
    );
    assert_eq!(rows(&mut reopened), expected_rows(ROWS));
}

#[test]
fn pressure_relief_settles_taken_deferred_full_durability_claims_before_heap_flush() {
    let (dir, mut engine) = open(WalSyncMode::Full);
    create_bulk(&mut engine);
    let insert = prepared_insert(&mut engine);

    let base_fsyncs = engine.wal_fsync_count();
    let mut tickets: Vec<WalDurabilityTicket> = Vec::new();
    let mut rows_written = 0i64;
    for id in 0..ROWS {
        let (result, ticket) = engine
            .run_with_deferred_durability(|engine| execute_prepared_insert(engine, &insert, id));
        result.unwrap_or_else(|error| panic!("deferred insert {id} failed: {error}"));
        tickets.push(ticket.expect("Full-mode deferred insert must produce a durability ticket"));
        rows_written += 1;
        if engine.wal_fsync_count() > base_fsyncs {
            break;
        }
    }

    assert!(
        engine.wal_fsync_count() > base_fsyncs,
        "dirty-budget pressure must settle already-taken deferred durability claims before flushing heap pages"
    );
    assert!(
        rows_written < ROWS,
        "test did not observe pressure relief before the row cap"
    );

    let fsyncs_after_pressure = engine.wal_fsync_count();
    for ticket in tickets {
        ticket.wait().unwrap();
    }
    assert!(
        engine.wal_fsync_count() >= fsyncs_after_pressure,
        "waiting on tickets must not undo the pressure-relief fsync"
    );
    assert_eq!(count(&mut engine), rows_written);
    std::mem::forget(engine);

    let mut reopened = Engine::new(dir.path()).unwrap();
    assert_eq!(
        count(&mut reopened),
        rows_written,
        "every ticket settled before acknowledgement must survive a hard crash"
    );
    assert_eq!(rows(&mut reopened), expected_rows(rows_written));
}
