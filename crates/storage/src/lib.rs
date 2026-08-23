//! PowDB storage engine: catalog, tables, heap files, B+tree indexes, and the
//! write-ahead log.
//!
//! # Public API boundary
//!
//! `btree`, `disk`, `format`, `heap`, `page`, `row`, and `wal` are marked
//! `#[doc(hidden)]`. They stay `pub` and code that imports them still compiles:
//! the attribute hides a module from the generated documentation without
//! restricting access to it. What it withdraws is the implication that these
//! are an interface. They are `pub` because sibling crates in this workspace
//! link against them, and they carry no compatibility promise.
//!
//! `row` is the clearest case. It is the raw on-disk row encoding, which the
//! `powdb-query` executor decodes and patches in place on its fast paths, so it
//! has to be reachable across the crate boundary. It also moves whenever the
//! storage format moves, and that is a format-version event governed by
//! `docs/FORMAT.md`, not a crate-API event.
//!
//! The supported entry points are the `powdb` facade crate and the `Engine`
//! API in `powdb-query`. See `docs/STABILITY.md` for what a version bump
//! promises.

#[doc(hidden)]
pub mod btree;
pub mod catalog;
pub mod dir_lock;
#[doc(hidden)]
pub mod disk;
pub mod error;
#[doc(hidden)]
pub mod format;
#[doc(hidden)]
pub mod heap;
#[doc(hidden)]
pub mod page;
pub mod pj1;
#[doc(hidden)]
pub mod row;
pub mod stored_json_path;
pub mod table;
pub mod types;
pub mod view;
#[doc(hidden)]
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

/// Validate that `data_dir` is an existing directory suitable for read-only
/// snapshot serving, **without mutating it**. Unlike [`create_data_dir_secure`]
/// this never creates the directory and never calls `set_permissions`: a
/// read-only open must leave the directory byte-for-byte unchanged. It only
/// confirms the path exists and is a directory; the actual read access is then
/// proven by opening the catalog and heap files read-only.
pub fn validate_data_dir_read_only(data_dir: &Path) -> io::Result<()> {
    let meta = std::fs::metadata(data_dir)?;
    if !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", data_dir.display()),
        ));
    }
    Ok(())
}
