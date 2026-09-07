//! The names of the files a PowDB data directory holds.
//!
//! One list, in the crate that owns the directory layout. Everything that has
//! to enumerate a data directory (a backup snapshot, a restore, a census test)
//! reads it from here instead of spelling the names again: a file added to the
//! layout but missed by a second hand-maintained list is a file a restored
//! database comes up without.

/// The catalog: schemas, indexes, links, and the format version.
pub const CATALOG_FILE: &str = "catalog.bin";

/// Sidecar holding the last durable WAL LSN, so recovery knows how far the
/// catalog has caught up.
pub const CATALOG_LSN_FILE: &str = "catalog.lsn";

/// The materialized-view registry. A database restored without it serves
/// whatever rows the backing heaps happen to hold and cannot refresh a view.
pub const VIEW_REGISTRY_FILE: &str = "views.bin";

/// The user and role store (written by `powdb-auth`). A server restored
/// without it accepts unauthenticated connections.
pub const AUTH_STORE_FILE: &str = "auth.json";

/// The write-ahead log.
pub const WAL_FILE: &str = "wal.log";

/// The exclusive writer lock.
pub const WRITER_LOCK_FILE: &str = "LOCK";

/// Directory holding one file per live read-only reader.
pub const READERS_DIR: &str = "readers";

/// Durable files that no catalog entry points at, so nothing reconstructs
/// their names from metadata. They are snapshotted whenever they exist, and
/// their absence is normal: a database with no views, or one that was never
/// given users.
pub const UNREFERENCED_DURABLE_FILES: &[&str] = &[VIEW_REGISTRY_FILE, AUTH_STORE_FILE];

/// Files a snapshot deliberately leaves behind: the WAL (a checkpoint precedes
/// every backup, so a snapshot is already a clean-shutdown image) and the lock
/// artifacts, which describe the process that took the backup and mean nothing
/// in a restored copy.
pub const NON_SNAPSHOT_FILES: &[&str] = &[WAL_FILE, WRITER_LOCK_FILE, READERS_DIR];
