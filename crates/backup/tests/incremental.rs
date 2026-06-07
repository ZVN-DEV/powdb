use powdb_backup::{ChangedFile, IncrementManifest};
use powdb_storage::catalog::Catalog;
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};

fn tmp(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    // pid + an atomic counter guarantee uniqueness across parallel test
    // threads even when the system clock resolution is too coarse to
    // distinguish two calls (macOS CI collided on nanos alone).
    let uniq = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "powdb_inc_{tag}_{}_{}_{}",
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

fn schema_t() -> Schema {
    Schema {
        table_name: "T".into(),
        columns: vec![ColumnDef {
            name: "id".into(),
            type_id: TypeId::Int,
            required: true,
            position: 0,
        }],
    }
}

/// Total number of page records across all `Pages` entries in an increment.
fn delta_page_count(inc: &IncrementManifest) -> usize {
    inc.changed
        .iter()
        .map(|c| match c {
            ChangedFile::Pages { page_indices, .. } => page_indices.len(),
            ChangedFile::Whole { .. } => 0,
        })
        .sum()
}

#[test]
fn incremental_only_stores_changed_pages() {
    let src = tmp("src");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    // Enough rows that the heap spans many 4KB pages.
    for i in 0..5000 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();

    let base_dir = tmp("base");
    let base = powdb_backup::full_backup(&mut cat, &base_dir).unwrap();
    let base_heap_len = std::fs::read(base_dir.join("T.heap")).unwrap().len();
    let base_total_pages = base_heap_len / 4096;

    // A few more rows -> touches only the tail page(s).
    for i in 5000..5005 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();

    let inc_dir = tmp("inc");
    let inc = powdb_backup::incremental_backup(&mut cat, &base, &inc_dir).unwrap();

    assert_eq!(inc.base_source_lsn, base.source_lsn);
    assert!(
        inc.source_lsn > base.source_lsn,
        "increment must advance the high-water LSN"
    );

    let changed_pages = delta_page_count(&inc);
    assert!(changed_pages >= 1, "the appended rows must change a page");
    assert!(
        changed_pages < base_total_pages,
        "delta pages ({changed_pages}) must be far fewer than total pages ({base_total_pages})"
    );
    // Concretely: only a handful of pages out of the whole heap.
    assert!(
        changed_pages <= 3,
        "appending 5 rows should touch <=3 pages, got {changed_pages}"
    );
    assert!(
        base_total_pages >= 10,
        "test premise: 5000 rows should span many pages, got {base_total_pages}"
    );
}

#[test]
fn restore_chain_reproduces_full_state() {
    let src = tmp("src");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    // State A
    for i in 0..200 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();

    let base_dir = tmp("base");
    let base = powdb_backup::full_backup(&mut cat, &base_dir).unwrap();

    // State B
    for i in 200..360 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();

    let inc_dir = tmp("inc");
    powdb_backup::incremental_backup(&mut cat, &base, &inc_dir).unwrap();

    // Cross-check: a fresh full backup at state B has 360 rows.
    let full_b_dir = tmp("fullb");
    powdb_backup::full_backup(&mut cat, &full_b_dir).unwrap();
    drop(cat);
    let cat_b = Catalog::open(&{
        let d = tmp("fullb_restored");
        powdb_backup::restore(&full_b_dir, &d).unwrap();
        d
    })
    .unwrap();
    assert_eq!(cat_b.scan("T").unwrap().count(), 360);
    drop(cat_b);

    // Chain restore should reproduce the same 360 rows.
    let restored = tmp("restored");
    powdb_backup::restore_chain(&base_dir, &[&inc_dir], &restored).unwrap();
    let cat2 = Catalog::open(&restored).unwrap();
    assert_eq!(
        cat2.scan("T").unwrap().count(),
        360,
        "chain restore must reproduce state B's row count"
    );
    // Spot-check specific rows present.
    let ids: std::collections::HashSet<i64> = cat2
        .scan("T")
        .unwrap()
        .map(|(_rid, row)| match &row[0] {
            Value::Int(n) => *n,
            other => panic!("unexpected value {other:?}"),
        })
        .collect();
    for probe in [0i64, 1, 199, 200, 359] {
        assert!(ids.contains(&probe), "row id {probe} must be present");
    }
}

#[test]
fn restore_chain_rejects_broken_chain() {
    let src = tmp("src");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    for i in 0..100 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();

    let base_dir = tmp("base");
    let base = powdb_backup::full_backup(&mut cat, &base_dir).unwrap();

    // incA builds on the full base.
    for i in 100..150 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();
    let inc_a_dir = tmp("incA");
    let inc_a = powdb_backup::incremental_backup(&mut cat, &base, &inc_a_dir).unwrap();

    // incB builds on incA (its base_source_lsn == incA.source_lsn).
    for i in 150..200 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();
    // Construct a BackupManifest-like base for incB from incA's high-water LSN.
    let base_for_b = powdb_backup::BackupManifest {
        format_version: powdb_backup::BackupManifest::FORMAT_VERSION,
        created_unix_secs: 0,
        source_lsn: inc_a.source_lsn,
        files: base.files.clone(),
    };
    let inc_b_dir = tmp("incB");
    let inc_b = powdb_backup::incremental_backup(&mut cat, &base_for_b, &inc_b_dir).unwrap();
    assert_eq!(inc_b.base_source_lsn, inc_a.source_lsn);
    drop(cat);

    // Applying incB directly on the full base must fail: incB expects incA's lsn.
    let restored = tmp("restored");
    let err = powdb_backup::restore_chain(&base_dir, &[&inc_b_dir], &restored).unwrap_err();
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("chain") && msg.contains("broken"),
        "broken chain must be rejected, got: {err}"
    );
}

#[test]
fn incremental_handles_catalog_change() {
    let src = tmp("src");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    for i in 0..50 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();

    let base_dir = tmp("base");
    let base = powdb_backup::full_backup(&mut cat, &base_dir).unwrap();

    // DDL: add a second table -> catalog.bin changes.
    let schema_u = Schema {
        table_name: "U".into(),
        columns: vec![ColumnDef {
            name: "id".into(),
            type_id: TypeId::Int,
            required: true,
            position: 0,
        }],
    };
    cat.create_table(schema_u).unwrap();
    for i in 0..20 {
        cat.insert("U", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();

    let inc_dir = tmp("inc");
    let inc = powdb_backup::incremental_backup(&mut cat, &base, &inc_dir).unwrap();
    drop(cat);

    assert!(
        inc.changed.iter().any(|c| matches!(
            c,
            ChangedFile::Whole { name, .. } if name == "catalog.bin"
        )),
        "catalog.bin must be recorded as Whole after DDL"
    );

    let restored = tmp("restored");
    powdb_backup::restore_chain(&base_dir, &[&inc_dir], &restored).unwrap();
    let cat2 = Catalog::open(&restored).unwrap();
    assert_eq!(cat2.scan("T").unwrap().count(), 50, "T must restore");
    assert_eq!(
        cat2.scan("U").unwrap().count(),
        20,
        "new table U must restore"
    );
}

#[test]
fn restore_chain_refuses_nonempty_dest() {
    let src = tmp("src");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    for i in 0..30 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();

    let base_dir = tmp("base");
    let base = powdb_backup::full_backup(&mut cat, &base_dir).unwrap();
    for i in 30..40 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();
    let inc_dir = tmp("inc");
    powdb_backup::incremental_backup(&mut cat, &base, &inc_dir).unwrap();
    drop(cat);

    let dirty = tmp("dirty");
    std::fs::create_dir_all(&dirty).unwrap();
    std::fs::write(dirty.join("wal.log"), b"stale").unwrap();
    let err = powdb_backup::restore_chain(&base_dir, &[&inc_dir], &dirty).unwrap_err();
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("empty") || msg.contains("not empty"),
        "non-empty dest must be refused, got: {err}"
    );
}
