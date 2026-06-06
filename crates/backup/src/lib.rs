//! Backup / restore / PITR for PowDB. See
//! docs/design/2026-06-05-backup-pitr-sync-migrations-plan.md and
//! docs/superpowers/plans/2026-06-05-backup-pitr-sync-migrations.md.
pub mod manifest;
pub use manifest::{BackupManifest, FileEntry};
