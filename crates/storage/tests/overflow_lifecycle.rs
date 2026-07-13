//! Overflow lifecycle regressions (v0.11 systematic-fix lane): the defect class
//! where a v2 (spilled) row is relocated without repointing its index, or its
//! old chain is never freed, or ALTER/index-rebuild reads a spilled column as
//! `Empty`. Every test here fails on the pre-fix engine.

use powdb_storage::catalog::Catalog;
use powdb_storage::types::{ColumnDef, RowId, Schema, TypeId, Value};

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_ovflife_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// `type T { unique auto id: int, v: str }`
fn t_schema() -> Schema {
    Schema {
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
    }
}

fn str_len(row: &[Value]) -> usize {
    match &row[1] {
        Value::Str(s) => s.len(),
        other => panic!("expected Str, got {other:?}"),
    }
}

/// First rid whose indexed `column` equals `key` (via the Table-level index).
fn lookup_rid(cat: &Catalog, table: &str, column: &str, key: &Value) -> Option<RowId> {
    cat.get_table(table)?
        .index_lookup_all(column, key)
        .first()
        .copied()
}

fn lookup_all(cat: &Catalog, table: &str, column: &str, key: &Value) -> Vec<RowId> {
    cat.get_table(table)
        .map(|t| t.index_lookup_all(column, key))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// P0: inline -> spilled -> inline update must keep the row reachable by index.
// ---------------------------------------------------------------------------

/// The exact bug-hunt repro: insert inline, update to spill (relocates), update
/// back inline (relocates again). After BOTH transitions every keyed access
/// must still find the row, and it must survive a restart. Pre-fix: the point
/// lookup, filtered update, and filtered delete all miss the relocated row
/// because the unique index still points at the stale rid.
#[test]
fn p0_inline_spill_inline_keeps_unique_index() {
    let dir = temp_dir("p0_unique");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(t_schema()).unwrap();
    cat.create_index_unique("t", "id", true).unwrap();

    let rid = cat
        .insert("t", &vec![Value::Int(1), Value::Str("a".repeat(4000))])
        .unwrap();
    cat.sync_wal().unwrap();

    // The executor hands `update_hinted` the CHANGED columns only ([v]); `id`
    // is unchanged, so the pre-fix relocation skipped index maintenance.
    cat.update_hinted(
        "t",
        rid,
        &vec![Value::Int(1), Value::Str("b".repeat(4200))],
        Some(&[1]),
    )
    .unwrap();
    cat.sync_wal().unwrap();
    cat.update_hinted(
        "t",
        rid,
        &vec![Value::Int(1), Value::Str("c".repeat(4000))],
        Some(&[1]),
    )
    .unwrap();
    cat.sync_wal().unwrap();

    // 1. Point lookup by the indexed key finds the (relocated) row.
    let looked = cat.index_lookup("t", "id", &Value::Int(1)).unwrap();
    assert!(looked.is_some(), "point lookup .id=1 must find the row");
    assert_eq!(str_len(&looked.unwrap()), 4000);

    // 2. Unfiltered scan finds exactly one row.
    assert_eq!(cat.scan("t").unwrap().count(), 1);

    // Restart: the index rebuild + persisted rows must agree.
    cat.checkpoint().unwrap();
    drop(cat);
    let mut cat = Catalog::open(&dir).unwrap();
    let looked = cat.index_lookup("t", "id", &Value::Int(1)).unwrap();
    assert!(looked.is_some(), "point lookup must survive restart");
    assert_eq!(str_len(&looked.unwrap()), 4000);

    // 3. A keyed update reaches the row (returns the pre-existing rid, not a miss).
    let rid_now = lookup_rid(&cat, "t", "id", &Value::Int(1)).unwrap();
    cat.update_hinted(
        "t",
        rid_now,
        &vec![Value::Int(1), Value::Str("d".repeat(10))],
        Some(&[1]),
    )
    .unwrap();
    cat.sync_wal().unwrap();
    assert_eq!(
        str_len(
            &cat.index_lookup("t", "id", &Value::Int(1))
                .unwrap()
                .unwrap()
        ),
        10
    );

    // 4. A keyed delete removes it.
    let rid_now = lookup_rid(&cat, "t", "id", &Value::Int(1)).unwrap();
    cat.delete("t", rid_now).unwrap();
    cat.sync_wal().unwrap();
    assert!(cat
        .index_lookup("t", "id", &Value::Int(1))
        .unwrap()
        .is_none());
    assert_eq!(cat.scan("t").unwrap().count(), 0);

    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

/// Same transition with a NON-unique (composite-key) secondary index: the
/// composite entry (value, rid) must move to the new rid or `lookup_prefix`
/// returns the stale rid and reads garbage / nothing.
#[test]
fn p0_inline_spill_inline_keeps_nonunique_index() {
    let dir = temp_dir("p0_nonuniq");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(t_schema()).unwrap();
    cat.create_index("t", "id").unwrap(); // non-unique

    let rid = cat
        .insert("t", &vec![Value::Int(7), Value::Str("a".repeat(4000))])
        .unwrap();
    cat.sync_wal().unwrap();
    cat.update_hinted(
        "t",
        rid,
        &vec![Value::Int(7), Value::Str("b".repeat(4200))],
        Some(&[1]),
    )
    .unwrap();
    cat.update_hinted(
        "t",
        rid,
        &vec![Value::Int(7), Value::Str("c".repeat(4000))],
        Some(&[1]),
    )
    .unwrap();
    cat.sync_wal().unwrap();

    let hits = lookup_all(&cat, "t", "id", &Value::Int(7));
    assert_eq!(
        hits.len(),
        1,
        "non-unique index must find the relocated row"
    );
    let row = cat.get("t", hits[0]).expect("indexed rid must be live");
    assert_eq!(str_len(&row), 4000);

    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

/// Multi-index table: `id` unique + `v` non-unique. A relocation with neither
/// indexed column in the changed-set must repoint BOTH indexes.
#[test]
fn p0_relocation_repoints_every_index() {
    let dir = temp_dir("p0_multi");
    std::fs::create_dir_all(&dir).unwrap();
    let schema = Schema {
        table_name: "m".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            },
            ColumnDef {
                name: "tag".into(),
                type_id: TypeId::Int,
                required: false,
                position: 1,
            },
            ColumnDef {
                name: "v".into(),
                type_id: TypeId::Str,
                required: true,
                position: 2,
            },
        ],
    };
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(schema).unwrap();
    cat.create_index_unique("m", "id", true).unwrap();
    cat.create_index("m", "tag").unwrap();

    let rid = cat
        .insert(
            "m",
            &vec![Value::Int(1), Value::Int(99), Value::Str("a".repeat(4000))],
        )
        .unwrap();
    cat.sync_wal().unwrap();
    // Only `v` (col 2) changes -> neither indexed column is in the changed-set.
    cat.update_hinted(
        "m",
        rid,
        &vec![Value::Int(1), Value::Int(99), Value::Str("b".repeat(4200))],
        Some(&[2]),
    )
    .unwrap();
    cat.update_hinted(
        "m",
        rid,
        &vec![Value::Int(1), Value::Int(99), Value::Str("c".repeat(4000))],
        Some(&[2]),
    )
    .unwrap();
    cat.sync_wal().unwrap();

    assert!(
        cat.index_lookup("m", "id", &Value::Int(1))
            .unwrap()
            .is_some(),
        "id index"
    );
    assert_eq!(
        lookup_all(&cat, "m", "tag", &Value::Int(99)).len(),
        1,
        "tag index"
    );

    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// P1: chains freed on churn; rollback safety.
// ---------------------------------------------------------------------------

/// Churn-update one spilled row N times. With eager commit-time chain freeing
/// the heap page count is BOUNDED (each update reuses the previous chain's
/// pages); pre-fix it grew ~linearly in N. The live value stays byte-exact.
#[test]
fn p1_churn_update_is_bounded() {
    let dir = temp_dir("p1_churn");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(t_schema()).unwrap();

    let rid = cat
        .insert("t", &vec![Value::Int(1), Value::Str("a".repeat(20_000))])
        .unwrap();
    cat.sync_wal().unwrap();
    let after_first = cat.get_table("t").unwrap().heap.num_pages();

    for i in 0..60u8 {
        let fill = (b'a' + (i % 26)) as char;
        cat.update_hinted(
            "t",
            rid,
            &vec![Value::Int(1), Value::Str(fill.to_string().repeat(20_000))],
            Some(&[1]),
        )
        .unwrap();
        cat.sync_wal().unwrap();
    }
    let after_churn = cat.get_table("t").unwrap().heap.num_pages();

    // 20K spills to ~5 overflow pages. Bounded means "within a couple of chains
    // of the single-row baseline", NOT linear in 60. Pre-fix this was ~after
    // + 60*5.
    assert!(
        after_churn <= after_first + 12,
        "churn must be bounded: after_first={after_first}, after_churn={after_churn}"
    );

    // Live value byte-exact.
    let row = cat.get("t", rid).unwrap();
    assert_eq!(str_len(&row), 20_000);
    let expected = (b'a' + (59 % 26)) as char;
    assert!(matches!(&row[1], Value::Str(s) if s.chars().all(|c| c == expected)));

    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

/// A spilling update inside an explicit transaction that is rolled back must
/// leave the ORIGINAL value byte-exact (its old chain was deferred, never
/// freed) and must not leak: the new (rolled-back) chain is reclaimed on the
/// rollback reopen, so a subsequent spill does not grow the file unbounded.
#[test]
fn p1_rollback_of_spilling_update_is_safe() {
    let dir = temp_dir("p1_rollback");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(t_schema()).unwrap();

    let rid = cat
        .insert("t", &vec![Value::Int(1), Value::Str("O".repeat(30_000))])
        .unwrap();
    cat.sync_wal().unwrap();
    cat.checkpoint().unwrap();
    let baseline_pages = cat.get_table("t").unwrap().heap.num_pages();

    cat.begin_transaction().unwrap();
    cat.update_hinted(
        "t",
        rid,
        &vec![Value::Int(1), Value::Str("N".repeat(40_000))],
        Some(&[1]),
    )
    .unwrap();
    cat.sync_wal().unwrap();
    cat.rollback_to_last_sync().unwrap();

    // Original value survives byte-exact.
    let row = cat.get("t", rid).expect("row must survive rollback");
    assert_eq!(str_len(&row), 30_000);
    assert!(matches!(&row[1], Value::Str(s) if s.chars().all(|c| c == 'O')));

    // The rolled-back new chain must have been reclaimed (auto-sweep on reopen),
    // so pages did not run away.
    let after_rollback = cat.get_table("t").unwrap().heap.num_pages();
    assert!(
        after_rollback <= baseline_pages + 12,
        "rolled-back chain must not leak: baseline={baseline_pages}, after={after_rollback}"
    );

    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// ALTER TABLE on a spilled table must preserve the out-of-line value.
// ---------------------------------------------------------------------------

/// Spill a value, then ALTER ADD COLUMN and ALTER DROP COLUMN. The surviving
/// large value must round-trip byte-exact and no chain may leak (a follow-up
/// sweep reclaims 0 because the ALTER freed the old chains). Pre-fix the ALTER
/// re-encoded the row via a v1 decode, reading the spilled column as NULL.
#[test]
fn alter_table_preserves_spilled_value() {
    let dir = temp_dir("alter");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(t_schema()).unwrap();

    let rid = cat
        .insert("t", &vec![Value::Int(1), Value::Str("S".repeat(50_000))])
        .unwrap();
    cat.sync_wal().unwrap();

    cat.alter_table_add_column(
        "t",
        ColumnDef {
            name: "extra".into(),
            type_id: TypeId::Int,
            required: false,
            position: 2,
        },
    )
    .unwrap();
    // The 50K value must survive the ADD (re-spilled under the new shape).
    let row = cat.get("t", rid).expect("row present after ADD");
    assert_eq!(str_len(&row), 50_000);
    assert!(matches!(&row[1], Value::Str(s) if s.chars().all(|c| c == 'S')));

    cat.alter_table_drop_column("t", "extra").unwrap();
    // ... and survive the DROP too, still byte-exact.
    let rows: Vec<_> = cat.scan("t").unwrap().collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(str_len(&rows[0].1), 50_000);
    assert!(matches!(&rows[0].1[1], Value::Str(s) if s.chars().all(|c| c == 'S')));

    // No chain leak: the ALTER freed the old chains, so sweep reclaims nothing.
    assert_eq!(cat.sweep("t").unwrap(), 0, "ALTER must not leak old chains");

    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

/// Force-spill an indexed column (only-way-to-fit), drop the `.idx` file to
/// trigger the open-time rebuild, and assert the spilled key is present. Pre-fix
/// the rebuild used a v1 decode and skipped the spilled key.
#[test]
fn index_rebuild_reassembles_spilled_key() {
    let dir = temp_dir("rebuild");
    std::fs::create_dir_all(&dir).unwrap();
    // Single Str column that is BOTH the only column and indexed, so spilling it
    // is the only way to fit an over-cap row.
    let schema = Schema {
        table_name: "k".into(),
        columns: vec![ColumnDef {
            name: "key".into(),
            type_id: TypeId::Str,
            required: true,
            position: 0,
        }],
    };
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(schema).unwrap();
    cat.create_index_unique("k", "key", true).unwrap();

    let big = "K".repeat(5000); // > 4070 inline cap -> must spill the key column
    cat.insert("k", &vec![Value::Str(big.clone())]).unwrap();
    cat.sync_wal().unwrap();
    cat.checkpoint().unwrap();
    drop(cat);

    // Delete the persisted index so the next open rebuilds it from the heap.
    std::fs::remove_file(dir.join("k_key.idx")).unwrap();

    let cat = Catalog::open(&dir).unwrap();
    let hit = cat
        .index_lookup("k", "key", &Value::Str(big.clone()))
        .unwrap();
    assert!(hit.is_some(), "rebuilt index must contain the spilled key");
    match &hit.unwrap()[0] {
        Value::Str(s) => assert_eq!(s.len(), 5000),
        other => panic!("expected Str, got {other:?}"),
    }

    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// Crash recovery of the (non-WAL-logged) heap v3 bump.
// ---------------------------------------------------------------------------

/// Insert a spilled row, then simulate a crash BEFORE checkpoint (drop the
/// catalog after `sync_wal` but without `checkpoint`) and reopen from disk.
/// The heap must reopen as v3 (the lazy bump on the first chain write is durable
/// via direct page writes) and the row must reassemble byte-exact. Locks the
/// fsync-flushes-all + write-order invariant the v3 bump relies on.
#[test]
fn crash_recovery_preserves_heap_v3_and_spilled_row() {
    let dir = temp_dir("crash_v3");
    std::fs::create_dir_all(&dir).unwrap();
    {
        let mut cat = Catalog::create(&dir).unwrap();
        cat.create_table(t_schema()).unwrap();
        cat.insert("t", &vec![Value::Int(1), Value::Str("C".repeat(60_000))])
            .unwrap();
        cat.sync_wal().unwrap();
        // Simulate a crash: no checkpoint, just abandon the catalog. `Drop`
        // must not be relied on for durability of the row (the WAL + direct
        // chain writes carry it).
        std::mem::forget(cat);
    }

    let cat = Catalog::open(&dir).unwrap();
    assert_eq!(
        cat.get_table("t").unwrap().heap.format_version(),
        3,
        "heap must reopen as v3 after a chain write"
    );
    let rows: Vec<(RowId, Vec<Value>)> = cat.scan("t").unwrap().collect();
    assert_eq!(rows.len(), 1, "spilled row must recover");
    assert_eq!(str_len(&rows[0].1), 60_000);
    assert!(matches!(&rows[0].1[1], Value::Str(s) if s.chars().all(|c| c == 'C')));

    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}
