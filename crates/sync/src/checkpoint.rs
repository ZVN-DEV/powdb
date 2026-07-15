use std::io;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

use powdb_storage::catalog::Catalog;
use powdb_storage::wal::WalRecord;

use crate::metadata::{open_or_create_identity, read_identity};
use crate::segment::{
    read_segment_file, segment_file_name, write_segment_atomic, RetainedSegment, RetainedUnit,
    SegmentIdentity,
};
use crate::DatabaseIdentity;

pub const RETAINED_SEGMENTS_DIR: &str = "retained";

pub struct SyncCatalog {
    catalog: Catalog,
}

impl Deref for SyncCatalog {
    type Target = Catalog;

    fn deref(&self) -> &Self::Target {
        &self.catalog
    }
}

impl DerefMut for SyncCatalog {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.catalog
    }
}

impl Drop for SyncCatalog {
    fn drop(&mut self) {
        let _ = checkpoint_preserving_retained_segments_if_enabled(&mut self.catalog);
    }
}

pub fn retained_segments_dir(data_dir: &Path) -> PathBuf {
    crate::sync_state_dir(data_dir).join(RETAINED_SEGMENTS_DIR)
}

/// Open a catalog and, if sync identity exists, archive replayed WAL records
/// before recovery truncates `wal.log`.
pub fn open_preserving_retained_segments(data_dir: &Path) -> io::Result<SyncCatalog> {
    let catalog = match read_identity(data_dir) {
        Ok(identity) => Catalog::open_with_wal_archive(data_dir, move |dir, records| {
            archive_wal_records_for_identity(dir, identity, records)
        }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Catalog::open(data_dir),
        Err(err) => Err(err),
    }?;
    Ok(SyncCatalog { catalog })
}

/// Checkpoint a catalog with sync enabled, creating database identity if this
/// is the first sync-aware operation.
pub fn checkpoint_with_retained_segments(catalog: &mut Catalog) -> io::Result<()> {
    let data_dir = catalog.data_dir().to_path_buf();
    let identity = open_or_create_identity(&data_dir)?;
    catalog.checkpoint_with_wal_archive(move |dir, records| {
        archive_wal_records_for_identity(dir, identity, records)
    })
}

/// Checkpoint normally unless sync identity already exists, in which case WAL
/// records are archived into retained segments before truncation.
pub fn checkpoint_preserving_retained_segments_if_enabled(catalog: &mut Catalog) -> io::Result<()> {
    let data_dir = catalog.data_dir().to_path_buf();
    match read_identity(&data_dir) {
        Ok(identity) => catalog.checkpoint_with_wal_archive(move |dir, records| {
            archive_wal_records_for_identity(dir, identity, records)
        }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => catalog.checkpoint(),
        Err(err) => Err(err),
    }
}

pub fn archive_wal_records_for_identity(
    data_dir: &Path,
    identity: DatabaseIdentity,
    records: &[WalRecord],
) -> io::Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    // Stamp the published segment with the database's *active* catalog version
    // (read from the on-disk catalog), not this binary's compile-time maximum.
    // A database that has not activated v6 keeps stamping v5, so a v0.12 replica
    // that states catalog_version 5 still matches. The catalog file is persisted
    // before any archive runs (create + activation both persist it), so the
    // on-disk version is authoritative here.
    let active_catalog_version = powdb_storage::catalog::read_active_catalog_version(data_dir)?;
    let segment_identity = SegmentIdentity::with_catalog_version(
        identity.database_id,
        identity.primary_generation,
        active_catalog_version,
    );
    let units: Vec<RetainedUnit> = records.iter().map(RetainedUnit::from).collect();
    let segment = RetainedSegment::new(segment_identity, units)?;
    write_segment_idempotent(&retained_segments_dir(data_dir), &segment).map(|_| ())
}

fn write_segment_idempotent(dir: &Path, segment: &RetainedSegment) -> io::Result<PathBuf> {
    match write_segment_atomic(dir, segment) {
        Ok(path) => Ok(path),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let path = dir.join(segment_file_name(segment.start_lsn, segment.end_lsn));
            let existing = read_segment_file(&path)?;
            if existing == *segment {
                Ok(path)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "retained segment already exists with different contents",
                ))
            }
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};
    use powdb_storage::wal::WalRecordType;

    fn schema_t() -> Schema {
        Schema {
            table_name: "T".into(),
            columns: vec![ColumnDef {
                name: "id".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            }],
        }
    }

    #[test]
    fn sync_checkpoint_archives_wal_before_truncate() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::create(dir.path()).unwrap();
        catalog.create_table(schema_t()).unwrap();
        catalog.insert("T", &vec![Value::Int(1)]).unwrap();
        catalog.sync_wal().unwrap();

        let identity = open_or_create_identity(dir.path()).unwrap();
        checkpoint_with_retained_segments(&mut catalog).unwrap();

        let units = crate::read_units_since(
            &retained_segments_dir(dir.path()),
            identity.segment_identity(),
            0,
            100,
        )
        .unwrap();
        assert!(
            units.iter().any(|unit| unit.lsn == catalog.max_lsn()),
            "retained segment should include the checkpointed high-water LSN"
        );
        assert!(
            units
                .iter()
                .any(|unit| unit.record_type == WalRecordType::Commit as u8),
            "sync-aware checkpoint should archive autocommit boundary markers"
        );
        assert!(
            Catalog::open(dir.path()).is_ok(),
            "plain open is safe after sync-aware checkpoint has emptied the WAL"
        );
    }

    #[test]
    fn plain_checkpoint_refuses_to_truncate_sync_enabled_wal() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::create(dir.path()).unwrap();
        catalog.create_table(schema_t()).unwrap();
        catalog.insert("T", &vec![Value::Int(1)]).unwrap();
        catalog.sync_wal().unwrap();
        open_or_create_identity(dir.path()).unwrap();

        let err = catalog.checkpoint().unwrap_err();
        assert!(
            format!("{err}").contains("without a WAL archive hook"),
            "plain checkpoint must fail closed for sync-enabled history, got: {err}"
        );

        checkpoint_with_retained_segments(&mut catalog).unwrap();
    }

    #[test]
    fn sync_open_archives_replayed_wal_before_recovery_truncate() {
        let dir = tempfile::tempdir().unwrap();
        let identity = {
            let mut catalog = Catalog::create(dir.path()).unwrap();
            catalog.create_table(schema_t()).unwrap();
            catalog.insert("T", &vec![Value::Int(1)]).unwrap();
            catalog.sync_wal().unwrap();
            open_or_create_identity(dir.path()).unwrap()
        };

        let mut catalog = open_preserving_retained_segments(dir.path()).unwrap();
        assert_eq!(catalog.scan("T").unwrap().count(), 1);

        let units = crate::read_units_since(
            &retained_segments_dir(dir.path()),
            identity.segment_identity(),
            0,
            100,
        )
        .unwrap();
        assert!(!units.is_empty());

        catalog.insert("T", &vec![Value::Int(2)]).unwrap();
        catalog.sync_wal().unwrap();
        checkpoint_preserving_retained_segments_if_enabled(&mut catalog).unwrap();
    }

    #[test]
    fn sync_open_drop_archives_later_writes_before_plain_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let identity = {
            let mut catalog = Catalog::create(dir.path()).unwrap();
            catalog.create_table(schema_t()).unwrap();
            catalog.insert("T", &vec![Value::Int(1)]).unwrap();
            catalog.sync_wal().unwrap();
            open_or_create_identity(dir.path()).unwrap()
        };

        {
            let mut catalog = open_preserving_retained_segments(dir.path()).unwrap();
            assert_eq!(catalog.scan("T").unwrap().count(), 1);
            catalog.insert("T", &vec![Value::Int(2)]).unwrap();
            catalog.sync_wal().unwrap();
        }

        let catalog = Catalog::open(dir.path()).unwrap();
        assert_eq!(catalog.scan("T").unwrap().count(), 2);
        drop(catalog);

        let units = crate::read_units_since(
            &retained_segments_dir(dir.path()),
            identity.segment_identity(),
            0,
            100,
        )
        .unwrap();
        assert!(
            units.iter().any(|unit| unit.lsn >= 2),
            "sync-aware drop must archive writes made after sync-aware open"
        );
    }

    #[test]
    fn plain_open_refuses_sync_enabled_wal_without_truncating() {
        let dir = tempfile::tempdir().unwrap();
        let identity = {
            let mut catalog = Catalog::create(dir.path()).unwrap();
            catalog.create_table(schema_t()).unwrap();
            catalog.insert("T", &vec![Value::Int(1)]).unwrap();
            catalog.sync_wal().unwrap();
            open_or_create_identity(dir.path()).unwrap()
        };

        let err = match Catalog::open(dir.path()) {
            Ok(_) => panic!("plain recovery must fail closed for sync-enabled WAL history"),
            Err(err) => err,
        };
        assert!(
            format!("{err}").contains("without a WAL archive hook"),
            "plain recovery must fail closed for sync-enabled WAL history, got: {err}"
        );

        let mut catalog = open_preserving_retained_segments(dir.path()).unwrap();
        assert_eq!(catalog.scan("T").unwrap().count(), 1);
        let units = crate::read_units_since(
            &retained_segments_dir(dir.path()),
            identity.segment_identity(),
            0,
            100,
        )
        .unwrap();
        assert!(
            !units.is_empty(),
            "plain recovery failure must leave WAL history available for sync-aware open"
        );

        catalog.insert("T", &vec![Value::Int(2)]).unwrap();
        catalog.sync_wal().unwrap();
        checkpoint_preserving_retained_segments_if_enabled(&mut catalog).unwrap();
    }
}
