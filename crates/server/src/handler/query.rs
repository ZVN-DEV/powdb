//! The query frontends (PowQL, SQL, parameterized PowQL) and the shared
//! transaction-routing state machine they execute one wire frame through.

use crate::metrics::{Metrics, QueryOutcome};
use crate::protocol::{ErrorClass, Message, WireParam};
use powdb_query::executor::{is_read_only_statement, Engine, WalDurabilityTicket};
use powdb_query::parser;
use powdb_query::result::{QueryError, QueryResult};
use powdb_query::sql;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::io::AsyncRead;
use tokio::sync::OwnedSemaphorePermit;
use tracing::debug;

use super::auth::{check_statement_permitted, Principal};
use super::classify::{classify_query_error, error_response, sanitize_error};
use super::transaction::{
    acquire_autocommit_permit, acquire_begin_permit, parsed_transaction_control,
    rollback_connection_transaction, statement_admission, AdmissionMode, TransactionControl,
    TxGate,
};
use super::wire::{
    is_query_cancellation_response, is_success_response, query_result_to_message,
    read_message_cancel_safe, ConnectionTermination, DecodedWireMessage, FrameStream,
    WireResultMode, MAX_IN_FLIGHT_READ_AHEAD_BYTES, MAX_IN_FLIGHT_READ_AHEAD_FRAMES,
};

/// Maximum query text length accepted from the wire (1 MB).
pub(super) const MAX_QUERY_LENGTH: usize = 1024 * 1024;

/// Log a received query with every literal value redacted.
///
/// Query text is user data: `filter .email = "ada@example.com"` would otherwise
/// put a real address into a log that is shipped and retained. We log the
/// literal-free shape plus the plan-cache canonical hash, which is enough to
/// identify the query, correlate repeats, and match it to a cached plan, and
/// carries no values. See [`crate::redact`].
pub(super) fn log_received_query(peer: &str, query: &str, message: &'static str) {
    debug!(
        peer = %peer,
        query_shape = %crate::redact::redact_query_literals(query),
        query_hash = ?crate::redact::query_shape_hash(query),
        query_len = query.len(),
        message
    );
}

/// [`log_received_query`] for the parameterized paths, where the bound values
/// travel outside the text and are never logged at all.
pub(super) fn log_received_query_with_params(
    peer: &str,
    query: &str,
    n_params: usize,
    message: &'static str,
) {
    debug!(
        peer = %peer,
        query_shape = %crate::redact::redact_query_literals(query),
        query_hash = ?crate::redact::query_shape_hash(query),
        query_len = query.len(),
        n_params,
        message
    );
}

/// Execute a query against the engine under the RwLock. Read-only
/// statements acquire `.read()` so concurrent SELECTs can scan in
/// parallel; mutations acquire `.write()`.
///
/// When `principal` is `Some`, the user's role is enforced first: a role
/// without the `Write` permission (i.e. `readonly`) gets a clean
/// "permission denied" error for any non-read statement, before any lock
/// is taken or any engine state is touched.
/// A statement's execution result plus its (not-yet-waited) WAL durability
/// obligation. The ticket is settled by [`finalize_durability`] AFTER the
/// TxGate permit is dropped, so overlapping committers on other connections
/// can share a single fsync (group commit).
type DispatchOutcome = (Result<QueryResult, QueryError>, Option<WalDurabilityTicket>);

/// Execute a mutating statement under the engine write lock with WAL group
/// commit: the commit's fsync obligation is registered (not performed) while
/// the lock is held, then the lock is dropped and the un-waited ticket is
/// returned. The caller must settle the ticket (via [`finalize_durability`])
/// before acknowledging the statement — and must do so with the TxGate
/// permit already released, or committers can never overlap.
fn execute_write_deferred(
    engine: &Arc<RwLock<Engine>>,
    f: impl FnOnce(&mut Engine) -> Result<QueryResult, QueryError>,
) -> DispatchOutcome {
    let mut eng = match engine.write() {
        Ok(eng) => eng,
        Err(e) => {
            return (
                Err(QueryError::Execution(format!("lock poisoned: {e}"))),
                None,
            )
        }
    };
    eng.run_with_deferred_durability(f)
}

pub(super) fn dispatch_query(
    engine: &Arc<RwLock<Engine>>,
    query: &str,
    principal: Option<&Principal>,
    allow_readonly_escalation: bool,
) -> DispatchOutcome {
    let stmt_result = parser::parse(query).map_err(|e| e.to_string());
    dispatch_query_parsed(
        engine,
        query,
        &stmt_result,
        principal,
        allow_readonly_escalation,
    )
}

fn dispatch_query_parsed(
    engine: &Arc<RwLock<Engine>>,
    query: &str,
    stmt_result: &Result<powdb_query::ast::Statement, String>,
    principal: Option<&Principal>,
    allow_readonly_escalation: bool,
) -> DispatchOutcome {
    // Role enforcement happens on the parsed AST. Statements that fail to
    // parse fall through — the engine returns the parse error itself and
    // can never execute anything for them.
    if let Ok(stmt) = &stmt_result {
        if let Err(e) = check_statement_permitted(principal, stmt) {
            return (Err(e), None);
        }
    }

    let can_try_read = matches!(&stmt_result, Ok(s) if is_read_only_statement(s));
    if can_try_read {
        let res = {
            let eng = match engine.read() {
                Ok(eng) => eng,
                Err(e) => {
                    return (
                        Err(QueryError::Execution(format!("lock poisoned: {e}"))),
                        None,
                    )
                }
            };
            eng.execute_powql_readonly(query)
        };
        match res {
            Ok(r) => return (Ok(r), None),
            Err(QueryError::ReadonlyNeedsWrite) => {
                if !allow_readonly_escalation {
                    return (Err(QueryError::ReadonlyNeedsWrite), None);
                }
                // The caller already owns writer admission, so it is safe to
                // fall through and retry under the engine write lock.
            }
            Err(e) => return (Err(e), None),
        }
    }

    if matches!(
        parsed_transaction_control(stmt_result),
        Some(TransactionControl::Rollback)
    ) {
        let mut eng = match engine.write() {
            Ok(eng) => eng,
            Err(e) => {
                return (
                    Err(QueryError::Execution(format!("lock poisoned: {e}"))),
                    None,
                )
            }
        };
        return (execute_rollback_preserving_sync_if_needed(&mut eng), None);
    }
    execute_write_deferred(engine, |eng| eng.execute_powql(query))
}

#[cfg(test)]
pub(super) fn dispatch_sql_query(
    engine: &Arc<RwLock<Engine>>,
    query: &str,
    principal: Option<&Principal>,
    allow_readonly_escalation: bool,
) -> DispatchOutcome {
    let stmt_result = sql::parse_sql(query).map_err(|e| e.to_string());
    dispatch_sql_query_parsed(
        engine,
        query,
        &stmt_result,
        principal,
        allow_readonly_escalation,
    )
}

fn dispatch_sql_query_parsed(
    engine: &Arc<RwLock<Engine>>,
    query: &str,
    stmt_result: &Result<powdb_query::ast::Statement, String>,
    principal: Option<&Principal>,
    allow_readonly_escalation: bool,
) -> DispatchOutcome {
    if let Ok(stmt) = &stmt_result {
        if let Err(e) = check_statement_permitted(principal, stmt) {
            return (Err(e), None);
        }
    }

    let can_try_read = matches!(&stmt_result, Ok(s) if is_read_only_statement(s));
    if can_try_read {
        let res = {
            let eng = match engine.read() {
                Ok(eng) => eng,
                Err(e) => {
                    return (
                        Err(QueryError::Execution(format!("lock poisoned: {e}"))),
                        None,
                    )
                }
            };
            eng.execute_sql_readonly(query)
        };
        match res {
            Ok(r) => return (Ok(r), None),
            Err(QueryError::ReadonlyNeedsWrite) => {
                if !allow_readonly_escalation {
                    return (Err(QueryError::ReadonlyNeedsWrite), None);
                }
            }
            Err(e) => return (Err(e), None),
        }
    }

    if matches!(
        parsed_transaction_control(stmt_result),
        Some(TransactionControl::Rollback)
    ) {
        let mut eng = match engine.write() {
            Ok(eng) => eng,
            Err(e) => {
                return (
                    Err(QueryError::Execution(format!("lock poisoned: {e}"))),
                    None,
                )
            }
        };
        return (execute_rollback_preserving_sync_if_needed(&mut eng), None);
    }
    execute_write_deferred(engine, |eng| eng.execute_sql(query))
}

/// Convert a wire parameter into the query-crate [`ParamValue`] used for
/// token-level binding.
fn wire_param_to_value(p: &WireParam) -> powdb_query::ast::ParamValue {
    use powdb_query::ast::ParamValue;
    match p {
        WireParam::Null => ParamValue::Null,
        WireParam::Int(v) => ParamValue::Int(*v),
        WireParam::Float(v) => ParamValue::Float(*v),
        WireParam::Bool(v) => ParamValue::Bool(*v),
        WireParam::Str(s) => ParamValue::Str(s.clone()),
    }
}

/// The terminal error a read-only (snapshot-serving) server returns for a
/// mutating statement, transaction-control frame, or a read that needs a writer
/// (a stale materialized view). The connection stays usable afterward.
fn readonly_terminal_message() -> Message {
    error_response(
        sanitize_error(&QueryError::ReadonlyMode.to_string()),
        ErrorClass::ReadonlyRefused,
    )
}

#[cfg(test)]
pub(super) fn classify_query_admission(query: &str) -> AdmissionMode {
    parser::parse(query)
        .map(|stmt| statement_admission(&stmt))
        .unwrap_or(AdmissionMode::Writer)
}

#[cfg(test)]
pub(super) fn classify_sql_admission(query: &str) -> AdmissionMode {
    sql::parse_sql(query)
        .map(|stmt| statement_admission(&stmt))
        .unwrap_or(AdmissionMode::Writer)
}

#[cfg(test)]
pub(super) fn classify_params_admission(query: &str, params: &[WireParam]) -> AdmissionMode {
    let bound: Vec<powdb_query::ast::ParamValue> = params.iter().map(wire_param_to_value).collect();
    parser::parse_with_params(query, &bound)
        .map(|stmt| statement_admission(&stmt))
        .unwrap_or(AdmissionMode::Writer)
}

fn execute_rollback_preserving_sync_if_needed(
    engine: &mut Engine,
) -> Result<QueryResult, QueryError> {
    engine.rollback_transaction_preserving_wal_archive()
}

#[cfg(test)]
pub(super) fn dispatch_query_with_params(
    engine: &Arc<RwLock<Engine>>,
    query: &str,
    params: &[WireParam],
    principal: Option<&Principal>,
    allow_readonly_escalation: bool,
) -> DispatchOutcome {
    let bound: Vec<powdb_query::ast::ParamValue> = params.iter().map(wire_param_to_value).collect();

    // Parse once (with params bound) so role enforcement and read/write
    // classification see exactly the statement that will execute.
    let stmt_result = parser::parse_with_params(query, &bound).map_err(|e| e.to_string());
    dispatch_query_with_bound_params_parsed(
        engine,
        query,
        &bound,
        &stmt_result,
        principal,
        allow_readonly_escalation,
    )
}

/// Parameterized counterpart of [`dispatch_query`]. Routes through the exact
/// same role-enforcement and read/write escalation logic, but binds the
/// `$N` placeholders at the token level via the query crate's
/// `parse_with_params` path. A string parameter can never change the query's
/// shape — it is substituted as a literal token, not interpolated text.
fn dispatch_query_with_bound_params_parsed(
    engine: &Arc<RwLock<Engine>>,
    query: &str,
    bound: &[powdb_query::ast::ParamValue],
    stmt_result: &Result<powdb_query::ast::Statement, String>,
    principal: Option<&Principal>,
    allow_readonly_escalation: bool,
) -> DispatchOutcome {
    if let Ok(stmt) = &stmt_result {
        if let Err(e) = check_statement_permitted(principal, stmt) {
            return (Err(e), None);
        }
    }

    let can_try_read = matches!(&stmt_result, Ok(s) if is_read_only_statement(s));
    if can_try_read {
        let res = {
            let eng = match engine.read() {
                Ok(eng) => eng,
                Err(e) => {
                    return (
                        Err(QueryError::Execution(format!("lock poisoned: {e}"))),
                        None,
                    )
                }
            };
            eng.execute_powql_readonly_with_params(query, bound)
        };
        match res {
            Ok(r) => return (Ok(r), None),
            Err(QueryError::ReadonlyNeedsWrite) => {
                if !allow_readonly_escalation {
                    return (Err(QueryError::ReadonlyNeedsWrite), None);
                }
            }
            Err(e) => return (Err(e), None),
        }
    }

    if matches!(
        parsed_transaction_control(stmt_result),
        Some(TransactionControl::Rollback)
    ) {
        let mut eng = match engine.write() {
            Ok(eng) => eng,
            Err(e) => {
                return (
                    Err(QueryError::Execution(format!("lock poisoned: {e}"))),
                    None,
                )
            }
        };
        return (execute_rollback_preserving_sync_if_needed(&mut eng), None);
    }
    execute_write_deferred(engine, |eng| eng.execute_powql_with_params(query, bound))
}

/// Reject a frame whose text failed to parse, before the TxGate is touched.
///
/// A statement that executes nothing must acquire nothing. Admission is
/// derived from the parsed AST and fails closed to [`AdmissionMode::Writer`],
/// so routing an unparsable frame into the state machine made it queue for
/// every gate permit (and then the engine write lock) only to have the engine
/// return the same parse error: any principal, including a readonly role,
/// could hold the whole gate by looping on garbage. The error is still
/// counted so `powdb_queries_total{result="error"}` stays truthful.
pub(super) fn parse_failure_response(
    message: &str,
    metrics: &Arc<Metrics>,
    start: Instant,
) -> (
    Message,
    Option<PendingDurability>,
    Option<ConnectionTermination>,
) {
    metrics.record_query(start.elapsed(), QueryOutcome::Error);
    (
        error_response(sanitize_error(message), ErrorClass::Parse),
        None,
        None,
    )
}

/// Reject a frame the principal's role may not run, before the TxGate is
/// touched.
///
/// Same rule as [`parse_failure_response`], and the same gap it closed: a
/// PARSEABLE write from a readonly role took full writer admission (the whole
/// gate) and queued for the engine write lock before the dispatcher reached
/// [`check_statement_permitted`] and refused it. That variant needs no garbage
/// at all, just `insert T { ... }` from a role without `Write`. Denying here
/// makes the permit acquisition unreachable for a statement that will never
/// execute; the dispatcher still re-checks, so the boundary stays enforced for
/// every non-wire caller.
fn permission_denied_response(
    denied: &QueryError,
    metrics: &Arc<Metrics>,
    start: Instant,
) -> (
    Message,
    Option<PendingDurability>,
    Option<ConnectionTermination>,
) {
    metrics.record_query(start.elapsed(), QueryOutcome::Error);
    (
        error_response(
            sanitize_error(&denied.to_string()),
            classify_query_error(denied),
        ),
        None,
        None,
    )
}

/// Everything one wire query frame is executed against: the connection's
/// engine handle, the admission gate and the permit it may already hold, the
/// identity it runs as, the budgets it runs under, and the socket it keeps
/// watching while the query runs.
///
/// The three dialect entry points and the state machine they share all need
/// exactly this set, which is why it is a type rather than eleven arguments
/// repeated four times. Passing it by value also encodes the ownership rule
/// the old argument list only implied: a frame is served by one of these at a
/// time, and the connection gets its socket and its permit back when the frame
/// is done with them.
pub(super) struct QueryContext<'a, R> {
    pub(super) engine: Arc<RwLock<Engine>>,
    pub(super) tx_gate: TxGate,
    pub(super) tx_permit: &'a mut Option<OwnedSemaphorePermit>,
    pub(super) principal: Option<Principal>,
    pub(super) result_mode: WireResultMode,
    pub(super) query_timeout: Duration,
    pub(super) tx_wait_timeout: Duration,
    pub(super) metrics: &'a Arc<Metrics>,
    pub(super) stream: FrameStream<'a, R>,
}

/// One parsed wire frame plus the routing facts derived from its AST.
///
/// The dialects differ only in how they reach this: PowQL, SQL, and
/// parameterized PowQL each parse their own text and then hand the state
/// machine the same four facts. Keeping them together is what lets the state
/// machine take one frame argument instead of four positional ones whose order
/// nothing but the compiler was checking.
struct ParsedFrame<Inner> {
    /// The dialect's already-parsed frame, shared with the rare dirty-view
    /// retry rather than re-parsed for it.
    query: Arc<Inner>,
    /// Its transaction-control classification, if it is one.
    tx_control: Option<TransactionControl>,
    /// The admission mode a bare (non-transaction) statement takes.
    autocommit_admission: AdmissionMode,
    /// When this frame's execution must have self-terminated by.
    query_deadline: Instant,
}

/// One dispatch of a frame onto the blocking pool: what the closure needs, the
/// budgets that bound it, and the socket its completion races.
pub(super) struct BlockingQuery<'a, R> {
    pub(super) engine: Arc<RwLock<Engine>>,
    pub(super) principal: Option<Principal>,
    pub(super) result_mode: WireResultMode,
    pub(super) query_timeout: Duration,
    pub(super) query_deadline: Instant,
    pub(super) metrics: &'a Arc<Metrics>,
    pub(super) stream: FrameStream<'a, R>,
}

/// Run the shared four-arm transaction-routing state machine for one wire
/// query frame, returning the response plus its un-waited WAL durability
/// ticket. The TxGate permit is managed here and, crucially, is already
/// released (bare statements, commit/rollback) by the time this returns, so
/// the caller's `finalize_durability` wait happens OUTSIDE the gate and
/// overlapping committers can share an fsync.
///
/// The three wire dialects (PowQL, SQL, parameterized PowQL) differ only in
/// how a frame is parsed and dispatched; they share this routing, so a
/// behavior fix (e.g. cancellation rollback parity) lands here exactly once.
/// [`ParsedFrame`] carries the dialect's already-parsed frame and the routing
/// facts derived from it; `dispatch` is the closure that executes it (its
/// `bool` argument is `allow_readonly_escalation`).
async fn run_wire_query_state_machine<Inner, D, R>(
    ctx: QueryContext<'_, R>,
    frame: ParsedFrame<Inner>,
    dispatch: D,
) -> (
    Message,
    Option<PendingDurability>,
    Option<ConnectionTermination>,
)
where
    Inner: Send + Sync + 'static,
    D: Fn(Arc<RwLock<Engine>>, Arc<Inner>, Option<Principal>, bool) -> DispatchOutcome
        + Clone
        + Send
        + Sync
        + 'static,
    R: AsyncRead + Unpin,
{
    let QueryContext {
        engine,
        tx_gate,
        tx_permit,
        principal,
        result_mode,
        query_timeout,
        tx_wait_timeout,
        metrics,
        mut stream,
    } = ctx;
    let ParsedFrame {
        query: parsed_query,
        tx_control,
        autocommit_admission,
        query_deadline,
    } = frame;

    // A read-only (snapshot-serving) engine never takes writer admission and
    // never runs a mutating statement. The flag is fixed for the engine's
    // lifetime; reading it costs one uncontended shared lock per frame.
    let read_only = engine.read().map(|eng| eng.is_read_only()).unwrap_or(false);
    if read_only {
        // Transaction control and any write statement require a writer that does
        // not exist in this mode: return the terminal error without touching the
        // gate or the engine write lock.
        let is_write = tx_control.is_some() || autocommit_admission == AdmissionMode::Writer;
        if is_write {
            return (readonly_terminal_message(), None, None);
        }
    }

    match tx_control {
        Some(TransactionControl::Begin) => {
            if tx_permit.is_some() {
                return (
                    error_response(
                        sanitize_error(
                            "cannot begin: a transaction is already active on this connection",
                        ),
                        ErrorClass::Execution,
                    ),
                    None,
                    None,
                );
            }
            let permit = match acquire_begin_permit(&tx_gate, tx_wait_timeout, metrics).await {
                Ok(permit) => permit,
                Err(response) => return (response, None, None),
            };
            let dispatch_begin = dispatch.clone();
            let (response, ticket, mut termination, _) = run_blocking_query(
                BlockingQuery {
                    engine: engine.clone(),
                    principal: principal.clone(),
                    result_mode,
                    query_timeout,
                    query_deadline,
                    metrics,
                    stream: stream.reborrow(),
                },
                Arc::clone(&parsed_query),
                move |engine, parsed_query, principal| {
                    dispatch_begin(engine, parsed_query, principal, true)
                },
            )
            .await;
            if is_success_response(&response) {
                *tx_permit = Some(permit);
            } else if is_query_cancellation_response(&response) {
                // Parity with the commit/rollback and in-transaction arms: a
                // cancelled begin must not leave the engine's transaction
                // state ownerless. Install the just-acquired permit so the
                // shared rollback helper can undo any transaction the begin
                // opened, then release the permit and close the connection.
                *tx_permit = Some(permit);
                rollback_connection_transaction(engine, principal, tx_permit).await;
                termination = Some(ConnectionTermination::Closed);
            }
            (response, ticket, termination)
        }
        Some(TransactionControl::Commit | TransactionControl::Rollback) => {
            let standalone_permit = if tx_permit.is_none() {
                match acquire_autocommit_permit(
                    &tx_gate,
                    AdmissionMode::Writer,
                    tx_wait_timeout,
                    metrics,
                )
                .await
                {
                    Ok(permit) => Some(permit),
                    Err(response) => return (response, None, None),
                }
            } else {
                None
            };
            let dispatch_commit = dispatch.clone();
            let (response, ticket, mut termination, _) = run_blocking_query(
                BlockingQuery {
                    engine: engine.clone(),
                    principal: principal.clone(),
                    result_mode,
                    query_timeout,
                    query_deadline,
                    metrics,
                    stream: stream.reborrow(),
                },
                Arc::clone(&parsed_query),
                move |engine, parsed_query, principal| {
                    dispatch_commit(engine, parsed_query, principal, true)
                },
            )
            .await;
            if is_success_response(&response) {
                // Release the gate BEFORE the caller waits on the commit's
                // ticket: the engine work is done and WAL order is fixed, so
                // another connection's commit can start (and share the fsync)
                // while this one waits.
                tx_permit.take();
            } else if is_query_cancellation_response(&response) {
                rollback_connection_transaction(engine, principal, tx_permit).await;
                termination = Some(ConnectionTermination::Closed);
            }
            drop(standalone_permit);
            (response, ticket, termination)
        }
        None if tx_permit.is_some() => {
            let dispatch_in_tx = dispatch.clone();
            let mut out = run_blocking_query(
                BlockingQuery {
                    engine: engine.clone(),
                    principal: principal.clone(),
                    result_mode,
                    query_timeout,
                    query_deadline,
                    metrics,
                    stream: stream.reborrow(),
                },
                Arc::clone(&parsed_query),
                move |engine, parsed_query, principal| {
                    dispatch_in_tx(engine, parsed_query, principal, true)
                },
            )
            .await;
            if is_query_cancellation_response(&out.0) {
                rollback_connection_transaction(engine, principal, tx_permit).await;
                out.2 = Some(ConnectionTermination::Closed);
            }
            (out.0, out.1, out.2)
        }
        None => {
            let admission = autocommit_admission;
            let permit = match acquire_autocommit_permit(
                &tx_gate,
                admission,
                tx_wait_timeout,
                metrics,
            )
            .await
            {
                Ok(permit) => permit,
                Err(response) => return (response, None, None),
            };
            // The parsed AST can be large. Share the first parse with the
            // rare dirty-view retry instead of cloning it on every successful
            // autocommit read.
            let retry_engine = Arc::clone(&engine);
            let retry_parsed_query = Arc::clone(&parsed_query);
            let retry_principal = principal.clone();
            let allow_readonly_escalation = admission == AdmissionMode::Writer;
            let dispatch_first = dispatch.clone();
            let mut out = run_blocking_query(
                BlockingQuery {
                    engine,
                    principal,
                    result_mode,
                    query_timeout,
                    query_deadline,
                    metrics,
                    stream: stream.reborrow(),
                },
                parsed_query,
                move |engine, parsed_query, principal| {
                    dispatch_first(engine, parsed_query, principal, allow_readonly_escalation)
                },
            )
            .await;
            drop(permit);
            if read_only && out.3 {
                // A read reached ReadonlyNeedsWrite (e.g. a stale materialized
                // view). There is no writer to escalate to in read-only mode:
                // surface the terminal error telling the operator to refresh
                // before snapshotting, rather than retrying under a writer.
                out.0 = readonly_terminal_message();
                out.3 = false;
            }
            if out.3 {
                let writer_permit = match acquire_autocommit_permit(
                    &tx_gate,
                    AdmissionMode::Writer,
                    tx_wait_timeout,
                    metrics,
                )
                .await
                {
                    Ok(permit) => permit,
                    Err(response) => return (response, None, None),
                };
                let dispatch_retry = dispatch.clone();
                out = run_blocking_query(
                    BlockingQuery {
                        engine: retry_engine,
                        principal: retry_principal,
                        result_mode,
                        query_timeout,
                        query_deadline,
                        metrics,
                        stream: stream.reborrow(),
                    },
                    retry_parsed_query,
                    move |engine, parsed_query, principal| {
                        dispatch_retry(engine, parsed_query, principal, true)
                    },
                )
                .await;
                drop(writer_permit);
            }
            (out.0, out.1, out.2)
        }
    }
}

/// Execute one PowQL wire query frame. Thin dialect wrapper over
/// [`run_wire_query_state_machine`].
pub(super) async fn execute_wire_query<R>(
    ctx: QueryContext<'_, R>,
    query: String,
) -> (
    Message,
    Option<PendingDurability>,
    Option<ConnectionTermination>,
)
where
    R: AsyncRead + Unpin,
{
    let start = Instant::now();
    let query_deadline = start + ctx.query_timeout;
    // Parse each frame once for transaction routing, admission, and role
    // enforcement. The engine still canonicalizes/parses as needed for plan
    // cache execution, but the server no longer repeats the same parse in
    // three separate routing helpers before reaching it.
    let stmt_result = parser::parse(&query).map_err(|e| e.to_string());
    match &stmt_result {
        Err(message) => return parse_failure_response(message, ctx.metrics, start),
        Ok(stmt) => {
            if let Err(denied) = check_statement_permitted(ctx.principal.as_ref(), stmt) {
                return permission_denied_response(&denied, ctx.metrics, start);
            }
        }
    }
    let parsed_query = Arc::new((query, stmt_result));
    let tx_control = parsed_transaction_control(&parsed_query.1);
    let autocommit_admission = parsed_query
        .1
        .as_ref()
        .map(statement_admission)
        .unwrap_or(AdmissionMode::Writer);
    run_wire_query_state_machine(
        ctx,
        ParsedFrame {
            query: parsed_query,
            tx_control,
            autocommit_admission,
            query_deadline,
        },
        |engine,
         parsed_query: Arc<(String, Result<powdb_query::ast::Statement, String>)>,
         principal: Option<Principal>,
         allow| {
            dispatch_query_parsed(
                &engine,
                &parsed_query.0,
                &parsed_query.1,
                principal.as_ref(),
                allow,
            )
        },
    )
    .await
}

pub(super) async fn execute_wire_query_sql<R>(
    ctx: QueryContext<'_, R>,
    query: String,
) -> (
    Message,
    Option<PendingDurability>,
    Option<ConnectionTermination>,
)
where
    R: AsyncRead + Unpin,
{
    let start = Instant::now();
    let query_deadline = start + ctx.query_timeout;
    let stmt_result = sql::parse_sql(&query).map_err(|e| e.to_string());
    match &stmt_result {
        Err(message) => return parse_failure_response(message, ctx.metrics, start),
        Ok(stmt) => {
            if let Err(denied) = check_statement_permitted(ctx.principal.as_ref(), stmt) {
                return permission_denied_response(&denied, ctx.metrics, start);
            }
        }
    }
    let parsed_query = Arc::new((query, stmt_result));
    let tx_control = parsed_transaction_control(&parsed_query.1);
    let autocommit_admission = parsed_query
        .1
        .as_ref()
        .map(statement_admission)
        .unwrap_or(AdmissionMode::Writer);
    run_wire_query_state_machine(
        ctx,
        ParsedFrame {
            query: parsed_query,
            tx_control,
            autocommit_admission,
            query_deadline,
        },
        |engine,
         parsed_query: Arc<(String, Result<powdb_query::ast::Statement, String>)>,
         principal: Option<Principal>,
         allow| {
            dispatch_sql_query_parsed(
                &engine,
                &parsed_query.0,
                &parsed_query.1,
                principal.as_ref(),
                allow,
            )
        },
    )
    .await
}

pub(super) async fn execute_wire_query_with_params<R>(
    ctx: QueryContext<'_, R>,
    query: String,
    params: Vec<WireParam>,
) -> (
    Message,
    Option<PendingDurability>,
    Option<ConnectionTermination>,
)
where
    R: AsyncRead + Unpin,
{
    let start = Instant::now();
    let query_deadline = start + ctx.query_timeout;
    let bound: Vec<powdb_query::ast::ParamValue> = params.iter().map(wire_param_to_value).collect();
    let stmt_result = parser::parse_with_params(&query, &bound).map_err(|e| e.to_string());
    match &stmt_result {
        Err(message) => return parse_failure_response(message, ctx.metrics, start),
        Ok(stmt) => {
            if let Err(denied) = check_statement_permitted(ctx.principal.as_ref(), stmt) {
                return permission_denied_response(&denied, ctx.metrics, start);
            }
        }
    }
    let parsed_query = Arc::new((query, bound, stmt_result));
    let tx_control = parsed_transaction_control(&parsed_query.2);
    let autocommit_admission = parsed_query
        .2
        .as_ref()
        .map(statement_admission)
        .unwrap_or(AdmissionMode::Writer);
    run_wire_query_state_machine(
        ctx,
        ParsedFrame {
            query: parsed_query,
            tx_control,
            autocommit_admission,
            query_deadline,
        },
        |engine,
         parsed_query: Arc<(
            String,
            Vec<powdb_query::ast::ParamValue>,
            Result<powdb_query::ast::Statement, String>,
        )>,
         principal: Option<Principal>,
         allow| {
            dispatch_query_with_bound_params_parsed(
                &engine,
                &parsed_query.0,
                &parsed_query.1,
                &parsed_query.2,
                principal.as_ref(),
                allow,
            )
        },
    )
    .await
}

/// A statement's metric sample whose recording is deferred until its WAL
/// durability obligation settles: a Full-mode fsync failure downgrades the
/// client's success reply to an error, and the metrics must tell the same
/// story (and the latency must include the wait the client observed).
pub(super) struct DeferredQueryMetric {
    pub(super) start: Instant,
    pub(super) outcome: QueryOutcome,
    pub(super) exceeded_timeout: bool,
}

/// Durability ticket + the deferred metric of the statement that produced it.
type PendingDurability = (WalDurabilityTicket, DeferredQueryMetric);

pub(super) async fn run_blocking_query<T, F, R>(
    ctx: BlockingQuery<'_, R>,
    input: T,
    f: F,
) -> (
    Message,
    Option<PendingDurability>,
    Option<ConnectionTermination>,
    bool,
)
where
    T: Send + 'static,
    F: FnOnce(Arc<RwLock<Engine>>, T, Option<Principal>) -> DispatchOutcome + Send + 'static,
    R: AsyncRead + Unpin,
{
    let BlockingQuery {
        engine,
        principal,
        result_mode,
        query_timeout,
        query_deadline,
        metrics,
        stream,
    } = ctx;
    let _in_flight = metrics.in_flight_guard();
    let start = Instant::now();

    // Cooperative cancellation. The blocking closure installs this token for the
    // executor thread; every unbounded executor loop polls it. We give it a
    // deadline of `now + query_timeout` so the query self-terminates even if the
    // async timeout arm below is slow to be scheduled — that is what actually
    // makes the timeout enforceable (a `spawn_blocking` thread cannot be
    // aborted, so before this the timeout arm just awaited the runaway query to
    // completion while it held the engine lock / tx-gate permit).
    let timeout_ms = query_timeout.as_millis().min(u128::from(u64::MAX)) as u64;
    let cancel = Arc::new(powdb_query::cancel::ExecCancel::with_deadline(
        query_deadline,
        timeout_ms,
    ));
    let cancel_task = Arc::clone(&cancel);
    let mut handle = tokio::task::spawn_blocking(move || {
        let _cancel_guard = powdb_query::cancel::install(cancel_task);
        f(engine, input, principal)
    });
    let mut exceeded_timeout = false;
    let mut termination = None;
    let timeout = tokio::time::sleep(query_deadline.saturating_duration_since(Instant::now()));
    tokio::pin!(timeout);
    let join_result = loop {
        tokio::select! {
            result = &mut handle => break result,
            _ = &mut timeout => {
                exceeded_timeout = true;
                // Signal the executor to stop at its next cancellation checkpoint,
                // then await the (now promptly returning) handle. The closure
                // returns a typed timeout error and releases the engine lock /
                // tx-gate permit as it unwinds.
                cancel.cancel(powdb_query::cancel::CancelReason::Timeout);
                break handle.await;
            }
            read = read_message_cancel_safe(
                stream.reader,
                stream.buffered,
                stream.pending.remaining_bytes(),
            ) => {
                match read {
                    Ok(Some(DecodedWireMessage { message: Message::Disconnect, .. })) => {
                        cancel.cancel(powdb_query::cancel::CancelReason::Disconnect);
                        termination = Some(ConnectionTermination::Closed);
                        break handle.await;
                    }
                    Ok(Some(frame)) => {
                        if stream.pending.len() + 1 >= MAX_IN_FLIGHT_READ_AHEAD_FRAMES
                            || stream.pending.wire_bytes + frame.wire_len
                                >= MAX_IN_FLIGHT_READ_AHEAD_BYTES
                        {
                            // Never stop observing the socket behind a full
                            // queue. Reaching either hard cap cancels the query
                            // and closes this connection immediately.
                            cancel.cancel(powdb_query::cancel::CancelReason::Disconnect);
                            termination = Some(ConnectionTermination::ReadError);
                            break handle.await;
                        }
                        // Preserve frames that arrive while the blocking query
                        // runs. The normal batching/main-loop path consumes them
                        // in order once execution completes.
                        stream.pending.push_back(frame);
                    }
                    Ok(None) => {
                        cancel.cancel(powdb_query::cancel::CancelReason::Disconnect);
                        termination = Some(ConnectionTermination::Closed);
                        break handle.await;
                    }
                    Err(_) => {
                        cancel.cancel(powdb_query::cancel::CancelReason::Disconnect);
                        termination = Some(ConnectionTermination::ReadError);
                        break handle.await;
                    }
                }
            }
        }
    };

    let (message, ticket, outcome, readonly_needs_write) = match join_result {
        Ok((Ok(result), ticket)) => match query_result_to_message(result, result_mode) {
            Ok(message) => (message, ticket, QueryOutcome::Ok, false),
            Err(e) => (
                error_response(sanitize_error(&e.to_string()), classify_query_error(&e)),
                ticket,
                QueryOutcome::Error,
                false,
            ),
        },
        Ok((Err(QueryError::ReadonlyNeedsWrite), ticket)) => {
            if exceeded_timeout {
                (
                    error_response(
                        sanitize_error(&QueryError::Timeout { timeout_ms }.to_string()),
                        ErrorClass::Timeout,
                    ),
                    ticket,
                    QueryOutcome::Timeout,
                    true,
                )
            } else {
                (
                    error_response("query execution error", ErrorClass::Internal),
                    ticket,
                    QueryOutcome::Error,
                    true,
                )
            }
        }
        Ok((Err(e), ticket)) => {
            // A deadline-driven cancellation returns Timeout even when the async
            // timeout arm has not fired yet (the executor self-cancels): treat it
            // as a timeout for metrics either way.
            if matches!(e, QueryError::Timeout { .. }) {
                exceeded_timeout = true;
            }
            let outcome = if matches!(e, QueryError::MemoryLimitExceeded { .. }) {
                QueryOutcome::MemoryLimit
            } else if matches!(e, QueryError::Timeout { .. }) {
                QueryOutcome::Timeout
            } else {
                QueryOutcome::Error
            };
            (
                error_response(sanitize_error(&e.to_string()), classify_query_error(&e)),
                ticket,
                outcome,
                false,
            )
        }
        Err(e) => (
            error_response(format!("internal error: {e}"), ErrorClass::Internal),
            None,
            QueryOutcome::Error,
            false,
        ),
    };
    let readonly_retry = readonly_needs_write && !exceeded_timeout && termination.is_none();
    match ticket {
        // The statement's durability (and thus its true outcome and the
        // latency the client observes) settles at batch end — defer the
        // metric to the settlement site instead of recording a success that
        // a failed fsync would falsify.
        Some(ticket) => (
            message,
            Some((
                ticket,
                DeferredQueryMetric {
                    start,
                    outcome,
                    exceeded_timeout,
                },
            )),
            termination,
            readonly_retry,
        ),
        None => {
            if !readonly_retry {
                if exceeded_timeout {
                    metrics.record_query(start.elapsed(), QueryOutcome::Timeout);
                } else {
                    metrics.record_query(start.elapsed(), outcome);
                }
            }
            (message, None, termination, readonly_retry)
        }
    }
}

/// Settle a WAL durability ticket off the async path, AFTER the TxGate
/// permit has been dropped — that ordering is what lets committers on other
/// connections append (and share the fsync) while this one waits.
///
/// Returns `None` when the covering fsync succeeded, or `Some(client-facing
/// error message)` when it failed — in which case no statement the ticket
/// covers may be acknowledged as durable (it executed in memory only).
pub(super) async fn settle_durability_ticket(ticket: WalDurabilityTicket) -> Option<String> {
    match tokio::task::spawn_blocking(move || ticket.wait()).await {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(sanitize_error(&format!("WAL durability sync failed: {e}"))),
        Err(e) => Some(format!("internal error: {e}")),
    }
}
