# PowDB Enterprise-Readiness Roadmap (multi-model consensus)

**Date:** 2026-06-06
**Status:** Roadmap / not yet scheduled. Each adopted epic gets its own TDD implementation plan (like `docs/superpowers/plans/2026-06-05-backup-pitr-sync-migrations.md`) before any code is written.

## How this list was built

Three models (Claude Opus 4.8, Sonnet 4.6, Haiku 4.5) were each asked **independently** — no shared context, no cross-talk — to identify the enterprise/production-readiness features PowDB is missing, beyond the already-in-flight backup/PITR/cloud-sync/migration work. Each returned a prioritized list with rationale, effort, dependencies, and a deliberate-rejection list.

**Adoption rule (per Kirby):** only features that **≥2 of the 3 models independently proposed** are adopted. Each row below shows the vote count. Unanimous (3/3) items are the highest-confidence gaps; majority (2/3) items are adopted but lower-confidence. Features proposed by only one model, or rejected by consensus, are listed at the bottom and are **not** on the roadmap.

The consensus priority is the rounded consensus of the three models' priorities (P0 = table-stakes blocker for any enterprise deployment; P1 = expected shortly after; P2 = maturity/differentiation).

## Adopted features (consensus ≥ 2/3)

| # | Feature | Category | Opus | Sonnet | Haiku | Votes | Consensus | Notes / PowDB-specific hook |
|---|---|---|---|---|---|---|---|---|
| 1 | **Multi-user auth + roles/RBAC** | Security | P0 | P0 | P0 | **3/3** | **P0** | Root dependency for audit, RLS, quotas, per-user rate limits. New `users`/`roles` system catalog extending `crates/storage/src/catalog.rs`; argon2/bcrypt password hashing; auth check at session establish. Replaces the single shared password. |
| 2 | **TLS on wire protocol + CLI** | Security | P0 | P0 | P0 | **3/3** | **P0** | Hard SOC2/HIPAA control. Server has `POWDB_TLS_CERT/KEY` already; **the CLI has no TLS** — concrete known hole. Add rustls to the accept loop + CLI client; WireGuard is not an accepted substitute by reviewers. |
| 3 | **Structured audit logging (append-only / tamper-evident)** | Compliance | P0 | P0 | P0 | **3/3** | **P0** | "Who ran what, when, from where." Separate append-only sink **distinct from the WAL** (WAL is a recovery artifact, not an audit log). Hash-chained for tamper-evidence. Requires #1 for principal identity. |
| 4 | **Query timeouts + statement cancellation** | Governance | P0 | P0 | P0 | **3/3** | **P0** | A single unbounded PowQL scan can pin a core and starve every connection on the single process. Needs a per-session deadline/cancel token threaded into the executor's row-iteration loops (`try_for_each_row_raw`). Current `POWDB_QUERY_TIMEOUT` is server-wide/advisory only. |
| 5 | **Prometheus metrics + health/readiness endpoints** | Observability | P0 | P0 | P1 | **3/3** | **P0** | Cheapest-high-leverage. `/metrics` + `/health` + `/ready` (ready gates on WAL replay complete). Instrument QPS, p99, WAL fsync lag, plan-cache hit rate, active connections. Required by Fly/k8s/ECS orchestration. |
| 6 | **Connection limits** | Operations | P0 | P1 | P1 | **3/3** | **P1** | Unbounded connections exhaust FDs/memory (each session holds executor state). `MAX_CONNECTIONS` is currently hardcoded — make it configurable + add a server-side semaphore/queue. |
| 7 | **Graceful shutdown / connection drain** | Operations | P0 | P1 | P1 | **3/3** | **P1** | SIGTERM should stop accepting, let in-flight txns commit/rollback, flush WAL, exit. Today a SIGTERM mid group-commit risks a recovery-on-every-restart situation. Drain flag checked at accept loop + WAL flush boundary. |
| 8 | **Structured logging + slow-query log** | Observability | P1 | P1 | P1 | **3/3** | **P1** | `tracing` already present; add JSON output + a slow-query log (query text, timing, rows examined, plan shape) hooked at executor exit. The #1 production DB debugging tool. |
| 9 | **Constraints: UNIQUE / CHECK / (FK)** | Data Integrity | P1 | P1 | P1 | **3/3** | **P1** | UNIQUE rides on the existing B+tree indexes (checked at INSERT/UPDATE). CHECK reuses the predicate evaluator before the WAL write. FK is the heaviest and lands last (needs referential checks across insert/update/delete). NOT NULL already exists (`required`). |
| 10 | **Async replication via WAL shipping (+ read replicas)** | High Availability | P1 | P2 | P1 | **3/3** | **P1** | The single biggest HA gap. PowDB's per-page-redo WAL (LSN-sequenced, fixed in v0.4.3) is the **architecturally-native** substrate: a follower replays shipped WAL. **Scope this WITH the planned PITR/cloud-sync work — both are WAL/LSN consumers and would otherwise duplicate the streaming plumbing.** |
| 11 | **Encryption at rest (engine-managed)** | Security | P1 | P2 | P1 | **3/3** | **P1** | Per-page AES-GCM in `page.rs` + WAL; key management is the hard part (ties to #15). Fly encrypted volumes cover one target but not on-prem/AWS-operator-controlled disks. |
| 12 | **Row-level security (RLS)** | Security / Multi-tenancy | P2 | P1 | P1 | **3/3** | **P1** | Enables safe single-process multi-tenancy. Planner injects filter predicates keyed to the session's role. **Interacts with the plan cache: the cache key must include the principal/role**, or cached plans leak across tenants. Requires #1. |
| 13 | **Bulk import/export (dump / load)** | Data Management | P1 | P2 | P1 | **3/3** | **P1** | "Can I get my data back?" is a procurement gate. Logical, portable export (walks heap files) + a bulk-load fast path reusing multi-row INSERT. Distinct from the binary backup work. |
| 14 | **Deadlock detection + lock timeouts** | Reliability | P2 | P1 | P1 | **3/3** | **P1** | With real concurrent writers, two txns can cycle and hang forever. Wait-for graph + cycle detection (on commit or background timer), or at minimum a lock-acquire timeout + client retry signal. |
| 15 | **Secrets handling (no plaintext creds)** | Security / Ops | P1 | P2 | — (key-mgmt) | **2/3** | **P1** | Shared password / future per-user creds / encryption keys must not sit in plaintext env/files. Pluggable secret source at startup. Ties to Kirby's existing Keeper-for-ZVN-tokens intent. |
| 16 | **Multi-tenancy isolation** | Governance | P2 (quotas) | — (via RLS) | P1 | **2/3** | **P1** | Run many logical DBs/tenants in one process safely (vs today's process-per-DB). Built on #1 + #12. Sequenced after RBAC/RLS. |
| 17 | **Per-tenant / per-query resource quotas** | Governance | P2 | P2 | P1 | **3/3** | **P2** | Noisy-neighbor isolation: CPU/memory/storage caps per principal. Current `POWDB_QUERY_MEMORY_LIMIT` is server-wide. Executor checks consumption against a per-connection budget. Requires #1. |
| 18 | **Automatic failover (external orchestration)** | High Availability | P2 | (reject built-in Raft) | (HA) | **2/3** | **P2** | Promote a replica on primary failure. **Consensus is to use external orchestration (Fly restart policies / Consul / etcd health checks), NOT a built-in Raft layer** (see rejections). Requires #10. |

## Suggested sequencing (epics)

The dependency graph makes the order fairly forced. Each epic is a future standalone TDD plan.

1. **Epic A — Identity & Transport (P0):** #1 RBAC → #2 TLS → #3 Audit logging. RBAC first because audit attribution, RLS, and quotas all key off principal identity and share the new `users`/`roles` catalog. This is the single biggest enterprise unlock.
2. **Epic B — Operability (P0/P1):** #5 metrics/health → #4 query timeouts/cancellation → #6 connection limits → #7 graceful shutdown → #8 structured/slow-query logging. Mostly independent of Epic A; can run in parallel. Highest leverage-per-effort.
3. **Epic C — Data Integrity (P1):** #9 constraints (UNIQUE → CHECK → FK), #13 bulk import/export, #14 deadlock detection.
4. **Epic D — Confidentiality & Tenancy (P1):** #11 encryption-at-rest, #15 secrets handling, #12 RLS, #16 multi-tenancy, #17 quotas. Depends on Epic A.
5. **Epic E — High Availability (P1/P2):** #10 async replication + read replicas (co-design with the in-flight PITR/cloud-sync work), then #18 failover via external orchestration.

**Highest-confidence "do these first" (all 3 models' top-5 overlap):** RBAC, TLS, audit logging, query timeouts/cancellation. These four are unanimous P0s with no workaround — they are the literal compliance/survivability gates.

## Deliberately NOT on the roadmap (consensus rejections or single-model)

| Feature | Why excluded |
|---|---|
| **SQL / Postgres-wire compatibility** | **Unanimous 3/3 reject.** Violates PowDB's entire thesis — re-introduces the translation overhead the project exists to eliminate. Never propose. |
| **Built-in Raft / distributed multi-node consensus / sharding / active-active** | **Unanimous 3/3 reject.** Massive effort, contradicts the lean single-node design. Async read replicas + external-orchestrated failover cover the real availability need at a fraction of the cost. Revisit only if a customer outgrows one node. |
| **Server-side connection pooler** | Rejected (Opus): connection *limits* matter (#6, adopted); a full pooler belongs in the client/sidecar layer. |
| **Query pipelining / multiplexing** | Rejected (Sonnet); low value for PowDB's WireGuard-private, few-services topology; connection pooling solves the latency motivation with far less protocol complexity. |
| **Column-level security** | Not championed; RLS (#12) covers ~90% of multi-tenant needs; CLS adds planner/compiled-predicate complexity for marginal gain. |
| **GUI / admin dashboard** | Rejected (Opus): real value but never a blocker; the metrics endpoint + CLI cover the operational need. |
| **More language clients (Python/Go/Java…)** | Single-model (Haiku) only. Valuable DX, not a blocker; defer until the wire protocol is stable. |
| **At-rest page-format versioning / upgrade routine** | Single-model (Opus) only. Genuinely valuable for long-term adoption; revisit — flagged here so it isn't lost, but not consensus-adopted. |
| **Online / non-blocking schema change** | Single-model (Haiku) only. `alter` currently rewrites the heap under the write lock; worth revisiting once MVCC/shadow-table groundwork exists. |
| **Data-residency / geographic constraints** | Single-model (Haiku) only; mostly a function of the (rejected) multi-region story. |

## Caveat from the research (durability prerequisite)

Two models flagged the 2026-06-04 durability assessment that found PowDB **not production-ready** (post-restart write loss, `count(subquery)=0`, stale matview reads). The git history shows these were addressed: `bef3f11` (v0.4.3, page-LSN stamping on alter rewrite) and `910bf12` ("eliminate three data-loss bugs in crash recovery", v0.4.4). **Recommendation:** before building any of the above on top, run the documented prod smoke-test (README PowQL flow + WAL-replay restart against the installed binary) to confirm the durability P0s are truly closed — none of these enterprise features matter if a committed write can still vanish on crash.

## Next step

Nothing here is scheduled yet. When ready, each epic becomes its own `docs/superpowers/plans/` TDD plan with the same continuous-verification protocol (per-task full-suite + clippy/fmt, per-phase `cargo run -p powdb-bench --bin compare` regression gate) used by the backup plan. The current in-flight work (backup/restore/PITR/sync/migrations) ships first.
