use crate::manifest::BackupManifest;
use powdb_storage::catalog::Catalog;
use std::io;
use std::path::Path;

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
    std::fs::create_dir_all(dest_data_dir)?;
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
        || (name.ends_with(".heap") && name.len() > ".heap".len())
        || (name.ends_with(".idx") && name.len() > ".idx".len());
    if !is_plain_manifest_name(name) || !durable_name {
        return Err(io::Error::other(format!(
            "invalid backup manifest file name: {name}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_delta_file_name(delta_file: &str, data_file: &str) -> io::Result<()> {
    validate_backup_file_name(data_file)?;
    if !(data_file.ends_with(".heap") || data_file.ends_with(".idx")) {
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
/// it into `dest`. Does NOT open the catalog — callers (full restore, chain
/// restore) decide when to validate. Assumes `dest` already exists.
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
        std::fs::write(dest_data_dir.join(&f.name), &bytes)?;
    }
    Ok(())
}

/// Rebuild a data dir from a full backup. Verifies every file's blake3 against
/// the manifest BEFORE writing it, then opens the result through
/// `Catalog::open` (which sets `next_lsn = max_page_lsn + 1` — the v0.4.3
/// LSN-reset fix) to validate that the restored database actually opens.
pub fn restore(backup_dir: &Path, dest_data_dir: &Path) -> io::Result<()> {
    let manifest = BackupManifest::read(backup_dir)?;
    ensure_empty_dir(dest_data_dir)?;
    verify_and_copy_full(&manifest, backup_dir, dest_data_dir)?;
    // Validate: opening must succeed and reset next_lsn correctly.
    let cat = Catalog::open(dest_data_dir)?;
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
        for good in ["catalog.bin", "User.heap", "User_email.idx"] {
            validate_backup_file_name(good).unwrap();
        }
    }

    #[test]
    fn delta_file_must_match_paged_file_name() {
        validate_delta_file_name("User.heap.delta", "User.heap").unwrap();
        assert!(validate_delta_file_name("../User.heap.delta", "User.heap").is_err());
        assert!(validate_delta_file_name("Other.heap.delta", "User.heap").is_err());
        assert!(validate_delta_file_name("catalog.bin.delta", "catalog.bin").is_err());
    }
}
