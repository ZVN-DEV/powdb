//! Crash-recovery tests for the one update shape every other crash test
//! misses: an update that *grows* a row past its page's free space, so the
//! heap relocates it via delete+insert and hands back a NEW RowId.
//!
//! Why this file exists separately from `wal_recovery.rs`: every update case
//! there shrinks or patches in place (see `test_crash_recovery_var_col_update`,
//! which shrinks 21 chars to 1 on purpose to stay on the in-place path). An
//! in-place update is idempotent, so re-applying its WAL record on every
//! recovery is harmless. A *relocating* update is not idempotent, replaying
//! it against a heap that already holds the pre-update row produces a second
//! live copy of that row unless the per-page redo guard actually arms.

use powdb_storage::catalog::Catalog;
use powdb_storage::types::{ColumnDef, RowId, Schema, TypeId, Value};

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_wal_relocating_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn user_schema() -> Schema {
    Schema {
        table_name: "users".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            },
            ColumnDef {
                name: "name".into(),
                type_id: TypeId::Str,
                required: true,
                position: 1,
            },
        ],
    }
}

/// Collect every `id` in the table, sorted. Panics on a non-Int id so a
/// decode regression surfaces as a test failure rather than a silent skip.
fn all_ids(cat: &Catalog) -> Vec<i64> {
    let mut ids: Vec<i64> = cat
        .scan("users")
        .unwrap()
        .map(|item| match item.unwrap().1[0] {
            Value::Int(i) => i,
            ref other => panic!("expected Int id, got {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

fn assert_no_duplicate_ids(ids: &[i64]) {
    let mut dupes: Vec<i64> = ids
        .windows(2)
        .filter(|w| w[0] == w[1])
        .map(|w| w[0])
        .collect();
    dupes.dedup();
    assert!(
        dupes.is_empty(),
        "replay duplicated rows: ids {dupes:?} appear more than once"
    );
}

/// The relocation itself: a 300-byte row updated to 3000 bytes cannot stay on
/// its page, so `HeapFile::update` falls back to delete + insert and the row
/// changes RowId. Guards the precondition of the crash test below: if the
/// heap ever grows in-place page splitting, that test stops testing anything.
#[test]
fn growing_update_relocates_the_row() {
    let dir = temp_dir("relocates");
    std::fs::create_dir_all(&dir).unwrap();

    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(user_schema()).unwrap();
    let mut rids = Vec::new();
    for i in 0..200i64 {
        rids.push(
            cat.insert("users", &vec![Value::Int(i), Value::Str("x".repeat(300))])
                .unwrap(),
        );
    }
    let old_rid = rids[11];
    let new_rid = cat
        .update(
            "users",
            old_rid,
            &vec![Value::Int(11), Value::Str("y".repeat(3000))],
        )
        .unwrap();
    assert_ne!(
        new_rid, old_rid,
        "a 300B -> 3000B update must relocate the row off its page"
    );

    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

/// A1/A2: crash after a relocating update, with the pre-update row already
/// made durable by a checkpoint. Replay must not resurrect the pre-update
/// copy alongside the relocated one.
///
/// The `checkpoint()` is what arms the bug: it flushes the pre-update row to
/// disk, so the on-disk heap holds row 11 at its original RowId while the
/// relocating update's tombstone (and its new copy) live only in the lost
/// dirty pages. Replay then re-applies the Update record on top of a heap
/// that still has the old row.
#[test]
fn crash_after_relocating_update_does_not_duplicate_the_row() {
    let dir = temp_dir("no_dup");
    std::fs::create_dir_all(&dir).unwrap();

    let relocated_rid: RowId;
    {
        let mut cat = Catalog::create(&dir).unwrap();
        // Stand in for a workload big enough to exceed the dirty-page budget:
        // once the buffer is over its ceiling, `park_hot_page` relieves it by
        // writing every dirty page to disk. That is the ordinary production
        // path by which a mutation reaches disk *without* the WAL being
        // truncated (only `checkpoint` truncates), and it is what makes the
        // relocation durable while its Update record is still replayable.
        cat.set_dirty_page_budget_bytes(2 * 4096);
        cat.create_table(user_schema()).unwrap();
        let mut rids = Vec::new();
        for i in 0..200i64 {
            rids.push(
                cat.insert("users", &vec![Value::Int(i), Value::Str("x".repeat(300))])
                    .unwrap(),
            );
        }
        // Make the pre-update row durable and empty the WAL. Everything the
        // WAL holds after this point is exactly the un-flushed tail.
        cat.checkpoint().unwrap();

        relocated_rid = cat
            .update(
                "users",
                rids[11],
                &vec![Value::Int(11), Value::Str("y".repeat(3000))],
            )
            .unwrap();
        assert_ne!(
            relocated_rid, rids[11],
            "update must relocate to a new page"
        );

        // Keep writing after the relocation so the Update record is followed
        // by higher-LSN Insert records, the realistic crash shape, and the
        // one that lets a badly-ordered redo clobber later rows.
        for i in 200..260i64 {
            cat.insert("users", &vec![Value::Int(i), Value::Str("x".repeat(300))])
                .unwrap();
        }

        cat.sync_wal().unwrap();
        std::mem::forget(cat);
    }

    {
        let cat = Catalog::open(&dir).unwrap();
        let ids = all_ids(&cat);
        assert_no_duplicate_ids(&ids);
        assert_eq!(
            ids,
            (0..260i64).collect::<Vec<_>>(),
            "expected exactly ids 0..260 after replaying a relocating update"
        );
        let row = cat
            .get("users", relocated_rid)
            .expect("relocated row must be readable at the RowId the update returned");
        assert_eq!(row[0], Value::Int(11));
        assert_eq!(
            row[1],
            Value::Str("y".repeat(3000)),
            "replay must land the grown row, not the pre-update one"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// Recovery must stay idempotent: crashing *during* recovery and recovering
/// again must not add a second copy either. Runs the same relocating-update
/// crash, then re-crashes the recovered catalog before it can checkpoint.
#[test]
fn repeated_recovery_after_relocating_update_stays_idempotent() {
    let dir = temp_dir("twice");
    std::fs::create_dir_all(&dir).unwrap();

    {
        let mut cat = Catalog::create(&dir).unwrap();
        cat.set_dirty_page_budget_bytes(2 * 4096);
        cat.create_table(user_schema()).unwrap();
        let mut rids = Vec::new();
        for i in 0..200i64 {
            rids.push(
                cat.insert("users", &vec![Value::Int(i), Value::Str("x".repeat(300))])
                    .unwrap(),
            );
        }
        cat.checkpoint().unwrap();
        cat.update(
            "users",
            rids[11],
            &vec![Value::Int(11), Value::Str("y".repeat(3000))],
        )
        .unwrap();
        for i in 200..260i64 {
            cat.insert("users", &vec![Value::Int(i), Value::Str("x".repeat(300))])
                .unwrap();
        }
        cat.sync_wal().unwrap();
        std::mem::forget(cat);
    }

    // First recovery, then crash again before any clean shutdown.
    {
        let cat = Catalog::open(&dir).unwrap();
        let ids = all_ids(&cat);
        assert_no_duplicate_ids(&ids);
        std::mem::forget(cat);
    }

    // Second recovery: the WAL was truncated by the first one, so this open
    // reads the heap replay left behind.
    {
        let cat = Catalog::open(&dir).unwrap();
        let ids = all_ids(&cat);
        assert_no_duplicate_ids(&ids);
        assert_eq!(
            ids,
            (0..260i64).collect::<Vec<_>>(),
            "a second recovery must not change the recovered row set"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}
