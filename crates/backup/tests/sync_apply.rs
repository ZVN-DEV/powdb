use powdb_storage::catalog::Catalog;
use powdb_storage::types::{ColumnDef, Row, RowId, Schema, TypeId, Value};

fn tmp(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let uniq = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "powdb_sync_apply_{tag}_{}_{}_{}",
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

fn schema_users() -> Schema {
    Schema {
        table_name: "User".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            },
            ColumnDef {
                name: "email".into(),
                type_id: TypeId::Str,
                required: false,
                position: 1,
            },
        ],
    }
}

fn user_row(id: i64) -> Row {
    vec![Value::Int(id), Value::Str(format!("user{id}@example.com"))]
}

fn insert_range(cat: &mut Catalog, start: i64, end: i64) {
    for id in start..end {
        cat.insert("User", &user_row(id)).unwrap();
    }
    cat.commit_autocommit().unwrap();
    cat.sync_wal().unwrap();
}

fn find_rid(cat: &Catalog, id: i64) -> RowId {
    cat.scan("User")
        .unwrap()
        .find_map(|(rid, row)| match row.first() {
            Some(Value::Int(found)) if *found == id => Some(rid),
            _ => None,
        })
        .unwrap()
}

fn update_email(cat: &mut Catalog, id: i64, email: &str) {
    let rid = find_rid(cat, id);
    cat.update(
        "User",
        rid,
        &vec![Value::Int(id), Value::Str(email.to_string())],
    )
    .unwrap();
    cat.commit_autocommit().unwrap();
    cat.sync_wal().unwrap();
}

fn delete_user(cat: &mut Catalog, id: i64) {
    let rid = find_rid(cat, id);
    cat.delete("User", rid).unwrap();
    cat.commit_autocommit().unwrap();
    cat.sync_wal().unwrap();
}

fn rows(cat: &Catalog) -> Vec<(i64, String)> {
    let mut rows: Vec<_> = cat
        .scan("User")
        .unwrap()
        .map(|(_, row)| {
            let id = match &row[0] {
                Value::Int(id) => *id,
                other => panic!("expected id int, got {other:?}"),
            };
            let email = match &row[1] {
                Value::Str(email) => email.clone(),
                other => panic!("expected email str, got {other:?}"),
            };
            (id, email)
        })
        .collect();
    rows.sort_by_key(|(id, _)| *id);
    rows
}

#[test]
fn snapshot_plus_post_snapshot_tail_apply_converges_rows_and_indexes() {
    let primary = tmp("primary");
    let mut primary_cat = Catalog::create(&primary).unwrap();
    primary_cat.create_table(schema_users()).unwrap();
    insert_range(&mut primary_cat, 0, 10);
    primary_cat.create_index_unique("User", "id", true).unwrap();
    let identity = powdb_sync::open_or_create_identity(&primary).unwrap();

    let backup = tmp("backup");
    let manifest = powdb_backup::full_backup(&mut primary_cat, &backup).unwrap();
    let snapshot_lsn = manifest.source_lsn;

    insert_range(&mut primary_cat, 10, 15);
    update_email(&mut primary_cat, 3, "updated-3@example.com");
    delete_user(&mut primary_cat, 4);
    let primary_rows = rows(&primary_cat);

    let replica = tmp("replica");
    let bootstrap = powdb_backup::bootstrap_replica_from_full_backup(
        &mut primary_cat,
        &backup,
        &replica,
        "replica-a",
    )
    .unwrap();
    assert_eq!(bootstrap.snapshot_lsn, snapshot_lsn);
    assert!(bootstrap.remote_lsn > bootstrap.snapshot_lsn);

    let mut replica_cat = powdb_sync::open_preserving_retained_segments(&replica).unwrap();
    assert_eq!(rows(&replica_cat).len(), 10);

    let applied = powdb_sync::apply_retained_tail(
        &mut replica_cat,
        &powdb_sync::retained_segments_dir(&primary),
        identity.segment_identity(),
        bootstrap.snapshot_lsn,
        bootstrap.remote_lsn,
    )
    .unwrap();
    assert_eq!(applied.first_lsn, Some(bootstrap.snapshot_lsn + 1));
    assert_eq!(applied.last_lsn, Some(bootstrap.remote_lsn));
    assert_eq!(rows(&replica_cat), primary_rows);

    assert_eq!(
        replica_cat
            .index_lookup("User", "id", &Value::Int(3))
            .unwrap()
            .unwrap()[1],
        Value::Str("updated-3@example.com".into())
    );
    assert!(replica_cat
        .index_lookup("User", "id", &Value::Int(4))
        .unwrap()
        .is_none());

    drop(replica_cat);
    let reopened = powdb_sync::open_preserving_retained_segments(&replica).unwrap();
    assert_eq!(rows(&reopened), primary_rows);
}

#[test]
fn chunked_tail_apply_exposes_coherent_rows_between_chunks() {
    let primary = tmp("chunked_primary");
    let mut primary_cat = Catalog::create(&primary).unwrap();
    primary_cat.create_table(schema_users()).unwrap();
    insert_range(&mut primary_cat, 0, 6);
    primary_cat.create_index_unique("User", "id", true).unwrap();
    let identity = powdb_sync::open_or_create_identity(&primary).unwrap();

    let backup = tmp("chunked_backup");
    let manifest = powdb_backup::full_backup(&mut primary_cat, &backup).unwrap();
    let snapshot_lsn = manifest.source_lsn;

    insert_range(&mut primary_cat, 6, 9);
    powdb_sync::checkpoint_preserving_retained_segments_if_enabled(&mut primary_cat).unwrap();
    let mid_lsn = primary_cat.max_lsn();
    let mid_rows = rows(&primary_cat);

    update_email(&mut primary_cat, 3, "chunked-3@example.com");
    delete_user(&mut primary_cat, 4);
    insert_range(&mut primary_cat, 9, 12);
    powdb_sync::checkpoint_preserving_retained_segments_if_enabled(&mut primary_cat).unwrap();
    let final_lsn = primary_cat.max_lsn();
    let final_rows = rows(&primary_cat);
    assert!(snapshot_lsn < mid_lsn && mid_lsn < final_lsn);

    let replica = tmp("chunked_replica");
    let bootstrap = powdb_backup::bootstrap_replica_from_full_backup(
        &mut primary_cat,
        &backup,
        &replica,
        "replica-chunked",
    )
    .unwrap();
    assert_eq!(bootstrap.snapshot_lsn, snapshot_lsn);
    assert_eq!(bootstrap.remote_lsn, final_lsn);

    let retained_dir = powdb_sync::retained_segments_dir(&primary);
    let first_chunk = powdb_sync::read_units_through(
        &retained_dir,
        identity.segment_identity(),
        snapshot_lsn,
        mid_lsn,
        4096,
    )
    .unwrap();
    let second_chunk = powdb_sync::read_units_through(
        &retained_dir,
        identity.segment_identity(),
        mid_lsn,
        final_lsn,
        4096,
    )
    .unwrap();
    assert_eq!(
        first_chunk.first().map(|unit| unit.lsn),
        Some(snapshot_lsn + 1)
    );
    assert_eq!(first_chunk.last().map(|unit| unit.lsn), Some(mid_lsn));
    assert_eq!(second_chunk.first().map(|unit| unit.lsn), Some(mid_lsn + 1));
    assert_eq!(second_chunk.last().map(|unit| unit.lsn), Some(final_lsn));

    let mut replica_cat = powdb_sync::open_preserving_retained_segments(&replica).unwrap();
    let first = powdb_sync::apply_retained_units_chunk(
        &mut replica_cat,
        identity.segment_identity(),
        snapshot_lsn,
        &first_chunk,
    )
    .unwrap();
    assert_eq!(first.first_lsn, Some(snapshot_lsn + 1));
    assert_eq!(first.last_lsn, Some(mid_lsn));
    assert_eq!(rows(&replica_cat), mid_rows);
    assert_eq!(
        replica_cat
            .index_lookup("User", "id", &Value::Int(7))
            .unwrap()
            .unwrap()[1],
        Value::Str("user7@example.com".into())
    );

    let duplicate_first = powdb_sync::apply_retained_units_chunk(
        &mut replica_cat,
        identity.segment_identity(),
        snapshot_lsn,
        &first_chunk,
    )
    .unwrap();
    assert_eq!(duplicate_first.units_applied, 0);
    assert_eq!(rows(&replica_cat), mid_rows);
    drop(replica_cat);

    let mut replica_cat = powdb_sync::open_preserving_retained_segments(&replica).unwrap();
    assert_eq!(
        rows(&replica_cat),
        mid_rows,
        "restart between chunks must expose the completed first chunk only"
    );

    let second = powdb_sync::apply_retained_units_chunk(
        &mut replica_cat,
        identity.segment_identity(),
        mid_lsn,
        &second_chunk,
    )
    .unwrap();
    assert_eq!(second.first_lsn, Some(mid_lsn + 1));
    assert_eq!(second.last_lsn, Some(final_lsn));
    assert_eq!(rows(&replica_cat), final_rows);
    assert_eq!(
        replica_cat
            .index_lookup("User", "id", &Value::Int(3))
            .unwrap()
            .unwrap()[1],
        Value::Str("chunked-3@example.com".into())
    );
    assert!(replica_cat
        .index_lookup("User", "id", &Value::Int(4))
        .unwrap()
        .is_none());

    drop(replica_cat);
    let reopened = powdb_sync::open_preserving_retained_segments(&replica).unwrap();
    assert_eq!(rows(&reopened), final_rows);
}

#[test]
fn duplicate_tail_apply_is_noop_after_replica_reaches_target_lsn() {
    let primary = tmp("dupe_primary");
    let mut primary_cat = Catalog::create(&primary).unwrap();
    primary_cat.create_table(schema_users()).unwrap();
    insert_range(&mut primary_cat, 0, 3);
    let identity = powdb_sync::open_or_create_identity(&primary).unwrap();

    let backup = tmp("dupe_backup");
    powdb_backup::full_backup(&mut primary_cat, &backup).unwrap();
    insert_range(&mut primary_cat, 3, 6);

    let replica = tmp("dupe_replica");
    let bootstrap = powdb_backup::bootstrap_replica_from_full_backup(
        &mut primary_cat,
        &backup,
        &replica,
        "replica-dupe",
    )
    .unwrap();

    let mut replica_cat = powdb_sync::open_preserving_retained_segments(&replica).unwrap();
    powdb_sync::apply_retained_tail(
        &mut replica_cat,
        &powdb_sync::retained_segments_dir(&primary),
        identity.segment_identity(),
        bootstrap.snapshot_lsn,
        bootstrap.remote_lsn,
    )
    .unwrap();
    let once = rows(&replica_cat);

    let second = powdb_sync::apply_retained_tail(
        &mut replica_cat,
        &powdb_sync::retained_segments_dir(&primary),
        identity.segment_identity(),
        bootstrap.snapshot_lsn,
        bootstrap.remote_lsn,
    )
    .unwrap();
    assert_eq!(second.units_applied, 0);
    assert_eq!(rows(&replica_cat), once);
}

#[test]
fn noop_tail_apply_rejects_local_pending_wal() {
    let primary = tmp("noop_primary");
    let mut primary_cat = Catalog::create(&primary).unwrap();
    primary_cat.create_table(schema_users()).unwrap();
    insert_range(&mut primary_cat, 0, 2);
    let identity = powdb_sync::open_or_create_identity(&primary).unwrap();

    let backup = tmp("noop_backup");
    powdb_backup::full_backup(&mut primary_cat, &backup).unwrap();

    let replica = tmp("noop_replica");
    let bootstrap = powdb_backup::bootstrap_replica_from_full_backup(
        &mut primary_cat,
        &backup,
        &replica,
        "replica-noop",
    )
    .unwrap();
    assert_eq!(bootstrap.snapshot_lsn, bootstrap.remote_lsn);

    let mut replica_cat = powdb_sync::open_preserving_retained_segments(&replica).unwrap();
    replica_cat.insert("User", &user_row(99)).unwrap();
    replica_cat.sync_wal().unwrap();

    let err = powdb_sync::apply_retained_tail(
        &mut replica_cat,
        &powdb_sync::retained_segments_dir(&primary),
        identity.segment_identity(),
        bootstrap.snapshot_lsn,
        bootstrap.remote_lsn,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("local WAL records are pending"),
        "noop apply must reject local divergent WAL history, got: {err}"
    );
}
