use crate::manifest::BackupManifest;
use powdb_storage::catalog::Catalog;
use std::io;
use std::path::Path;

/// Rebuild a data dir from a full backup. Verifies every file's blake3 against
/// the manifest BEFORE writing it, then opens the result through
/// `Catalog::open` (which sets `next_lsn = max_page_lsn + 1` — the v0.4.3
/// LSN-reset fix) to validate that the restored database actually opens.
pub fn restore(backup_dir: &Path, dest_data_dir: &Path) -> io::Result<()> {
    let manifest = BackupManifest::read(backup_dir)?;
    // Refuse a non-empty destination: a stale wal.log left there would replay
    // onto the restored data on Catalog::open and corrupt it. Restore requires
    // a fresh or empty directory. A nonexistent or empty dest is fine.
    if dest_data_dir.exists() && dest_data_dir.read_dir()?.next().is_some() {
        return Err(io::Error::other(format!(
            "restore destination {} is not empty; restore requires a fresh or empty directory",
            dest_data_dir.display()
        )));
    }
    std::fs::create_dir_all(dest_data_dir)?;
    for f in &manifest.files {
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
    // Validate: opening must succeed and reset next_lsn correctly.
    let cat = Catalog::open(dest_data_dir)?;
    drop(cat);
    Ok(())
}
