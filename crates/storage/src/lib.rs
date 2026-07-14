pub mod btree;
pub mod catalog;
pub mod dir_lock;
pub mod disk;
pub mod error;
pub mod format;
pub mod heap;
pub mod page;
pub mod pj1;
pub mod row;
pub mod table;
pub mod types;
pub mod view;
pub mod wal;

use std::io;
use std::path::Path;

/// Create a database data directory with owner-only permissions.
///
/// On Unix the directory is created (and, if it already exists, tightened) to
/// `0700` so that the heap files, WAL, and indexes it contains — which hold all
/// row data — are not world- or group-readable under a typical umask. This is
/// the same posture PostgreSQL enforces on its data directory: locking the
/// directory down protects every file inside it regardless of each file's own
/// mode. On non-Unix platforms this is a plain recursive create.
pub fn create_data_dir_secure(data_dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(data_dir)?;
        // `recursive(true)` does not re-apply the mode to a directory that
        // already existed, so tighten an existing dir explicitly.
        std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(data_dir)
    }
}
