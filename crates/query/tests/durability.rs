//! Durability contract suite — the crash/restart/recovery scenarios that
//! have repeatedly shipped P0 data-loss bugs (v0.4.1, v0.4.2, v0.4.3 were
//! all yanked). Every test here encodes a guarantee a real deployment
//! depends on: **acknowledged writes are never lost.**
//!
//! Two crash simulators are used:
//!   - `std::mem::forget(engine)` — a hard crash with NO checkpoint. The
//!     heap on disk is left in its pre-mutation state; the WAL holds every
//!     logged record. The next `Engine::new` must replay them.
//!   - dropping the engine normally — a *graceful* shutdown. `Catalog`'s
//!     Drop runs `checkpoint()`, which flushes dirty heap pages and
//!     truncates the WAL. This is the "clean restart / redeploy" path.
//!
//! The combination "graceful restart, then write, then hard crash" is the
//! one that exposed the v0.4.3-era P0: a graceful shutdown truncates the
//! WAL, and if the LSN counter resets on reopen while heap pages keep
//! their high LSNs, post-restart writes reuse persisted LSNs and replay
//! discards them as "already applied."

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_durability_{name}_{}_{}",
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

/// Scalar `count(...)` value, extracted from whichever result shape the
/// engine returns (Scalar on the fast path, single-row Rows otherwise).
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

fn int_field(engine: &mut Engine, query: &str, field: &str) -> Option<i64> {
    match exec(engine, query) {
        QueryResult::Rows { columns, rows } => {
            let idx = columns.iter().position(|c| c == field)?;
            rows.first().and_then(|r| match &r[idx] {
                Value::Int(n) => Some(*n),
                _ => None,
            })
        }
        _ => None,
    }
}

/// P0 (v0.4.3-era): writes issued after a crash-recovery must survive a
/// *second* crash. This is the realistic deploy/crash-loop shape and the
/// exact bug behind the yanked releases.
///
/// Mechanism: WAL replay stamps each recovered page with the record's LSN.
/// `Wal::open` then leaves `next_lsn` at 1, so the writes taken after
/// recovery reuse LSNs at or below those stamped page LSNs. On the next
/// crash, replay's idempotency guard (`rec.lsn <= max page LSN`) skips
/// them — the writes vanish. LSNs must be monotonic across restarts:
/// `next_lsn` has to be restored to `max(page LSN) + 1` on open.
///
/// Sequence: insert 200 → hard crash → reopen (replay stamps pages) →
/// insert 50 → hard crash → reopen → all 250 must be present.
#[test]
fn test_writes_after_crash_recovery_survive_second_crash() {
    let dir = temp_dir("after_recovery");
    std::fs::create_dir_all(&dir).unwrap();

    // Session 1: load 200 rows, then HARD crash (no checkpoint).
    {
        let mut engine = Engine::new(&dir).unwrap();
        exec(&mut engine, "type K { required id: int, v: int }");
        for i in 1..=200i64 {
            exec(&mut engine, &format!("insert K {{ id := {i}, v := {i} }}"));
        }
        assert_eq!(count(&mut engine, "count(K)"), 200);
        std::mem::forget(engine); // crash #1
    }

    // Session 2: recovery replays + stamps pages with LSNs 1..=200. Now
    // write 50 MORE rows, then crash again. With the bug, these 50 reuse
    // LSNs 1..=50 (next_lsn reset) which are <= the stamped page LSNs.
    {
        let mut engine = Engine::new(&dir).unwrap();
        assert_eq!(
            count(&mut engine, "count(K)"),
            200,
            "the 200 rows must be recovered after crash #1"
        );
        for i in 201..=250i64 {
            exec(&mut engine, &format!("insert K {{ id := {i}, v := {i} }}"));
        }
        assert_eq!(count(&mut engine, "count(K)"), 250);
        std::mem::forget(engine); // crash #2
    }

    // Session 3: recover again. All 250 rows must survive — the 50 writes
    // taken after the first recovery must NOT be skipped.
    {
        let mut engine = Engine::new(&dir).unwrap();
        assert_eq!(
            count(&mut engine, "count(K)"),
            250,
            "writes taken after crash-recovery were lost on the next crash — \
             next_lsn reset to 1 on WAL open and replay skipped them"
        );
        for i in 201..=250i64 {
            assert_eq!(
                int_field(&mut engine, &format!("K filter .id = {i} {{ .v }}"), "v"),
                Some(i),
                "row id={i} written after recovery is missing after crash #2"
            );
        }
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// Stress version of the above: many restart→write→crash cycles in a row,
/// mirroring a service that redeploys/crash-loops repeatedly. Each cycle
/// adds rows after a clean restart and then hard-crashes; every row from
/// every cycle must still be present at the end.
#[test]
fn test_repeated_restart_write_crash_cycles() {
    let dir = temp_dir("restart_cycles");
    std::fs::create_dir_all(&dir).unwrap();

    {
        let mut engine = Engine::new(&dir).unwrap();
        exec(&mut engine, "type K { required id: int }");
        // graceful drop
    }

    let cycles = 5i64;
    let per_cycle = 4i64;
    for c in 0..cycles {
        // graceful restart, write, hard crash
        let mut engine = Engine::new(&dir).unwrap();
        for j in 0..per_cycle {
            let id = c * per_cycle + j;
            exec(&mut engine, &format!("insert K {{ id := {id} }}"));
        }
        if c == cycles - 1 {
            // last cycle: graceful so the final state is fully flushed
            // (the intermediate cycles already proved crash-durability)
        } else {
            std::mem::forget(engine);
        }
    }

    let mut engine = Engine::new(&dir).unwrap();
    assert_eq!(
        count(&mut engine, "count(K)"),
        cycles * per_cycle,
        "rows from earlier restart→write→crash cycles were lost"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Mixed-mutation crash recovery: a single session that inserts, updates,
/// deletes, and upserts, then hard-crashes. Every committed effect must be
/// exactly reflected after recovery — no reverted deletes, no lost updates.
/// (The v0.4.3 full-scale test saw a 4240-row delete and a bulk update
/// revert after a crash; this is the shape, scaled down.)
#[test]
fn test_mixed_mutations_survive_crash() {
    let dir = temp_dir("mixed_mutations");
    std::fs::create_dir_all(&dir).unwrap();

    {
        let mut engine = Engine::new(&dir).unwrap();
        exec(
            &mut engine,
            "type P { required id: int, price: int, tag: str }",
        );
        for i in 1..=300i64 {
            exec(
                &mut engine,
                &format!(
                    "insert P {{ id := {i}, price := {p}, tag := \"t\" }}",
                    p = i
                ),
            );
        }
        // update the first 100 prices
        exec(&mut engine, "P filter .id <= 100 update { price := 9999 }");
        // delete a 50-row band
        exec(&mut engine, "P filter .id > 250 delete");
        // upsert: update an existing row + insert a brand-new one
        exec(
            &mut engine,
            "upsert P on .id { id := 50, price := 7777, tag := \"u\" }",
        );
        exec(
            &mut engine,
            "upsert P on .id { id := 400, price := 1, tag := \"new\" }",
        );
        std::mem::forget(engine); // crash
    }

    let mut engine = Engine::new(&dir).unwrap();
    // 300 - 50 deleted + 1 upsert-insert = 251
    assert_eq!(
        count(&mut engine, "count(P)"),
        251,
        "row count wrong after crash recovery of mixed mutations"
    );
    assert_eq!(
        count(&mut engine, "count(P filter .id > 250 { .id })"),
        1,
        "deleted band reappeared after crash (only the upserted id=400 should remain > 250)"
    );
    assert_eq!(
        int_field(&mut engine, "P filter .id = 100 { .price }", "price"),
        Some(9999),
        "bulk update reverted after crash"
    );
    assert_eq!(
        int_field(&mut engine, "P filter .id = 50 { .price }", "price"),
        Some(7777),
        "upsert-update reverted after crash"
    );
    assert_eq!(
        int_field(&mut engine, "P filter .id = 400 { .price }", "price"),
        Some(1),
        "upsert-insert lost after crash"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Cross-table: mutations on table B followed by DDL (add column, which
/// stamps every page of A with the DDL's LSN) on table A, then a crash.
/// B's mutations must survive — stamping A's pages must not cause B's
/// records to be skipped on replay. Mirrors the full-scale test where
/// DDL ran on User/Order while Event/Product had pending mutations.
#[test]
fn test_cross_table_mutations_with_ddl_survive_crash() {
    let dir = temp_dir("cross_table_ddl");
    std::fs::create_dir_all(&dir).unwrap();

    {
        let mut engine = Engine::new(&dir).unwrap();
        exec(&mut engine, "type A { required id: int, name: str }");
        exec(&mut engine, "type B { required id: int, val: int }");
        for i in 1..=150i64 {
            exec(
                &mut engine,
                &format!("insert A {{ id := {i}, name := \"a\" }}"),
            );
            exec(
                &mut engine,
                &format!("insert B {{ id := {i}, val := {i} }}"),
            );
        }
        // mutate B
        exec(&mut engine, "B filter .id > 100 delete");
        exec(&mut engine, "B filter .id <= 50 update { val := -1 }");
        // DDL on A (stamps all A pages with the DDL LSN) + index
        exec(&mut engine, "alter A add column score: int");
        exec(&mut engine, "alter A add index .id");
        std::mem::forget(engine); // crash
    }

    let mut engine = Engine::new(&dir).unwrap();
    assert_eq!(
        count(&mut engine, "count(A)"),
        150,
        "A rows lost after crash"
    );
    assert_eq!(
        count(&mut engine, "count(B)"),
        100,
        "B delete reverted after crash (DDL on A skipped B's records)"
    );
    assert_eq!(
        count(&mut engine, "count(B filter .val = -1 { .id })"),
        50,
        "B bulk update reverted after crash"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Prepared-query inserts must be crash-durable too. The executor has an
/// insert fast path for prepared statements (`execute_prepared`) that builds
/// the Row from the literal slice and dispatches straight into the table —
/// it must still log to the WAL, or a crash loses the rows.
#[test]
fn test_prepared_insert_survives_crash() {
    use powdb_query::ast::Literal;
    let dir = temp_dir("prepared_insert");
    std::fs::create_dir_all(&dir).unwrap();

    {
        let mut engine = Engine::new(&dir).unwrap();
        exec(&mut engine, "type K { required id: int, v: int }");
        let prep = engine
            .prepare("insert K { id := 0, v := 0 }")
            .expect("prepare insert");
        for i in 1..=50i64 {
            engine
                .execute_prepared(&prep, &[Literal::Int(i), Literal::Int(i * 10)])
                .expect("prepared insert");
        }
        assert_eq!(count(&mut engine, "count(K)"), 50);
        std::mem::forget(engine); // crash
    }

    let mut engine = Engine::new(&dir).unwrap();
    assert_eq!(
        count(&mut engine, "count(K)"),
        50,
        "prepared inserts were lost on crash — the fast path must WAL-log"
    );
    assert_eq!(
        int_field(&mut engine, "K filter .id = 25 { .v }", "v"),
        Some(250)
    );

    std::fs::remove_dir_all(&dir).ok();
}
