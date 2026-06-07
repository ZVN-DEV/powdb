//! The critical restore invariant: a database rebuilt from a backup must
//! accept new writes that survive a crash. This proves `Catalog::open` reset
//! `next_lsn` above the restored data's LSN high-water mark (guards the
//! v0.4.3 "post-restart writes lost" data-loss P0).

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::catalog::Catalog;
use powdb_storage::types::Value;

/// A fresh, non-existent temp path. `Engine::new` / `Catalog::open` create the
/// directory on demand; `restore` creates its dest dir itself.
fn tmp(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    // pid + atomic counter => unique even when the clock is too coarse to
    // distinguish two parallel calls (macOS CI collided on nanos alone).
    let uniq = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "powdb_rd_{tag}_{}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        uniq
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn exec(e: &mut Engine, q: &str) -> QueryResult {
    e.execute_powql(q)
        .unwrap_or_else(|err| panic!("`{q}` failed: {err}"))
}

// Same count-extraction shape as crates/query/tests/multi_row_insert.rs::count.
fn count(e: &mut Engine, q: &str) -> i64 {
    match exec(e, q) {
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

#[test]
fn writes_after_restore_survive_a_crash() {
    // 1. Build a DB with 100 rows via the Engine, then drop it.
    let src = tmp("src");
    {
        let mut e = Engine::new(&src).unwrap();
        exec(&mut e, "type T { required id: int }");
        for i in 0..100 {
            exec(&mut e, &format!("insert T {{ id := {i} }}"));
        }
        // graceful drop flushes; WAL holds the data either way
    }

    // 2. Open a Catalog on that dir and take a full backup.
    let backup = tmp("bkp");
    {
        let mut cat = Catalog::open(&src).unwrap();
        powdb_backup::full_backup(&mut cat, &backup).unwrap();
    }

    // 3. Restore into a fresh dir.
    let restored = tmp("restored");
    powdb_backup::restore(&backup, &restored).unwrap();

    // 4. Open the restored DB, confirm 100 rows, write 50 MORE, then HARD CRASH
    //    (mem::forget skips Drop/checkpoint — the 50 rows live only in the WAL).
    {
        let mut e = Engine::new(&restored).unwrap();
        assert_eq!(
            count(&mut e, "count(T)"),
            100,
            "restored rows must be present"
        );
        for i in 100..150 {
            exec(&mut e, &format!("insert T {{ id := {i} }}"));
        }
        std::mem::forget(e); // crash before checkpoint
    }

    // 5. Reopen — the post-restore writes MUST replay. If next_lsn was reset
    //    wrong during restore, these 50 rows vanish and the count is 100.
    let mut e = Engine::new(&restored).unwrap();
    assert_eq!(
        count(&mut e, "count(T)"),
        150,
        "writes made after restore must survive a crash — restore must not reset next_lsn"
    );
}
