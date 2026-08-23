//! Transaction admission and lifetime: the [`TxGate`] permit pool, the
//! bounded acquires each frontend takes, and the reaper that rolls back a
//! transaction which has held the gate for its whole permitted lifetime.

use crate::metrics::Metrics;
use crate::protocol::{ErrorClass, Message};
use powdb_query::executor::{is_read_only_statement, Engine};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncWrite, BufWriter};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::warn;

use super::auth::Principal;
use super::classify::error_response;
use super::query::dispatch_query;
use super::wire::write_msg_with_budget;

/// Fixed reader-permit pool. Read-only autocommit statements take one permit;
/// writers, sync operations, and explicit transactions take the entire pool.
/// Tokio's fair semaphore queue prevents a waiting writer from being starved by
/// later readers.
///
/// The gate also carries the maximum time ONE connection may hold it inside an
/// explicit transaction. That bound lives here, not in [`ConnOpts`], because it
/// is a property of the gate being held rather than of a connection: every
/// listener (TCP, TLS, Unix socket) already clones the same gate into every
/// connection, so the bound reaches all of them without a per-listener wiring
/// step that a future frontend could forget.
#[derive(Clone)]
pub struct TxGate {
    pub(super) semaphore: Arc<Semaphore>,
    pub(super) permit_count: u32,
    pub(super) max_tx_lifetime: Option<Duration>,
}

pub const DEFAULT_TX_GATE_READER_PERMITS: u32 = 1024;

/// Default ceiling on how long one connection may hold the gate inside an
/// explicit transaction before the server rolls it back. Matches the default
/// connection idle timeout, which is what used to bound a *silent* holder; the
/// lifetime bound is what bounds a NOISY one, because the idle deadline
/// re-arms on every frame and a bare PING is a frame.
pub const DEFAULT_TX_MAX_LIFETIME: Duration = Duration::from_secs(300);

/// Create a transaction gate for a shared engine.
pub fn new_tx_gate() -> TxGate {
    new_tx_gate_with_permits(DEFAULT_TX_GATE_READER_PERMITS)
}

/// Create a transaction gate with an explicit reader capacity.
///
/// The configurable constructor exists so benchmark and compatibility tests can
/// reproduce the former single-permit admission policy using the exact same
/// handler code. Production uses [`new_tx_gate`].
pub fn new_tx_gate_with_permits(permit_count: u32) -> TxGate {
    new_tx_gate_with_permits_and_max_tx_lifetime(permit_count, Some(DEFAULT_TX_MAX_LIFETIME))
}

/// Create a transaction gate with an explicit maximum transaction lifetime.
/// `None` disables the bound (the `POWDB_TX_MAX_LIFETIME_MS=0` opt-out), which
/// restores the pre-0.22 behavior where a client controlled the hold duration.
pub fn new_tx_gate_with_max_tx_lifetime(max_tx_lifetime: Option<Duration>) -> TxGate {
    new_tx_gate_with_permits_and_max_tx_lifetime(DEFAULT_TX_GATE_READER_PERMITS, max_tx_lifetime)
}

fn new_tx_gate_with_permits_and_max_tx_lifetime(
    permit_count: u32,
    max_tx_lifetime: Option<Duration>,
) -> TxGate {
    assert!(permit_count > 0, "transaction gate requires a permit");
    TxGate {
        semaphore: Arc::new(Semaphore::new(permit_count as usize)),
        permit_count,
        max_tx_lifetime,
    }
}

impl TxGate {
    pub fn permit_count(&self) -> u32 {
        self.permit_count
    }

    /// How long one connection may hold this gate inside an explicit
    /// transaction before the server rolls the transaction back and releases
    /// it. `None` means unbounded.
    pub fn max_tx_lifetime(&self) -> Option<Duration> {
        self.max_tx_lifetime
    }

    pub(super) fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    pub(super) async fn acquire_many_owned(
        self,
        permits: u32,
    ) -> Result<OwnedSemaphorePermit, tokio::sync::AcquireError> {
        self.semaphore.acquire_many_owned(permits).await
    }

    pub(super) fn try_acquire_many_owned(
        self,
        permits: u32,
    ) -> Result<OwnedSemaphorePermit, tokio::sync::TryAcquireError> {
        self.semaphore.try_acquire_many_owned(permits)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TransactionControl {
    Begin,
    Commit,
    Rollback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AdmissionMode {
    Reader,
    Writer,
}

pub(super) fn statement_admission(stmt: &powdb_query::ast::Statement) -> AdmissionMode {
    if is_read_only_statement(stmt) {
        AdmissionMode::Reader
    } else {
        AdmissionMode::Writer
    }
}

fn transaction_control(stmt: &powdb_query::ast::Statement) -> Option<TransactionControl> {
    use powdb_query::ast::Statement;
    match stmt {
        Statement::Begin => Some(TransactionControl::Begin),
        Statement::Commit => Some(TransactionControl::Commit),
        Statement::Rollback => Some(TransactionControl::Rollback),
        _ => None,
    }
}

pub(super) fn parsed_transaction_control(
    stmt_result: &Result<powdb_query::ast::Statement, String>,
) -> Option<TransactionControl> {
    stmt_result.as_ref().ok().and_then(transaction_control)
}

/// Acquire the TxGate for an explicit `begin`, bounded by `tx_wait_timeout`.
/// Overlapping explicit transactions queue behind the permit rather than being
/// rejected, but a connection gives up with a clear, client-facing error once
/// the wait elapses — so a transaction stalled (or held open) on another
/// connection can never block this one indefinitely. A timeout is recorded so
/// `powdb_tx_gate_timeouts_total` (and the error total) stay truthful.
pub(super) async fn acquire_begin_permit(
    tx_gate: &TxGate,
    tx_wait_timeout: Duration,
    metrics: &Arc<Metrics>,
) -> Result<OwnedSemaphorePermit, Message> {
    match tokio::time::timeout(
        tx_wait_timeout,
        tx_gate.clone().acquire_many_owned(tx_gate.permit_count()),
    )
    .await
    {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err(error_response(
            "query execution error",
            ErrorClass::Internal,
        )),
        Err(_) => {
            metrics.inc_tx_gate_timeout();
            Err(error_response(
                format!(
                    "transaction gate timeout after {}ms waiting for concurrent transaction to complete",
                    tx_wait_timeout.as_millis()
                ),
                ErrorClass::Timeout,
            ))
        }
    }
}

/// Acquire the TxGate for a BARE autocommit statement, bounded by
/// `tx_wait_timeout` exactly like [`acquire_begin_permit`]. Autocommit writes
/// serialize through the same gate as explicit transactions, so a stalled (or
/// held-open) transaction on another connection would otherwise block this
/// write indefinitely. Bounding the acquire turns that indefinite wait into a
/// clear, client-facing timeout error and records the timeout so
/// `powdb_tx_gate_timeouts_total` (and the error total) stay truthful. This
/// only bounds the ACQUIRE; the permit is still dropped BEFORE the caller's
/// durability wait so overlapping committers can share an fsync.
pub(super) async fn acquire_autocommit_permit(
    tx_gate: &TxGate,
    admission: AdmissionMode,
    tx_wait_timeout: Duration,
    metrics: &Arc<Metrics>,
) -> Result<OwnedSemaphorePermit, Message> {
    let permits = match admission {
        AdmissionMode::Reader => 1,
        AdmissionMode::Writer => tx_gate.permit_count(),
    };

    // The uncontended path is overwhelmingly common for autocommit work.
    // Avoid constructing and polling a timeout-wrapped semaphore future when
    // the permits are already available. If a reader or writer is queued, the
    // try-acquire fails and we fall back to Tokio's fair semaphore queue, so a
    // waiting writer still cannot be bypassed by later readers.
    if let Ok(permit) = tx_gate.clone().try_acquire_many_owned(permits) {
        return Ok(permit);
    }

    let acquire = tx_gate.clone().acquire_many_owned(permits);
    match tokio::time::timeout(tx_wait_timeout, acquire).await {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err(error_response(
            "query execution error",
            ErrorClass::Internal,
        )),
        Err(_) => {
            metrics.inc_tx_gate_timeout();
            Err(error_response(
                format!(
                    "transaction gate timeout after {}ms waiting for concurrent transaction to complete",
                    tx_wait_timeout.as_millis()
                ),
                ErrorClass::Timeout,
            ))
        }
    }
}

pub(super) fn rollback_open_transaction(engine: Arc<RwLock<Engine>>, principal: Option<Principal>) {
    let (res, ticket) = dispatch_query(&engine, "rollback", principal.as_ref(), true);
    let _ = res;
    // Rollback takes the sync-preserving path (no ticket), but settle one
    // defensively if it ever appears so the durability watermark stays honest.
    if let Some(ticket) = ticket {
        let _ = ticket.wait();
    }
}

/// Keep the transaction-lifetime deadline in step with the gate permit itself.
///
/// The deadline is DERIVED from `tx_permit` rather than armed at each install
/// site on purpose. The permit is installed and released in seven places
/// (begin, commit/rollback, three cancellation rollbacks, the standalone
/// commit permit, disconnect teardown), and a bound that has to be re-armed at
/// each of them is exactly the partial application this bound exists to
/// prevent: miss one and that path silently becomes unbounded again. Reading
/// the permit's own presence cannot miss a site.
///
/// Called once per handled frame, so `begin; commit; begin` inside one
/// pipelined batch re-arms rather than carrying the first transaction's
/// deadline into the second.
pub(super) fn sync_tx_deadline(
    tx_permit: &Option<OwnedSemaphorePermit>,
    tx_deadline: &mut Option<Instant>,
    max_tx_lifetime: Option<Duration>,
) {
    match (tx_permit.is_some(), tx_deadline.is_some()) {
        (true, false) => *tx_deadline = max_tx_lifetime.map(|max| Instant::now() + max),
        (false, true) => *tx_deadline = None,
        _ => {}
    }
}

/// The connection state a transaction reap acts on: the engine and identity it
/// rolls the transaction back through, the gate permit and lifetime deadline it
/// clears, and the socket, peer label, and counters it reports through.
///
/// A reap is one event that touches all seven, which is why they are one type.
/// Both reapers take it by value and the second hands its own straight to the
/// first, so there is no way to reap through a half-assembled set of them.
pub(super) struct ReapContext<'a, W> {
    pub(super) engine: &'a Arc<RwLock<Engine>>,
    pub(super) principal: &'a Option<Principal>,
    pub(super) tx_permit: &'a mut Option<OwnedSemaphorePermit>,
    pub(super) tx_deadline: &'a mut Option<Instant>,
    pub(super) writer: &'a mut BufWriter<W>,
    pub(super) peer: &'a str,
    pub(super) metrics: &'a Arc<Metrics>,
}

/// Roll back a transaction that has held the gate for its whole permitted
/// lifetime, tell the client why, and release the gate.
///
/// Nothing else bounds this. The connection idle timeout is re-armed by every
/// frame the client sends and a bare `PING` is a frame, so a client that pings
/// once per idle period holds the entire writer gate forever while every other
/// connection, readers included, times out against it. Serializing readers
/// behind an explicit transaction is deliberate and documented; letting the
/// client choose how long that lasts is not.
///
/// On a connection whose last write completed, the reply is a typed
/// [`ErrorClass::Timeout`] naming the budget and the knob, never a silent
/// disconnect: the client's transaction is gone and it has to know that before
/// it sends the next statement. When the reap was triggered BY a write that
/// could not finish, there is nowhere to put that frame; see [`ReapNotice`].
pub(super) async fn reap_expired_transaction<W>(
    ctx: ReapContext<'_, W>,
    max_tx_lifetime: Duration,
    notice: ReapNotice,
) where
    W: AsyncWrite + Unpin,
{
    let ReapContext {
        engine,
        principal,
        tx_permit,
        tx_deadline,
        writer,
        peer,
        metrics,
    } = ctx;
    warn!(
        peer = %peer,
        max_tx_lifetime_ms = max_tx_lifetime.as_millis(),
        notified = matches!(notice, ReapNotice::Speak(_)),
        "transaction exceeded its maximum lifetime; rolling back and releasing the transaction gate"
    );
    rollback_connection_transaction(engine.clone(), principal.clone(), tx_permit).await;
    *tx_deadline = None;
    metrics.inc_tx_reaped();
    let ReapNotice::Speak(budget) = notice else {
        return;
    };
    let err = error_response(
        format!(
            "transaction exceeded the maximum lifetime of {}ms and was rolled back; \
             raise POWDB_TX_MAX_LIFETIME_MS if transactions on this server legitimately run longer",
            max_tx_lifetime.as_millis()
        ),
        ErrorClass::Timeout,
    );
    write_msg_with_budget(writer, &err, budget).await;
}

/// Whether a reap may still tell the client what happened.
///
/// A frame may only be written on a frame boundary, and a reply write that
/// failed did not necessarily fail before touching the socket:
/// [`Message::write_to`] is one `write_all` of the encoded frame, and any
/// frame larger than the `BufWriter`'s buffer goes straight through to the
/// socket, so cancelling that write on the budget leaves a partial frame on
/// the wire with its length already announced. There is no resume and no
/// rollback for those bytes. Anything written next is read by the client as
/// the dead frame's payload: the notification does not arrive, and the bytes
/// that carry it corrupt the framing of a stream the client was about to see
/// closed anyway.
///
/// So the reap speaks only where speaking is possible. Everything else it does
/// (roll back, release the gate, log, count) happens either way, which is what
/// keeps the event operator-visible when the wire cannot be.
#[derive(Clone, Copy, Debug)]
pub(super) enum ReapNotice {
    /// The last write on this connection completed, so the next byte written
    /// starts a frame. The budget bounds the attempt: the gate is already
    /// released, so a client that has stopped reading can no longer cost
    /// anyone else anything.
    Speak(Duration),
    /// A reply write failed partway. The stream is no longer framable; close
    /// it without writing anything else.
    Silence,
}

/// A reply write failed. If this connection was inside an explicit transaction
/// whose lifetime has run out, that is a REAP, not an ordinary write failure,
/// and it must be treated as one: rolled back, released, logged, and counted.
///
/// Without this the connection simply broke out of the loop and let the
/// disconnect teardown roll the transaction back with no log line and no
/// counter, so the one budget that was actually being enforced left no trace
/// anywhere an operator could see it. A write that failed for any other reason
/// falls through to the same teardown as before.
///
/// This reap is always [`ReapNotice::Silence`]: it exists BECAUSE a write
/// failed, and a write after a failed write cannot be framed. The log line and
/// `powdb_tx_reaped_total` are what make it visible instead.
pub(super) async fn reap_after_stalled_write<W>(
    ctx: ReapContext<'_, W>,
    max_tx_lifetime: Option<Duration>,
) where
    W: AsyncWrite + Unpin,
{
    let (Some(deadline), Some(max)) = (*ctx.tx_deadline, max_tx_lifetime) else {
        return;
    };
    if Instant::now() < deadline {
        return;
    }
    reap_expired_transaction(ctx, max, ReapNotice::Silence).await;
}

/// Roll back this connection's explicit transaction while it still owns the
/// transaction-gate permit, then release the permit. A timed-out/cancelled
/// statement cannot leave an ambiguous transaction open and block every later
/// writer; releasing first would let another connection enter the engine before
/// this rollback has restored the prior snapshot.
pub(super) async fn rollback_connection_transaction(
    engine: Arc<RwLock<Engine>>,
    principal: Option<Principal>,
    tx_permit: &mut Option<OwnedSemaphorePermit>,
) {
    if tx_permit.is_none() {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || rollback_open_transaction(engine, principal)).await;
    tx_permit.take();
}
