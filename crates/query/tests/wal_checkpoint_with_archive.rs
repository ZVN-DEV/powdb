//! E18, for the configuration the shipped binaries actually run.
//!
//! `powdb-cli` and `powdb-server` both open through
//! `Engine::new_with_wal_archive`, handing it a hook that is a no-op unless
//! sync is enabled. `Catalog::open_with_wal_archive` only borrows that hook for
//! the open, so the catalog could not run it later and refused to checkpoint at
//! all rather than truncate records it had no way to publish. The refusal was
//! right about the danger and wrong about the remedy: it turned
//! `--wal-checkpoint-bytes` into a no-op in both binaries and let the WAL grow
//! without bound, which is the one thing the threshold exists to prevent.
//!
//! The engine now hands the catalog a hook it owns, so the automatic
//! checkpoint publishes the log and then truncates it.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::wal::{WalRecord, WalRecordType};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_wal_ckpt_archive_{name}_{}_{}",
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

fn wal_len(dir: &Path) -> u64 {
    std::fs::metadata(dir.join("wal.log")).map_or(0, |m| m.len())
}

/// Number of rows the workload writes. Each row is one statement, so each one
/// is its own commit and its own threshold check.
const ROWS: i64 = 200;

/// Open an engine that records every record its archive hook is handed, run
/// the workload, and report the WAL length it leaves behind.
///
/// The workload runs against a *reopened* directory on purpose. That is the
/// path a restarting server takes, and it is the only one that goes through
/// `Catalog::open_with_wal_archive`: `Catalog::create` takes no hook at all, so
/// a freshly created directory never recorded that one existed. The guard this
/// replaces therefore protected reopened directories and silently truncated
/// behind the hook on brand-new ones. Both legs are exercised here.
fn run_workload(dir: &Path, checkpoint_bytes: u64) -> (u64, Vec<WalRecordType>) {
    std::fs::create_dir_all(dir).unwrap();
    let seen: Arc<Mutex<Vec<WalRecordType>>> = Arc::default();

    // Exactly the shape both binaries pass: a plain `Fn` hook, installed for
    // the life of the engine.
    let open = |seen: &Arc<Mutex<Vec<WalRecordType>>>| {
        let recorder = Arc::clone(seen);
        Engine::new_with_wal_archive(dir, move |_dir: &Path, records: &[WalRecord]| {
            recorder
                .lock()
                .unwrap()
                .extend(records.iter().map(|record| record.record_type));
            io::Result::Ok(())
        })
        .unwrap()
    };

    // First open creates the directory and the table; dropping it checkpoints.
    {
        let mut engine = open(&seen);
        exec(&mut engine, "type S { required id: int, v: str }");
    }

    let mut engine = open(&seen);
    engine
        .catalog_mut()
        .set_wal_checkpoint_bytes(checkpoint_bytes);
    let body = "x".repeat(120);
    for i in 0..ROWS {
        exec(
            &mut engine,
            &format!("insert S {{ id := {i}, v := \"{body}\" }}"),
        );
    }

    let len = wal_len(dir);
    // Dropping the engine checkpoints on the way out, which would truncate the
    // log and erase the very thing under test, so measure first.
    drop(engine);
    let seen = seen.lock().unwrap().clone();
    (len, seen)
}

#[test]
fn an_engine_with_a_wal_archive_hook_still_checkpoints_at_the_threshold() {
    // Control: the same workload with the threshold switched off. This is what
    // an unbounded log looks like, and it is what the archive-hook engine used
    // to produce at every threshold setting.
    let unbounded_dir = temp_dir("unbounded");
    let (unbounded_len, _) = run_workload(&unbounded_dir, 0);
    assert!(
        unbounded_len > 8 * 1024,
        "the workload must write a log worth truncating, or the test proves nothing; \
         got {unbounded_len} bytes"
    );

    let bounded_dir = temp_dir("bounded");
    let (bounded_len, archived) = run_workload(&bounded_dir, 4096);

    assert!(
        bounded_len < unbounded_len,
        "an engine opened with an archive hook must still honour the checkpoint \
         threshold: {bounded_len} bytes with the threshold at 4096, {unbounded_len} \
         bytes with it off"
    );
    assert!(
        bounded_len <= 8 * 1024,
        "the threshold is 4096 bytes, so the log must stay near it, not grow to \
         {bounded_len} bytes"
    );

    // And the records were published before they were destroyed. Every
    // truncated byte went through the hook first: that is why running the hook
    // is the fix and deleting the guard is not.
    let inserts = archived
        .iter()
        .filter(|kind| **kind == WalRecordType::Insert)
        .count();
    assert!(
        inserts > 0,
        "the automatic checkpoint truncated the log without publishing a single \
         record: saw {archived:?}"
    );

    std::fs::remove_dir_all(&unbounded_dir).ok();
    std::fs::remove_dir_all(&bounded_dir).ok();
}
