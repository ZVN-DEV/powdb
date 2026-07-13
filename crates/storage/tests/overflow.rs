//! Overflow-page (P-2) end-to-end tests: big-row round trips across the spill
//! boundary, the per-value cap, multi-spill rows, chain reuse, and crash
//! recovery of an out-of-line value. Exercises the full write path
//! (`Catalog::insert` spill orchestration + WAL OverflowWrite ordering) and
//! the read path (`Table::get` transparent chain reassembly).

use powdb_storage::catalog::Catalog;
use powdb_storage::types::{ColumnDef, RowId, Schema, TypeId, Value};

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_overflow_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn docs_schema() -> Schema {
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
                name: "body".into(),
                type_id: TypeId::Str,
                required: true,
                position: 1,
            },
        ],
    }
}

fn body_of(row: &[Value]) -> &str {
    match &row[1] {
        Value::Str(s) => s,
        other => panic!("expected Str body, got {other:?}"),
    }
}

/// Round-trip a value of each size straddling the 4070B inline cap plus large
/// multi-page chains. Every one must read back byte-for-byte.
#[test]
fn test_roundtrip_across_spill_boundary() {
    let dir = temp_dir("roundtrip");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(docs_schema()).unwrap();

    // 4069/4070 fit inline; 4071 and up spill (accounting for row overhead the
    // 4070-char body already spills, which is fine — the point is exact
    // reassembly on both sides of the boundary).
    let sizes = [1usize, 4069, 4070, 4071, 8192, 16_384, 1_000_000];
    let mut rids: Vec<(usize, RowId)> = Vec::new();
    for (i, &n) in sizes.iter().enumerate() {
        let body = "x".repeat(n);
        let rid = cat
            .insert("docs", &vec![Value::Int(i as i64), Value::Str(body)])
            .unwrap();
        rids.push((n, rid));
    }
    cat.sync_wal().unwrap();

    for (n, rid) in &rids {
        let row = cat.get("docs", *rid).expect("row present");
        assert_eq!(body_of(&row).len(), *n, "length mismatch for size {n}");
        assert!(body_of(&row).bytes().all(|b| b == b'x'), "content mismatch");
    }
    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

/// A Bytes value and a Str value both spilled in the same row reassemble
/// independently.
#[test]
fn test_multi_spill_row() {
    let dir = temp_dir("multi_spill");
    std::fs::create_dir_all(&dir).unwrap();
    let schema = Schema {
        table_name: "t".into(),
        columns: vec![
            ColumnDef {
                name: "a".into(),
                type_id: TypeId::Str,
                required: true,
                position: 0,
            },
            ColumnDef {
                name: "b".into(),
                type_id: TypeId::Bytes,
                required: true,
                position: 1,
            },
        ],
    };
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(schema).unwrap();

    let a = "A".repeat(5000);
    let b = vec![0x5Au8; 6000];
    let rid = cat
        .insert("t", &vec![Value::Str(a.clone()), Value::Bytes(b.clone())])
        .unwrap();
    cat.sync_wal().unwrap();

    let row = cat.get("t", rid).unwrap();
    assert_eq!(row[0], Value::Str(a));
    assert_eq!(row[1], Value::Bytes(b));
    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

/// A committed out-of-line value survives a simulated crash (mem::forget the
/// catalog so no heap flush) via WAL OverflowWrite + Insert replay.
#[test]
fn test_crash_recovery_of_spilled_value() {
    let dir = temp_dir("crash_spill");
    std::fs::create_dir_all(&dir).unwrap();
    let big = "Z".repeat(50_000);

    {
        let mut cat = Catalog::create(&dir).unwrap();
        cat.create_table(docs_schema()).unwrap();
        cat.insert("docs", &vec![Value::Int(1), Value::Str(big.clone())])
            .unwrap();
        cat.sync_wal().unwrap();
        std::mem::forget(cat); // crash: skip all Drops (no heap flush)
    }

    {
        let cat = Catalog::open(&dir).unwrap();
        let rows: Vec<_> = cat.scan("docs").unwrap().collect();
        assert_eq!(rows.len(), 1, "row must survive replay");
        assert_eq!(body_of(&rows[0].1), big.as_str());
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// Update transitions: inline -> spilled, spilled -> spilled (grow/shrink),
/// and spilled -> inline all read back correctly.
#[test]
fn test_update_transitions() {
    let dir = temp_dir("update_transitions");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(docs_schema()).unwrap();

    // Start inline.
    let rid = cat
        .insert("docs", &vec![Value::Int(1), Value::Str("tiny".into())])
        .unwrap();
    cat.sync_wal().unwrap();
    let mut rid = rid;

    // inline -> spilled.
    rid = cat
        .update(
            "docs",
            rid,
            &vec![Value::Int(1), Value::Str("a".repeat(20_000))],
        )
        .unwrap();
    cat.sync_wal().unwrap();
    assert_eq!(
        cat.get("docs", rid).map(|r| body_of(&r).len()),
        Some(20_000)
    );

    // spilled -> spilled grow.
    rid = cat
        .update(
            "docs",
            rid,
            &vec![Value::Int(1), Value::Str("b".repeat(60_000))],
        )
        .unwrap();
    cat.sync_wal().unwrap();
    let row = cat.get("docs", rid).unwrap();
    assert_eq!(body_of(&row).len(), 60_000);
    assert!(body_of(&row).bytes().all(|c| c == b'b'));

    // spilled -> spilled shrink.
    rid = cat
        .update(
            "docs",
            rid,
            &vec![Value::Int(1), Value::Str("c".repeat(9_000))],
        )
        .unwrap();
    cat.sync_wal().unwrap();
    assert_eq!(cat.get("docs", rid).map(|r| body_of(&r).len()), Some(9_000));

    // spilled -> inline.
    rid = cat
        .update(
            "docs",
            rid,
            &vec![Value::Int(1), Value::Str("small-again".into())],
        )
        .unwrap();
    cat.sync_wal().unwrap();
    let row = cat.get("docs", rid).unwrap();
    assert_eq!(body_of(&row), "small-again");

    // Survives a reopen (clean shutdown checkpoints the heap).
    drop(cat);
    let cat = Catalog::open(&dir).unwrap();
    let rows: Vec<_> = cat.scan("docs").unwrap().collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(body_of(&rows[0].1), "small-again");
    std::fs::remove_dir_all(&dir).ok();
}

/// A value above the 64MB engine limit is a clean typed error, not a panic,
/// and the table stays usable.
#[test]
fn test_value_size_cap_rejected() {
    let dir = temp_dir("value_cap");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(docs_schema()).unwrap();

    let too_big = "x".repeat(64 * 1024 * 1024 + 1);
    let err = cat
        .insert("docs", &vec![Value::Int(1), Value::Str(too_big)])
        .unwrap_err();
    assert!(
        err.to_string().contains("value too large"),
        "unexpected error: {err}"
    );

    // Table remains usable after the rejected oversized insert.
    let rid = cat
        .insert("docs", &vec![Value::Int(2), Value::Str("ok".into())])
        .unwrap();
    cat.sync_wal().unwrap();
    assert_eq!(
        cat.get("docs", rid).map(|r| body_of(&r).to_string()),
        Some("ok".into())
    );
    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

/// Sweep reclaims a genuinely orphaned chain (the kind a crashed transaction
/// leaves: pages flushed to disk that no committed row references), does not
/// touch a live row's chains, is idempotent, and the reclaimed pages are reused
/// by a subsequent spill.
///
/// NOTE: a committed `delete` now frees its chain eagerly at commit (design 3.6,
/// see `test_delete_frees_chain_eagerly`), so it leaves NO orphan for sweep.
/// Sweep's real job is crash/rollback orphans, which this test synthesizes via
/// the low-level overflow-page API (the same shape a crash-flushed chain has).
#[test]
fn test_sweep_reclaims_orphan_chains() {
    use powdb_storage::page::{OVERFLOW_CHAIN_END, OVERFLOW_PAYLOAD_CAP};
    let dir = temp_dir("sweep");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(docs_schema()).unwrap();

    // A live big row whose chain must NEVER be swept.
    let keep = cat
        .insert("docs", &vec![Value::Int(1), Value::Str("K".repeat(40_000))])
        .unwrap();
    cat.sync_wal().unwrap();

    // Synthesize an orphan chain: allocate + write two overflow pages that no
    // row's stub references (exactly what a crashed spill leaves behind).
    {
        let tbl = cat.get_table_mut("docs").unwrap();
        let p0 = tbl.heap.allocate_overflow_page().unwrap();
        let p1 = tbl.heap.allocate_overflow_page().unwrap();
        let chunk = vec![b'Z'; OVERFLOW_PAYLOAD_CAP];
        tbl.heap.write_overflow_page(p0, p1, &chunk, 0).unwrap();
        tbl.heap
            .write_overflow_page(p1, OVERFLOW_CHAIN_END, &chunk, 0)
            .unwrap();
    }

    let reclaimed = cat.sweep("docs").unwrap();
    assert_eq!(reclaimed, 2, "sweep must reclaim exactly the orphan chain");

    // The live row's value is intact (its chain was NOT swept).
    let row = cat.get("docs", keep).unwrap();
    assert_eq!(body_of(&row).len(), 40_000);
    assert!(body_of(&row).bytes().all(|b| b == b'K'));

    // Sweep is idempotent: a second pass reclaims nothing.
    assert_eq!(cat.sweep("docs").unwrap(), 0);

    // Reuse: the reclaimed pages are on the free list, so a fresh spill of the
    // same size does not extend the file.
    let before = cat.get_table("docs").unwrap().heap.num_pages();
    cat.insert("docs", &vec![Value::Int(3), Value::Str("R".repeat(8_000))])
        .unwrap();
    cat.sync_wal().unwrap();
    let after = cat.get_table("docs").unwrap().heap.num_pages();
    assert_eq!(
        after, before,
        "reclaimed pages must be reused, not re-grown"
    );

    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

/// A committed delete of a spilled row frees its overflow chain at commit
/// (design 3.6): the pages return to the free list and are reused by the next
/// spill, so a delete/insert churn does not leak, and no orphan is left for
/// sweep. The surviving rows stay byte-exact throughout.
#[test]
fn test_delete_frees_chain_eagerly() {
    let dir = temp_dir("delete_frees");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(docs_schema()).unwrap();

    let keep = cat
        .insert("docs", &vec![Value::Int(1), Value::Str("K".repeat(40_000))])
        .unwrap();
    let drop_rid = cat
        .insert("docs", &vec![Value::Int(2), Value::Str("D".repeat(40_000))])
        .unwrap();
    cat.sync_wal().unwrap();
    let baseline = cat.get_table("docs").unwrap().heap.num_pages();

    // Delete the second row: its chain is freed immediately.
    cat.delete("docs", drop_rid).unwrap();
    cat.sync_wal().unwrap();

    // Nothing to reclaim -- the delete already freed the chain.
    assert_eq!(
        cat.sweep("docs").unwrap(),
        0,
        "committed delete frees eagerly, leaving no orphan"
    );

    // A fresh 40K insert reuses the freed pages instead of extending the file.
    cat.insert("docs", &vec![Value::Int(3), Value::Str("N".repeat(40_000))])
        .unwrap();
    cat.sync_wal().unwrap();
    let after = cat.get_table("docs").unwrap().heap.num_pages();
    assert!(
        after <= baseline,
        "freed pages must be reused (after={after}, baseline={baseline})"
    );

    // The untouched row is byte-exact.
    let row = cat.get("docs", keep).unwrap();
    assert_eq!(body_of(&row).len(), 40_000);
    assert!(body_of(&row).bytes().all(|b| b == b'K'));

    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

/// An uncommitted big row (explicit tx rolled back) must leave no live row and
/// its chains must not corrupt subsequent reads.
#[test]
fn test_rollback_discards_spilled_row() {
    let dir = temp_dir("rollback_spill");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(docs_schema()).unwrap();

    // A committed small row first.
    cat.insert("docs", &vec![Value::Int(1), Value::Str("small".into())])
        .unwrap();
    cat.sync_wal().unwrap();

    cat.begin_transaction().unwrap();
    cat.insert("docs", &vec![Value::Int(2), Value::Str("Q".repeat(40_000))])
        .unwrap();
    cat.sync_wal().unwrap();
    cat.rollback_to_last_sync().unwrap();

    let rows: Vec<_> = cat.scan("docs").unwrap().collect();
    assert_eq!(rows.len(), 1, "rolled-back big row must be gone");
    assert_eq!(body_of(&rows[0].1), "small");
    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}
