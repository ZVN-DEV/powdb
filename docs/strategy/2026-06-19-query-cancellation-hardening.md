# Query Cancellation Hardening Plan

Date: 2026-06-19

PowDB 0.6.1 uses a conservative timeout mitigation: if a `spawn_blocking`
query exceeds `POWDB_QUERY_TIMEOUT`, the server records the timeout threshold
breach but waits for the blocking task to finish before replying. This avoids
the unsafe old behavior where a client could receive `query timeout exceeded`
while the same query continued running and possibly mutating state in the
background.

Full cancellation is still not implemented. Tokio cannot abort a
`spawn_blocking` task after it has started, so proper cancellation must be
cooperative inside PowDB's synchronous engine.

## Required Follow-Up

1. Add a `QueryContext` or `CancelToken` accepted by every public execution path:
   `execute_powql`, `execute_powql_readonly`, SQL variants, and params variants.
2. Store a deadline or atomic cancellation flag in that context.
3. Check the context at all long-running executor/materialization boundaries:
   table scans, mmap raw scan closures, joins, sorts, group-by, subqueries,
   view refresh, and large insert/update/delete loops.
4. Check the context before write commit boundaries so cancellation cannot leave
   a partially-applied statement visible.
5. Keep transaction semantics explicit: if a statement is cancelled inside an
   explicit transaction, rollback the statement/transaction and release the
   connection's transaction gate only after rollback completes.
6. Add integration tests that prove a cancelled long read releases read locks,
   a cancelled write does not become visible, and a timed-out explicit
   transaction cannot clobber a later transaction.
7. Once those tests pass, restore client-visible `query timeout exceeded`
   responses for actually-cancelled queries.

