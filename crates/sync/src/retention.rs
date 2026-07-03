use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::checkpoint::retained_segments_dir;
use crate::metadata::{
    minimum_retained_lsn, read_replica_cursors_unlocked, replace_replica_cursors_unlocked,
    with_cursor_metadata_lock,
};
use crate::segment::{list_segment_files, read_segment_file, SegmentIdentity};
use crate::{DatabaseIdentity, ReplicaCursor};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionGcSummary {
    pub retain_from_lsn: Option<u64>,
    pub segments_deleted: usize,
    pub bytes_deleted: u64,
    pub oldest_retained_lsn: Option<u64>,
    pub newest_retained_lsn: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPressurePolicy {
    pub max_retained_bytes: Option<u64>,
    pub inactive_cursor_expiry_secs: Option<u64>,
    pub operator_retain_from_lsn: Option<u64>,
    pub now_unix_secs: u64,
}

impl RetentionPressurePolicy {
    pub fn conservative(now_unix_secs: u64) -> Self {
        Self {
            max_retained_bytes: None,
            inactive_cursor_expiry_secs: None,
            operator_retain_from_lsn: None,
            now_unix_secs,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPressureSummary {
    pub gc: RetentionGcSummary,
    pub retained_bytes_before: u64,
    pub retained_bytes_after: u64,
    pub max_retained_bytes: Option<u64>,
    pub max_retained_bytes_exceeded: bool,
    pub inactive_cursors_retired: Vec<String>,
    pub override_cursors_retired: Vec<String>,
}

#[derive(Debug, Clone)]
struct RetentionCandidate {
    path: PathBuf,
    start_lsn: u64,
    end_lsn: u64,
    len: u64,
    delete: bool,
}

/// Prune retained-unit segments that no active replica can still need.
///
/// `minimum_retained_lsn` returns the smallest next-required LSN across active
/// replicas, so a segment is deletable only when its entire range ends before
/// that boundary. Segments crossing the boundary are kept intact rather than
/// rewritten in place.
pub fn prune_retained_segments_for_cursors(
    data_dir: &Path,
    identity: DatabaseIdentity,
) -> io::Result<RetentionGcSummary> {
    with_cursor_metadata_lock(data_dir, || {
        let retain_from_lsn = minimum_retained_lsn(data_dir)?;
        prune_retained_segments(
            &retained_segments_dir(data_dir),
            identity.segment_identity(),
            retain_from_lsn,
        )
    })
}

/// Prune retained segments with explicit production pressure controls.
///
/// This policy never strands an active cursor merely because retained bytes are
/// over budget. A byte limit is reported as pressure after safe cursor-based
/// pruning. History can be released beyond active cursors only through explicit
/// inactive-cursor expiry or an operator retain-boundary override; cursors that
/// fall behind such a boundary are retired and must rebootstrap.
pub fn prune_retained_segments_with_policy(
    data_dir: &Path,
    identity: DatabaseIdentity,
    policy: RetentionPressurePolicy,
) -> io::Result<RetentionPressureSummary> {
    if matches!(policy.operator_retain_from_lsn, Some(0)) {
        return Err(invalid_input(
            "operator retention boundary must be non-zero",
        ));
    }
    let expected_identity = identity.segment_identity();
    expected_identity.validate()?;

    with_cursor_metadata_lock(data_dir, || {
        let segment_dir = retained_segments_dir(data_dir);
        let retained_bytes_before = retained_segment_bytes(&segment_dir)?;
        let mut cursors = read_replica_cursors_unlocked(data_dir)?;
        let inactive_cursors_retired = retire_inactive_cursors(&mut cursors, policy);
        let override_cursors_retired = retire_cursors_behind_override(&mut cursors, policy)?;

        let cursor_boundary = minimum_retained_lsn_for_cursors(&cursors)?;
        let retain_from_lsn = match (policy.operator_retain_from_lsn, cursor_boundary) {
            (Some(override_lsn), Some(cursor_lsn)) => Some(override_lsn.max(cursor_lsn)),
            (Some(override_lsn), None) => Some(override_lsn),
            (None, cursor_lsn) => cursor_lsn,
        };
        let candidates = retention_candidates(&segment_dir, expected_identity, retain_from_lsn)?;

        if !inactive_cursors_retired.is_empty() || !override_cursors_retired.is_empty() {
            replace_replica_cursors_unlocked(data_dir, cursors.clone())?;
        }

        let gc = delete_retention_candidates(&segment_dir, retain_from_lsn, &candidates)?;
        let retained_bytes_after = retained_segment_bytes(&segment_dir)?;
        let max_retained_bytes_exceeded = policy
            .max_retained_bytes
            .map(|max| retained_bytes_after > max)
            .unwrap_or(false);

        Ok(RetentionPressureSummary {
            gc,
            retained_bytes_before,
            retained_bytes_after,
            max_retained_bytes: policy.max_retained_bytes,
            max_retained_bytes_exceeded,
            inactive_cursors_retired,
            override_cursors_retired,
        })
    })
}

/// Prune retained-unit segments below an explicit retention boundary.
///
/// Passing `None` is a conservative no-op. This prevents accidental history
/// loss when no active replica cursor exists yet; callers that want to drop all
/// history need a separate explicit policy.
pub fn prune_retained_segments(
    dir: &Path,
    expected_identity: SegmentIdentity,
    retain_from_lsn: Option<u64>,
) -> io::Result<RetentionGcSummary> {
    let candidates = retention_candidates(dir, expected_identity, retain_from_lsn)?;
    delete_retention_candidates(dir, retain_from_lsn, &candidates)
}

fn retention_candidates(
    dir: &Path,
    expected_identity: SegmentIdentity,
    retain_from_lsn: Option<u64>,
) -> io::Result<Vec<RetentionCandidate>> {
    expected_identity.validate()?;
    let files = list_segment_files(dir)?;
    let mut candidates = Vec::with_capacity(files.len());

    for file in files {
        let segment = read_segment_file(&file.path)?;
        if segment.start_lsn != file.start_lsn || segment.end_lsn != file.end_lsn {
            return Err(invalid_data(format!(
                "retained segment filename range {}-{} does not match header range {}-{}",
                file.start_lsn, file.end_lsn, segment.start_lsn, segment.end_lsn
            )));
        }
        if segment.identity != expected_identity {
            return Err(invalid_data(
                "retained segment identity does not match expected database history",
            ));
        }
        let len = fs::metadata(&file.path)?.len();
        let delete = retain_from_lsn
            .map(|boundary| file.end_lsn < boundary)
            .unwrap_or(false);
        candidates.push(RetentionCandidate {
            path: file.path,
            start_lsn: file.start_lsn,
            end_lsn: file.end_lsn,
            len,
            delete,
        });
    }

    validate_retained_tail(retain_from_lsn, &candidates)?;
    Ok(candidates)
}

fn delete_retention_candidates(
    dir: &Path,
    retain_from_lsn: Option<u64>,
    candidates: &[RetentionCandidate],
) -> io::Result<RetentionGcSummary> {
    let mut segments_deleted = 0usize;
    let mut bytes_deleted = 0u64;
    for candidate in candidates.iter().filter(|candidate| candidate.delete) {
        fs::remove_file(&candidate.path)?;
        segments_deleted += 1;
        bytes_deleted = bytes_deleted
            .checked_add(candidate.len)
            .ok_or_else(|| io::Error::other("retained segment deleted byte count overflow"))?;
    }
    if segments_deleted > 0 {
        fsync_dir(dir)?;
    }

    let retained = candidates.iter().filter(|candidate| !candidate.delete);
    let mut oldest_retained_lsn = None;
    let mut newest_retained_lsn = None;
    for candidate in retained {
        oldest_retained_lsn = Some(
            oldest_retained_lsn
                .map(|current: u64| current.min(candidate.start_lsn))
                .unwrap_or(candidate.start_lsn),
        );
        newest_retained_lsn = Some(
            newest_retained_lsn
                .map(|current: u64| current.max(candidate.end_lsn))
                .unwrap_or(candidate.end_lsn),
        );
    }

    Ok(RetentionGcSummary {
        retain_from_lsn,
        segments_deleted,
        bytes_deleted,
        oldest_retained_lsn,
        newest_retained_lsn,
    })
}

fn retire_inactive_cursors(
    cursors: &mut [ReplicaCursor],
    policy: RetentionPressurePolicy,
) -> Vec<String> {
    let Some(expiry_secs) = policy.inactive_cursor_expiry_secs else {
        return Vec::new();
    };
    let mut retired = Vec::new();
    for cursor in cursors.iter_mut().filter(|cursor| cursor.active) {
        let age = policy
            .now_unix_secs
            .saturating_sub(cursor.updated_unix_secs);
        if age < expiry_secs {
            continue;
        }
        cursor.active = false;
        cursor.updated_unix_secs = policy.now_unix_secs;
        retired.push(cursor.replica_id.clone());
    }
    retired
}

fn retire_cursors_behind_override(
    cursors: &mut [ReplicaCursor],
    policy: RetentionPressurePolicy,
) -> io::Result<Vec<String>> {
    let Some(boundary) = policy.operator_retain_from_lsn else {
        return Ok(Vec::new());
    };
    let mut retired = Vec::new();
    for cursor in cursors.iter_mut().filter(|cursor| cursor.active) {
        if cursor.next_required_lsn()? >= boundary {
            continue;
        }
        cursor.active = false;
        cursor.updated_unix_secs = policy.now_unix_secs;
        retired.push(cursor.replica_id.clone());
    }
    Ok(retired)
}

fn minimum_retained_lsn_for_cursors(cursors: &[ReplicaCursor]) -> io::Result<Option<u64>> {
    let mut min_lsn: Option<u64> = None;
    for cursor in cursors {
        if !cursor.active {
            continue;
        }
        let next_lsn = cursor.next_required_lsn()?;
        min_lsn = Some(match min_lsn {
            Some(current) => current.min(next_lsn),
            None => next_lsn,
        });
    }
    Ok(min_lsn)
}

fn retained_segment_bytes(dir: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    for file in list_segment_files(dir)? {
        let len = fs::metadata(file.path)?.len();
        total = total
            .checked_add(len)
            .ok_or_else(|| io::Error::other("retained segment byte count overflow"))?;
    }
    Ok(total)
}

fn validate_retained_tail(
    retain_from_lsn: Option<u64>,
    candidates: &[RetentionCandidate],
) -> io::Result<()> {
    let Some(boundary) = retain_from_lsn else {
        return Ok(());
    };

    let mut retained = candidates
        .iter()
        .filter(|candidate| candidate.end_lsn >= boundary);
    let Some(first) = retained.next() else {
        return Ok(());
    };

    if first.start_lsn > boundary {
        return Err(invalid_data(format!(
            "retained segment gap at retention boundary: expected LSN {}, found {}",
            boundary, first.start_lsn
        )));
    }

    let mut expected_next_lsn = first
        .end_lsn
        .checked_add(1)
        .ok_or_else(|| invalid_data("retained segment LSN overflow"))?;
    for candidate in retained {
        if candidate.start_lsn < expected_next_lsn {
            return Err(invalid_data(format!(
                "retained segment overlap after retention boundary: expected LSN {}, found {}",
                expected_next_lsn, candidate.start_lsn
            )));
        }
        if candidate.start_lsn > expected_next_lsn {
            return Err(invalid_data(format!(
                "retained segment gap after retention boundary: expected LSN {}, found {}",
                expected_next_lsn, candidate.start_lsn
            )));
        }
        expected_next_lsn = candidate
            .end_lsn
            .checked_add(1)
            .ok_or_else(|| invalid_data("retained segment LSN overflow"))?;
    }
    Ok(())
}

#[cfg(unix)]
fn fsync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{upsert_replica_cursor, ReplicaCursor};
    use crate::segment::{
        read_units_since, segment_file_name, write_segment_atomic, RetainedSegment, RetainedUnit,
    };

    fn database_identity() -> DatabaseIdentity {
        DatabaseIdentity {
            database_id: *b"0123456789abcdef",
            primary_generation: 3,
        }
    }

    fn unit(lsn: u64) -> RetainedUnit {
        RetainedUnit {
            tx_id: 1,
            record_type: 1,
            lsn,
            data: vec![lsn as u8],
        }
    }

    fn write_segment(dir: &Path, start_lsn: u64, end_lsn: u64) {
        let units = (start_lsn..=end_lsn).map(unit).collect();
        let segment = RetainedSegment::new(database_identity().segment_identity(), units).unwrap();
        write_segment_atomic(dir, &segment).unwrap();
    }

    #[test]
    fn prunes_only_segments_fully_below_active_cursor_boundary() {
        let data_dir = tempfile::tempdir().unwrap();
        let segment_dir = retained_segments_dir(data_dir.path());
        write_segment(&segment_dir, 1, 5);
        write_segment(&segment_dir, 6, 10);
        write_segment(&segment_dir, 11, 15);

        upsert_replica_cursor(data_dir.path(), ReplicaCursor::active("replica-a", 10)).unwrap();
        let summary = prune_retained_segments_for_cursors(data_dir.path(), database_identity())
            .expect("GC should succeed");

        assert_eq!(summary.retain_from_lsn, Some(11));
        assert_eq!(summary.segments_deleted, 2);
        assert!(summary.bytes_deleted > 0);
        assert_eq!(summary.oldest_retained_lsn, Some(11));
        assert_eq!(summary.newest_retained_lsn, Some(15));

        let remaining = list_segment_files(&segment_dir).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].start_lsn, 11);
        assert_eq!(remaining[0].end_lsn, 15);

        let units = read_units_since(
            &segment_dir,
            database_identity().segment_identity(),
            10,
            100,
        )
        .unwrap();
        let lsns: Vec<u64> = units.into_iter().map(|unit| unit.lsn).collect();
        assert_eq!(lsns, vec![11, 12, 13, 14, 15]);
    }

    #[test]
    fn keeps_segment_that_crosses_retention_boundary() {
        let data_dir = tempfile::tempdir().unwrap();
        let segment_dir = retained_segments_dir(data_dir.path());
        write_segment(&segment_dir, 1, 20);
        write_segment(&segment_dir, 21, 25);

        let summary = prune_retained_segments(
            &segment_dir,
            database_identity().segment_identity(),
            Some(11),
        )
        .unwrap();

        assert_eq!(summary.segments_deleted, 0);
        assert_eq!(summary.oldest_retained_lsn, Some(1));
        assert_eq!(summary.newest_retained_lsn, Some(25));
        assert_eq!(list_segment_files(&segment_dir).unwrap().len(), 2);
    }

    #[test]
    fn no_active_cursor_boundary_is_noop() {
        let data_dir = tempfile::tempdir().unwrap();
        let segment_dir = retained_segments_dir(data_dir.path());
        write_segment(&segment_dir, 1, 5);

        let summary =
            prune_retained_segments_for_cursors(data_dir.path(), database_identity()).unwrap();

        assert_eq!(summary.retain_from_lsn, None);
        assert_eq!(summary.segments_deleted, 0);
        assert_eq!(summary.oldest_retained_lsn, Some(1));
        assert_eq!(summary.newest_retained_lsn, Some(5));
        assert_eq!(list_segment_files(&segment_dir).unwrap().len(), 1);
    }

    #[test]
    fn corrupt_or_mismatched_segments_block_gc_before_deleting() {
        let data_dir = tempfile::tempdir().unwrap();
        let segment_dir = retained_segments_dir(data_dir.path());
        write_segment(&segment_dir, 1, 5);
        write_segment(&segment_dir, 11, 15);

        let segment = RetainedSegment::new(
            database_identity().segment_identity(),
            (21..=25).map(unit).collect(),
        )
        .unwrap();
        fs::write(
            segment_dir.join(segment_file_name(21, 26)),
            segment.to_bytes().unwrap(),
        )
        .unwrap();

        let err = prune_retained_segments(
            &segment_dir,
            database_identity().segment_identity(),
            Some(11),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("filename range"));

        let remaining = list_segment_files(&segment_dir).unwrap();
        assert_eq!(
            remaining.len(),
            3,
            "GC must validate the full candidate set before deleting anything"
        );
    }

    #[test]
    fn gap_at_retention_boundary_blocks_gc_before_deleting() {
        let data_dir = tempfile::tempdir().unwrap();
        let segment_dir = retained_segments_dir(data_dir.path());
        write_segment(&segment_dir, 1, 5);
        write_segment(&segment_dir, 13, 15);

        let err = prune_retained_segments(
            &segment_dir,
            database_identity().segment_identity(),
            Some(11),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("gap"));

        let remaining = list_segment_files(&segment_dir).unwrap();
        assert_eq!(
            remaining.len(),
            2,
            "GC must not delete older segments when the retained tail is already gapped"
        );
    }

    #[test]
    fn invalid_identity_is_rejected_even_without_segments() {
        let data_dir = tempfile::tempdir().unwrap();
        let err = prune_retained_segments(
            &retained_segments_dir(data_dir.path()),
            SegmentIdentity::current([0; 16], 1),
            Some(1),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
