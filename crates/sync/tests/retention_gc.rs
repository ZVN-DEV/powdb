use std::path::Path;

use powdb_sync::{
    list_segment_files, prune_retained_segments_for_cursors, prune_retained_segments_with_policy,
    read_replica_cursors, read_units_since, retained_segments_dir, retire_replica_cursor,
    upsert_replica_cursor, write_identity_snapshot, write_segment_atomic, DatabaseIdentity,
    IdentitySnapshot, ReplicaCursor, RetainedSegment, RetainedUnit, RetentionPressurePolicy,
};

fn database_identity() -> DatabaseIdentity {
    DatabaseIdentity {
        database_id: *b"retention-gc-id!",
        primary_generation: 1,
    }
}

fn unit(lsn: u64) -> RetainedUnit {
    RetainedUnit {
        tx_id: 1,
        record_type: 1,
        lsn,
        data: lsn.to_le_bytes().to_vec(),
    }
}

fn write_segment(data_dir: &Path, start_lsn: u64, end_lsn: u64) {
    let segment_dir = retained_segments_dir(data_dir);
    let units = (start_lsn..=end_lsn).map(unit).collect();
    let segment = RetainedSegment::new(database_identity().segment_identity(), units).unwrap();
    write_segment_atomic(&segment_dir, &segment).unwrap();
}

fn write_identity(data_dir: &Path) {
    let snapshot = IdentitySnapshot::from_identity(database_identity(), 1);
    write_identity_snapshot(data_dir, &snapshot).unwrap();
}

fn active_cursor(replica_id: &str, applied_lsn: u64, updated_unix_secs: u64) -> ReplicaCursor {
    ReplicaCursor {
        replica_id: replica_id.to_string(),
        applied_lsn,
        updated_unix_secs,
        active: true,
    }
}

#[test]
fn active_lagging_replica_sets_retention_floor() {
    let data_dir = tempfile::tempdir().unwrap();
    write_identity(data_dir.path());
    write_segment(data_dir.path(), 1, 5);
    write_segment(data_dir.path(), 6, 10);
    write_segment(data_dir.path(), 11, 15);

    upsert_replica_cursor(data_dir.path(), ReplicaCursor::active("replica-a", 5)).unwrap();
    upsert_replica_cursor(data_dir.path(), ReplicaCursor::active("replica-b", 12)).unwrap();

    let summary =
        prune_retained_segments_for_cursors(data_dir.path(), database_identity()).unwrap();
    assert_eq!(summary.retain_from_lsn, Some(6));
    assert_eq!(summary.segments_deleted, 1);
    assert_eq!(summary.oldest_retained_lsn, Some(6));
    assert_eq!(summary.newest_retained_lsn, Some(15));

    let segment_dir = retained_segments_dir(data_dir.path());
    let files = list_segment_files(&segment_dir).unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].start_lsn, 6);

    let units =
        read_units_since(&segment_dir, database_identity().segment_identity(), 5, 20).unwrap();
    let lsns: Vec<u64> = units.into_iter().map(|unit| unit.lsn).collect();
    assert_eq!(lsns, vec![6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
}

#[test]
fn inactive_cursor_expiry_releases_history_and_marks_cursor_inactive() {
    let data_dir = tempfile::tempdir().unwrap();
    write_identity(data_dir.path());
    write_segment(data_dir.path(), 1, 5);
    write_segment(data_dir.path(), 6, 10);
    write_segment(data_dir.path(), 11, 15);

    upsert_replica_cursor(data_dir.path(), active_cursor("replica-stale", 5, 10)).unwrap();
    upsert_replica_cursor(data_dir.path(), active_cursor("replica-fast", 12, 200)).unwrap();

    let mut policy = RetentionPressurePolicy::conservative(200);
    policy.inactive_cursor_expiry_secs = Some(100);
    let summary =
        prune_retained_segments_with_policy(data_dir.path(), database_identity(), policy).unwrap();

    assert_eq!(summary.inactive_cursors_retired, vec!["replica-stale"]);
    assert!(summary.override_cursors_retired.is_empty());
    assert_eq!(summary.gc.retain_from_lsn, Some(13));
    assert_eq!(summary.gc.segments_deleted, 2);
    assert!(summary.retained_bytes_before > summary.retained_bytes_after);
    assert!(!summary.max_retained_bytes_exceeded);

    let cursors = read_replica_cursors(data_dir.path()).unwrap();
    let stale = cursors
        .iter()
        .find(|cursor| cursor.replica_id == "replica-stale")
        .unwrap();
    assert!(!stale.active);
    assert_eq!(stale.updated_unix_secs, 200);

    let err =
        upsert_replica_cursor(data_dir.path(), active_cursor("replica-stale", 5, 201)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        err.to_string().contains("rebootstrap"),
        "expired cursor should require rebootstrap before resuming, got: {err}"
    );
}

#[test]
fn max_retained_bytes_reports_pressure_without_stranding_active_cursor() {
    let data_dir = tempfile::tempdir().unwrap();
    write_identity(data_dir.path());
    write_segment(data_dir.path(), 1, 5);
    write_segment(data_dir.path(), 6, 10);

    upsert_replica_cursor(data_dir.path(), active_cursor("replica-lagging", 0, 100)).unwrap();

    let mut policy = RetentionPressurePolicy::conservative(200);
    policy.max_retained_bytes = Some(1);
    let summary =
        prune_retained_segments_with_policy(data_dir.path(), database_identity(), policy).unwrap();

    assert_eq!(summary.gc.retain_from_lsn, Some(1));
    assert_eq!(summary.gc.segments_deleted, 0);
    assert_eq!(summary.retained_bytes_before, summary.retained_bytes_after);
    assert_eq!(summary.max_retained_bytes, Some(1));
    assert!(summary.max_retained_bytes_exceeded);
    assert!(summary.inactive_cursors_retired.is_empty());
    assert!(summary.override_cursors_retired.is_empty());

    let cursors = read_replica_cursors(data_dir.path()).unwrap();
    assert!(cursors
        .iter()
        .any(|cursor| cursor.replica_id == "replica-lagging" && cursor.active));
    assert_eq!(
        list_segment_files(&retained_segments_dir(data_dir.path()))
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn operator_retain_boundary_retires_lagging_cursors_for_rebootstrap() {
    let data_dir = tempfile::tempdir().unwrap();
    write_identity(data_dir.path());
    write_segment(data_dir.path(), 1, 5);
    write_segment(data_dir.path(), 6, 10);
    write_segment(data_dir.path(), 11, 15);

    upsert_replica_cursor(data_dir.path(), active_cursor("replica-lagging", 5, 100)).unwrap();
    upsert_replica_cursor(data_dir.path(), active_cursor("replica-fast", 12, 100)).unwrap();

    let mut policy = RetentionPressurePolicy::conservative(300);
    policy.operator_retain_from_lsn = Some(13);
    let summary =
        prune_retained_segments_with_policy(data_dir.path(), database_identity(), policy).unwrap();

    assert_eq!(summary.override_cursors_retired, vec!["replica-lagging"]);
    assert!(summary.inactive_cursors_retired.is_empty());
    assert_eq!(summary.gc.retain_from_lsn, Some(13));
    assert_eq!(summary.gc.segments_deleted, 2);
    assert_eq!(summary.gc.oldest_retained_lsn, Some(11));
    assert_eq!(summary.gc.newest_retained_lsn, Some(15));

    let cursors = read_replica_cursors(data_dir.path()).unwrap();
    assert!(cursors
        .iter()
        .any(|cursor| cursor.replica_id == "replica-lagging" && !cursor.active));
    assert!(cursors
        .iter()
        .any(|cursor| cursor.replica_id == "replica-fast" && cursor.active));

    let err = upsert_replica_cursor(data_dir.path(), active_cursor("replica-lagging", 5, 301))
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        err.to_string().contains("rebootstrap"),
        "operator-retired cursor should require rebootstrap before resuming, got: {err}"
    );
}

#[test]
fn retention_policy_validation_error_leaves_cursors_unchanged() {
    let data_dir = tempfile::tempdir().unwrap();
    write_identity(data_dir.path());
    write_segment(data_dir.path(), 1, 5);
    write_segment(data_dir.path(), 6, 10);
    write_segment(data_dir.path(), 13, 15);

    upsert_replica_cursor(data_dir.path(), active_cursor("replica-lagging", 5, 100)).unwrap();
    let segment_dir = retained_segments_dir(data_dir.path());
    let gap_file = list_segment_files(&segment_dir)
        .unwrap()
        .into_iter()
        .find(|file| file.start_lsn == 6)
        .unwrap();
    std::fs::remove_file(gap_file.path).unwrap();

    let mut policy = RetentionPressurePolicy::conservative(300);
    policy.operator_retain_from_lsn = Some(11);
    let err = prune_retained_segments_with_policy(data_dir.path(), database_identity(), policy)
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        err.to_string().contains("gap"),
        "gapped retained tail should fail validation before cursor mutation, got: {err}"
    );

    let cursors = read_replica_cursors(data_dir.path()).unwrap();
    let cursor = cursors
        .iter()
        .find(|cursor| cursor.replica_id == "replica-lagging")
        .unwrap();
    assert!(cursor.active);
    assert_eq!(cursor.updated_unix_secs, 100);
    assert_eq!(list_segment_files(&segment_dir).unwrap().len(), 2);
}

#[test]
fn retired_cursor_releases_old_segments_but_keeps_boundary_segment() {
    let data_dir = tempfile::tempdir().unwrap();
    write_identity(data_dir.path());
    write_segment(data_dir.path(), 1, 5);
    write_segment(data_dir.path(), 6, 10);
    write_segment(data_dir.path(), 11, 15);

    upsert_replica_cursor(data_dir.path(), ReplicaCursor::active("replica-a", 5)).unwrap();
    upsert_replica_cursor(data_dir.path(), ReplicaCursor::active("replica-b", 12)).unwrap();
    prune_retained_segments_for_cursors(data_dir.path(), database_identity()).unwrap();

    retire_replica_cursor(data_dir.path(), "replica-a", 100).unwrap();
    let summary =
        prune_retained_segments_for_cursors(data_dir.path(), database_identity()).unwrap();
    assert_eq!(summary.retain_from_lsn, Some(13));
    assert_eq!(summary.segments_deleted, 1);
    assert_eq!(summary.oldest_retained_lsn, Some(11));
    assert_eq!(summary.newest_retained_lsn, Some(15));

    let segment_dir = retained_segments_dir(data_dir.path());
    let files = list_segment_files(&segment_dir).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].start_lsn, 11);
    assert_eq!(files[0].end_lsn, 15);

    let units =
        read_units_since(&segment_dir, database_identity().segment_identity(), 12, 20).unwrap();
    let lsns: Vec<u64> = units.into_iter().map(|unit| unit.lsn).collect();
    assert_eq!(lsns, vec![13, 14, 15]);
}

#[test]
fn stale_cursor_publication_after_gc_requires_rebootstrap() {
    let data_dir = tempfile::tempdir().unwrap();
    write_identity(data_dir.path());
    write_segment(data_dir.path(), 1, 5);
    write_segment(data_dir.path(), 6, 10);
    write_segment(data_dir.path(), 11, 15);

    upsert_replica_cursor(data_dir.path(), ReplicaCursor::active("replica-fast", 12)).unwrap();
    prune_retained_segments_for_cursors(data_dir.path(), database_identity()).unwrap();

    let err = upsert_replica_cursor(data_dir.path(), ReplicaCursor::active("replica-lagging", 5))
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        err.to_string().contains("rebootstrap"),
        "stale cursor publication should return a repair hint, got: {err}"
    );
}

#[test]
fn concurrent_gc_and_lagging_cursor_publication_do_not_strand_active_cursor() {
    use std::sync::{Arc, Barrier};

    let data_dir = tempfile::tempdir().unwrap();
    write_identity(data_dir.path());
    write_segment(data_dir.path(), 1, 5);
    write_segment(data_dir.path(), 6, 10);
    write_segment(data_dir.path(), 11, 15);
    upsert_replica_cursor(data_dir.path(), ReplicaCursor::active("replica-fast", 12)).unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let gc_dir = data_dir.path().to_path_buf();
    let cursor_dir = data_dir.path().to_path_buf();
    let gc_barrier = Arc::clone(&barrier);
    let cursor_barrier = Arc::clone(&barrier);

    let gc = std::thread::spawn(move || {
        gc_barrier.wait();
        prune_retained_segments_for_cursors(&gc_dir, database_identity())
    });
    let cursor = std::thread::spawn(move || {
        cursor_barrier.wait();
        upsert_replica_cursor(&cursor_dir, ReplicaCursor::active("replica-lagging", 5))
    });

    let gc_result = gc.join().unwrap();
    let cursor_result = cursor.join().unwrap();
    gc_result.unwrap();

    let segment_dir = retained_segments_dir(data_dir.path());
    match cursor_result {
        Ok(()) => {
            let units =
                read_units_since(&segment_dir, database_identity().segment_identity(), 5, 20)
                    .unwrap();
            assert!(
                units.iter().any(|unit| unit.lsn == 6),
                "if lagging cursor publication wins the lock, GC must keep its needed tail"
            );
        }
        Err(err) => {
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
            assert!(
                err.to_string().contains("rebootstrap"),
                "if GC wins the lock, lagging cursor publication must fail with repair guidance, got: {err}"
            );
        }
    }
}
