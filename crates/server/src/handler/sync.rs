//! The private replication frontend: status, pull, and ack. Every frame is
//! refused as far ahead of the transaction gate as it can be decided, then
//! executed under the whole gate.

use crate::metrics::{Metrics, SyncOperation, SyncOutcome, SyncRepairLabel};
use crate::protocol::{
    ErrorClass, Message, WireRetainedUnit, WireSyncRepairAction, WireSyncStatus,
};
use powdb_auth::{Permission, Role};
use powdb_query::executor::Engine;
use powdb_sync::{
    acknowledge_replica_apply, read_identity, read_units_through, replica_sync_status,
    retained_segments_dir, validate_retained_tail_available, validate_v1_retained_units_applyable,
    ReplicaSyncStatus, RetainedUnit, SegmentIdentity, SyncRepairAction,
    RETAINED_SEGMENT_FORMAT_VERSION,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::OwnedSemaphorePermit;
use tracing::{debug, info, warn};

use super::auth::Principal;
use super::classify::error_response;
use super::transaction::TxGate;

/// Server-side cap for private sync pull batches.
pub(super) const MAX_SYNC_PULL_UNITS: u32 = 4096;

/// Server-side cap for retained-unit payload bytes in a private sync pull.
pub(super) const MAX_SYNC_PULL_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub(super) struct SyncPullRequest {
    pub(super) replica_id: String,
    pub(super) since_lsn: u64,
    pub(super) max_units: u32,
    pub(super) max_bytes: u64,
    pub(super) database_id: [u8; 16],
    pub(super) primary_generation: u64,
    pub(super) wal_format_version: u16,
    pub(super) catalog_version: u16,
    pub(super) segment_format_version: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SyncErrorClass {
    AuthRequired,
    PermissionDenied,
    InvalidReplicaId,
    ActiveTransaction,
    GateTimeout,
    QueryExecution,
    InvalidMaxUnits,
    InvalidMaxBytes,
    SyncContext,
    StatusRead,
    CursorLsnMismatch,
    IdentityRead,
    IdentityOrFormatMismatch,
    RetainedRead,
    RetainedUnitEncoding,
    RetainedChunkNotApplyable,
    LsnAheadOfRemote,
    AckValidation,
    /// The primary refused to advance this replica's cursor for a reason the
    /// replica itself must act on: no cursor, an inactive cursor, or an LSN
    /// behind the one already recorded. Sibling of
    /// [`SyncErrorClass::IdentityOrFormatMismatch`], and classified the same
    /// way, so the two "rebootstrap required" answers a replica has to branch
    /// on cannot arrive with different wire classes.
    AckRejected,
    /// The cursor update failed for a reason on the server's side (an I/O
    /// error). Not actionable by the replica.
    AckUpdate,
    Internal,
}

impl SyncErrorClass {
    pub(super) const fn as_label(self) -> &'static str {
        match self {
            Self::AuthRequired => "auth_required",
            Self::PermissionDenied => "permission_denied",
            Self::InvalidReplicaId => "invalid_replica_id",
            Self::ActiveTransaction => "active_transaction",
            Self::GateTimeout => "gate_timeout",
            Self::QueryExecution => "query_execution",
            Self::InvalidMaxUnits => "invalid_max_units",
            Self::InvalidMaxBytes => "invalid_max_bytes",
            Self::SyncContext => "sync_context",
            Self::StatusRead => "status_read",
            Self::CursorLsnMismatch => "cursor_lsn_mismatch",
            Self::IdentityRead => "identity_read",
            Self::IdentityOrFormatMismatch => "identity_or_format_mismatch",
            Self::RetainedRead => "retained_read",
            Self::RetainedUnitEncoding => "retained_unit_encoding",
            Self::RetainedChunkNotApplyable => "retained_chunk_not_applyable",
            Self::LsnAheadOfRemote => "lsn_ahead_of_remote",
            Self::AckValidation => "ack_validation",
            Self::AckRejected => "ack_rejected",
            Self::AckUpdate => "ack_update",
            Self::Internal => "internal",
        }
    }

    /// The stable wire [`ErrorClass`] this sync rejection is reported as.
    ///
    /// Sync errors used to reach the wire as a bare `MSG_ERROR` with no class
    /// byte at all, which is the one thing this protocol promises never to do:
    /// a driver could tell a timeout from an auth failure on every query
    /// frontend but not on this one. The match is exhaustive, so a new
    /// [`SyncErrorClass`] cannot be added without choosing a class, and no arm
    /// falls back to [`ErrorClass::Internal`] just to compile.
    ///
    /// Classes are the ones the query frontends already use for the same
    /// meaning, so a client branches identically whichever frontend answered:
    /// a role refusal is `Execution` because `check_statement_permitted`
    /// reports its "permission denied" that way, and a gate wait is `Timeout`
    /// because `acquire_begin_permit` reports its wait that way.
    pub(super) const fn wire_class(self) -> ErrorClass {
        match self {
            // Fixable by reconnecting with credentials.
            Self::AuthRequired => ErrorClass::AuthFailed,
            // Not fixable by re-authenticating as the same principal: parity
            // with the query frontends' role refusals.
            Self::PermissionDenied => ErrorClass::Execution,
            // The client's own request field is wrong and it can say so.
            Self::InvalidReplicaId
            | Self::ActiveTransaction
            | Self::CursorLsnMismatch
            | Self::IdentityOrFormatMismatch
            | Self::RetainedChunkNotApplyable
            | Self::LsnAheadOfRemote
            | Self::AckValidation
            // A refused cursor advance is the ack-side twin of
            // `IdentityOrFormatMismatch`: both tell the replica to rebootstrap,
            // so both must arrive as the same class. Reporting this one as
            // `Internal` made it indistinguishable from an unclassified server
            // fault, which is the one answer a replica cannot act on.
            | Self::AckRejected => ErrorClass::Execution,
            // A caller-supplied bound outside the server's accepted range.
            Self::InvalidMaxUnits | Self::InvalidMaxBytes => ErrorClass::LimitExceeded,
            // The gate wait elapsed; retryable, like every other time budget.
            Self::GateTimeout => ErrorClass::Timeout,
            // Server-side failures the client cannot act on.
            Self::QueryExecution
            | Self::SyncContext
            | Self::StatusRead
            | Self::IdentityRead
            | Self::RetainedRead
            | Self::RetainedUnitEncoding
            | Self::AckUpdate
            | Self::Internal => ErrorClass::Internal,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct SyncDecision {
    pub(super) message: Message,
    pub(super) error_class: Option<SyncErrorClass>,
}

impl SyncDecision {
    pub(super) fn ok(message: Message) -> Self {
        Self {
            message,
            error_class: None,
        }
    }

    pub(super) fn error(class: SyncErrorClass, message: impl Into<String>) -> Self {
        Self {
            message: error_response(message, class.wire_class()),
            error_class: Some(class),
        }
    }
}

pub(super) fn check_sync_protocol_permitted(
    credential_authenticated: bool,
    principal: Option<&Principal>,
) -> Result<(), (SyncErrorClass, String)> {
    if !credential_authenticated {
        return Err((
            SyncErrorClass::AuthRequired,
            "sync protocol requires authentication".to_string(),
        ));
    }
    if let Some(principal) = principal {
        let allowed =
            Role::builtin(&principal.role).is_some_and(|role| role.allows(Permission::Write));
        if !allowed {
            return Err((
                SyncErrorClass::PermissionDenied,
                format!(
                    "permission denied: role '{}' cannot use sync protocol",
                    principal.role
                ),
            ));
        }
    }
    Ok(())
}

/// Range check for a sync pull's caller-supplied batch bounds. Pure: it needs
/// no engine access, so it belongs to [`SyncPreGate`], which is the ONLY place
/// it runs. [`dispatch_sync_pull_decision`] reaches it through the same
/// pre-gate rather than repeating it.
pub(super) fn check_sync_pull_bounds(
    max_units: u32,
    max_bytes: u64,
) -> Result<(), (SyncErrorClass, String)> {
    if max_units == 0 || max_units > MAX_SYNC_PULL_UNITS {
        return Err((
            SyncErrorClass::InvalidMaxUnits,
            format!("sync pull maxUnits must be between 1 and {MAX_SYNC_PULL_UNITS}"),
        ));
    }
    if max_bytes == 0 || max_bytes > MAX_SYNC_PULL_BYTES {
        return Err((
            SyncErrorClass::InvalidMaxBytes,
            format!("sync pull maxBytes must be between 1 and {MAX_SYNC_PULL_BYTES}"),
        ));
    }
    Ok(())
}

/// The half of the sync ack LSN validation that compares only the two numbers
/// the client sent. The remaining checks need the primary's LSN and therefore
/// stay inside [`dispatch_sync_ack_decision`], under the gate.
pub(super) fn check_sync_ack_lsn_bounds(
    applied_lsn: u64,
    observed_remote_lsn: u64,
) -> Result<(), (SyncErrorClass, String)> {
    if applied_lsn > observed_remote_lsn {
        return Err((
            SyncErrorClass::LsnAheadOfRemote,
            format!(
                "sync ack appliedLsn {applied_lsn} is ahead of observed remoteLsn {observed_remote_lsn}"
            ),
        ));
    }
    Ok(())
}

/// Everything a sync frame can be rejected for WITHOUT touching the engine,
/// carried per sync message type.
///
/// This exists so [`execute_gated_sync`] can apply the rule the three query
/// frontends already follow ("a statement that executes nothing must acquire
/// nothing", see [`parse_failure_response`]) to the sync frontend. Before it,
/// every one of these rejections ran *after* `acquire_many_owned(all permits)`
/// inside the blocking decision function, so an unauthenticated peer sending a
/// frame that would be refused in microseconds still seized the entire gate
/// first.
///
/// A sync message type reaches the gate only by building one of these
/// variants, and [`SyncPreGate::check`] matches exhaustively, so a fourth sync
/// frame cannot be added without deciding what it may be rejected for
/// pre-gate.
pub(super) enum SyncPreGate {
    Status {
        replica_id: String,
    },
    Pull {
        replica_id: String,
        max_units: u32,
        max_bytes: u64,
    },
    Ack {
        replica_id: String,
        applied_lsn: u64,
        observed_remote_lsn: u64,
    },
}

impl SyncPreGate {
    pub(super) fn check(
        &self,
        credential_authenticated: bool,
        principal: Option<&Principal>,
    ) -> Result<(), (SyncErrorClass, String)> {
        check_sync_protocol_permitted(credential_authenticated, principal)?;
        let replica_id = match self {
            Self::Status { replica_id }
            | Self::Pull { replica_id, .. }
            | Self::Ack { replica_id, .. } => replica_id,
        };
        validate_wire_replica_id(replica_id)
            .map_err(|message| (SyncErrorClass::InvalidReplicaId, message))?;
        match self {
            Self::Status { .. } => Ok(()),
            Self::Pull {
                max_units,
                max_bytes,
                ..
            } => check_sync_pull_bounds(*max_units, *max_bytes),
            Self::Ack {
                applied_lsn,
                observed_remote_lsn,
                ..
            } => check_sync_ack_lsn_bounds(*applied_lsn, *observed_remote_lsn),
        }
    }
}

fn validate_wire_replica_id(replica_id: &str) -> Result<(), String> {
    if replica_id.is_empty() {
        return Err("replica id must be non-empty".to_string());
    }
    if replica_id.len() > 128 {
        return Err("replica id must be at most 128 bytes".to_string());
    }
    if !replica_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    {
        return Err("replica id contains unsupported characters".to_string());
    }
    Ok(())
}

pub(super) fn sync_context(engine: &Arc<RwLock<Engine>>) -> Result<SyncContext, String> {
    let engine = engine
        .read()
        .map_err(|e| format!("lock poisoned while reading sync context: {e}"))?;
    let catalog = engine.catalog();
    Ok(SyncContext {
        data_dir: catalog.data_dir().to_path_buf(),
        remote_lsn: catalog.max_lsn(),
        active_catalog_version: catalog.active_catalog_version(),
    })
}

/// Snapshot of the primary's sync-relevant state captured under the engine lock.
/// `active_catalog_version` is the database's *active* on-disk catalog format,
/// used to stamp the expected segment identity a pulling replica is checked
/// against (a database that never activated v6 keeps expecting v5).
pub(super) struct SyncContext {
    pub(super) data_dir: PathBuf,
    pub(super) remote_lsn: u64,
    pub(super) active_catalog_version: u16,
}

fn wire_repair_action(action: SyncRepairAction) -> WireSyncRepairAction {
    match action {
        SyncRepairAction::None => WireSyncRepairAction::None,
        SyncRepairAction::Pull => WireSyncRepairAction::Pull,
        SyncRepairAction::AwaitArchive => WireSyncRepairAction::AwaitArchive,
        SyncRepairAction::Rebootstrap => WireSyncRepairAction::Rebootstrap,
    }
}

fn wire_sync_status(status: ReplicaSyncStatus) -> WireSyncStatus {
    WireSyncStatus {
        replica_id: status.replica_id,
        active: status.active,
        last_applied_lsn: status.last_applied_lsn,
        remote_lsn: status.remote_lsn,
        servable_lsn: status.servable_lsn,
        unarchived_lsn: status.unarchived_lsn,
        lag_lsn: status.lag_lsn,
        lag_bytes: status.lag_bytes,
        lag_ms: status.lag_ms,
        stale: status.stale,
        repair_action: wire_repair_action(status.repair_action),
        last_sync_error: status.last_sync_error,
    }
}

fn wire_retained_unit(unit: RetainedUnit) -> WireRetainedUnit {
    WireRetainedUnit {
        tx_id: unit.tx_id,
        record_type: unit.record_type,
        lsn: unit.lsn,
        data: unit.data,
    }
}

fn sync_operation_outcome(message: &Message) -> SyncOutcome {
    match message {
        Message::SyncStatusResult { .. }
        | Message::SyncPullResult { .. }
        | Message::SyncAckResult { .. } => SyncOutcome::Ok,
        _ => SyncOutcome::Error,
    }
}

fn sync_operation_label(operation: SyncOperation) -> &'static str {
    match operation {
        SyncOperation::Status => "status",
        SyncOperation::Pull => "pull",
        SyncOperation::Ack => "ack",
    }
}

fn sync_pull_payload_bytes(units: &[WireRetainedUnit]) -> u64 {
    units.iter().fold(0, |total, unit| {
        total.saturating_add(unit.encoded_len().unwrap_or(0))
    })
}

fn sync_repair_label(action: WireSyncRepairAction) -> SyncRepairLabel {
    match action {
        WireSyncRepairAction::None => SyncRepairLabel::None,
        WireSyncRepairAction::Pull => SyncRepairLabel::Pull,
        WireSyncRepairAction::AwaitArchive => SyncRepairLabel::AwaitArchive,
        WireSyncRepairAction::Rebootstrap => SyncRepairLabel::Rebootstrap,
    }
}

fn wire_repair_action_label(action: WireSyncRepairAction) -> &'static str {
    match action {
        WireSyncRepairAction::None => "none",
        WireSyncRepairAction::Pull => "pull",
        WireSyncRepairAction::AwaitArchive => "await_archive",
        WireSyncRepairAction::Rebootstrap => "rebootstrap",
    }
}

const FNV1A64_OFFSET: u64 = 0xcbf29ce484222325;

const FNV1A64_PRIME: u64 = 0x100000001b3;

pub(super) const INVALID_REPLICA_FINGERPRINT: &str = "invalid";

pub(super) fn replica_fingerprint(replica_id: &str) -> String {
    let mut hash = FNV1A64_OFFSET;
    for byte in replica_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV1A64_PRIME);
    }
    format!("{hash:016x}")
}

pub(super) fn log_replica_fingerprint(replica_id: &str) -> String {
    if validate_wire_replica_id(replica_id).is_ok() {
        replica_fingerprint(replica_id)
    } else {
        INVALID_REPLICA_FINGERPRINT.to_string()
    }
}

#[derive(Debug, Clone)]
pub(super) struct SyncLogContext {
    pub(super) replica_fingerprint: String,
    pub(super) since_lsn: Option<u64>,
    pub(super) applied_lsn: Option<u64>,
    pub(super) observed_remote_lsn: Option<u64>,
    pub(super) max_units: Option<u32>,
    pub(super) max_bytes: Option<u64>,
}

impl SyncLogContext {
    pub(super) fn status(replica_id: &str) -> Self {
        Self::base(replica_id)
    }

    pub(super) fn pull(request: &SyncPullRequest) -> Self {
        Self {
            replica_fingerprint: log_replica_fingerprint(&request.replica_id),
            since_lsn: Some(request.since_lsn),
            applied_lsn: None,
            observed_remote_lsn: None,
            max_units: Some(request.max_units),
            max_bytes: Some(request.max_bytes),
        }
    }

    pub(super) fn ack(replica_id: &str, applied_lsn: u64, observed_remote_lsn: u64) -> Self {
        Self {
            replica_fingerprint: log_replica_fingerprint(replica_id),
            since_lsn: None,
            applied_lsn: Some(applied_lsn),
            observed_remote_lsn: Some(observed_remote_lsn),
            max_units: None,
            max_bytes: None,
        }
    }

    pub(super) fn base(replica_id: &str) -> Self {
        Self {
            replica_fingerprint: log_replica_fingerprint(replica_id),
            since_lsn: None,
            applied_lsn: None,
            observed_remote_lsn: None,
            max_units: None,
            max_bytes: None,
        }
    }
}

pub(super) struct SyncExecutionContext<'a> {
    pub(super) tx_gate: TxGate,
    pub(super) connection_has_transaction: bool,
    pub(super) operation: SyncOperation,
    pub(super) log_context: SyncLogContext,
    pub(super) metrics: &'a Arc<Metrics>,
    pub(super) query_timeout: Duration,
    pub(super) tx_wait_timeout: Duration,
    pub(super) credential_authenticated: bool,
    pub(super) principal: Option<Principal>,
    pub(super) pre_gate: SyncPreGate,
}

fn log_sync_decision(
    operation: SyncOperation,
    context: &SyncLogContext,
    elapsed: Duration,
    decision: &SyncDecision,
) {
    let operation = sync_operation_label(operation);
    let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
    match &decision.message {
        Message::SyncStatusResult { status } => {
            let repair_action = wire_repair_action_label(status.repair_action);
            if status.stale || status.repair_action != WireSyncRepairAction::None {
                info!(
                    operation = operation,
                    replica_fingerprint = %context.replica_fingerprint,
                    remote_lsn = status.remote_lsn,
                    last_applied_lsn = ?status.last_applied_lsn,
                    servable_lsn = ?status.servable_lsn,
                    unarchived_lsn = ?status.unarchived_lsn,
                    lag_lsn = ?status.lag_lsn,
                    lag_bytes = ?status.lag_bytes,
                    lag_ms = ?status.lag_ms,
                    stale = status.stale,
                    repair_action,
                    elapsed_ms,
                    "sync decision"
                );
            } else {
                debug!(
                    operation = operation,
                    replica_fingerprint = %context.replica_fingerprint,
                    remote_lsn = status.remote_lsn,
                    last_applied_lsn = ?status.last_applied_lsn,
                    repair_action,
                    elapsed_ms,
                    "sync decision"
                );
            }
        }
        Message::SyncPullResult {
            status,
            units,
            has_more,
        } => {
            let repair_action = wire_repair_action_label(status.repair_action);
            let units_len = units.len();
            let payload_bytes = sync_pull_payload_bytes(units);
            if *has_more || status.stale || status.repair_action != WireSyncRepairAction::None {
                info!(
                    operation = operation,
                    replica_fingerprint = %context.replica_fingerprint,
                    since_lsn = ?context.since_lsn,
                    max_units = ?context.max_units,
                    max_bytes = ?context.max_bytes,
                    units = units_len,
                    payload_bytes,
                    has_more = *has_more,
                    remote_lsn = status.remote_lsn,
                    last_applied_lsn = ?status.last_applied_lsn,
                    servable_lsn = ?status.servable_lsn,
                    unarchived_lsn = ?status.unarchived_lsn,
                    lag_lsn = ?status.lag_lsn,
                    stale = status.stale,
                    repair_action,
                    elapsed_ms,
                    "sync decision"
                );
            } else {
                debug!(
                    operation = operation,
                    replica_fingerprint = %context.replica_fingerprint,
                    since_lsn = ?context.since_lsn,
                    units = units_len,
                    payload_bytes,
                    has_more = *has_more,
                    remote_lsn = status.remote_lsn,
                    repair_action,
                    elapsed_ms,
                    "sync decision"
                );
            }
        }
        Message::SyncAckResult {
            previous_applied_lsn,
            applied_lsn,
            remote_lsn,
            advanced,
            status,
        } => {
            let repair_action = wire_repair_action_label(status.repair_action);
            if *advanced || status.stale || status.repair_action != WireSyncRepairAction::None {
                info!(
                    operation = operation,
                    replica_fingerprint = %context.replica_fingerprint,
                    requested_applied_lsn = ?context.applied_lsn,
                    observed_remote_lsn = ?context.observed_remote_lsn,
                    previous_applied_lsn = *previous_applied_lsn,
                    applied_lsn = *applied_lsn,
                    remote_lsn = *remote_lsn,
                    advanced = *advanced,
                    stale = status.stale,
                    repair_action,
                    elapsed_ms,
                    "sync decision"
                );
            } else {
                debug!(
                    operation = operation,
                    replica_fingerprint = %context.replica_fingerprint,
                    requested_applied_lsn = ?context.applied_lsn,
                    previous_applied_lsn = *previous_applied_lsn,
                    applied_lsn = *applied_lsn,
                    remote_lsn = *remote_lsn,
                    advanced = *advanced,
                    repair_action,
                    elapsed_ms,
                    "sync decision"
                );
            }
        }
        Message::Error { .. } | Message::ErrorWithClass { .. } => {
            warn!(
                operation = operation,
                replica_fingerprint = %context.replica_fingerprint,
                since_lsn = ?context.since_lsn,
                applied_lsn = ?context.applied_lsn,
                observed_remote_lsn = ?context.observed_remote_lsn,
                max_units = ?context.max_units,
                max_bytes = ?context.max_bytes,
                error_class = decision
                    .error_class
                    .unwrap_or(SyncErrorClass::Internal)
                    .as_label(),
                elapsed_ms,
                "sync decision rejected"
            );
        }
        _ => {
            debug!(
                operation = operation,
                replica_fingerprint = %context.replica_fingerprint,
                elapsed_ms,
                "unexpected sync decision response"
            );
        }
    }
}

fn trim_to_applyable_v1_prefix(
    raw_units: &mut Vec<RetainedUnit>,
    wire_units: &mut Vec<WireRetainedUnit>,
) -> Result<(), String> {
    let mut last_error = None;
    while !raw_units.is_empty() {
        match validate_v1_retained_units_applyable(raw_units) {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_error = Some(err.to_string());
                raw_units.pop();
                wire_units.pop();
            }
        }
    }
    if let Some(error) = last_error {
        return Err(format!(
            "sync pull cannot serve an applyable V1 retained chunk with current limits: {error}"
        ));
    }
    Ok(())
}

fn validate_sync_ack_applyable_boundary(
    data_dir: &Path,
    replica_id: &str,
    applied_lsn: u64,
    remote_lsn: u64,
) -> Result<(), String> {
    let status =
        replica_sync_status(data_dir, replica_id, remote_lsn).map_err(|err| err.to_string())?;
    let Some(previous_lsn) = status.last_applied_lsn else {
        return Ok(());
    };
    if applied_lsn <= previous_lsn {
        return Ok(());
    }
    let range_len = applied_lsn - previous_lsn;
    if range_len > u64::from(MAX_SYNC_PULL_UNITS) {
        return Err(format!(
            "sync ack range contains {range_len} units; acknowledge ranges no larger than {MAX_SYNC_PULL_UNITS}"
        ));
    }
    let max_units =
        usize::try_from(range_len).map_err(|_| "sync ack range is too large to validate")?;
    let identity = read_identity(data_dir).map_err(|err| err.to_string())?;
    let segment_dir = retained_segments_dir(data_dir);
    let units = read_units_through(
        &segment_dir,
        identity.segment_identity(),
        previous_lsn,
        applied_lsn,
        max_units,
    )
    .map_err(|err| err.to_string())?;
    if units.len() != max_units || units.last().map(|unit| unit.lsn) != Some(applied_lsn) {
        return Err(
            "sync ack does not cover a complete retained-unit range; rebootstrap required".into(),
        );
    }
    validate_v1_retained_units_applyable(&units).map_err(|err| err.to_string())
}

#[cfg(test)]
pub(super) fn dispatch_sync_status(
    engine: &Arc<RwLock<Engine>>,
    replica_id: String,
    credential_authenticated: bool,
    principal: Option<&Principal>,
) -> Message {
    dispatch_sync_status_decision(engine, replica_id, credential_authenticated, principal).message
}

pub(super) fn dispatch_sync_status_decision(
    engine: &Arc<RwLock<Engine>>,
    replica_id: String,
    credential_authenticated: bool,
    principal: Option<&Principal>,
) -> SyncDecision {
    // Everything decidable without the engine goes through the SAME pre-gate
    // the wire path applies before taking a permit, so the two can never
    // disagree about what is refusable for free. See
    // `no_sync_dispatch_function_refuses_a_frame_before_it_reads_the_engine`.
    let pre_gate = SyncPreGate::Status {
        replica_id: replica_id.clone(),
    };
    if let Err((class, message)) = pre_gate.check(credential_authenticated, principal) {
        return SyncDecision::error(class, message);
    }
    let SyncContext {
        data_dir,
        remote_lsn,
        ..
    } = match sync_context(engine) {
        Ok(context) => context,
        Err(message) => return SyncDecision::error(SyncErrorClass::SyncContext, message),
    };
    match replica_sync_status(&data_dir, &replica_id, remote_lsn) {
        Ok(status) => SyncDecision::ok(Message::SyncStatusResult {
            status: wire_sync_status(status),
        }),
        Err(err) => SyncDecision::error(SyncErrorClass::StatusRead, err.to_string()),
    }
}

#[cfg(test)]
pub(super) fn dispatch_sync_pull(
    engine: &Arc<RwLock<Engine>>,
    request: SyncPullRequest,
    credential_authenticated: bool,
    principal: Option<&Principal>,
) -> Message {
    dispatch_sync_pull_decision(engine, request, credential_authenticated, principal).message
}

pub(super) fn dispatch_sync_pull_decision(
    engine: &Arc<RwLock<Engine>>,
    request: SyncPullRequest,
    credential_authenticated: bool,
    principal: Option<&Principal>,
) -> SyncDecision {
    // See the note in `dispatch_sync_status_decision`: one pre-gate, shared.
    let pre_gate = SyncPreGate::Pull {
        replica_id: request.replica_id.clone(),
        max_units: request.max_units,
        max_bytes: request.max_bytes,
    };
    if let Err((class, message)) = pre_gate.check(credential_authenticated, principal) {
        return SyncDecision::error(class, message);
    }

    let SyncContext {
        data_dir,
        remote_lsn,
        active_catalog_version,
    } = match sync_context(engine) {
        Ok(context) => context,
        Err(message) => return SyncDecision::error(SyncErrorClass::SyncContext, message),
    };
    let status = match replica_sync_status(&data_dir, &request.replica_id, remote_lsn) {
        Ok(status) => status,
        Err(err) => {
            return SyncDecision::error(SyncErrorClass::StatusRead, err.to_string());
        }
    };
    let Some(cursor_lsn) = status.last_applied_lsn else {
        return SyncDecision::ok(Message::SyncPullResult {
            status: wire_sync_status(status),
            units: Vec::new(),
            has_more: false,
        });
    };
    if status.repair_action != SyncRepairAction::Pull {
        return SyncDecision::ok(Message::SyncPullResult {
            status: wire_sync_status(status),
            units: Vec::new(),
            has_more: false,
        });
    }
    if request.since_lsn != cursor_lsn {
        return SyncDecision::error(
            SyncErrorClass::CursorLsnMismatch,
            format!(
                "sync pull sinceLsn {} does not match primary cursor LSN {cursor_lsn}",
                request.since_lsn
            ),
        );
    }

    let identity = match read_identity(&data_dir) {
        Ok(identity) => identity,
        Err(err) => {
            return SyncDecision::error(SyncErrorClass::IdentityRead, err.to_string());
        }
    };
    // Stamp the expected identity with the database's *active* catalog version,
    // not this binary's compile-time maximum, so a database that never activated
    // v6 still expects v5 and accepts a v0.12 replica.
    let expected = SegmentIdentity::with_catalog_version(
        identity.database_id,
        identity.primary_generation,
        active_catalog_version,
    );
    if request.database_id != expected.database_id
        || request.primary_generation != expected.primary_generation
        || request.wal_format_version != expected.wal_format_version
        || request.segment_format_version != RETAINED_SEGMENT_FORMAT_VERSION
    {
        return SyncDecision::error(
            SyncErrorClass::IdentityOrFormatMismatch,
            "sync pull identity or format version mismatch; rebootstrap required",
        );
    }
    // The request states the maximum catalog format the replica can read. Accept
    // any replica that can read the active format (>= active); reject a replica
    // whose maximum is older than the data it would receive.
    if request.catalog_version < expected.catalog_version {
        return SyncDecision::error(
            SyncErrorClass::IdentityOrFormatMismatch,
            format!(
                "sync pull replica catalog format v{} cannot read this database's active catalog format v{}; rebootstrap with an upgraded replica required",
                request.catalog_version, expected.catalog_version
            ),
        );
    }

    let effective_max_units = request.max_units.min(MAX_SYNC_PULL_UNITS) as usize;
    let requested_through_lsn = request
        .since_lsn
        .saturating_add(request.max_units as u64)
        .min(remote_lsn)
        .min(status.servable_lsn.unwrap_or(request.since_lsn));
    let segment_dir = retained_segments_dir(&data_dir);
    if requested_through_lsn > request.since_lsn {
        if let Err(err) = validate_retained_tail_available(
            &segment_dir,
            expected,
            request.since_lsn,
            requested_through_lsn,
        ) {
            let mut rebootstrap_status = status;
            rebootstrap_status.stale = true;
            rebootstrap_status.repair_action = SyncRepairAction::Rebootstrap;
            rebootstrap_status.last_sync_error = Some(format!(
                "retained history is unavailable; rebootstrap required: {err}"
            ));
            return SyncDecision::ok(Message::SyncPullResult {
                status: wire_sync_status(rebootstrap_status),
                units: Vec::new(),
                has_more: false,
            });
        }
    }

    let raw_units = match read_units_through(
        &segment_dir,
        expected,
        request.since_lsn,
        requested_through_lsn,
        effective_max_units,
    ) {
        Ok(units) => units,
        Err(err) => {
            return SyncDecision::error(SyncErrorClass::RetainedRead, err.to_string());
        }
    };

    let mut selected_raw = Vec::new();
    let mut selected = Vec::new();
    let mut selected_bytes = 0u64;
    for unit in raw_units {
        let wire_unit = wire_retained_unit(unit.clone());
        let unit_bytes = match wire_unit.encoded_len() {
            Ok(bytes) => bytes,
            Err(message) => {
                return SyncDecision::error(SyncErrorClass::RetainedUnitEncoding, message);
            }
        };
        if selected_bytes.saturating_add(unit_bytes) > request.max_bytes {
            if selected.is_empty() {
                return SyncDecision::error(
                    SyncErrorClass::InvalidMaxBytes,
                    "sync pull maxBytes is too small for the next retained unit",
                );
            }
            break;
        }
        selected_bytes += unit_bytes;
        selected_raw.push(unit);
        selected.push(wire_unit);
    }
    if let Err(message) = trim_to_applyable_v1_prefix(&mut selected_raw, &mut selected) {
        return SyncDecision::error(SyncErrorClass::RetainedChunkNotApplyable, message);
    }

    let fetchable_through_lsn = status.servable_lsn.unwrap_or(remote_lsn).min(remote_lsn);
    let has_more = selected
        .last()
        .is_some_and(|unit| unit.lsn < fetchable_through_lsn);
    SyncDecision::ok(Message::SyncPullResult {
        status: wire_sync_status(status),
        units: selected,
        has_more,
    })
}

#[cfg(test)]
pub(super) fn dispatch_sync_ack(
    engine: &Arc<RwLock<Engine>>,
    replica_id: String,
    applied_lsn: u64,
    observed_remote_lsn: u64,
    credential_authenticated: bool,
    principal: Option<&Principal>,
) -> Message {
    dispatch_sync_ack_decision(
        engine,
        replica_id,
        applied_lsn,
        observed_remote_lsn,
        credential_authenticated,
        principal,
    )
    .message
}

pub(super) fn dispatch_sync_ack_decision(
    engine: &Arc<RwLock<Engine>>,
    replica_id: String,
    applied_lsn: u64,
    observed_remote_lsn: u64,
    credential_authenticated: bool,
    principal: Option<&Principal>,
) -> SyncDecision {
    // See the note in `dispatch_sync_status_decision`: one pre-gate, shared.
    let pre_gate = SyncPreGate::Ack {
        replica_id: replica_id.clone(),
        applied_lsn,
        observed_remote_lsn,
    };
    if let Err((class, message)) = pre_gate.check(credential_authenticated, principal) {
        return SyncDecision::error(class, message);
    }
    let SyncContext {
        data_dir,
        remote_lsn,
        ..
    } = match sync_context(engine) {
        Ok(context) => context,
        Err(message) => return SyncDecision::error(SyncErrorClass::SyncContext, message),
    };
    if observed_remote_lsn > remote_lsn {
        return SyncDecision::error(
            SyncErrorClass::LsnAheadOfRemote,
            format!(
                "sync ack remoteLsn {observed_remote_lsn} is ahead of primary LSN {remote_lsn}"
            ),
        );
    }
    if let Err(message) =
        validate_sync_ack_applyable_boundary(&data_dir, &replica_id, applied_lsn, remote_lsn)
    {
        return SyncDecision::error(SyncErrorClass::AckValidation, message);
    }
    match acknowledge_replica_apply(&data_dir, &replica_id, applied_lsn, remote_lsn) {
        Ok(summary) => SyncDecision::ok(Message::SyncAckResult {
            previous_applied_lsn: summary.previous_applied_lsn,
            applied_lsn: summary.applied_lsn,
            remote_lsn: summary.remote_lsn,
            advanced: summary.advanced,
            status: wire_sync_status(summary.status),
        }),
        Err(err) => SyncDecision::error(classify_sync_ack_failure(&err), err.to_string()),
    }
}

/// Split a failed cursor advance into "the replica must act" and "the server
/// failed". `acknowledge_replica_apply` rejects a missing cursor with
/// `NotFound` and an inactive or behind cursor with `InvalidInput`; every other
/// kind is a real I/O failure the replica cannot do anything about.
pub(super) fn classify_sync_ack_failure(err: &std::io::Error) -> SyncErrorClass {
    match err.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput => {
            SyncErrorClass::AckRejected
        }
        _ => SyncErrorClass::AckUpdate,
    }
}

pub(super) async fn run_blocking_sync<T, F>(input: T, query_timeout: Duration, f: F) -> SyncDecision
where
    T: Send + 'static,
    F: FnOnce(T) -> SyncDecision + Send + 'static,
{
    let mut handle = tokio::task::spawn_blocking(move || f(input));
    tokio::select! {
        result = &mut handle => match result {
            Ok(decision) => decision,
            Err(err) => SyncDecision::error(SyncErrorClass::Internal, format!("internal error: {err}")),
        },
        _ = tokio::time::sleep(query_timeout) => match handle.await {
            Ok(decision) => decision,
            Err(err) => SyncDecision::error(SyncErrorClass::Internal, format!("internal error: {err}")),
        },
    }
}

pub(super) async fn execute_gated_sync<T, F>(
    context: SyncExecutionContext<'_>,
    input: T,
    f: F,
) -> Message
where
    T: Send + 'static,
    F: FnOnce(T) -> SyncDecision + Send + 'static,
{
    let SyncExecutionContext {
        tx_gate,
        connection_has_transaction,
        operation,
        log_context,
        metrics,
        query_timeout,
        tx_wait_timeout,
        credential_authenticated,
        principal,
        pre_gate,
    } = context;
    let start = Instant::now();
    let reject = |decision: SyncDecision| -> Message {
        let elapsed = start.elapsed();
        log_sync_decision(operation, &log_context, elapsed, &decision);
        metrics.record_sync_operation(operation, elapsed, SyncOutcome::Error);
        decision.message
    };

    if connection_has_transaction {
        return reject(SyncDecision::error(
            SyncErrorClass::ActiveTransaction,
            "sync protocol is unavailable inside an active transaction",
        ));
    }
    // Every rejection that needs no engine access happens HERE, before the gate
    // is touched: a frame that will execute nothing must acquire nothing. The
    // decision functions run the SAME pre-gate as their first act, so a
    // non-wire caller gets the identical answer and the two can never drift
    // (`no_sync_dispatch_function_refuses_a_frame_before_it_reads_the_engine`).
    if let Err((class, message)) = pre_gate.check(credential_authenticated, principal.as_ref()) {
        return reject(SyncDecision::error(class, message));
    }

    let permit = match acquire_sync_permit(&tx_gate, tx_wait_timeout, metrics).await {
        Ok(permit) => permit,
        Err(decision) => return reject(decision),
    };
    let decision = run_blocking_sync(input, query_timeout, f).await;
    drop(permit);
    match &decision.message {
        Message::SyncStatusResult { status } => {
            metrics.record_sync_repair_action(operation, sync_repair_label(status.repair_action));
        }
        Message::SyncPullResult { status, units, .. } => {
            metrics.record_sync_repair_action(operation, sync_repair_label(status.repair_action));
            metrics.record_sync_pull_payload(units.len() as u64, sync_pull_payload_bytes(units));
        }
        Message::SyncAckResult {
            advanced, status, ..
        } => {
            metrics.record_sync_repair_action(operation, sync_repair_label(status.repair_action));
            if *advanced {
                metrics.inc_sync_ack_advanced();
            }
        }
        _ => {}
    }
    let elapsed = start.elapsed();
    log_sync_decision(operation, &log_context, elapsed, &decision);
    metrics.record_sync_operation(
        operation,
        elapsed,
        sync_operation_outcome(&decision.message),
    );
    decision.message
}

/// Acquire the whole TxGate for a sync frame, bounded by `tx_wait_timeout`
/// exactly like [`acquire_begin_permit`] and [`acquire_autocommit_permit`].
///
/// This was the only unbounded gate acquire in the server. A sync frame that
/// arrived while another connection held an explicit transaction waited out
/// that connection's ENTIRE hold: it blew straight through `tx_wait_timeout`,
/// wrote no error frame at all, and was invisible to
/// `powdb_tx_gate_timeouts_total` because only the two query-side acquires
/// recorded a timeout. Bounding it here makes every gate waiter, on every
/// frontend, give up on the same deadline with the same typed error.
pub(super) async fn acquire_sync_permit(
    tx_gate: &TxGate,
    tx_wait_timeout: Duration,
    metrics: &Arc<Metrics>,
) -> Result<OwnedSemaphorePermit, SyncDecision> {
    match tokio::time::timeout(
        tx_wait_timeout,
        tx_gate.clone().acquire_many_owned(tx_gate.permit_count()),
    )
    .await
    {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err(SyncDecision::error(
            SyncErrorClass::QueryExecution,
            "query execution error",
        )),
        Err(_) => {
            metrics.inc_tx_gate_timeout();
            Err(SyncDecision::error(
                SyncErrorClass::GateTimeout,
                format!(
                    "transaction gate timeout after {}ms waiting for concurrent transaction to complete",
                    tx_wait_timeout.as_millis()
                ),
            ))
        }
    }
}
