# Sprint Plan — PowDB Embedded Sync

Generated: 2026-06-30
Review packet: `docs/strategy/2026-06-30-embed-sync-review-packet.md`
Review report: `docs/strategy/2026-06-30-embed-sync-plan-review-report.md`
Based on:

- `docs/design/2026-06-27-embedded-mode-design.md`
- `docs/design/2026-06-05-deployment-and-sync-strategy.md`
- `docs/design/2026-06-05-backup-pitr-sync-migrations-plan.md`
- `docs/design/distributed-and-tenancy-roadmap.md`
- `docs/strategy/2026-06-30-embed-sync-test-spec.md`
- Current repo scan of `crates/powdb`, `bindings/node`, `crates/storage`, `crates/server`, `crates/backup`, `clients/ts`

## Sprint Goal

Design and build the first production-grade PowDB Embedded Sync vertical slice: local embedded reads, remote write-forward, primary-to-replica sync-down, and observable replica lag.

## Success Criteria

- [ ] Existing embedded package remains stable and benchmarked.
- [ ] Sync contract is documented before implementation.
- [ ] Retained replication-unit/snapshot substrate has tests before protocol work depends on it.
- [ ] V1 supports local readonly queries and remote writes.
- [ ] Replica catches up after restart.
- [ ] Lag and failures are observable.
- [ ] No planner changes are required.
- [ ] No partial sync or offline local writes are promised in V1.
- [ ] The retained replication unit is defined before implementation.
- [ ] DDL is rejected through V1 `write()` unless schema propagation tests exist.

## Priority Definitions

| Priority | Criteria |
|---|---|
| P0 | Data loss, corruption, false production claim, or sync correctness blocker. |
| P1 | Required for a credible V1 launch. |
| P2 | Useful but deferrable until after V1. |
| P3 | Research or future product expansion. |

## Track 1: Embedded Package Baseline

Owner role: backend implementer / package maintainer

Primary files:

- `crates/powdb/src/lib.rs`
- `bindings/node/src/lib.rs`
- `bindings/node/package.json`
- `bindings/node/README.md`
- `bindings/node/__test__/embedded.test.mjs`
- `.github/workflows/publish-node-addon.yml`

Tasks:

- [ ] ES-01 (P0): Add or verify embedded/server result-shape parity tests.
- [ ] ES-02 (P0): Add crash-open/corrupt-data smoke tests where practical.
- [ ] ES-03 (P1): Verify `setSyncMode("normal")` is documented as bounded-loss, not fully durable.
- [ ] ES-04 (P1): Add install smoke for all supported addon platforms.
- [ ] ES-05 (P1): Add benchmark fixture for embedded read and write latency by WAL mode.
- [ ] ES-06 (P2): Document same-process vs multi-process data-dir lock behavior.

Exit evidence:

- `cd bindings/node && npm test`
- `cargo test -p powdb`
- addon packaging smoke, or documented platform blocker

## Track 2: Sync Contract And Public API

Owner role: architect / docs implementer

Primary files:

- `docs/design/`
- `docs/POWQL.md`
- `README.md`
- future `clients/sync/README.md`

Tasks:

- [ ] SC-01 (P0): Write exact V1 consistency contract: local reads, remote writes, primary-authoritative pull.
- [ ] SC-02 (P0): Define stale-read semantics and lag visibility.
- [ ] SC-03 (P0): Define failure behavior for remote write outage, pull outage, schema mismatch, stale cursor, and corrupt retained unit.
- [ ] SC-04 (P1): Specify JS API for `ReplicaDatabase.open`, `queryReadonly`, `write`, `syncNow`, `syncStatus`, and `close`.
- [ ] SC-05 (P1): Specify auth model for replica pull and write-forward.
- [ ] SC-06 (P1): Define unsupported cases in docs: partial sync, offline local writes, multi-primary, automatic sharding.

Exit evidence:

- API docs reviewed against implementation plan.
- Unsupported cases are visible in launch docs.

Current progress:

- SC-01/SC-02/SC-03/SC-06 are now summarized in `docs/embedded-sync.md` as the user-facing V1 contract: local reads may be stale, writes go to the primary, no offline write queue, read-your-writes requires pull, DDL is rejected in V1 until schema propagation tests exist, and unsupported V1 cases are explicit.

## Track 3: Retained Replication-Unit Log And Snapshot Substrate

Owner role: storage/database implementer

Primary files:

- `crates/storage/src/wal.rs`
- `crates/storage/src/catalog.rs`
- `crates/backup/src/*`
- new `crates/sync/src/*` or equivalent

Tasks:

- [x] RF-01 (P0): Design retained replication-unit segment format with magic/version/checksum/LSN range.
- [x] RF-02 (P0): Enforce archive-before-truncate ordering.
- [x] RF-03 (P0): Add per-replica cursor metadata and minimum retained LSN.
- [ ] RF-04 (P0): Add crash tests for checkpoint while retained units are needed by a replica.
- [x] RF-05 (P1): Add snapshot + retained-tail bootstrap path that reuses `powdb-backup` snapshot/manifest concepts instead of creating an unrelated backup format.
- [x] RF-06 (P1): Add corrupt/missing segment tests for the segment substrate.
- [x] RF-07 (P1): Add retention GC tests proving active cursors cannot be invalidated.
- [ ] RF-08 (P2): Share manifest/versioning logic with `powdb-backup`.
- [ ] RF-09 (P0): Define the retained replication unit: WAL-record segment, page-delta segment, or explicit hybrid. Add golden encode/decode/apply fixtures for insert/update/delete/DDL/transaction boundaries.
- [x] RF-10 (P0): Add snapshot identity/fork-safety metadata: database id, primary generation, source LSN, schema/catalog hash, WAL/retained-unit/catalog versions.
- [ ] RF-11 (P0): Prove atomic publish ordering: temp segment, segment fsync, no-clobber final publish, directory fsync, manifest/cursor fsync, then checkpoint/truncate.
- [x] RF-12 (P1): Add retention-pressure policy: max retained bytes, inactive cursor expiry, operator override, alerting, and rebootstrap behavior.

Exit evidence:

- `cargo test -p powdb-storage`
- `cargo test -p powdb-backup`
- new sync substrate tests
- explicit test proving truncation cannot outrun retention
- corrupt/partial segment recovery test

Current progress:

- RF-01 complete in `docs/design/2026-06-30-retained-replication-unit-log-design.md` and `crates/sync/src/segment.rs`.
- RF-02 complete through a storage-level WAL archive hook plus `powdb-sync` checkpoint/recovery helpers: sync-aware checkpoint/open archive WAL records into retained segments before truncation, backup uses the preserving checkpoint path when identity exists, and plain checkpoint/recovery fails closed for sync-enabled WAL history if no archive hook is supplied.
- RF-03 substrate complete in `crates/sync/src/metadata.rs`: durable database identity, secure OS-random database id generation, primary generation, replica cursor file, serialized cursor mutation, stale-lock recovery, active-cursor minimum retained LSN, atomic cursor updates, and corrupt metadata rejection exist.
- RF-05 cold-start substrate complete in `crates/backup/src/bootstrap.rs` and `crates/backup/tests/sync_bootstrap.rs`: bootstrap takes a live primary `Catalog`, archives the uncheckpointed WAL tail, validates snapshot identity, retained-tail continuity, and V1 applyability through the current primary LSN, rejects unsupported post-snapshot DDL tails before restore/cursor publication, restores the full backup into an empty replica path, registers the primary-side cursor under the cursor metadata lock, and removes the restored replica directory if cursor publication fails after restore.
- RF-07 complete in `crates/sync/src/retention.rs`, `crates/sync/src/metadata.rs`, and `crates/sync/tests/retention_gc.rs`: retained segment GC uses active replica cursors, shares the cursor metadata lock with cursor publication, rejects stale cursor publication when retained history is already missing, ignores retired cursors, keeps any segment crossing the active retention boundary, validates retained tail identity/range/gaps before deleting anything, treats no-active-cursor as a conservative no-op, reports max-retained-byte pressure, retires inactive cursors by explicit age policy, and supports an operator retain-LSN override that forces lagging cursors into rebootstrap.
- RF-09 definition complete: V1 retained unit is the current WAL record. Apply fixtures complete in `crates/sync/src/apply.rs` and `crates/backup/tests/sync_apply.rs`: restored replicas can apply a complete post-snapshot tail, converge insert/update/delete rows plus index-backed lookup, reject divergent local WAL, persist apply-state, replay from the recorded safe watermark only while the catalog still matches that watermark, mark complete when storage reached the target LSN but the complete marker was not written, fail closed when the catalog advanced partway without complete state, reject unsupported DDL units, and reject explicit transaction ranges that cut before commit/rollback. DDL propagation, concurrent-read chunked apply, and broader repair fixtures remain pending.
- RF-11 partially complete: segment temp-write, file sync, no-clobber hard-link publish, directory fsync, temp cleanup, second directory fsync on Unix, idempotent same-range archive retry, and checkpoint/recovery archive-before-truncate integration exist. Cursor-GC fsync and crash-injection matrix remain pending.
- RF-06 segment-substrate tests complete: corrupt footer checksum, truncated segment, missing segment gap, overlapping segments, filename/header mismatch, concurrent same-range publish, impossible record-count header rejection, and retained-tail availability overlap/gap rejection exist. Apply repair-path tests remain pending.
- RF-10 complete in `powdb-backup`: full and incremental manifests preserve sync fork-safety metadata when identity exists, default full/chain restore strips sync identity and remains writable through the plain engine lifecycle, explicit preserve restore recreates only the source `identity.json`, explicit fork restore mints a new sync identity, legacy manifests still restore without sync state, and chain restore rejects mixed identity/generation or stale catalog hash.
- Engine backup hardening complete for active explicit transactions: `Catalog::checkpoint` and `checkpoint_with_wal_archive` refuse to flush while a transaction is active, `Catalog` drop abandons active transaction dirty heap state, and full/incremental backup tests prove failed backups do not persist uncommitted rows.
- Storage LSN hardening complete for this gate: `Catalog::max_lsn()` now includes a durable catalog-level LSN sidecar so DDL-only backups and recovery-skipped WAL records cannot reuse retained LSNs or hide schema-only progress.

## Track 4: Server Sync Protocol

Owner role: backend/server implementer

Primary files:

- `crates/server/src/protocol.rs`
- `crates/server/src/handler.rs`
- `crates/server/src/main.rs`
- `crates/server/src/metrics.rs`
- `crates/auth/src/*`

Tasks:

- [ ] SP-01 (P0): Add sync handshake with database id, client replica id, format versions, and cursor.
- [x] SP-02 (P0): Authenticate sync before returning schema or LSN metadata.
- [x] SP-03 (P0): Add pull retained units since cursor with batching and resume. Pull output is capped by the currently servable retained LSN and the server-computed authoritative remote LSN, not by retained segment contents alone; V1 pull chunks are transaction-boundary-aware and fail clearly instead of returning a range that cuts before commit/rollback.
- [x] SP-04 (P1): Add remote write-forward path using existing query execution and RBAC; reject DDL through V1 `write()` with a typed error unless DDL propagation tests are part of the same release.
- [ ] SP-05 (P1): Add schema mismatch and stale cursor errors.
- [x] SP-06 (P1): Add sync metrics and structured logs.
- [x] SP-07 (P1): Add integration test for pull after write.
- [ ] SP-08 (P2): Add server-side read-only replica mode only after V1 protocol is stable.
- [x] SP-09 (P1): Define lost-response semantics for write-forward: idempotency key or typed `commit_outcome_unknown`; non-idempotent retries must not double-apply silently.
- [x] SP-10 (P1): Add primary-side cursor acknowledgement and sync-status substrate in `powdb-sync` before exposing wire messages. Status now separates primary progress (`remoteLsn`) from currently ship-ready retained history (`servableLsn`) and not-yet-archived primary history (`unarchivedLsn`).
- [x] SP-11 (P0): Add private server sync status, retained-tail pull, and apply-ack wire messages. Sync frames require credentialed access, reject named `readonly` users, share the same explicit-transaction gate as normal queries, and are unavailable on a connection that currently owns an open transaction; pull requests bind replica id, cursor LSN, database identity, primary generation, WAL/catalog/segment format versions, max unit/byte budgets, the server's authoritative remote-LSN cap, and the currently servable retained-LSN cap. Apply acknowledgements validate the retained range before cursor advance so clients cannot acknowledge a transaction-cut LSN.

Current progress:

- SP-06 complete for the private server sync protocol in
  `crates/server/src/metrics.rs` and `crates/server/src/handler.rs`: sync
  status/pull/ack frames now record operation/result counters, per-operation
  duration histograms, repair-action counters, pull unit/byte totals, and
  ack-advanced counts without replica-id labels. Retained-unit byte accounting
  now lives next to the protocol wire type so max-byte enforcement and metrics
  stay aligned with encoding changes. Structured sync decision logs include
  operation, redacted replica fingerprint, LSNs, lag, unit/byte counts,
  repair action, ack-advanced state, bounded typed error classes, and elapsed time;
  they omit raw replica ids, database ids, credentials, query text, retained
  payload text, local data paths, and raw filesystem-derived error strings.
  Invalid or overlong replica ids use a fixed placeholder fingerprint instead
  of hashing attacker-controlled input. The stable fingerprint is documented as
  pseudonymous operator-correlation data, not strong anonymization.
  `crates/server/tests/sync_protocol.rs` verifies real authenticated
  status/pull/ack traffic increments the metrics and does not expose sensitive
  sync details through the metrics scrape; handler unit tests pin stable
  redacted replica fingerprints and bounded error labels.

Exit evidence:

- `cargo test -p powdb-server`
- `cargo test -p powdb-server sync_status_reports_await_archive_when_primary_outruns_retained_tail -- --nocapture`
- `cargo test -p powdb-server sync_pull_serves_partial_retained_prefix_when_archive_lags_remote_lsn -- --nocapture`
- `cargo test -p powdb-server sync_pull_never_serves_units_beyond_server_remote_lsn -- --nocapture`
- `cargo test -p powdb-server sync_pull_and_ack_reject_transaction_cut_boundaries -- --nocapture`
- `cargo test -p powdb-server --test sync_protocol`
- `cargo test -p powdb-server metrics`
- `cargo test -p powdb-query rollback_wal_archive_failure_keeps_transaction_retryable -- --nocapture`
- `cargo test -p powdb-storage --test wal_recovery`
- TS client/server integration test for write-forward + pull
- metrics scrape includes sync counters

## Track 5: Embedded Replica Apply Loop

Owner role: embedded/sync implementer

Primary files:

- new `crates/sync/src/replica.rs`
- new `clients/sync/src/*` or `packages/sync/src/*`
- `bindings/node/src/lib.rs` only if native API surface needs extension

Tasks:

- [ ] RA-01 (P0): Apply pulled units so local reads see either the previous applied LSN or a fully applied committed LSN, never a partial statement/DDL/transaction batch.
- [x] RA-02 (P0): Persist local sync state separately from volatile process memory.
- [ ] RA-03 (P0): Refuse incompatible schema/WAL/catalog format versions.
- [x] RA-04 (P1): Implement `syncNow()` and background sync interval.
- [x] RA-05 (P1): Implement `syncStatus()`.
- [x] RA-06 (P1): Implement `write()` remote-forward method.
- [ ] RA-07 (P1): Add restart-after-partial-apply tests.
- [ ] RA-08 (P2): Add local repair/rebootstrap helper.
- [ ] RA-09 (P0): Add concurrent-read-during-apply tests covering heap, index, and catalog/schema mutations.

Exit evidence:

- JS integration test: open local replica, pull, read locally, write remotely, pull, read update locally.
- Crash/restart test around apply.

Current progress:

- First complete-tail apply path exists in `powdb-sync::apply_retained_tail`. It validates replica identity, retained-tail continuity, local LSN alignment, persists local apply-state, and then applies retained WAL records through storage's LSN-preserving replay path.
- `crates/backup/tests/sync_apply.rs` proves snapshot plus post-snapshot retained tail converges rows after insert/update/delete and that duplicate apply is a no-op after the target LSN is reached. `crates/sync/src/apply.rs` unit tests prove retry replays from the recorded safe watermark when the catalog still matches, marks complete when storage already reached the target LSN, fails closed when the catalog advanced only partway without complete state, rejects unsupported DDL, rejects explicit transaction ranges that cut before commit/rollback, and rejects a different in-progress apply range.
- `crates/sync/src/replica.rs` now provides primary-side `acknowledge_replica_apply` and `replica_sync_status`. Unit tests prove acknowledgements advance cursors monotonically, stale/inactive acknowledgements fail closed, status reports pullable lag with retained segment byte estimates, missing retained history recommends rebootstrap, and status-generation errors leave the previous cursor intact.
- `clients/sync` now provides `PowDBSyncReplica.syncNow()`,
  `startBackgroundSync(...)`, `syncStatus()`, and `write(...)`; package tests
  cover structural adapters, real native local apply, and a full
  backup-bootstrap/server/client/native e2e. `write(...)` rejects V1 DDL before
  remote execution and maps ambiguous remote failures to
  `commit_outcome_unknown` instead of retrying non-idempotent writes.
- RA-01 remains open for concurrent-read visibility, multi-chunk apply state, and DDL/index propagation.

## Track 6: Production Hardening And Observability

Owner role: backend validator / test engineer

Primary files:

- `crates/server/src/metrics.rs`
- `crates/server/src/handler.rs`
- `crates/query/src/executor/*`
- `.github/workflows/ci.yml`
- `scripts/quality.sh`
- `crates/bench/*`

Tasks:

- [x] PH-01 (P0): Add current sync correctness tests to CI. `ts-client` now
  runs the live server-backed sync test, and the `embedded-sync-js` CI job builds
  release CLI/server, builds the native addon, and runs
  `@zvndev/powdb-sync` build/unit/native/e2e checks. Crash/rebootstrap/metrics
  expansion remains PH-02/PH-04 work, not a missing CI hook for the current
  vertical slice.
- [ ] PH-02 (P0): Add crash matrix for WAL/retained-unit retention and replica apply.
- [ ] PH-03 (P1): Add slow-query log fields: normalized shape, plan summary, index use/fallback, rows scanned/output.
- [ ] PH-04 (P1): Add metrics for WAL fsync latency, checkpoint duration, retained bytes, and sync lag.
- [ ] PH-05 (P1): Add benchmark thresholds for local reads with sync package loaded.
- [ ] PH-06 (P1): Add previous-release open/restore fixture once artifacts exist.
- [ ] PH-07 (P1): Add fuzz/property tests for retained segment parser before protocol exposure.
- [x] PH-08 (P1): Add offline operator sync visibility. `powdb-cli
  sync-status [REPLICA_ID]` now reports primary-side cursor state, local and
  remote LSNs, servable/unarchived retained history, lag estimates, stale
  state, and recommended repair action. Repair/rebootstrap automation remains
  separate beta work.

Exit evidence:

- `cargo fmt --all`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `cargo bench -p powdb-bench`
- `cargo run -p powdb-bench --bin compare`

## Initial Implementation Order

1. Lock the V1 sync contract in docs.
2. Define retained replication unit and fork-safety metadata. Retained unit, durable database identity, and snapshot/backup fork-safety metadata are defined.
3. Harden embedded package and parity tests.
4. Build retained log and snapshot/bootstrap tests.
5. Add server pull protocol. Private authenticated status/pull/ack frames are now in place and exercised by the TS client.
6. Add replica apply loop. Native retained-unit apply and JS orchestration are in place for the current vertical slice.
7. Add JS `@zvndev/powdb-sync` API. Experimental package, background sync, native adapter, and full backup-bootstrap e2e now exist.
8. Add observability and release gates.
9. Run code-review and product-review before release.

## Boundary Rules

- `crates/query` must remain distribution-unaware.
- `crates/sync` owns cursors, retained units, bootstrap, apply, protocol state, and conflict policies.
- `crates/backup` remains the restore/proof substrate; sync should reuse it for bootstrap and repair.
- `crates/server` owns transport, auth, metrics, and primary-side dispatch.
- `crates/powdb` and `bindings/node` expose sync controls only as facades.

## Deliberately Deferred

- Offline local writes.
- Manual conflict resolution.
- Partial row-level sync.
- Multi-primary.
- Built-in Raft.
- Automatic sharding.
- Postgres wire compatibility.
- Full SQL compatibility.

## Review Gates

Before merging any implementation:

- Run a two-lane code review: `code-reviewer` + `architect`.
- Run backend validation on durability, sync, auth, and observability.
- Run product review against launch claims.
- Verify the relevant acceptance criteria in `docs/strategy/2026-06-30-embed-sync-test-spec.md`.
- Confirm docs do not claim unsupported distributed behavior.

Before launch:

- Run install smoke for packages.
- Run crash/recovery/sync interruption suite.
- Run benchmarks and compare to baseline.
- Verify metrics/logs answer: "why is my replica stale?"
- Verify a fresh user can follow the quickstart.

## Top 3 Next Actions

1. Add concurrent-read chunked retained-tail apply plus crash-injection tests around segment publish, checkpoint, recovery, cursor update, and bootstrap repair.
2. Add server-side idempotency-key transport or keep the existing explicit
   `commit_outcome_unknown` behavior as the documented V1 retry boundary.
3. Add authenticated/private sync metrics/logs that match the new CLI
   `sync-status` state model without leaking paths, tokens, query params, or
   high-cardinality IDs.
