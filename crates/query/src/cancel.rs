//! Cooperative query cancellation.
//!
//! An unindexed nested-loop join (or any other unbounded executor loop) can run
//! for tens of seconds at full CPU on a crafted dataset. Nothing inside the
//! executor used to check a deadline, so a per-query timeout could not actually
//! stop such a query: it held the engine lock / transaction gate for its whole
//! run, wedging even reads on other connections, and a client disconnect could
//! not free the CPU either. This module adds a cheap cooperative-cancellation
//! signal that the server installs per query and every unbounded executor loop
//! polls, so an over-budget query returns a clean, typed error promptly and
//! releases its locks.
//!
//! ## Why a thread-local token
//!
//! This mirrors the memory-budget accumulator (see [`crate::executor::mem_budget`]):
//! each query runs to completion synchronously on a single `spawn_blocking`
//! thread, so a thread-local slot holding the current query's cancellation
//! token is both correct and contention-free. The token itself is an
//! `Arc<ExecCancel>` so the async side of the server can hold a clone and flip
//! it (on timeout or client disconnect) while the executor thread reads it.
//!
//! ## How loops poll it
//!
//! Reading the token on every row would add a thread-local lookup (and, for the
//! deadline, an `Instant::now()`) to the hottest loop in the engine. Instead
//! each loop holds a [`CancelCheck`] with a plain local counter and calls
//! [`CancelCheck::tick`] once per iteration; only every 4096th tick performs the
//! real check. The amortized cost is a register increment and a mask test.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::result::QueryError;

const STATE_RUNNING: u8 = 0;
const STATE_TIMEOUT: u8 = 1;
const STATE_DISCONNECT: u8 = 2;

/// One real cancellation check per this many [`CancelCheck::tick`] calls.
/// A power of two so the gate is a mask test. 4096 rows of nested-loop work
/// between checks bounds the post-deadline overrun to sub-millisecond while
/// keeping the per-row cost to an increment.
const CHECK_INTERVAL_MASK: u32 = 0xFFF;

/// Why a query was cancelled. Determines the (wire-safe) error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    /// The per-query deadline elapsed.
    Timeout,
    /// The client that issued the query disconnected.
    Disconnect,
}

/// Shared cancellation signal for one executing query.
///
/// Construct with [`ExecCancel::with_deadline`] (server path) or
/// [`ExecCancel::new`] (never auto-cancels; useful for explicit `cancel` only).
/// The async side keeps a clone and calls [`ExecCancel::cancel`]; the executor
/// thread reads it through the installed thread-local via [`CancelCheck`].
#[derive(Debug)]
pub struct ExecCancel {
    state: AtomicU8,
    deadline: Option<Instant>,
    timeout_ms: u64,
}

impl ExecCancel {
    /// A token with no deadline. Only an explicit [`ExecCancel::cancel`] trips it.
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(STATE_RUNNING),
            deadline: None,
            timeout_ms: 0,
        }
    }

    /// A token that trips itself once `deadline` passes. `timeout_ms` is the
    /// configured timeout used to render the timeout error message.
    pub fn with_deadline(deadline: Instant, timeout_ms: u64) -> Self {
        Self {
            state: AtomicU8::new(STATE_RUNNING),
            deadline: Some(deadline),
            timeout_ms,
        }
    }

    /// Explicitly cancel for `reason`. Idempotent; the first reason to win
    /// stands. Safe to call from any thread.
    pub fn cancel(&self, reason: CancelReason) {
        let want = match reason {
            CancelReason::Timeout => STATE_TIMEOUT,
            CancelReason::Disconnect => STATE_DISCONNECT,
        };
        // Only record a reason if none is set yet; a passed deadline still
        // reports Timeout via `reason()` even without this store.
        let _ = self.state.compare_exchange(
            STATE_RUNNING,
            want,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    /// Whether the query should stop: explicitly cancelled, or past its deadline.
    pub fn is_cancelled(&self) -> bool {
        self.reason().is_some()
    }

    /// The cancellation reason, or `None` if the query may continue. An
    /// explicit cancel wins; otherwise a passed deadline reports `Timeout`.
    pub fn reason(&self) -> Option<CancelReason> {
        match self.state.load(Ordering::Relaxed) {
            STATE_TIMEOUT => return Some(CancelReason::Timeout),
            STATE_DISCONNECT => return Some(CancelReason::Disconnect),
            _ => {}
        }
        if let Some(deadline) = self.deadline {
            if Instant::now() >= deadline {
                return Some(CancelReason::Timeout);
            }
        }
        None
    }

    /// The typed error this token's current reason maps to, or `None` if the
    /// query may continue.
    fn error(&self) -> Option<QueryError> {
        match self.reason()? {
            CancelReason::Timeout => Some(QueryError::Timeout {
                timeout_ms: self.timeout_ms,
            }),
            CancelReason::Disconnect => Some(QueryError::Cancelled),
        }
    }
}

impl Default for ExecCancel {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    /// The cancellation token for the query currently executing on this thread,
    /// installed by [`install`] for the duration of one query.
    static CURRENT: RefCell<Option<Arc<ExecCancel>>> = const { RefCell::new(None) };
}

/// Install `token` as the current thread's cancellation token, returning a
/// guard that restores the previous token (if any) on drop. Nesting is
/// save/restore so a recursively executed sub-statement (e.g. a view refresh)
/// remains cancellable under the outer token.
#[must_use = "the guard must be held for the duration of the query"]
pub fn install(token: Arc<ExecCancel>) -> InstallGuard {
    let prev = CURRENT.with(|c| c.borrow_mut().replace(token));
    InstallGuard { prev }
}

/// RAII guard from [`install`]; restores the previous token on drop.
pub struct InstallGuard {
    prev: Option<Arc<ExecCancel>>,
}

impl Drop for InstallGuard {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = self.prev.take());
    }
}

/// Perform the real cancellation check against the installed token. Returns the
/// typed error if the query has been cancelled or has passed its deadline.
/// Cheap no-op (one thread-local read) when no token is installed.
#[inline]
pub fn check() -> Result<(), QueryError> {
    CURRENT.with(|c| match c.borrow().as_ref() {
        Some(token) => match token.error() {
            Some(err) => Err(err),
            None => Ok(()),
        },
        None => Ok(()),
    })
}

/// A per-loop cancellation poller with a local iteration counter. Call
/// [`CancelCheck::tick`] once per loop iteration; it performs the real
/// [`check`] only every 4096th call, so the amortized per-iteration cost is a
/// register increment and a mask test.
#[derive(Debug)]
pub struct CancelCheck {
    n: u32,
}

impl CancelCheck {
    #[inline]
    pub fn new() -> Self {
        Self { n: 0 }
    }

    /// Advance the counter; every 4096th call runs the real cancellation check
    /// and propagates its typed error.
    #[inline]
    pub fn tick(&mut self) -> Result<(), QueryError> {
        self.n = self.n.wrapping_add(1);
        if self.n & CHECK_INTERVAL_MASK == 0 {
            check()
        } else {
            Ok(())
        }
    }
}

impl Default for CancelCheck {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn no_token_installed_never_cancels() {
        let mut cc = CancelCheck::new();
        for _ in 0..100_000 {
            cc.tick().expect("no token means never cancelled");
        }
        assert!(check().is_ok());
    }

    #[test]
    fn explicit_cancel_trips_check() {
        let token = Arc::new(ExecCancel::new());
        let _guard = install(Arc::clone(&token));
        assert!(check().is_ok());
        token.cancel(CancelReason::Disconnect);
        match check() {
            Err(QueryError::Cancelled) => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[test]
    fn passed_deadline_reports_timeout() {
        let token = Arc::new(ExecCancel::with_deadline(
            Instant::now() - Duration::from_millis(1),
            2000,
        ));
        let _guard = install(token);
        match check() {
            Err(QueryError::Timeout { timeout_ms }) => assert_eq!(timeout_ms, 2000),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn explicit_timeout_beats_disconnect_ordering() {
        let token = ExecCancel::new();
        token.cancel(CancelReason::Timeout);
        // A later disconnect cannot override the first recorded reason.
        token.cancel(CancelReason::Disconnect);
        assert_eq!(token.reason(), Some(CancelReason::Timeout));
    }

    #[test]
    fn guard_restores_previous_token_on_drop() {
        let outer = Arc::new(ExecCancel::new());
        let guard_outer = install(Arc::clone(&outer));
        {
            let inner = Arc::new(ExecCancel::new());
            let _guard_inner = install(Arc::clone(&inner));
            inner.cancel(CancelReason::Disconnect);
            assert!(check().is_err(), "inner token active");
        }
        // Inner guard dropped: outer token restored, still running.
        assert!(check().is_ok(), "outer token restored and running");
        outer.cancel(CancelReason::Timeout);
        assert!(check().is_err());
        drop(guard_outer);
        assert!(check().is_ok(), "no token after outermost guard drops");
    }

    #[test]
    fn tick_checks_at_interval_boundary() {
        let token = Arc::new(ExecCancel::new());
        let _guard = install(Arc::clone(&token));
        token.cancel(CancelReason::Timeout);
        let mut cc = CancelCheck::new();
        // The first 4095 ticks do not perform the real check; the 4096th does.
        for _ in 0..(CHECK_INTERVAL_MASK) {
            cc.tick().expect("pre-boundary ticks skip the real check");
        }
        assert!(cc.tick().is_err(), "the 4096th tick runs the real check");
    }
}
