use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::catalog::{Catalog, CATALOG_LSN_FILE};
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};

fn tmp(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    // pid + atomic counter => unique even when the clock is too coarse to
    // distinguish two parallel calls (macOS CI collided on nanos alone).
    let uniq = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "powdb_bk_{tag}_{}_{}_{}",
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

#[test]
fn full_backup_copies_files_and_records_lsn() {
    let src = tmp("src");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    cat.insert("T", &vec![Value::Int(1)]).unwrap();
    cat.sync_wal().unwrap();

    let dest = tmp("dest");
    let manifest = powdb_backup::full_backup(&mut cat, &dest).unwrap();

    assert!(
        manifest.source_lsn > 0,
        "snapshot must record a nonzero LSN"
    );
    assert!(manifest.files.iter().any(|f| f.name == "catalog.bin"));
    assert!(manifest.files.iter().any(|f| f.name == "T.heap"));
    assert!(
        !manifest.files.iter().any(|f| f.name == "wal.log"),
        "WAL is truncated by checkpoint; a snapshot must not include it"
    );
    assert!(dest.join("T.heap").exists());
    assert!(dest.join(powdb_backup::BackupManifest::FILE_NAME).exists());
    // Every recorded file must actually exist in dest with a matching blake3.
    for f in &manifest.files {
        let bytes = std::fs::read(dest.join(&f.name)).unwrap();
        assert_eq!(bytes.len() as u64, f.len);
        assert_eq!(blake3::hash(&bytes).to_hex().to_string(), f.blake3_hex);
    }
}

#[test]
fn full_backup_records_ddl_only_lsn() {
    let src = tmp("ddl_lsn_src");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();

    let backup = tmp("ddl_lsn_backup");
    let manifest = powdb_backup::full_backup(&mut cat, &backup).unwrap();

    assert!(
        manifest.source_lsn > 0,
        "DDL-only snapshots must advance the durable LSN"
    );
    assert!(
        manifest.files.iter().any(|f| f.name == CATALOG_LSN_FILE),
        "full backups must preserve the catalog LSN sidecar"
    );

    let restored = tmp("ddl_lsn_restored");
    powdb_backup::restore(&backup, &restored).unwrap();
    let restored_cat = Catalog::open(&restored).unwrap();
    assert_eq!(restored_cat.list_tables(), vec!["T"]);
    assert_eq!(restored_cat.max_lsn(), manifest.source_lsn);
}

#[test]
fn restore_rebuilds_a_usable_database() {
    let src = tmp("rsrc");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    for i in 0..50 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();

    let backup = tmp("bkp");
    powdb_backup::full_backup(&mut cat, &backup).unwrap();
    drop(cat);

    let restored = tmp("restored");
    powdb_backup::restore(&backup, &restored).unwrap();

    // Reopen the restored dir and confirm every row is present.
    let cat2 = Catalog::open(&restored).unwrap();
    assert_eq!(cat2.scan("T").unwrap().count(), 50);
}

#[test]
fn full_backup_refuses_active_transaction_without_persisting_uncommitted_rows() {
    let src = tmp("active_tx_src");
    {
        let mut cat = Catalog::create(&src).unwrap();
        cat.create_table(schema_t()).unwrap();
        cat.checkpoint().unwrap();
        cat.begin_transaction().unwrap();
        cat.insert("T", &vec![Value::Int(1)]).unwrap();
        cat.sync_wal().unwrap();

        let backup = tmp("active_tx_bkp");
        let err = powdb_backup::full_backup(&mut cat, &backup).unwrap_err();
        assert!(
            err.to_string().contains("transaction is active"),
            "full backup must fail closed during an active transaction, got: {err}"
        );
    }

    let cat = Catalog::open(&src).unwrap();
    assert_eq!(
        cat.scan("T").unwrap().count(),
        0,
        "failed backup must not leak active transaction rows through drop/checkpoint"
    );
}

#[test]
fn full_backup_records_sync_metadata_plain_restore_strips_identity_explicit_preserve_keeps_it() {
    let src = tmp("syncsrc");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    cat.insert("T", &vec![Value::Int(1)]).unwrap();
    cat.sync_wal().unwrap();
    let identity = powdb_sync::open_or_create_identity(&src).unwrap();

    let backup = tmp("syncbkp");
    let manifest = powdb_backup::full_backup(&mut cat, &backup).unwrap();
    drop(cat);

    let sync = manifest
        .sync
        .clone()
        .expect("sync-enabled backup must record fork-safety metadata");
    assert_eq!(sync.identity.identity().unwrap(), identity);
    assert_eq!(sync.source_lsn, manifest.source_lsn);
    assert_eq!(
        sync.wal_format_version,
        powdb_storage::wal::WAL_FORMAT_VERSION
    );
    assert_eq!(
        sync.catalog_version,
        powdb_storage::catalog::LEGACY_CATALOG_VERSION
    );
    assert_eq!(manifest.catalog_version, sync.catalog_version);
    assert_eq!(
        sync.retained_segment_format_version,
        powdb_sync::RETAINED_SEGMENT_FORMAT_VERSION
    );
    assert!(
        !backup.join(powdb_sync::SYNC_STATE_DIR).exists(),
        "backup must not copy mutable sync state wholesale"
    );
    let retained_units = powdb_sync::read_units_since(
        &powdb_sync::retained_segments_dir(&src),
        identity.segment_identity(),
        0,
        100,
    )
    .unwrap();
    assert!(
        !retained_units.is_empty(),
        "sync-enabled backup checkpoint must archive retained WAL records before truncation"
    );

    let restored = tmp("syncrestored");
    powdb_backup::restore(&backup, &restored).unwrap();

    let err = powdb_sync::read_identity(&restored).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    let mut plain_cat = Catalog::open(&restored).unwrap();
    plain_cat.insert("T", &vec![Value::Int(2)]).unwrap();
    plain_cat.sync_wal().unwrap();
    drop(plain_cat);
    let plain_reopened = Catalog::open(&restored).unwrap();
    assert_eq!(plain_reopened.scan("T").unwrap().count(), 2);
    drop(plain_reopened);

    let preserved = tmp("syncpreserved");
    powdb_backup::restore_with_sync_mode(
        &backup,
        &preserved,
        powdb_backup::RestoreSyncMode::PreserveSyncIdentity,
    )
    .unwrap();
    assert_eq!(powdb_sync::read_identity(&preserved).unwrap(), identity);
    assert!(
        !preserved
            .join(powdb_sync::SYNC_STATE_DIR)
            .join(powdb_sync::REPLICA_CURSORS_FILE)
            .exists(),
        "preserve restore must not synthesize stale replica cursors"
    );
    let cat2 = Catalog::open(&preserved).unwrap();
    assert_eq!(cat2.scan("T").unwrap().count(), 1);
}

#[test]
fn full_backup_fork_restore_mints_new_sync_identity() {
    let src = tmp("syncforksrc");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    cat.insert("T", &vec![Value::Int(1)]).unwrap();
    cat.sync_wal().unwrap();
    let source_identity = powdb_sync::open_or_create_identity(&src).unwrap();

    let backup = tmp("syncforkbkp");
    let manifest = powdb_backup::full_backup(&mut cat, &backup).unwrap();
    drop(cat);
    assert!(
        manifest.sync.is_some(),
        "sync-enabled backup must include sync metadata"
    );

    let restored = tmp("syncforkrestored");
    powdb_backup::restore_with_sync_mode(
        &backup,
        &restored,
        powdb_backup::RestoreSyncMode::ForkWithNewSyncIdentity,
    )
    .unwrap();

    let restored_identity = powdb_sync::read_identity(&restored).unwrap();
    assert_ne!(
        restored_identity, source_identity,
        "fork restores must not reuse the source sync identity"
    );
    assert_eq!(
        restored_identity.primary_generation, 1,
        "fork restores start a fresh primary generation"
    );
    let cat2 = Catalog::open(&restored).unwrap();
    assert_eq!(cat2.scan("T").unwrap().count(), 1);
}

#[test]
fn restore_rejects_tampered_sync_snapshot_metadata() {
    let src = tmp("syncbadsrc");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    cat.insert("T", &vec![Value::Int(1)]).unwrap();
    cat.sync_wal().unwrap();
    powdb_sync::open_or_create_identity(&src).unwrap();

    let backup = tmp("syncbadbkp");
    let mut manifest = powdb_backup::full_backup(&mut cat, &backup).unwrap();
    drop(cat);
    manifest
        .sync
        .as_mut()
        .expect("sync metadata")
        .catalog_blake3_hex = "not-the-catalog-hash".into();
    manifest.write(&backup).unwrap();

    let restored = tmp("syncbadrestored");
    let err = powdb_backup::restore(&backup, &restored).unwrap_err();
    assert!(
        format!("{err}").to_lowercase().contains("catalog hash"),
        "tampered sync catalog hash must be rejected, got: {err}"
    );
}

#[test]
fn legacy_manifest_without_sync_identity_still_restores() {
    let src = tmp("legsrc");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    cat.insert("T", &vec![Value::Int(1)]).unwrap();
    cat.sync_wal().unwrap();

    let backup = tmp("legbkp");
    powdb_backup::full_backup(&mut cat, &backup).unwrap();
    drop(cat);

    let manifest_path = backup.join(powdb_backup::BackupManifest::FILE_NAME);
    let raw = std::fs::read_to_string(&manifest_path).unwrap();
    let mut json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    json.as_object_mut().unwrap().remove("sync");
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&json).unwrap()).unwrap();

    let restored = tmp("legrestored");
    powdb_backup::restore(&backup, &restored).unwrap();

    assert_eq!(
        powdb_sync::read_identity(&restored).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    let cat2 = Catalog::open(&restored).unwrap();
    assert_eq!(cat2.scan("T").unwrap().count(), 1);
}

#[test]
fn preserve_restore_requires_sync_metadata() {
    let src = tmp("preserve_legacy_src");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    cat.insert("T", &vec![Value::Int(1)]).unwrap();
    cat.sync_wal().unwrap();

    let backup = tmp("preserve_legacy_bkp");
    powdb_backup::full_backup(&mut cat, &backup).unwrap();
    drop(cat);

    let restored = tmp("preserve_legacy_restored");
    let err = powdb_backup::restore_with_sync_mode(
        &backup,
        &restored,
        powdb_backup::RestoreSyncMode::PreserveSyncIdentity,
    )
    .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        err.to_string().contains("requires sync snapshot metadata"),
        "preserve restore without sync metadata must be explicit, got: {err}"
    );
}

// Same count-extraction shape as restore_durability.rs::count.
fn count(e: &mut Engine, q: &str) -> i64 {
    match e
        .execute_powql(q)
        .unwrap_or_else(|err| panic!("`{q}` failed: {err}"))
    {
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
fn restore_refuses_nonempty_destination() {
    let src = tmp("nesrc");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    cat.insert("T", &vec![Value::Int(1)]).unwrap();
    cat.sync_wal().unwrap();

    let backup = tmp("nebkp");
    powdb_backup::full_backup(&mut cat, &backup).unwrap();
    drop(cat);

    // A dest dir that already holds a stale file (e.g. a leftover wal.log that
    // would replay onto restored data and corrupt it) must be refused.
    let dirty = tmp("nedirty");
    std::fs::create_dir_all(&dirty).unwrap();
    std::fs::write(dirty.join("wal.log"), b"stale").unwrap();
    let err = powdb_backup::restore(&backup, &dirty).unwrap_err();
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("empty") || msg.contains("exists") || msg.contains("not empty"),
        "non-empty dest must be refused, got: {err}"
    );

    // Happy path must not regress: a fresh/nonexistent dir still succeeds.
    let fresh = tmp("nefresh");
    powdb_backup::restore(&backup, &fresh).unwrap();
    let cat2 = Catalog::open(&fresh).unwrap();
    assert_eq!(cat2.scan("T").unwrap().count(), 1);
}

#[test]
fn index_bearing_table_round_trips() {
    let src = tmp("idxsrc");
    {
        let mut e = Engine::new(&src).unwrap();
        e.execute_powql("type T { required id: int }").unwrap();
        e.execute_powql("alter T add index .id").unwrap();
        for i in 0..30 {
            e.execute_powql(&format!("insert T {{ id := {i} }}"))
                .unwrap();
        }
    }

    let backup = tmp("idxbkp");
    let mut cat = Catalog::open(&src).unwrap();
    let manifest = powdb_backup::full_backup(&mut cat, &backup).unwrap();
    assert!(
        manifest.files.iter().any(|f| f.name.ends_with(".idx")),
        "index file must be backed up"
    );
    drop(cat);

    let restored = tmp("idxrestored");
    powdb_backup::restore(&backup, &restored).unwrap();
    let mut e2 = Engine::new(&restored).unwrap();
    assert_eq!(count(&mut e2, "count(T)"), 30, "all rows must restore");
    assert_eq!(
        count(&mut e2, "count(T filter .id = 7)"),
        1,
        "indexed point lookup must work on the restored DB"
    );
}

#[test]
fn backup_restore_backup_is_idempotent() {
    let src = tmp("idemsrc");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    for i in 0..40 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();

    let backup1 = tmp("idembkp1");
    let m1 = powdb_backup::full_backup(&mut cat, &backup1).unwrap();
    drop(cat);

    let restored = tmp("idemrestored");
    powdb_backup::restore(&backup1, &restored).unwrap();

    let backup2 = tmp("idembkp2");
    let mut cat2 = Catalog::open(&restored).unwrap();
    let m2 = powdb_backup::full_backup(&mut cat2, &backup2).unwrap();
    drop(cat2);

    assert_eq!(m1.source_lsn, m2.source_lsn, "consistent LSN must match");
    let names1: Vec<&String> = m1.files.iter().map(|f| &f.name).collect();
    let names2: Vec<&String> = m2.files.iter().map(|f| &f.name).collect();
    assert_eq!(names1, names2, "same set of files must be produced");
    for (a, b) in m1.files.iter().zip(m2.files.iter()) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.len, b.len, "{} len must match", a.name);
        assert_eq!(a.blake3_hex, b.blake3_hex, "{} hash must match", a.name);
    }
}

#[test]
fn restore_rejects_unknown_manifest_format() {
    let src = tmp("fmtsrc");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    cat.insert("T", &vec![Value::Int(1)]).unwrap();
    cat.sync_wal().unwrap();

    let backup = tmp("fmtbkp");
    powdb_backup::full_backup(&mut cat, &backup).unwrap();
    drop(cat);

    // Bump format_version to an unsupported value in the on-disk manifest.
    let mpath = backup.join(powdb_backup::BackupManifest::FILE_NAME);
    let raw = std::fs::read_to_string(&mpath).unwrap();
    let mut json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    json["format_version"] = serde_json::json!(999);
    std::fs::write(&mpath, serde_json::to_vec_pretty(&json).unwrap()).unwrap();

    let dest = tmp("fmtdest");
    let err = powdb_backup::restore(&backup, &dest).unwrap_err();
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("format") || msg.contains("unsupported"),
        "unknown manifest format must be rejected at restore, got: {err}"
    );
}

#[test]
fn restore_rejects_manifest_path_traversal_name() {
    let src = tmp("travsrc");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    cat.insert("T", &vec![Value::Int(1)]).unwrap();
    cat.sync_wal().unwrap();

    let backup = tmp("travbkp");
    let mut manifest = powdb_backup::full_backup(&mut cat, &backup).unwrap();
    drop(cat);

    manifest.files[0].name = "../escaped.heap".into();
    manifest.write(&backup).unwrap();

    let dest = tmp("travdest");
    let err = powdb_backup::restore(&backup, &dest).unwrap_err();
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("invalid") && msg.contains("manifest"),
        "path traversal manifest name must be rejected, got: {err}"
    );
    assert!(
        !backup.parent().unwrap().join("escaped.heap").exists(),
        "restore must not write outside the destination"
    );
}

#[test]
fn restore_rejects_a_tampered_backup() {
    let src = tmp("tsrc");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    cat.insert("T", &vec![Value::Int(1)]).unwrap();
    cat.sync_wal().unwrap();

    let backup = tmp("tbkp");
    powdb_backup::full_backup(&mut cat, &backup).unwrap();

    // Corrupt a backed-up data file (blake3 in the manifest will no longer match).
    std::fs::write(backup.join("T.heap"), b"corrupted").unwrap();

    let restored = tmp("trestored");
    let err = powdb_backup::restore(&backup, &restored).unwrap_err();
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("integrity") || msg.contains("hash") || msg.contains("corrupt"),
        "tampered backup must be rejected with an integrity error, got: {err}"
    );
}
