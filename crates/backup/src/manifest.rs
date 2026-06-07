use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

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
        Ok(())
    }

    pub fn write(&self, dir: &Path) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        std::fs::write(dir.join(Self::FILE_NAME), json)
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
    /// A paged file (.heap/.idx): only pages whose LSN > base.source_lsn.
    /// The sidecar delta file `<name>.delta` holds, for each listed page index
    /// in order, a 4-byte LE page index followed by PAGE_SIZE bytes.
    Pages {
        name: String,
        /// page count of the file at increment time
        total_pages: u32,
        /// which pages are in the delta (ascending)
        page_indices: Vec<u32>,
        /// "<name>.delta"
        delta_file: String,
        delta_len: u64,
        delta_blake3_hex: String,
    },
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
        Ok(())
    }

    pub fn write(&self, dir: &Path) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        std::fs::write(dir.join(Self::FILE_NAME), json)
    }

    pub fn read(dir: &Path) -> io::Result<Self> {
        let bytes = std::fs::read(dir.join(Self::FILE_NAME))?;
        let m: IncrementManifest = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        m.validate_version()?;
        Ok(m)
    }
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
