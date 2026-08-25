use super::*;

// ─── Cooperative query cancellation ──────────────────────────────
//
// These tests prove the deadline / cancel token actually stops a runaway
// executor loop promptly and leaves the engine usable and consistent. WAL
// sync is turned Off so the fixtures load quickly; durability is not under
// test here.

use crate::cancel::{CancelReason, ExecCancel};
use crate::result::QueryError;
use std::sync::Arc as CancelArc;
use std::time::{Duration as CancelDuration, Instant as CancelInstant};

/// Two tables used to exercise cancellation inside join execution. Tests that
/// require the nested-loop path make the equality expression-valued so it is
/// deliberately ineligible as a hash key.
fn compound_join_engine(left: usize, right: usize) -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "powdb_cancel_join_{}_{}_{}",
        std::process::id(),
        id,
        nonce
    ));
    let mut engine = Engine::new(&dir).unwrap();
    engine.set_wal_sync_mode(super::WalSyncMode::Off);
    engine
        .execute_powql("type Ver { required id: int }")
        .unwrap();
    engine
        .execute_powql("type Grp { required version_id: int, required field_ns: str }")
        .unwrap();
    for i in 0..left {
        engine
            .execute_powql(&format!("insert Ver {{ id := {i} }}"))
            .unwrap();
    }
    for i in 0..right {
        engine
            .execute_powql(&format!(
                r#"insert Grp {{ version_id := {i}, field_ns := "f1" }}"#
            ))
            .unwrap();
    }
    engine
}

/// A single table of `n` rows for scan/mutation cancellation tests.
fn item_engine(n: usize) -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_cancel_item_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine.set_wal_sync_mode(super::WalSyncMode::Off);
    engine
        .execute_powql("type Item { required id: int, required v: int }")
        .unwrap();
    for i in 0..n {
        engine
            .execute_powql(&format!("insert Item {{ id := {i}, v := 0 }}"))
            .unwrap();
    }
    engine
}

/// Signal cancellation only after the executor has crossed `target` real
/// checkpoints. This proves the statement was already running inside the loop
/// under test rather than merely rejecting a pre-cancelled token at entry.
pub(super) fn cancel_after_checkpoint(
    token: CancelArc<ExecCancel>,
    target: usize,
) -> std::thread::JoinHandle<()> {
    // Arm the rendezvous before the query starts: the executor parks at the
    // target checkpoint until the observer below delivers the cancel, so the
    // outcome cannot depend on thread scheduling between checkpoint and cancel.
    token.block_at_checkpoint(target);
    std::thread::spawn(move || {
        let deadline = CancelInstant::now() + CancelDuration::from_secs(3);
        while token.checkpoint_count() < target && CancelInstant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            token.checkpoint_count() >= target,
            "query completed without reaching cancellation checkpoint {target}"
        );
        token.cancel(CancelReason::Disconnect);
    })
}

#[test]
fn nested_loop_join_honors_deadline() {
    // 1200 x 1200 = 1.44M inner iterations of the unindexed nested-loop join —
    // seconds of work uninstrumented. With a ~100ms deadline it must return the
    // typed timeout error well under the generous CI bound.
    let engine = compound_join_engine(1200, 1200);
    let cancel = CancelArc::new(ExecCancel::with_deadline(
        CancelInstant::now() + CancelDuration::from_millis(100),
        100,
    ));
    let start = CancelInstant::now();
    let result = engine.execute_powql_readonly_with_cancel(
        r#"Ver as ver join Grp as g on ver.id + 0 = g.version_id and g.field_ns = "f1""#,
        cancel,
    );
    let elapsed = start.elapsed();
    assert!(
        matches!(result, Err(QueryError::Timeout { timeout_ms: 100 })),
        "expected Timeout, got {result:?}"
    );
    assert!(
        elapsed < CancelDuration::from_secs(3),
        "cancellation should be prompt, took {elapsed:?}"
    );
    // The engine is still fully usable after a cancelled query.
    let after = engine.execute_powql_readonly("count(Ver)").unwrap();
    assert!(matches!(after, QueryResult::Scalar(Value::Int(1200))));
}

#[test]
fn nested_loop_join_honors_explicit_cancel() {
    // A second thread owns the same token the executor is polling, matching the
    // server-side shape where socket monitoring signals a blocking query.
    let engine = compound_join_engine(1200, 1200);
    let cancel = CancelArc::new(ExecCancel::new());
    // Entry is checkpoint 1; neither 1200-row input scan reaches the 4096-row
    // interval. Checkpoint 2 is therefore necessarily inside the join product.
    let cancel_thread = cancel_after_checkpoint(CancelArc::clone(&cancel), 2);
    let start = CancelInstant::now();
    let result = engine.execute_powql_readonly_with_cancel(
        r#"Ver as ver join Grp as g on ver.id + 0 = g.version_id and g.field_ns = "f1""#,
        cancel,
    );
    cancel_thread.join().unwrap();
    assert!(
        matches!(result, Err(QueryError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    assert!(start.elapsed() < CancelDuration::from_secs(3));
}

#[test]
fn cooperative_stable_sort_matches_std_stable_sort() {
    let mut actual: Vec<(u32, usize)> = (0..20_000).map(|i| (((i * 37) % 97) as u32, i)).collect();
    let mut expected = actual.clone();
    expected.sort_by_key(|&(key, _)| key);

    super::mem_budget::reset();
    super::plan_exec::cooperative_stable_sort_by(
        &mut actual,
        usize::MAX,
        |&(left, _), &(right, _)| left.cmp(&right),
    )
    .unwrap();
    assert_eq!(
        actual, expected,
        "ordering and equal-key stability must match"
    );
}

#[test]
fn regular_sort_honors_live_cancel_inside_sort_on_both_entry_paths() {
    let mut mutable_engine = item_engine(20_000);
    let mutable_cancel = CancelArc::new(ExecCancel::new());
    // 1 entry + 4 scan + 4 memory-charge + helper entry + first run's
    // before/after checks. Cancellation is signalled between sorted runs.
    let mutable_signal = cancel_after_checkpoint(CancelArc::clone(&mutable_cancel), 12);
    let mutable_result =
        mutable_engine.execute_powql_with_cancel("Item order .id desc", mutable_cancel);
    mutable_signal.join().unwrap();
    assert!(matches!(mutable_result, Err(QueryError::Cancelled)));
    assert!(matches!(
        mutable_engine.execute_powql("count(Item)").unwrap(),
        QueryResult::Scalar(Value::Int(20_000))
    ));

    let readonly_engine = item_engine(20_000);
    let readonly_cancel = CancelArc::new(ExecCancel::new());
    let readonly_signal = cancel_after_checkpoint(CancelArc::clone(&readonly_cancel), 12);
    let readonly_result =
        readonly_engine.execute_powql_readonly_with_cancel("Item order .id desc", readonly_cancel);
    readonly_signal.join().unwrap();
    assert!(matches!(readonly_result, Err(QueryError::Cancelled)));
    assert!(matches!(
        readonly_engine
            .execute_powql_readonly("count(Item)")
            .unwrap(),
        QueryResult::Scalar(Value::Int(20_000))
    ));
}

#[test]
fn window_sort_honors_live_cancel_inside_sort_and_engine_stays_usable() {
    let engine = item_engine(20_000);
    let cancel = CancelArc::new(ExecCancel::new());
    // 1 entry + 4 scan + helper entry + first run's before/after checks.
    let signal = cancel_after_checkpoint(CancelArc::clone(&cancel), 8);
    let result = engine.execute_powql_readonly_with_cancel(
        "Item { .id, rn: row_number() over (order .id desc) }",
        cancel,
    );
    signal.join().unwrap();
    assert!(matches!(result, Err(QueryError::Cancelled)));
    assert!(matches!(
        engine.execute_powql_readonly("count(Item)").unwrap(),
        QueryResult::Scalar(Value::Int(20_000))
    ));
}

#[test]
fn compiled_scan_honors_cancel_and_engine_stays_usable() {
    // Checkpoint 1 is statement entry; checkpoint 2 is reached only after the
    // compiled raw scan has processed 4096 rows. Cancellation is therefore
    // live and must be observed by a later checkpoint inside the same scan.
    let engine = item_engine(20_000);
    let cancel = CancelArc::new(ExecCancel::new());
    let signal = cancel_after_checkpoint(CancelArc::clone(&cancel), 2);
    let result = engine.execute_powql_readonly_with_cancel("Item filter .id >= 0 { id }", cancel);
    signal.join().unwrap();
    assert!(
        matches!(result, Err(QueryError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    // A fresh query with no token scans the whole table normally.
    let full = engine
        .execute_powql_readonly("count(Item filter .id >= 0)")
        .unwrap();
    assert!(matches!(full, QueryResult::Scalar(Value::Int(20_000))));
}

#[test]
fn mutable_alias_scan_honors_live_cancel_and_engine_stays_usable() {
    let mut engine = item_engine(20_000);
    let cancel = CancelArc::new(ExecCancel::new());
    let signal = cancel_after_checkpoint(CancelArc::clone(&cancel), 2);
    let result = engine.execute_powql_with_cancel("Item as i", cancel);
    signal.join().unwrap();
    assert!(
        matches!(result, Err(QueryError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    let after = engine.execute_powql("count(Item)").unwrap();
    assert!(matches!(after, QueryResult::Scalar(Value::Int(20_000))));
}

#[test]
fn readonly_fast_count_honors_live_cancel_and_engine_stays_usable() {
    let engine = item_engine(20_000);
    let cancel = CancelArc::new(ExecCancel::new());
    let signal = cancel_after_checkpoint(CancelArc::clone(&cancel), 2);
    let result = engine.execute_powql_readonly_with_cancel("count(Item)", cancel);
    signal.join().unwrap();
    assert!(
        matches!(result, Err(QueryError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    let after = engine.execute_powql_readonly("count(Item)").unwrap();
    assert!(matches!(after, QueryResult::Scalar(Value::Int(20_000))));
}

#[test]
fn distinct_materialization_honors_live_cancel_and_engine_stays_usable() {
    let engine = item_engine(20_000);
    let cancel = CancelArc::new(ExecCancel::new());
    // Entry + four projection-scan checkpoints + the first checkpoint in the
    // distinct loop. The following distinct checkpoint must see cancellation.
    let signal = cancel_after_checkpoint(CancelArc::clone(&cancel), 6);
    let result = engine.execute_powql_readonly_with_cancel("Item distinct { .id, .v }", cancel);
    signal.join().unwrap();
    assert!(
        matches!(result, Err(QueryError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    let after = engine.execute_powql_readonly("count(Item)").unwrap();
    assert!(matches!(after, QueryResult::Scalar(Value::Int(20_000))));
}

#[test]
fn cancelled_update_before_write_leaves_all_rows_unchanged() {
    // Cancellation must happen before mutation begins. There is no
    // statement-level savepoint for rolling back a partially applied update,
    // so returning Timeout after a logged prefix would be unsafe.
    let mut engine = item_engine(20_000);
    let cancel = CancelArc::new(ExecCancel::with_deadline(
        CancelInstant::now() - CancelDuration::from_millis(1),
        500,
    ));
    let result =
        engine.execute_powql_with_cancel("Item filter .id >= 0 update { v := .v + 1 }", cancel);
    assert!(
        matches!(result, Err(QueryError::Timeout { .. })),
        "expected Timeout, got {result:?}"
    );
    let unchanged = engine.execute_powql("sum(Item { .v })").unwrap();
    assert!(
        matches!(unchanged, QueryResult::Scalar(Value::Int(0))),
        "a cancelled update must not leave a written prefix: {unchanged:?}"
    );

    // A subsequent uncancelled statement applies to every row, proving the
    // cancellation token and executor state were both released.
    let full = engine
        .execute_powql("Item filter .id >= 0 update { v := .v + 1 }")
        .unwrap();
    assert!(
        matches!(full, QueryResult::Modified(20_000)),
        "expected all rows updated, got {full:?}"
    );
}

#[test]
fn cancelled_mutation_does_not_abort_explicit_transaction() {
    let mut engine = item_engine(1);
    engine.execute_powql("begin").unwrap();

    let cancel = CancelArc::new(ExecCancel::new());
    cancel.cancel(CancelReason::Disconnect);
    let result =
        engine.execute_powql_with_cancel("Item filter .id = 0 update { v := .v + 1 }", cancel);
    assert!(matches!(result, Err(QueryError::Cancelled)));

    let unchanged = engine.execute_powql("sum(Item { .v })").unwrap();
    assert!(matches!(unchanged, QueryResult::Scalar(Value::Int(0))));
    let committed = engine.execute_powql("commit").unwrap();
    assert!(matches!(committed, QueryResult::Executed { .. }));
}

#[test]
fn cancel_only_affects_its_own_query() {
    // After a cancelled query, a normal query on the same engine with no token
    // runs to completion — the cancellation is per-query, not sticky.
    let engine = compound_join_engine(600, 600);
    let cancel = CancelArc::new(ExecCancel::new());
    cancel.cancel(CancelReason::Timeout);
    let _ = engine.execute_powql_readonly_with_cancel(
        r#"Ver as ver join Grp as g on ver.id + 0 = g.version_id and g.field_ns = "f1""#,
        cancel,
    );
    // No token installed now: the same join runs fully and returns matches.
    let ok = engine
        .execute_powql_readonly(
            r#"Ver as ver join Grp as g on ver.id = g.version_id and g.field_ns = "f1""#,
        )
        .unwrap();
    match ok {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 600),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn symmetric_rid_dedup_set_is_charged_to_query_memory_budget() {
    super::mem_budget::reset();
    let columns = vec!["a.value".to_string()];
    let rows = vec![vec![Value::Int(10)]];
    let provenance = vec![vec![Some(RowId {
        page_id: 1,
        slot_index: 0,
    })]];
    let error = super::plan_exec::compute_group_aggregate(
        crate::ast::AggFunc::Sum,
        &Expr::QualifiedField {
            qualifier: "a".into(),
            field: "value".into(),
        },
        Some(0),
        super::plan_exec::GroupAggregateContext {
            columns: &columns,
            all_rows: &rows,
            row_indices: &[0],
            source_index: Some(0),
            provenance: Some((&provenance, 1)),
        },
    )
    .unwrap_err();
    assert!(matches!(error, QueryError::MemoryLimitExceeded { .. }));
}
