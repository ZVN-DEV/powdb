use crate::manifest::{
    active_durable_file_names, current_sync_snapshot_metadata, BackupManifest, FileEntry,
};
use powdb_storage::catalog::Catalog;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Take a consistent full snapshot of `catalog`'s data dir into `dest`.
///
/// Consistency model: checkpoint flushes every dirty heap page + index and
/// truncates the WAL, producing a clean-shutdown image. If sync identity exists,
/// the checkpoint first archives retained WAL records into `powdb-sync`
/// segments. We then copy the durable files. The brief write-quiesce is the
/// duration of the checkpoint, held by the caller's `&mut` borrow.
pub fn full_backup(catalog: &mut Catalog, dest: &Path) -> io::Result<BackupManifest> {
    powdb_sync::checkpoint_preserving_retained_segments_if_enabled(catalog)?;
    let source_lsn = catalog.max_lsn();
    let catalog_version = catalog.active_catalog_version();
    let src = catalog.data_dir().to_path_buf();
    let sync = current_sync_snapshot_metadata(&src, source_lsn, catalog_version)?;
    crate::secure::create_dir_secure(dest)?;

    let mut files = Vec::new();
    for name in active_durable_file_names(catalog) {
        let source_path = src.join(&name);
        if !source_path.exists() {
            // `catalog.lsn` is absent in pristine databases with no durable
            // statement boundary yet. Every metadata-referenced heap/index is
            // required and a missing one must fail closed.
            if name == powdb_storage::catalog::CATALOG_LSN_FILE {
                continue;
            }
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("catalog references missing durable file {name}"),
            ));
        }
        if !source_path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("catalog durable path is not a file: {name}"),
            ));
        }
        let bytes = std::fs::read(source_path)?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        crate::secure::write_file_secure(&dest.join(&name), &bytes)?;
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
            .map(|d| d.as_secs())
            .unwrap_or(0),
        source_lsn,
        catalog_version,
        sync,
        files,
    };
    manifest.write(dest)?;
    Ok(manifest)
}
