use powdb_storage::catalog::Catalog;
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};

fn tmp(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let uniq = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "powdb_sync_bootstrap_{tag}_{}_{}_{}",
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

fn schema_u() -> Schema {
    Schema {
        table_name: "U".into(),
        columns: vec![ColumnDef {
            name: "id".into(),
            type_id: TypeId::Int,
            required: true,
            position: 0,
        }],
    }
}

fn insert_range(cat: &mut Catalog, start: i64, end: i64) {
    for i in start..end {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.commit_autocommit().unwrap();
    cat.sync_wal().unwrap();
}

#[test]
fn bootstrap_rejects_post_snapshot_ddl_tail_without_publishing_cursor() {
    let primary = tmp("ddl_tail_primary");
    let mut cat = Catalog::create(&primary).unwrap();
    cat.create_table(schema_t()).unwrap();
    insert_range(&mut cat, 0, 2);
    powdb_sync::open_or_create_identity(&primary).unwrap();

    let backup = tmp("ddl_tail_backup");
    let manifest = powdb_backup::full_backup(&mut cat, &backup).unwrap();
    let snapshot_lsn = manifest.source_lsn;

    cat.create_table(schema_u()).unwrap();
    powdb_sync::checkpoint_preserving_retained_segments_if_enabled(&mut cat).unwrap();
    let remote_lsn = cat.max_lsn();
    assert!(remote_lsn > snapshot_lsn);

    let replica = tmp("ddl_tail_replica");
    let err = powdb_backup::bootstrap_replica_from_full_backup(
        &mut cat,
        &backup,
        &replica,
        "replica-ddl-tail",
    )
    .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        err.to_string()
            .contains("DDL retained units are not supported"),
        "bootstrap must reject unsupported DDL tail before restore/cursor publication, got: {err}"
    );
    assert!(!replica.exists());
    assert!(powdb_sync::read_replica_cursors(&primary)
        .unwrap()
        .iter()
        .all(|cursor| cursor.replica_id != "replica-ddl-tail"));
}

#[test]
fn bootstrap_replica_from_full_backup_pins_cursor_and_validates_tail() {
    let primary = tmp("primary");
    let mut cat = Catalog::create(&primary).unwrap();
    cat.create_table(schema_t()).unwrap();
    insert_range(&mut cat, 0, 10);
    let identity = powdb_sync::open_or_create_identity(&primary).unwrap();

    let backup = tmp("backup");
    let manifest = powdb_backup::full_backup(&mut cat, &backup).unwrap();
    let snapshot_lsn = manifest.source_lsn;
    assert!(manifest.sync.is_some());

    insert_range(&mut cat, 10, 15);
    let remote_lsn = cat.max_lsn();
    assert!(remote_lsn > snapshot_lsn);

    let replica = tmp("replica");
    let summary =
        powdb_backup::bootstrap_replica_from_full_backup(&mut cat, &backup, &replica, "replica-a")
            .unwrap();

    assert_eq!(summary.replica_id, "replica-a");
    assert_eq!(summary.snapshot_lsn, snapshot_lsn);
    assert_eq!(summary.remote_lsn, remote_lsn);
    assert_eq!(summary.retained_tail_start_lsn, Some(snapshot_lsn + 1));
    assert_eq!(summary.retained_tail_end_lsn, Some(remote_lsn));
    assert_eq!(
        summary.retained_units_available as u64,
        remote_lsn - snapshot_lsn
    );

    let cursors = powdb_sync::read_replica_cursors(&primary).unwrap();
    let cursor = cursors
        .iter()
        .find(|cursor| cursor.replica_id == "replica-a")
        .expect("bootstrap should publish an active primary-side cursor");
    assert!(cursor.active);
    assert_eq!(cursor.applied_lsn, snapshot_lsn);

    assert_eq!(powdb_sync::read_identity(&replica).unwrap(), identity);
    let restored = Catalog::open(&replica).unwrap();
    assert_eq!(
        restored.scan("T").unwrap().count(),
        10,
        "bootstrap restores the snapshot; retained-tail apply is covered by sync_apply"
    );
}

#[test]
fn bootstrap_rejects_legacy_backup_without_sync_metadata() {
    let primary = tmp("legacy_primary");
    let mut cat = Catalog::create(&primary).unwrap();
    cat.create_table(schema_t()).unwrap();
    insert_range(&mut cat, 0, 1);

    let backup = tmp("legacy_backup");
    let manifest = powdb_backup::full_backup(&mut cat, &backup).unwrap();
    assert!(manifest.sync.is_none());

    let replica = tmp("legacy_replica");
    let err = powdb_backup::bootstrap_replica_from_full_backup(
        &mut cat,
        &backup,
        &replica,
        "replica-legacy",
    )
    .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("sync-enabled"));
}

#[test]
fn bootstrap_rejects_missing_retained_tail_without_publishing_cursor() {
    let primary = tmp("missing_tail_primary");
    let mut cat = Catalog::create(&primary).unwrap();
    cat.create_table(schema_t()).unwrap();
    insert_range(&mut cat, 0, 2);
    powdb_sync::open_or_create_identity(&primary).unwrap();

    let backup = tmp("missing_tail_backup");
    let manifest = powdb_backup::full_backup(&mut cat, &backup).unwrap();
    insert_range(&mut cat, 2, 3);
    powdb_sync::checkpoint_preserving_retained_segments_if_enabled(&mut cat).unwrap();
    let remote_lsn = cat.max_lsn();
    assert!(remote_lsn > manifest.source_lsn);
    for file in powdb_sync::list_segment_files(&powdb_sync::retained_segments_dir(&primary))
        .unwrap()
        .into_iter()
        .filter(|file| file.end_lsn > manifest.source_lsn)
    {
        std::fs::remove_file(file.path).unwrap();
    }

    let replica = tmp("missing_tail_replica");
    let err = powdb_backup::bootstrap_replica_from_full_backup(
        &mut cat,
        &backup,
        &replica,
        "replica-missing-tail",
    )
    .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("missing required LSN"));

    assert!(powdb_sync::read_replica_cursors(&primary)
        .unwrap()
        .iter()
        .all(|cursor| cursor.replica_id != "replica-missing-tail"));
    assert_eq!(cat.max_lsn(), remote_lsn);
}

#[test]
fn bootstrap_rejects_primary_identity_mismatch() {
    let source = tmp("source");
    let mut source_cat = Catalog::create(&source).unwrap();
    source_cat.create_table(schema_t()).unwrap();
    insert_range(&mut source_cat, 0, 1);
    powdb_sync::open_or_create_identity(&source).unwrap();

    let backup = tmp("mismatch_backup");
    let manifest = powdb_backup::full_backup(&mut source_cat, &backup).unwrap();
    drop(source_cat);

    let other_primary = tmp("other_primary");
    let mut other_cat = Catalog::create(&other_primary).unwrap();
    other_cat.create_table(schema_t()).unwrap();
    insert_range(&mut other_cat, 0, 1);
    powdb_sync::open_or_create_identity(&other_primary).unwrap();

    let replica = tmp("mismatch_replica");
    let err = powdb_backup::bootstrap_replica_from_full_backup(
        &mut other_cat,
        &backup,
        &replica,
        "replica-mismatch",
    )
    .unwrap_err();
    assert!(manifest.source_lsn > 0);
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("does not match"));
    assert!(powdb_sync::read_replica_cursors(&other_primary)
        .unwrap()
        .is_empty());
}

#[test]
fn bootstrap_rejects_existing_active_cursor_without_clobbering_it() {
    let primary = tmp("active_cursor_primary");
    let mut cat = Catalog::create(&primary).unwrap();
    cat.create_table(schema_t()).unwrap();
    insert_range(&mut cat, 0, 2);
    powdb_sync::open_or_create_identity(&primary).unwrap();

    let backup = tmp("active_cursor_backup");
    let manifest = powdb_backup::full_backup(&mut cat, &backup).unwrap();
    insert_range(&mut cat, 2, 4);
    powdb_sync::checkpoint_preserving_retained_segments_if_enabled(&mut cat).unwrap();
    let existing_applied_lsn = cat.max_lsn();
    powdb_sync::upsert_replica_cursor(
        &primary,
        powdb_sync::ReplicaCursor::active("replica-existing", existing_applied_lsn),
    )
    .unwrap();

    let replica = tmp("active_cursor_replica");
    let err = powdb_backup::bootstrap_replica_from_full_backup(
        &mut cat,
        &backup,
        &replica,
        "replica-existing",
    )
    .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(!replica.exists());

    let cursors = powdb_sync::read_replica_cursors(&primary).unwrap();
    let cursor = cursors
        .iter()
        .find(|cursor| cursor.replica_id == "replica-existing")
        .unwrap();
    assert!(cursor.active);
    assert_eq!(cursor.applied_lsn, existing_applied_lsn);
    assert!(manifest.source_lsn < existing_applied_lsn);
}

#[test]
fn bootstrap_cleans_restored_replica_when_cursor_publication_fails() {
    let primary = tmp("cleanup_primary");
    let mut cat = Catalog::create(&primary).unwrap();
    cat.create_table(schema_t()).unwrap();
    insert_range(&mut cat, 0, 2);
    powdb_sync::open_or_create_identity(&primary).unwrap();

    let backup = tmp("cleanup_backup");
    let manifest = powdb_backup::full_backup(&mut cat, &backup).unwrap();
    insert_range(&mut cat, 2, 4);
    powdb_sync::checkpoint_preserving_retained_segments_if_enabled(&mut cat).unwrap();

    let replica = tmp("cleanup_replica");
    let err = powdb_backup::bootstrap_replica_from_full_backup(
        &mut cat,
        &backup,
        &replica,
        "replica/invalid",
    )
    .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        err.to_string().contains("replica id contains unsupported"),
        "invalid replica id should fail during cursor publication, got: {err}"
    );
    assert!(
        !replica.exists(),
        "bootstrap must remove the restored replica directory when cursor publication fails"
    );
    assert!(powdb_sync::read_replica_cursors(&primary)
        .unwrap()
        .is_empty());
    assert!(manifest.source_lsn < cat.max_lsn());
}
