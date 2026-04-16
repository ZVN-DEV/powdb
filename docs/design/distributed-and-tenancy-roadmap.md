# Distributed + Multi-Tenancy Roadmap

Date: 2026-04-16
Scope: options and tradeoffs for (1) multi-node replication / sharding / consensus, and (2) fine-grained access control, row-level security, multi-tenant isolation. Stored procedures / UDFs / triggers are intentionally out of scope.

Today's state (single-node, shared password) is the baseline. Every option below describes the delta.

## Guiding constraints that shape every option

These aren't obstacles to route around — they're load-bearing. Any proposal that breaks one of them breaks PowDB's identity.

1. **The thesis is "fewer layers = faster."** Every network hop on the write path erodes the core pitch. Distributed writes are a product positioning change, not just an implementation change. The benchmark story becomes "faster than SQLite *per node*" rather than "faster than SQLite."
2. **Planner is pure — no catalog access** (`crates/query/src/planner.rs`). Plans are produced without knowing what indexes or nodes exist; the executor lowers at runtime. Any sharding design that needs distribution-aware planning breaks this.
3. **mmap-based scanning.** Heap scans use memory-mapped I/O. Distributed storage — remote blocks, replicated pages — invalidates this fast path entirely.
4. **WAL + group commit already exists** (`crates/storage/src/wal.rs`). This is the single biggest existing asset for anything replication-adjacent. Replication options that use it are dramatically cheaper than ones that don't.
5. **One connection = one database** already (`dbName` in the connect handshake). Multi-tenancy option β is mostly a matter of making this routing real.

---

# Part 1: Multi-node — replication, sharding, consensus

There are four separable axes here. People conflate them because vendors bundle them. They don't have to be.

| Axis | Provides | Does NOT provide on its own |
|---|---|---|
| Durability replication (WAL streaming) | Warm DR, PITR | HA failover, read scale |
| Leader-follower read replicas | Read scale, warm DR | Write HA, strong consistency |
| Consensus (Raft) | Automated failover, strongly consistent writes | Horizontal scale |
| Sharding | Horizontal scale for working set > one box | Anything else by itself |

Pick based on what you're trying to buy, not on what sounds impressive.

## Option D: External WAL shipping (Litestream-style)

**What:** Stream the WAL to object storage (S3-compatible) or to a sidecar. Reader nodes replay the WAL to maintain a lagging replica on disk. No consensus, no automated failover, no write coordination.

**What it buys:**
- Point-in-time recovery
- Read-only replica for reporting / dev staging
- Disaster recovery (loss of primary → bring up a replica elsewhere, lose some seconds of data)

**What it doesn't buy:**
- Write HA (primary dies → writes down until operator intervenes)
- Linearizable reads from replicas (they lag)

**Effort:** **2–4 weeks.** Add a WAL-shipper background task in `powdb-server`, a WAL-applier mode that runs the replica, a storage target abstraction (S3 / filesystem). No changes to query path, no changes to planner, no changes to lock model.

**Risks:** Small. The main correctness trap is ensuring WAL framing is stable and that applier handles truncation/checkpointing cleanly. The WAL was designed for local use — you may discover it doesn't serialize enough context for a cold replica (e.g., catalog changes should be in the WAL; verify they are).

**Who picks this:** "I just need a backup story and maybe a read-only reporting slave." Most early-stage deployments.

**Verdict: ship this first, regardless of which other option follows.** It's the prerequisite asset for A and B anyway, and it provides 80% of the "distributed" ask for small teams.

## Option A: Leader-follower read replicas

**What:** Primary owns all writes. One or more followers pull WAL chunks from primary over TCP, apply them, and serve reads. Clients opt into "read-from-replica" explicitly. Failover is manual.

**What it buys (over D):**
- Lower-latency read replicas (pull cadence can be ms-scale vs seconds for WAL shipping)
- A real read-scale story — point 10 read replicas at one primary

**What it doesn't buy:**
- Write HA. Primary still SPOF.
- Strong consistency (reads may be stale by the replication lag).

**Effort:** **4–8 weeks.** Reuses D's applier. New work: a streaming WAL subscription on the primary's wire protocol (a new message type or a dedicated replication port), client-side awareness that a connection is read-only, lag metrics, and "fence" checkpoints so replicas can detect if they forked.

**Risks:**
- **Split-brain on failover.** Manual failover is easy to botch; two primaries will silently diverge. Either document the exact "how to fail over" runbook carefully, or accept you'll eventually need a coordinator.
- **Read semantics.** If a client does write-then-read on a replica, the read may miss the write. You need `read-your-writes` routing (sticky to primary after any write) or explicit "I accept stale" opt-in in the wire protocol. Postgres chose sticky; it's a PITA but safe.

**Who picks this:** "I'm hitting CPU limits on read throughput, but my write throughput fits on one box." Very common — most OLTP workloads are 90%+ reads.

## Option B: Raft consensus (single-group)

**What:** Replace (or wrap) the WAL with a Raft log. Every write proposes to the Raft leader, which replicates the log entry to a quorum of followers before committing. Automated leader election on primary failure. Reads can be served by the leader (fast, via leader leases) or linearizably through Raft (slower).

**What it buys:**
- Write HA (tolerate `floor(N/2)` failures with `N` nodes, usually 3 or 5)
- No operator intervention on primary death
- Linearizable semantics if you want them

**What it doesn't buy:**
- Horizontal scale. All writes still go through one leader — a single box's write throughput.
- Lower latency. The *minimum* write latency becomes `round-trip to second-closest follower + fsync on quorum`. Typically 2-10ms vs sub-ms for local.

**Effort:** **3–6 months.** The hard parts:
- Integrate a Raft crate. `openraft` is the modern choice (async, pluggable log/state), `raft-rs` is older and tikv-proven. Estimate on integrating `openraft`: 2-3 weeks before first green test, then long tail of edge cases.
- Make the WAL the Raft log, or stack them. Stacking is easier but doubles fsync cost; unifying is faster but requires WAL format changes.
- Snapshot + log compaction. Followers need to be able to catch up from a snapshot if they're too far behind.
- Leader-lease reads. Without them, every read is a Raft round trip and you lose the speed pitch entirely.
- Membership changes (adding/removing nodes) done safely.
- Cluster bootstrap / discovery UX.

**Risks:**
- **Latency story changes.** This is the big one. "3-4× faster than SQLite" (from memory, PR #8) is a single-node number; in a 3-node Raft deployment the write path is `fsync + 1 RTT`. If your peers are in the same DC that's ~1ms extra; cross-DC it's 20-100ms. Be honest about this in docs — don't run single-node benchmarks and claim the distributed version is "just as fast."
- **Operational surface.** Clusters are operationally heavy — quorum loss scenarios, node replacement, snapshot transfer, version skew during rolling upgrades. Every one of these is a test matrix dimension you didn't have.
- **Planner assumption.** Planner purity survives; executor needs to know "am I leader? can I serve this read locally?" That's a small, localized change.

**Who picks this:** "I need automated HA — I cannot be the pager for a DB that crashes." Paid tiers of SaaS, anything with an SLA.

## Option C: Sharded Raft (CockroachDB-lite)

**What:** Data is partitioned (by hash of primary key, or by range) across many Raft groups. Each group is a small Raft cluster (3 or 5 replicas). A distributed planner routes queries to the right groups; cross-shard queries fan out and merge.

**What it buys:**
- True horizontal scale. Working set bigger than one machine is now supported.
- Can combine with B for the HA story.

**What it doesn't buy:**
- Anything, really, that matters before you hit a one-box ceiling.

**Effort:** **12+ months.** This is where the architecture stops being recognizable as current-PowDB:
- Planner must become distribution-aware → kills the "planner is pure, no catalog access" invariant.
- Executor must ship plan fragments and partial results between nodes → needs a distributed execution protocol (Arrow Flight / custom).
- Transactions spanning shards need 2PC or deterministic ordering (Calvin-style) or optimistic concurrency with retry (Cockroach-style). Each has a bespoke implementation cost measured in quarters, not weeks.
- Range split/merge, rebalancing, hot-range mitigation. An entire subsystem.
- Cross-shard joins: either execute locally after shipping data (bad for big joins), or implement distributed join algorithms (very hard).

**Risks:**
- You effectively rewrite the query engine. Every existing fast path (`Filter(SeqScan)`, mmap scans, compiled predicates) has to be re-proved in a distributed context.
- The benchmarks stop being comparable to SQLite — you're now competing with Cockroach, Yugabyte, TiDB. Different positioning, different table stakes.

**Who picks this:** "My *working set* doesn't fit on one machine" — terabyte-plus actively-queried data. Most startups never hit this in their lifetime.

**Recommendation: do not pursue C unless PMF is proven, a paying customer explicitly asks for it, and the single-node-plus-read-replica path is provably insufficient.** It is the most tempting-sounding feature on this list and by far the least likely to be the right use of the next 18 months.

## Suggested sequencing

```
Phase 1 (now):      D — WAL shipping / PITR / cold DR replica
Phase 2 (when users have >1 server): A — online read replicas
Phase 3 (when users start asking for HA/SLA): B — single-group Raft
Phase 4 (only if truly needed): C — sharding
```

Ship D and move on. Don't architect for C today.

---

# Part 2: Multi-tenancy, ACLs, row-level security

Today: one shared password; any connected client can do anything. Four options, two of which should probably land together.

## Option α: Users + per-table grants (Postgres-style)

**What:** Add user accounts and tables. Connect auth becomes user/password. Grants attach permissions (select/insert/update/delete) to (user × table) pairs. Executor checks the grant before running each statement.

**Effort:** **4–8 weeks.**
- New system catalog types: `__User`, `__Grant` (or similar).
- Handshake message gets a username field (wire protocol bump).
- Password hashing: Argon2id. Don't roll your own.
- `create user`, `alter user`, `drop user`, `grant`, `revoke` DDL.
- Permission check in the executor — one hook at plan-root level, plus one per target table in multi-table statements.

**What it buys:**
- Separation of duties within a team (reporting read-only user, app write-user, migration admin)
- Audit trail potential (log `current_user` per query)
- Per-role password rotation

**What it doesn't buy:**
- Tenant isolation (see β)
- Per-row access (see γ)
- Column-level sensitivity (see δ)

**Risks:** Small. Well-understood problem. The main one: PowQL has no GRANT today, so all existing scripts will need compatibility shimming (or: gate behind a `--auth` server flag so existing users are unaffected until they enable it).

## Option β: Database-per-tenant (near-free hard isolation)

**What:** `dbName` in the connect handshake already exists but doesn't meaningfully isolate today (needs verification — `crates/server/src/handler.rs`). Make it real: per-dbName storage directory, per-dbName catalog, per-dbName WAL. Users bind to one db at connect time.

**Effort:** **2–4 weeks** (dramatically less if the routing already exists and just needs wiring).
- Make the storage engine instantiate per dbName.
- Validate dbName aggressively (regex + reserved names) to prevent path traversal.
- `create database`, `drop database` DDL.
- Per-db backup/restore becomes trivial: it's a directory.

**What it buys:**
- **Hard tenant isolation.** Two tenants cannot, by any PowQL query, see each other's data. Bugs in predicate logic are not a cross-tenant leak because the data isn't even in the same catalog.
- Per-tenant backup/restore/delete/rename is a directory-level op.
- Per-tenant resource accounting (disk, WAL size) falls out for free.

**What it doesn't buy:**
- Cross-tenant analytical queries ("which tenants are over 1GB of data") — need a separate admin-side tool or a special admin connection that can iterate databases.
- Shared reference tables (currencies, country codes). Either duplicate them per tenant or add a read-only "system" db.

**Risks:**
- **Connection overhead.** If you have 10,000 tenants and each has a long-idle connection, you're holding 10,000 file handles. Mitigation: idle connection timeout + client-side pool. This is a known Postgres operational issue; solutions like pgbouncer exist there, will need a similar story here eventually.
- **Table proliferation / inode pressure** at large tenant counts. Probably fine up to tens of thousands of tenants; becomes a thing at hundreds of thousands.

## Option γ: Row-level security (RLS)

**What:** Attach policies to types. A policy is a predicate evaluated against row + session context. Planner/executor injects the predicate into every query touching the table. Example:

```powql
policy User visible_to_tenant:
  session.tenant_id = .tenant_id

alter User enable rls
```

**Effort:** **3–6 months.**
- Session variables (server state per connection, settable by client or admin).
- Policy DSL and parser.
- Planner rewriting — every `User ...` becomes `User filter (policy_pred) ...`.
- Executor needs access to session context during predicate eval.
- `bypass_rls` grant for admin users.
- Correctness testing — RLS bypass is a classic CVE generator (`SECURITY DEFINER` confusion, optimizer leaking rows via error messages, side channels through cost-based plan shapes).

**What it buys:**
- Shared-schema multi-tenancy. 10,000 tenants share one `User` table, one set of indexes. Better for small tenants (amortizes per-tenant overhead).
- Fine-grained rules that go beyond tenancy (row-level audit, per-user "only your own records").
- Cross-tenant admin queries trivially possible (admin has `bypass_rls`).

**What it doesn't buy:**
- Hard isolation. A bug in the planner or predicate compiler can leak across tenants. You will want a test harness that fuzzes policy enforcement.
- The simplicity of β.

**Risks (non-trivial):**
- **Performance cost.** Every scan now evaluates at least one extra predicate. With an index on the tenant key you get `IndexSeek(tenant_id = X)` — cheap. Without one, you get `SeqScan + Filter` — can be a 100× regression. You must warn loudly at `enable rls` time if the policy's base column isn't indexed.
- **Planner purity.** The planner currently doesn't access the catalog. RLS requires the planner (or a pre-planner pass) to know which policies apply — that's catalog access. This is a real architectural change and should be designed deliberately.
- **Security footguns.** Error messages that leak row values, optimizer behavior that leaks cardinality, side channels through performance. See the literature on "inference attacks against RLS" before shipping.

**Who picks this:** SaaS with lots of tiny tenants where β's overhead is real. Also: apps with "share with user X" / "this row is mine" semantics that aren't tenant-based.

## Option δ: Column-level grants

**What:** Grant SELECT not just on `User` but on `User.name`. Deny on `User.ssn`.

**Effort:** **4–6 weeks on top of α.** Needs catalog support for per-column grants, and the planner must reject or strip projections a user can't see.

**What it buys:** Intra-team data-sensitivity separation (analysts can see names, not PII). Useful but niche.

**Risk:** Query rewrites to strip unauthorized columns can be surprising (`select *` on a table you don't have full access to — does it error, or silently skip columns?). Postgres errors; MySQL used to silently skip. Pick a side and document it.

## Suggested sequencing

```
Phase 1 (now):     α — users + table grants
                   β — per-database isolation (ship in parallel; they don't conflict)
Phase 2 (later):   δ — column grants (only if paying customers ask)
Phase 3 (maybe):   γ — RLS (only for shared-schema multi-tenancy needs β's overhead
                         genuinely can't absorb)
```

α + β is a 1-3 month push that delivers 95% of what real customers ask for under the label "multi-tenancy." RLS is tempting and shiny. It's also the option where subtle bugs are the most expensive. Earn it.

---

# Interaction between the two halves

Some combinations are natural, some are bad:

- **α (users) + β (db-per-tenant)** — clean. One or more users per database, passwords scoped to the database.
- **β + D (WAL shipping)** — straightforward. Ship each tenant's WAL independently; per-tenant PITR falls out.
- **β + B (Raft)** — interesting. Each database could be its own Raft group — lightweight per-tenant HA, and a natural sharding boundary if C ever happens.
- **γ (RLS) + C (sharding)** — avoid. RLS on sharded data means the predicate must be applied on every node before results merge; easy to get wrong. If you're ever doing both, prove the design under adversarial testing before shipping.

## One-page summary

| Goal | Cheapest option | Order of magnitude effort |
|---|---|---|
| Backup + DR | D (WAL shipping) | weeks |
| Read scale | A (read replicas) | ~1 month on top of D |
| Automated failover / HA | B (Raft) | ~3-6 months |
| Data > 1 box | C (sharding) | ~year+, defer |
| Internal role separation | α (users + grants) | ~1-2 months |
| Hard tenant isolation | β (db-per-tenant) | weeks |
| Shared-schema multi-tenancy | γ (RLS) | ~3-6 months |
| Column-level sensitivity | δ (column grants) | ~1 month on top of α |

**If I were betting your next quarter on one direction:** D + A + (α + β) in that order. It gives backup story, read scale, role separation, and hard tenant isolation — four of the five most common customer asks — in roughly one calendar quarter of focused work, without breaking any of the five constraints at the top of this doc.

Save B for when a customer is actually signing a contract that requires it. Save C and γ for a world where you're sure you need them.
