use std::fs;
use std::io;
use std::path::Path;

use crate::metadata::{
    now_unix_secs, read_replica_cursors_unlocked, replace_replica_cursors_unlocked,
    with_cursor_metadata_lock,
};
use crate::segment::list_segment_files;
use crate::{read_identity, retained_segments_dir, retained_tail_progress, ReplicaCursor};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncRepairAction {
    None,
    Pull,
    AwaitArchive,
    Rebootstrap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaSyncStatus {
    pub replica_id: String,
    pub active: bool,
    pub last_applied_lsn: Option<u64>,
    pub remote_lsn: u64,
    /// Largest contiguous retained-unit LSN currently servable after the
    /// replica cursor. This can lag behind `remote_lsn` while the primary has
    /// progressed but its latest WAL records have not yet been published into
    /// retained segments.
    pub servable_lsn: Option<u64>,
    /// Primary progress that is not yet represented in contiguous retained
    /// history for this replica.
    pub unarchived_lsn: Option<u64>,
    pub lag_lsn: Option<u64>,
    /// Estimated retained segment bytes that overlap the unapplied LSN range.
    pub lag_bytes: Option<u64>,
    /// Milliseconds since the primary-side cursor was last acknowledged while stale.
    pub lag_ms: Option<u64>,
    pub stale: bool,
    pub repair_action: SyncRepairAction,
    pub last_sync_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaApplyAckSummary {
    pub replica_id: String,
    pub previous_applied_lsn: u64,
    pub applied_lsn: u64,
    pub remote_lsn: u64,
    pub advanced: bool,
    pub status: ReplicaSyncStatus,
}

/// Acknowledge that a replica successfully applied retained history through an
/// LSN observed on the primary.
///
/// This is the primary-side half of the future pull protocol: after a replica
/// applies a tail locally, the server should call this to advance the retained
/// history floor for that replica. The update is monotonic and fail-closed:
/// stale acknowledgements, inactive cursors, and acknowledgements beyond the
/// advertised primary LSN are rejected.
pub fn acknowledge_replica_apply(
    data_dir: &Path,
    replica_id: &str,
    applied_lsn: u64,
    remote_lsn: u64,
) -> io::Result<ReplicaApplyAckSummary> {
    acknowledge_replica_apply_at(
        data_dir,
        replica_id,
        applied_lsn,
        remote_lsn,
        now_unix_secs(),
    )
}

/// Return primary-side sync status for one replica cursor.
pub fn replica_sync_status(
    data_dir: &Path,
    replica_id: &str,
    remote_lsn: u64,
) -> io::Result<ReplicaSyncStatus> {
    replica_sync_status_at(data_dir, replica_id, remote_lsn, now_unix_secs())
}

fn acknowledge_replica_apply_at(
    data_dir: &Path,
    replica_id: &str,
    applied_lsn: u64,
    remote_lsn: u64,
    now: u64,
) -> io::Result<ReplicaApplyAckSummary> {
    if applied_lsn > remote_lsn {
        return Err(invalid_input(format!(
            "replica acknowledgement LSN {applied_lsn} is ahead of remote LSN {remote_lsn}"
        )));
    }

    with_cursor_metadata_lock(data_dir, || {
        let mut cursors = read_replica_cursors_unlocked(data_dir)?;
        let Some(cursor) = cursors
            .iter_mut()
            .find(|cursor| cursor.replica_id == replica_id)
        else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "replica cursor not found; rebootstrap required",
            ));
        };
        if !cursor.active {
            return Err(invalid_input(
                "replica cursor is inactive; rebootstrap required",
            ));
        }
        if applied_lsn < cursor.applied_lsn {
            return Err(invalid_input(format!(
                "replica acknowledgement LSN {applied_lsn} is behind primary cursor LSN {}",
                cursor.applied_lsn
            )));
        }

        let previous_applied_lsn = cursor.applied_lsn;
        let mut acknowledged = cursor.clone();
        acknowledged.applied_lsn = applied_lsn;
        acknowledged.updated_unix_secs = now;
        let status = status_for_cursor(data_dir, replica_id, Some(&acknowledged), remote_lsn, now)?;
        *cursor = acknowledged;
        replace_replica_cursors_unlocked(data_dir, cursors)?;

        Ok(ReplicaApplyAckSummary {
            replica_id: replica_id.to_string(),
            previous_applied_lsn,
            applied_lsn,
            remote_lsn,
            advanced: applied_lsn > previous_applied_lsn,
            status,
        })
    })
}

fn replica_sync_status_at(
    data_dir: &Path,
    replica_id: &str,
    remote_lsn: u64,
    now: u64,
) -> io::Result<ReplicaSyncStatus> {
    let cursors = read_replica_cursors_unlocked(data_dir)?;
    let cursor = cursors
        .iter()
        .find(|cursor| cursor.replica_id == replica_id);
    status_for_cursor(data_dir, replica_id, cursor, remote_lsn, now)
}

fn status_for_cursor(
    data_dir: &Path,
    replica_id: &str,
    cursor: Option<&ReplicaCursor>,
    remote_lsn: u64,
    now: u64,
) -> io::Result<ReplicaSyncStatus> {
    let Some(cursor) = cursor else {
        return Ok(ReplicaSyncStatus {
            replica_id: replica_id.to_string(),
            active: false,
            last_applied_lsn: None,
            remote_lsn,
            servable_lsn: None,
            unarchived_lsn: None,
            lag_lsn: None,
            lag_bytes: None,
            lag_ms: None,
            stale: true,
            repair_action: SyncRepairAction::Rebootstrap,
            last_sync_error: Some("replica cursor not found; rebootstrap required".to_string()),
        });
    };

    let lag_lsn = remote_lsn.checked_sub(cursor.applied_lsn);
    if !cursor.active {
        return Ok(ReplicaSyncStatus {
            replica_id: replica_id.to_string(),
            active: false,
            last_applied_lsn: Some(cursor.applied_lsn),
            remote_lsn,
            servable_lsn: None,
            unarchived_lsn: None,
            lag_lsn,
            lag_bytes: None,
            lag_ms: None,
            stale: true,
            repair_action: SyncRepairAction::Rebootstrap,
            last_sync_error: Some("replica cursor is inactive; rebootstrap required".to_string()),
        });
    }
    if cursor.applied_lsn > remote_lsn {
        return Ok(ReplicaSyncStatus {
            replica_id: replica_id.to_string(),
            active: true,
            last_applied_lsn: Some(cursor.applied_lsn),
            remote_lsn,
            servable_lsn: None,
            unarchived_lsn: None,
            lag_lsn: None,
            lag_bytes: None,
            lag_ms: None,
            stale: true,
            repair_action: SyncRepairAction::Rebootstrap,
            last_sync_error: Some(format!(
                "replica cursor LSN {} is ahead of remote LSN {remote_lsn}",
                cursor.applied_lsn
            )),
        });
    }
    if cursor.applied_lsn == remote_lsn {
        return Ok(ReplicaSyncStatus {
            replica_id: replica_id.to_string(),
            active: true,
            last_applied_lsn: Some(cursor.applied_lsn),
            remote_lsn,
            servable_lsn: Some(remote_lsn),
            unarchived_lsn: Some(0),
            lag_lsn: Some(0),
            lag_bytes: Some(0),
            lag_ms: Some(0),
            stale: false,
            repair_action: SyncRepairAction::None,
            last_sync_error: None,
        });
    }

    let lag_ms = now
        .saturating_sub(cursor.updated_unix_secs)
        .saturating_mul(1000);
    let identity = read_identity(data_dir)?;
    let segment_dir = retained_segments_dir(data_dir);
    let progress = retained_tail_progress(
        &segment_dir,
        identity.segment_identity(),
        cursor.applied_lsn,
        remote_lsn,
    );

    match progress {
        Ok(progress) => {
            let servable_lsn = progress.contiguous_through_lsn;
            let unarchived_lsn = remote_lsn.saturating_sub(servable_lsn);
            let (repair_action, last_sync_error) = if servable_lsn > cursor.applied_lsn {
                (SyncRepairAction::Pull, None)
            } else {
                (
                    SyncRepairAction::AwaitArchive,
                    Some("primary has advanced, but retained history is not yet archived; retry after archive".to_string()),
                )
            };
            Ok(ReplicaSyncStatus {
                replica_id: replica_id.to_string(),
                active: true,
                last_applied_lsn: Some(cursor.applied_lsn),
                remote_lsn,
                servable_lsn: Some(servable_lsn),
                unarchived_lsn: Some(unarchived_lsn),
                lag_lsn,
                lag_bytes: Some(retained_lag_bytes(
                    &segment_dir,
                    cursor.applied_lsn,
                    servable_lsn,
                )?),
                lag_ms: Some(lag_ms),
                stale: true,
                repair_action,
                last_sync_error,
            })
        }
        Err(err)
            if err.kind() == io::ErrorKind::InvalidData
                && (err.to_string().contains("gap") || err.to_string().contains("missing")) =>
        {
            Ok(ReplicaSyncStatus {
                replica_id: replica_id.to_string(),
                active: true,
                last_applied_lsn: Some(cursor.applied_lsn),
                remote_lsn,
                servable_lsn: None,
                unarchived_lsn: None,
                lag_lsn,
                lag_bytes: None,
                lag_ms: Some(lag_ms),
                stale: true,
                repair_action: SyncRepairAction::Rebootstrap,
                last_sync_error: Some(format!(
                    "retained history is unavailable; rebootstrap required: {err}"
                )),
            })
        }
        Err(err) => Err(err),
    }
}

fn retained_lag_bytes(dir: &Path, applied_lsn: u64, remote_lsn: u64) -> io::Result<u64> {
    let mut total = 0u64;
    for file in list_segment_files(dir)? {
        if file.end_lsn <= applied_lsn || file.start_lsn > remote_lsn {
            continue;
        }
        let len = fs::metadata(file.path)?.len();
        total = total
            .checked_add(len)
            .ok_or_else(|| io::Error::other("retained lag byte count overflow"))?;
    }
    Ok(total)
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    crate::SyncError::InvalidRequest(message.into()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        minimum_retained_lsn, read_replica_cursors, write_identity_snapshot, write_segment_atomic,
        DatabaseIdentity, IdentitySnapshot, RetainedSegment, RetainedUnit,
    };

    fn identity() -> DatabaseIdentity {
        DatabaseIdentity {
            database_id: *b"replica-status!!",
            primary_generation: 1,
        }
    }

    fn unit(lsn: u64) -> RetainedUnit {
        RetainedUnit {
            tx_id: 1,
            record_type: 4,
            lsn,
            data: lsn.to_le_bytes().to_vec(),
        }
    }

    fn write_identity(data_dir: &Path) {
        let snapshot = IdentitySnapshot::from_identity(identity(), 1);
        write_identity_snapshot(data_dir, &snapshot).unwrap();
    }

    fn write_segment(data_dir: &Path, start_lsn: u64, end_lsn: u64) {
        let units = (start_lsn..=end_lsn).map(unit).collect();
        let segment = RetainedSegment::new(identity().segment_identity(), units).unwrap();
        write_segment_atomic(&retained_segments_dir(data_dir), &segment).unwrap();
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
    fn acknowledgement_advances_cursor_and_clears_lag() {
        let dir = tempfile::tempdir().unwrap();
        write_identity(dir.path());
        write_segment(dir.path(), 1, 5);
        write_segment(dir.path(), 6, 10);
        crate::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 0)).unwrap();

        let ack = acknowledge_replica_apply_at(dir.path(), "replica-a", 10, 10, 200).unwrap();

        assert_eq!(ack.previous_applied_lsn, 0);
        assert_eq!(ack.applied_lsn, 10);
        assert!(ack.advanced);
        assert_eq!(ack.status.repair_action, SyncRepairAction::None);
        assert!(!ack.status.stale);
        assert_eq!(ack.status.lag_lsn, Some(0));
        assert_eq!(ack.status.lag_bytes, Some(0));
        assert_eq!(minimum_retained_lsn(dir.path()).unwrap(), Some(11));

        let cursor = read_replica_cursors(dir.path())
            .unwrap()
            .into_iter()
            .find(|cursor| cursor.replica_id == "replica-a")
            .unwrap();
        assert_eq!(cursor.applied_lsn, 10);
        assert_eq!(cursor.updated_unix_secs, 200);
    }

    #[test]
    fn stale_status_reports_pull_with_lag_bytes() {
        let dir = tempfile::tempdir().unwrap();
        write_identity(dir.path());
        write_segment(dir.path(), 1, 5);
        write_segment(dir.path(), 6, 10);
        crate::upsert_replica_cursor(dir.path(), active_cursor("replica-a", 5, 100)).unwrap();

        let status = replica_sync_status_at(dir.path(), "replica-a", 10, 500).unwrap();

        assert!(status.stale);
        assert_eq!(status.repair_action, SyncRepairAction::Pull);
        assert_eq!(status.servable_lsn, Some(10));
        assert_eq!(status.unarchived_lsn, Some(0));
        assert_eq!(status.lag_lsn, Some(5));
        assert!(status.lag_bytes.unwrap() > 0);
        assert!(status.lag_ms.unwrap() > 0);
        assert_eq!(status.last_sync_error, None);
    }

    #[test]
    fn stale_status_reports_pull_for_contiguous_archived_prefix() {
        let dir = tempfile::tempdir().unwrap();
        write_identity(dir.path());
        write_segment(dir.path(), 1, 8);
        crate::upsert_replica_cursor(dir.path(), active_cursor("replica-a", 5, 100)).unwrap();

        let status = replica_sync_status_at(dir.path(), "replica-a", 10, 500).unwrap();

        assert!(status.stale);
        assert_eq!(status.repair_action, SyncRepairAction::Pull);
        assert_eq!(status.servable_lsn, Some(8));
        assert_eq!(status.unarchived_lsn, Some(2));
        assert_eq!(status.lag_lsn, Some(5));
        assert!(status.lag_bytes.unwrap() > 0);
        assert_eq!(status.last_sync_error, None);
    }

    #[test]
    fn stale_status_reports_await_archive_when_primary_is_ahead_of_retained_tail() {
        let dir = tempfile::tempdir().unwrap();
        write_identity(dir.path());
        crate::upsert_replica_cursor(dir.path(), active_cursor("replica-a", 5, 100)).unwrap();

        let status = replica_sync_status_at(dir.path(), "replica-a", 10, 500).unwrap();

        assert!(status.stale);
        assert_eq!(status.repair_action, SyncRepairAction::AwaitArchive);
        assert_eq!(status.servable_lsn, Some(5));
        assert_eq!(status.unarchived_lsn, Some(5));
        assert_eq!(status.lag_lsn, Some(5));
        assert_eq!(status.lag_bytes, Some(0));
        assert!(status.last_sync_error.unwrap().contains("not yet archived"));
    }

    #[test]
    fn missing_retained_tail_reports_rebootstrap() {
        let dir = tempfile::tempdir().unwrap();
        write_identity(dir.path());
        crate::upsert_replica_cursor(dir.path(), active_cursor("replica-a", 5, 100)).unwrap();
        write_segment(dir.path(), 8, 10);

        let status = replica_sync_status_at(dir.path(), "replica-a", 10, 500).unwrap();

        assert!(status.stale);
        assert_eq!(status.repair_action, SyncRepairAction::Rebootstrap);
        assert_eq!(status.servable_lsn, None);
        assert_eq!(status.unarchived_lsn, None);
        assert_eq!(status.lag_bytes, None);
        assert!(status
            .last_sync_error
            .unwrap()
            .contains("rebootstrap required"));
    }

    #[test]
    fn acknowledgement_rejects_stale_and_inactive_cursors() {
        let dir = tempfile::tempdir().unwrap();
        write_identity(dir.path());
        write_segment(dir.path(), 1, 10);
        crate::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 5)).unwrap();

        let err = acknowledge_replica_apply_at(dir.path(), "replica-a", 4, 10, 100).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("behind primary cursor"));

        crate::retire_replica_cursor(dir.path(), "replica-a", 101).unwrap();
        let err = acknowledge_replica_apply_at(dir.path(), "replica-a", 6, 10, 102).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("rebootstrap required"));
    }

    #[test]
    fn acknowledgement_status_error_leaves_cursor_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        write_identity(dir.path());
        write_segment(dir.path(), 1, 10);
        crate::upsert_replica_cursor(dir.path(), active_cursor("replica-a", 5, 100)).unwrap();
        fs::remove_file(crate::sync_state_dir(dir.path()).join(crate::IDENTITY_FILE)).unwrap();

        let err = acknowledge_replica_apply_at(dir.path(), "replica-a", 6, 10, 200).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let cursor = read_replica_cursors(dir.path())
            .unwrap()
            .into_iter()
            .find(|cursor| cursor.replica_id == "replica-a")
            .unwrap();
        assert_eq!(cursor.applied_lsn, 5);
        assert_eq!(cursor.updated_unix_secs, 100);
    }
}
