//! Process-level replica crash test: SIGKILL a real process in the middle of
//! a chunked retained-tail apply, then prove the replica data dir reopens
//! cleanly and the apply RESUMES to convergence.
//!
//! The apply-state machine (`apply-state.json`, InProgress/Complete, the
//! `applied_lsn` watermark) exists exactly for this crash; the in-process
//! suites only ever exercise it with polite errors. Here nothing is polite:
//! the child gets SIGKILL — no Drop impls, no flush, no state finalization —
//! at a moment chosen to land between chunk applies.
//!
//! Pattern mirrors crates/server/tests/kill9_durability.rs, but the killed
//! process is this test binary re-executing itself in child mode (there is
//! no long-running replica daemon to spawn; embedded replicas drive
//! `apply_retained_units_chunk` from a host process, which is what dies).
#![cfg(unix)]

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use powdb_storage::catalog::Catalog;
use powdb_storage::types::{ColumnDef, Row, Schema, TypeId, Value};

const CHILD_ENV: &str = "POWDB_REPLICA_KILL9_CHILD";

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "powdb_replica_kill9_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn schema_users() -> Schema {
    Schema {
        table_name: "User".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            },
            ColumnDef {
                name: "email".into(),
                type_id: TypeId::Str,
                required: false,
                position: 1,
            },
        ],
    }
}

fn user_row(id: i64) -> Row {
    vec![Value::Int(id), Value::Str(format!("user{id}@example.com"))]
}

fn rows(cat: &Catalog) -> Vec<i64> {
    let mut ids: Vec<i64> = cat
        .scan("User")
        .unwrap()
        .map(|item| match &item.unwrap().1[0] {
            Value::Int(id) => *id,
            other => panic!("expected int id, got {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

/// Child mode: apply the retained tail in tiny chunks with a sleep between
/// each, so the parent's SIGKILL lands mid-apply with high probability. Runs
/// only when the parent set CHILD_ENV; as a plain test it is a no-op.
#[test]
fn helper_child_applies_in_small_chunks() {
    let Ok(spec) = std::env::var(CHILD_ENV) else {
        return;
    };
    let parts: Vec<&str> = spec.split('\x1f').collect();
    let (replica, retained, snapshot_lsn, remote_lsn): (PathBuf, PathBuf, u64, u64) = (
        parts[0].into(),
        parts[1].into(),
        parts[2].parse().unwrap(),
        parts[3].parse().unwrap(),
    );

    let mut cat = powdb_sync::open_preserving_retained_segments(&replica).unwrap();
    let identity = powdb_sync::read_identity(&replica)
        .unwrap()
        .segment_identity();
    // Signal the parent that the catalog is open and applying is starting.
    std::fs::write(replica.join("child-started"), b"1").unwrap();

    // Fresh from bootstrap, the trusted apply boundary is the snapshot LSN.
    let mut boundary = snapshot_lsn;
    while boundary < remote_lsn {
        // Two units per chunk: one committed single-row insert is exactly
        // (Insert, Commit), so every chunk ends on a transaction boundary.
        let units =
            powdb_sync::segment::read_units_through(&retained, identity, boundary, remote_lsn, 2)
                .unwrap();
        if units.is_empty() {
            break;
        }
        let summary =
            powdb_sync::apply_retained_units_chunk(&mut cat, identity, boundary, &units).unwrap();
        boundary = summary.through_lsn;
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn sigkill_mid_chunked_apply_reopens_and_resumes_to_convergence() {
    // ── primary with a long committed post-snapshot tail ──
    let primary = tmp("primary");
    let mut primary_cat = Catalog::create(&primary).unwrap();
    primary_cat.create_table(schema_users()).unwrap();
    for id in 0..8 {
        primary_cat.insert("User", &user_row(id)).unwrap();
    }
    primary_cat.commit_autocommit().unwrap();
    primary_cat.sync_wal().unwrap();
    primary_cat.create_index_unique("User", "id", true).unwrap();
    let identity = powdb_sync::open_or_create_identity(&primary).unwrap();

    let backup = tmp("backup");
    powdb_backup::full_backup(&mut primary_cat, &backup).unwrap();

    // 400 single-row committed inserts: 400 (Insert, Commit) unit pairs, so
    // the child's 2-unit chunks each end on a transaction boundary and its
    // ~1ms/chunk pacing gives the kill a ~400ms window to land in.
    for id in 8..408 {
        primary_cat.insert("User", &user_row(id)).unwrap();
        primary_cat.commit_autocommit().unwrap();
        primary_cat.sync_wal().unwrap();
    }
    let primary_rows = rows(&primary_cat);

    let replica = tmp("replica");
    let bootstrap = powdb_backup::bootstrap_replica_from_full_backup(
        &mut primary_cat,
        &backup,
        &replica,
        "replica-kill9",
    )
    .unwrap();
    let retained = powdb_sync::retained_segments_dir(&primary);

    // ── spawn ourselves in child mode and SIGKILL mid-apply ──
    let spec = format!(
        "{}\x1f{}\x1f{}\x1f{}",
        replica.display(),
        retained.display(),
        bootstrap.snapshot_lsn,
        bootstrap.remote_lsn
    );
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "helper_child_applies_in_small_chunks",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, &spec)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let started = replica.join("child-started");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !started.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "child never started applying"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(100));
    child.kill().unwrap(); // SIGKILL on unix
    child.wait().unwrap();

    // ── the kill must have landed mid-apply, or this test proves nothing ──
    let reopened = powdb_sync::open_preserving_retained_segments(&replica).unwrap();
    let after_kill = rows(&reopened);
    assert!(
        after_kill.len() >= 8,
        "replica lost bootstrapped rows: {}",
        after_kill.len()
    );
    assert!(
        after_kill.len() < primary_rows.len(),
        "child finished before the kill ({} rows); the test is vacuous — \
         widen the tail or shorten the delay",
        after_kill.len()
    );
    let mut cat = reopened;

    // ── resume from the replica's own recovered boundary ──
    //
    // The natural restart protocol: one whole-tail apply starting at the
    // catalog LSN recovered on reopen. The apply-state reconciler accepts
    // this over any crash shape a SIGKILL can leave — rolled-back intent,
    // completed-but-unflipped intent, or a frontier mid-way through the
    // stranded range (records redo into pages one at a time, so the kill
    // can land between two of them) — because the recovered catalog LSN
    // pins the durable prefix. Each relaxation exists because a run of
    // THIS test wedged without it: the first run locally on "not a trusted
    // completed apply boundary", the first ASan CI run on "another
    // retained-tail apply is in progress".
    let sid = identity.segment_identity();
    let resume_from = cat.max_lsn();
    let applied = powdb_sync::apply_retained_tail(
        &mut cat,
        &retained,
        sid,
        resume_from,
        bootstrap.remote_lsn,
    )
    .unwrap();
    assert_eq!(applied.through_lsn, bootstrap.remote_lsn);
    assert_eq!(
        rows(&cat),
        primary_rows,
        "replica must converge after resume"
    );
    assert_eq!(
        cat.index_lookup("User", "id", &Value::Int(300))
            .unwrap()
            .unwrap()[0],
        Value::Int(300),
        "indexes must be consistent after the resumed apply"
    );

    // A clean reopen after convergence, for good measure.
    drop(cat);
    let final_cat = powdb_sync::open_preserving_retained_segments(&replica).unwrap();
    assert_eq!(rows(&final_cat), primary_rows);

    let _ = writeln!(
        std::io::stdout(),
        "killed at {} rows, converged at {}",
        after_kill.len(),
        primary_rows.len()
    );
    for dir in [&primary, &backup, &replica] {
        let _ = std::fs::remove_dir_all(dir);
    }
}
