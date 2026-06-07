//! Backup / restore / PITR for PowDB. See
//! docs/design/2026-06-05-backup-pitr-sync-migrations-plan.md and
//! docs/superpowers/plans/2026-06-05-backup-pitr-sync-migrations.md.
pub mod full;
pub mod incremental;
pub mod manifest;
pub mod restore;
pub use full::full_backup;
pub use incremental::{incremental_backup, restore_chain};
pub use manifest::{BackupManifest, ChangedFile, FileEntry, IncrementManifest};
pub use restore::restore;
