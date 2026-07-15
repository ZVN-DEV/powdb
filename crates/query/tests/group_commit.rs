//! WAL group-commit contract suite (Full sync mode).
//!
//! Full mode's promise is untouched by group commit: **no statement is
//! acknowledged before an fsync covering its WAL records has returned.**
//! What changes is *who* performs the fsync — overlapping committers share
//! one (leader/follower over the WAL's shared sync fd) instead of paying one
//! each. These tests pin down the three load-bearing properties:
//!
//!   1. A lone sequential committer still fsyncs exactly once per commit
//!      (no timers, no batching delay — the no-regression guarantee).
//!   2. Overlapping committers produce FEWER fsyncs than commits
//!      (the throughput win), and every acknowledged commit survives a
//!      hard crash (`std::mem::forget`, same simulator as durability.rs).
//!   3. A coalesced batch — many commits covered by a single fsync — is
//!      fully recovered by WAL replay after a hard crash.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use std::sync::{Arc, Barrier, RwLock};

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_group_commit_{name}_{}_{}",
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

/// No-regression guarantee: a lone connection issuing sequential awaited
/// statements pays exactly one fsync per commit — group commit must never
/// add a wait for company that isn't there.
#[test]
fn test_sequential_committer_fsyncs_once_per_commit() {
    let dir = temp_dir("sequential");
    std::fs::create_dir_all(&dir).unwrap();
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type S { required id: int, v: int }");

    let base = engine.wal_fsync_count();
    for i in 0..20i64 {
        exec(&mut engine, &format!("insert S {{ id := {i}, v := {i} }}"));
    }
    assert_eq!(
        engine.wal_fsync_count() - base,
        20,
        "a lone sequential committer must fsync exactly once per commit"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The throughput win, deterministically: register many deferred commits
/// before waiting on any of them. The first wait's fsync covers the whole
/// batch; every other wait returns without an fsync. Then prove the batch
/// is crash-durable: hard-crash (no checkpoint) and replay must restore
/// every acknowledged commit.
#[test]
fn test_coalesced_batch_single_fsync_and_crash_recovery() {
    let dir = temp_dir("coalesced_batch");
    std::fs::create_dir_all(&dir).unwrap();

    {
        let mut engine = Engine::new(&dir).unwrap();
        exec(&mut engine, "type B { required id: int, v: int }");

        // Pipelined burst: each statement commits with deferred durability,
        // stacking up tickets the way in-flight commits do on a live system.
        let mut tickets = Vec::new();
        for i in 0..10i64 {
            let (res, ticket) = engine.run_with_deferred_durability(|e| {
                e.execute_powql(&format!("insert B {{ id := {i}, v := {i} }}"))
            });
            res.unwrap();
            tickets.push(ticket.expect("Full mode must hand out a durability ticket"));
        }

        let base = engine.wal_fsync_count();
        // Wait in submission order: the first ticket becomes the group-commit
        // leader and its fsync covers all ten generations.
        for ticket in tickets {
            ticket.wait().unwrap();
        }
        assert_eq!(
            engine.wal_fsync_count() - base,
            1,
            "ten queued commits must be covered by a single fsync"
        );
        assert_eq!(count(&mut engine, "count(B)"), 10);
        std::mem::forget(engine); // hard crash: no Drop, no checkpoint
    }

    {
        let mut engine = Engine::new(&dir).unwrap();
        assert_eq!(
            count(&mut engine, "count(B)"),
            10,
            "every commit acknowledged after the coalesced fsync must survive a hard crash"
        );
        for i in 0..10i64 {
            assert_eq!(
                count(&mut engine, &format!("count(B filter .id = {i})")),
                1,
                "row {i} missing after replay of a coalesced batch"
            );
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// Concurrent committers through the shared-engine lock pattern the server
/// uses: append + register under the write lock, drop the lock, wait for a
/// covering fsync. Every acknowledged commit must survive a hard crash on
/// every attempt (strict), and at least one attempt must show fsync
/// coalescing (fewer fsyncs than commits). Coalescing per run is a
/// scheduling-dependent property, not a guarantee: if the durability worker
/// keeps pace with the producers (observed under AddressSanitizer's uniform
/// slowdown), every commit legally gets its own fsync. Retrying with a fresh
/// directory turns the flaky per-run assertion into a stable capability
/// check without weakening the durability half.
#[test]
fn test_concurrent_committers_coalesce_and_survive_crash() {
    const THREADS: usize = 8;
    const PER_THREAD: i64 = 20;
    const TOTAL: i64 = THREADS as i64 * PER_THREAD;
    const ATTEMPTS: usize = 5;

    let mut fsyncs_seen = Vec::new();
    for attempt in 0..ATTEMPTS {
        let dir = temp_dir(&format!("concurrent-{attempt}"));
        std::fs::create_dir_all(&dir).unwrap();

        let fsyncs = {
            let mut engine = Engine::new(&dir).unwrap();
            exec(&mut engine, "type C { required id: int, v: int }");
            let base = engine.wal_fsync_count();
            let engine = Arc::new(RwLock::new(engine));

            let barrier = Arc::new(Barrier::new(THREADS));
            let mut handles = Vec::new();
            for t in 0..THREADS {
                let engine = Arc::clone(&engine);
                let barrier = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    barrier.wait();
                    for i in 0..PER_THREAD {
                        let id = t as i64 * PER_THREAD + i;
                        let (res, ticket) = {
                            let mut eng = engine.write().unwrap();
                            eng.run_with_deferred_durability(|e| {
                                e.execute_powql(&format!("insert C {{ id := {id}, v := {id} }}"))
                            })
                        };
                        res.unwrap();
                        // The lock is dropped; only now do we wait for
                        // coverage, exactly the server's
                        // acknowledge-after-fsync ordering.
                        ticket
                            .expect("Full mode must hand out a durability ticket")
                            .wait()
                            .unwrap();
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }

            let mut engine = Arc::try_unwrap(engine)
                .unwrap_or_else(|_| panic!("engine still shared after join"))
                .into_inner()
                .unwrap();
            let fsyncs = engine.wal_fsync_count() - base;
            assert_eq!(count(&mut engine, "count(C)"), TOTAL);
            std::mem::forget(engine); // hard crash: no Drop, no checkpoint
            fsyncs
        };

        {
            let mut engine = Engine::new(&dir).unwrap();
            assert_eq!(
                count(&mut engine, "count(C)"),
                TOTAL,
                "all acknowledged concurrent commits must survive a hard crash"
            );
        }
        std::fs::remove_dir_all(&dir).ok();

        if fsyncs < TOTAL as u64 {
            return;
        }
        fsyncs_seen.push(fsyncs);
    }
    panic!(
        "{TOTAL} overlapping commits never shared an fsync in {ATTEMPTS} \
         attempts (fsync counts: {fsyncs_seen:?}); group commit is not \
         coalescing"
    );
}

/// Deferral is scoped: outside `run_with_deferred_durability` the engine
/// behaves exactly as before (durable inline, no ticket left behind), and a
/// deferred statement's ticket is mandatory in Full mode.
#[test]
fn test_deferral_scope_is_tight() {
    let dir = temp_dir("scope");
    std::fs::create_dir_all(&dir).unwrap();
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type T { required id: int }");

    // Deferred statement hands out a ticket.
    let (res, ticket) =
        engine.run_with_deferred_durability(|e| e.execute_powql("insert T { id := 1 }"));
    res.unwrap();
    let ticket = ticket.expect("deferred Full-mode commit must produce a ticket");

    // A plain statement afterwards is durable inline and leaves no claim.
    let base = engine.wal_fsync_count();
    exec(&mut engine, "insert T { id := 2 }");
    assert!(
        engine.wal_fsync_count() > base,
        "inline statement must fsync before returning"
    );

    // The earlier ticket is already covered by the inline fsync (generations
    // are cumulative) — waiting is a no-op, not an error.
    ticket.wait().unwrap();

    // Reads never produce durability claims.
    let (res, ticket) = engine.run_with_deferred_durability(|e| e.execute_powql("count(T)"));
    res.unwrap();
    assert!(
        ticket.is_none(),
        "reads must not register durability claims"
    );
    std::fs::remove_dir_all(&dir).ok();
}
