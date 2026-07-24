use powdb_storage::catalog::{
    expression_index_file_name, Catalog, IndexKeySource, CATALOG_LSN_FILE, CATALOG_VERSION,
    LEGACY_CATALOG_VERSION,
};
use powdb_storage::wal::WAL_FORMAT_VERSION;
use powdb_sync::IdentitySnapshot;
use powdb_sync::RETAINED_SEGMENT_FORMAT_VERSION;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io;
use std::path::Path;

fn legacy_catalog_version() -> u16 {
    LEGACY_CATALOG_VERSION
}

pub(crate) fn validate_catalog_version(version: u16) -> io::Result<()> {
    if !(LEGACY_CATALOG_VERSION..=CATALOG_VERSION).contains(&version) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported snapshot catalog format {version}"),
        ));
    }
    Ok(())
}

pub(crate) fn validate_catalog_transition(base: u16, next: u16) -> io::Result<()> {
    validate_catalog_version(base)?;
    validate_catalog_version(next)?;
    if next < base {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("catalog format cannot move backward from {base} to {next}"),
        ));
    }
    Ok(())
}

/// Durable files referenced by current catalog metadata.
///
/// Expression-index filenames are reconstructed only from persisted table and
/// index IDs. Stray files from a dropped or failed index are deliberately not
/// included in a new snapshot.
pub(crate) fn active_durable_file_names(catalog: &Catalog) -> BTreeSet<String> {
    let mut names = BTreeSet::from(["catalog.bin".to_string(), CATALOG_LSN_FILE.to_string()]);
    for table in catalog.list_tables() {
        names.insert(format!("{table}.heap"));
        if let Some(indexes) = catalog.index_metadata(table) {
            for index in indexes {
                match index.source {
                    IndexKeySource::Column { column } => {
                        names.insert(format!("{table}_{column}.idx"));
                    }
                    IndexKeySource::Expression { index_id, .. } => {
                        names.insert(expression_index_file_name(table, index_id));
                    }
                }
            }
        }
    }
    names
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub len: u64,
    pub blake3_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub format_version: u32,
    pub created_unix_secs: u64,
    /// The page-LSN high-water mark this backup is consistent at.
    pub source_lsn: u64,
    /// Active on-disk catalog format at the snapshot boundary.
    #[serde(default = "legacy_catalog_version")]
    pub catalog_version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncSnapshotMetadata>,
    pub files: Vec<FileEntry>,
}

impl BackupManifest {
    pub const FORMAT_VERSION: u32 = 1;
    pub const FILE_NAME: &'static str = "manifest.json";

    pub fn validate_version(&self) -> io::Result<()> {
        if self.format_version != Self::FORMAT_VERSION {
            return Err(io::Error::other(format!(
                "unsupported backup format {} (this build understands {})",
                self.format_version,
                Self::FORMAT_VERSION
            )));
        }
        validate_catalog_version(self.catalog_version)?;
        validate_sync_metadata(&self.sync, self.source_lsn, self.catalog_version)?;
        Ok(())
    }

    pub fn write(&self, dir: &Path) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        crate::secure::write_file_secure(&dir.join(Self::FILE_NAME), &json)
    }

    pub fn read(dir: &Path) -> io::Result<Self> {
        let bytes = std::fs::read(dir.join(Self::FILE_NAME))?;
        let m: BackupManifest = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        m.validate_version()?;
        Ok(m)
    }
}

/// A file that changed (relative to a base) in an incremental backup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChangedFile {
    /// A small/unpaged file copied whole (e.g. catalog.bin).
    Whole {
        name: String,
        len: u64,
        blake3_hex: String,
    },
    /// A paged heap file: only pages whose LSN > base.source_lsn.
    /// The sidecar delta file `<name>.delta` holds, for each listed page index
    /// in order, a 4-byte LE page index followed by PAGE_SIZE bytes.
    Pages {
        name: String,
        /// page count of the file at increment time
        total_pages: u32,
        /// which pages are in the delta (ascending)
        page_indices: Vec<u32>,
        /// `<name>.delta`
        delta_file: String,
        delta_len: u64,
        delta_blake3_hex: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncSnapshotMetadata {
    pub identity: IdentitySnapshot,
    pub source_lsn: u64,
    pub catalog_blake3_hex: String,
    pub wal_format_version: u16,
    pub catalog_version: u16,
    pub retained_segment_format_version: u16,
}

impl SyncSnapshotMetadata {
    pub fn current(
        identity: IdentitySnapshot,
        source_lsn: u64,
        catalog_blake3_hex: String,
        catalog_version: u16,
    ) -> Self {
        Self {
            identity,
            source_lsn,
            catalog_blake3_hex,
            wal_format_version: WAL_FORMAT_VERSION,
            catalog_version,
            retained_segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
        }
    }

    pub fn validate_against_source_lsn(&self, source_lsn: u64) -> io::Result<()> {
        self.identity.validate()?;
        if self.source_lsn != source_lsn {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "sync snapshot source_lsn {} does not match manifest source_lsn {}",
                    self.source_lsn, source_lsn
                ),
            ));
        }
        if self.catalog_blake3_hex.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sync snapshot catalog hash must be non-empty",
            ));
        }
        if self.wal_format_version != WAL_FORMAT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported sync snapshot WAL format {}",
                    self.wal_format_version
                ),
            ));
        }
        validate_catalog_version(self.catalog_version)?;
        if self.retained_segment_format_version != RETAINED_SEGMENT_FORMAT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported retained segment format {}",
                    self.retained_segment_format_version
                ),
            ));
        }
        Ok(())
    }
}

/// Manifest for an incremental (page-LSN diff) backup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncrementManifest {
    /// reuse FORMAT_VERSION = 1
    pub format_version: u32,
    pub created_unix_secs: u64,
    /// the source_lsn of the base (full or prior increment) this builds on
    pub base_source_lsn: u64,
    /// high-water mark after this increment (== catalog.max_lsn())
    pub source_lsn: u64,
    /// Active on-disk catalog format after applying this increment.
    #[serde(default = "legacy_catalog_version")]
    pub catalog_version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncSnapshotMetadata>,
    pub changed: Vec<ChangedFile>,
}

impl IncrementManifest {
    pub const FORMAT_VERSION: u32 = 1;
    pub const FILE_NAME: &'static str = "increment.json";

    pub fn validate_version(&self) -> io::Result<()> {
        if self.format_version != Self::FORMAT_VERSION {
            return Err(io::Error::other(format!(
                "unsupported increment format {} (this build understands {})",
                self.format_version,
                Self::FORMAT_VERSION
            )));
        }
        validate_catalog_version(self.catalog_version)?;
        validate_sync_metadata(&self.sync, self.source_lsn, self.catalog_version)?;
        Ok(())
    }

    pub fn write(&self, dir: &Path) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        crate::secure::write_file_secure(&dir.join(Self::FILE_NAME), &json)
    }

    pub fn read(dir: &Path) -> io::Result<Self> {
        let bytes = std::fs::read(dir.join(Self::FILE_NAME))?;
        let m: IncrementManifest = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        m.validate_version()?;
        Ok(m)
    }
}

pub(crate) fn current_sync_snapshot_metadata(
    data_dir: &Path,
    source_lsn: u64,
    catalog_version: u16,
) -> io::Result<Option<SyncSnapshotMetadata>> {
    let Some(identity) = powdb_sync::read_identity_snapshot_if_exists(data_dir)? else {
        return Ok(None);
    };
    let catalog_bytes = std::fs::read(data_dir.join("catalog.bin"))?;
    let catalog_blake3_hex = blake3::hash(&catalog_bytes).to_hex().to_string();
    Ok(Some(SyncSnapshotMetadata::current(
        identity,
        source_lsn,
        catalog_blake3_hex,
        catalog_version,
    )))
}

fn validate_sync_metadata(
    metadata: &Option<SyncSnapshotMetadata>,
    source_lsn: u64,
    catalog_version: u16,
) -> io::Result<()> {
    if let Some(metadata) = metadata {
        metadata.validate_against_source_lsn(source_lsn)?;
        if metadata.catalog_version != catalog_version {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sync snapshot catalog format does not match manifest catalog format",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manifest_round_trips_and_rejects_bad_version() {
        let m = BackupManifest {
            format_version: BackupManifest::FORMAT_VERSION,
            created_unix_secs: 1_700_000_000,
            source_lsn: 42,
            catalog_version: LEGACY_CATALOG_VERSION,
            sync: None,
            files: vec![FileEntry {
                name: "catalog.bin".into(),
                len: 10,
                blake3_hex: "ab".into(),
            }],
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: BackupManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source_lsn, 42);
        assert_eq!(back.files.len(), 1);

        let mut bad = m.clone();
        bad.format_version = 999;
        assert!(
            bad.validate_version().is_err(),
            "unknown format must be rejected"
        );
    }
}
