# Review Packet — PowDB Embedded Sync

Date: 2026-06-30
Status: Approved direction; first substrate slice in progress

## What To Review First

Read these in order:

1. `docs/strategy/2026-06-30-embed-sync-plan-review-report.md`
2. `docs/strategy/2026-06-30-embed-sync-product-plan.md`
3. `docs/strategy/2026-06-30-embed-sync-sprint-plan.md`
4. `docs/strategy/2026-06-30-embed-sync-test-spec.md`
5. `docs/strategy/2026-06-30-continuous-production-review-plan.md`

This packet is the decision summary so review does not require holding every detail in memory.

## Proposed Product Decision

Ship **PowDB Embedded Sync V1** as a primary-authoritative embedded replica:

- local embedded reads,
- writes forwarded to the remote PowDB primary,
- sync-down from primary to local replica,
- visible replica lag,
- no silent offline write queue in V1.

This is intentionally narrower than "local-first multi-master." It is the smallest useful synced embedded product that can be verified against PowDB's current storage model.

Target V1 user: Node/Electron/native app developers building read-heavy local tools backed by their own PowDB primary. It is not for offline-first collaborative apps or tenant-filtered mobile sync.

## Decisions Needed

| Decision | Proposed answer | Why |
|---|---|---|
| V1 sync mode | Primary-authoritative embedded replica | Useful, testable, and aligned with PowDB's physical replication substrate. |
| First public JS package | `@zvndev/powdb-sync` | Keeps `@zvndev/powdb-embedded` as the simple local engine package. |
| Rust crate boundary | new `powdb-sync` crate | Keeps replication below the query layer and out of the planner. |
| Bootstrap substrate | reuse `powdb-backup` snapshot/manifest concepts | Avoids a second backup/snapshot format. |
| First implementation slice | retained replication-unit segment format + archive-before-truncate proof | Sync cannot be credible until retention is durable. |
| Offline writes | defer to V2 | Requires conflict policy, retained units, rebase/push, and crash tests. |
| Partial sync | defer to logical CDC | Physical retained units are full-DB/page granularity, not tenant/user row subsets. |
| Raft/sharding | defer until proven customer bottleneck | Large scope and inconsistent with current single-node performance thesis. |
| Postgres compatibility | do not promise wire compatibility or full SQL | PowDB can be a practical alternative without being a drop-in clone. |
| DDL in V1 writes | reject DDL through `write()` by default | Schema propagation needs separate lagging-replica tests. |
| Read-your-writes | no local-read guarantee until pull/sync completes | Keeps stale local reads honest and observable. |

## Red Lines

Pause implementation and replan if review makes any of these required for V1:

- V1 must include offline local writes.
- V1 must include partial row-level sync.
- V1 must be Postgres wire compatible.
- V1 must include built-in Raft, active-active, or automatic sharding.
- Sync is allowed to change planner purity or make `crates/query` distribution-aware.
- V1 must guarantee local read-your-writes immediately after remote write without a completed pull.
- V1 must forward DDL before schema propagation tests exist.

Those are possible future products, but they require a different architecture and should get separate plans.

## What Is Already Shipped

- Embedded Rust facade: `crates/powdb`
- Node addon: `bindings/node`
- Package: `@zvndev/powdb-embedded`
- Network client: `@zvndev/powdb-client`
- WAL with LSN/CRC primitives
- Backup/restore crate
- Server auth/TLS/metrics foundations
- `powdb-sync` crate with retained-unit segment format, no-clobber atomic publish, validated range read, durable sync identity, replica cursor metadata with stale-lock recovery, cursor-locked retention GC, stale cursor rejection, and corruption/missing-history tests
- `powdb-backup` manifests with optional sync fork-safety metadata, default restore that strips sync identity for plain-engine safety, and explicit preserve/fork sync-identity restore modes

## What Is Not Yet Built

- `@zvndev/powdb-sync` package
- sync protocol
- retention pressure policy for inactive cursor expiry, max-retained-bytes, operator override, alerting, and rebootstrap behavior
- replica apply loop
- write-forward high-level API
- sync lag metrics
- sync-specific crash/recovery test suite

## First Implementation Gate

Do not build the JS package first. Build the substrate first.

First substrate gates:

1. define the retained replication unit precisely,
2. segment format with magic/version/checksum/LSN range,
3. append/read retained units by LSN range,
4. durable database identity and primary-side replica cursor metadata,
5. archive-before-truncate proof with atomic publish and directory fsync,
6. corrupt/partial segment rejection,
7. snapshot identity/fork-safety metadata,
8. retained-tail apply after bootstrap fixture.

This slice has no public product API. It proves that the data needed for sync survives checkpoint, crash, and restart.

Status after implementation start:

- Done: retained unit defined as the current WAL record in `docs/design/2026-06-30-retained-replication-unit-log-design.md`.
- Done: `crates/sync` segment envelope with magic/version/WAL format/catalog format/database id/primary generation/unit count/LSN range/footer CRC.
- Done: atomic segment publish via temp write, file sync, no-clobber hard-link publish, directory fsync, temp cleanup, and second directory fsync on Unix.
- Done: identity-aware range reads by LSN with gap/overlap/header-range validation and corrupt/truncated/impossible-header rejection.
- Done: durable sync identity plus serialized primary-side replica cursor metadata with active minimum-retained-LSN calculation and stale-lock recovery.
- Done: full and incremental backup manifests preserve sync fork-safety metadata when identity exists; default restore strips sync identity for plain-engine safety; explicit preserve restore keeps same-lineage identity; explicit fork restore mints a new sync identity; legacy backups remain valid.
- Done: sync-aware checkpoint, recovery, engine open/drop, and rollback archive WAL records into retained segments before truncation; plain checkpoint/recovery fails closed for sync-enabled WAL history when no archive hook is provided, and archive-hook rollback failure leaves the transaction retryable.
- Done: cursor-based retention GC uses active replica metadata, shares the cursor metadata lock with cursor publication, rejects stale cursor publication when retained history is already missing, and fails closed before deletion if the retained tail is corrupt, gapped, overlapping, or from the wrong identity.
- Done: checkpoint and backup refuse to run during active explicit transactions; failed full/incremental backups do not persist uncommitted rows; sync-aware rollback archives committed pre-transaction WAL before recovery truncates it.
- Done: retention pressure policy, crash-safe complete-tail retained apply, and private authenticated server status/pull/ack protocol with pull output capped by both the server's authoritative remote LSN and the currently servable retained LSN. Sync pull and ack now validate V1 transaction boundaries so chunk limits or buggy clients cannot strand retained history at a mid-transaction cursor. Sync frames share the server transaction gate, `hasMore` means another retained chunk is currently fetchable, and status separates `remoteLsn`, `servableLsn`, `unarchivedLsn`, `pull`, `awaitArchive`, and `rebootstrap`.
- Pending: concurrent-read chunked retained apply, write-forward, sync metrics, public JS package, and broader repair/DDL propagation fixtures.

## Review Checklist

Approved direction, with implementation guardrails:

- [x] V1 product shape: primary-authoritative embedded replica.
- [x] Package name: `@zvndev/powdb-sync`.
- [x] Rust crate name: `powdb-sync` / `crates/sync`.
- [x] No offline local writes in V1.
- [x] No partial sync in V1.
- [x] No Raft/sharding in V1.
- [x] First implementation slice is retained replication units, not UI/API.
- [x] Replication unit is precisely defined before checkpoint/protocol code.
- [x] Do not break existing main PowDB features.
- [x] Do not delete existing features without explicit Kirby approval.
- [ ] DDL is rejected through V1 `write()` until schema propagation tests exist.
- [ ] Local read-your-writes requires completed sync/pull.
- [ ] Release blockers in the test spec are strict enough.
- [ ] Continuous review loop matches how you want ongoing PowDB work run.

## If Approved

Next cycle should be:

1. add retention-pressure policy tests for inactive cursor expiry, max-retained-bytes, operator override, alerting, and rebootstrap behavior,
2. add crash-injection tests around segment publish, checkpoint, recovery, and cursor update,
3. add transaction-aware chunked apply and repair fixtures on top of the crash-safe retained-tail apply-state path,
4. run storage/backup/sync tests,
5. run code-review and architecture review again before any JS package work.

## If Rejected Or Changed

Update the product plan before implementation. The most likely branching decisions are:

- V1 must support offline writes,
- package naming should differ,
- sync should be exposed inside `@zvndev/powdb-embedded`,
- enterprise replication/read replicas should be prioritized before embedded sync,
- partial sync is mandatory for the first launch.

Any of those changes should alter the sprint plan and test spec before code starts.
