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
fn incremental_backup_refuses_active_transaction_without_persisting_uncommitted_rows() {
    let src = tmp("active_tx_src");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    cat.insert("T", &vec![Value::Int(1)]).unwrap();
    cat.sync_wal().unwrap();

    let base_dir = tmp("active_tx_base");
    let base = powdb_backup::full_backup(&mut cat, &base_dir).unwrap();

    cat.begin_transaction().unwrap();
    cat.insert("T", &vec![Value::Int(2)]).unwrap();
    cat.sync_wal().unwrap();

    let inc_dir = tmp("active_tx_inc");
    let err = powdb_backup::incremental_backup(&mut cat, &base, &inc_dir).unwrap_err();
    assert!(
        err.to_string().contains("transaction is active"),
        "incremental backup must fail closed during an active transaction, got: {err}"
    );
    drop(cat);

    let cat = Catalog::open(&src).unwrap();
    assert_eq!(
        cat.scan("T").unwrap().count(),
        1,
        "failed incremental backup must not persist active transaction rows"
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
        catalog_version: inc_a.catalog_version,
        sync: inc_a.sync.clone(),
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
fn incremental_backup_and_restore_chain_reject_sync_identity_mismatch() {
    let src = tmp("syncsrc");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    for i in 0..20 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();
    powdb_sync::open_or_create_identity(&src).unwrap();

    let base_dir = tmp("syncbase");
    let base = powdb_backup::full_backup(&mut cat, &base_dir).unwrap();
    let original_identity = base
        .sync
        .clone()
        .expect("base backup should record sync metadata");

    for i in 20..30 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();

    let mut mismatched_base = base.clone();
    let mut mismatched_sync = original_identity.clone();
    mismatched_sync.identity.primary_generation += 1;
    mismatched_base.sync = Some(mismatched_sync);

    let inc_dir = tmp("badinc");
    let err = powdb_backup::incremental_backup(&mut cat, &mismatched_base, &inc_dir).unwrap_err();
    assert!(
        format!("{err}")
            .to_lowercase()
            .contains("sync identity changed"),
        "incremental backup must reject changed sync identity, got: {err}"
    );

    let good_inc_dir = tmp("goodinc");
    let mut good_inc = powdb_backup::incremental_backup(&mut cat, &base, &good_inc_dir).unwrap();
    let mut wrong_increment_sync = good_inc.sync.clone().expect("increment sync metadata");
    wrong_increment_sync.identity.primary_generation += 1;
    good_inc.sync = Some(wrong_increment_sync);
    good_inc.write(&good_inc_dir).unwrap();
    drop(cat);

    let restored = tmp("badrestore");
    let err = powdb_backup::restore_chain(&base_dir, &[&good_inc_dir], &restored).unwrap_err();
    assert!(
        format!("{err}")
            .to_lowercase()
            .contains("sync identity does not match"),
        "restore_chain must reject mismatched increment identity, got: {err}"
    );
}

#[test]
fn restore_chain_rejects_stale_sync_catalog_hash() {
    let src = tmp("synccathashsrc");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    for i in 0..20 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();
    powdb_sync::open_or_create_identity(&src).unwrap();

    let base_dir = tmp("synccathashbase");
    let base = powdb_backup::full_backup(&mut cat, &base_dir).unwrap();

    for i in 20..30 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();

    let inc_dir = tmp("synccathashinc");
    let mut inc = powdb_backup::incremental_backup(&mut cat, &base, &inc_dir).unwrap();
    drop(cat);
    inc.sync.as_mut().expect("sync metadata").catalog_blake3_hex =
        "not-the-final-catalog-hash".into();
    inc.write(&inc_dir).unwrap();

    let restored = tmp("synccathashrestored");
    let err = powdb_backup::restore_chain(&base_dir, &[&inc_dir], &restored).unwrap_err();
    assert!(
        format!("{err}").to_lowercase().contains("catalog hash"),
        "restore_chain must reject stale sync catalog hash, got: {err}"
    );
}

#[test]
fn restore_chain_strips_sync_identity_by_default_and_explicit_modes_preserve_or_fork() {
    let src = tmp("syncforksrc");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    for i in 0..20 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();
    let source_identity = powdb_sync::open_or_create_identity(&src).unwrap();

    let base_dir = tmp("syncforkbase");
    let base = powdb_backup::full_backup(&mut cat, &base_dir).unwrap();

    for i in 20..30 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();

    let inc_dir = tmp("syncforkinc");
    powdb_backup::incremental_backup(&mut cat, &base, &inc_dir).unwrap();
    drop(cat);

    let plain = tmp("syncforkplain");
    powdb_backup::restore_chain(&base_dir, &[&inc_dir], &plain).unwrap();
    let err = powdb_sync::read_identity(&plain).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    let mut plain_cat = Catalog::open(&plain).unwrap();
    assert_eq!(plain_cat.scan("T").unwrap().count(), 30);
    plain_cat.insert("T", &vec![Value::Int(30)]).unwrap();
    plain_cat.sync_wal().unwrap();
    drop(plain_cat);
    let plain_reopened = Catalog::open(&plain).unwrap();
    assert_eq!(plain_reopened.scan("T").unwrap().count(), 31);
    drop(plain_reopened);

    let preserved = tmp("syncforkpreserved");
    powdb_backup::restore_chain_with_sync_mode(
        &base_dir,
        &[&inc_dir],
        &preserved,
        powdb_backup::RestoreSyncMode::PreserveSyncIdentity,
    )
    .unwrap();
    assert_eq!(
        powdb_sync::read_identity(&preserved).unwrap(),
        source_identity
    );
    let preserved_cat = Catalog::open(&preserved).unwrap();
    assert_eq!(preserved_cat.scan("T").unwrap().count(), 30);

    let forked = tmp("syncforkrestored");
    powdb_backup::restore_chain_with_sync_mode(
        &base_dir,
        &[&inc_dir],
        &forked,
        powdb_backup::RestoreSyncMode::ForkWithNewSyncIdentity,
    )
    .unwrap();

    let forked_identity = powdb_sync::read_identity(&forked).unwrap();
    assert_ne!(
        forked_identity, source_identity,
        "forked chain restores must not reuse the source sync identity"
    );
    assert_eq!(
        forked_identity.primary_generation, 1,
        "forked chain restores start a fresh primary generation"
    );
    let forked_cat = Catalog::open(&forked).unwrap();
    assert_eq!(forked_cat.scan("T").unwrap().count(), 30);
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

#[test]
fn restore_chain_rejects_increment_path_traversal_name() {
    let src = tmp("src");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    for i in 0..100 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();

    let base_dir = tmp("base");
    let base = powdb_backup::full_backup(&mut cat, &base_dir).unwrap();
    for i in 100..120 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();

    let inc_dir = tmp("inc");
    let mut inc = powdb_backup::incremental_backup(&mut cat, &base, &inc_dir).unwrap();
    drop(cat);

    let first = inc
        .changed
        .first_mut()
        .expect("increment should record at least one changed file");
    match first {
        ChangedFile::Whole { name, .. } | ChangedFile::Pages { name, .. } => {
            *name = "../escaped.heap".into();
        }
    }
    inc.write(&inc_dir).unwrap();

    let restored = tmp("restored");
    let err = powdb_backup::restore_chain(&base_dir, &[&inc_dir], &restored).unwrap_err();
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("invalid") && msg.contains("manifest"),
        "path traversal increment name must be rejected, got: {err}"
    );
}

#[test]
fn restore_chain_rejects_delta_page_index_mismatch() {
    let src = tmp("src_delta_idx");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    for i in 0..5000 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();

    let base_dir = tmp("base_delta_idx");
    let base = powdb_backup::full_backup(&mut cat, &base_dir).unwrap();
    for i in 5000..5010 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();

    let inc_dir = tmp("inc_delta_idx");
    let mut inc = powdb_backup::incremental_backup(&mut cat, &base, &inc_dir).unwrap();
    drop(cat);

    let mut patched = false;
    for changed in &mut inc.changed {
        let ChangedFile::Pages {
            total_pages,
            page_indices,
            delta_file,
            delta_blake3_hex,
            ..
        } = changed
        else {
            continue;
        };
        let expected = *page_indices
            .first()
            .expect("paged increment should record at least one page");
        let replacement = if expected + 1 < *total_pages {
            expected + 1
        } else {
            expected.saturating_sub(1)
        };
        if replacement == expected {
            continue;
        }
        let delta_path = inc_dir.join(delta_file);
        let mut delta = std::fs::read(&delta_path).unwrap();
        delta[0..4].copy_from_slice(&replacement.to_le_bytes());
        std::fs::write(&delta_path, &delta).unwrap();
        *delta_blake3_hex = blake3::hash(&delta).to_hex().to_string();
        patched = true;
        break;
    }
    assert!(patched, "test must patch a paged delta record");
    inc.write(&inc_dir).unwrap();

    let restored = tmp("restored_delta_idx");
    let err = powdb_backup::restore_chain(&base_dir, &[&inc_dir], &restored).unwrap_err();
    assert!(
        err.to_string()
            .contains("does not match manifest page index"),
        "embedded page index mismatch must be rejected, got: {err}"
    );
}

/// Two increments taken the way the CLI takes them (both `--base` the same
/// full backup) cannot be chained, and `docs/backup-and-restore.md` now says so
/// and quotes this error.
///
/// This is the differential model working as designed, not a defect: `inc2`
/// records the *base* LSN as its starting point, so once `inc1` has moved the
/// restored database forward the continuity check must reject `inc2`. The doc
/// used to show exactly this two-increment command as the coarse-PITR recipe,
/// which no user could ever have run. The test exists so that claim stays
/// machine-checked instead of living only in prose.
#[test]
fn two_differential_increments_against_one_base_cannot_be_chained() {
    let src = tmp("difsrc");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    cat.insert("T", &vec![Value::Int(1)]).unwrap();
    cat.sync_wal().unwrap();

    let base_dir = tmp("difbase");
    let base = powdb_backup::full_backup(&mut cat, &base_dir).unwrap();

    // Both increments are diffed against the same full base, which is the only
    // thing `powdb-cli backup --base <full>` can produce.
    cat.insert("T", &vec![Value::Int(2)]).unwrap();
    cat.sync_wal().unwrap();
    let inc1_dir = tmp("difinc1");
    powdb_backup::incremental_backup(&mut cat, &base, &inc1_dir).unwrap();

    cat.insert("T", &vec![Value::Int(3)]).unwrap();
    cat.sync_wal().unwrap();
    let inc2_dir = tmp("difinc2");
    powdb_backup::incremental_backup(&mut cat, &base, &inc2_dir).unwrap();
    drop(cat);

    let chained = tmp("difchained");
    let err = powdb_backup::restore_chain(&base_dir, &[&inc1_dir, &inc2_dir], &chained)
        .expect_err("two increments against the same base must not chain");
    assert!(
        err.to_string().contains("increment chain broken"),
        "expected the continuity check to reject this, got: {err}"
    );

    // The documented single-increment recipe reaches each point in time.
    let at_inc2 = tmp("difat2");
    powdb_backup::restore_chain(&base_dir, &[&inc2_dir], &at_inc2).unwrap();
    assert_eq!(
        Catalog::open(&at_inc2).unwrap().scan("T").unwrap().count(),
        3,
        "inc2 alone must restore the state at its own capture time"
    );

    let at_inc1 = tmp("difat1");
    powdb_backup::restore_chain(&base_dir, &[&inc1_dir], &at_inc1).unwrap();
    assert_eq!(
        Catalog::open(&at_inc1).unwrap().scan("T").unwrap().count(),
        2,
        "inc1 alone must restore the earlier point in time"
    );
}
