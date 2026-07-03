//! Backup / restore / PITR for PowDB. See
//! docs/design/2026-06-05-backup-pitr-sync-migrations-plan.md and
//! docs/superpowers/plans/2026-06-05-backup-pitr-sync-migrations.md.
pub mod bootstrap;
pub mod full;
pub mod incremental;
pub mod manifest;
pub mod restore;
pub use bootstrap::{bootstrap_replica_from_full_backup, ReplicaBootstrapSummary};
pub use full::full_backup;
pub use incremental::{incremental_backup, restore_chain, restore_chain_with_sync_mode};
pub use manifest::{
    BackupManifest, ChangedFile, FileEntry, IncrementManifest, SyncSnapshotMetadata,
};
pub use restore::{restore, restore_with_sync_mode, RestoreSyncMode};
