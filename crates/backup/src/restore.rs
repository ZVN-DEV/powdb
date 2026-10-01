use crate::manifest::{BackupManifest, SyncSnapshotMetadata, UNREFERENCED_DURABLE_FILES};
use powdb_storage::catalog::{Catalog, CATALOG_LSN_FILE};
use std::fs;
use std::io;
use std::io::{Read, Seek, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

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
    match fs::symlink_metadata(dest_data_dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(io::Error::other(format!(
                    "restore destination {} is a symlink; restore requires a real fresh or empty directory",
                    dest_data_dir.display()
                )));
            }
            if !metadata.file_type().is_dir() {
                return Err(io::Error::other(format!(
                    "restore destination {} is not a directory",
                    dest_data_dir.display()
                )));
            }
            if dest_data_dir.read_dir()?.next().is_some() {
                return Err(io::Error::other(format!(
                    "restore destination {} is not empty; restore requires a fresh or empty directory",
                    dest_data_dir.display()
                )));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
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
    let durable_name = name == powdb_storage::data_dir::CATALOG_FILE
        || name == CATALOG_LSN_FILE
        || UNREFERENCED_DURABLE_FILES.contains(&name)
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

pub(crate) fn validate_backup_file_entry(
    backup_dir: &Path,
    name: &str,
    len: u64,
    expected_blake3_hex: &str,
) -> io::Result<()> {
    let mut file = open_manifest_entry(backup_dir, name, len)?;
    let mut hasher = blake3::Hasher::new();
    let mut read_len = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        read_len += n as u64;
        hasher.update(&buf[..n]);
    }
    if read_len != len {
        return Err(io::Error::other(format!(
            "integrity check failed for {name}: read length {read_len} differs from manifest {len}"
        )));
    }
    let actual = hasher.finalize().to_hex().to_string();
    if actual != expected_blake3_hex {
        return Err(io::Error::other(format!(
            "integrity check failed for {name}: blake3 mismatch (backup is corrupt)"
        )));
    }
    Ok(())
}

fn open_manifest_entry(backup_dir: &Path, name: &str, len: u64) -> io::Result<fs::File> {
    validate_backup_file_name(name)?;
    let path = backup_dir.join(name);
    let mut file = fs::File::open(&path)?;
    let handle_metadata = file.metadata()?;
    let path_metadata = fs::symlink_metadata(&path)?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.file_type().is_file()
        || !handle_metadata.file_type().is_file()
    {
        return Err(io::Error::other(format!(
            "backup entry '{name}' is not a regular file"
        )));
    }
    verify_opened_file_identity(name, &handle_metadata, &path_metadata)?;
    if handle_metadata.len() != len {
        return Err(io::Error::other(format!(
            "integrity check failed for {name}: length {} differs from manifest {len}",
            handle_metadata.len()
        )));
    }
    file.rewind()?;
    Ok(file)
}

#[cfg(unix)]
fn verify_opened_file_identity(
    name: &str,
    opened: &fs::Metadata,
    path: &fs::Metadata,
) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if opened.dev() != path.dev() || opened.ino() != path.ino() {
        return Err(io::Error::other(format!(
            "backup entry '{name}' changed while opening"
        )));
    }
    Ok(())
}

// The race-hardening identity check is implemented for Unix platforms (Linux
// and macOS), where backup/restore currently enforces owner-only modes. On
// non-Unix targets we still require a regular file and stream from one opened
// handle, but std exposes no stable dev/inode equivalent to compare.
#[cfg(not(unix))]
fn verify_opened_file_identity(
    _name: &str,
    _opened: &fs::Metadata,
    _path: &fs::Metadata,
) -> io::Result<()> {
    Ok(())
}

fn copy_backup_file_verified(
    backup_dir: &Path,
    dest_data_dir: &Path,
    name: &str,
    len: u64,
    expected_blake3_hex: &str,
) -> io::Result<()> {
    let mut input = open_manifest_entry(backup_dir, name, len)?;
    let final_path = dest_data_dir.join(name);
    if final_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "restore destination file {} already exists",
                final_path.display()
            ),
        ));
    }
    let tmp_path = dest_data_dir.join(format!(
        ".{name}.tmp.{}.{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let result = (|| {
        let mut output = crate::secure::create_new_file_secure(&tmp_path)?;
        let mut hasher = blake3::Hasher::new();
        let mut copied_len = 0u64;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            copied_len += n as u64;
            hasher.update(&buf[..n]);
            output.write_all(&buf[..n])?;
        }
        output.flush()?;
        drop(output);
        if copied_len != len {
            return Err(io::Error::other(format!(
                "integrity check failed for {name}: read length {copied_len} differs from manifest {len}"
            )));
        }
        let actual = hasher.finalize().to_hex().to_string();
        if actual != expected_blake3_hex {
            return Err(io::Error::other(format!(
                "integrity check failed for {name}: blake3 mismatch (backup is corrupt)"
            )));
        }
        fs::rename(&tmp_path, &final_path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
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
        if let Err(error) =
            copy_backup_file_verified(backup_dir, dest_data_dir, &f.name, f.len, &f.blake3_hex)
        {
            warn!(file = %f.name, %error, "backup file validation failed while restoring");
            return Err(error);
        }
    }
    Ok(())
}

pub(crate) fn verify_restored_sync_catalog(
    sync: &SyncSnapshotMetadata,
    dest_data_dir: &Path,
) -> io::Result<()> {
    let catalog_bytes = std::fs::read(dest_data_dir.join(powdb_storage::data_dir::CATALOG_FILE))?;
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
    info!(
        dest = %dest_data_dir.display(),
        source_lsn = manifest.source_lsn,
        files = manifest.files.len(),
        ?sync_mode,
        "restored a full backup"
    );
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
            "views.bin",
            "auth.json",
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
