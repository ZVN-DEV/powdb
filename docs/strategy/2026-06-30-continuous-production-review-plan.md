# Continuous Production Review Plan

Date: 2026-06-30
Status: Draft for review
Scope: Ongoing code-review, product-review, product-team, and product-sprint loop for PowDB production readiness.

Review packet: `docs/strategy/2026-06-30-embed-sync-review-packet.md`
Review report: `docs/strategy/2026-06-30-embed-sync-plan-review-report.md`

## Goal

Continually improve PowDB as:

- a useful local embedded database for native apps,
- a server-backed synced embedded database,
- a PowQL-first Postgres alternative for app-owned workloads with a supported SQL subset, not Postgres wire compatibility,
- a production-ready Rust database engine with strong performance, logging, safety, and recovery behavior.

This loop should keep shipping concrete improvements, not just reports.

## Operating Model

Use a weekly or milestone-based cycle:

1. **Audit:** run code review and product review.
2. **Backlog:** consolidate findings into one prioritized backlog.
3. **Sprint:** implement the highest-impact P0/P1 items.
4. **Verify:** run tests, benchmarks, smoke flows, and review again.
5. **Document:** update docs to match the real product state.

The process is continuous, but each cycle must have a finite stop condition and verification evidence.

## Review Lanes

### Code Review

Use the `code-review` skill shape:

- `code-reviewer` lane: security, correctness, performance, maintainability.
- `architect` lane: system boundaries, hidden coupling, long-term tradeoffs.

Final gate:

- `REQUEST CHANGES` if either lane finds a blocker.
- `COMMENT` if only non-blocking watch items remain.
- `APPROVE` only when both independent lanes return clear evidence.

### Product Review

Use the `product-review` skill shape for launch readiness:

- Is this real and runnable?
- What is differentiated?
- What is not differentiated?
- Who is it for?
- What security and production gaps remain?
- What would make a serious user reject it?

The review must distinguish:

- shipped,
- tested but not released,
- designed but not built,
- not planned.

### Product Team

Use the `product-team` shape for backlog generation:

- Product strategy
- Backend/security/data validation
- E2E/product flow validation where applicable
- Docs and onboarding review
- Implementation only for clear quick wins or explicitly selected sprint items

Standing guardrail from the local skill: no git worktrees by default.

### Product Sprint

Use product-sprint as the implementation frame, adapted to this repo:

- No worktrees unless explicitly overridden.
- Current branch or normal feature branch only.
- Small, reviewable tracks by subsystem.
- Verify after each track.
- Do not merge unrelated refactors into production fixes.

## P0 Audit Areas

### Durability And Recovery

Audit:

- WAL fsync ordering.
- WAL replay idempotency.
- page LSN monotonicity.
- checkpoint and truncate ordering.
- restore `next_lsn > max_page_lsn`.
- DDL crash behavior.
- data-dir lock behavior.
- backup/restore chains.
- sync retained-unit behavior once built.

Release blockers:

- Any committed write can disappear after crash.
- Restore can reopen with reused or stale LSN.
- Checkpoint can truncate data required by backup or sync.
- Corrupt files panic instead of erroring at public boundaries.

### Query Correctness

Audit:

- PowQL parser/planner/executor consistency.
- SQL lowering parity for the supported subset.
- NULL semantics.
- arithmetic and casts.
- joins.
- subqueries.
- aggregates.
- window functions.
- plan-cache literal substitution.
- params and injection safety.

Release blockers:

- Silent wrong answer.
- Supported SQL returns a shape that differs from equivalent PowQL.
- Plan cache returns stale or wrongly substituted plan.
- Query failure corrupts engine state.

### Index And Storage Correctness

Audit:

- B+tree persistence.
- index rebuild.
- unique enforcement.
- update/delete index maintenance.
- range-bound inclusivity.
- speculative index scan fallback.
- mmap scan races.
- oversized rows.

Release blockers:

- Index can return wrong row set.
- Unique index allows duplicates.
- update/delete leaves stale index entries.
- mmap scan can observe torn data.

### Server, Auth, And Client Behavior

Audit:

- RBAC role boundaries.
- TLS client/server behavior.
- auth failures.
- query cancellation.
- connection limits.
- graceful shutdown.
- TS client params and errors.
- Node addon parity.

Release blockers:

- readonly user can write.
- auth failure exposes internals.
- client/server protocol drift breaks compatibility.
- cancellation leaves server wedged.

### Sync And Replication

Audit once sync work begins:

- retained replication-unit log.
- snapshot + tail bootstrap.
- per-replica cursors.
- archive-before-truncate.
- retained-unit apply idempotency.
- stale cursor recovery.
- corrupt segment refusal.
- lag metrics.
- write-forward semantics.

Release blockers:

- A replica can corrupt itself during pull/apply.
- A sync cursor can be silently skipped.
- Retained-unit retention can delete needed history.
- Users cannot tell whether local reads are stale.

## Enterprise Feature Backlog

| Priority | Feature | Why |
|---|---|---|
| P0 | Structured audit logging | Enterprise users need attributable query/activity records. |
| P0 | Slow-query logging | Operators need to debug production performance. |
| P0 | Query cancellation enforcement | Advisory timeout is not enough for a database server. |
| P0 | Backup/restore proof suite | Production credibility depends on restore, not just backup. |
| P0 | Sync retention correctness | Embedded Sync cannot ship without this. |
| P1 | Read replicas | Natural next HA step after retained replication units. |
| P1 | Replica lag metrics | Required for trust in stale reads. |
| P1 | Upgrade compatibility tests | Users need safe version movement. |
| P1 | Bulk import/export | Procurement and migration table stakes. |
| P1 | Encryption-at-rest design | Needed for on-device and enterprise trust. |
| P1 | Per-database tenancy | Hard isolation before RLS complexity. |
| P2 | RLS | Only after identity, grants, plan-cache safety, and policy tests. |
| P2 | External failover tooling | After read replicas are real. |
| P3 | Sharding research | Only after a demonstrated one-node ceiling. |

## PowQL Optimization Backlog

Focus on correctness first, then hot-path speed.

High-value areas:

- compiled predicates coverage,
- plan-cache hit rate and invalidation correctness,
- index range scan bounds,
- join build/probe allocation,
- projection-before-aggregate fast paths,
- SQL lowering into existing fast-path shapes,
- typed row decode avoidance,
- top-k sort memory behavior,
- mmap scan remap behavior,
- prepared/parameterized query path.

Bench gates:

- indexed point lookup,
- non-indexed point lookup,
- scan/filter/project top N,
- aggregate min/max/sum/avg/count,
- group by/having,
- join equi-predicate,
- subquery IN/EXISTS,
- write throughput by WAL sync mode,
- backup/restore throughput,
- sync apply throughput once built.

Rule:

> No performance optimization is complete unless the correctness test that protects the optimized shape exists first.

## Logging And Metrics Backlog

Structured fields:

- request id,
- query id,
- principal,
- client address,
- database id/path hash,
- statement kind,
- normalized query shape,
- plan summary,
- index used or fallback reason,
- duration,
- rows scanned,
- rows returned,
- rows affected,
- WAL fsync duration,
- checkpoint duration,
- sync cursor,
- replica id,
- error class.

Prometheus additions:

- query latency by statement class,
- errors by class,
- WAL fsync latency,
- checkpoint duration/failures,
- WAL retained bytes,
- backup age and failures,
- sync lag and failures,
- lock wait time,
- memory budget exceeded,
- cancellation count,
- auth failures,
- active connections.

Performance rule:

- Hot path logging must be opt-in or aggregated.
- Metrics scrape must stay lock-free with respect to the engine.

## Verification Matrix

Required commands for standard cycles:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Required commands for performance or storage changes:

```bash
cargo bench -p powdb-bench
cargo run -p powdb-bench --bin compare
```

Required package checks when client/addon surfaces change:

```bash
cd clients/ts && pnpm run build && pnpm run test:sync-live
cd clients/sync && pnpm run build && pnpm test && pnpm run test:native && pnpm run test:e2e
cd bindings/node && npm run build && npm test
```

Required scenario checks:

- fresh install smoke,
- README PowQL flow,
- server auth flow,
- TLS flow,
- backup restore roundtrip,
- crash recovery,
- embedded open/query/close,
- sync replica catch-up once built.

Sync-specific acceptance criteria live in
`docs/strategy/2026-06-30-embed-sync-test-spec.md`; every sync cycle must name
which criteria it proves before implementation starts.

## Cycle Template

For each cycle, create or update a short cycle note with:

- scope,
- findings,
- selected P0/P1 items,
- files expected to change,
- verification commands,
- benchmark expectations,
- remaining risks,
- release/docs changes.

Use this default sprint shape:

1. **Cycle 1: Sync Contract And Substrate**
   - V1 docs
   - retained replication-unit design
   - archive-before-truncate tests

2. **Cycle 2: Embedded Replica Vertical Slice**
   - server pull
   - local apply
   - JS package skeleton
   - lag metrics

3. **Cycle 3: Production Hardening**
   - crash matrix
   - slow query logs
   - backup/sync restore proof
   - docs and packaging

4. **Cycle 4: Performance Pass**
   - PowQL fast-path coverage
   - sync overhead benchmarks
   - WAL/fsync latency instrumentation

5. **Cycle 5: Enterprise Readiness**
   - audit log
   - import/export
   - read replica plan
   - upgrade compatibility

## Stop Conditions

A cycle is done only when:

- selected P0/P1 items are implemented or explicitly deferred with reason,
- tests and relevant benchmarks have run,
- docs match the actual behavior,
- review findings are resolved or tracked,
- no known release blocker remains in the cycle scope.

## Immediate Next Cycle Recommendation

Continue **Cycle 2: Embedded Replica Vertical Slice**.

The retained replication-unit substrate, private pull/apply/ack control plane,
chunked apply primitive, experimental JS orchestration package, native embedded
`applyRetainedUnits` binding, CLI sync-enable/sync-bootstrap bridge, live TS
client status/pull/ack test, background scheduler, and full backup-bootstrap +
native local-apply JS e2e now exist. The next highest-value work is to harden
the proven vertical slice: stale/rebootstrap repair, DDL policy, idempotency or
explicit unknown-outcome semantics, metrics/logging, crash/interruption
coverage, and package publishing/version locking.

Do not begin offline local writes, conflict resolution, or sharding until the
primary-authoritative embedded replica is proven end to end.
