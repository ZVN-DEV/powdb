//! Owner-only filesystem helpers for backup, increment, and restore output.
//!
//! A backup directory holds a byte-for-byte copy of every heap page, every
//! index, and the catalog: exactly the data the live data directory protects
//! with `0700` via [`powdb_storage::create_data_dir_secure`]. Plain
//! `create_dir_all` / `fs::write` would apply the process umask instead
//! (typically `0755` directories and `0644` files), so `powdb-cli backup
//! /var/backups/db` on a shared host would publish the whole database to every
//! local user. Every write in this crate goes through the helpers below.
//!
//! On non-Unix platforms these are plain creates/writes, matching
//! `create_data_dir_secure`'s behavior there.

use std::fs::File;
use std::io;
use std::path::Path;

/// File mode for backup output: owner read/write only.
#[cfg(unix)]
const FILE_MODE: u32 = 0o600;

/// Create (or tighten) a backup/restore output directory to owner-only.
pub(crate) fn create_dir_secure(dir: &Path) -> io::Result<()> {
    powdb_storage::create_data_dir_secure(dir)
}

/// Open `path` for writing, creating it with owner-only permissions and
/// tightening it if it already exists.
fn open_file_secure(path: &Path, truncate: bool) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    if truncate {
        options.truncate(true);
    } else {
        options.truncate(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(FILE_MODE);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        // `mode` only applies when the file is created, so tighten an existing
        // file (e.g. a re-run into the same backup directory) explicitly.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(FILE_MODE))?;
    }
    Ok(file)
}

/// Write `bytes` to `path` with owner-only permissions. Drop-in replacement for
/// `std::fs::write`.
pub(crate) fn write_file_secure(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut file = open_file_secure(path, true)?;
    file.write_all(bytes)?;
    file.flush()
}

/// Open a page-addressed file for random-access writes (the incremental
/// restore delta path) with owner-only permissions, preserving existing
/// content.
pub(crate) fn open_paged_file_secure(path: &Path) -> io::Result<File> {
    open_file_secure(path, false)
}
