//! The transaction-gate parity matrix: every frontend, every refusal that
//! must happen before a permit is taken, and the structural guards that keep
//! the sync path from growing a new refusal under the gate.

use super::*;

// ---- Transaction-gate parity matrix ----
//
// The tests this replaces named three frontends by hand and stayed green
// for a year while a FOURTH (sync) seized the whole gate before running
// its own auth check, waited on it with no timeout at all, and answered
// with an untyped error frame.
//
// Naming frontends by hand was only half the defect. The first repair
// enumerated the FRONTENDS and then probed each one with a single
// hard-coded example, which exercises each RULE on exactly one frontend:
// pre-gate replica-id validation could be narrowed to `SyncStatus` alone,
// or dropped from `SyncPull` and `SyncAck`, with the whole suite still
// green. That is a spot check wearing a matrix's clothes.
//
// What follows is the CROSS PRODUCT of two enumerations:
//
//   axis 1  every rule a frame can be refused by BEFORE the gate
//   axis 2  every frontend that rule is reachable from
//
// and axis 2 is DERIVED rather than written down. A rule declares which
// SLOT of a runnable frame it corrupts, and it is probed on every frontend
// whose runnable frame has that slot. `SyncSlot::replica_id` is not
// optional, because every `SyncPreGate` variant carries a replica id and
// `SyncPreGate::check` destructures all three together, so a replica-id
// rule is probed on all three sync frontends by construction.
//
// The last test in this section closes the other direction: it enumerates
// every rejection site inside the sync dispatch functions straight out of
// `handler/sync.rs`, so a NEW refusal added under the gate fails the
// build until its author says whether it needs the engine.

use std::collections::{BTreeMap, BTreeSet};

/// Declare the gate-frontend enum and its iteration list together, so a
/// variant cannot exist without being in the list the matrix walks.
macro_rules! gate_frontends {
    ($($variant:ident),+ $(,)?) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
        enum GateFrontend { $($variant),+ }

        impl GateFrontend {
            const ALL: &'static [GateFrontend] = &[$(GateFrontend::$variant),+];
        }
    };
}

gate_frontends!(PowQl, Sql, Params, SyncStatus, SyncPull, SyncAck);

/// Declare the pre-gate rule enum and its iteration list together, for the
/// same reason: a rule cannot exist without the matrix walking it, and
/// walking it means probing every frontend it reaches.
macro_rules! gate_rules {
    ($($variant:ident),+ $(,)?) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
        enum GateRule { $($variant),+ }

        impl GateRule {
            const ALL: &'static [GateRule] = &[$(GateRule::$variant),+];
        }
    };
}

gate_rules!(
    UnparsableStatement,
    RoleForbidsStatement,
    SyncCredentialMissing,
    SyncRoleForbidsProtocol,
    SyncInvalidReplicaId,
    SyncPullMaxUnitsOutOfRange,
    SyncPullMaxBytesOutOfRange,
    SyncAckLsnAheadOfClientRemote,
    SyncInsideActiveTransaction,
);

/// Which frontend serves a wire frame, or `None` when the frame never
/// reaches the transaction gate.
///
/// Exhaustive on purpose: a new `Message` variant does not compile until
/// someone decides whether it can reach the gate, and answering "yes"
/// forces a `GateFrontend` variant, which the macro forces into `ALL`,
/// which the matrix forces probes for.
fn gate_frontend(msg: &Message) -> Option<GateFrontend> {
    match msg {
        // Six wire message types, three frontends: the native variants
        // differ only in result encoding and share the same wrapper.
        Message::Query { .. } | Message::QueryNative { .. } => Some(GateFrontend::PowQl),
        Message::QuerySql { .. } | Message::QuerySqlNative { .. } => Some(GateFrontend::Sql),
        Message::QueryWithParams { .. } | Message::QueryWithParamsNative { .. } => {
            Some(GateFrontend::Params)
        }
        Message::SyncStatus { .. } => Some(GateFrontend::SyncStatus),
        Message::SyncPull { .. } => Some(GateFrontend::SyncPull),
        Message::SyncAck { .. } => Some(GateFrontend::SyncAck),
        // Handshake frames, server responses, and control frames the main
        // loop answers without ever touching the gate.
        Message::Connect { .. }
        | Message::ConnectWithHello { .. }
        | Message::ConnectOk { .. }
        | Message::ConnectOkWithHello { .. }
        | Message::SyncStatusResult { .. }
        | Message::SyncPullResult { .. }
        | Message::SyncAckResult { .. }
        | Message::ResultRows { .. }
        | Message::ResultScalar { .. }
        | Message::ResultRowsNative { .. }
        | Message::ResultScalarNative { .. }
        | Message::ResultOk { .. }
        | Message::ResultMessage { .. }
        | Message::Error { .. }
        | Message::ErrorWithClass { .. }
        | Message::Disconnect
        | Message::Ping
        | Message::Pong => None,
    }
}

/// A wire frame served by `frontend`, used to prove the matrix and the
/// dispatch surface describe the same six frontends.
fn representative_frame(frontend: GateFrontend) -> Message {
    match frontend {
        GateFrontend::PowQl => Message::Query {
            query: "User".into(),
        },
        GateFrontend::Sql => Message::QuerySql {
            query: "SELECT * FROM User".into(),
        },
        GateFrontend::Params => Message::QueryWithParams {
            query: "User filter .id = $1".into(),
            params: vec![WireParam::Int(1)],
        },
        GateFrontend::SyncStatus => Message::SyncStatus {
            replica_id: "replica-a".into(),
        },
        GateFrontend::SyncPull => Message::SyncPull {
            replica_id: "replica-a".into(),
            since_lsn: 0,
            max_units: 1,
            max_bytes: 1024,
            database_id: [0; 16],
            primary_generation: 1,
            wal_format_version: 1,
            catalog_version: 6,
            segment_format_version: 1,
        },
        GateFrontend::SyncAck => Message::SyncAck {
            replica_id: "replica-a".into(),
            applied_lsn: 0,
            remote_lsn: 0,
        },
    }
}

/// The statement slot of a runnable frame. The unparsable and forbidden
/// variants live beside the runnable text, in the same language, so a rule
/// can swap in its violation without knowing which frontend it is looking
/// at.
#[derive(Clone)]
struct QuerySlot {
    text: String,
    unparsable: &'static str,
    forbidden_write: &'static str,
    params: Vec<WireParam>,
    principal: Option<Principal>,
}

/// The identity and cursor fields only a pull frame carries.
#[derive(Clone, Copy)]
struct PullIdentity {
    since_lsn: u64,
    database_id: [u8; 16],
    primary_generation: u64,
    wal_format_version: u16,
    catalog_version: u16,
    segment_format_version: u16,
}

/// The sync slot of a runnable frame.
///
/// `replica_id` is NOT optional. Every `SyncPreGate` variant carries one
/// (see the combined destructure in `SyncPreGate::check`), so every sync
/// frontend can be given an invalid replica id, and a rule that corrupts
/// the replica id is therefore probed on all three of them without anyone
/// listing which three.
#[derive(Clone)]
struct SyncSlot {
    replica_id: String,
    credential_authenticated: bool,
    principal: Option<Principal>,
    connection_has_transaction: bool,
    /// Present only on the frontend whose frame carries batch bounds.
    pull_bounds: Option<(u32, u64)>,
    /// Present only on the frontend whose frame carries the identity and
    /// cursor fields.
    pull_identity: Option<PullIdentity>,
    /// Present only on the frontend whose frame carries the two LSNs.
    ack_lsns: Option<(u64, u64)>,
}

/// A frame this frontend would run, before any rule corrupts it.
#[derive(Clone)]
struct ProbeSpec {
    frontend: GateFrontend,
    query: Option<QuerySlot>,
    sync: Option<SyncSlot>,
}

fn runnable_spec(frontend: GateFrontend) -> ProbeSpec {
    let query = |text: &str,
                 unparsable: &'static str,
                 forbidden_write: &'static str,
                 params: Vec<WireParam>| {
        Some(QuerySlot {
            text: text.to_string(),
            unparsable,
            forbidden_write,
            params,
            principal: None,
        })
    };
    let sync = |pull_bounds, pull_identity, ack_lsns| {
        Some(SyncSlot {
            replica_id: "replica-a".to_string(),
            credential_authenticated: true,
            principal: Some(admin_principal()),
            connection_has_transaction: false,
            pull_bounds,
            pull_identity,
            ack_lsns,
        })
    };
    let identity = PullIdentity {
        since_lsn: 0,
        database_id: [0; 16],
        primary_generation: 1,
        wal_format_version: 1,
        catalog_version: 6,
        segment_format_version: 1,
    };
    match frontend {
        GateFrontend::PowQl => ProbeSpec {
            frontend,
            query: query(
                "User",
                "this is not valid PowQL",
                "insert User { id := 2 }",
                Vec::new(),
            ),
            sync: None,
        },
        GateFrontend::Sql => ProbeSpec {
            frontend,
            query: query(
                "SELECT * FROM User",
                "SELEKT * FROM",
                "INSERT INTO User (id) VALUES (2)",
                Vec::new(),
            ),
            sync: None,
        },
        GateFrontend::Params => ProbeSpec {
            frontend,
            query: query(
                "User filter .id = $1",
                "User filter .id = = $1",
                "insert User { id := $1 }",
                vec![WireParam::Int(1)],
            ),
            sync: None,
        },
        GateFrontend::SyncStatus => ProbeSpec {
            frontend,
            query: None,
            sync: sync(None, None, None),
        },
        GateFrontend::SyncPull => ProbeSpec {
            frontend,
            query: None,
            sync: sync(Some((1, 1024)), Some(identity), None),
        },
        GateFrontend::SyncAck => ProbeSpec {
            frontend,
            query: None,
            sync: sync(None, None, Some((0, 0))),
        },
    }
}

impl GateRule {
    /// Corrupt `spec` so it violates exactly this rule, and report whether
    /// this frontend even HAS the slot the rule corrupts.
    ///
    /// Returning `false` is how "not reachable from this frontend" is
    /// derived. Nothing anywhere lists which frontends a rule applies to:
    /// the answer falls out of which slots `runnable_spec` gave them.
    fn violate(self, spec: &mut ProbeSpec) -> bool {
        match self {
            Self::UnparsableStatement => match spec.query.as_mut() {
                Some(slot) => {
                    slot.text = slot.unparsable.to_string();
                    true
                }
                None => false,
            },
            Self::RoleForbidsStatement => match spec.query.as_mut() {
                Some(slot) => {
                    slot.text = slot.forbidden_write.to_string();
                    slot.principal = principal("readonly");
                    true
                }
                None => false,
            },
            Self::SyncCredentialMissing => match spec.sync.as_mut() {
                Some(slot) => {
                    slot.credential_authenticated = false;
                    true
                }
                None => false,
            },
            Self::SyncRoleForbidsProtocol => match spec.sync.as_mut() {
                Some(slot) => {
                    slot.principal = principal("readonly");
                    true
                }
                None => false,
            },
            Self::SyncInvalidReplicaId => match spec.sync.as_mut() {
                Some(slot) => {
                    slot.replica_id = String::new();
                    true
                }
                None => false,
            },
            Self::SyncPullMaxUnitsOutOfRange => {
                match spec.sync.as_mut().and_then(|s| s.pull_bounds.as_mut()) {
                    Some(bounds) => {
                        bounds.0 = 0;
                        true
                    }
                    None => false,
                }
            }
            Self::SyncPullMaxBytesOutOfRange => {
                match spec.sync.as_mut().and_then(|s| s.pull_bounds.as_mut()) {
                    Some(bounds) => {
                        bounds.1 = MAX_SYNC_PULL_BYTES + 1;
                        true
                    }
                    None => false,
                }
            }
            Self::SyncAckLsnAheadOfClientRemote => {
                match spec.sync.as_mut().and_then(|s| s.ack_lsns.as_mut()) {
                    Some(lsns) => {
                        *lsns = (5, 0);
                        true
                    }
                    None => false,
                }
            }
            Self::SyncInsideActiveTransaction => match spec.sync.as_mut() {
                Some(slot) => {
                    slot.connection_has_transaction = true;
                    true
                }
                None => false,
            },
        }
    }

    /// The wire class this refusal must carry, whichever frontend answered.
    /// No arm is a fallback: each is the class the query frontends already
    /// use for that meaning.
    fn expected_class(self) -> ErrorClass {
        match self {
            Self::UnparsableStatement => ErrorClass::Parse,
            // Fixable only by reconnecting with credentials.
            Self::SyncCredentialMissing => ErrorClass::AuthFailed,
            // A caller-supplied bound outside the server's accepted range.
            Self::SyncPullMaxUnitsOutOfRange | Self::SyncPullMaxBytesOutOfRange => {
                ErrorClass::LimitExceeded
            }
            // The caller's own request is wrong and it can say so.
            Self::RoleForbidsStatement
            | Self::SyncRoleForbidsProtocol
            | Self::SyncInvalidReplicaId
            | Self::SyncAckLsnAheadOfClientRemote
            | Self::SyncInsideActiveTransaction => ErrorClass::Execution,
        }
    }

    /// The class `SyncPreGate::check` itself must answer with.
    ///
    /// `None` for the two query-side rules, which have no `SyncPreGate` at
    /// all, and for the active-transaction refusal, which
    /// `execute_gated_sync` makes before it consults the pre-gate.
    fn pre_gate_class(self) -> Option<SyncErrorClass> {
        match self {
            Self::UnparsableStatement
            | Self::RoleForbidsStatement
            | Self::SyncInsideActiveTransaction => None,
            Self::SyncCredentialMissing => Some(SyncErrorClass::AuthRequired),
            Self::SyncRoleForbidsProtocol => Some(SyncErrorClass::PermissionDenied),
            Self::SyncInvalidReplicaId => Some(SyncErrorClass::InvalidReplicaId),
            Self::SyncPullMaxUnitsOutOfRange => Some(SyncErrorClass::InvalidMaxUnits),
            Self::SyncPullMaxBytesOutOfRange => Some(SyncErrorClass::InvalidMaxBytes),
            Self::SyncAckLsnAheadOfClientRemote => Some(SyncErrorClass::LsnAheadOfRemote),
        }
    }
}

struct GateProbeEnv {
    engine: Arc<RwLock<Engine>>,
    gate: TxGate,
    metrics: Arc<Metrics>,
    tx_wait_timeout: Duration,
}

/// The pre-gate a sync frontend builds for this slot. Exhaustive on the
/// frontend, and every arm reads `slot.replica_id`, which is what makes the
/// replica-id rule reach all three.
fn sync_pre_gate(frontend: GateFrontend, slot: &SyncSlot) -> SyncPreGate {
    match frontend {
        GateFrontend::SyncStatus => SyncPreGate::Status {
            replica_id: slot.replica_id.clone(),
        },
        GateFrontend::SyncPull => {
            let (max_units, max_bytes) = slot.pull_bounds.expect("a pull frame carries bounds");
            SyncPreGate::Pull {
                replica_id: slot.replica_id.clone(),
                max_units,
                max_bytes,
            }
        }
        GateFrontend::SyncAck => {
            let (applied_lsn, observed_remote_lsn) =
                slot.ack_lsns.expect("an ack frame carries both LSNs");
            SyncPreGate::Ack {
                replica_id: slot.replica_id.clone(),
                applied_lsn,
                observed_remote_lsn,
            }
        }
        GateFrontend::PowQl | GateFrontend::Sql | GateFrontend::Params => {
            panic!("{frontend:?} is not a sync frontend")
        }
    }
}

fn sync_pull_request(slot: &SyncSlot) -> SyncPullRequest {
    let (max_units, max_bytes) = slot.pull_bounds.expect("a pull frame carries bounds");
    let identity = slot
        .pull_identity
        .expect("a pull frame carries its identity");
    SyncPullRequest {
        replica_id: slot.replica_id.clone(),
        since_lsn: identity.since_lsn,
        max_units,
        max_bytes,
        database_id: identity.database_id,
        primary_generation: identity.primary_generation,
        wal_format_version: identity.wal_format_version,
        catalog_version: identity.catalog_version,
        segment_format_version: identity.segment_format_version,
    }
}

/// Run the frontend's own decision function against `engine`, with no gate
/// and no pre-gate in the way. Used to ask what the code UNDER the gate
/// would decide.
fn dispatch_sync_decision(
    engine: &Arc<RwLock<Engine>>,
    frontend: GateFrontend,
    slot: &SyncSlot,
) -> SyncDecision {
    match frontend {
        GateFrontend::SyncStatus => dispatch_sync_status_decision(
            engine,
            slot.replica_id.clone(),
            slot.credential_authenticated,
            slot.principal.as_ref(),
        ),
        GateFrontend::SyncPull => dispatch_sync_pull_decision(
            engine,
            sync_pull_request(slot),
            slot.credential_authenticated,
            slot.principal.as_ref(),
        ),
        GateFrontend::SyncAck => {
            let (applied_lsn, observed_remote_lsn) =
                slot.ack_lsns.expect("an ack frame carries both LSNs");
            dispatch_sync_ack_decision(
                engine,
                slot.replica_id.clone(),
                applied_lsn,
                observed_remote_lsn,
                slot.credential_authenticated,
                slot.principal.as_ref(),
            )
        }
        GateFrontend::PowQl | GateFrontend::Sql | GateFrontend::Params => {
            panic!("{frontend:?} is not a sync frontend")
        }
    }
}

/// Run one probe frame through the real frontend that serves it, and
/// return the response the client would receive.
async fn run_gate_probe(env: &GateProbeEnv, spec: &ProbeSpec) -> Message {
    let (_client, server) = tokio::io::duplex(1024);
    let mut reader = BufReader::new(server);
    let mut wire_read_buffer = Vec::new();
    let mut pending_messages = InFlightReadAhead::default();
    let mut tx_permit = None;
    let engine = Arc::clone(&env.engine);
    let metrics = Arc::clone(&env.metrics);
    let query_timeout = Duration::from_secs(2);

    match spec.frontend {
        GateFrontend::PowQl => {
            let slot = spec
                .query
                .as_ref()
                .expect("a query frontend has a statement");
            execute_wire_query(
                QueryContext {
                    engine,
                    tx_gate: env.gate.clone(),
                    tx_permit: &mut tx_permit,
                    principal: slot.principal.clone(),
                    result_mode: WireResultMode::Native,
                    query_timeout,
                    tx_wait_timeout: env.tx_wait_timeout,
                    metrics: &metrics,
                    stream: FrameStream {
                        reader: &mut reader,
                        buffered: &mut wire_read_buffer,
                        pending: &mut pending_messages,
                    },
                },
                slot.text.clone(),
            )
            .await
            .0
        }
        GateFrontend::Sql => {
            let slot = spec
                .query
                .as_ref()
                .expect("a query frontend has a statement");
            execute_wire_query_sql(
                QueryContext {
                    engine,
                    tx_gate: env.gate.clone(),
                    tx_permit: &mut tx_permit,
                    principal: slot.principal.clone(),
                    result_mode: WireResultMode::Native,
                    query_timeout,
                    tx_wait_timeout: env.tx_wait_timeout,
                    metrics: &metrics,
                    stream: FrameStream {
                        reader: &mut reader,
                        buffered: &mut wire_read_buffer,
                        pending: &mut pending_messages,
                    },
                },
                slot.text.clone(),
            )
            .await
            .0
        }
        GateFrontend::Params => {
            let slot = spec
                .query
                .as_ref()
                .expect("a query frontend has a statement");
            execute_wire_query_with_params(
                QueryContext {
                    engine,
                    tx_gate: env.gate.clone(),
                    tx_permit: &mut tx_permit,
                    principal: slot.principal.clone(),
                    result_mode: WireResultMode::Native,
                    query_timeout,
                    tx_wait_timeout: env.tx_wait_timeout,
                    metrics: &metrics,
                    stream: FrameStream {
                        reader: &mut reader,
                        buffered: &mut wire_read_buffer,
                        pending: &mut pending_messages,
                    },
                },
                slot.text.clone(),
                slot.params.clone(),
            )
            .await
            .0
        }
        GateFrontend::SyncStatus | GateFrontend::SyncPull | GateFrontend::SyncAck => {
            let slot = spec
                .sync
                .as_ref()
                .expect("a sync frontend has a sync slot")
                .clone();
            let frontend = spec.frontend;
            let operation = match frontend {
                GateFrontend::SyncStatus => SyncOperation::Status,
                GateFrontend::SyncPull => SyncOperation::Pull,
                _ => SyncOperation::Ack,
            };
            let log_context = match frontend {
                GateFrontend::SyncStatus => SyncLogContext::status(&slot.replica_id),
                GateFrontend::SyncPull => SyncLogContext::pull(&sync_pull_request(&slot)),
                _ => {
                    let (applied_lsn, observed_remote_lsn) =
                        slot.ack_lsns.expect("an ack frame carries both LSNs");
                    SyncLogContext::ack(&slot.replica_id, applied_lsn, observed_remote_lsn)
                }
            };
            execute_gated_sync(
                SyncExecutionContext {
                    tx_gate: env.gate.clone(),
                    connection_has_transaction: slot.connection_has_transaction,
                    operation,
                    log_context,
                    metrics: &metrics,
                    query_timeout,
                    tx_wait_timeout: env.tx_wait_timeout,
                    credential_authenticated: slot.credential_authenticated,
                    principal: slot.principal.clone(),
                    pre_gate: sync_pre_gate(frontend, &slot),
                },
                (engine, frontend, slot),
                |(engine, frontend, slot)| dispatch_sync_decision(&engine, frontend, &slot),
            )
            .await
        }
    }
}

#[test]
fn every_gate_frontend_is_reachable_from_the_dispatch_surface() {
    for frontend in GateFrontend::ALL.iter().copied() {
        assert_eq!(
            gate_frontend(&representative_frame(frontend)),
            Some(frontend),
            "{frontend:?} is in the parity matrix but no wire frame dispatches to it"
        );
    }
    for msg in [Message::Ping, Message::Pong, Message::Disconnect] {
        assert_eq!(
            gate_frontend(&msg),
            None,
            "{msg:?} does not reach the transaction gate"
        );
    }
}

/// Every frontend that has a sync slot, derived from `runnable_spec`.
fn sync_frontends() -> Vec<GateFrontend> {
    GateFrontend::ALL
        .iter()
        .copied()
        .filter(|f| runnable_spec(*f).sync.is_some())
        .collect()
}

/// PARITY, axis 1 x axis 2: every pre-gate rule, on every frontend it
/// reaches, must be refused without acquiring a permit and with the class
/// that rule means. The held single-permit gate is the mutation check: any
/// frontend that queues for a permit before deciding blows the 250ms
/// budget, a fortieth of the 10s `tx_wait_timeout` it would be waiting on.
#[tokio::test]
async fn every_pre_gate_rule_is_refused_without_a_permit_on_every_frontend_it_reaches() {
    let (_dir, engine) = one_row_engine();
    let gate = new_tx_gate_with_permits(1);
    let metrics = Arc::new(Metrics::new());
    let held_reader = acquire_autocommit_permit(
        &gate,
        AdmissionMode::Reader,
        Duration::from_secs(1),
        &metrics,
    )
    .await
    .expect("held reader admission");
    assert_eq!(gate.available_permits(), 0);

    let env = GateProbeEnv {
        engine,
        gate: gate.clone(),
        metrics: Arc::clone(&metrics),
        tx_wait_timeout: Duration::from_secs(10),
    };

    let mut coverage: BTreeMap<GateRule, Vec<GateFrontend>> = BTreeMap::new();
    for rule in GateRule::ALL.iter().copied() {
        for frontend in GateFrontend::ALL.iter().copied() {
            let mut spec = runnable_spec(frontend);
            if !rule.violate(&mut spec) {
                continue;
            }
            coverage.entry(rule).or_default().push(frontend);
            let response =
                tokio::time::timeout(Duration::from_millis(250), run_gate_probe(&env, &spec))
                    .await
                    .unwrap_or_else(|_| {
                        panic!(
                            "{rule:?} on {frontend:?} waited on the transaction gate for a \
                             frame that executes nothing"
                        )
                    });
            match response {
                Message::ErrorWithClass { class, message } => assert_eq!(
                    class,
                    rule.expected_class(),
                    "{rule:?} on {frontend:?} carried the wrong class: {message}"
                ),
                other => panic!(
                    "{rule:?} on {frontend:?} answered without a wire error class: {other:?}"
                ),
            }
            assert_eq!(
                gate.available_permits(),
                0,
                "{rule:?} on {frontend:?} must acquire nothing"
            );
        }
        assert!(
            coverage.contains_key(&rule),
            "{rule:?} is enumerated but reaches no frontend: either the rule is dead or the \
             slot it corrupts was renamed out from under `violate`"
        );
    }

    // Derived, not listed: a rule that corrupts a field EVERY sync frame
    // carries must have been probed on EVERY sync frontend. This is the
    // assertion the one-example-per-frontend matrix could not make, and it
    // is the one that fails when a pre-gate check is narrowed to a single
    // sync message type.
    let sync = sync_frontends();
    for rule in GateRule::ALL.iter().copied() {
        let universal = sync.iter().all(|frontend| {
            let mut probe = runnable_spec(*frontend);
            rule.violate(&mut probe)
        });
        if universal {
            assert_eq!(
                coverage[&rule], sync,
                "{rule:?} corrupts a field every sync frame carries but was probed on a subset"
            );
        }
    }

    assert!(
        metrics.render().contains("powdb_tx_gate_timeouts_total 0"),
        "no frontend may report a gate timeout it never waited for"
    );
    drop(held_reader);
    assert_eq!(gate.available_permits(), 1);
}

/// The same cross product asserted directly against `SyncPreGate::check`,
/// with no gate, no timing, and no engine in the way.
///
/// The wire-level matrix above catches a narrowed pre-gate check by timing
/// out on a held gate; this one catches it by name, on every sync frontend,
/// in microseconds.
#[test]
fn every_sync_pre_gate_rule_is_checked_on_every_sync_frontend() {
    for frontend in sync_frontends() {
        let runnable = runnable_spec(frontend);
        let slot = runnable.sync.as_ref().expect("a sync frontend has a slot");
        assert!(
            sync_pre_gate(frontend, slot)
                .check(slot.credential_authenticated, slot.principal.as_ref())
                .is_ok(),
            "{frontend:?}'s runnable frame must pass the pre-gate, or every probe below \
             proves nothing"
        );
    }

    for rule in GateRule::ALL.iter().copied() {
        let Some(expected) = rule.pre_gate_class() else {
            continue;
        };
        let mut checked = 0usize;
        for frontend in sync_frontends() {
            let mut spec = runnable_spec(frontend);
            if !rule.violate(&mut spec) {
                continue;
            }
            let slot = spec.sync.as_ref().expect("a sync frontend has a slot");
            match sync_pre_gate(frontend, slot)
                .check(slot.credential_authenticated, slot.principal.as_ref())
            {
                Err((class, message)) => assert_eq!(
                    class, expected,
                    "{rule:?} on {frontend:?} was refused as {class:?}, not {expected:?}: \
                     {message}"
                ),
                Ok(()) => panic!(
                    "{rule:?} is not checked pre-gate on {frontend:?}: this frame reaches \
                     `acquire_sync_permit` and takes the whole transaction gate before the \
                     dispatch function refuses it"
                ),
            }
            checked += 1;
        }
        assert!(
            checked > 0,
            "{rule:?} declares a pre-gate class but reaches no sync frontend"
        );
    }
}

/// PARITY: every frontend's gate acquire is bounded by `tx_wait_timeout`,
/// answers with a typed `Timeout`, and is counted. The sync frontend had
/// none of the three: it waited out the whole hold, wrote no error frame,
/// and never touched the counter, so a starved replica was invisible.
#[tokio::test]
async fn every_gate_frontend_bounds_its_acquire_and_counts_the_timeout() {
    let (_dir, engine) = one_row_engine();
    let gate = new_tx_gate_with_permits(1);
    let metrics = Arc::new(Metrics::new());
    let tx_wait_timeout = Duration::from_millis(150);
    let held_reader = acquire_autocommit_permit(
        &gate,
        AdmissionMode::Reader,
        Duration::from_secs(1),
        &metrics,
    )
    .await
    .expect("held reader admission");

    let env = GateProbeEnv {
        engine,
        gate: gate.clone(),
        metrics: Arc::clone(&metrics),
        tx_wait_timeout,
    };
    for (waited, frontend) in GateFrontend::ALL.iter().copied().enumerate() {
        let started = Instant::now();
        let spec = runnable_spec(frontend);
        let response = tokio::time::timeout(Duration::from_secs(5), run_gate_probe(&env, &spec))
            .await
            .unwrap_or_else(|_| panic!("{frontend:?} acquires the gate with no timeout at all"));
        assert!(
            started.elapsed() >= tx_wait_timeout,
            "{frontend:?} gave up before its wait elapsed"
        );
        match response {
            Message::ErrorWithClass { class, message } => {
                assert_eq!(class, ErrorClass::Timeout, "{frontend:?}: {message}");
                assert!(
                    message.contains("transaction gate timeout"),
                    "{frontend:?} did not name the gate wait: {message}"
                );
            }
            other => panic!("{frontend:?} answered without a wire error class: {other:?}"),
        }
        assert!(
            metrics
                .render()
                .contains(&format!("powdb_tx_gate_timeouts_total {}", waited + 1)),
            "{frontend:?} timed out on the gate without counting it"
        );
    }
    drop(held_reader);
}

/// Poison the engine lock so any read of it fails. Every sync dispatch
/// function reaches the engine through `sync_context`, so after this the
/// ONLY answers they can still give are the ones that need no engine at
/// all, plus `SyncContext` itself.
fn poison_engine_lock(engine: &Arc<RwLock<Engine>>) {
    let poisoner = Arc::clone(engine);
    let _ = std::thread::spawn(move || {
        let _guard = poisoner.write().expect("lock is not poisoned yet");
        panic!("poisoning the engine lock on purpose (expected in this test)");
    })
    .join();
    assert!(
        engine.read().is_err(),
        "the engine lock did not end up poisoned; this test proves nothing without it"
    );
}

/// One-field-at-a-time sweep from this frontend's runnable frame. A refusal
/// keyed on any single request field shows up here.
fn sync_corpus(frontend: GateFrontend) -> Vec<SyncSlot> {
    let base = runnable_spec(frontend)
        .sync
        .expect("a sync frontend has a slot");
    let mut out = vec![base.clone()];
    let mut push = |mutate: &dyn Fn(&mut SyncSlot)| {
        let mut slot = base.clone();
        mutate(&mut slot);
        out.push(slot);
    };

    for authenticated in [false, true] {
        push(&|slot: &mut SyncSlot| slot.credential_authenticated = authenticated);
    }
    for role in ["readonly", "admin", "no-such-role"] {
        push(&|slot: &mut SyncSlot| slot.principal = principal(role));
    }
    push(&|slot: &mut SyncSlot| slot.principal = None);
    let long_id = "x".repeat(129);
    for replica_id in ["", "replica-a", "bad id!", long_id.as_str()] {
        push(&|slot: &mut SyncSlot| slot.replica_id = replica_id.to_string());
    }
    if base.pull_bounds.is_some() {
        for units in [
            0u32,
            1,
            MAX_SYNC_PULL_UNITS,
            MAX_SYNC_PULL_UNITS + 1,
            u32::MAX,
        ] {
            push(&|slot: &mut SyncSlot| {
                if let Some(bounds) = slot.pull_bounds.as_mut() {
                    bounds.0 = units;
                }
            });
        }
        for bytes in [
            0u64,
            1,
            MAX_SYNC_PULL_BYTES,
            MAX_SYNC_PULL_BYTES + 1,
            u64::MAX,
        ] {
            push(&|slot: &mut SyncSlot| {
                if let Some(bounds) = slot.pull_bounds.as_mut() {
                    bounds.1 = bytes;
                }
            });
        }
    }
    if base.pull_identity.is_some() {
        for since_lsn in [0u64, 1, 2, 3, 4, 5, 6, 7, 8, u64::MAX] {
            push(&|slot: &mut SyncSlot| {
                if let Some(identity) = slot.pull_identity.as_mut() {
                    identity.since_lsn = since_lsn;
                }
            });
        }
        for database_id in [[0u8; 16], [0xffu8; 16]] {
            push(&|slot: &mut SyncSlot| {
                if let Some(identity) = slot.pull_identity.as_mut() {
                    identity.database_id = database_id;
                }
            });
        }
        for generation in [0u64, 1, u64::MAX] {
            push(&|slot: &mut SyncSlot| {
                if let Some(identity) = slot.pull_identity.as_mut() {
                    identity.primary_generation = generation;
                }
            });
        }
        for wal in [0u16, 1, u16::MAX] {
            push(&|slot: &mut SyncSlot| {
                if let Some(identity) = slot.pull_identity.as_mut() {
                    identity.wal_format_version = wal;
                }
            });
        }
        for catalog in [0u16, 5, 6, 7, u16::MAX] {
            push(&|slot: &mut SyncSlot| {
                if let Some(identity) = slot.pull_identity.as_mut() {
                    identity.catalog_version = catalog;
                }
            });
        }
        for segment in [0u16, 1, u16::MAX] {
            push(&|slot: &mut SyncSlot| {
                if let Some(identity) = slot.pull_identity.as_mut() {
                    identity.segment_format_version = segment;
                }
            });
        }
    }
    if base.ack_lsns.is_some() {
        for lsns in [
            (0u64, 0u64),
            (0, 1),
            (1, 0),
            (1, 1),
            (5, 0),
            (u64::MAX, 0),
            (0, u64::MAX),
            (u64::MAX, u64::MAX),
        ] {
            push(&|slot: &mut SyncSlot| slot.ack_lsns = Some(lsns));
        }
    }
    out
}

/// PARITY, the other direction: a refusal the dispatch functions can reach
/// WITHOUT reading the engine must also be a pre-gate refusal.
///
/// This is the runtime half of the guard against a pure rejection sitting
/// under the gate. With the engine lock poisoned every engine read fails,
/// so any answer other than `SyncContext` is by construction one the server
/// could have given before taking a single permit. If the pre-gate does not
/// give that same answer, the frame takes the entire gate on its way to
/// being refused, which is exactly the outage this whole section exists to
/// prevent.
#[test]
fn every_engine_free_sync_refusal_is_also_a_pre_gate_refusal() {
    let (_dir, engine) = one_row_engine();
    poison_engine_lock(&engine);

    let mut observed_pure: BTreeSet<&'static str> = BTreeSet::new();
    let mut saw_engine_dependent = false;
    for frontend in sync_frontends() {
        for slot in sync_corpus(frontend) {
            let decision = dispatch_sync_decision(&engine, frontend, &slot);
            let class = decision.error_class.unwrap_or_else(|| {
                panic!(
                    "{frontend:?} answered successfully with a poisoned engine: {:?}",
                    decision.message
                )
            });
            if class == SyncErrorClass::SyncContext {
                saw_engine_dependent = true;
                continue;
            }
            observed_pure.insert(class.as_label());
            match sync_pre_gate(frontend, &slot)
                .check(slot.credential_authenticated, slot.principal.as_ref())
            {
                Err((pre_gate_class, _)) => assert_eq!(
                    pre_gate_class, class,
                    "{frontend:?} refuses replica {:?} as {class:?} without reading the \
                     engine, but the pre-gate refuses it as {pre_gate_class:?}",
                    slot.replica_id
                ),
                Ok(()) => panic!(
                    "{frontend:?} refuses replica {:?} as {class:?} without ever reading the \
                     engine, and the pre-gate lets it through: that refusal runs under the \
                     whole transaction gate. Move the check into `SyncPreGate::check`, add a \
                     `GateRule` for it, and the matrix will probe it on every frontend it \
                     reaches.",
                    slot.replica_id
                ),
            }
        }
    }
    assert!(
        saw_engine_dependent,
        "no corpus entry reached the engine, so this test proved nothing"
    );
    for rule in GateRule::ALL.iter().copied() {
        if let Some(class) = rule.pre_gate_class() {
            assert!(
                observed_pure.contains(class.as_label()),
                "{rule:?} declares the pre-gate class {class:?} but no corpus frame produced \
                 it, so the sweep does not actually cover that rule"
            );
        }
    }
}

/// Every rejection site inside the gated sync path, declared.
///
/// `(function, SyncErrorClass, occurrences)`. The test below reads this
/// file's own source and rebuilds the same table; a mismatch means someone
/// added, removed, or moved a way for a sync frame to be refused.
///
/// This exists because the wire-level and pre-gate matrices above can only
/// probe rules they know about. A BRAND-NEW refusal added inside a dispatch
/// function is invisible to both of them: it is decided under the gate, so
/// a frame that will be refused in microseconds still waits out another
/// connection's entire transaction first. Failing the build until the
/// author classifies the new site is the only way that stays caught.
///
/// When this test fails: if the new refusal needs NO engine access, move it
/// into `SyncPreGate::check` and add a `GateRule` for it, which makes the
/// matrix probe it on every frontend it reaches. If it genuinely needs the
/// engine, it belongs under the gate, and updating this table is the whole
/// fix.
const DECLARED_SYNC_REJECTION_SITES: &[(&str, &str, usize)] = &[
    ("acquire_sync_permit", "GateTimeout", 1),
    ("acquire_sync_permit", "QueryExecution", 1),
    ("classify_sync_ack_failure", "AckRejected", 1),
    ("classify_sync_ack_failure", "AckUpdate", 1),
    ("dispatch_sync_ack_decision", "AckValidation", 1),
    ("dispatch_sync_ack_decision", "LsnAheadOfRemote", 1),
    ("dispatch_sync_ack_decision", "SyncContext", 1),
    ("dispatch_sync_pull_decision", "CursorLsnMismatch", 1),
    ("dispatch_sync_pull_decision", "IdentityOrFormatMismatch", 2),
    ("dispatch_sync_pull_decision", "IdentityRead", 1),
    ("dispatch_sync_pull_decision", "InvalidMaxBytes", 1),
    (
        "dispatch_sync_pull_decision",
        "RetainedChunkNotApplyable",
        1,
    ),
    ("dispatch_sync_pull_decision", "RetainedRead", 1),
    ("dispatch_sync_pull_decision", "RetainedUnitEncoding", 1),
    ("dispatch_sync_pull_decision", "StatusRead", 1),
    ("dispatch_sync_pull_decision", "SyncContext", 1),
    ("dispatch_sync_status_decision", "StatusRead", 1),
    ("dispatch_sync_status_decision", "SyncContext", 1),
    ("execute_gated_sync", "ActiveTransaction", 1),
    ("run_blocking_sync", "Internal", 2),
];

/// Everything a sync dispatch function may do before it reads the engine,
/// declared: clone what it needs for the pre-gate, consult the pre-gate,
/// and build the pre-gate's refusal. Nothing else.
///
/// `Err` is on the list because the scan below is syntactic and cannot
/// tell `Err(..)` in a pattern from a call; it decides nothing either way.
const PRE_ENGINE_CALLS_ALLOWED_IN_SYNC_DISPATCH: &[&str] =
    &["Err", "SyncDecision::error", "check", "clone"];

/// The three dispatch functions run under the whole transaction gate, so a
/// refusal they make BEFORE they read the engine is a refusal another
/// connection's open transaction can delay by minutes. Structurally there
/// may be exactly one such refusal per function, and it must be the shared
/// pre-gate the wire path already applied.
///
/// WHAT THIS CATCHES. A new refusal written into one of these three
/// functions ahead of the engine read, in each form it can reach source:
/// a second `SyncDecision::error(`; a second early `return` (any early exit
/// from a function returning `SyncDecision` needs one, since there is no
/// `?` to hide behind); or a call to a helper holding either. The last one
/// is why the permitted calls are declared rather than counted: moving the
/// refusal behind `fn refuse_x() -> SyncDecision` or
/// `fn refuse_x(..) -> Option<SyncDecision>` changes nothing this test can
/// see about `SyncDecision::error`, but it cannot avoid naming `refuse_x`
/// here.
///
/// WHAT THIS CANNOT CATCH. It reads `handler/sync.rs`'s text, so a refusal added
/// inside something it already permits is invisible to it: a new check
/// inside `SyncPreGate::check`, or inside `sync_context`, or in another
/// module entirely. The pre-gate is the sanctioned place for exactly that,
/// and `every_pre_gate_class_has_a_rule_the_matrix_walks` forces every
/// class it can answer with to have a `GateRule` beside it, which is what
/// makes the matrix probe it. So this test is a fence around the one region
/// the matrices cannot reach, not a proof that no free refusal exists
/// anywhere. The load-bearing half of this section is the runtime parity
/// matrix: `every_sync_pre_gate_rule_is_checked_on_every_sync_frontend`
/// and the wire-level matrix above it, which probe behavior on every
/// frontend instead of reading text.
#[test]
fn no_sync_dispatch_function_refuses_a_frame_before_it_reads_the_engine() {
    let src = include_str!("../sync.rs");
    for name in [
        "dispatch_sync_status_decision",
        "dispatch_sync_pull_decision",
        "dispatch_sync_ack_decision",
    ] {
        let body = top_level_fn_body(src, name);
        let engine_read = body
            .find("sync_context(engine)")
            .unwrap_or_else(|| panic!("{name} no longer reads the engine through sync_context"));
        let pre_gate_check = body
            .find("pre_gate.check(credential_authenticated, principal)")
            .unwrap_or_else(|| {
                panic!(
                    "{name} no longer delegates to `SyncPreGate::check`, so the pre-gate and \
                     the gated path can drift apart again"
                )
            });
        assert!(
            pre_gate_check < engine_read,
            "{name} consults the pre-gate only after reading the engine"
        );
        let before_engine = &body[..engine_read];

        let early = before_engine.match_indices("SyncDecision::error(").count();
        assert_eq!(
            early, 1,
            "{name} refuses a frame {early} times before it reads the engine. Only the shared \
             pre-gate may do that: any other refusal there needs no engine access, which \
             means it is being decided while this frame holds the whole transaction gate. \
             Move it into `SyncPreGate::check` and give it a `GateRule`, and the parity \
             matrix will probe it on every frontend it reaches."
        );

        let returns = word_occurrences(before_engine, "return");
        assert_eq!(
            returns, 1,
            "{name} leaves itself {returns} times before it reads the engine. Exactly one \
             early exit may live there, the pre-gate's; a second one is a refusal decided \
             while this frame holds the whole transaction gate, even when the decision \
             itself was made inside a helper."
        );

        let mut unexpected: Vec<String> = called_names(before_engine)
            .into_iter()
            .filter(|called| {
                called != name && !PRE_ENGINE_CALLS_ALLOWED_IN_SYNC_DISPATCH.contains(&&**called)
            })
            .collect();
        unexpected.sort();
        unexpected.dedup();
        assert!(
            unexpected.is_empty(),
            "{name} calls {unexpected:?} before it reads the engine. Whatever those decide is \
             decided while this frame holds the whole transaction gate. If they can refuse \
             the frame, the refusal belongs in `SyncPreGate::check` with a `GateRule` beside \
             it; if they genuinely cannot, add them to \
             PRE_ENGINE_CALLS_ALLOWED_IN_SYNC_DISPATCH and say why."
        );
    }
}

/// Occurrences of `word` in `src` that are whole tokens, so `return` does
/// not match `returns` and `fn` does not match `fnord`.
fn word_occurrences(src: &str, word: &str) -> usize {
    let bytes = src.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    src.match_indices(word)
        .filter(|(idx, _)| {
            let before_ok = *idx == 0 || !is_ident(bytes[idx - 1]);
            let after = idx + word.len();
            let after_ok = after == bytes.len() || !is_ident(bytes[after]);
            before_ok && after_ok
        })
        .count()
}

/// Every name that is CALLED in `src`: `foo(` as `foo`, `Type::assoc(` as
/// `Type::assoc`, `x.method(` as `method`, and `mac!(` as `mac!`.
///
/// Deliberately syntactic. It is not trying to understand the code, only
/// to make a function that appears in a region where nothing may decide
/// anything impossible to add silently.
fn called_names(src: &str) -> Vec<String> {
    let bytes = src.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut names = Vec::new();
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte != b'(' {
            continue;
        }
        let mut end = idx;
        let macro_call = end > 0 && bytes[end - 1] == b'!';
        if macro_call {
            end -= 1;
        }
        let mut start = end;
        while start > 0 && is_ident(bytes[start - 1]) {
            start -= 1;
        }
        if start == end {
            // A `(` that opens a group, a tuple, or an argument list.
            continue;
        }
        // Keep any `Type::` qualification, so `SyncDecision::error` cannot
        // be permitted by declaring a bare `error`.
        let mut path = start;
        while path >= 2 && bytes[path - 1] == b':' && bytes[path - 2] == b':' {
            let mut segment = path - 2;
            while segment > 0 && is_ident(bytes[segment - 1]) {
                segment -= 1;
            }
            if segment == path - 2 {
                break;
            }
            path = segment;
        }
        let mut name = src[path..end].to_string();
        if macro_call {
            name.push('!');
        }
        names.push(name);
    }
    names
}

/// The body of an `impl` block, from its header to the closing brace in
/// column 0.
fn impl_block<'a>(src: &'a str, header: &str) -> &'a str {
    let start = src
        .find(header)
        .unwrap_or_else(|| panic!("`{header}` is not in sync.rs"));
    let rest = &src[start..];
    let end = rest
        .find("\n}\n")
        .unwrap_or_else(|| panic!("`{header}` has no closing brace in column 0"));
    &rest[..end]
}

/// Every class the pre-gate can answer with must have a `GateRule`, and
/// every `GateRule` class must be one the pre-gate can answer with.
///
/// Without this, a new pre-gate check would be safe from the gate but
/// invisible to the matrix, so it could be applied to one sync frontend and
/// forgotten on the other two: the exact partial application this section
/// exists to make impossible.
#[test]
fn every_pre_gate_class_has_a_rule_the_matrix_walks() {
    let src = include_str!("../sync.rs");
    let mut produced: BTreeSet<String> = BTreeSet::new();
    let mut scan = |body: &str| {
        for (idx, _) in body.match_indices("SyncErrorClass::") {
            let tail = &body[idx + "SyncErrorClass::".len()..];
            produced.insert(
                tail.chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect(),
            );
        }
    };
    scan(impl_block(src, "\nimpl SyncPreGate {"));
    for name in [
        "check_sync_protocol_permitted",
        "check_sync_pull_bounds",
        "check_sync_ack_lsn_bounds",
    ] {
        scan(top_level_fn_body(src, name));
    }

    let declared: BTreeSet<String> = GateRule::ALL
        .iter()
        .filter_map(|rule| rule.pre_gate_class())
        .map(|class| format!("{class:?}"))
        .collect();
    assert_eq!(
        produced, declared,
        "the pre-gate and the rule enumeration disagree about what can be refused for free. \
         Every class on the left needs a `GateRule`, which is what makes the matrix probe it \
         on every frontend it reaches; every class on the right must actually be reachable \
         from the pre-gate."
    );
}

/// The body of a top-level function, from its signature to the closing
/// brace in column 0. The visibility qualifier is left out, so `pub(super)`
/// does not read as a call to something named `pub` in [`called_names`].
///
/// The name is matched as a whole token: `fn run_blocking_sync` must not
/// resolve to `fn run_blocking_sync_preflight` declared earlier in the
/// file. A prefix match there is not a near miss, it is a hole: the guards
/// below would then count rejection sites in a decoy and never look at the
/// function they name, and every one of them would pass while saying
/// nothing.
fn top_level_fn_body<'a>(src: &'a str, name: &str) -> &'a str {
    let bytes = src.as_bytes();
    let mut starts: Vec<usize> = Vec::new();
    for visibility in ["", "pub ", "pub(crate) ", "pub(super) "] {
        for asyncness in ["", "async "] {
            let needle = format!("\n{visibility}{asyncness}fn {name}");
            for (idx, _) in src.match_indices(&needle) {
                // The declaration ends the name here, rather than
                // continuing it: `(` for a plain function, `<` for a
                // generic one.
                match bytes.get(idx + needle.len()) {
                    Some(b'(') | Some(b'<') => starts.push(idx + 1 + visibility.len()),
                    _ => {}
                }
            }
        }
    }
    starts.sort_unstable();
    starts.dedup();
    assert!(
        starts.len() <= 1,
        "`fn {name}` is declared {} times at the top level of sync.rs; the guards below \
         would inspect one of them and ignore the rest",
        starts.len()
    );
    let start = starts.first().copied().unwrap_or_else(|| {
        panic!("`fn {name}` is not in sync.rs; the rejection-site guard cannot see it")
    });
    let rest = &src[start..];
    let end = rest
        .find("\n}\n")
        .unwrap_or_else(|| panic!("`fn {name}` has no closing brace in column 0"));
    &rest[..end]
}

#[test]
fn every_gated_sync_rejection_site_is_declared() {
    let src = include_str!("../sync.rs");
    let mut actual: BTreeMap<(&str, String), usize> = BTreeMap::new();
    for name in [
        "acquire_sync_permit",
        "classify_sync_ack_failure",
        "dispatch_sync_ack_decision",
        "dispatch_sync_pull_decision",
        "dispatch_sync_status_decision",
        "execute_gated_sync",
        "run_blocking_sync",
    ] {
        let body = top_level_fn_body(src, name);
        for (idx, _) in body.match_indices("SyncErrorClass::") {
            let tail = &body[idx + "SyncErrorClass::".len()..];
            let class: String = tail
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            *actual.entry((name, class)).or_insert(0) += 1;
        }
    }

    let declared: BTreeMap<(&str, String), usize> = DECLARED_SYNC_REJECTION_SITES
        .iter()
        .map(|(function, class, count)| ((*function, (*class).to_string()), *count))
        .collect();

    if actual != declared {
        let rendered = actual
            .iter()
            .map(|((function, class), count)| {
                format!("        (\"{function}\", \"{class}\", {count}),")
            })
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "the gated sync path gained, lost, or moved a way to refuse a frame.\n\n\
             If the new refusal needs NO engine access it must move into \
             `SyncPreGate::check` with a `GateRule` beside it, so the parity matrix probes it \
             on every frontend it reaches; a pure refusal decided under the gate makes a \
             frame wait out another connection's whole transaction before being told no.\n\
             If it genuinely needs the engine, paste this over \
             DECLARED_SYNC_REJECTION_SITES:\n\n{rendered}\n"
        );
    }
}

#[tokio::test]
async fn forbidden_write_is_denied_without_acquiring_any_permit() {
    let (_dir, engine) = one_row_engine();
    // A single-permit gate whose only permit is already held: writer
    // admission takes the whole gate, so any acquisition would have to
    // wait out the full tx_wait_timeout below.
    let gate = new_tx_gate_with_permits(1);
    let metrics = Arc::new(Metrics::new());
    let held_reader = acquire_autocommit_permit(
        &gate,
        AdmissionMode::Reader,
        Duration::from_secs(1),
        &metrics,
    )
    .await
    .expect("held reader admission");
    assert_eq!(gate.available_permits(), 0);

    let (_client, server) = tokio::io::duplex(1024);
    let mut reader = BufReader::new(server);
    let mut wire_read_buffer = Vec::new();
    let mut pending_messages = InFlightReadAhead::default();
    let mut tx_permit = None;
    let (message, ticket, termination) = tokio::time::timeout(
        Duration::from_millis(250),
        execute_wire_query(
            QueryContext {
                engine,
                tx_gate: gate.clone(),
                tx_permit: &mut tx_permit,
                principal: principal("readonly"),
                result_mode: WireResultMode::Native,
                query_timeout: Duration::from_secs(2),
                tx_wait_timeout: Duration::from_secs(10),
                metrics: &metrics,
                stream: FrameStream {
                    reader: &mut reader,
                    buffered: &mut wire_read_buffer,
                    pending: &mut pending_messages,
                },
            },
            "insert User { id := 2 }".into(),
        ),
    )
    .await
    .expect("a statement the principal may not run must never wait on the gate");

    match message {
        Message::ErrorWithClass { message, class } => {
            assert!(
                message.contains("permission denied"),
                "unexpected message: {message}"
            );
            assert_eq!(class, ErrorClass::Execution);
        }
        other => panic!("expected a typed permission error, got {other:?}"),
    }
    assert!(ticket.is_none());
    assert!(termination.is_none());
    assert!(tx_permit.is_none());
    assert_eq!(
        gate.available_permits(),
        0,
        "a statement the principal may not run must acquire nothing"
    );
    drop(held_reader);
    assert_eq!(gate.available_permits(), 1);
    assert!(metrics
        .render()
        .contains("powdb_queries_total{result=\"error\"} 1"));
}

#[tokio::test]
async fn forbidden_write_is_denied_without_a_permit_on_every_frontend() {
    let (_dir, engine) = one_row_engine();
    let gate = new_tx_gate_with_permits(1);
    let metrics = Arc::new(Metrics::new());
    let _held_reader = acquire_autocommit_permit(
        &gate,
        AdmissionMode::Reader,
        Duration::from_secs(1),
        &metrics,
    )
    .await
    .expect("held reader admission");

    let (_sql_client, sql_server) = tokio::io::duplex(1024);
    let mut reader = BufReader::new(sql_server);
    let mut wire_read_buffer = Vec::new();
    let mut pending_messages = InFlightReadAhead::default();
    let mut tx_permit = None;
    let (message, _, _) = tokio::time::timeout(
        Duration::from_millis(250),
        execute_wire_query_sql(
            QueryContext {
                engine: Arc::clone(&engine),
                tx_gate: gate.clone(),
                tx_permit: &mut tx_permit,
                principal: principal("readonly"),
                result_mode: WireResultMode::Native,
                query_timeout: Duration::from_secs(2),
                tx_wait_timeout: Duration::from_secs(10),
                metrics: &metrics,
                stream: FrameStream {
                    reader: &mut reader,
                    buffered: &mut wire_read_buffer,
                    pending: &mut pending_messages,
                },
            },
            "INSERT INTO User (id) VALUES (2)".into(),
        ),
    )
    .await
    .expect("a forbidden SQL write must never wait on the gate");
    assert!(
        matches!(
            &message,
            Message::ErrorWithClass { message, class: ErrorClass::Execution }
                if message.contains("permission denied")
        ),
        "unexpected SQL response: {message:?}"
    );

    let (_params_client, params_server) = tokio::io::duplex(1024);
    let mut reader = BufReader::new(params_server);
    let mut wire_read_buffer = Vec::new();
    let mut pending_messages = InFlightReadAhead::default();
    let mut tx_permit = None;
    let (message, _, _) = tokio::time::timeout(
        Duration::from_millis(250),
        execute_wire_query_with_params(
            QueryContext {
                engine,
                tx_gate: gate.clone(),
                tx_permit: &mut tx_permit,
                principal: principal("readonly"),
                result_mode: WireResultMode::Native,
                query_timeout: Duration::from_secs(2),
                tx_wait_timeout: Duration::from_secs(10),
                metrics: &metrics,
                stream: FrameStream {
                    reader: &mut reader,
                    buffered: &mut wire_read_buffer,
                    pending: &mut pending_messages,
                },
            },
            "insert User { id := $1 }".into(),
            vec![WireParam::Int(2)],
        ),
    )
    .await
    .expect("a forbidden parameterized write must never wait on the gate");
    assert!(
        matches!(
            &message,
            Message::ErrorWithClass { message, class: ErrorClass::Execution }
                if message.contains("permission denied")
        ),
        "unexpected parameterized response: {message:?}"
    );
    assert_eq!(gate.available_permits(), 0);
}

#[tokio::test]
async fn unparsable_flood_cannot_starve_a_concurrent_reader() {
    let (_dir, engine) = one_row_engine();
    let gate = new_tx_gate_with_permits(2);
    let metrics = Arc::new(Metrics::new());
    // One connection is mid-read and holds reader admission, so writer
    // admission (the whole gate) stays unavailable for as long as it runs.
    let _held_reader = acquire_autocommit_permit(
        &gate,
        AdmissionMode::Reader,
        Duration::from_secs(1),
        &metrics,
    )
    .await
    .expect("held reader admission");

    let flood_engine = Arc::clone(&engine);
    let flood_gate = gate.clone();
    let flood_metrics = Arc::clone(&metrics);
    let flood = tokio::spawn(async move {
        let (_client, server) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(server);
        let mut wire_read_buffer = Vec::new();
        let mut pending_messages = InFlightReadAhead::default();
        let mut tx_permit = None;
        for _ in 0..64 {
            let (message, _, _) = execute_wire_query(
                QueryContext {
                    engine: Arc::clone(&flood_engine),
                    tx_gate: flood_gate.clone(),
                    tx_permit: &mut tx_permit,
                    principal: None,
                    result_mode: WireResultMode::Native,
                    query_timeout: Duration::from_secs(2),
                    tx_wait_timeout: Duration::from_secs(10),
                    metrics: &flood_metrics,
                    stream: FrameStream {
                        reader: &mut reader,
                        buffered: &mut wire_read_buffer,
                        pending: &mut pending_messages,
                    },
                },
                ")))".into(),
            )
            .await;
            assert!(
                matches!(
                    message,
                    Message::ErrorWithClass {
                        class: ErrorClass::Parse,
                        ..
                    }
                ),
                "unexpected flood response: {message:?}"
            );
        }
    });
    // Give the flood time to queue for the gate if it takes admission.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (_reader_client, reader_server) = tokio::io::duplex(1024);
    let mut reader = BufReader::new(reader_server);
    let mut wire_read_buffer = Vec::new();
    let mut pending_messages = InFlightReadAhead::default();
    let mut tx_permit = None;
    let (message, _, _) = tokio::time::timeout(
        Duration::from_millis(500),
        execute_wire_query(
            QueryContext {
                engine,
                tx_gate: gate.clone(),
                tx_permit: &mut tx_permit,
                principal: None,
                result_mode: WireResultMode::Native,
                query_timeout: Duration::from_secs(2),
                tx_wait_timeout: Duration::from_secs(10),
                metrics: &metrics,
                stream: FrameStream {
                    reader: &mut reader,
                    buffered: &mut wire_read_buffer,
                    pending: &mut pending_messages,
                },
            },
            "User filter .id = 1".into(),
        ),
    )
    .await
    .expect("a real reader must complete while unparsable frames flood another connection");
    assert!(matches!(message, Message::ResultRowsNative { .. }));
    flood.await.expect("flood task");
}

#[tokio::test]
async fn writer_admission_excludes_readers() {
    let gate = new_tx_gate();
    let metrics = Arc::new(Metrics::new());
    let writer = acquire_begin_permit(&gate, Duration::from_secs(1), &metrics)
        .await
        .expect("writer admission");
    let blocked = acquire_autocommit_permit(
        &gate,
        AdmissionMode::Reader,
        Duration::from_millis(25),
        &metrics,
    )
    .await;
    assert!(blocked.is_err(), "reader must wait behind writer admission");
    drop(writer);
    let _reader = acquire_autocommit_permit(
        &gate,
        AdmissionMode::Reader,
        Duration::from_secs(1),
        &metrics,
    )
    .await
    .expect("reader must proceed after writer releases");
}

#[tokio::test]
async fn queued_writer_is_not_starved_by_later_readers() {
    let gate = new_tx_gate();
    let metrics = Arc::new(Metrics::new());
    let first_reader = acquire_autocommit_permit(
        &gate,
        AdmissionMode::Reader,
        Duration::from_secs(1),
        &metrics,
    )
    .await
    .expect("first reader admission");

    let writer_gate = gate.clone();
    let writer_metrics = metrics.clone();
    let mut writer = tokio::spawn(async move {
        acquire_begin_permit(&writer_gate, Duration::from_secs(1), &writer_metrics).await
    });
    tokio::time::sleep(Duration::from_millis(10)).await;

    let late_gate = gate.clone();
    let late_metrics = metrics.clone();
    let mut late_reader = tokio::spawn(async move {
        acquire_autocommit_permit(
            &late_gate,
            AdmissionMode::Reader,
            Duration::from_secs(1),
            &late_metrics,
        )
        .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut late_reader)
            .await
            .is_err(),
        "a later reader must queue behind the waiting writer"
    );

    drop(first_reader);
    let writer_permit = tokio::time::timeout(Duration::from_secs(1), &mut writer)
        .await
        .expect("writer must acquire once prior readers drain")
        .expect("writer task")
        .expect("writer admission");
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut late_reader)
            .await
            .is_err(),
        "writer must hold exclusive admission before the late reader"
    );
    drop(writer_permit);
    let _late_reader = tokio::time::timeout(Duration::from_secs(1), late_reader)
        .await
        .expect("late reader must eventually acquire")
        .expect("late reader task")
        .expect("late reader admission");
}
