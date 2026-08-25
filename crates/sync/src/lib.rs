//! Sync substrate for PowDB.
//!
//! This crate is intentionally below the query layer. Its first responsibility is
//! retaining durable WAL records as immutable replication-unit segments so future
//! embedded replicas and read replicas can bootstrap from snapshot + tail without
//! making `powdb-query` distribution-aware.

pub mod apply;
pub mod checkpoint;
pub mod error;
pub mod metadata;
pub mod replica;
pub mod retention;
pub mod segment;

pub use apply::{
    apply_retained_tail, apply_retained_units_chunk, seed_retained_apply_boundary,
    validate_v1_retained_tail_applyable, validate_v1_retained_units_applyable,
    RetainedTailApplySummary,
};
pub use checkpoint::{
    archive_wal_records_for_identity, checkpoint_preserving_retained_segments_if_enabled,
    checkpoint_with_retained_segments, open_preserving_retained_segments, retained_segments_dir,
    SyncCatalog, RETAINED_SEGMENTS_DIR,
};
pub use error::SyncError;
pub use metadata::{
    minimum_retained_lsn, open_or_create_identity, read_identity, read_identity_snapshot,
    read_identity_snapshot_if_exists, read_replica_cursors, register_bootstrap_cursor,
    retire_replica_cursor, sync_state_dir, upsert_replica_cursor, write_identity_snapshot,
    write_replica_cursors, DatabaseIdentity, IdentitySnapshot, ReplicaCursor, IDENTITY_FILE,
    REPLICA_CURSORS_FILE, SYNC_METADATA_FORMAT_VERSION, SYNC_STATE_DIR,
};
pub use replica::{
    acknowledge_replica_apply, replica_sync_status, ReplicaApplyAckSummary, ReplicaSyncStatus,
    SyncRepairAction,
};
pub use retention::{
    prune_retained_segments, prune_retained_segments_for_cursors,
    prune_retained_segments_with_policy, RetentionGcSummary, RetentionPressurePolicy,
    RetentionPressureSummary,
};
pub use segment::{
    list_segment_files, read_segment_file, read_units_since, read_units_through,
    retained_tail_progress, segment_file_name, validate_retained_tail_available,
    write_segment_atomic, RetainedSegment, RetainedTailAvailability, RetainedTailProgress,
    RetainedUnit, SegmentFile, SegmentIdentity, RETAINED_SEGMENT_FORMAT_VERSION,
};
