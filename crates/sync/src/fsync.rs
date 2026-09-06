//! Directory fsync, in one place.
//!
//! Every durable write in this crate (identity, cursors, segments, retention)
//! has to fsync the containing directory before the rename it just did counts
//! as durable, so this used to be copy-pasted into three modules.

use std::io;
use std::path::Path;

/// Flush the directory entry itself, so a rename into `dir` survives a crash.
/// A no-op off Unix, where directories cannot be opened for fsync.
#[cfg(unix)]
pub(crate) fn fsync_dir(dir: &Path) -> io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// See the Unix variant: directories are not fsyncable here.
#[cfg(not(unix))]
pub(crate) fn fsync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}
