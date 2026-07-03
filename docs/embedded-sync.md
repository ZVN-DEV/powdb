# PowDB Embedded Sync

Status: implementation in progress. This page is the V1 product contract for
the embedded-replica variant. The public JS orchestration package now exists in
`clients/sync` as experimental code, and `@zvndev/powdb-embedded` exposes the
native `applyRetainedUnits(...)` binding. Real `powdb-server` +
`@zvndev/powdb-client` status/pull/ack coverage now exists, and
`@zvndev/powdb-sync` has a full backup-bootstrap + native local-apply e2e test,
and the current JS sync vertical slice is wired into CI. The embedded-sync
variant is still not public beta-ready until stale/rebootstrap flows, DDL
policy, crash/interruption behavior, idempotency/unknown-outcome behavior,
metrics deployment-surface policy, broader production audit logging, and
versioned package publishing are hardened.

## What V1 Means

PowDB Embedded Sync is a primary-authoritative embedded replica mode:

- local reads run against an embedded PowDB database,
- writes are forwarded to the remote primary,
- sync pulls primary state down to the local replica,
- reads may be stale until pull completes,
- no offline write queue exists in V1,
- replicas are full-database replicas, not row-filtered or tenant-filtered
  partial replicas.

This keeps the first product small enough to make correct. It is closer to
Turso-style embedded reads with remote writes than to a multi-primary sync
engine.

## Consistency Contract

`queryReadonly(...)` reads local state. It should be fast and available even
when the network is down, but it can return stale rows.

`write(...)` sends DML to the primary. It succeeds only if the primary commits.
If the remote is unavailable, the write fails; PowDB must not silently enqueue
the write for later.

After a successful remote write, the next local read sees that write only after
`syncNow()` or background sync applies the primary's retained tail locally.
Until then, `syncStatus().stale` remains true and exposes the lag.
`WriteResult.localVisible` is a guarantee, not a guess: it is true only when
the local replica has caught up far enough for a local read to include the
remote write. If local retained-unit apply succeeds but primary acknowledgement
fails, callers receive `localVisibility: "applied_but_unacked"` plus the local
applied and remote LSNs.

DDL through V1 `write(...)` is rejected unless the same release includes schema
propagation, lagging-replica DDL, and crash-safety tests.

## Write Failure Contract

The dangerous case is: the primary commits, but the client loses the response.
Current V1 handles that by returning a typed `commit_outcome_unknown` error
instead of retrying blindly. Idempotency-key transport is planned server/client
work, not part of the current JS package API:

- future path: caller supplies an idempotency key, so retry is safe,
- current path: return a typed `commit_outcome_unknown` error,
- never retry a non-idempotent write in a way that can double-apply silently.

## Status Surface

Every embedded replica needs a status surface that answers "why is my read
stale?" without debug logs:

- `lastAppliedLsn`
- `remoteLsn`
- `servableLsn`
- `unarchivedLsn`
- `lagBytes`
- `lagMs`
- `stale`
- `lastSyncError`
- recommended repair action, such as retry, pull, await archive, or rebootstrap.

Current crate-level status fields are progress indicators, not authorization or
completeness proofs. `remoteLsn` is primary progress; `servableLsn` is the
largest contiguous retained-unit LSN the server can currently ship to this
replica; `unarchivedLsn` is the gap between those two. `lagBytes` is an estimate
from retained segment files that overlap the servable unapplied LSN range.
`lagMs` currently means time since the primary-side cursor was last acknowledged
while stale; true commit-age lag requires remote commit timestamps in the future
wire protocol.

## Unsupported In V1

- offline local writes,
- partial row or tenant sync,
- multi-primary writes,
- automatic sharding,
- Raft-style consensus,
- Postgres wire compatibility,
- full SQL compatibility beyond PowDB's documented SQL subset.

## Current Implementation

The current implementation work is split between the Rust sync substrate, the
private server/client protocol, and the experimental public JS orchestration
boundary:

- `powdb-sync` persists retained WAL-record segments with identity, format,
  range, and checksum validation.
- sync-aware checkpoint and recovery paths archive WAL records before
  truncation.
- plain checkpoint and plain recovery fail closed for sync-enabled WAL history
  when no archive hook is supplied.
- backup manifests carry optional sync fork-safety metadata.
- `powdb-cli sync-enable` creates a sync identity and retained checkpoint for an
  offline/admin data dir. `powdb-cli sync-bootstrap <BKP> <REPLICA_DIR>
  <REPLICA_ID>` restores a sync-enabled full backup into a replica and publishes
  the primary-side cursor. CLI `backup` now opens through the sync-aware
  lifecycle so pending sync WAL is archived before backup recovery truncates it.
- `powdb-cli sync-status [REPLICA_ID]` gives operators an offline primary-side
  status view over registered replica cursors: local/remote LSNs, servable and
  unarchived retained history, lag estimates, stale state, and the recommended
  repair action (`none`, `pull`, `awaitArchive`, or `rebootstrap`).
  This is a sync-aware maintenance command, not a passive filesystem probe: it
  opens the data dir through the sync-aware lifecycle and may archive/checkpoint
  pending WAL. The `repairAction` is an operator hint derived from primary
  cursor state and retained-tail continuity; actual pull/apply compatibility is
  still enforced later by the wire protocol and local applier.
- ordinary restore strips sync identity so restored data dirs remain safe for
  the plain PowDB engine lifecycle; `powdb-cli restore --sync-preserve` keeps
  the source identity for disaster recovery, and `--sync-fork` mints a fresh
  identity for clone/fork restores.
- retention GC now uses active replica cursors, shares the cursor metadata lock
  with cursor publication, rejects stale cursor registration when retained
  history is already missing, and refuses to delete retained segments unless
  the retained tail needed by active replicas remains valid.
- retention pressure policy now reports retained byte pressure, can retire
  inactive cursors by explicit age policy, and supports an operator retain-LSN
  override that marks lagging cursors inactive so they must rebootstrap instead
  of silently stranding them.
- primary-side replica acknowledgement now advances a cursor monotonically
  after a successful local apply, rejects stale or inactive acknowledgements,
  and exposes status fields for `lastAppliedLsn`, `remoteLsn`, `servableLsn`,
  `unarchivedLsn`, `lagBytes`, `lagMs`, `stale`, `lastSyncError`, and repair
  action (`pull`, `awaitArchive`, or `rebootstrap`). The server now exposes
  private authenticated sync status, retained-tail pull, and apply-ack wire
  messages over append-only frame tags; open/no-auth connections and named
  `readonly` users cannot access sync metadata. Pull requests bind replica id,
  cursor LSN, database identity, primary generation, WAL/catalog/segment format
  versions, and a max unit/byte budget before any retained units are returned.
  Pull responses must be V1-applyable chunks: if `maxUnits` or `maxBytes`
  would cut through an explicit transaction before its commit/rollback marker,
  the server fails clearly instead of returning an unusable partial chunk.
  Apply acknowledgements validate the acknowledged retained-unit range before
  advancing the primary-side cursor, so a buggy client cannot acknowledge a
  transaction-cut LSN and strand required retained history.
  Sync frames share the normal explicit-transaction gate: a connection with an
  active transaction cannot use sync frames, and other connections wait until
  that transaction closes. `hasMore` means another retained chunk is fetchable
  now; not-yet-archived primary history is represented by `awaitArchive` and
  `unarchivedLsn`. Scalar LSN fields remain progress indicators, not
  authorization or contiguous-apply proofs.
- `powdb-server` Prometheus metrics now include low-cardinality sync
  operations: `powdb_sync_operations_total{operation,result}`,
  `powdb_sync_operation_duration_seconds{operation}`,
  `powdb_sync_repair_actions_total{operation,repair_action}`,
  `powdb_sync_pull_units_total`, `powdb_sync_pull_bytes_total`, and
  `powdb_sync_ack_advanced_total`. These metrics intentionally do not expose
  replica ids, data paths, query text, credentials, database identity bytes, or
  retained payload bytes as labels or values beyond aggregate pull byte counts.
  The metrics endpoint remains the existing opt-in, unauthenticated
  `--metrics-addr` / `POWDB_METRICS_ADDR` surface; sync-capable deployments
  should bind it only to a private scrape network until an authenticated metrics
  surface exists.
- `powdb-server` now emits structured `tracing` logs for sync status, pull, and
  acknowledgement decisions. The log fields include operation, LSNs, lag,
  unit/byte counts, repair action, whether an acknowledgement advanced, bounded
  typed error classes for rejects, elapsed time, and a stable redacted replica
  fingerprint. They intentionally do not include raw replica ids, database
  identity bytes, local paths, credentials, query text, retained payload bytes,
  or raw filesystem-derived error strings. The fingerprint is pseudonymous
  operator-correlation data, not strong anonymization; do not export it as
  anonymous telemetry without a separate keyed redaction layer.
- `@zvndev/powdb-client` now has experimental low-level authenticated sync helpers
  (`syncStatus`, `syncPull`, and `syncAck`) plus typed retained-unit/status
  decoding and a `sync` observability event. This removes hand-rolled frame
  code from the embedded-replica package; callers should pin matching
  client/server versions while dogfooding it. The TS client now has a live
  integration test that starts a real password-authenticated `powdb-server`,
  bootstraps a primary-side replica cursor, performs a post-bootstrap write,
  gracefully restarts the server so retained WAL units are archived, then
  verifies `syncStatus`, `syncPull`, and `syncAck` over the real wire protocol.
- `@zvndev/powdb-sync` now provides the first public orchestration boundary for
  embedded replicas. `PowDBSyncReplica` delegates readonly queries to a local
  embedded adapter, pulls bounded retained chunks from the primary, validates
  contiguous LSNs before local apply, acknowledges only after local apply
  reports the expected through-LSN, validates ack LSN/status before reporting
  success, rejects DDL through `write(...)`, supports deferred or immediate sync
  after remote writes, and returns typed stale, rebootstrap,
  remote-unavailable, remote-write-failed, applied-but-unacked, and
  `commit_outcome_unknown` outcomes. The package is intentionally structural:
  it depends on adapter interfaces instead of importing the native embedded
  binding at runtime, and it validates required adapter capabilities at
  construction time so mismatches fail before sync starts. Package coverage now
  includes a native-adapter integration test that drives
  `PowDBSyncReplica.write(...)` through the real `@zvndev/powdb-embedded`
  local adapter and a deterministic fake primary, plus a full e2e test that uses
  `powdb-cli sync-enable`, `powdb-cli backup`, a real `powdb-server`, the real
  TS client, `powdb-cli sync-bootstrap`, native local readonly queries, and
  native retained-unit apply to prove post-snapshot remote writes converge into
  the local replica.
- `@zvndev/powdb-embedded` now exposes `Database.applyRetainedUnits(...)`, the
  native local adapter for `@zvndev/powdb-sync`. The binding validates
  `databaseId` as either a 32-character hex string or a 16-byte `Uint8Array`,
  retained payload bytes as `Uint8Array` or `Buffer`, BigInt u64 fields, format
  versions, retained-unit record type width, and then calls the same crash-safe
  `powdb-sync` chunk applier used by Rust tests. Current addon coverage proves
  the binding is callable through the real native module, accepts byte-form
  identity and retained payload input, applies a non-empty retained-unit chunk,
  and rejects malformed identity input.
- sync-aware open now returns a lifecycle owner that archives later writes on
  drop, so sync-enabled clean shutdown does not strand retained WAL history.
- backup-based cold-start bootstrap can restore a sync snapshot, archive the
  primary's live WAL tail, prove retained-tail continuity and V1 applyability
  through the current primary LSN, reject unsupported DDL tails before restore,
  and publish an active primary-side replica cursor. If cursor publication
  fails after restore, bootstrap removes the restored replica directory.
- `powdb-sync::apply_retained_tail` can apply a complete validated retained
  tail to a restored same-lineage replica. `apply_retained_units_chunk` applies
  an already-pulled, contiguous, V1-applyable retained-unit chunk only when the
  chunk starts at a trusted local apply boundary seeded by bootstrap or promoted
  by a previous completed chunk. This gives the applier local start-boundary
  provenance instead of trusting arbitrary caller-provided `sinceLsn` values. An
  embedded replica can apply bounded chunks and run local reads between
  successful chunk boundaries. Current fixtures prove post-snapshot
  insert/update/delete convergence, index-backed lookup after apply, duplicate
  chunk no-op behavior, restart between chunks, and coherent reads after a
  first chunk before a second chunk catches up to the primary.
- retained-tail apply now persists `.powdb-sync/apply-state.json` before local
  replay and marks it complete after replay. Retry replays from the recorded
  safe apply watermark only when the recorded identity/range matches and the
  catalog is still at that watermark. If storage replay reached the target LSN
  but the complete marker was not written, retry marks the apply complete; if
  the catalog advanced only partway through the range, retry fails closed for
  repair/rebootstrap. Backup bootstrap now seeds a zero-width complete apply
  state at the verified snapshot LSN, and each completed chunk replaces it with
  the newly trusted boundary.
- V1 applyability validation rejects unsupported retained DDL units and
  explicit transaction ranges that end before the transaction reaches commit or
  rollback, preventing storage replay from skipping rows and advancing LSN.
- current retained-tail chunk apply is a synchronous, catalog-write-lock
  primitive. The JS package now orchestrates explicit pull/apply/ack and
  write-forward calls around that primitive, and the embedded package now
  exposes the native apply adapter. `PowDBSyncReplica.startBackgroundSync(...)`
  now schedules explicit `syncNow()` calls for online catch-up without
  overlapping runs, with stop/abort support and result/error callbacks.
- storage now persists a catalog-level LSN sidecar so DDL-only snapshots and
  skipped recovery records cannot cause retained LSN reuse or collapse backup
  cursors to the last row-page LSN.

## Next Gates

Before embedded-sync public beta:

1. broaden the full backup-bootstrap + native embedded local-apply e2e coverage
   to missing-history rebootstrap-required repair, DDL rejection/propagation
   policy, restart between pull chunks, and crash or interrupted-apply recovery,
2. add server-side idempotency-key support for write-forward, or keep
   `commit_outcome_unknown` as the documented non-idempotent failure mode,
3. harden sync metrics deployment policy and broaden production audit/slow-query
   logs,
4. either propagate index/unique DDL safely or keep explicit sync-time DDL
   rejection with integration coverage,
5. wire package publishing/release checks so `@zvndev/powdb-client`,
   `@zvndev/powdb-embedded`, and `@zvndev/powdb-sync` versions remain locked
   together across published artifacts.
