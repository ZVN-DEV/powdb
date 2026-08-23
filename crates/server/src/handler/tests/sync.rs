//! The replication frontend's decisions, end to end against a real data dir.

use super::*;

#[test]
fn replica_fingerprint_is_stable_and_redacted() {
    let replica_id = "customer-prod-replica-a";
    let fingerprint = replica_fingerprint(replica_id);
    assert_eq!(fingerprint, replica_fingerprint(replica_id));
    assert_eq!(fingerprint, log_replica_fingerprint(replica_id));
    assert_ne!(fingerprint, replica_fingerprint("customer-prod-replica-b"));
    assert_eq!(fingerprint.len(), 16);
    assert!(fingerprint.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(!fingerprint.contains("customer"));
    assert!(!fingerprint.contains("replica"));
    assert!(!fingerprint.contains(replica_id));
}

#[test]
fn invalid_replica_ids_use_fixed_log_fingerprint() {
    assert_eq!(log_replica_fingerprint(""), INVALID_REPLICA_FINGERPRINT);
    assert_eq!(
        log_replica_fingerprint("customer/prod/replica"),
        INVALID_REPLICA_FINGERPRINT
    );
    assert_eq!(
        log_replica_fingerprint(&"a".repeat(4096)),
        INVALID_REPLICA_FINGERPRINT
    );
}

#[test]
fn sync_error_classes_use_bounded_labels() {
    assert_eq!(SyncErrorClass::AuthRequired.as_label(), "auth_required");
    assert_eq!(
        SyncErrorClass::PermissionDenied.as_label(),
        "permission_denied"
    );
    assert_eq!(
        SyncErrorClass::IdentityOrFormatMismatch.as_label(),
        "identity_or_format_mismatch"
    );
    assert_eq!(SyncErrorClass::AckValidation.as_label(), "ack_validation");
    assert_eq!(SyncErrorClass::Internal.as_label(), "internal");
}

/// A replica has exactly two "rebootstrap required" answers to branch on,
/// one from the pull side and one from the ack side, and they must arrive
/// as the SAME wire class. The ack one used to be reported as `Internal`,
/// which a driver is told to treat as "server bug, nothing you can fix",
/// so a replica whose cursor was gone or deactivated could not tell that
/// answer apart from an unclassified server fault.
#[test]
fn a_refused_cursor_advance_is_classified_like_its_pull_side_twin() {
    for kind in [
        std::io::ErrorKind::NotFound,     // cursor not found
        std::io::ErrorKind::InvalidInput, // cursor inactive, or LSN behind
    ] {
        let err = std::io::Error::new(kind, "replica cursor not found; rebootstrap required");
        let class = classify_sync_ack_failure(&err);
        assert_eq!(class, SyncErrorClass::AckRejected, "{kind:?}");
        assert_eq!(
            class.wire_class(),
            SyncErrorClass::IdentityOrFormatMismatch.wire_class(),
            "the two rebootstrap answers must reach the replica as one class"
        );
        assert_ne!(class.wire_class(), ErrorClass::Internal);
    }
    // A real I/O failure is still the server's problem, not the replica's.
    let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "disk is read-only");
    assert_eq!(classify_sync_ack_failure(&io), SyncErrorClass::AckUpdate);
    assert_eq!(
        SyncErrorClass::AckUpdate.wire_class(),
        ErrorClass::Internal,
        "an I/O failure the replica cannot act on stays unclassified"
    );
}

fn sync_identity() -> DatabaseIdentity {
    DatabaseIdentity {
        database_id: *b"server-sync-test",
        primary_generation: 1,
    }
}

fn retained_unit(lsn: u64) -> RetainedUnit {
    RetainedUnit {
        tx_id: 1,
        record_type: 4,
        lsn,
        data: lsn.to_le_bytes().to_vec(),
    }
}

fn retained_unit_with(tx_id: u64, record_type: WalRecordType, lsn: u64) -> RetainedUnit {
    RetainedUnit {
        tx_id,
        record_type: record_type as u8,
        lsn,
        data: lsn.to_le_bytes().to_vec(),
    }
}

fn write_sync_identity_and_tail(data_dir: &std::path::Path, through_lsn: u64) {
    let identity = sync_identity();
    write_identity_snapshot(data_dir, &IdentitySnapshot::from_identity(identity, 1)).unwrap();
    let units = (1..=through_lsn).map(retained_unit).collect();
    let segment = RetainedSegment::new(identity.segment_identity(), units).unwrap();
    write_segment_atomic(&retained_segments_dir(data_dir), &segment).unwrap();
}

fn write_sync_identity_and_units(data_dir: &std::path::Path, units: Vec<RetainedUnit>) {
    let identity = sync_identity();
    write_identity_snapshot(data_dir, &IdentitySnapshot::from_identity(identity, 1)).unwrap();
    let segment = RetainedSegment::new(identity.segment_identity(), units).unwrap();
    write_segment_atomic(&retained_segments_dir(data_dir), &segment).unwrap();
}

fn write_sync_identity_only(data_dir: &std::path::Path) {
    let identity = sync_identity();
    write_identity_snapshot(data_dir, &IdentitySnapshot::from_identity(identity, 1)).unwrap();
}

#[test]
fn sync_protocol_requires_credential_auth_and_rejects_readonly() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(RwLock::new(Engine::new(dir.path()).unwrap()));

    match dispatch_sync_status(&engine, "replica-a".into(), false, None) {
        Message::ErrorWithClass { message, class } => {
            assert!(message.contains("requires authentication"));
            assert_eq!(class, ErrorClass::AuthFailed);
        }
        other => panic!("expected auth error, got {other:?}"),
    }

    let readonly = Principal {
        name: "reader".into(),
        role: "readonly".into(),
    };
    match dispatch_sync_status(&engine, "replica-a".into(), true, Some(&readonly)) {
        Message::ErrorWithClass { message, class } => {
            assert!(message.contains("permission denied"));
            // Same class the query frontends give a role refusal.
            assert_eq!(class, ErrorClass::Execution);
        }
        other => panic!("expected permission error, got {other:?}"),
    }
}

#[test]
fn sync_status_pull_and_ack_use_server_remote_lsn() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type SyncT { required id: int, v: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert SyncT { id := 1, v := "one" }"#)
        .unwrap();
    let remote_lsn = engine.catalog().max_lsn();
    assert!(remote_lsn > 0);
    write_sync_identity_and_tail(dir.path(), remote_lsn);
    powdb_sync::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 0)).unwrap();

    let engine = Arc::new(RwLock::new(engine));
    let principal = admin_principal();
    let status = match dispatch_sync_status(&engine, "replica-a".into(), true, Some(&principal)) {
        Message::SyncStatusResult { status } => status,
        other => panic!("expected sync status, got {other:?}"),
    };
    assert_eq!(status.remote_lsn, remote_lsn);
    assert_eq!(status.servable_lsn, Some(remote_lsn));
    assert_eq!(status.unarchived_lsn, Some(0));
    assert_eq!(status.last_applied_lsn, Some(0));
    assert_eq!(status.repair_action, WireSyncRepairAction::Pull);
    assert!(status.stale);

    let identity = sync_identity().segment_identity();
    let pull = SyncPullRequest {
        replica_id: "replica-a".into(),
        since_lsn: 0,
        max_units: MAX_SYNC_PULL_UNITS,
        max_bytes: MAX_SYNC_PULL_BYTES,
        database_id: identity.database_id,
        primary_generation: identity.primary_generation,
        wal_format_version: identity.wal_format_version,
        catalog_version: identity.catalog_version,
        segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
    };
    let units = match dispatch_sync_pull(&engine, pull, true, Some(&principal)) {
        Message::SyncPullResult {
            status,
            units,
            has_more,
        } => {
            assert_eq!(status.repair_action, WireSyncRepairAction::Pull);
            assert!(!has_more);
            units
        }
        other => panic!("expected sync pull result, got {other:?}"),
    };
    assert_eq!(units.len() as u64, remote_lsn);
    assert_eq!(units.last().unwrap().lsn, remote_lsn);

    let ack = match dispatch_sync_ack(
        &engine,
        "replica-a".into(),
        remote_lsn,
        remote_lsn,
        true,
        Some(&principal),
    ) {
        Message::SyncAckResult {
            previous_applied_lsn,
            applied_lsn,
            remote_lsn: ack_remote_lsn,
            advanced,
            status,
        } => {
            assert_eq!(previous_applied_lsn, 0);
            assert_eq!(applied_lsn, remote_lsn);
            assert_eq!(ack_remote_lsn, remote_lsn);
            assert!(advanced);
            status
        }
        other => panic!("expected sync ack result, got {other:?}"),
    };
    assert_eq!(ack.repair_action, WireSyncRepairAction::None);
    assert!(!ack.stale);
    assert_eq!(ack.lag_lsn, Some(0));
}

fn seed_pullable_replica(engine: &mut Engine) -> u64 {
    let data_dir = engine.catalog().data_dir().to_path_buf();
    let remote_lsn = engine.catalog().max_lsn();
    assert!(remote_lsn > 0);
    write_sync_identity_and_tail(&data_dir, remote_lsn);
    powdb_sync::upsert_replica_cursor(&data_dir, ReplicaCursor::active("replica-a", 0)).unwrap();
    remote_lsn
}

fn pull_request_with_catalog_version(catalog_version: u16) -> SyncPullRequest {
    let identity = sync_identity().segment_identity();
    SyncPullRequest {
        replica_id: "replica-a".into(),
        since_lsn: 0,
        max_units: MAX_SYNC_PULL_UNITS,
        max_bytes: MAX_SYNC_PULL_BYTES,
        database_id: identity.database_id,
        primary_generation: identity.primary_generation,
        wal_format_version: identity.wal_format_version,
        catalog_version,
        segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
    }
}

#[test]
fn fresh_database_expects_legacy_catalog_version_and_accepts_v5_replica() {
    use powdb_storage::catalog::{CATALOG_VERSION, LEGACY_CATALOG_VERSION};

    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type Doc { required id: int, data: json }")
        .unwrap();
    engine
        .execute_powql(r#"insert Doc { id := 1, data := "{\"score\":20}" }"#)
        .unwrap();
    // No expression index created yet: the database stays at the legacy
    // catalog format, exactly as a v0.12 database on disk.
    assert_eq!(
        engine.catalog().active_catalog_version(),
        LEGACY_CATALOG_VERSION
    );
    let remote_lsn = seed_pullable_replica(&mut engine);

    let engine = Arc::new(RwLock::new(engine));
    let principal = admin_principal();

    // A replica whose maximum is the legacy version (as v0.12 clients state)
    // is accepted against a legacy-active server.
    let pull = pull_request_with_catalog_version(LEGACY_CATALOG_VERSION);
    match dispatch_sync_pull(&engine, pull, true, Some(&principal)) {
        Message::SyncPullResult { units, .. } => {
            assert_eq!(units.len() as u64, remote_lsn);
        }
        other => panic!("expected sync pull result, got {other:?}"),
    }

    // A newer replica (states this binary's max) is also accepted.
    let pull = pull_request_with_catalog_version(CATALOG_VERSION);
    assert!(matches!(
        dispatch_sync_pull(&engine, pull, true, Some(&principal)),
        Message::SyncPullResult { .. }
    ));

    // A replica whose maximum is older than the active format is rejected
    // with a message naming both versions.
    let pull = pull_request_with_catalog_version(LEGACY_CATALOG_VERSION - 1);
    match dispatch_sync_pull(&engine, pull, true, Some(&principal)) {
        Message::ErrorWithClass { message, .. } => {
            assert!(message.contains("v4"), "message: {message}");
            assert!(message.contains("v5"), "message: {message}");
            assert!(
                message.contains("rebootstrap with an upgraded replica required"),
                "message: {message}"
            );
        }
        other => panic!("expected identity mismatch error, got {other:?}"),
    }
}

#[test]
fn activated_database_expects_v6_and_rejects_v5_replica() {
    use powdb_storage::catalog::{EXPRESSION_INDEX_CATALOG_VERSION, LEGACY_CATALOG_VERSION};

    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type Doc { required id: int, data: json }")
        .unwrap();
    engine
        .execute_powql(r#"insert Doc { id := 1, data := "{\"score\":20}" }"#)
        .unwrap();
    // Creating a JSON-path expression index activates the v6 catalog format.
    engine
        .execute_powql("alter Doc add index (.data->score)")
        .unwrap();
    assert_eq!(
        engine.catalog().active_catalog_version(),
        EXPRESSION_INDEX_CATALOG_VERSION
    );
    let remote_lsn = seed_pullable_replica(&mut engine);

    let engine = Arc::new(RwLock::new(engine));
    let principal = admin_principal();

    // A v0.12 replica (states catalog_version 5) genuinely cannot read the
    // now-activated v6 data and is rejected with the targeted message.
    let pull = pull_request_with_catalog_version(LEGACY_CATALOG_VERSION);
    match dispatch_sync_pull(&engine, pull, true, Some(&principal)) {
        Message::ErrorWithClass { message, .. } => {
            assert!(message.contains("v5"), "message: {message}");
            assert!(message.contains("v6"), "message: {message}");
            assert!(
                message.contains("rebootstrap with an upgraded replica required"),
                "message: {message}"
            );
        }
        other => panic!("expected identity mismatch error, got {other:?}"),
    }

    // A v6-capable replica is accepted.
    let pull = pull_request_with_catalog_version(EXPRESSION_INDEX_CATALOG_VERSION);
    match dispatch_sync_pull(&engine, pull, true, Some(&principal)) {
        Message::SyncPullResult { units, .. } => {
            assert_eq!(units.len() as u64, remote_lsn);
        }
        other => panic!("expected sync pull result, got {other:?}"),
    }
}

#[test]
fn sync_pull_and_ack_reject_transaction_cut_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type SyncT { required id: int }")
        .unwrap();
    for id in 1..=3 {
        engine
            .execute_powql(&format!("insert SyncT {{ id := {id} }}"))
            .unwrap();
    }
    let remote_lsn = engine.catalog().max_lsn();
    assert!(remote_lsn >= 3);
    write_sync_identity_and_units(
        dir.path(),
        vec![
            retained_unit_with(77, WalRecordType::Begin, 1),
            retained_unit_with(77, WalRecordType::Insert, 2),
            retained_unit_with(77, WalRecordType::Commit, 3),
        ],
    );
    powdb_sync::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 0)).unwrap();

    let engine = Arc::new(RwLock::new(engine));
    let principal = admin_principal();
    let identity = sync_identity().segment_identity();
    let cut_pull = SyncPullRequest {
        replica_id: "replica-a".into(),
        since_lsn: 0,
        max_units: 2,
        max_bytes: MAX_SYNC_PULL_BYTES,
        database_id: identity.database_id,
        primary_generation: identity.primary_generation,
        wal_format_version: identity.wal_format_version,
        catalog_version: identity.catalog_version,
        segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
    };
    match dispatch_sync_pull(&engine, cut_pull, true, Some(&principal)) {
        Message::ErrorWithClass { message, .. } => {
            assert!(message.contains("cuts through transaction"))
        }
        other => panic!("expected transaction-cut pull error, got {other:?}"),
    }

    let cut_bytes_pull = SyncPullRequest {
        replica_id: "replica-a".into(),
        since_lsn: 0,
        max_units: 3,
        max_bytes: 58,
        database_id: identity.database_id,
        primary_generation: identity.primary_generation,
        wal_format_version: identity.wal_format_version,
        catalog_version: identity.catalog_version,
        segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
    };
    match dispatch_sync_pull(&engine, cut_bytes_pull, true, Some(&principal)) {
        Message::ErrorWithClass { message, .. } => {
            assert!(message.contains("cuts through transaction"))
        }
        other => panic!("expected byte-capped transaction-cut pull error, got {other:?}"),
    }

    let full_pull = SyncPullRequest {
        replica_id: "replica-a".into(),
        since_lsn: 0,
        max_units: 3,
        max_bytes: MAX_SYNC_PULL_BYTES,
        database_id: identity.database_id,
        primary_generation: identity.primary_generation,
        wal_format_version: identity.wal_format_version,
        catalog_version: identity.catalog_version,
        segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
    };
    match dispatch_sync_pull(&engine, full_pull, true, Some(&principal)) {
        Message::SyncPullResult { units, .. } => {
            assert_eq!(units.len(), 3);
            assert_eq!(units.last().unwrap().lsn, 3);
        }
        other => panic!("expected complete transaction pull, got {other:?}"),
    }

    match dispatch_sync_ack(
        &engine,
        "replica-a".into(),
        2,
        remote_lsn,
        true,
        Some(&principal),
    ) {
        Message::ErrorWithClass { message, .. } => {
            assert!(message.contains("cuts through transaction"))
        }
        other => panic!("expected transaction-cut ack error, got {other:?}"),
    }
    let cursor = powdb_sync::read_replica_cursors(dir.path()).unwrap();
    assert_eq!(cursor[0].applied_lsn, 0);

    match dispatch_sync_ack(
        &engine,
        "replica-a".into(),
        3,
        remote_lsn,
        true,
        Some(&principal),
    ) {
        Message::SyncAckResult {
            previous_applied_lsn,
            applied_lsn,
            advanced,
            ..
        } => {
            assert_eq!(previous_applied_lsn, 0);
            assert_eq!(applied_lsn, 3);
            assert!(advanced);
        }
        other => panic!("expected complete transaction ack, got {other:?}"),
    }
}

#[test]
fn sync_pull_byte_cap_returns_applyable_prefix_with_reused_tx_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type SyncT { required id: int }")
        .unwrap();
    for id in 1..=6 {
        engine
            .execute_powql(&format!("insert SyncT {{ id := {id} }}"))
            .unwrap();
    }
    let remote_lsn = engine.catalog().max_lsn();
    assert!(remote_lsn >= 6);
    write_sync_identity_and_units(
        dir.path(),
        vec![
            retained_unit_with(1, WalRecordType::Begin, 1),
            retained_unit_with(1, WalRecordType::Insert, 2),
            retained_unit_with(1, WalRecordType::Commit, 3),
            retained_unit_with(1, WalRecordType::Begin, 4),
            retained_unit_with(1, WalRecordType::Insert, 5),
            retained_unit_with(1, WalRecordType::Commit, 6),
        ],
    );
    powdb_sync::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 0)).unwrap();

    let engine = Arc::new(RwLock::new(engine));
    let principal = admin_principal();
    let identity = sync_identity().segment_identity();
    let pull = SyncPullRequest {
        replica_id: "replica-a".into(),
        since_lsn: 0,
        max_units: 6,
        max_bytes: 100,
        database_id: identity.database_id,
        primary_generation: identity.primary_generation,
        wal_format_version: identity.wal_format_version,
        catalog_version: identity.catalog_version,
        segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
    };

    match dispatch_sync_pull(&engine, pull, true, Some(&principal)) {
        Message::SyncPullResult {
            status,
            units,
            has_more,
        } => {
            assert_eq!(status.repair_action, WireSyncRepairAction::Pull);
            assert_eq!(units.len(), 3);
            assert_eq!(units.last().unwrap().lsn, 3);
            assert!(has_more);
        }
        other => panic!("expected byte-capped applyable prefix, got {other:?}"),
    }
}

#[test]
fn sync_pull_never_serves_units_beyond_server_remote_lsn() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type SyncT { required id: int }")
        .unwrap();
    engine.execute_powql("insert SyncT { id := 1 }").unwrap();
    let remote_lsn = engine.catalog().max_lsn();
    assert!(remote_lsn > 0);
    write_sync_identity_and_tail(dir.path(), remote_lsn + 2);
    powdb_sync::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 0)).unwrap();

    let engine = Arc::new(RwLock::new(engine));
    let principal = admin_principal();
    let identity = sync_identity().segment_identity();
    let pull = SyncPullRequest {
        replica_id: "replica-a".into(),
        since_lsn: 0,
        max_units: MAX_SYNC_PULL_UNITS,
        max_bytes: MAX_SYNC_PULL_BYTES,
        database_id: identity.database_id,
        primary_generation: identity.primary_generation,
        wal_format_version: identity.wal_format_version,
        catalog_version: identity.catalog_version,
        segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
    };

    match dispatch_sync_pull(&engine, pull, true, Some(&principal)) {
        Message::SyncPullResult {
            status,
            units,
            has_more,
        } => {
            assert_eq!(status.remote_lsn, remote_lsn);
            assert_eq!(status.servable_lsn, Some(remote_lsn));
            assert_eq!(status.unarchived_lsn, Some(0));
            assert_eq!(status.repair_action, WireSyncRepairAction::Pull);
            assert!(!has_more);
            assert_eq!(units.len() as u64, remote_lsn);
            assert_eq!(units.last().unwrap().lsn, remote_lsn);
            assert!(units.iter().all(|unit| unit.lsn <= remote_lsn));
        }
        other => panic!("expected capped sync pull result, got {other:?}"),
    }
}

#[test]
fn sync_status_reports_await_archive_when_primary_outruns_retained_tail() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type SyncT { required id: int }")
        .unwrap();
    engine.execute_powql("insert SyncT { id := 1 }").unwrap();
    let remote_lsn = engine.catalog().max_lsn();
    assert!(remote_lsn > 0);
    write_sync_identity_only(dir.path());
    powdb_sync::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 0)).unwrap();

    let engine = Arc::new(RwLock::new(engine));
    let principal = admin_principal();
    let identity = sync_identity().segment_identity();
    let status = match dispatch_sync_status(&engine, "replica-a".into(), true, Some(&principal)) {
        Message::SyncStatusResult { status } => status,
        other => panic!("expected sync status, got {other:?}"),
    };
    assert_eq!(status.remote_lsn, remote_lsn);
    assert_eq!(status.servable_lsn, Some(0));
    assert_eq!(status.unarchived_lsn, Some(remote_lsn));
    assert_eq!(status.repair_action, WireSyncRepairAction::AwaitArchive);
    assert!(status
        .last_sync_error
        .as_deref()
        .unwrap()
        .contains("not yet archived"));

    let pull = SyncPullRequest {
        replica_id: "replica-a".into(),
        since_lsn: 0,
        max_units: MAX_SYNC_PULL_UNITS,
        max_bytes: MAX_SYNC_PULL_BYTES,
        database_id: identity.database_id,
        primary_generation: identity.primary_generation,
        wal_format_version: identity.wal_format_version,
        catalog_version: identity.catalog_version,
        segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
    };
    match dispatch_sync_pull(&engine, pull, true, Some(&principal)) {
        Message::SyncPullResult {
            status,
            units,
            has_more,
        } => {
            assert_eq!(status.repair_action, WireSyncRepairAction::AwaitArchive);
            assert!(units.is_empty());
            assert!(!has_more);
        }
        other => panic!("expected await-archive sync pull result, got {other:?}"),
    }
}

#[test]
fn sync_pull_serves_partial_retained_prefix_when_archive_lags_remote_lsn() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type SyncT { required id: int }")
        .unwrap();
    engine.execute_powql("insert SyncT { id := 1 }").unwrap();
    engine.execute_powql("insert SyncT { id := 2 }").unwrap();
    let remote_lsn = engine.catalog().max_lsn();
    assert!(remote_lsn > 1);
    let servable_lsn = remote_lsn - 1;
    write_sync_identity_and_tail(dir.path(), servable_lsn);
    powdb_sync::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 0)).unwrap();

    let engine = Arc::new(RwLock::new(engine));
    let principal = admin_principal();
    let identity = sync_identity().segment_identity();
    let pull = SyncPullRequest {
        replica_id: "replica-a".into(),
        since_lsn: 0,
        max_units: MAX_SYNC_PULL_UNITS,
        max_bytes: MAX_SYNC_PULL_BYTES,
        database_id: identity.database_id,
        primary_generation: identity.primary_generation,
        wal_format_version: identity.wal_format_version,
        catalog_version: identity.catalog_version,
        segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
    };

    match dispatch_sync_pull(&engine, pull, true, Some(&principal)) {
        Message::SyncPullResult {
            status,
            units,
            has_more,
        } => {
            assert_eq!(status.remote_lsn, remote_lsn);
            assert_eq!(status.servable_lsn, Some(servable_lsn));
            assert_eq!(status.unarchived_lsn, Some(1));
            assert_eq!(status.repair_action, WireSyncRepairAction::Pull);
            assert!(!has_more);
            assert_eq!(units.len() as u64, servable_lsn);
            assert_eq!(units.last().unwrap().lsn, servable_lsn);
            assert!(units.iter().all(|unit| unit.lsn <= servable_lsn));
        }
        other => panic!("expected partial sync pull result, got {other:?}"),
    }
}

#[test]
fn sync_pull_rejects_cursor_or_format_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type SyncT { required id: int }")
        .unwrap();
    engine.execute_powql("insert SyncT { id := 1 }").unwrap();
    let remote_lsn = engine.catalog().max_lsn();
    write_sync_identity_and_tail(dir.path(), remote_lsn);
    powdb_sync::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 0)).unwrap();
    let engine = Arc::new(RwLock::new(engine));
    let principal = admin_principal();
    let identity = sync_identity().segment_identity();

    let wrong_cursor = SyncPullRequest {
        replica_id: "replica-a".into(),
        since_lsn: 1,
        max_units: 10,
        max_bytes: MAX_SYNC_PULL_BYTES,
        database_id: identity.database_id,
        primary_generation: identity.primary_generation,
        wal_format_version: identity.wal_format_version,
        catalog_version: identity.catalog_version,
        segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
    };
    match dispatch_sync_pull(&engine, wrong_cursor, true, Some(&principal)) {
        Message::ErrorWithClass { message, .. } => assert!(message.contains("does not match")),
        other => panic!("expected cursor mismatch error, got {other:?}"),
    }

    let wrong_format = SyncPullRequest {
        replica_id: "replica-a".into(),
        since_lsn: 0,
        max_units: 10,
        max_bytes: MAX_SYNC_PULL_BYTES,
        database_id: identity.database_id,
        primary_generation: identity.primary_generation,
        wal_format_version: identity.wal_format_version,
        catalog_version: identity.catalog_version,
        segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION + 1,
    };
    match dispatch_sync_pull(&engine, wrong_format, true, Some(&principal)) {
        Message::ErrorWithClass { message, .. } => {
            assert!(message.contains("rebootstrap required"))
        }
        other => panic!("expected format mismatch error, got {other:?}"),
    }
}
