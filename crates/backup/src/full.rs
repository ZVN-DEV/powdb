use crate::manifest::{BackupManifest, FileEntry};
use powdb_storage::catalog::Catalog;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Take a consistent full snapshot of `catalog`'s data dir into `dest`.
///
/// Consistency model: `checkpoint()` flushes every dirty heap page + index
/// and truncates the WAL, producing a clean-shutdown image. We then copy the
/// durable files. The brief write-quiesce is the duration of the checkpoint,
/// held by the caller's `&mut` borrow.
pub fn full_backup(catalog: &mut Catalog, dest: &Path) -> io::Result<BackupManifest> {
    catalog.checkpoint()?;
    let source_lsn = catalog.max_lsn();
    let src = catalog.data_dir().to_path_buf();
    std::fs::create_dir_all(dest)?;

    let mut files = Vec::new();
    for entry in std::fs::read_dir(&src)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        // Durable state only: catalog + heaps + indexes. wal.log was just
        // truncated; manifest.json is ours.
        let is_durable = name == "catalog.bin" || name.ends_with(".heap") || name.ends_with(".idx");
        if !is_durable {
            continue;
        }
        let bytes = std::fs::read(entry.path())?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        std::fs::write(dest.join(&name), &bytes)?;
        files.push(FileEntry {
            name,
            len: bytes.len() as u64,
            blake3_hex: hash,
        });
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));

    let manifest = BackupManifest {
        format_version: BackupManifest::FORMAT_VERSION,
        created_unix_secs: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        source_lsn,
        files,
    };
    manifest.write(dest)?;
    Ok(manifest)
}
