use std::fs;
use std::io;
use std::path::Path;

use crate::manifest::{BackupManifest, SyncSnapshotMetadata};
use crate::restore::{restore_with_sync_mode, RestoreSyncMode};
use powdb_storage::catalog::Catalog;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaBootstrapSummary {
    pub replica_id: String,
    pub snapshot_lsn: u64,
    pub remote_lsn: u64,
    pub retained_units_available: usize,
    pub retained_tail_start_lsn: Option<u64>,
    pub retained_tail_end_lsn: Option<u64>,
}

/// Restore a sync-enabled full backup into an empty replica path and pin the
/// primary-side retained history needed to catch that replica up.
///
/// This restores the snapshot and pins the primary-side cursor; local tail apply
/// is intentionally owned by `powdb-sync::apply_retained_tail` so bootstrap and
/// replica catch-up can be tested independently.
pub fn bootstrap_replica_from_full_backup(
    primary: &mut Catalog,
    backup_dir: &Path,
    replica_data_dir: &Path,
    replica_id: &str,
) -> io::Result<ReplicaBootstrapSummary> {
    let manifest = BackupManifest::read(backup_dir)?;
    let sync = manifest.sync.as_ref().ok_or_else(|| {
        invalid_input("replica bootstrap requires a sync-enabled full backup manifest")
    })?;
    let primary_data_dir = primary.data_dir().to_path_buf();
    let identity = validate_bootstrap_identity(&primary_data_dir, sync)?;

    powdb_sync::checkpoint_preserving_retained_segments_if_enabled(primary)?;
    let remote_lsn = primary.max_lsn();
    if remote_lsn < manifest.source_lsn {
        return Err(invalid_input(format!(
            "remote LSN {remote_lsn} is behind snapshot LSN {}",
            manifest.source_lsn
        )));
    }
    if powdb_sync::read_replica_cursors(&primary_data_dir)?
        .iter()
        .any(|cursor| cursor.replica_id == replica_id && cursor.active)
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "replica cursor is already active; retire it before bootstrap",
        ));
    }

    let availability = powdb_sync::validate_v1_retained_tail_applyable(
        &powdb_sync::retained_segments_dir(&primary_data_dir),
        identity.segment_identity(),
        manifest.source_lsn,
        remote_lsn,
    )?;
    restore_with_sync_mode(
        backup_dir,
        replica_data_dir,
        RestoreSyncMode::PreserveSyncIdentity,
    )?;
    if let Err(err) = powdb_sync::seed_retained_apply_boundary(
        replica_data_dir,
        identity.segment_identity(),
        manifest.source_lsn,
    ) {
        return cleanup_restored_replica(replica_data_dir, err);
    }
    if let Err(err) = powdb_sync::register_bootstrap_cursor(
        &primary_data_dir,
        replica_id,
        manifest.source_lsn,
        remote_lsn,
    ) {
        return cleanup_restored_replica(replica_data_dir, err);
    }

    Ok(ReplicaBootstrapSummary {
        replica_id: replica_id.to_string(),
        snapshot_lsn: manifest.source_lsn,
        remote_lsn,
        retained_units_available: availability.units_available,
        retained_tail_start_lsn: availability.first_lsn,
        retained_tail_end_lsn: availability.last_lsn,
    })
}

fn cleanup_restored_replica<T>(replica_data_dir: &Path, err: io::Error) -> io::Result<T> {
    let cleanup_result = fs::remove_dir_all(replica_data_dir);
    match cleanup_result {
        Ok(()) => Err(err),
        Err(cleanup_err) if cleanup_err.kind() == io::ErrorKind::NotFound => Err(err),
        Err(cleanup_err) => Err(io::Error::new(
            err.kind(),
            format!(
                "{err}; additionally failed to clean up restored replica directory {}: {cleanup_err}",
                replica_data_dir.display()
            ),
        )),
    }
}

fn validate_bootstrap_identity(
    primary_data_dir: &Path,
    sync: &SyncSnapshotMetadata,
) -> io::Result<powdb_sync::DatabaseIdentity> {
    let manifest_identity = sync.identity.identity()?;
    let primary_identity = powdb_sync::read_identity(primary_data_dir)?;
    if primary_identity != manifest_identity {
        return Err(invalid_input(
            "primary sync identity does not match bootstrap snapshot identity",
        ));
    }
    Ok(primary_identity)
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
