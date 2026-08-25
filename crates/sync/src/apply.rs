use std::fs;
use std::io;
use std::path::Path;

use powdb_storage::catalog::Catalog;
use powdb_storage::create_data_dir_secure;
use powdb_storage::wal::{WalRecord, WalRecordType};
use serde::{Deserialize, Serialize};

use crate::metadata::{atomic_replace_json, now_unix_secs, read_identity, sync_state_dir};
use crate::segment::{
    read_units_since, validate_retained_tail_available, RetainedTailAvailability, RetainedUnit,
    SegmentIdentity,
};

const APPLY_STATE_FILE: &str = "apply-state.json";
const APPLY_STATE_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedTailApplySummary {
    pub from_lsn: u64,
    pub through_lsn: u64,
    pub units_applied: usize,
    pub first_lsn: Option<u64>,
    pub last_lsn: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ApplyStatus {
    InProgress,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ApplyStateFile {
    format_version: u32,
    database_id: [u8; 16],
    primary_generation: u64,
    wal_format_version: u16,
    catalog_version: u16,
    from_lsn: u64,
    through_lsn: u64,
    applied_lsn: u64,
    status: ApplyStatus,
    started_unix_secs: u64,
    updated_unix_secs: u64,
}

pub fn apply_retained_tail(
    catalog: &mut Catalog,
    retained_dir: &Path,
    expected_identity: SegmentIdentity,
    since_lsn: u64,
    through_lsn: u64,
) -> io::Result<RetainedTailApplySummary> {
    expected_identity.validate()?;
    let local_identity = read_identity(catalog.data_dir())?;
    if !local_identity
        .segment_identity()
        .lineage_matches(expected_identity)
    {
        return Err(invalid_input(
            "replica sync identity does not match retained tail history",
        ));
    }
    catalog.ensure_no_pending_wal_records()?;
    if through_lsn < since_lsn {
        return Err(invalid_input(format!(
            "retained tail target LSN {through_lsn} is behind start LSN {since_lsn}"
        )));
    }
    let resume_lsn = reconcile_apply_state(catalog, expected_identity, since_lsn, through_lsn)?;
    if through_lsn == since_lsn {
        if resume_lsn == since_lsn {
            write_complete_apply_state(
                catalog.data_dir(),
                expected_identity,
                since_lsn,
                through_lsn,
            )?;
            return Ok(noop_summary(since_lsn, through_lsn));
        }
        return Err(invalid_input(format!(
            "replica apply resume LSN {resume_lsn} does not match retained tail start LSN {since_lsn}"
        )));
    }

    if resume_lsn == through_lsn {
        write_complete_apply_state(
            catalog.data_dir(),
            expected_identity,
            since_lsn,
            through_lsn,
        )?;
        return Ok(noop_summary(since_lsn, through_lsn));
    }
    if resume_lsn < since_lsn || resume_lsn > through_lsn {
        return Err(invalid_input(format!(
            "replica apply resume LSN {resume_lsn} does not match retained tail start LSN {since_lsn}"
        )));
    }

    let availability =
        validate_retained_tail_available(retained_dir, expected_identity, resume_lsn, through_lsn)?;
    let max_units = usize::try_from(through_lsn - resume_lsn)
        .map_err(|_| invalid_input("retained tail range is too large to apply in one batch"))?;
    let units = read_units_since(retained_dir, expected_identity, resume_lsn, max_units)?;
    if units.len() != availability.units_available
        || units.last().map(|unit| unit.lsn) != Some(through_lsn)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "retained tail did not yield the complete requested LSN range",
        ));
    }
    validate_v1_retained_units_applyable(&units)?;

    let first_lsn = units.first().map(|unit| unit.lsn);
    let last_lsn = units.last().map(|unit| unit.lsn);
    let records = units
        .into_iter()
        .map(wal_record_from_retained_unit)
        .collect::<io::Result<Vec<_>>>()?;
    write_in_progress_apply_state(
        catalog.data_dir(),
        expected_identity,
        since_lsn,
        through_lsn,
        resume_lsn,
    )?;
    catalog.apply_wal_records(&records)?;
    write_complete_apply_state(
        catalog.data_dir(),
        expected_identity,
        since_lsn,
        through_lsn,
    )?;

    Ok(RetainedTailApplySummary {
        from_lsn: since_lsn,
        through_lsn,
        units_applied: records.len(),
        first_lsn,
        last_lsn,
    })
}

/// Record a trusted local retained-apply boundary.
///
/// Backup bootstrap uses this after restoring a verified same-lineage snapshot.
/// Chunked apply then requires each chunk to start at this boundary and promotes
/// the boundary only after the whole chunk has been replayed and marked complete.
pub fn seed_retained_apply_boundary(
    data_dir: &Path,
    expected_identity: SegmentIdentity,
    safe_lsn: u64,
) -> io::Result<()> {
    expected_identity.validate()?;
    let local_identity = read_identity(data_dir)?;
    if !local_identity
        .segment_identity()
        .lineage_matches(expected_identity)
    {
        return Err(invalid_input(
            "replica sync identity does not match retained tail history",
        ));
    }
    write_complete_apply_state(data_dir, expected_identity, safe_lsn, safe_lsn)
}

/// Apply one already-pulled retained-unit chunk to a local replica.
///
/// This is the V1 chunked-apply primitive used by embedded replicas after a
/// sync pull. It intentionally applies only a complete, contiguous chunk that
/// starts from a trusted local apply boundary: no gaps after `since_lsn`, no
/// unsupported DDL records, and no transaction-cut ranges. Callers can run
/// local reads between successful chunk calls; the catalog is only advanced
/// after the whole chunk is replayed.
pub fn apply_retained_units_chunk(
    catalog: &mut Catalog,
    expected_identity: SegmentIdentity,
    since_lsn: u64,
    units: &[RetainedUnit],
) -> io::Result<RetainedTailApplySummary> {
    expected_identity.validate()?;
    let local_identity = read_identity(catalog.data_dir())?;
    if !local_identity
        .segment_identity()
        .lineage_matches(expected_identity)
    {
        return Err(invalid_input(
            "replica sync identity does not match retained tail history",
        ));
    }
    catalog.ensure_no_pending_wal_records()?;

    let through_lsn = validate_retained_chunk_lsn_range(since_lsn, units)?;
    let resume_lsn = reconcile_apply_state(catalog, expected_identity, since_lsn, through_lsn)?;
    if units.is_empty() {
        if resume_lsn == since_lsn {
            ensure_retained_chunk_start_boundary(catalog, expected_identity, since_lsn)?;
            write_complete_apply_state(
                catalog.data_dir(),
                expected_identity,
                since_lsn,
                through_lsn,
            )?;
            return Ok(noop_summary(since_lsn, through_lsn));
        }
        return Err(invalid_input(format!(
            "replica apply resume LSN {resume_lsn} does not match retained chunk start LSN {since_lsn}"
        )));
    }
    if resume_lsn == through_lsn {
        ensure_retained_chunk_target_provenance(
            catalog,
            expected_identity,
            since_lsn,
            through_lsn,
        )?;
        write_complete_apply_state(
            catalog.data_dir(),
            expected_identity,
            since_lsn,
            through_lsn,
        )?;
        return Ok(noop_summary(since_lsn, through_lsn));
    }
    if resume_lsn != since_lsn {
        return Err(invalid_data(format!(
            "replica apply resume LSN {resume_lsn} is inside retained chunk {since_lsn}..{through_lsn}; repair required",
        )));
    }
    ensure_retained_chunk_start_boundary(catalog, expected_identity, since_lsn)?;

    validate_v1_retained_units_applyable(units)?;

    let first_lsn = units.first().map(|unit| unit.lsn);
    let last_lsn = units.last().map(|unit| unit.lsn);
    let records = units
        .iter()
        .cloned()
        .map(wal_record_from_retained_unit)
        .collect::<io::Result<Vec<_>>>()?;
    write_in_progress_apply_state(
        catalog.data_dir(),
        expected_identity,
        since_lsn,
        through_lsn,
        since_lsn,
    )?;
    catalog.apply_wal_records(&records)?;
    write_complete_apply_state(
        catalog.data_dir(),
        expected_identity,
        since_lsn,
        through_lsn,
    )?;

    Ok(RetainedTailApplySummary {
        from_lsn: since_lsn,
        through_lsn,
        units_applied: records.len(),
        first_lsn,
        last_lsn,
    })
}

/// Validate that a retained tail is safe for the V1 embedded-sync applier.
///
/// V1 applies complete, committed WAL-record ranges. It intentionally rejects
/// schema-changing records and ranges that end before every included explicit
/// transaction reaches a commit or rollback boundary.
pub fn validate_v1_retained_tail_applyable(
    retained_dir: &Path,
    expected_identity: SegmentIdentity,
    since_lsn: u64,
    through_lsn: u64,
) -> io::Result<RetainedTailAvailability> {
    expected_identity.validate()?;
    let availability =
        validate_retained_tail_available(retained_dir, expected_identity, since_lsn, through_lsn)?;
    if availability.units_available == 0 {
        return Ok(availability);
    }
    let max_units = usize::try_from(through_lsn - since_lsn)
        .map_err(|_| invalid_input("retained tail range is too large to validate"))?;
    let units = read_units_since(retained_dir, expected_identity, since_lsn, max_units)?;
    if units.len() != availability.units_available
        || units.last().map(|unit| unit.lsn) != Some(through_lsn)
    {
        return Err(invalid_data(
            "retained tail did not yield the complete requested LSN range",
        ));
    }
    validate_v1_retained_units_applyable(&units)?;
    Ok(availability)
}

fn validate_retained_chunk_lsn_range(since_lsn: u64, units: &[RetainedUnit]) -> io::Result<u64> {
    let Some(mut expected_lsn) = since_lsn.checked_add(1) else {
        if units.is_empty() {
            return Ok(since_lsn);
        }
        return Err(invalid_input("retained chunk start LSN overflow"));
    };
    for unit in units {
        if unit.lsn != expected_lsn {
            return Err(invalid_input(format!(
                "retained chunk is not contiguous after LSN {since_lsn}: expected LSN {expected_lsn}, found {}",
                unit.lsn
            )));
        }
        expected_lsn = expected_lsn
            .checked_add(1)
            .ok_or_else(|| invalid_input("retained chunk LSN overflow"))?;
    }
    Ok(units.last().map(|unit| unit.lsn).unwrap_or(since_lsn))
}

fn reconcile_apply_state(
    catalog: &Catalog,
    expected_identity: SegmentIdentity,
    since_lsn: u64,
    through_lsn: u64,
) -> io::Result<u64> {
    let current_lsn = catalog.max_lsn();
    let Some(state) = read_apply_state(catalog.data_dir())? else {
        if current_lsn == since_lsn || current_lsn == through_lsn {
            return Ok(current_lsn);
        }
        return Err(invalid_input(format!(
            "replica applied LSN {current_lsn} does not match retained tail start LSN {since_lsn}"
        )));
    };

    state.validate()?;
    if !state.identity().lineage_matches(expected_identity) {
        return Err(invalid_data(
            "local retained-tail apply state belongs to a different database history",
        ));
    }
    if matches!(state.status, ApplyStatus::Complete) {
        if state.applied_lsn > current_lsn {
            return Err(invalid_data(
                "local retained-tail apply state is ahead of the catalog LSN",
            ));
        }
        if current_lsn == since_lsn || current_lsn == through_lsn {
            return Ok(current_lsn);
        }
        return Err(invalid_input(format!(
            "replica applied LSN {current_lsn} does not match retained tail start LSN {since_lsn}"
        )));
    }

    if state.from_lsn != since_lsn || state.through_lsn != through_lsn {
        // A crash can strand an InProgress intent for a range this call is
        // not asking about. Two shapes are provably safe to supersede,
        // because the catalog LSN (recovered by WAL replay) pins exactly
        // what was durably applied:
        //  * nothing of the intent applied: the catalog stands at the
        //    boundary the intent's `applied_lsn` carried forward;
        //  * all of the intent applied, but the crash landed before the
        //    state flipped Complete: the catalog stands at the intent's
        //    target.
        // In both, `since_lsn == current_lsn` is a trusted boundary and the
        // stale intent is void. Anything else (a catalog LSN strictly
        // inside the stranded range, or a caller not starting at the
        // recovered boundary) stays fail-closed below.
        if current_lsn == since_lsn
            && (current_lsn == state.applied_lsn || current_lsn == state.through_lsn)
        {
            return Ok(current_lsn);
        }
        return Err(invalid_data(
            "another retained-tail apply is in progress for this replica",
        ));
    }
    if current_lsn == state.through_lsn {
        return Ok(current_lsn);
    }
    if current_lsn != state.applied_lsn {
        return Err(invalid_data(
            "local retained-tail apply state requires repair before retry",
        ));
    }
    write_in_progress_apply_state(
        catalog.data_dir(),
        expected_identity,
        state.from_lsn,
        state.through_lsn,
        state.applied_lsn,
    )?;
    Ok(state.applied_lsn)
}

fn ensure_retained_chunk_target_provenance(
    catalog: &Catalog,
    expected_identity: SegmentIdentity,
    since_lsn: u64,
    through_lsn: u64,
) -> io::Result<()> {
    let Some(state) = read_apply_state(catalog.data_dir())? else {
        return Err(invalid_data(format!(
            "retained chunk target LSN {through_lsn} has no trusted local apply provenance"
        )));
    };
    state.validate()?;
    if !state.identity().lineage_matches(expected_identity) {
        return Err(invalid_data(
            "retained chunk target provenance belongs to a different database history",
        ));
    }
    if state.from_lsn != since_lsn || state.through_lsn != through_lsn {
        return Err(invalid_data(
            "retained chunk target provenance belongs to a different apply range",
        ));
    }
    if catalog.max_lsn() != through_lsn {
        return Err(invalid_data(format!(
            "catalog LSN {} does not match retained chunk target boundary {through_lsn}",
            catalog.max_lsn()
        )));
    }
    match state.status {
        ApplyStatus::Complete if state.applied_lsn == through_lsn => Ok(()),
        ApplyStatus::InProgress if state.applied_lsn == since_lsn => Ok(()),
        _ => Err(invalid_data(
            "retained chunk target is not backed by completed or replayed apply state",
        )),
    }
}

fn ensure_retained_chunk_start_boundary(
    catalog: &Catalog,
    expected_identity: SegmentIdentity,
    since_lsn: u64,
) -> io::Result<()> {
    let Some(state) = read_apply_state(catalog.data_dir())? else {
        return Err(invalid_data(format!(
            "retained chunk start LSN {since_lsn} has no trusted local apply boundary"
        )));
    };
    state.validate()?;
    if !state.identity().lineage_matches(expected_identity) {
        return Err(invalid_data(
            "trusted retained chunk boundary belongs to a different database history",
        ));
    }
    if state.applied_lsn != since_lsn {
        return Err(invalid_data(format!(
            "retained chunk start LSN {since_lsn} is not a trusted completed apply boundary"
        )));
    }
    // An InProgress record whose `applied_lsn` matches is the crash shape
    // where the intent never touched the catalog: `applied_lsn` carries the
    // Complete boundary it overwrote, and the catalog-LSN check below proves
    // the chunk rolled back. Refusing it wedged crashed replicas permanently
    // (found by the replica kill-9 test).
    if catalog.max_lsn() != since_lsn {
        return Err(invalid_data(format!(
            "catalog LSN {} does not match retained chunk start boundary {since_lsn}",
            catalog.max_lsn()
        )));
    }
    Ok(())
}

fn read_apply_state(data_dir: &Path) -> io::Result<Option<ApplyStateFile>> {
    let path = sync_state_dir(data_dir).join(APPLY_STATE_FILE);
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let state: ApplyStateFile = serde_json::from_slice(&bytes).map_err(invalid_data)?;
    state.validate()?;
    Ok(Some(state))
}

fn write_in_progress_apply_state(
    data_dir: &Path,
    identity: SegmentIdentity,
    from_lsn: u64,
    through_lsn: u64,
    applied_lsn: u64,
) -> io::Result<()> {
    write_apply_state(
        data_dir,
        identity,
        from_lsn,
        through_lsn,
        applied_lsn,
        ApplyStatus::InProgress,
    )
}

fn write_complete_apply_state(
    data_dir: &Path,
    identity: SegmentIdentity,
    from_lsn: u64,
    through_lsn: u64,
) -> io::Result<()> {
    write_apply_state(
        data_dir,
        identity,
        from_lsn,
        through_lsn,
        through_lsn,
        ApplyStatus::Complete,
    )
}

fn write_apply_state(
    data_dir: &Path,
    identity: SegmentIdentity,
    from_lsn: u64,
    through_lsn: u64,
    applied_lsn: u64,
    status: ApplyStatus,
) -> io::Result<()> {
    let state_dir = sync_state_dir(data_dir);
    create_data_dir_secure(&state_dir)?;
    let existing_started = read_apply_state(data_dir)?
        .filter(|existing| {
            existing.identity().lineage_matches(identity)
                && existing.from_lsn == from_lsn
                && existing.through_lsn == through_lsn
        })
        .map(|existing| existing.started_unix_secs);
    let now = now_unix_secs();
    let state = ApplyStateFile {
        format_version: APPLY_STATE_FORMAT_VERSION,
        database_id: identity.database_id,
        primary_generation: identity.primary_generation,
        wal_format_version: identity.wal_format_version,
        catalog_version: identity.catalog_version,
        from_lsn,
        through_lsn,
        applied_lsn,
        status,
        started_unix_secs: existing_started.unwrap_or(now),
        updated_unix_secs: now,
    };
    state.validate()?;
    atomic_replace_json(&state_dir, APPLY_STATE_FILE, &state)
}

impl ApplyStateFile {
    fn identity(&self) -> SegmentIdentity {
        SegmentIdentity {
            database_id: self.database_id,
            primary_generation: self.primary_generation,
            wal_format_version: self.wal_format_version,
            catalog_version: self.catalog_version,
        }
    }

    fn validate(&self) -> io::Result<()> {
        if self.format_version != APPLY_STATE_FORMAT_VERSION {
            return Err(invalid_data(format!(
                "unsupported retained-tail apply state format {}",
                self.format_version
            )));
        }
        self.identity().validate()?;
        if self.through_lsn < self.from_lsn {
            return Err(invalid_data(
                "retained-tail apply state target is behind its start LSN",
            ));
        }
        if self.applied_lsn < self.from_lsn || self.applied_lsn > self.through_lsn {
            return Err(invalid_data(
                "retained-tail apply state applied LSN is outside the apply range",
            ));
        }
        if matches!(self.status, ApplyStatus::Complete) && self.applied_lsn != self.through_lsn {
            return Err(invalid_data(
                "completed retained-tail apply state must be applied through its target LSN",
            ));
        }
        Ok(())
    }
}

fn wal_record_from_retained_unit(unit: RetainedUnit) -> io::Result<WalRecord> {
    let record_type = WalRecordType::from_u8(unit.record_type).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "retained unit contains an unknown WAL record type",
        )
    })?;
    reject_unsupported_v1_record_type(record_type)?;
    Ok(WalRecord {
        tx_id: unit.tx_id,
        record_type,
        lsn: unit.lsn,
        data: unit.data,
    })
}

/// Validate an already-read retained-unit slice for V1 embedded-sync apply.
///
/// The slice must contain only V1-supported record types and must not end
/// inside an explicit transaction. Server pull/ack paths use this to avoid
/// advertising or accepting transaction-cut LSN boundaries.
pub fn validate_v1_retained_units_applyable(units: &[RetainedUnit]) -> io::Result<()> {
    let mut pending_tx_spans = Vec::new();
    for unit in units {
        let record_type = WalRecordType::from_u8(unit.record_type).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "retained unit contains an unknown WAL record type",
            )
        })?;
        reject_unsupported_v1_record_type(record_type)?;
        match record_type {
            WalRecordType::Begin if unit.tx_id != 0 => {
                pending_tx_spans.push(unit.tx_id);
            }
            WalRecordType::Insert | WalRecordType::Update | WalRecordType::Delete
                if unit.tx_id != 0 && !pending_tx_spans.contains(&unit.tx_id) =>
            {
                pending_tx_spans.push(unit.tx_id);
            }
            WalRecordType::Commit | WalRecordType::Rollback if unit.tx_id != 0 => {
                if let Some(index) = pending_tx_spans
                    .iter()
                    .rposition(|pending_tx_id| *pending_tx_id == unit.tx_id)
                {
                    pending_tx_spans.remove(index);
                }
            }
            _ => {}
        }
    }
    if let Some(tx_id) = pending_tx_spans.first() {
        return Err(invalid_input(format!(
            "retained tail cuts through transaction {tx_id}; retry with a range through its commit or rollback boundary",
        )));
    }
    Ok(())
}

fn reject_unsupported_v1_record_type(record_type: WalRecordType) -> io::Result<()> {
    if matches!(
        record_type,
        WalRecordType::DdlCreateTable
            | WalRecordType::DdlDropTable
            | WalRecordType::DdlAddColumn
            | WalRecordType::DdlDropColumn
    ) {
        return Err(invalid_input(
            "DDL retained units are not supported by V1 embedded sync; rebootstrap or upgrade required",
        ));
    }
    Ok(())
}

fn noop_summary(since_lsn: u64, through_lsn: u64) -> RetainedTailApplySummary {
    RetainedTailApplySummary {
        from_lsn: since_lsn,
        through_lsn,
        units_applied: 0,
        first_lsn: None,
        last_lsn: None,
    }
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn invalid_data(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        checkpoint_with_retained_segments, open_or_create_identity,
        open_preserving_retained_segments, read_identity_snapshot, read_units_through,
        retained_segments_dir, write_identity_snapshot,
    };
    use crate::{write_segment_atomic, RetainedSegment};
    use powdb_storage::types::{ColumnDef, Row, Schema, TypeId, Value};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp(tag: &str) -> PathBuf {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let uniq = CTR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "powdb_sync_apply_state_{tag}_{}_{}_{}",
            std::process::id(),
            now_unix_secs(),
            uniq
        ));
        let _ = fs::remove_dir_all(&path);
        path
    }

    fn schema_users() -> Schema {
        Schema {
            table_name: "User".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    type_id: TypeId::Int,
                    required: true,
                    position: 0,
                },
                ColumnDef {
                    name: "email".into(),
                    type_id: TypeId::Str,
                    required: false,
                    position: 1,
                },
            ],
        }
    }

    fn user_row(id: i64) -> Row {
        vec![Value::Int(id), Value::Str(format!("user{id}@example.com"))]
    }

    fn insert_range(catalog: &mut Catalog, start: i64, end: i64) {
        for id in start..end {
            catalog.insert("User", &user_row(id)).unwrap();
        }
        catalog.commit_autocommit().unwrap();
        catalog.sync_wal().unwrap();
    }

    fn retained_unit(tx_id: u64, record_type: WalRecordType, lsn: u64) -> RetainedUnit {
        RetainedUnit {
            tx_id,
            record_type: record_type as u8,
            lsn,
            data: Vec::new(),
        }
    }

    fn rows(catalog: &Catalog) -> Vec<(i64, String)> {
        let mut rows: Vec<_> = catalog
            .scan("User")
            .unwrap()
            .map(|item| {
                let (_, row) = item.unwrap();
                let id = match &row[0] {
                    Value::Int(id) => *id,
                    other => panic!("expected int id, got {other:?}"),
                };
                let email = match &row[1] {
                    Value::Str(email) => email.clone(),
                    other => panic!("expected email string, got {other:?}"),
                };
                (id, email)
            })
            .collect();
        rows.sort_by_key(|(id, _)| *id);
        rows
    }

    #[test]
    fn v1_applyability_rejects_transaction_split_before_commit() {
        let dir = tmp("split_tx_segments");
        let identity = SegmentIdentity::current(*b"apply-split-tx!!", 1);
        let segment = RetainedSegment::new(
            identity,
            vec![
                retained_unit(7, WalRecordType::Begin, 1),
                retained_unit(7, WalRecordType::Insert, 2),
            ],
        )
        .unwrap();
        write_segment_atomic(&dir, &segment).unwrap();

        let err = validate_v1_retained_tail_applyable(&dir, identity, 0, 2).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("cuts through transaction 7"),
            "split transaction tail must fail before storage replay, got: {err}"
        );
    }

    #[test]
    fn v1_applyability_rejects_begin_only_transaction_range() {
        let units = vec![retained_unit(7, WalRecordType::Begin, 1)];
        let err = validate_v1_retained_units_applyable(&units).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("cuts through transaction 7"),
            "begin-only transaction range must fail before storage replay, got: {err}"
        );
    }

    #[test]
    fn v1_applyability_rejects_row_only_nonzero_transaction_range() {
        let units = vec![retained_unit(9, WalRecordType::Insert, 1)];
        let err = validate_v1_retained_units_applyable(&units).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("cuts through transaction 9"),
            "row-only transaction range must fail before storage replay, got: {err}"
        );
    }

    #[test]
    fn v1_applyability_rejects_reused_tx_id_with_later_incomplete_span() {
        let units = vec![
            retained_unit(1, WalRecordType::Begin, 1),
            retained_unit(1, WalRecordType::Insert, 2),
            retained_unit(1, WalRecordType::Commit, 3),
            retained_unit(1, WalRecordType::Begin, 4),
            retained_unit(1, WalRecordType::Insert, 5),
        ];
        let err = validate_v1_retained_units_applyable(&units).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("cuts through transaction 1"),
            "later incomplete span with reused tx id must fail, got: {err}"
        );
    }

    #[test]
    fn v1_applyability_accepts_transaction_closed_by_commit_or_rollback() {
        let commit_units = vec![
            retained_unit(7, WalRecordType::Begin, 1),
            retained_unit(7, WalRecordType::Insert, 2),
            retained_unit(7, WalRecordType::Commit, 3),
        ];
        validate_v1_retained_units_applyable(&commit_units).unwrap();

        let rollback_units = vec![
            retained_unit(8, WalRecordType::Begin, 1),
            retained_unit(8, WalRecordType::Insert, 2),
            retained_unit(8, WalRecordType::Rollback, 3),
        ];
        validate_v1_retained_units_applyable(&rollback_units).unwrap();
    }

    #[test]
    fn retained_units_chunk_rejects_non_contiguous_lsn_range() {
        let units = vec![
            retained_unit(0, WalRecordType::Commit, 6),
            retained_unit(0, WalRecordType::Commit, 8),
        ];
        let err = validate_retained_chunk_lsn_range(5, &units).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("expected LSN 7"),
            "non-contiguous retained chunks must fail before replay, got: {err}"
        );
    }

    #[test]
    fn apply_retained_units_chunk_requires_trusted_start_boundary() {
        let replica = tmp("chunk_no_boundary_replica");
        let mut replica_cat = Catalog::create(&replica).unwrap();
        replica_cat.create_table(schema_users()).unwrap();
        insert_range(&mut replica_cat, 0, 1);
        let identity = open_or_create_identity(&replica).unwrap();
        checkpoint_with_retained_segments(&mut replica_cat).unwrap();
        let since_lsn = replica_cat.max_lsn();
        let units = vec![retained_unit(0, WalRecordType::Commit, since_lsn + 1)];

        let err = apply_retained_units_chunk(
            &mut replica_cat,
            identity.segment_identity(),
            since_lsn,
            &units,
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("no trusted local apply boundary"),
            "unseeded retained chunks must fail before replay, got: {err}"
        );
        assert_eq!(
            replica_cat.max_lsn(),
            since_lsn,
            "failed chunk apply must not advance catalog LSN"
        );
    }

    #[test]
    fn apply_retained_units_chunk_rejects_catalog_only_target_noop_provenance() {
        let replica = tmp("chunk_catalog_only_target_replica");
        let mut replica_cat = Catalog::create(&replica).unwrap();
        replica_cat.create_table(schema_users()).unwrap();
        insert_range(&mut replica_cat, 0, 2);
        let identity = open_or_create_identity(&replica).unwrap();
        checkpoint_with_retained_segments(&mut replica_cat).unwrap();
        let through_lsn = replica_cat.max_lsn();
        let since_lsn = through_lsn - 1;
        let units = vec![retained_unit(0, WalRecordType::Commit, through_lsn)];

        let err = apply_retained_units_chunk(
            &mut replica_cat,
            identity.segment_identity(),
            since_lsn,
            &units,
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string()
                .contains("no trusted local apply provenance"),
            "catalog-only target no-op must not mint a trusted boundary, got: {err}"
        );
        assert!(
            read_apply_state(&replica).unwrap().is_none(),
            "failed catalog-only target no-op must not write apply-state"
        );
    }

    #[test]
    fn apply_retained_units_chunk_rejects_transaction_cut_without_advancing_lsn() {
        let replica = tmp("chunk_cut_replica");
        let mut replica_cat = Catalog::create(&replica).unwrap();
        replica_cat.create_table(schema_users()).unwrap();
        insert_range(&mut replica_cat, 0, 1);
        let identity = open_or_create_identity(&replica).unwrap();
        checkpoint_with_retained_segments(&mut replica_cat).unwrap();
        let since_lsn = replica_cat.max_lsn();
        seed_retained_apply_boundary(&replica, identity.segment_identity(), since_lsn).unwrap();
        let cut_units = vec![
            retained_unit(7, WalRecordType::Begin, since_lsn + 1),
            retained_unit(7, WalRecordType::Insert, since_lsn + 2),
        ];

        let err = apply_retained_units_chunk(
            &mut replica_cat,
            identity.segment_identity(),
            since_lsn,
            &cut_units,
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("cuts through transaction 7"),
            "transaction-cut retained chunks must fail before replay, got: {err}"
        );
        assert_eq!(
            replica_cat.max_lsn(),
            since_lsn,
            "failed chunk apply must not advance catalog LSN"
        );
    }

    fn copy_snapshot_files(src: &Path, dest: &Path) {
        fs::create_dir_all(dest).unwrap();
        for entry in fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "catalog.bin"
                || name == powdb_storage::catalog::CATALOG_LSN_FILE
                || name.ends_with(".heap")
                || name.ends_with(".idx")
            {
                fs::copy(entry.path(), dest.join(name)).unwrap();
            }
        }
    }

    #[test]
    fn in_progress_apply_state_replays_from_recorded_safe_lsn_when_catalog_matches() {
        let primary = tmp("primary");
        let mut primary_cat = Catalog::create(&primary).unwrap();
        primary_cat.create_table(schema_users()).unwrap();
        insert_range(&mut primary_cat, 0, 3);
        let identity = open_or_create_identity(&primary).unwrap();
        checkpoint_with_retained_segments(&mut primary_cat).unwrap();
        let snapshot_lsn = primary_cat.max_lsn();

        let replica = tmp("replica");
        copy_snapshot_files(&primary, &replica);
        let identity_snapshot = read_identity_snapshot(&primary).unwrap().unwrap();
        write_identity_snapshot(&replica, &identity_snapshot).unwrap();

        insert_range(&mut primary_cat, 3, 8);
        checkpoint_with_retained_segments(&mut primary_cat).unwrap();
        let through_lsn = primary_cat.max_lsn();
        assert!(through_lsn > snapshot_lsn);

        let retained_dir = retained_segments_dir(&primary);
        let replica_cat = open_preserving_retained_segments(&replica).unwrap();
        write_in_progress_apply_state(
            &replica,
            identity.segment_identity(),
            snapshot_lsn,
            through_lsn,
            snapshot_lsn,
        )
        .unwrap();
        drop(replica_cat);

        let mut reopened = open_preserving_retained_segments(&replica).unwrap();
        let summary = apply_retained_tail(
            &mut reopened,
            &retained_dir,
            identity.segment_identity(),
            snapshot_lsn,
            through_lsn,
        )
        .unwrap();

        assert_eq!(summary.from_lsn, snapshot_lsn);
        assert_eq!(summary.first_lsn, Some(snapshot_lsn + 1));
        assert_eq!(summary.last_lsn, Some(through_lsn));
        assert_eq!(rows(&reopened), rows(&primary_cat));
        assert!(matches!(
            read_apply_state(&replica).unwrap().unwrap().status,
            ApplyStatus::Complete
        ));
    }

    #[test]
    fn in_progress_apply_state_fails_closed_when_catalog_lsn_advanced() {
        let primary = tmp("advanced_primary");
        let mut primary_cat = Catalog::create(&primary).unwrap();
        primary_cat.create_table(schema_users()).unwrap();
        insert_range(&mut primary_cat, 0, 3);
        let identity = open_or_create_identity(&primary).unwrap();
        checkpoint_with_retained_segments(&mut primary_cat).unwrap();
        let snapshot_lsn = primary_cat.max_lsn();

        let replica = tmp("advanced_replica");
        copy_snapshot_files(&primary, &replica);
        let identity_snapshot = read_identity_snapshot(&primary).unwrap().unwrap();
        write_identity_snapshot(&replica, &identity_snapshot).unwrap();

        insert_range(&mut primary_cat, 3, 8);
        checkpoint_with_retained_segments(&mut primary_cat).unwrap();
        let through_lsn = primary_cat.max_lsn();
        let retained_dir = retained_segments_dir(&primary);
        let units = read_units_since(
            &retained_dir,
            identity.segment_identity(),
            snapshot_lsn,
            100,
        )
        .unwrap();
        assert!(units.len() > 1, "test needs a multi-unit retained tail");
        let partial_records = units[..1]
            .iter()
            .cloned()
            .map(wal_record_from_retained_unit)
            .collect::<io::Result<Vec<_>>>()
            .unwrap();

        let mut replica_cat = open_preserving_retained_segments(&replica).unwrap();
        write_in_progress_apply_state(
            &replica,
            identity.segment_identity(),
            snapshot_lsn,
            through_lsn,
            snapshot_lsn,
        )
        .unwrap();
        replica_cat.apply_wal_records(&partial_records).unwrap();
        assert!(replica_cat.max_lsn() > snapshot_lsn);
        drop(replica_cat);

        let mut reopened = open_preserving_retained_segments(&replica).unwrap();
        let err = apply_retained_tail(
            &mut reopened,
            &retained_dir,
            identity.segment_identity(),
            snapshot_lsn,
            through_lsn,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("requires repair before retry"),
            "advanced catalog LSN without complete apply-state must fail closed, got: {err}"
        );
    }

    /// SIGKILL between writing the InProgress intent and applying any unit:
    /// WAL replay recovers the catalog at exactly the boundary the intent's
    /// `applied_lsn` records. The stale intent is void — a host restarting
    /// with a DIFFERENT target range (the natural whole-tail resume) must
    /// not be wedged behind "another retained-tail apply is in progress".
    #[test]
    fn a_rolled_back_interrupted_apply_accepts_a_new_resume_range() {
        let primary = tmp("rolledback_primary");
        let mut primary_cat = Catalog::create(&primary).unwrap();
        primary_cat.create_table(schema_users()).unwrap();
        insert_range(&mut primary_cat, 0, 3);
        let identity = open_or_create_identity(&primary).unwrap();
        checkpoint_with_retained_segments(&mut primary_cat).unwrap();
        let snapshot_lsn = primary_cat.max_lsn();

        let replica = tmp("rolledback_replica");
        copy_snapshot_files(&primary, &replica);
        let identity_snapshot = read_identity_snapshot(&primary).unwrap().unwrap();
        write_identity_snapshot(&replica, &identity_snapshot).unwrap();

        insert_range(&mut primary_cat, 3, 8);
        checkpoint_with_retained_segments(&mut primary_cat).unwrap();
        let through_lsn = primary_cat.max_lsn();
        let retained_dir = retained_segments_dir(&primary);

        // Crash shape: intent written for some mid-range target, nothing
        // applied (the catalog is still at the snapshot boundary).
        write_in_progress_apply_state(
            &replica,
            identity.segment_identity(),
            snapshot_lsn,
            snapshot_lsn + 2,
            snapshot_lsn,
        )
        .unwrap();

        let mut reopened = open_preserving_retained_segments(&replica).unwrap();
        assert_eq!(reopened.max_lsn(), snapshot_lsn);
        let summary = apply_retained_tail(
            &mut reopened,
            &retained_dir,
            identity.segment_identity(),
            snapshot_lsn,
            through_lsn,
        )
        .unwrap();
        assert_eq!(summary.through_lsn, through_lsn);
        assert_eq!(rows(&reopened), rows(&primary_cat));
    }

    /// Same crash shape, but the host retries its EXACT interrupted chunk
    /// (the `apply_retained_units_chunk` contract). The rolled-back intent
    /// must count as a trusted start boundary: `applied_lsn` carries the
    /// Complete boundary it overwrote forward, and the catalog LSN proves
    /// the chunk never touched the catalog.
    #[test]
    fn a_rolled_back_interrupted_chunk_can_be_retried_exactly() {
        let primary = tmp("chunkretry_primary");
        let mut primary_cat = Catalog::create(&primary).unwrap();
        primary_cat.create_table(schema_users()).unwrap();
        insert_range(&mut primary_cat, 0, 3);
        let identity = open_or_create_identity(&primary).unwrap();
        checkpoint_with_retained_segments(&mut primary_cat).unwrap();
        let snapshot_lsn = primary_cat.max_lsn();

        let replica = tmp("chunkretry_replica");
        copy_snapshot_files(&primary, &replica);
        let identity_snapshot = read_identity_snapshot(&primary).unwrap().unwrap();
        write_identity_snapshot(&replica, &identity_snapshot).unwrap();

        insert_range(&mut primary_cat, 3, 8);
        checkpoint_with_retained_segments(&mut primary_cat).unwrap();
        let through_lsn = primary_cat.max_lsn();
        let retained_dir = retained_segments_dir(&primary);
        let units = read_units_since(
            &retained_dir,
            identity.segment_identity(),
            snapshot_lsn,
            100,
        )
        .unwrap();

        write_in_progress_apply_state(
            &replica,
            identity.segment_identity(),
            snapshot_lsn,
            through_lsn,
            snapshot_lsn,
        )
        .unwrap();

        let mut reopened = open_preserving_retained_segments(&replica).unwrap();
        let summary = apply_retained_units_chunk(
            &mut reopened,
            identity.segment_identity(),
            snapshot_lsn,
            &units,
        )
        .unwrap();
        assert_eq!(summary.through_lsn, through_lsn);
        assert_eq!(rows(&reopened), rows(&primary_cat));
    }

    /// SIGKILL after the chunk's records were durably applied but before the
    /// state flipped to Complete: the catalog stands exactly at the intent's
    /// target. Continuing from there with a new range must work — everything
    /// up to `state.through_lsn` is provably applied.
    #[test]
    fn a_completed_but_unflipped_chunk_accepts_the_next_range() {
        let primary = tmp("unflipped_primary");
        let mut primary_cat = Catalog::create(&primary).unwrap();
        primary_cat.create_table(schema_users()).unwrap();
        insert_range(&mut primary_cat, 0, 3);
        let identity = open_or_create_identity(&primary).unwrap();
        checkpoint_with_retained_segments(&mut primary_cat).unwrap();
        let snapshot_lsn = primary_cat.max_lsn();

        let replica = tmp("unflipped_replica");
        copy_snapshot_files(&primary, &replica);
        let identity_snapshot = read_identity_snapshot(&primary).unwrap().unwrap();
        write_identity_snapshot(&replica, &identity_snapshot).unwrap();

        insert_range(&mut primary_cat, 3, 5);
        checkpoint_with_retained_segments(&mut primary_cat).unwrap();
        let mid_lsn = primary_cat.max_lsn();
        insert_range(&mut primary_cat, 5, 8);
        checkpoint_with_retained_segments(&mut primary_cat).unwrap();
        let through_lsn = primary_cat.max_lsn();
        let retained_dir = retained_segments_dir(&primary);

        // Apply the first chunk's records, then "crash" before the state
        // flips: the intent stays InProgress while the catalog stands at
        // its target.
        let first_units = read_units_through(
            &retained_dir,
            identity.segment_identity(),
            snapshot_lsn,
            mid_lsn,
            usize::MAX,
        )
        .unwrap();
        let records = first_units
            .iter()
            .cloned()
            .map(wal_record_from_retained_unit)
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        let mut replica_cat = open_preserving_retained_segments(&replica).unwrap();
        write_in_progress_apply_state(
            &replica,
            identity.segment_identity(),
            snapshot_lsn,
            mid_lsn,
            snapshot_lsn,
        )
        .unwrap();
        replica_cat.apply_wal_records(&records).unwrap();
        assert_eq!(replica_cat.max_lsn(), mid_lsn);
        drop(replica_cat);

        let mut reopened = open_preserving_retained_segments(&replica).unwrap();
        let summary = apply_retained_tail(
            &mut reopened,
            &retained_dir,
            identity.segment_identity(),
            mid_lsn,
            through_lsn,
        )
        .unwrap();
        assert_eq!(summary.through_lsn, through_lsn);
        assert_eq!(rows(&reopened), rows(&primary_cat));
    }

    #[test]
    fn in_progress_apply_state_marks_complete_when_catalog_reached_target() {
        let primary = tmp("complete_window_primary");
        let mut primary_cat = Catalog::create(&primary).unwrap();
        primary_cat.create_table(schema_users()).unwrap();
        insert_range(&mut primary_cat, 0, 3);
        let identity = open_or_create_identity(&primary).unwrap();
        checkpoint_with_retained_segments(&mut primary_cat).unwrap();
        let snapshot_lsn = primary_cat.max_lsn();

        let replica = tmp("complete_window_replica");
        copy_snapshot_files(&primary, &replica);
        let identity_snapshot = read_identity_snapshot(&primary).unwrap().unwrap();
        write_identity_snapshot(&replica, &identity_snapshot).unwrap();

        insert_range(&mut primary_cat, 3, 8);
        checkpoint_with_retained_segments(&mut primary_cat).unwrap();
        let through_lsn = primary_cat.max_lsn();
        let retained_dir = retained_segments_dir(&primary);
        let records = read_units_since(
            &retained_dir,
            identity.segment_identity(),
            snapshot_lsn,
            100,
        )
        .unwrap()
        .into_iter()
        .map(wal_record_from_retained_unit)
        .collect::<io::Result<Vec<_>>>()
        .unwrap();

        let mut replica_cat = open_preserving_retained_segments(&replica).unwrap();
        write_in_progress_apply_state(
            &replica,
            identity.segment_identity(),
            snapshot_lsn,
            through_lsn,
            snapshot_lsn,
        )
        .unwrap();
        replica_cat.apply_wal_records(&records).unwrap();
        assert_eq!(replica_cat.max_lsn(), through_lsn);
        drop(replica_cat);

        let mut reopened = open_preserving_retained_segments(&replica).unwrap();
        let summary = apply_retained_tail(
            &mut reopened,
            &retained_dir,
            identity.segment_identity(),
            snapshot_lsn,
            through_lsn,
        )
        .unwrap();

        assert_eq!(summary.units_applied, 0);
        assert_eq!(rows(&reopened), rows(&primary_cat));
        assert!(matches!(
            read_apply_state(&replica).unwrap().unwrap().status,
            ApplyStatus::Complete
        ));
    }

    #[test]
    fn chunk_apply_marks_complete_when_in_progress_replay_reached_target() {
        let primary = tmp("chunk_complete_window_primary");
        let mut primary_cat = Catalog::create(&primary).unwrap();
        primary_cat.create_table(schema_users()).unwrap();
        insert_range(&mut primary_cat, 0, 3);
        let identity = open_or_create_identity(&primary).unwrap();
        checkpoint_with_retained_segments(&mut primary_cat).unwrap();
        let snapshot_lsn = primary_cat.max_lsn();

        let replica = tmp("chunk_complete_window_replica");
        copy_snapshot_files(&primary, &replica);
        let identity_snapshot = read_identity_snapshot(&primary).unwrap().unwrap();
        write_identity_snapshot(&replica, &identity_snapshot).unwrap();

        insert_range(&mut primary_cat, 3, 8);
        checkpoint_with_retained_segments(&mut primary_cat).unwrap();
        let through_lsn = primary_cat.max_lsn();
        let units = read_units_through(
            &retained_segments_dir(&primary),
            identity.segment_identity(),
            snapshot_lsn,
            through_lsn,
            100,
        )
        .unwrap();
        let records = units
            .iter()
            .cloned()
            .map(wal_record_from_retained_unit)
            .collect::<io::Result<Vec<_>>>()
            .unwrap();

        let mut replica_cat = open_preserving_retained_segments(&replica).unwrap();
        write_in_progress_apply_state(
            &replica,
            identity.segment_identity(),
            snapshot_lsn,
            through_lsn,
            snapshot_lsn,
        )
        .unwrap();
        replica_cat.apply_wal_records(&records).unwrap();
        assert_eq!(replica_cat.max_lsn(), through_lsn);
        drop(replica_cat);

        let mut reopened = open_preserving_retained_segments(&replica).unwrap();
        let summary = apply_retained_units_chunk(
            &mut reopened,
            identity.segment_identity(),
            snapshot_lsn,
            &units,
        )
        .unwrap();

        assert_eq!(summary.units_applied, 0);
        assert_eq!(rows(&reopened), rows(&primary_cat));
        assert!(matches!(
            read_apply_state(&replica).unwrap().unwrap().status,
            ApplyStatus::Complete
        ));
    }

    #[test]
    fn ddl_retained_units_fail_closed_in_v1_apply() {
        let err = wal_record_from_retained_unit(RetainedUnit {
            tx_id: 0,
            record_type: WalRecordType::DdlCreateTable as u8,
            lsn: 42,
            data: Vec::new(),
        })
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("DDL retained units are not supported"),
            "DDL retained units must fail closed, got: {err}"
        );
    }

    #[test]
    fn different_in_progress_apply_range_fails_closed() {
        let primary = tmp("blocked_primary");
        let mut primary_cat = Catalog::create(&primary).unwrap();
        primary_cat.create_table(schema_users()).unwrap();
        insert_range(&mut primary_cat, 0, 1);
        let identity = open_or_create_identity(&primary).unwrap();

        write_in_progress_apply_state(&primary, identity.segment_identity(), 1, 10, 1).unwrap();
        let err =
            reconcile_apply_state(&primary_cat, identity.segment_identity(), 1, 11).unwrap_err();
        assert!(
            err.to_string().contains("another retained-tail apply"),
            "mismatched in-progress range must fail closed, got: {err}"
        );
    }
}
