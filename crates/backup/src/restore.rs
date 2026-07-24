use crate::manifest::{BackupManifest, SyncSnapshotMetadata};
use powdb_storage::catalog::{Catalog, CATALOG_LSN_FILE};
use std::io;
use std::path::Path;

/// Controls how restore writes sync identity metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreSyncMode {
    /// Restore the durable database files but do not recreate sync identity.
    /// This is the default for ordinary backup restore so restored data dirs
    /// remain safe for the plain PowDB engine lifecycle.
    StripSyncIdentity,
    /// Restore keeps the source database identity. This is the right mode for
    /// disaster recovery of the same sync lineage through sync-aware
    /// open/checkpoint paths.
    PreserveSyncIdentity,
    /// Restore mints a fresh database identity after verifying the source sync
    /// snapshot metadata. Use this for clone/fork restores that must not share
    /// replication lineage with the source database.
    ForkWithNewSyncIdentity,
}

/// Refuse a non-empty destination: a stale wal.log left there would replay
/// onto the restored data on `Catalog::open` and corrupt it. Restore requires
/// a fresh or empty directory. A nonexistent or empty dest is fine.
pub(crate) fn ensure_empty_dir(dest_data_dir: &Path) -> io::Result<()> {
    if dest_data_dir.exists() && dest_data_dir.read_dir()?.next().is_some() {
        return Err(io::Error::other(format!(
            "restore destination {} is not empty; restore requires a fresh or empty directory",
            dest_data_dir.display()
        )));
    }
    crate::secure::create_dir_secure(dest_data_dir)?;
    Ok(())
}

fn is_plain_manifest_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains(':')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

pub(crate) fn validate_backup_file_name(name: &str) -> io::Result<()> {
    let durable_name = name == "catalog.bin"
        || name == CATALOG_LSN_FILE
        || (name.ends_with(".heap") && name.len() > ".heap".len())
        || (name.ends_with(".idx") && name.len() > ".idx".len())
        || (name.ends_with(".eidx") && name.len() > ".eidx".len());
    if !is_plain_manifest_name(name) || !durable_name {
        return Err(io::Error::other(format!(
            "invalid backup manifest file name: {name}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_delta_file_name(delta_file: &str, data_file: &str) -> io::Result<()> {
    validate_backup_file_name(data_file)?;
    if !data_file.ends_with(".heap") {
        return Err(io::Error::other(format!(
            "invalid backup manifest delta target file name: {data_file}"
        )));
    }
    let expected = format!("{data_file}.delta");
    if !is_plain_manifest_name(delta_file) || delta_file != expected {
        return Err(io::Error::other(format!(
            "invalid backup manifest delta file name: {delta_file}"
        )));
    }
    Ok(())
}

/// Verify every file in a full backup's manifest against its blake3, then copy
/// it into `dest`. Does NOT open the catalog or write sync identity metadata —
/// callers decide when to validate. Assumes `dest` already exists.
pub(crate) fn verify_and_copy_full(
    manifest: &BackupManifest,
    backup_dir: &Path,
    dest_data_dir: &Path,
) -> io::Result<()> {
    for f in &manifest.files {
        validate_backup_file_name(&f.name)?;
        let bytes = std::fs::read(backup_dir.join(&f.name))?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        if hash != f.blake3_hex {
            return Err(io::Error::other(format!(
                "integrity check failed for {}: blake3 mismatch (backup is corrupt)",
                f.name
            )));
        }
        crate::secure::write_file_secure(&dest_data_dir.join(&f.name), &bytes)?;
    }
    Ok(())
}

pub(crate) fn verify_restored_sync_catalog(
    sync: &SyncSnapshotMetadata,
    dest_data_dir: &Path,
) -> io::Result<()> {
    let catalog_bytes = std::fs::read(dest_data_dir.join("catalog.bin"))?;
    let catalog_hash = blake3::hash(&catalog_bytes).to_hex().to_string();
    if catalog_hash != sync.catalog_blake3_hex {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "restored catalog hash does not match sync snapshot metadata",
        ));
    }
    Ok(())
}

pub(crate) fn apply_restore_sync_mode(
    sync: Option<&SyncSnapshotMetadata>,
    dest_data_dir: &Path,
    sync_mode: RestoreSyncMode,
) -> io::Result<()> {
    match sync_mode {
        RestoreSyncMode::StripSyncIdentity => {
            if let Some(sync) = sync {
                verify_restored_sync_catalog(sync, dest_data_dir)?;
            }
        }
        RestoreSyncMode::PreserveSyncIdentity => {
            let sync = sync.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "preserve sync identity restore requires sync snapshot metadata",
                )
            })?;
            verify_restored_sync_catalog(sync, dest_data_dir)?;
            powdb_sync::write_identity_snapshot(dest_data_dir, &sync.identity)?;
        }
        RestoreSyncMode::ForkWithNewSyncIdentity => {
            if let Some(sync) = sync {
                verify_restored_sync_catalog(sync, dest_data_dir)?;
            }
            let _ = powdb_sync::open_or_create_identity(dest_data_dir)?;
        }
    }
    Ok(())
}

/// Rebuild a data dir from a full backup. Verifies every file's blake3 against
/// the manifest BEFORE writing it, then opens the result through
/// `Catalog::open` (which sets `next_lsn = max_page_lsn + 1` — the v0.4.3
/// LSN-reset fix) to validate that the restored database actually opens.
pub fn restore(backup_dir: &Path, dest_data_dir: &Path) -> io::Result<()> {
    restore_with_sync_mode(
        backup_dir,
        dest_data_dir,
        RestoreSyncMode::StripSyncIdentity,
    )
}

/// Rebuild a data dir from a full backup with explicit sync identity semantics.
/// `restore` calls this with `StripSyncIdentity` so ordinary restored data dirs
/// stay safe for the plain PowDB engine lifecycle.
pub fn restore_with_sync_mode(
    backup_dir: &Path,
    dest_data_dir: &Path,
    sync_mode: RestoreSyncMode,
) -> io::Result<()> {
    let manifest = BackupManifest::read(backup_dir)?;
    ensure_empty_dir(dest_data_dir)?;
    verify_and_copy_full(&manifest, backup_dir, dest_data_dir)?;
    apply_restore_sync_mode(manifest.sync.as_ref(), dest_data_dir, sync_mode)?;
    // Validate: opening must succeed and reset next_lsn correctly.
    let cat = Catalog::open(dest_data_dir)?;
    if cat.active_catalog_version() != manifest.catalog_version {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "restored catalog format does not match backup manifest",
        ));
    }
    drop(cat);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_file_names_reject_path_traversal() {
        for bad in [
            "../catalog.bin",
            "/tmp/catalog.bin",
            "nested/catalog.bin",
            "nested\\catalog.bin",
            "C:\\tmp\\catalog.bin",
            "",
            ".",
            "..",
            ".heap",
            ".idx",
            ".eidx",
            "wal.log",
        ] {
            assert!(
                validate_backup_file_name(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn manifest_file_names_accept_only_durable_root_files() {
        for good in [
            "catalog.bin",
            CATALOG_LSN_FILE,
            "User.heap",
            "User_email.idx",
            "User_7.eidx",
        ] {
            validate_backup_file_name(good).unwrap();
        }
    }

    #[test]
    fn delta_file_must_match_paged_file_name() {
        validate_delta_file_name("User.heap.delta", "User.heap").unwrap();
        assert!(validate_delta_file_name("User_email.idx.delta", "User_email.idx").is_err());
        assert!(validate_delta_file_name("../User.heap.delta", "User.heap").is_err());
        assert!(validate_delta_file_name("Other.heap.delta", "User.heap").is_err());
        assert!(validate_delta_file_name("catalog.bin.delta", "catalog.bin").is_err());
        assert!(validate_delta_file_name("catalog.lsn.delta", CATALOG_LSN_FILE).is_err());
    }
}
