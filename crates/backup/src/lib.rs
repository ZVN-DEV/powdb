//! Physical backup, restore, and point-in-time recovery for PowDB.
//!
//! Supports full snapshots ([`full_backup`]) and incremental (differential)
//! backups against a full base ([`incremental_backup`]). Heap files use
//! page-LSN deltas; B+tree index files are copied as opaque whole artifacts.
//! Restore rebuilds a data directory from a full backup ([`restore()`]), or
//! chains a full base plus ordered increments for coarse point-in-time
//! recovery ([`restore_chain`]). [`RestoreSyncMode`] controls whether a
//! restored copy strips, preserves, or forks the source's sync identity, and
//! [`bootstrap_replica_from_full_backup`] seeds a sync replica from a backup.
pub mod bootstrap;
pub mod full;
pub mod incremental;
pub mod manifest;
pub mod restore;
mod secure;
pub use bootstrap::{bootstrap_replica_from_full_backup, ReplicaBootstrapSummary};
pub use full::full_backup;
pub use incremental::{incremental_backup, restore_chain, restore_chain_with_sync_mode};
pub use manifest::{
    BackupManifest, ChangedFile, FileEntry, IncrementManifest, SyncSnapshotMetadata,
};
pub use restore::{restore, restore_with_sync_mode, RestoreSyncMode};
