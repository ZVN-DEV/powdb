//! Transaction admission and lifetime: the [`TxGate`] permit pool, the
//! bounded acquires each frontend takes, and the reaper that rolls back a
//! transaction which has held the gate for its whole permitted lifetime.

use crate::metrics::Metrics;
use crate::protocol::{ErrorClass, Message};
use powdb_query::executor::{is_read_only_statement, Engine};
use std::sync::atomic::{AtomicBool, Ordering};
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
/// An explicit transaction that has not written yet is the one holder that
/// lets reads through anyway, out of a second pool. See
/// `TxGateHold` and `upgrade_to_exclusive`.
///
/// The gate also carries the maximum time ONE connection may hold it inside an
/// explicit transaction. That bound lives here, not in
/// [`ConnOpts`](crate::handler::ConnOpts), because it is a property of the
/// gate being held rather than of a connection: every listener (TCP, TLS,
/// Unix socket) already clones the same gate into every connection, so the
/// bound reaches all of them without a per-listener wiring step that a future
/// frontend could forget.
#[derive(Clone)]
pub struct TxGate {
    pub(super) semaphore: Arc<Semaphore>,
    /// Admission for readers running BESIDE an explicit transaction that has
    /// not written yet. It is a second pool rather than a slice of the first
    /// because the transaction's first write has to drain it, and an acquire
    /// on the main pool would queue behind the connections that are waiting
    /// for this very transaction to finish. Only readers ever take from this
    /// pool, so draining it waits for reads and nothing else.
    pub(super) open_tx_readers: Arc<Semaphore>,
    /// Set while an explicit transaction holds the whole gate and has not
    /// written. It is the only thing that admits a reader to
    /// `open_tx_readers`, and it is cleared by the transaction's first write
    /// and by the end of the transaction, so a later holder of the gate (an
    /// autocommit write, a sync operation) can never inherit an open door.
    pub(super) reader_bypass: Arc<AtomicBool>,
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
        open_tx_readers: Arc::new(Semaphore::new(permit_count as usize)),
        reader_bypass: Arc::new(AtomicBool::new(false)),
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

    /// Reader permits still free in the pool that serves reads running beside
    /// an explicit transaction that has not written.
    #[cfg(test)]
    pub(super) fn available_open_tx_reader_permits(&self) -> usize {
        self.open_tx_readers.available_permits()
    }

    /// Whether a read arriving now would be admitted beside an open explicit
    /// transaction instead of waiting for it.
    #[cfg(test)]
    pub(super) fn admits_readers_beside_transaction(&self) -> bool {
        self.reader_bypass.load(Ordering::Acquire)
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

/// What one connection holds while it owns the transaction gate inside an
/// explicit transaction.
///
/// A `begin` takes the whole permit pool, so no second transaction, no
/// autocommit write and no sync operation can run beside it. Reads are
/// admitted anyway, out of [`TxGate::open_tx_readers`], until the
/// transaction's first write drains that pool and keeps it.
///
/// The two halves are one type because they have to be released together and
/// in order: dropping the pool permit while reader admission is still open
/// would let the next holder of the gate inherit a door this transaction
/// opened.
pub(super) struct TxGateHold {
    gate: TxGate,
    /// Taken at the first write. Its presence IS "this transaction is
    /// exclusive", so there is no second flag to keep in step with it.
    open_tx_readers: Option<OwnedSemaphorePermit>,
    _pool: OwnedSemaphorePermit,
}

impl TxGateHold {
    /// Whether the transaction has taken the reader pool, which it does at its
    /// first write.
    pub(super) fn is_exclusive(&self) -> bool {
        self.open_tx_readers.is_some()
    }
}

impl std::fmt::Debug for TxGateHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TxGateHold")
            .field("exclusive", &self.is_exclusive())
            .finish()
    }
}

impl Drop for TxGateHold {
    fn drop(&mut self) {
        // Runs before the fields, so reader admission closes while this
        // connection still holds the pool.
        self.gate.reader_bypass.store(false, Ordering::Release);
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
///
/// The whole pool, so no second transaction, no autocommit write and no sync
/// operation runs beside it. Reads are admitted anyway until this
/// transaction's first write; see `upgrade_to_exclusive`.
///
/// Overlapping explicit transactions queue behind the permit rather than being
/// rejected, but a connection gives up with a clear, client-facing error once
/// the wait elapses — so a transaction stalled (or held open) on another
/// connection can never block this one indefinitely. A timeout is recorded so
/// `powdb_tx_gate_timeouts_total` (and the error total) stay truthful.
// `Message` is a large enum, so clippy::result_large_err fires on this
// signature under Rust 1.98 and newer. Boxing the Err would change an error
// path that the handler split deliberately left byte-identical, and this
// shape predates that split (it sat at crates/server/src/handler.rs on main).
// Left as-is for now; boxing the wire Message error type is its own change.
#[allow(clippy::result_large_err)]
pub(super) async fn acquire_begin_permit(
    tx_gate: &TxGate,
    tx_wait_timeout: Duration,
    metrics: &Arc<Metrics>,
) -> Result<TxGateHold, Message> {
    match tokio::time::timeout(
        tx_wait_timeout,
        tx_gate.clone().acquire_many_owned(tx_gate.permit_count()),
    )
    .await
    {
        Ok(Ok(permit)) => {
            // Open reader admission only once the pool is actually held, so
            // the flag is never set while another holder owns the gate.
            tx_gate.reader_bypass.store(true, Ordering::Release);
            Ok(TxGateHold {
                gate: tx_gate.clone(),
                open_tx_readers: None,
                _pool: permit,
            })
        }
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

/// Take the rest of the gate for an explicit transaction that is about to
/// write, bounded by `tx_wait_timeout`.
///
/// An explicit transaction becomes exclusive at its FIRST write, not at
/// `begin`. From that write on, its rows are in the heap and uncommitted, and
/// there is no MVCC to hide them, so every reader has to wait exactly as it
/// did before. Until then reads are admitted out of
/// [`TxGate::open_tx_readers`] and run beside it.
///
/// Exclusion is enforced by holding that pool rather than by a flag alone:
/// admission is closed first and the reads already running are waited out, so
/// there is no window between "decided to write" and "readers excluded".
///
/// A no-op once the transaction is exclusive, so the second and later writes
/// of a transaction cost nothing.
// `Message` is a large enum; see the note on `acquire_begin_permit`.
#[allow(clippy::result_large_err)]
pub(super) async fn upgrade_to_exclusive(
    tx_gate: &TxGate,
    tx_permit: &mut Option<TxGateHold>,
    tx_wait_timeout: Duration,
    metrics: &Arc<Metrics>,
) -> Result<(), Message> {
    let Some(hold) = tx_permit.as_mut() else {
        return Ok(());
    };
    if hold.is_exclusive() {
        return Ok(());
    }

    // Close admission first, then wait out the reads already running. The wait
    // is on the reader pool, not the main one, and only readers ever take from
    // it: a second `begin`, an autocommit write and a sync operation all wait
    // for the main pool, which this transaction holds. So nothing this
    // transaction is blocking can end up queued in front of this acquire,
    // which is what would otherwise make the first write wait on connections
    // that are waiting on it.
    tx_gate.reader_bypass.store(false, Ordering::Release);
    let acquire = tx_gate
        .open_tx_readers
        .clone()
        .acquire_many_owned(tx_gate.permit_count());
    match tokio::time::timeout(tx_wait_timeout, acquire).await {
        Ok(Ok(readers)) => {
            hold.open_tx_readers = Some(readers);
            Ok(())
        }
        Ok(Err(_)) => Err(error_response(
            "query execution error",
            ErrorClass::Internal,
        )),
        Err(_) => {
            // The write did not happen, so this transaction still has nothing
            // uncommitted to hide and reads may keep running beside it.
            tx_gate.reader_bypass.store(true, Ordering::Release);
            metrics.inc_tx_gate_timeout();
            Err(error_response(
                format!(
                    "transaction gate timeout after {}ms waiting for concurrent readers to finish before this transaction's first write",
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
// `Message` is a large enum, so clippy::result_large_err fires on this
// signature under Rust 1.98 and newer. Boxing the Err would change an error
// path that the handler split deliberately left byte-identical, and this
// shape predates that split (it sat at crates/server/src/handler.rs on main).
// Left as-is for now; boxing the wire Message error type is its own change.
#[allow(clippy::result_large_err)]
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

    // A read arriving while an explicit transaction is open but has written
    // nothing is admitted beside it. The permit comes from the reader pool,
    // which the transaction drains at its first write, so the read is excluded
    // from that write onwards exactly as it would have been by the main pool.
    // The flag is re-read after the permit is in hand: a first write racing
    // this admission has already cleared it, and this read then falls through
    // and waits for the transaction like any other.
    if admission == AdmissionMode::Reader && tx_gate.reader_bypass.load(Ordering::Relaxed) {
        if let Ok(permit) = tx_gate.open_tx_readers.clone().try_acquire_owned() {
            if tx_gate.reader_bypass.load(Ordering::Acquire) {
                return Ok(permit);
            }
        }
    }

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
    tx_permit: &Option<TxGateHold>,
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
    pub(super) tx_permit: &'a mut Option<TxGateHold>,
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
    tx_permit: &mut Option<TxGateHold>,
) {
    if tx_permit.is_none() {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || rollback_open_transaction(engine, principal)).await;
    tx_permit.take();
}
