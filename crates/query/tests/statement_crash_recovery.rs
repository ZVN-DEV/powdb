//! The parent kills a child at an exact statement boundary; none of the child
//! engine's Drop/checkpoint code runs. This models process death, not power loss.
#![cfg(feature = "testing")]

use powdb_query::executor::{Engine, StatementCommitPhase};
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use powdb_storage::wal::WalSyncMode;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const ROWS: usize = 150;

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn rendezvous(phase: StatementCommitPhase) {
    let selected = std::env::var("POWDB_CRASH_TEST_PHASE").unwrap();
    if (selected == "before" && phase == StatementCommitPhase::BeforeCommit)
        || (selected == "after" && phase == StatementCommitPhase::AfterCommit)
    {
        std::fs::write(
            std::env::var_os("POWDB_CRASH_TEST_READY").unwrap(),
            b"ready",
        )
        .unwrap();
        loop {
            std::thread::park();
        }
    }
}

#[test]
#[ignore = "subprocess fixture invoked by logged_statement_commit_survives_process_death"]
fn statement_crash_child() {
    let path = std::env::var_os("POWDB_CRASH_TEST_DIR").expect("parent supplies database");
    let mut engine = Engine::new(std::path::Path::new(&path)).unwrap();
    let mode = match std::env::var("POWDB_CRASH_TEST_MODE").unwrap().as_str() {
        "full" => WalSyncMode::Full,
        "normal" => WalSyncMode::Normal,
        other => panic!("unexpected WAL mode: {other}"),
    };
    engine.catalog_mut().set_wal_sync_mode(mode);
    engine.set_statement_commit_hook_for_testing(Some(rendezvous));
    engine
        .execute_powql("Item update { value := .value + 1000 }")
        .unwrap();
    panic!("parent should kill the process at the selected commit boundary");
}

#[test]
fn logged_statement_commit_survives_process_death() {
    // Normal acknowledges bytes written to the OS, so it survives process
    // death too; this does not promise survival of machine/power failure.
    // Off intentionally has no WAL crash guarantee. Its live rollback and
    // graceful reopen contracts are covered by statement_atomicity.rs.
    for (mode, phase) in [
        ("full", "before"),
        ("full", "after"),
        ("normal", "before"),
        ("normal", "after"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let ready = dir.path().join("ready");
        {
            let mut engine = Engine::new(&data).unwrap();
            engine
                .execute_powql("type Item { required unique id: int, value: int }")
                .unwrap();
            engine.execute_powql("begin").unwrap();
            for id in 0..ROWS {
                engine
                    .execute_powql(&format!("insert Item {{ id := {id}, value := {id} }}"))
                    .unwrap();
            }
            engine.execute_powql("commit").unwrap();
        }
        let mut child = ChildGuard(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "statement_crash_child",
                    "--nocapture",
                ])
                .env("POWDB_CRASH_TEST_DIR", &data)
                .env("POWDB_CRASH_TEST_READY", &ready)
                .env("POWDB_CRASH_TEST_PHASE", phase)
                .env("POWDB_CRASH_TEST_MODE", mode)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        while !ready.exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "child exited before {phase} checkpoint"
            );
            assert!(
                Instant::now() < deadline,
                "child failed to reach {phase} checkpoint"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        let mut reopened = Engine::new(&data).unwrap();
        match reopened
            .execute_powql("Item order .id { .id, .value }")
            .unwrap()
        {
            QueryResult::Rows { rows, .. } => {
                assert_eq!(rows.len(), ROWS);
                for (id, row) in rows.iter().enumerate() {
                    let value = id as i64 + if phase == "after" { 1000 } else { 0 };
                    assert_eq!(
                        row,
                        &vec![Value::Int(id as i64), Value::Int(value)],
                        "{mode}/{phase}, row {id}"
                    );
                }
            }
            result => panic!("unexpected recovery result: {result:?}"),
        }
        // Exercise the persisted unique index as well as the scan.
        match reopened
            .execute_powql("Item filter .id = 149 { .value }")
            .unwrap()
        {
            QueryResult::Rows { rows, .. } => assert_eq!(
                rows,
                vec![vec![Value::Int(if phase == "after" { 1149 } else { 149 })]]
            ),
            result => panic!("unexpected indexed result: {result:?}"),
        }
    }
}
