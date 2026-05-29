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
//! Disk-spill (so over-budget queries still succeed) is explicitly deferred to
//! Phase 3; for now over-budget is a clean error.

use std::cell::Cell;

use powdb_storage::types::Value;

use crate::result::QueryError;

/// Default per-query memory budget: 256 MB. Plumbed from
/// `POWDB_QUERY_MEMORY_LIMIT` by the server.
pub const DEFAULT_QUERY_MEMORY_LIMIT: usize = 256 * 1024 * 1024;

/// A per-query byte-budget accumulator.
///
/// Cheap: a single `Cell<usize>` plus the immutable limit. Charged on the
/// read path (`&self`) so the field uses interior mutability rather than
/// requiring `&mut`. One budget is created per top-level query and is **not**
/// shared across queries, so the `Cell` is never touched from two threads.
#[derive(Debug)]
pub struct MemoryBudget {
    limit_bytes: usize,
    used_bytes: Cell<usize>,
}

impl MemoryBudget {
    /// Create a budget with the given byte limit.
    pub fn new(limit_bytes: usize) -> Self {
        MemoryBudget {
            limit_bytes,
            used_bytes: Cell::new(0),
        }
    }

    /// Charge `bytes` against the budget. Returns
    /// [`QueryError::MemoryLimitExceeded`] if this allocation would push the
    /// running total over the limit. On error nothing is charged (the caller
    /// has not yet performed the allocation).
    #[inline]
    pub fn charge(&self, bytes: usize) -> Result<(), QueryError> {
        let requested = self.used_bytes.get().saturating_add(bytes);
        if requested > self.limit_bytes {
            return Err(QueryError::MemoryLimitExceeded {
                limit_bytes: self.limit_bytes,
                requested_bytes: requested,
            });
        }
        self.used_bytes.set(requested);
        Ok(())
    }

    /// Charge the estimated heap+stack footprint of a fully materialized row.
    #[inline]
    pub fn charge_row(&self, row: &[Value]) -> Result<(), QueryError> {
        self.charge(estimate_row_size(row))
    }

    /// Reset the running total to zero (reuse the same budget for a fresh
    /// query). Works through `&self` so the read path (`&self`) can reset it.
    #[inline]
    pub fn reset(&self) {
        self.used_bytes.set(0);
    }

    /// Bytes charged so far (test/diagnostic helper).
    #[cfg(test)]
    pub fn used(&self) -> usize {
        self.used_bytes.get()
    }
}

/// Estimate the in-memory footprint of a single `Value`, including the heap
/// allocation behind `Str`/`Bytes`. The estimate counts the enum slot plus any
/// owned heap bytes — it is intentionally an over-approximation (rounds the
/// enum size up) so the guard trips slightly early rather than slightly late.
#[inline]
pub fn estimate_value_size(v: &Value) -> usize {
    // `Value` is an enum whose largest inline variant is `Str(String)` /
    // `Bytes(Vec<u8>)` (3 words) plus the discriminant. Use the actual size.
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
        let b = MemoryBudget::new(1024);
        assert!(b.charge(512).is_ok());
        assert!(b.charge(512).is_ok());
        assert_eq!(b.used(), 1024);
    }

    #[test]
    fn charge_over_limit_errors_without_charging() {
        let b = MemoryBudget::new(1024);
        assert!(b.charge(512).is_ok());
        let err = b.charge(1024).unwrap_err();
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
        assert_eq!(b.used(), 512);
    }

    #[test]
    fn string_value_counts_heap_bytes() {
        let small = estimate_value_size(&Value::Int(1));
        let big = estimate_value_size(&Value::Str("x".repeat(10_000)));
        assert!(big >= small + 10_000);
    }
}
