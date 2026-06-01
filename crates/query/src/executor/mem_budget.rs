//! Per-query memory budget accumulator (WS2).
//!
//! The blunt row-count caps (`MAX_SORT_ROWS`, `MAX_JOIN_ROWS`) cannot stop a
//! query that materializes a small number of very large rows, and they don't
//! cover GROUP BY hash tables or IN-list materialization at all. A crafted
//! query could therefore OOM-kill the server process — fatal on AWS / Railway /
//! Cloudflare where the process has a hard memory ceiling.
//!
//! This module adds a lightweight byte-budget accumulator that each
//! materialization point charges as it grows its buffer. When the running
//! total would exceed the configured limit we return
//! [`QueryError::MemoryLimitExceeded`] cleanly — no panic, no partial state.
//!
//! ## Why a thread-local accumulator
//!
//! The read path runs concurrently behind `Arc<RwLock<Engine>>`: many threads
//! call `execute_powql_readonly(&self)` at once. A single accumulator field on
//! the `Engine` would (a) make `Engine` `!Sync` if it used `Cell`, and (b) be
//! *wrong* even with an atomic, because concurrent queries would sum and reset
//! each other's totals. Each query, however, runs to completion synchronously
//! on a single thread (`spawn_blocking` → `dispatch_query` → `execute_powql*`),
//! so a thread-local running total is both correct and contention-free. The
//! per-query limit is passed explicitly (it lives on the `Engine` as a plain
//! `usize`, which is `Copy`/`Sync`).
//!
//! Disk-spill (so over-budget queries still succeed) is explicitly deferred to
//! Phase 3; for now over-budget is a clean error.

use std::cell::Cell;

use powdb_storage::types::Value;

use crate::result::QueryError;

/// Default per-query memory budget: 256 MB. Plumbed from
/// `POWDB_QUERY_MEMORY_LIMIT` by the server.
pub const DEFAULT_QUERY_MEMORY_LIMIT: usize = 256 * 1024 * 1024;

thread_local! {
    /// Bytes charged so far for the query currently executing on this thread.
    /// Reset to zero at every *outermost* query entry via [`enter`].
    static USED_BYTES: Cell<usize> = const { Cell::new(0) };

    /// Reentrancy depth: how many budgeted statements are active on this
    /// thread's call stack. `create_view` / `refresh_view` recursively call
    /// `execute_powql` mid-statement, so the inner entry runs at depth > 0.
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Enter a budgeted statement frame. Returns an [`EnterGuard`] that decrements
/// the depth on drop so the count stays correct even if execution unwinds via
/// `?`/panic. Only the *outermost* entry (depth 0 → 1) zeroes the accumulator;
/// nested entries (e.g. the source query of a `create_view`/`refresh_view`
/// recursively calling `execute_powql`) leave the outer frame's charged bytes
/// intact. This is a reentrancy guard rather than a save/restore because it is
/// simpler — there is exactly one running total to protect and the guard makes
/// the "outermost statement owns the reset" rule self-evident at the call site.
#[inline]
#[must_use = "the guard must be held for the duration of the statement"]
pub fn enter() -> EnterGuard {
    DEPTH.with(|d| {
        let depth = d.get();
        if depth == 0 {
            USED_BYTES.with(|u| u.set(0));
        }
        d.set(depth + 1);
    });
    EnterGuard
}

/// RAII guard returned by [`enter`]; decrements the reentrancy depth on drop.
pub struct EnterGuard;

impl Drop for EnterGuard {
    #[inline]
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Reset the per-query running total for the current thread, unconditionally.
/// Test-only escape hatch; production code goes through [`enter`] so nested
/// statements never clobber an outer frame's accounting.
#[cfg(test)]
pub fn reset() {
    USED_BYTES.with(|u| u.set(0));
    DEPTH.with(|d| d.set(0));
}

/// Charge `bytes` against the current thread's running total, checking it
/// against `limit_bytes`. Returns [`QueryError::MemoryLimitExceeded`] if this
/// allocation would push the total over the limit. On error nothing is charged.
#[inline]
pub fn charge(bytes: usize, limit_bytes: usize) -> Result<(), QueryError> {
    USED_BYTES.with(|u| {
        let requested = u.get().saturating_add(bytes);
        if requested > limit_bytes {
            return Err(QueryError::MemoryLimitExceeded {
                limit_bytes,
                requested_bytes: requested,
            });
        }
        u.set(requested);
        Ok(())
    })
}

/// Bytes charged so far on the current thread (test/diagnostic helper).
#[cfg(test)]
pub fn used() -> usize {
    USED_BYTES.with(|u| u.get())
}

/// Estimate the in-memory footprint of a single `Value`, including the heap
/// allocation behind `Str`/`Bytes`. The estimate counts the enum slot plus any
/// owned heap bytes — it is intentionally an over-approximation so the guard
/// trips slightly early rather than slightly late.
#[inline]
pub fn estimate_value_size(v: &Value) -> usize {
    let base = std::mem::size_of::<Value>();
    let heap = match v {
        Value::Str(s) => s.capacity(),
        Value::Bytes(b) => b.capacity(),
        _ => 0,
    };
    base + heap
}

/// Estimate the in-memory footprint of a fully materialized row, including the
/// `Vec<Value>` backing allocation.
#[inline]
pub fn estimate_row_size(row: &[Value]) -> usize {
    let mut total = std::mem::size_of::<Vec<Value>>();
    for v in row {
        total += estimate_value_size(v);
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charge_under_limit_succeeds() {
        reset();
        assert!(charge(512, 1024).is_ok());
        assert!(charge(512, 1024).is_ok());
        assert_eq!(used(), 1024);
    }

    #[test]
    fn charge_over_limit_errors_without_charging() {
        reset();
        assert!(charge(512, 1024).is_ok());
        let err = charge(1024, 1024).unwrap_err();
        match err {
            QueryError::MemoryLimitExceeded {
                limit_bytes,
                requested_bytes,
            } => {
                assert_eq!(limit_bytes, 1024);
                assert_eq!(requested_bytes, 1536);
            }
            other => panic!("expected MemoryLimitExceeded, got {other:?}"),
        }
        // The failed charge did not advance the counter.
        assert_eq!(used(), 512);
    }

    #[test]
    fn nested_enter_preserves_outer_accounting() {
        // Simulates an outer statement that charges bytes, then recursively
        // enters a nested statement (as create_view/refresh_view do when their
        // source query calls execute_powql). The nested `enter` must NOT reset
        // the running total, so the outer frame's accounting survives.
        reset();
        let outer = enter();
        assert_eq!(used(), 0, "outermost enter zeroes the accumulator");
        charge(1000, 1_000_000).unwrap();
        assert_eq!(used(), 1000);

        {
            // Nested statement entry (depth 1 -> 2): must not reset.
            let _inner = enter();
            assert_eq!(used(), 1000, "nested enter must not discard outer bytes");
            charge(500, 1_000_000).unwrap();
            assert_eq!(used(), 1500);
        } // inner guard drops, depth 2 -> 1, accumulator untouched

        assert_eq!(used(), 1500, "outer accounting includes nested charges");
        drop(outer); // depth 1 -> 0

        // A fresh outermost statement starts clean again.
        let _next = enter();
        assert_eq!(used(), 0, "next outermost statement resets");
    }

    #[test]
    fn string_value_counts_heap_bytes() {
        let small = estimate_value_size(&Value::Int(1));
        let big = estimate_value_size(&Value::Str("x".repeat(10_000)));
        assert!(big >= small + 10_000);
    }
}
