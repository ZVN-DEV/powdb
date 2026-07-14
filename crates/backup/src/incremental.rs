use crate::manifest::{
    active_durable_file_names, current_sync_snapshot_metadata, validate_catalog_transition,
    BackupManifest, ChangedFile, IncrementManifest,
};
use crate::restore::{
    apply_restore_sync_mode, ensure_empty_dir, validate_backup_file_name, validate_delta_file_name,
    verify_and_copy_full, RestoreSyncMode,
};
use powdb_storage::catalog::{Catalog, CATALOG_LSN_FILE};
use powdb_storage::page::{page_lsn, PAGE_SIZE};
use std::io;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_paged(name: &str) -> bool {
    name.ends_with(".heap")
}

/// Take an incremental backup: copy only heap pages whose page LSN is greater
/// than `base.source_lsn`, plus every changed non-heap file. B+tree `.idx`
/// files are opaque whole-file artifacts and are never interpreted as heap
/// pages. Builds on `base` (a full backup or a prior increment's effective
/// state).
pub fn incremental_backup(
    catalog: &mut Catalog,
    base: &BackupManifest,
    dest: &Path,
) -> io::Result<IncrementManifest> {
    base.validate_version()?;
    powdb_sync::checkpoint_preserving_retained_segments_if_enabled(catalog)?;
    let source_lsn = catalog.max_lsn();
    let catalog_version = catalog.active_catalog_version();
    validate_catalog_transition(base.catalog_version, catalog_version)?;
    let src = catalog.data_dir().to_path_buf();
    let sync = current_sync_snapshot_metadata(&src, source_lsn, catalog_version)?;
    if !sync_metadata_builds_on_same_history(base.sync.as_ref(), sync.as_ref()) {
        return Err(io::Error::other(
            "sync identity changed between base and incremental backup",
        ));
    }
    std::fs::create_dir_all(dest)?;

    let mut changed: Vec<ChangedFile> = Vec::new();

    // Stable iteration order so manifests are deterministic.
    let entries = active_durable_file_names(catalog);

    for name in entries {
        let path = src.join(&name);
        if !path.exists() {
            if name == CATALOG_LSN_FILE {
                continue;
            }
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("catalog references missing durable file {name}"),
            ));
        }
        let bytes = std::fs::read(&path)?;

        if !is_paged(&name) || bytes.len() % PAGE_SIZE != 0 {
            // Whole-file path: catalog metadata, every B+tree, or a
            // defensively handled non-page-aligned heap file.
            let hash = blake3::hash(&bytes).to_hex().to_string();
            let unchanged = base
                .files
                .iter()
                .any(|f| f.name == name && f.blake3_hex == hash);
            if unchanged {
                continue;
            }
            std::fs::write(dest.join(&name), &bytes)?;
            changed.push(ChangedFile::Whole {
                name,
                len: bytes.len() as u64,
                blake3_hex: hash,
            });
            continue;
        }

        // Paged file: diff by page LSN.
        let total_pages = (bytes.len() / PAGE_SIZE) as u32;
        let mut page_indices: Vec<u32> = Vec::new();
        let mut delta: Vec<u8> = Vec::new();
        for i in 0..total_pages {
            let start = i as usize * PAGE_SIZE;
            let chunk = &bytes[start..start + PAGE_SIZE];
            if page_lsn(chunk) > base.source_lsn {
                page_indices.push(i);
                delta.extend_from_slice(&i.to_le_bytes());
                delta.extend_from_slice(chunk);
            }
        }
        if page_indices.is_empty() {
            // Nothing changed for this file; omit entirely.
            continue;
        }
        let delta_file = format!("{name}.delta");
        std::fs::write(dest.join(&delta_file), &delta)?;
        let delta_blake3_hex = blake3::hash(&delta).to_hex().to_string();
        changed.push(ChangedFile::Pages {
            name,
            total_pages,
            page_indices,
            delta_file,
            delta_len: delta.len() as u64,
            delta_blake3_hex,
        });
    }

    let manifest = IncrementManifest {
        format_version: IncrementManifest::FORMAT_VERSION,
        created_unix_secs: now_secs(),
        base_source_lsn: base.source_lsn,
        source_lsn,
        catalog_version,
        sync,
        changed,
    };
    manifest.write(dest)?;
    Ok(manifest)
}

/// Rebuild a data dir from a full base backup plus an ordered chain of
/// increments. Verifies chain continuity (each increment's `base_source_lsn`
/// must equal the running high-water LSN) and blake3-checks every file/delta,
/// then validates the result by reopening the catalog.
///
/// Coarse PITR = choosing which prefix of the chain to pass here.
pub fn restore_chain(full_dir: &Path, increment_dirs: &[&Path], dest: &Path) -> io::Result<()> {
    restore_chain_with_sync_mode(
        full_dir,
        increment_dirs,
        dest,
        RestoreSyncMode::StripSyncIdentity,
    )
}

/// Rebuild a data dir from a full backup plus an ordered chain of increments
/// with explicit sync identity semantics.
pub fn restore_chain_with_sync_mode(
    full_dir: &Path,
    increment_dirs: &[&Path],
    dest: &Path,
    sync_mode: RestoreSyncMode,
) -> io::Result<()> {
    ensure_empty_dir(dest)?;

    // 1. Lay down the full base.
    let full_manifest = BackupManifest::read(full_dir)?;
    verify_and_copy_full(&full_manifest, full_dir, dest)?;
    let mut running_lsn = full_manifest.source_lsn;
    let mut running_catalog_version = full_manifest.catalog_version;
    let mut running_sync = full_manifest.sync.clone();

    // 2. Apply each increment in order.
    for inc_dir in increment_dirs {
        let inc = IncrementManifest::read(inc_dir)?;
        if inc.base_source_lsn != running_lsn {
            return Err(io::Error::other(format!(
                "increment chain broken: expected base lsn {}, increment built on {}",
                running_lsn, inc.base_source_lsn
            )));
        }
        if !sync_metadata_builds_on_same_history(full_manifest.sync.as_ref(), inc.sync.as_ref()) {
            return Err(io::Error::other(
                "increment sync identity does not match full backup identity",
            ));
        }
        validate_catalog_transition(running_catalog_version, inc.catalog_version)?;
        for cf in &inc.changed {
            match cf {
                ChangedFile::Whole {
                    name,
                    len: _,
                    blake3_hex,
                } => {
                    validate_backup_file_name(name)?;
                    let bytes = std::fs::read(inc_dir.join(name))?;
                    let hash = blake3::hash(&bytes).to_hex().to_string();
                    if &hash != blake3_hex {
                        return Err(io::Error::other(format!(
                            "integrity check failed for {name}: blake3 mismatch (increment is corrupt)"
                        )));
                    }
                    std::fs::write(dest.join(name), &bytes)?;
                }
                ChangedFile::Pages {
                    name,
                    total_pages,
                    page_indices,
                    delta_file,
                    delta_len: _,
                    delta_blake3_hex,
                } => {
                    validate_delta_file_name(delta_file, name)?;
                    let delta = std::fs::read(inc_dir.join(delta_file))?;
                    let hash = blake3::hash(&delta).to_hex().to_string();
                    if &hash != delta_blake3_hex {
                        return Err(io::Error::other(format!(
                            "integrity check failed for {delta_file}: blake3 mismatch (increment is corrupt)"
                        )));
                    }
                    apply_page_delta(&dest.join(name), *total_pages, page_indices, &delta)?;
                }
            }
        }
        running_lsn = inc.source_lsn;
        running_catalog_version = inc.catalog_version;
        running_sync = inc.sync.clone();
    }

    apply_restore_sync_mode(running_sync.as_ref(), dest, sync_mode)?;

    // 3. Validate the reconstructed DB opens (LSN invariant).
    let cat = Catalog::open(dest)?;
    if cat.active_catalog_version() != running_catalog_version {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "restored catalog format does not match increment manifest",
        ));
    }
    drop(cat);
    Ok(())
}

fn sync_metadata_builds_on_same_history(
    base: Option<&crate::manifest::SyncSnapshotMetadata>,
    next: Option<&crate::manifest::SyncSnapshotMetadata>,
) -> bool {
    match (base, next) {
        (None, None) => true,
        (Some(base), Some(next)) => {
            base.identity == next.identity
                && base.wal_format_version == next.wal_format_version
                && base.retained_segment_format_version == next.retained_segment_format_version
        }
        _ => false,
    }
}

/// Open/extend `path` to `total_pages * PAGE_SIZE` bytes, then write each
/// page record from the delta at its target offset. Pages not in the delta are
/// left untouched (already correct from the base / prior increments).
fn apply_page_delta(
    path: &Path,
    total_pages: u32,
    page_indices: &[u32],
    delta: &[u8],
) -> io::Result<()> {
    let record_len = 4 + PAGE_SIZE;
    let expected = page_indices
        .len()
        .checked_mul(record_len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "delta length overflow"))?;
    if delta.len() != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "delta for {} has length {} but {} page records expected {}",
                path.display(),
                delta.len(),
                page_indices.len(),
                expected
            ),
        ));
    }

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let target_len = total_pages as u64 * PAGE_SIZE as u64;
    if file.metadata()?.len() < target_len {
        file.set_len(target_len)?;
    }

    let mut off = 0usize;
    for expected_idx in page_indices {
        if *expected_idx >= total_pages {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "increment manifest page index {expected_idx} is outside {total_pages} pages for {}",
                    path.display()
                ),
            ));
        }
        let idx = u32::from_le_bytes([delta[off], delta[off + 1], delta[off + 2], delta[off + 3]]);
        if idx != *expected_idx {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "delta page index {idx} does not match manifest page index {expected_idx} for {}",
                    path.display()
                ),
            ));
        }
        let page = &delta[off + 4..off + 4 + PAGE_SIZE];
        file.seek(SeekFrom::Start(idx as u64 * PAGE_SIZE as u64))?;
        file.write_all(page)?;
        off += record_len;
    }
    file.flush()?;
    Ok(())
}
