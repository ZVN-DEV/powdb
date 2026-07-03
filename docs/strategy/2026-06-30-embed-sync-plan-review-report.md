# Review Report — PowDB Embedded Sync Planning Packet

Date: 2026-06-30
Status: Review synthesis for Kirby

Reviewed artifacts:

- `docs/strategy/2026-06-30-embed-sync-review-packet.md`
- `docs/strategy/2026-06-30-embed-sync-product-plan.md`
- `docs/strategy/2026-06-30-embed-sync-sprint-plan.md`
- `docs/strategy/2026-06-30-embed-sync-test-spec.md`
- `docs/strategy/2026-06-30-continuous-production-review-plan.md`

## Review Outcome

| Lane | Result | Status |
|---|---|---|
| Product strategy | COMMENT | Incorporated into plan packet. |
| Code/spec review, pass 1 | REQUEST CHANGES | Blockers fixed. |
| Backend/database validation | PARTIAL | P0/P1 gates added. |
| Code/spec review, pass 2 | COMMENT | Medium/low comments fixed. |
| Code review, implementation recheck | PASS | No retained-segment blockers after fixes. |
| Code review, metadata recheck | APPROVE | No code/spec/security findings after cursor, stale-lock, identity, segment-listing, permissions, oversized-file, and RNG fixes. |
| Architecture review | WATCH | No planner-boundary blocker. Cross-segment continuity, identity-aware reads, cursor stale-lock recovery, snapshot/backup identity, fork/clone restore semantics, archive-before-truncate, cursor-based retention GC, sync-aware clean shutdown, cold-start bootstrap, and first complete-tail retained apply are fixed in code; crash-safe/chunked apply, retention pressure policy, index DDL propagation, and non-Unix durability scope remain next-gate watch items. |
| Code review, delta recheck | COMMENT | No findings after GC/cursor race and active-transaction checkpoint/backup fixes; formal approval withheld only because LSP diagnostics were unavailable. |
| Architecture review, delta recheck | WATCH | No merge-blocking architectural defect after GC/cursor locking, sync-aware clean shutdown, active-transaction backup/checkpoint, and cold-start bootstrap fixes; public sync API remains gated on retained-tail apply, protocol, observability, and retention pressure policy. |
| Code review, final restore-boundary recheck | COMMENT | No correctness/security findings; two stale documentation comments were fixed. |
| Architecture review, final restore-boundary recheck | CLEAR | Prior BLOCK cleared: default full/chain restore strips sync identity for plain-engine safety, while explicit preserve/fork modes remain for sync-aware callers. |
| Code review, native adapter parity recheck | COMMENT | Prior Buffer-only payload and standalone rustfmt findings fixed; no remaining code findings, with formal approval withheld only because LSP diagnostics were unavailable to the review lane. |
| Architecture review, native adapter parity recheck | CLEAR | Native `databaseId` and retained payload byte forms now match the sync package contract, the non-empty addon smoke is scoped correctly, and docs remain experimental/no-beta-overclaim. |
| Code review, native adapter integration recheck | COMMENT | Initial fallback masking issue fixed; no remaining code/spec/security findings, with formal approval withheld only because LSP diagnostics were unavailable to the review lane. |
| Architecture review, native adapter integration recheck | CLEAR | `test:native` is correctly opt-in, package-resolution-first with repo-local fallback only when the package is absent, and docs keep real server/bootstrap e2e as the beta gate. |
| Backend validation, JS sync/CI recheck | PASS | ACK remote-LSN race is fixed, and current JS sync unit/native/e2e plus TS live sync checks are CI-gated. |
| Architecture review, production-boundary recheck | WATCH | Embedded facade lifecycle, row decoder guards, incremental delta validation, and CLI restore identity modes are fixed. Server-issued pull provenance/nonce, crash/interruption matrix, metrics, stale/rebootstrap repair, and package release locking remain public-beta gates. |

## Major Findings And Fixes

### Product Strategy

Finding: the packet was too architecture-led and did not clearly name the first user, competitor frame, or demo.

Fixes:

- Added `Who V1 Is For`.
- Added `Competitive Frame`.
- Added first demo: Electron inventory/search app with local catalog reads, remote writes, visible lag, and network outage behavior.
- Clarified V1 is not for offline-first collaboration, tenant-filtered sync, or per-user mobile sync.

### Retention Sequencing

Finding: the product roadmap originally placed the embedded replica before the retained history substrate, contradicting the implementation gate.

Fixes:

- Reordered roadmap so **Phase 1: Sync Substrate Proof** precedes **Phase 2: Primary-Authoritative Embedded Replica**.
- Added **Milestone 0: Sync Substrate Proof** and **Milestone 1: Embedded Replica Beta**.
- Made `@zvndev/powdb-sync` explicitly dependent on retained-unit proof.

### Replication Unit Ambiguity

Finding: the packet used "frames" without deciding whether PowDB will retain WAL records, page deltas, or a hybrid.

Fixes:

- Added a P0 gate requiring the retained replication unit to be chosen before implementation.
- Changed pre-design wording from "frames" to "retained units" where appropriate.
- Added golden tests for retained-unit encode/decode/apply across insert/update/delete/DDL/index/transaction boundaries.

### Replica Apply Correctness

Finding: "atomically enough" was not a real apply contract.

Fixes:

- Added visibility rule: local reads during apply must see either the previous applied LSN or a fully applied committed LSN, never a partial statement/DDL/transaction batch.
- Added concurrent-read-during-apply tests over heap, index, and catalog/schema mutations.

### DDL And Schema Evolution

Finding: V1 write-forward could accidentally forward DDL because existing RBAC may permit it.

Fixes:

- V1 `write()` now rejects DDL with a typed error by default.
- DDL propagation requires separate schema propagation and lagging-replica tests.

### Write-Forward Retry Semantics

Finding: if a remote commit succeeds but the client loses the response, naive retry can double-apply non-idempotent writes.

Fixes:

- Added requirement for either idempotency keys or typed `commit_outcome_unknown`.
- Added release blocker against silent double-apply on retry.

### Snapshot And Fork Safety

Finding: snapshot + tail bootstrap needed identity checks beyond source LSN.

Fixes:

- Added database id, primary generation, source LSN, schema/catalog hash, WAL/retained-unit/catalog format versions.
- Added rejection tests for wrong database id, wrong primary generation, stale schema hash, tail gap, wrong snapshot tail, and primary reinit with reused LSN.

### Archive-Before-Truncate Durability

Finding: fsync was not enough; atomic publish ordering and partial segment recovery were missing.

Fixes:

- Added temp write, segment fsync, no-clobber final publish, directory fsync, manifest/cursor fsync, then checkpoint/truncate ordering.
- Added partial/temp segment recovery tests.

### Retention Pressure

Finding: slow replica cursors can become disk-fill or outage vectors.

Fixes:

- Added max retained bytes, inactive cursor expiry, alerting, operator override, and rebootstrap behavior.
- Added GC tests under retention pressure.

### Metrics And Security

Finding: sync metrics could leak sensitive replica/database metadata on an unauthenticated endpoint.

Fixes:

- Sync-capable deployments must expose metrics only through private bind or authenticated surface.
- Metrics must redact/hash identifiers and avoid high-cardinality labels.
- Added tests for no raw database paths, tokens, or query params in metrics/logs.

### Token Scope And Revocation

Finding: token scope was required but not adequately tested.

Fixes:

- Added revoked token, expired token, wrong database scope, wrong operation class, replica-id spoofing, and no metadata disclosure tests.
- Added token rotation/revocation-latency documentation requirement.

### Performance Gates

Finding: performance gates were too vague.

Fixes:

- Added provisional 5% max regression for embedded local reads with sync loaded and idle sync.
- Added owner approval and tracked release blocker/release-note exception requirement for out-of-envelope write-forward/apply results.

### Architecture Review

Finding: the focused architecture pass returned `WATCH`, not `BLOCK`. The plan is coherent on V1 local reads, remote-primary writes, no offline queue, no partial sync, no multi-primary/sharding, no planner changes, and substrate-first sequencing.

Fixes:

- Normalized remaining retained-history wording so pre-design docs do not imply a chosen physical retained-unit format.
- Kept the retained-unit shape as an explicit first substrate design decision: WAL records, page deltas, or a documented hybrid.
- Kept write-forward lost-response semantics, metrics/auth deployment surface, and full-copy replica warnings as first-design watch items.
- Fixed the review's cross-segment continuity blocker in the first code slice: `read_units_since` now rejects missing ranges, overlaps, and filename/header LSN mismatches.

### Code Review Fixes

Finding: first code review found three high-severity retained-segment issues: publish could replace an existing segment, reads could silently skip gaps or duplicate overlaps, and corrupt headers could request implausible allocation before capacity validation.

Fixes:

- Changed segment publish to same-directory temp write, file sync, no-clobber hard-link publish, directory fsync, temp cleanup, and second directory fsync on Unix.
- Added same-range concurrent publish coverage proving one writer succeeds and later writers fail without replacing the segment.
- Added cross-segment continuity validation for missing ranges, overlaps, out-of-order units, and filename/header range mismatch.
- Added a record-count/file-capacity guard before reserving unit storage.
- Re-ran implementation review after the fixes; reviewer returned `APPROVE` with no retained-segment blockers.

## Current Recommendation

Kirby approved moving forward on the packet with two guardrails: do not break existing main PowDB features, and do not delete existing features without explicit approval.

The first implementation slices have advanced with architecture status still
`WATCH` for public beta, not for continued incremental work.

There is no hard architecture blocker in the plan after the
archive-before-truncate gate. The implementation now locks the retained-unit
definition as current WAL records in immutable segment files, and the
experimental JS package now exists as an adapter-based orchestration layer.
Remaining watch items before public beta:

1. broadened JS e2e coverage for stale status, rebootstrap-required repair,
   restart-between-chunks, and DDL policy,
2. server-side idempotency-key support or continued explicit
   `commit_outcome_unknown` behavior,
3. sync metrics/security deployment surface,
4. index/unique DDL propagation or explicit sync-time rejection coverage,
5. non-Unix segment publish durability scope,
6. full-copy replica warning in launch/package docs.

## Implementation Evidence

Completed:

- Added `docs/design/2026-06-30-retained-replication-unit-log-design.md`.
- Added `crates/sync` / `powdb-sync`.
- Added immutable retained-unit segment encode/decode with magic/version/WAL format/catalog format/database id/primary generation/unit count/LSN range/footer CRC.
- Added no-clobber atomic segment publish via temp write, file sync, hard-link publish, directory fsync, temp cleanup, and second directory fsync on Unix.
- Added identity-aware LSN-range reads with cross-segment gap, overlap, out-of-order unit, and filename/header mismatch validation.
- Added corrupt/truncated segment rejection and impossible record-count capacity validation before allocation.
- Added owner-only retained segment directory creation and maximum segment-file-size rejection before allocation.
- Added regression tests for concurrent same-range publish, missing segment gaps, overlapping segments, filename/header mismatch, mixed database id/generation rejection, zero identity rejection, oversized segment-file rejection, owner-only segment-directory creation, and impossible record-count headers.
- Added durable sync identity and primary-side replica cursor metadata with secure OS-random database id generation, no-clobber identity creation, serialized cursor mutation, stale-lock recovery, atomic cursor replacement, active minimum-retained-LSN calculation, and fail-closed corrupt metadata handling.
- Added metadata tests for identity create/reuse, concurrent identity creation, corrupt identity rejection, cursor roundtrip, active minimum-retained-LSN calculation, cursor upsert/retire, concurrent distinct-replica upserts, stale cursor-lock reclamation, invalid cursor rejection, and corrupt cursor file rejection.
- Added optional sync fork-safety metadata to full and incremental backup manifests: database identity, primary generation, source LSN, catalog hash, WAL format, catalog format, and retained-segment format.
- Added restore logic that verifies sync manifest metadata but strips sync identity for default plain-engine restores; explicit preserve restore recreates only `.powdb-sync/identity.json` from manifest metadata, and no restore mode copies mutable sync cursors, locks, retained segments, or arbitrary `.powdb-sync` files.
- Added backup tests for plain-safe default restore, explicit sync identity preservation, legacy manifest compatibility, tampered catalog hash rejection, changed identity rejection during incremental backup, and mixed-history restore-chain rejection.
- Added storage-level WAL archive hooks for checkpoint and recovery without making `powdb-storage` depend on `powdb-sync`.
- Added sync-aware checkpoint/open helpers that archive WAL records into retained segments before checkpoint/recovery truncation, including idempotent same-range retry.
- Added a sync-aware catalog lifecycle owner so `open_preserving_retained_segments` archives later writes on drop, preventing clean-shutdown WAL history from being stranded.
- Added fail-closed protection so plain checkpoint/recovery refuses to truncate non-empty WAL history for sync-enabled databases when no archive hook is supplied.
- Added an explicit plain-open regression test proving sync-enabled WAL history is not truncated when recovery is attempted without an archive hook.
- Added cursor-based retained segment GC that deletes only segments fully below the active replica retention boundary, keeps boundary-crossing segments intact, validates the retained tail before deletion, treats no active cursor as a no-op, and blocks deletion on identity/range/corruption gaps.
- Serialized retention GC with cursor metadata publication and rejected active cursor registration when retained history is already missing, so concurrent GC cannot strand a newly active lagging replica.
- Added retained-tail availability validation that rejects gaps, overlaps, filename/header mismatches, identity mismatches, and missing required tail LSNs without materializing the full tail.
- Added backup-based cold-start bootstrap that consumes sync backup metadata, archives the primary's live WAL tail, verifies retained-tail continuity through the current primary LSN, restores the snapshot into an empty replica path, and registers the primary-side cursor under the cursor metadata lock.
- Added active explicit-transaction guards to checkpoint/checkpoint-with-archive and drop cleanup that abandons active transaction dirty heap state instead of flushing it.
- Added full/incremental backup regressions proving backup fails closed during active transactions and does not persist uncommitted rows.
- Updated full and incremental backup to use the sync-preserving checkpoint path when sync identity exists.
- Added `docs/embedded-sync.md` as the user-facing V1 contract page for embedded-replica semantics without claiming the public JS sync package exists yet.
- Corrected the embedded Node README's PowQL example so it uses execution-order syntax and dotted field references.
- Added `powdb-sync::read_units_through` and routed server `SyncPull` through it so private pull responses cannot serve retained units beyond the server-computed authoritative `catalog.max_lsn()`, even if retained segments contain later records.
- Added `sync_pull_never_serves_units_beyond_server_remote_lsn` regression coverage for that invariant.
- Added `powdb-sync::retained_tail_progress`, `servableLsn`, `unarchivedLsn`, and `awaitArchive` repair state so status distinguishes primary progress from currently ship-ready retained history.
- Added `sync_status_reports_await_archive_when_primary_outruns_retained_tail` and `sync_pull_serves_partial_retained_prefix_when_archive_lags_remote_lsn` coverage; `hasMore` now means another retained chunk is immediately fetchable rather than merely that primary history exists past the response.
- Routed private sync status/pull/ack frames through the server transaction gate and reject sync frames from the transaction-owning connection. `sync_frames_respect_open_transaction_gate` verifies same-connection sync frames fail during an active transaction, another connection waits, and rollback resumes sync status afterward.
- Added sync-aware rollback plumbing so committed pre-transaction WAL is archived before rollback recovery truncates it in sync-enabled data directories.

Current-cycle update, 2026-07-01:

- Moved sync-aware lifecycle selection to the engine/server boundary for this slice: `Engine::new_with_wal_archive` and `Engine::with_memory_limit_and_wal_archive` carry the archive hook through open, drop checkpoint, and rollback; server startup now uses the archive-aware constructor.
- Moved the core PowQL rollback plan branch through `rollback_transaction_preserving_wal_archive`, so archive-aware engines no longer depend on a server-only rollback wrapper.
- Added a rollback regression proving an archive-hook failure leaves the transaction retryable instead of clearing engine transaction state prematurely.
- Added server pull/ack transaction-boundary validation: `SyncPull` fails clearly rather than returning a V1-unapplyable chunk that cuts before commit/rollback, and `SyncAck` validates the acknowledged retained range before cursor advance.
- Fixed transaction-boundary validation and storage replay to treat transactions as ordered WAL spans instead of global transaction-id membership, so tx-id reuse across reopen cannot make a later incomplete transaction look committed.
- Added direct named-user TCP sync auth coverage and a direct byte-budget transaction-cut pull regression.
- Added TCP lifecycle coverage proving a sync-aware engine archives writes on drop and reopens successfully with retained units preserved.
- Verification passed: `cargo fmt --all --check`, `git diff --check`, targeted rollback/sync regressions, `cargo test -p powdb-sync`, `cargo test -p powdb-query`, `cargo test -p powdb-storage --test wal_recovery -- --nocapture`, `cargo test -p powdb-server`, touched-crate clippy with `-D warnings`, and `cargo test --workspace`.

Current-cycle package update, 2026-07-01:

- Added `clients/sync` as the experimental `@zvndev/powdb-sync` package.
- Added `PowDBSyncReplica` with local readonly delegation, explicit
  pull/apply/ack orchestration, contiguous retained-unit LSN validation,
  stale/await-archive/rebootstrap handling, deferred or immediate sync after
  remote writes, V1 DDL rejection, and typed remote write failure outcomes.
- Kept the package dependency-light through structural adapter interfaces for
  `remote` and `local`, so the sync package does not import the native addon at
  runtime. Added constructor-time capability checks so adapters missing
  `query`, `syncStatus`, `syncPull`, `syncAck`, `queryReadonly`, or
  `applyRetainedUnits` fail before sync starts. The native embedded
  `applyRetainedUnits` method now satisfies the local adapter side.
- Removed the public no-op `idempotencyKey` option from `WriteOptions`; until
  the TS client/server transport can actually carry an idempotency key,
  ambiguous write failures return `commit_outcome_unknown`.
- Tightened ack-stage correctness after independent review: `syncNow()` now
  validates ack LSNs and ack status before reporting success, preserves
  local-apply context when ack fails after local replay, and `write()` exposes
  `localVisibility`, `syncAppliedLsn`, and `syncRemoteLsn` so callers can
  distinguish guaranteed local read-your-write from applied-but-unacked partial
  sync.
- Added package-local tests for readonly delegation, pull/apply/ack ordering,
  adapter capability validation, ack mismatch rejection after local apply,
  non-contiguous pull rejection before local apply/ack, await-archive status,
  rebootstrap-required status, DDL rejection, remote unavailable mapping,
  unknown commit outcome, deferred sync, applied-but-unacked write visibility,
  and successful write with sync failure.
- Added package-local pnpm lock/config so optional peer packages are not fetched
  during dev install while `@zvndev/powdb-client` and
  `@zvndev/powdb-embedded` remain version-locked peers.
- Updated `docs/embedded-sync.md` and `clients/sync/README.md` to state the
  current experimental package status and remaining e2e gates.
- Verification passed: `cd clients/sync && pnpm run build`, `cd clients/sync &&
  pnpm test` (13 tests), `git diff --check`, and `cd clients/sync && npm pack
  --dry-run` (publish contents limited to dist, README, license, changelog, and
  package metadata).

Current-cycle native binding update, 2026-07-01:

- Added a narrow embedded facade method, `Database::apply_retained_units(...)`,
  that validates retained segment format version, translates retained units,
  and calls `powdb-sync::apply_retained_units_chunk(...)` through the existing
  panic-safe embedded handle boundary.
- Added `@zvndev/powdb-embedded` `Database.applyRetainedUnits(...)`, with
  BigInt u64 validation, u16/u8 width checks, 32-hex-character or 16-byte
  `Uint8Array` database id validation, `Uint8Array` or `Buffer` retained
  payload conversion into the Rust applier.
- Generated napi declarations confirm the JS surface matches the sync
  orchestration package contract: `applyRetainedUnits`, `sinceLsn`,
  `databaseId: string | Uint8Array`, `primaryGeneration`, `walFormatVersion`,
  `catalogVersion`, `segmentFormatVersion`, `txId`, `recordType`, `lsn`, and
  `data: Uint8Array | Buffer`, and `unitsApplied`.
- Added facade tests for seeded-boundary no-op apply and wrong retained segment
  format rejection.
- Added Node addon tests proving the native binding is callable through the
  real module, accepts byte-form database identity, applies a non-empty
  retained-unit chunk with a plain `Uint8Array` payload, and rejects malformed
  database identity.
- Updated `bindings/node/README.md`, `clients/sync/README.md`, and
  `docs/embedded-sync.md` so the native binding is no longer described as a
  future adapter boundary.
- Verification passed: `cargo fmt --all --check`, `cd bindings/node && npm run
  build`, generated napi declaration inspection, `cd bindings/node && npm
  test`, `cargo test -p powdb apply_retained_units -- --nocapture`, `cd
  clients/sync && pnpm run build && pnpm test`, `cargo clippy -p powdb
  --all-targets -- -D warnings`, `cd bindings/node && cargo clippy
  --all-targets -- -D warnings`, `git diff --check`, and package dry-runs for
  `clients/sync` and `bindings/node`.

Current-cycle native adapter integration update, 2026-07-01:

- Added `clients/sync/test/native-adapter.test.ts`, a JS integration test that
  runs `PowDBSyncReplica.write(...)` through a deterministic fake primary and
  the real `@zvndev/powdb-embedded` local adapter. The test seeds the trusted
  retained-apply boundary, uses byte-form `databaseId`, pulls a non-empty
  retained commit unit with a plain `Uint8Array` payload, applies locally
  through `Database.applyRetainedUnits(...)`, acknowledges after local apply,
  and verifies `.powdb-sync/apply-state.json` advances to LSN 1.
- Added `clients/sync` `test:native` so the native adapter proof can run
  explicitly after the embedded addon is built, without making default
  structural package tests depend on native build artifacts.
- The native adapter test resolves `POWDB_SYNC_NATIVE_EMBEDDED_ENTRY` first,
  then an installed `@zvndev/powdb-embedded`, and keeps the repo-local
  `bindings/node` loader as a monorepo fallback only when the package is not
  installed. A broken installed package fails the test instead of silently using
  the fallback.
- Updated `clients/sync/README.md` and `docs/embedded-sync.md` so the new
  native-adapter integration coverage is documented without overstating public
  beta readiness. Real server/client/bootstrap e2e was still the next gate at
  that point; the later 2026-07-01 full e2e update below covers it.
- Verification passed: `cd clients/sync && pnpm run build`, `cd clients/sync
  && pnpm test`, and `cd clients/sync && pnpm run test:native`.

Current-cycle live server/client and scheduler update, 2026-07-01:

- Added `clients/ts/test/sync-live.test.ts` plus the explicit
  `clients/ts` `test:sync-live` script. The test starts a real
  password-authenticated `powdb-server`, creates schema, writes
  `.powdb-sync` identity/cursor metadata, captures a bootstrap baseline LSN
  through `Client.syncStatus(...)`, performs a post-bootstrap write, gracefully
  restarts the server so the sync-aware shutdown archives retained WAL units,
  then verifies `syncStatus`, `syncPull`, and `syncAck` over the real wire
  protocol.
- Added `PowDBSyncReplica.startBackgroundSync(...)` to the experimental
  `@zvndev/powdb-sync` package. The scheduler wraps explicit `syncNow()` calls,
  prevents overlapping sync runs, supports immediate or interval operation,
  supports stop/abort, and reports result/error callbacks without silently
  queueing writes.
- Added scheduler unit coverage for immediate start, repeated interval sync,
  and stop-on-error behavior.
- Verification passed: `cargo build --release -p powdb-server`,
  `cargo test -p powdb-server --test sync_protocol`, `cd clients/ts && pnpm run
  build`, `cd clients/ts && pnpm run test:sync-live`, and `cd clients/sync &&
  pnpm run build && pnpm test`.

Current-cycle CLI bootstrap and full JS e2e update, 2026-07-01:

- Added offline/admin CLI sync bridge commands: `powdb-cli sync-enable` creates a
  durable sync identity plus retained checkpoint, and `powdb-cli sync-bootstrap
  <BKP> <REPLICA_DIR> <REPLICA_ID>` restores a sync-enabled full backup into a
  replica and publishes the primary-side cursor.
- Changed CLI embedded execution and backup entry points to use the sync-aware
  WAL archive lifecycle when a sync identity exists. This fixes the production
  edge where a sync-enabled data dir with pending WAL could fail plain recovery
  during CLI backup instead of archiving retained history before truncation.
- Added `clients/sync/test/bootstrap-e2e.test.ts` plus the `test:e2e` script.
  The test creates a primary, enables sync, takes a full backup, performs a
  real remote write through `powdb-server` + `@zvndev/powdb-client`, bootstraps a
  native local replica from the backup, verifies the local snapshot, then pulls,
  applies, and acknowledges retained units through `PowDBSyncReplica` until the
  local replica sees the post-snapshot row.
- Added CLI regression coverage proving `powdb-cli backup` replays and archives
  pending sync WAL before backup recovery truncates it.
- Verification passed: `cargo test -p powdb-cli --test backup_cli`,
  `cargo test -p powdb-backup --test sync_bootstrap --test sync_apply`,
  `cargo test -p powdb-server --test sync_protocol`, `cargo test -p powdb-sync`,
  `cd clients/ts && pnpm run build && pnpm run test:sync-live`, `cd
  bindings/node && npm run build && npm test`, `cd clients/sync && pnpm run
  build && pnpm test && pnpm run test:native && pnpm run test:e2e`, and `cargo
  clippy -p powdb-cli --all-targets -- -D warnings`.

Current-cycle production-boundary hardening update, 2026-07-01:

- Fixed the JS sync ACK remote-LSN race: `PowDBSyncReplica.syncNow()` now
  accepts an ACK whose returned primary LSN has advanced after pull, while still
  rejecting ACK results behind the requested remote LSN or status behind the
  acknowledged LSN.
- Added CI coverage for the current embedded-sync JS vertical slice:
  `ts-client` runs the live server-backed sync test, and the new
  `embedded-sync-js` job builds release `powdb-cli`/`powdb-server`, builds the
  native addon, and runs `@zvndev/powdb-sync` build/unit/native/e2e checks.
- Moved the public Rust embedded facade to the sync-aware WAL archive lifecycle:
  `Database::open` and `Database::open_with_memory_limit` now preserve pending
  sync WAL before recovery truncation when sync identity exists.
- Hardened the TS row decoder so impossible row/column counts fail before large
  allocation, including nonzero rows with zero columns and payloads too short to
  contain the declared row shape.
- Hardened incremental restore so embedded delta page indexes must match the
  manifest page indexes and cannot write outside the restored file page count.
- Added operator-facing CLI restore identity modes:
  `powdb-cli restore --sync-preserve` keeps the source identity for disaster
  recovery, `--sync-fork` mints a fresh identity for clone/fork restores, and
  default restore still strips sync identity for plain-engine safety.
- Addressed the follow-up code-review parser findings: restore sync flags now
  work before or after restore positionals, conflicting restore sync flags fail
  with a usage error, restore-only flags are rejected on backup, count
  assertions use exact scalar output, and chain restore is covered for preserve
  and fork modes.
- Added the first operator visibility command: `powdb-cli sync-status
  [REPLICA_ID]` opens the primary data dir through the sync-aware lifecycle,
  computes `remoteLsn`, and prints registered replica cursor status, servable
  retained history, lag, stale state, and recommended repair action. Regression
  coverage proves it reports `pull` for a bootstrapped lagging replica, lists
  registered cursors, reports `rebootstrap` for a missing cursor, and fails
  clearly before `sync-enable`.
- Architecture recheck status: `WATCH`, not `BLOCK`. `sync-status` is aligned
  as an offline primary-side maintenance command, but it is not a passive
  read-only probe because sync-aware open/drop can archive/checkpoint retained
  WAL. Its `repairAction` is an operator hint, not proof that a future
  pull/apply will succeed; wire-protocol version/identity checks and local
  apply validation still own that proof.
- Verification passed: `cargo test -p powdb-cli --test backup_cli`,
  `cargo clippy -p powdb-cli --all-targets -- -D warnings`,
  `cargo test -p powdb-backup --test incremental`, `cargo test -p powdb-sync`,
  `cargo test -p powdb-server --test sync_protocol`,
  `cargo test -p powdb open_archives_pending_sync_wal_before_recovery_truncates`,
  direct `tsc`/`tsx` script-body checks for `clients/ts` protocol and live sync,
  direct `tsc`/`tsx` script-body checks for `clients/sync` unit/native/e2e, the
  `bindings/node` native build plus `node --test`, touched-crate clippy for
  `powdb`/`powdb-backup`, release CLI/server build, and `git diff --check`.

Verification:

- `cargo fmt --all`
- `cargo test -p powdb-sync` (36 unit tests, 4 integration tests)
- `cargo test -p powdb-sync --test retention_gc`
- `cargo test -p powdb-backup --test sync_bootstrap`
- `cargo test -p powdb-storage --test wal_recovery`
- `cargo test -p powdb-backup --test backup_roundtrip --test incremental`
- `cargo clippy -p powdb-sync --all-targets -- -D warnings`
- `cargo clippy -p powdb-storage -p powdb-sync -p powdb-backup --all-targets -- -D warnings`
- `cargo test -p powdb-storage --test wal_crc --test wal_recovery`
- `cargo test -p powdb-backup`
- `cargo test -p powdb --test embedded`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `git diff --check`
- Final code-review delta recheck: `COMMENT`; no findings, with formal approval withheld only because LSP diagnostics were unavailable to the review lane.
- Final code-review restore-boundary recheck: `COMMENT`; no correctness/security findings, and stale restore/docs wording was fixed.
- Final architecture restore-boundary recheck: `CLEAR`; default full/chain restore now strips sync identity for plain-engine safety, explicit `PreserveSyncIdentity` remains available for sync-aware disaster recovery/bootstrap, and tests prove write/drop/reopen works through default restore.

## Decisions Still Needed From Kirby

- Confirm DDL stays rejected through V1 `write()` until schema propagation tests
  exist.
- Confirm local read-your-writes requires completed sync/pull.
- Confirm whether server-side idempotency-key transport is the next
  write-forward slice, or whether V1 should ship with only explicit
  `commit_outcome_unknown` for ambiguous non-idempotent writes.

## Next Implementation Gate

Before public embedded-sync beta:

1. broaden the new JS backup-bootstrap/native e2e to cover restart-between-
   chunks, DDL policy, rebootstrap-required repair flows, and crash/interrupted-
   apply recovery,
2. add server-side idempotency-key transport if V1 should support safe retry
   instead of only returning `commit_outcome_unknown`,
3. add crash-injection around segment publish, checkpoint, recovery, cursor
   update, bootstrap repair, and chunk apply,
4. add missing-segment repair-path tests,
5. add metrics/logs for `pull`, `awaitArchive`, and `rebootstrap` states,
6. run code-review and architecture review again before publishing the package.
