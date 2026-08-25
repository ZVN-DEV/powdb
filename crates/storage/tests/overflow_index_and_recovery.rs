//! Storage-layer regression tests for the v1/v2 overflow defect class:
//!
//! - P0-1: WAL replay of a spilled (v2) row must not panic / brick the DB, and
//!   the committed value must survive reopen byte-exact (double-replay too).
//! - P2  : delete-time index maintenance must not leave a dangling btree entry
//!   for a spilled indexed column (via create-index-after-spill), and
//!   `plan_spill` keeps indexed columns inline.
//! - sweep: rollback of a spilled insert then sweep reclaims the orphaned
//!   chain pages, while committed data stays intact.

use powdb_storage::catalog::Catalog;
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_ovf_idx_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// id (int) + s (str) + b (str). Used for the P2 index tests.
fn isb_schema() -> Schema {
    Schema {
        table_name: "docs".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            },
            ColumnDef {
                name: "s".into(),
                type_id: TypeId::Str,
                required: true,
                position: 1,
            },
            ColumnDef {
                name: "b".into(),
                type_id: TypeId::Str,
                required: true,
                position: 2,
            },
        ],
    }
}

// ── P0-1: replay of a spilled row must not brick, and must survive reopen ───

#[test]
fn p0_1_spilled_row_survives_crash_replay_byte_exact() {
    let dir = temp_dir("replay");
    std::fs::create_dir_all(&dir).unwrap();
    let body = "x".repeat(16_384); // spills

    {
        let mut cat = Catalog::create(&dir).unwrap();
        cat.create_table(Schema {
            table_name: "t".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    type_id: TypeId::Int,
                    required: true,
                    position: 0,
                },
                ColumnDef {
                    name: "v".into(),
                    type_id: TypeId::Str,
                    required: true,
                    position: 1,
                },
            ],
        })
        .unwrap();
        // An index makes recovery run `rebuild_indexes_from_heap`, which decoded
        // the replayed row — exactly where the v1-only decoder bricked on a v2
        // row (panic at row.rs:914).
        cat.create_index_unique("t", "id", true).unwrap();
        cat.insert("t", &vec![Value::Int(1), Value::Str(body.clone())])
            .unwrap();
        cat.sync_wal().unwrap();
        // Simulate a crash: drop WITHOUT a clean checkpoint so recovery must
        // replay the Insert + OverflowWrite records from the WAL.
        std::mem::forget(cat);
    }

    // Reopen → replay. Before the fix this panicked in `decode_row` (v1-only)
    // during the post-replay index rebuild and crash-looped forever.
    let cat = Catalog::open(&dir).expect("reopen/replay must not brick");
    let row = cat.get_table("t").unwrap().get(row0(&cat)).unwrap();
    assert_eq!(row[0], Value::Int(1));
    assert_eq!(
        match &row[1] {
            Value::Str(s) => s.len(),
            _ => 0,
        },
        16_384,
        "replayed spilled value must be byte-exact"
    );
    drop(cat);

    // Double reopen (double replay) must be idempotent and still intact.
    let cat = Catalog::open(&dir).expect("second reopen");
    let row = cat.get_table("t").unwrap().get(row0(&cat)).unwrap();
    assert_eq!(
        match &row[1] {
            Value::Str(s) => s.len(),
            _ => 0,
        },
        16_384
    );
    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

/// First live rid of table "t" (single-row helper).
fn row0(cat: &Catalog) -> powdb_storage::types::RowId {
    cat.get_table("t")
        .unwrap()
        .scan()
        .map(|r| r.unwrap())
        .next()
        .unwrap()
        .0
}

// ── P2: no dangling index entry after deleting a row with a spilled sibling ─

#[test]
fn p2_delete_with_spilled_sibling_keeps_index_consistent() {
    let dir = temp_dir("sibling");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(isb_schema()).unwrap();
    // Index the small `s` column; `b` is the large sibling that spills.
    cat.create_index_unique("docs", "s", true).unwrap();

    let big = "b".repeat(8192);
    cat.insert(
        "docs",
        &vec![
            Value::Int(1),
            Value::Str("key1".into()),
            Value::Str(big.clone()),
        ],
    )
    .unwrap();
    cat.sync_wal().unwrap();

    // The indexed `s` stayed inline (plan_spill keeps indexed cols inline), so
    // deleting the row must remove its `s` btree entry even though the row is v2.
    let rid = cat
        .get_table("docs")
        .unwrap()
        .scan()
        .map(|r| r.unwrap())
        .next()
        .unwrap()
        .0;
    cat.get_table_mut("docs").unwrap().delete(rid).unwrap();

    // No dangling entry: lookup by the old key finds nothing.
    assert!(
        cat.get_table("docs")
            .unwrap()
            .index_lookup_all("s", &Value::Str("key1".into()))
            .is_empty(),
        "index entry for the deleted row must be gone"
    );
    // And re-inserting the same key succeeds (a dangling unique entry would
    // wrongly trip the unique constraint).
    cat.insert(
        "docs",
        &vec![
            Value::Int(2),
            Value::Str("key1".into()),
            Value::Str("small".into()),
        ],
    )
    .expect("re-insert of the same unique key must succeed");
    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn p2_create_index_after_spill_then_delete_reassembles_keys() {
    let dir = temp_dir("afterspill");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(isb_schema()).unwrap();

    // Insert first with a LARGE `s` so `s` itself spills (no index yet).
    let big_s = "s".repeat(8192);
    cat.insert(
        "docs",
        &vec![
            Value::Int(1),
            Value::Str(big_s.clone()),
            Value::Str("b".into()),
        ],
    )
    .unwrap();
    cat.sync_wal().unwrap();

    // Now index `s`. The build reassembles the spilled value → correct key.
    cat.create_index("docs", "s").unwrap();
    assert_eq!(
        cat.get_table("docs")
            .unwrap()
            .index_lookup_all("s", &Value::Str(big_s.clone()))
            .len(),
        1,
        "create-index-after-spill must build the key from the reassembled value"
    );

    // Deleting the row must reassemble the spilled indexed value to remove the
    // key (a v1-only decode would extract Empty and leave it dangling).
    let rid = cat
        .get_table("docs")
        .unwrap()
        .scan()
        .map(|r| r.unwrap())
        .next()
        .unwrap()
        .0;
    cat.get_table_mut("docs").unwrap().delete(rid).unwrap();
    assert!(
        cat.get_table("docs")
            .unwrap()
            .index_lookup_all("s", &Value::Str(big_s))
            .is_empty(),
        "spilled indexed key must be removed on delete, not left dangling"
    );
    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

// ── sweep: orphaned overflow chain pages are reclaimed (> 0), live data kept ─

#[test]
fn sweep_reclaims_orphaned_overflow_pages_after_delete() {
    let dir = temp_dir("sweep");
    std::fs::create_dir_all(&dir).unwrap();
    let schema = Schema {
        table_name: "t".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            },
            ColumnDef {
                name: "v".into(),
                type_id: TypeId::Str,
                required: true,
                position: 1,
            },
        ],
    };
    let victim_rid;
    {
        let mut cat = Catalog::create(&dir).unwrap();
        cat.create_table(schema).unwrap();
        // A committed spilled row whose chain must stay referenced.
        cat.insert("t", &vec![Value::Int(1), Value::Str("k".repeat(20_000))])
            .unwrap();
        // A second committed spilled row we will delete → its chain orphans.
        victim_rid = cat
            .insert("t", &vec![Value::Int(2), Value::Str("o".repeat(60_000))])
            .unwrap();
        cat.sync_wal().unwrap();
    }

    // Reopen so both chains are physically on disk (replayed from the WAL).
    let mut cat = Catalog::open(&dir).unwrap();

    // Delete the victim: `heap.delete` clears the stub-row slot but never frees
    // the overflow chain, so its pages become orphaned on disk.
    cat.get_table_mut("t").unwrap().delete(victim_rid).unwrap();

    // Sweep must reclaim the orphaned chain pages (> 0). This is the primitive
    // whose reclaim path was never exercised before.
    let reclaimed = cat.sweep("t").expect("sweep");
    assert!(
        reclaimed > 0,
        "sweep must reclaim the deleted row's orphaned chain pages (got {reclaimed})"
    );

    // The surviving committed row is untouched and reads back byte-exact.
    let keep_rid = cat
        .get_table("t")
        .unwrap()
        .scan()
        .map(|r| r.unwrap())
        .next()
        .unwrap()
        .0;
    let row = cat.get_table("t").unwrap().get(keep_rid).unwrap();
    assert_eq!(row[0], Value::Int(1));
    assert_eq!(
        match &row[1] {
            Value::Str(s) => s.len(),
            _ => 0,
        },
        20_000,
        "live data must survive sweep"
    );
    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}
