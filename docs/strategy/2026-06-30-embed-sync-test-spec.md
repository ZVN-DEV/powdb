# Test Spec — PowDB Embedded Sync

Date: 2026-06-30
Status: Draft for review
Review packet: `docs/strategy/2026-06-30-embed-sync-review-packet.md`
Review report: `docs/strategy/2026-06-30-embed-sync-plan-review-report.md`
Companion plans:

- `docs/strategy/2026-06-30-embed-sync-product-plan.md`
- `docs/strategy/2026-06-30-embed-sync-sprint-plan.md`
- `docs/strategy/2026-06-30-continuous-production-review-plan.md`

## Purpose

This spec defines the proof required before PowDB Embedded Sync can be called production-ready.

The first implementation target is **primary-authoritative embedded replicas**:

- local embedded reads,
- remote primary writes,
- primary-to-local sync-down,
- observable lag,
- no offline local write queue in V1.

## Acceptance Criteria

### A1: Existing Embedded Mode Is Preserved

Evidence required:

- `@zvndev/powdb-embedded` can open a local DB, create schema, insert, query, query SQL, set sync mode, close, and reopen.
- Result shapes match `@zvndev/powdb-client`.
- Embedded local read latency does not regress beyond the numeric thresholds in `Performance Gates`.
- `setSyncMode("normal")` remains documented as bounded-loss, not fully durable.
- Baseline source and allowed regression are recorded before accepting performance claims.

Minimum tests:

- Node addon E2E open/query/querySql/reopen.
- Rust facade test for open/query/reopen.
- Result-shape parity test between embedded and server client.
- Benchmark fixture for local read and write by WAL mode.

### A2: Sync Contract Is Explicit

Evidence required:

- Docs define V1 consistency: local reads may be stale; writes go to the primary; sync pulls primary state into the local replica.
- Docs define behavior for remote write outage, pull outage, stale cursor, corrupt segment, schema mismatch, and auth failure.
- Docs state unsupported V1 features: offline local writes, partial sync, multi-primary, automatic sharding, Postgres wire compatibility.

Minimum tests:

- Documentation review checklist.
- Product-review gate verifying no unsupported claim appears in README, package README, or launch copy.

### A3: Retained Replication-Unit Log Is Durable

Evidence required:

- The retained replication unit is explicitly defined before implementation. The design must choose WAL-record segments, page-delta segments, or a documented hybrid; do not use vague "frames" once code begins.
- Checkpoint cannot truncate retained units still needed by backup, PITR, or sync.
- Retained segments have magic, version, checksum, start LSN, end LSN, and unit count.
- Segment publish order is crash-safe: temp write, segment fsync, no-clobber final publish, directory fsync, manifest/cursor fsync, then checkpoint/truncate.
- GC is based on the minimum required cursor and cannot delete active history.
- Retention pressure behavior is defined: max retained bytes, inactive cursor expiry, alerting, operator override, and rebootstrap path.

Minimum tests:

- replication-unit golden tests for insert, update, delete, DDL create/drop/add/drop column, index/unique changes, and transaction boundaries,
- Append retained units, checkpoint, reopen, pull retained units.
- Crash between retained segment write and checkpoint.
- Crash after checkpoint but before cursor metadata update.
- GC with two replicas, one lagging.
- GC under max-retention pressure and inactive cursor expiry.
- Corrupt segment checksum fails closed.
- Missing segment returns a repairable error.
- Partial/temp segment after crash is ignored or repaired deterministically.

Current evidence:

- `crates/sync/src/segment.rs` implements retained-unit segment magic/version/checksum, WAL/catalog format metadata, non-zero database id, non-zero primary generation, unit count, start/end LSN, contiguous LSN validation, identity-aware range reads, no-clobber atomic publish, owner-only segment-directory creation, oversized segment-file rejection before allocation, corrupt checksum rejection, truncated segment rejection, filename/header range validation, and record-count capacity validation before allocation.
- `crates/sync/src/metadata.rs` implements durable sync identity and primary-side retention cursor metadata: versioned identity, secure OS-random database id generation, primary generation, versioned replica cursors, active-cursor minimum retained LSN, serialized cursor mutation with stale-lock recovery, atomic cursor replacement, no-clobber identity creation, duplicate/invalid replica id rejection, and corrupt metadata fail-closed behavior.
- `cargo test -p powdb-sync` verifies segment roundtrip, empty/non-contiguous rejection, corrupt footer checksum rejection, truncated segment rejection, atomic publish/range read, retained segment directory permissions, oversized segment-file rejection, range-read limit handling, concurrent same-range no-clobber publish, missing segment gaps, overlapping segments, filename/header range mismatch, mixed database id/generation rejection, zero identity rejection, and impossible record-count headers.
- `cargo test -p powdb-sync` verifies identity create/reuse, concurrent identity creation, corrupt identity rejection, cursor roundtrip, active minimum-retained-LSN calculation, cursor upsert/retire, concurrent distinct-replica upserts, stale cursor-lock reclamation, duplicate/bad/overflowing cursor rejection, and corrupt cursor file rejection.
- `crates/backup/src/manifest.rs` adds optional sync fork-safety metadata to full and incremental backup manifests: database identity, primary generation, source LSN, catalog hash, WAL format, catalog format, and retained-segment format.
- `cargo test -p powdb-backup` verifies sync backups record fork-safety metadata, default full and chain restore strip sync identity while remaining writable/reopenable through the plain engine lifecycle, explicit preserve restore keeps same-lineage sync identity, explicit fork restore mints a new sync identity, legacy manifests without sync metadata still restore without creating sync state, mutable `.powdb-sync` state is not copied wholesale, tampered sync catalog hashes are rejected, incremental backup rejects changed sync identity, and restore-chain rejects mixed identity or stale catalog hash.
- `crates/storage/src/catalog.rs` exposes WAL archive hooks for checkpoint and recovery without depending on `powdb-sync`; plain checkpoint/recovery refuses to truncate non-empty WAL history for sync-enabled databases unless an archive hook is supplied.
- `crates/sync/src/checkpoint.rs` implements sync-aware checkpoint/open helpers that archive WAL records into retained segments before checkpoint/recovery truncation, with idempotent same-range retry after an archive-before-truncate interruption.
- `cargo test -p powdb-sync` verifies sync checkpoint archives WAL before truncate, plain checkpoint and plain open fail closed for sync-enabled WAL history without an archive hook, and sync-aware open archives replayed WAL before recovery truncates it.
- `crates/sync/src/checkpoint.rs` returns a sync-aware lifecycle owner from `open_preserving_retained_segments`; its drop path archives later writes before plain `Catalog` drop runs, preventing clean-shutdown WAL history from being stranded.
- `crates/sync/src/segment.rs` exposes retained-tail availability validation that rejects gaps, overlaps, filename/header mismatches, identity mismatches, and missing required tail LSNs without materializing the full tail.
- `crates/sync/src/retention.rs` implements cursor-based retained segment GC: active replica cursors set the retention boundary, retired cursors are ignored, segments crossing the boundary are kept intact, no active cursor is a conservative no-op, GC shares the cursor metadata lock with cursor publication, and the retained tail is validated for identity, filename/header range, gaps, and overlaps before deletion.
- `crates/sync/src/metadata.rs` rejects active cursor publication when sync identity and retained segments exist but the requested next LSN is already below retained history, returning a rebootstrap-required error instead of creating a stranded active cursor.
- `crates/sync/src/retention.rs` also exposes explicit retention pressure policy: max retained bytes are reported without stranding active cursors, inactive cursors can be retired by age, and an operator retain-LSN override retires lagging cursors so they must rebootstrap.
- `cargo test -p powdb-sync` and `cargo test -p powdb-sync --test retention_gc` verify lagging active replicas are protected, retired cursors release older segments, stale cursor publication after GC is rejected, concurrent GC plus lagging cursor publication cannot strand an active cursor, corrupt or mismatched segment sets block GC before deletion, invalid expected identity is rejected, boundary-crossing segments are preserved, byte pressure is reported without deletion past active cursors, inactive cursor expiry releases history, and operator override forces lagging cursors to rebootstrap.
- `crates/backup/src/bootstrap.rs` implements backup-based cold-start bootstrap: it requires sync backup metadata, takes a live primary `Catalog`, archives uncheckpointed primary WAL records before computing the remote LSN, validates primary identity and retained-tail continuity, restores the full backup into an empty replica path, and registers the primary-side cursor under the cursor metadata lock.
- `crates/backup/src/bootstrap.rs` validates V1 applyability before restore/cursor publication, so unsupported DDL tails are rejected before a replica path is created. If cursor publication fails after restore, it removes the restored replica directory.
- `cargo test -p powdb-backup --test sync_bootstrap` verifies bootstrap succeeds with post-snapshot writes that have not been explicitly checkpointed, rejects legacy non-sync backups, rejects missing retained tails without publishing a cursor, rejects unsupported DDL tails before restore/cursor publication, rejects primary identity mismatch, rejects an already-active replica cursor without clobbering it, and cleans up the restored directory if cursor publication fails after restore.
- `crates/sync/src/apply.rs` implements complete-tail retained-unit apply for same-lineage replicas by converting retained WAL-record units back into LSN-preserving storage WAL records.
- `cargo test -p powdb-backup --test sync_apply` verifies snapshot plus retained-tail apply converges post-snapshot insert/update/delete rows, keeps index-backed lookup consistent, persists applied rows across reopen, and treats duplicate apply as a no-op after the target LSN is reached.
- `crates/sync/src/apply.rs` persists local apply-state before replay and marks it complete after replay; retry replays from the recorded safe apply watermark only when the recorded database identity/range match and the catalog still equals that watermark. If replay reached the target LSN but the complete marker was not written, retry marks complete; if catalog LSN advanced only partway, retry fails closed. `catalog.max_lsn()` is only a consistency check, not proof of a contiguous applied prefix.
- `crates/sync/src/apply.rs` validates V1 applyability before replay: unsupported DDL records are rejected, and explicit transaction ranges must include a commit or rollback boundary before any row record for that transaction can be applied.
- `cargo test -p powdb-sync apply::tests::in_progress_apply_state_replays_from_recorded_safe_lsn_when_catalog_matches` verifies retry can replay safely while the catalog still matches the watermark, `cargo test -p powdb-sync apply::tests::in_progress_apply_state_marks_complete_when_catalog_reached_target` verifies the crash window after storage replay but before the complete marker, `cargo test -p powdb-sync apply::tests::in_progress_apply_state_fails_closed_when_catalog_lsn_advanced` verifies partial advanced catalog state without completion is rejected, `cargo test -p powdb-sync apply::tests::v1_applyability_rejects_transaction_split_before_commit` verifies split transaction ranges are rejected before storage replay, `cargo test -p powdb-sync apply::tests::v1_applyability_rejects_reused_tx_id_with_later_incomplete_span` verifies tx-id reuse across retained spans cannot make a later incomplete transaction look closed, and `cargo test -p powdb-sync apply::tests::different_in_progress_apply_range_fails_closed` verifies mismatched in-progress state is rejected.
- `crates/sync/src/replica.rs` implements the primary-side cursor acknowledgement/status substrate for the future pull protocol. `acknowledge_replica_apply` advances active cursors monotonically after successful apply; `replica_sync_status` reports `lastAppliedLsn`, `remoteLsn`, `servableLsn`, `unarchivedLsn`, `lagBytes`, `lagMs`, `stale`, `lastSyncError`, and repair action. These are progress indicators, not authorization or contiguous-apply proofs. `remoteLsn` is primary progress, `servableLsn` is the largest contiguous retained-unit LSN currently ship-ready for that cursor, `unarchivedLsn` is the not-yet-retained gap, `lagBytes` is a retained-segment overlap estimate, and `lagMs` is time since the primary-side cursor was last acknowledged while stale.
- `cargo test -p powdb-sync replica::tests` verifies acknowledgements advance cursors and clear lag, stale status recommends pull for a contiguous retained prefix, stale status reports `awaitArchive` when primary progress outruns retained segments, missing retained history recommends rebootstrap, stale/inactive acknowledgements fail closed, and status-generation errors leave the previous cursor intact.
- `crates/server/src/protocol.rs` adds private append-only sync frame tags for `SyncStatus`, `SyncPull`, and `SyncAck`, plus typed status/pull/ack results. `SyncPull` carries `replicaId`, `sinceLsn`, `maxUnits`, `maxBytes`, database id, primary generation, WAL/catalog format versions, and retained-segment format version.
- `crates/server/src/handler.rs` exposes those sync frames only after credentialed `CONNECT`; open/no-auth connections and named `readonly` users cannot read sync metadata or mutate cursors. The server computes the authoritative remote LSN from `Engine::catalog().max_lsn()`, validates pull cursor and identity/format bindings, validates retained-tail continuity through the requested chunk, caps served retained units at the currently servable retained LSN and the authoritative remote LSN even if retained segments contain later records, bounds unit/byte output, trims or rejects V1-unapplyable chunks that would cut through explicit transactions, and validates acknowledgement ranges before advancing primary-side cursors.
- `crates/sync/src/segment.rs` exposes `read_units_through` for bounded retained-unit range reads, so protocol callers can request units after a cursor through a server-authoritative target LSN instead of filtering an uncapped retained tail after the fact.
- `crates/sync/src/segment.rs` also exposes `retained_tail_progress`, allowing status generation to report a contiguous retained prefix without falsely treating not-yet-archived primary WAL as missing history.
- `cargo test -p powdb-server sync -- --nocapture` verifies protocol round trips, sync auth/readonly rejection, server-computed status, retained-tail pull, ack clearing stale state, and cursor/format mismatch rejection.
- `cargo test -p powdb-server sync_status_reports_await_archive_when_primary_outruns_retained_tail -- --nocapture` verifies primary progress ahead of retained segments returns `awaitArchive` rather than `rebootstrap`.
- `cargo test -p powdb-server sync_pull_serves_partial_retained_prefix_when_archive_lags_remote_lsn -- --nocapture` verifies pull returns the current contiguous retained prefix, reports the unarchived primary gap, and keeps `hasMore` false once the client has consumed the currently fetchable retained prefix.
- `cargo test -p powdb-server sync_pull_never_serves_units_beyond_server_remote_lsn -- --nocapture` verifies a retained segment containing records past `catalog.max_lsn()` does not leak future units through `SyncPull`.
- `cargo test -p powdb-server sync_pull_and_ack_reject_transaction_cut_boundaries -- --nocapture` verifies `SyncPull` does not return a retained range ending inside an explicit transaction and `SyncAck` does not advance a cursor to a transaction-cut LSN.
- `cargo test -p powdb-server --test sync_protocol -- --nocapture` verifies the real TCP path: an open server rejects sync metadata access, named `readwrite`/`admin` users can use sync frames while named `readonly` users are denied, sync-aware engine drop archives later writes before reopen, sync frames are unavailable on a connection with an active explicit transaction, another connection's sync status waits behind the same transaction gate as normal queries, and an authenticated connection can run `SyncStatus -> SyncPull -> SyncAck` after primary writes and clear stale status.
- `cargo test -p powdb-storage replay_records_treats_reused_tx_ids_as_ordered_spans -- --nocapture` verifies storage WAL replay does not treat reused transaction ids as one global transaction membership set.
- `cargo test -p powdb-query rollback_wal_archive_failure_keeps_transaction_retryable -- --nocapture` verifies an archive-hook failure during rollback leaves the transaction retryable instead of clearing engine transaction state prematurely.
- `crates/storage/src/catalog.rs` persists `catalog.lsn` so DDL-only snapshots and recovery-skipped WAL records advance the durable catalog high-water mark even when no heap page LSN changes.
- `cargo test -p powdb-backup --test backup_roundtrip full_backup_records_ddl_only_lsn` verifies DDL-only full backups record a nonzero source LSN and restore the sidecar.
- `crates/storage/src/catalog.rs` refuses checkpoint and checkpoint-with-archive while an explicit transaction is active, and drop abandons active transaction dirty heap state instead of flushing it.
- `cargo test -p powdb-storage --test wal_recovery` verifies checkpoint refusal during an active transaction and proves drop/reopen do not persist the uncommitted rows.
- `cargo test -p powdb-backup --test backup_roundtrip --test incremental` verifies full and incremental backups fail closed during active transactions without leaking uncommitted rows.
- `cargo test --workspace` verifies the additive crate without breaking current engine behavior.

Still pending:

- Broader stale/rebootstrap repair e2e coverage after the first full JS
  backup-bootstrap/native local-apply flow.
- Apply-level golden fixtures for index/unique DDL, multi-chunk transaction
  state, and broader repair paths.

### A4: Bootstrap Uses Snapshot Plus Retained Tail

Evidence required:

- A new replica can restore a sync snapshot and prove retained-tail availability through the primary LSN.
- A new replica can apply that retained tail and converge to the primary rows.
- A far-behind replica can be repaired by rebootstrap.
- Snapshot manifest format is compatible with backup/restore concepts instead of a separate parallel format.
- Snapshot + tail apply is fork-safe: database id, primary generation, source LSN, schema/catalog hash, WAL/retained-unit/catalog versions must match.

Minimum tests:

- Full cold-start restore from empty local path.
- Bootstrap with writes occurring after snapshot but before first pull.
- Retained-tail apply after cold-start restore.
- Restore local replica from snapshot + retained units and verify row equality.
- Reject newer unsupported catalog/WAL/retained-unit versions.
- Reject wrong database id, wrong primary generation, stale schema hash, tail gap, tail from another snapshot, and primary reinitialized with reused LSN.

### A5: Server Sync Protocol Is Authenticated And Resumable

Evidence required:

- Sync handshake authenticates before revealing schema, LSN, or retained-unit metadata.
- Pull requests resume from a cursor.
- Pull batches are idempotent under retry.
- Stale cursor returns a clear rebootstrap-required response.
- Schema mismatch returns a clear incompatible-version response.

Minimum tests:

- unauthenticated sync handshake rejected,
- current private sync frames reject named `readonly` users; future scoped replica tokens must get separate pull-vs-admin coverage before public exposure,
- write-forward respects normal RBAC,
- pull since cursor returns expected retained units,
- duplicate pull/apply is idempotent,
- stale cursor triggers repair path.
- revoked token rejected,
- expired token rejected,
- token scoped to wrong database rejected without metadata disclosure,
- token lacking requested operation class rejected,
- replica-id spoofing rejected or harmless.

### A6: Replica Apply Is Crash-Safe

Evidence required:

- Applying retained units cannot corrupt the local DB if the process dies.
- Local sync state is persisted independently of volatile process memory.
- A duplicate retained unit is harmless.
- A partial retained-unit range either replays from a recorded safe watermark while the catalog still matches that watermark or fails closed.
- Local reads during apply see either the previous applied LSN or a fully applied committed LSN, never a partial batch or heap/index/schema mismatch.

Minimum tests:

- crash before apply begins,
- crash after some retained units applied but before cursor update, covered for fail-closed retry by `apply::tests::in_progress_apply_state_fails_closed_when_catalog_lsn_advanced`,
- crash after cursor update,
- duplicate retained-unit replay,
- corrupt local sync state,
- reopen and local read after interrupted apply.
- continuous local readonly queries while apply mutates heap, indexes, and catalog/schema.

### A7: Write-Forward Is Honest

Evidence required:

- V1 writes go to the remote primary.
- V1 does not silently queue offline writes.
- V1 `write()` rejects DDL with a typed error unless the same release includes DDL propagation and lagging-replica schema tests.
- After a successful write, local replica is either updated by immediate pull or marked stale until pull completes.
- API exposes enough status for callers to decide whether local reads are stale.
- Lost response after remote commit is handled by an idempotency key or a typed `commit_outcome_unknown` error. Retrying a non-idempotent write must not double-apply silently.

Minimum tests:

- remote write succeeds, pull makes row visible locally,
- remote unavailable makes write fail with a typed error,
- remote write succeeds but pull fails, status reports stale local replica,
- status fields include local LSN, remote LSN, last success, last error.
- DDL through V1 `write()` rejected with typed error.
- commit succeeds remotely but response is dropped; retry behavior is idempotent or returns `commit_outcome_unknown`.

### A8: Observability Explains Staleness

Evidence required:

- Logs and metrics answer: "why is my local read stale?"
- Sync metrics are available without debug logging.
- Hot-path local reads are not slowed by sync logging.

Minimum tests:

- metrics scrape includes sync pull/push/apply/lag counters,
- slow query log remains independent of sync logs,
- failed sync emits structured error class,
- local read benchmark with sync package loaded stays within threshold.
- metrics label cardinality is bounded,
- raw database paths, tokens, and query params are absent from metrics/logs,
- sync metrics endpoint exposure reviewed as private bind or authenticated surface.

## Test Matrix

| Area | Tests | Required before |
|---|---|---|
| Embedded facade | open/query/reopen, panic safety, result parity | Any sync package release |
| Retained units | segment format, archive-before-truncate, GC, corruption | Server pull protocol |
| Bootstrap | full snapshot, snapshot+tail, incompatible version | Replica apply loop |
| Server protocol | auth, pull, cursor resume, stale cursor, write-forward | JS API release |
| Replica apply | idempotence, crash matrix, cursor persistence | V1 beta |
| Observability | metrics/logs/lag/stale explanation | V1 beta |
| Performance | embedded read, write-forward p50/p99, apply throughput | V1 beta |
| Product claims | unsupported claim scan, docs review | public launch |

## Failure Injection Plan

Use deterministic hooks where possible. If hooks are too invasive, use process-level tests with temporary data dirs and child processes.

Failure points:

1. after retained segment file write, before fsync,
2. after retained segment fsync, before checkpoint,
3. after checkpoint, before retained cursor metadata update,
4. halfway through retained-unit apply,
5. after retained-unit apply, before local cursor update,
6. after local cursor update, before process exit,
7. during sync handshake,
8. during write-forward response,
9. during schema change while replica is lagging.

Each failure test must assert one of:

- DB reopens and converges,
- operation resumes from cursor,
- operation fails closed with a repair path,
- unsupported version/cursor is rejected without data mutation.

## Performance Gates

Baseline commands:

```bash
cargo bench -p powdb-bench
cargo run -p powdb-bench --bin compare
```

Additional sync benchmarks to add:

- local readonly query with no sync background task,
- local readonly query while background pull is idle,
- local readonly query while retained units are applying,
- write-forward p50/p95/p99 over loopback,
- pull batch apply throughput,
- full bootstrap throughput by database size,
- retained-unit GC duration,
- metrics render under sync load.

Initial numeric gates, to be refined from the first baseline run:

- Embedded local read with sync package loaded: no more than 5% regression from the no-sync embedded baseline, unless explicitly accepted in the release notes.
- Local readonly query while background sync is idle: no more than 5% regression.
- Metrics render under sync load: must not lock the engine.
- Write-forward p95/p99 and apply throughput must be reported before beta. Any result outside the agreed release envelope must have named owner approval and a tracked release blocker or explicit release-note exception.

Rule:

> Sync cannot tax the local read hot path unless the user explicitly opts into stronger freshness or blocking read semantics.

## Release Blockers

Do not ship V1 if any of these are true:

- retained-unit history can lose needed history,
- replica apply can corrupt local data,
- remote write outage silently queues or drops a write,
- users cannot observe replica lag,
- sync auth leaks metadata before authentication,
- backup/restore and sync disagree on LSN or format version,
- docs imply offline local writes or partial sync are available in V1,
- local embedded read latency regresses without a documented and accepted reason,
- DDL can be forwarded through V1 `write()` without schema propagation tests.
- write-forward retry can silently double-apply a non-idempotent mutation.
- metrics expose raw identifiers, tokens, query params, or unbounded label cardinality.

## Review Checklist

Before implementation starts:

- [ ] Product plan accepted.
- [ ] Sprint plan accepted.
- [ ] This test spec accepted.
- [ ] Unsupported claims are explicitly documented.

Before code merge:

- [ ] Code review lane returns no blockers.
- [ ] Architect lane returns `CLEAR` or accepted `WATCH`.
- [ ] Storage crash tests pass.
- [ ] Server protocol tests pass.
- [ ] Node package tests pass.
- [ ] Performance gate has fresh numbers.

Before launch:

- [ ] Product review confirms launch copy matches shipped behavior.
- [ ] Fresh user quickstart works.
- [ ] Recovery runbook exists.
- [ ] Lag/staleness troubleshooting guide exists.
- [ ] Package installation smoke passes on supported targets.

## First Test-Driven Slice

The first implementation slice is deliberately narrow:

1. retained replication-unit segment format — implemented,
2. append and read retained units by LSN range — implemented for immutable segments,
3. archive-before-truncate proof — segment no-clobber atomic publish plus sync-aware checkpoint/recovery hooks implemented,
4. corrupt/missing segment rejection — implemented for checksum, truncation, impossible record count, missing ranges, overlaps, and filename/header mismatch,
5. one cold-start bootstrap fixture using snapshot + retained-tail proof — implemented,
6. retained-tail row-convergence fixture — implemented for complete insert/update/delete tails,
7. no public JS API yet — preserved.

This proves the substrate before exposing a product surface.
