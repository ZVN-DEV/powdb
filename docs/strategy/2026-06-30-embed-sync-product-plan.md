# PowDB Embedded Sync Product Plan

Date: 2026-06-30
Status: Draft for review
Scope: Product and architecture plan for the embedded-sync PowDB variant.

Review packet: `docs/strategy/2026-06-30-embed-sync-review-packet.md`
Review report: `docs/strategy/2026-06-30-embed-sync-plan-review-report.md`

## Decision Summary

PowDB already has the embedded engine front door:

- Rust facade: `crates/powdb`
- Node addon: `bindings/node`
- NPM package: `@zvndev/powdb-embedded`
- Network client: `@zvndev/powdb-client`

The next product is **PowDB Embedded Sync**: a local embedded database for reads, with server-backed synchronization and a clear consistency contract. The first shippable version should be **primary-authoritative embedded replicas**:

- Reads run locally through the embedded engine.
- Writes go to the remote PowDB primary.
- The local database syncs down committed changes from the primary.
- Replica lag is visible and measurable.

Offline local writes are a second tier. They require retained WAL history, conflict semantics, and crash-tested rebase/push behavior. Do not start with offline writes as the default product.

## Positioning

Use this framing:

> PowDB Embedded Replica gives app-owned workloads local-read latency while a PowDB primary remains the only write authority.

Do not claim:

- Drop-in Postgres replacement.
- Postgres wire compatibility.
- Full SQL compatibility.
- Built-in Raft, active-active multi-primary, or automatic sharding.
- Partial row-level sync in V1.

The correct market comparison is Turso-style embedded replicas and local-first SQLite systems, not distributed SQL databases.

Current external reference point:

- Turso embedded replicas serve reads locally and send writes to the cloud primary, then reflect changes back to the replica.
- Turso Sync separates the newer local-write model with explicit `push()` / `pull()` calls.
- Turso conflict docs describe last-push-wins behavior for sync.

Implication for PowDB: model write-forward replicas and local-write sync as distinct product modes.

## Who V1 Is For

V1 is for:

- Node/Electron/native app developers building read-heavy local tools backed by their own PowDB primary.
- Edge services that need low-latency local reads and can route writes to a primary.
- App-owned data stores where the whole local database belongs to one app/user/device scope.
- Teams that value PowQL/native Rust engine behavior more than SQLite/Postgres compatibility.

V1 is not for:

- Offline-first collaborative apps.
- Per-user partial mobile sync.
- Tenant-filtered row sync.
- Multi-primary or active-active deployments.
- Apps that need local reads to always reflect a just-forwarded write without waiting for sync.

V1 physical sync creates a full local database replica. Do not use it for row-filtered tenant/user sync.

## Competitive Frame

| Product | Relevant model | How PowDB V1 should be judged |
|---|---|---|
| Turso Embedded Replicas | Local reads, primary writes | Closest V1 category reference. |
| Turso Sync | Explicit local-write push/pull | Future V2 comparison, not V1. |
| PowerSync / Electric | Partial/offline local-first sync | Different category; requires logical CDC/row identity. |
| SQLite | Local embedded DB | PowDB's local embedded performance and PowQL story matter here. |
| Postgres | Remote primary database | PowDB can be an app-owned alternative, not a wire-compatible clone. |

First demo: Electron inventory/search app with a local product catalog, instant local reads, remote primary writes, visible lag, and a forced network outage showing reads continue while writes fail clearly.

## Product Modes

| Mode | Product name | Reads | Writes | Sync direction | Ship order |
|---|---|---:|---:|---|---:|
| Existing | Embedded | Local | Local | None | Shipped |
| V1 | Embedded Replica | Local | Remote primary | Primary -> local | First |
| V2 | Embedded Sync | Local | Local, then push | Local -> primary and primary -> local | Second |
| Later | Logical Sync | Local subset | Local or remote | Row/change level | Later |

## Non-Negotiable Invariants

- Keep the planner pure and distribution-unaware.
- Keep sync below the query layer first: WAL/retained-unit/snapshot substrate, not planner rewrites.
- Keep mmap scan performance intact for local reads.
- Do not turn physical retained-unit shipping into partial sync. Physical pages imply full-database or page-granularity replication.
- Keep the server as the source of truth for V1.
- Make every sync cursor resumable or fail closed with a repair path.
- Never truncate WAL or retained-unit history needed by a replica, backup, or restore chain.

## Package Plan

### Existing Packages

| Package | Current role | Plan |
|---|---|---|
| `powdb` | Rust embedded facade | Extend with replica/sync handles once lower substrate is ready. |
| `@zvndev/powdb-embedded` | Node embedded addon | Keep as the no-network local package. Add sync package rather than overloading this first. |
| `@zvndev/powdb-client` | Network TCP/TLS client | Reuse for write-forward and primary admin paths. |

### New Packages / Crates

| Package | Purpose |
|---|---|
| `powdb-sync` crate | Shared sync substrate: cursors, retained units, replica protocol types, applier, tests. |
| `@zvndev/powdb-sync` | High-level JS package combining local embedded reads with remote write-forward/sync. |
| `powdb-server` sync extension | Primary-side replica handshake, retained-unit pull, write-forward routing, lag metrics. |
| Optional `powdb-sync-cli` commands | Inspect replica state, reset local replica, force pull, verify chain. |

Keep `@zvndev/powdb-embedded` simple: it remains the embedded engine. `@zvndev/powdb-sync` composes embedded + network behavior and owns distributed semantics.

### Crate Boundary Rule

`powdb-sync` should sit beside the engine, not inside the planner:

- Depend on `powdb-storage` for WAL/page/log primitives.
- Reuse `powdb-backup` for snapshot/bootstrap and repair flows where possible.
- Use `powdb-server` only at the transport boundary.
- Expose controls through `powdb` / Node wrappers only as thin facades.
- Do not make `powdb-query` or `crates/query/src/planner.rs` aware of replicas, shards, policies, or sync cursors.

The planner stays a syntax-to-plan function. Sync is a storage/replication concern.

## Architecture Roadmap

### Phase 0: Harden The Existing Embedded Package

Goal: make the local package boring and trustworthy before attaching sync.

Tasks:

- Verify install/build matrix for `@zvndev/powdb-embedded`.
- Confirm `Database.open`, `query`, `querySql`, `queryReadonly`, `setSyncMode`, `openWithMemoryLimit`, and `isPoisoned` match docs.
- Add parity tests between embedded results and server `QueryResult` shapes.
- Add crash/open safety tests for corrupt files and handle poisoning.
- Add packaging smoke in CI for supported platforms.
- Document `full`, `normal`, and `off` durability in a way app developers cannot misread.

Exit criteria:

- Node addon smoke test passes on each supported CI target.
- Embedded local read/write benchmark is tracked.
- No sync code is needed to use the existing embedded package.

### Phase 1: Sync Substrate Proof

Goal: prove retained history and bootstrap before any public sync API.

Build:

- Define the exact retained replication unit before implementation. Current PowDB has WAL records and page-LSN backup deltas; the design must choose the retained unit and name it precisely.
- Segmented retained log keyed by LSN range.
- Archive-before-truncate ordering: a retained segment must be atomically published and durable before checkpoint can remove it from the crash WAL.
- Directory fsync, temp/partial segment recovery, and manifest/cursor fsync ordering.
- Snapshot identity/fork-safety metadata: database id, primary generation, source LSN, catalog hash, WAL/retained-unit format versions.
- Per-replica cursors and minimum retained LSN.
- GC policy based on the slowest required cursor, max-retention budget, inactive-cursor expiry, operator override, and alerting.
- Corrupt/missing segment detection.
- Snapshot + tail bootstrap for new or far-behind replicas, using backup snapshots as the cold-start substrate rather than inventing a second snapshot format.

Current substrate progress: retained WAL-record segments, durable sync identity,
primary-side cursor metadata, cursor-locked retained segment GC with stale cursor
rejection, explicit retention pressure policy, primary-side cursor
acknowledgement, sync status with `remoteLsn`/`servableLsn`/`unarchivedLsn`,
private authenticated server status/pull/ack wire messages, backup manifest
fork-safety metadata, plain-safe default restore, sync-aware rollback plus
archive-before-truncate checkpoint/recovery hooks, sync-aware clean-shutdown
ownership, backup-based cold-start bootstrap, complete-tail and chunked local
retained apply, durable apply-state, fail-closed partial-apply repair,
unsupported DDL rejection, transaction-split rejection, the experimental
`@zvndev/powdb-sync` JS orchestration API, write-forward through the normal
authenticated primary client, CLI sync-enable/sync-bootstrap, and a full
backup-bootstrap/server/client/native local-apply e2e are implemented. Sync
operator status is now visible through `powdb-cli sync-status`. Sync metrics,
crash/interruption coverage, rebootstrap repair helpers, DDL propagation
policy, idempotency-key transport, and package
publishing/version-locking remain before public sync APIs. CI now gates the
current JS sync vertical slice, but the broader crash/repair/metrics matrix is
still a beta prerequisite.

Exit criteria:

- A replica can restore a sync snapshot and prove retained-tail availability using backup metadata and retained segments in tests.
- A replica can apply that retained tail and converge to primary rows in tests.
- Retention GC cannot break any active cursor.
- Archive-before-truncate survives crash/restart.
- Corrupt or partial segment tests fail closed.
- Backup/PITR and sync agree on LSN and format metadata.

### Phase 2: Primary-Authoritative Embedded Replica

Goal: local reads with remote writes and sync-down catch-up.

User API sketch:

```ts
import { ReplicaDatabase } from "@zvndev/powdb-sync";

const db = await ReplicaDatabase.open({
  localPath: "./app.powdb",
  remote: {
    host: "db.example.com",
    port: 5433,
    user: "app",
    password: process.env.POWDB_PASSWORD,
    tls: true,
  },
  syncIntervalMs: 5000,
});

const users = db.queryReadonly("User filter .active = true { .id, .name }");
await db.write("insert User { name := $1 }", ["Ada"]);
await db.syncNow();

console.log(db.syncStatus());
```

Semantics:

- `queryReadonly` always reads local data.
- `write` sends the mutation to the primary.
- `write` succeeds only if the primary commits.
- `write` does not guarantee the next local read sees the write unless a pull completes.
- After a successful remote write, the local replica either pulls immediately or `syncStatus().stale` remains true until the next pull.
- If the remote commits but the client loses the response, V1 must return a typed `commit_outcome_unknown` error or use an idempotency key. Retrying a non-idempotent write must not double-apply silently.
- API exposes `lastAppliedLsn`, `remoteLsn`, `servableLsn`, `unarchivedLsn`, `lagBytes`, `lagMs`, `stale`, `lastSyncError`. V1 treats these as progress/status fields only: `remoteLsn` is primary progress, `servableLsn` is the largest contiguous retained-unit LSN currently ship-ready for the replica, `unarchivedLsn` means the primary has progressed beyond retained history, `lagBytes` is a retained-segment overlap estimate, `lagMs` is time since last acknowledged apply while stale unless the protocol later adds remote commit timestamps, and scalar LSN equality is not an authorization or contiguous-apply proof.
- Writes fail when remote is unavailable. Local offline writes are not silently queued in V1.
- V1 `write` allows DML only by default. DDL through `write` is rejected with a typed error until schema propagation and lagging-replica DDL tests are implemented.

Server substrate:

- Primary exposes a replica handshake.
- Primary exposes private authenticated status, retained-unit pull, and apply-ack
  frames. The TS client and experimental JS sync package now exercise these
  frames through live server-backed and backup-bootstrap e2e tests.
- Pull and ack are transaction-boundary-aware: V1 chunking must never return or
  acknowledge a range that cuts through an explicit transaction before its
  commit/rollback boundary.
- Replica tokens are scoped to a database and operation class.
- Server refuses incompatible catalog/WAL format versions.

Exit criteria:

- Local reads continue working when remote is temporarily unavailable.
- Writes fail clearly when remote is unavailable.
- A successful write becomes visible locally after sync.
- Concurrent local reads during apply see either the previous applied LSN or a fully applied committed LSN, never a partial batch.
- Replica lag is observable in logs and metrics.
- Crash during pull/apply does not corrupt the local DB.

### Phase 3: Offline Local Writes

Goal: explicit local-write sync mode, not default.

User API sketch:

```ts
const db = await SyncDatabase.open({ localPath, remote, conflict: "last-push-wins" });
db.query("insert Todo { title := $1 }", ["offline task"]);
await db.push();
await db.pull();
```

Conflict policy options:

- `fail-on-conflict`
- `discard-local`
- `rebase-local`
- `last-push-wins`

Do not expose `manual-resolution` until logical row-level change records exist. Physical retained units are a bad UX surface for per-row conflict resolution.

Exit criteria:

- Local writes survive process restart before push.
- Push is idempotent under retry.
- Pull can rebase or discard local retained units according to policy.
- Conflicts are deterministic and documented.
- Users can inspect unpushed local changes.

### Phase 4: Logical Sync

Goal: partial sync, tenant/user subsets, and per-row conflict UX.

Prerequisites:

- Real primary-key identity in change records.
- Logical row-change log.
- Tombstones.
- Row version or hybrid logical clock.
- Sync rules or bucket model.
- Per-row conflict resolution surface.

This is not part of the first embedded-sync launch. It is the product needed for partial mobile sync and multi-user collaboration, but it is a separate storage/query feature.

### Phase 5: Enterprise Replication And Redundancy

Goal: mature production deployment without pretending PowDB is distributed SQL.

Build in this order:

1. WAL/retained-unit shipping to object storage for DR.
2. Read replicas using the same retained replication-unit log.
3. Manual failover runbook and replica promotion tooling.
4. External-orchestrated failover.
5. Sharding research only after a real one-node ceiling appears.

Do not build built-in Raft or automatic sharding in this plan.

## Observability Requirements

Required logs:

- sync session id
- replica id
- database id/path hash
- auth principal
- local cursor
- remote cursor
- pulled retained units
- applied retained units
- bytes pulled
- apply duration
- write-forward duration
- error class
- recovery action

Required metrics:

- `powdb_sync_pull_total{result}`
- `powdb_sync_push_total{result}`
- `powdb_sync_units_applied_total`
- `powdb_sync_bytes_applied_total`
- `powdb_sync_lag_lsn`
- `powdb_sync_lag_seconds`
- `powdb_sync_last_success_timestamp_seconds`
- `powdb_sync_apply_duration_seconds`
- `powdb_wal_retained_bytes`
- `powdb_wal_retention_min_lsn`
- `powdb_replica_cursors_active`

Slow-query and sync logs must never add per-row overhead to hot read paths unless debug/trace is explicitly enabled.

Metrics label rules:

- Sync-capable deployments must expose metrics only through a private bind or authenticated surface.
- Do not expose raw database paths, raw tokens, query params, or high-cardinality replica identifiers.
- Use bounded labels and hash/redact database and replica identifiers where needed.
- Add alerts/SLOs for lag, retained bytes, stale replicas, sync failures, and retention pressure.

## Security Requirements

- Replica credentials are scoped and revocable.
- A replica token cannot perform arbitrary admin operations.
- Write-forward uses normal server RBAC.
- Sync protocol authenticates before revealing schema, LSN, or retained-unit metadata.
- Tests must cover revoked token rejection, expired token rejection, database scoping, operation-class scoping, replica-id spoofing, and no metadata disclosure after failed auth.
- Token rotation and revocation latency must be documented before launch.
- Metrics and logs do not leak query parameter values by default.
- Local data dir permissions remain private by default.
- Docs must warn that local replicas contain a full copy in physical-sync modes.
- Launch docs must include local full-copy risk and platform permission/encryption guidance. Engine-managed encryption can remain a later feature, but the risk cannot be hidden.

## Launch Gates

| Gate | Requirement |
|---|---|
| Correctness | Crash during sync, interrupted apply, duplicate retained units, stale cursor, schema mismatch, corrupt segment, and restart are tested. |
| Performance | Embedded read benchmarks remain within threshold after sync package integration. |
| Recovery | A local replica can be rebuilt from snapshot + retained units. |
| Security | Replica auth, RBAC, TLS, secret scan, and token scope tests pass. |
| Observability | Lag, sync errors, retention pressure, and write-forward latency are visible without debug builds. |
| Documentation | Docs state exact consistency model and unsupported cases. |
| Packaging | `@zvndev/powdb-sync` installs cleanly and does not require a local Rust toolchain on supported platforms. |
| Metrics safety | Sync metrics are private/redacted/cardinality-bounded. |

## Recommended Milestones

### Milestone 0: Sync Substrate Proof

Ship no public API. Prove:

- retained replication unit,
- segment format,
- archive-before-truncate,
- atomic segment publish,
- snapshot identity/fork safety,
- corrupt segment rejection,
- retained-tail apply after bootstrap fixture,
- catalog-level durable LSN for DDL-only snapshots.

### Milestone 1: Embedded Replica Beta

Ship a narrow V1:

- `@zvndev/powdb-sync`
- local embedded readonly queries
- remote primary writes
- manual `syncNow()`
- background pull interval
- `syncStatus()`
- crash-safe full-DB catch-up
- lag metrics
- one deployment guide

This is useful, honest, and small enough to verify.
