# PowDB Write-Performance Design Spec

- **Date:** 2026-06-27
- **Status:** Proposed
- **Author:** the ORM integration (cross-engine benchmark findings)
- **Scope:** Write-path latency & throughput. Reads are already competitive and are out of scope except where noted (§3.7).

---

## 1. Motivation

While building the ORM integration's PowDB backend, a cross-engine benchmark (same ORM-realistic
workload run through the ORM integration against Postgres / SQLite / MySQL / PowDB) showed PowDB's **reads
rivalling Postgres but its writes ~40× slower**. This spec roots-causes that gap in the PowDB
source and proposes fixes, prioritised.

### 1.1 Benchmark evidence (p50 latency, ms — lower is better)

| operation | Postgres | SQLite | MySQL | **PowDB** |
|---|--:|--:|--:|--:|
| findUnique by PK | 0.077 | 0.006 | 0.107 | **0.086** ✅ |
| findMany filter+order+limit | 0.143 | 0.086 | 0.395 | **0.204** ✅ |
| nested read (N+1 loaders) | 0.401 | 0.363 | 0.846 | **0.435** ✅ |
| create (single insert) | 0.089 | 0.015 | 0.249 | **4.041** ❌ |
| createMany (100 rows) | 0.435 | 1.067 | 0.743 | **18.105** ❌ |
| update (atomic increment) | 0.086 | 0.011 | 0.220 | **4.049** ❌ |

Reads are fine. **Single-row writes are the problem: ~4 ms each, ~250 ops/s.**

### 1.2 Isolating the engine from the ORM

To rule out ORM overhead (the ORM integration does a reselect after each write because PowDB has no
`RETURNING`), the same writes were measured with the **raw `@zvndev/powdb-client`**, no ORM:

```
raw INSERT (autocommit)   p50 = 4.048 ms   (~252 ops/s)
raw UPDATE (autocommit)   p50 = 3.994 ms   (~257 ops/s)
raw SELECT by pk          p50 = 0.070 ms   (~13,500 ops/s)
```

The reselect adds only ~0.07 ms. **The ~4 ms is entirely the engine's commit path**, and it is
~50× the in-memory insert cost (issue #57 puts `insert_single` at ~77 µs). The missing ~3.9 ms is
a durable `fsync`.

---

## 2. Root cause (grounded in the source)

### 2.1 Every autocommit statement fsyncs

`crates/query/src/executor/mod.rs:508` — after a non-transactional mutation the executor calls
`commit_autocommit()` once per statement:

```rust
if !self.in_transaction {
    self.catalog.commit_autocommit()?;   // -> Wal::flush() -> sync_data()
}
```

`crates/storage/src/wal.rs:280-300` — `flush()` is the group-commit point and, in the default
sync mode, issues a real `fsync`:

```rust
pub fn flush(&mut self) -> io::Result<()> {
    ...
    writer.flush()?;
    if matches!(self.sync_mode, WalSyncMode::Full) {
        writer.get_ref().sync_data()?;   // <- ~3.9 ms per single-row autocommit write
    }
    ...
}
```

So a single-row autocommit `INSERT`/`UPDATE` = 1 WAL record + 1 `fsync`. The `batch_size`
group-commit logic (`wal.rs:268-271`) only coalesces records **within one statement / one explicit
transaction** — it never helps the common case of many independent autocommit writes.

### 2.2 The fsync is serialized under the global write lock

`crates/server/src/handler.rs:369-372` — mutations take the exclusive engine lock and run the whole
statement (including the commit fsync) while holding it:

```rust
let mut eng = engine.write()?;   // exclusive Arc<RwLock<Engine>>
eng.execute_powql(query)          // mutate + commit_autocommit() + fsync, all under .write()
```

Consequence: **no two writers can fsync concurrently.** Total write throughput across *all*
connections is capped at `1 / fsync ≈ 250 writes/s`, no matter how many clients connect. A
write-heavy app cannot scale writes by adding concurrency.

### 2.3 Durability is hardcoded and binary

`crates/storage/src/wal.rs:111-116` — `WalSyncMode` has only two values and defaults to `Full`:

```rust
pub enum WalSyncMode { #[default] Full, Off }
```

- `Full` = fsync every commit (safe; the 4 ms).
- `Off` = no CRC, no fsync, no recovery ("never use in production"; bench-only).

There is **no middle ground** (no SQLite-`NORMAL`, no Postgres `synchronous_commit=off`), and
`set_sync_mode` is **never wired to any server/CLI/session config** (grep of `crates/server`,
`crates/cli` finds no caller). Operators cannot trade durability for speed even when their workload
allows it.

### 2.4 Secondary contributors

- **No `RETURNING`** → clients must reselect after every write (extra round-trip; cheap here but
  doubles write RTTs and is pure waste under network latency).
- **No server-generated IDs** → clients must mint PKs (the ORM integration generates UUIDs), pushing key
  management to the app.
- **Bulk insert overhead** — `createMany` 100 rows = 18 ms = ~0.18 ms/row, ~2.3× the ~0.077 ms
  in-memory insert. Relates to the `insert_single` regression in issue #57 (77 µs vs 3.6 µs).
- **Nested reads degrade to N+1** — no JSON aggregation / server-side join, so an ORM cannot do
  single-query nested fetches (fine at this scale; matters at depth/volume).

---

## 3. Proposed improvements

Each item: **what · why · expected impact · effort/risk.** Ordered by impact-per-effort.

### 3.1 [P0] Configurable durability + an async/`NORMAL` commit mode

**What.** Add a third `WalSyncMode` (call it `Normal`/`Async`): a commit is acknowledged once its
WAL record is in the OS buffer (`BufWriter::flush`, no `fsync`); a **background flusher** fsyncs on
a fixed cadence (e.g. every 5–25 ms) or every K records. On crash you lose only the unsynced tail
(bounded, like Postgres `synchronous_commit=off` / SQLite `NORMAL`). Wire all three modes to
server config, and ideally to a per-session `SET`/`PRAGMA`.

**Why.** Removes the per-write fsync from the latency path entirely for workloads that tolerate a
small, bounded loss window — which is most app workloads.

**Impact.** Single autocommit write **4 ms → ~0.1–0.3 ms** (~15–40×). `create` ~250 ops/s →
~5–10 k ops/s, landing PowDB next to Postgres.

**Effort/risk.** Medium. The flusher must guarantee the bounded-loss contract and interact correctly
with `checkpoint()` and recovery. `WalSyncMode` plumbing already exists — most of the work is the
background flusher + config surface. **Default stays `Full`** (no silent durability downgrade).

### 3.2 [P0] Real group commit (amortise fsync across concurrent writers)

**What.** Decouple `fsync` from per-statement, per-lock execution. Writers append their WAL record,
then **park on a shared "commit batch"**; one flusher fsyncs the batch and wakes all parked
committers. This is the classic group-commit / commit-coalescing design (PostgreSQL `commit_delay`,
MySQL binlog group commit, FoundationDB).

**Why.** Even in `Full` (fully durable) mode, N concurrent committers should share ~1 fsync instead
of paying N serial fsyncs.

**Impact.** In `Full` mode, durable-write **throughput scales with concurrency** instead of being
pinned at ~250/s: 16 concurrent writers ≈ one fsync per batch ≈ ~10–16× aggregate throughput.
Single-writer latency is unchanged (still one fsync) — this is a throughput fix; §3.1 is the latency
fix. They compose.

**Effort/risk.** Medium-high. Needs a commit queue + condvar/notify and careful LSN/ack ordering.
Pairs naturally with §3.3.

### 3.3 [P0] Move `fsync` out of the exclusive write lock

**What.** Under `engine.write()`, do only the in-memory mutation + WAL **append** (~tens of µs);
release the lock; perform `flush()/fsync` **outside** the exclusive section (feeding the §3.2 group
committer).

**Why.** Today the ~4 ms fsync is held inside `.write()` (§2.2), so the lock is held ~50× longer
than the actual data mutation. Releasing before fsync lets the next writer apply its in-memory
mutation while the previous commit is still syncing — pipelining the cheap part.

**Impact.** Unblocks §3.2; raises write concurrency markedly even before async mode. Must preserve
the invariant that a commit is only ack'd to the client after its WAL record is durable (in `Full`).

**Effort/risk.** Medium-high — it changes the locking/commit contract; needs care that visibility and
durability ordering stay correct (a reader must not see a write that isn't yet durable in `Full`, or
define the visibility model explicitly).

### 3.4 [P1] `RETURNING` (and/or `OUTPUT`)

**What.** Let `insert`/`update`/`upsert`/`delete` return the affected rows.

**Why.** Eliminates the client reselect (the ORM integration currently issues a follow-up `SELECT` after every
write because there's no way to get the row back). Saves a full round-trip per write.

**Impact.** −1 round-trip per write; lets the ORM integration drop the reselect strategy. Bigger win over a
network than on loopback.

**Effort/risk.** Medium (PowQL surface + executor returns rows it already has in hand).

### 3.5 [P1] Server-generated IDs / column `DEFAULT`s

**What.** Auto-increment/identity columns and/or `DEFAULT` expressions (sequence, `uuid()`), so the
PK doesn't have to be client-supplied.

**Why.** Removes app-side key management; with §3.4 enables the natural "insert, get id back"
pattern. (the ORM integration currently mints client-side UUIDs as a workaround.)

**Effort/risk.** Medium (sequence/identity allocation must be crash-safe & WAL-logged).

### 3.6 [P1] Bulk-ingest path

**What.** Make `createMany`/multi-row insert a first-class bulk path: one WAL batch (verify it
already coalesces), deferred/bulk index maintenance, and ideally a COPY-style streaming ingest.
Also resolve the `insert_single` regression tracked in #57.

**Why.** `createMany` is 0.18 ms/row vs 0.077 ms in-memory — ~2.3× overhead per row, and it's the
recommended way to write a lot on PowDB (it already amortises the single fsync).

**Impact.** Faster seeds/imports; better amortised write throughput.

**Effort/risk.** Medium.

### 3.7 [P2] Server-side joins / JSON aggregation (nested reads)

**What.** A way to fetch related rows in one statement (server-side join or JSON aggregation).

**Why.** Lets ORMs do single-query nested reads instead of N+1 (D round-trips for depth D). Reads
are already fast, so this is a scale/depth concern, not a latency emergency.

**Effort/risk.** High; lower priority than the write items.

### 3.8 [P2] Investigate macOS fsync (`F_FULLFSYNC`) cost

**What.** The observed ~3.9 ms/fsync is high and consistent with macOS's full-barrier fsync. Confirm
whether `sync_data()` resolves to `F_FULLFSYNC` here, and benchmark fsync cost on Linux/NVMe vs
macOS so the §3.1/§3.2 targets are calibrated to the real per-platform barrier cost. Optionally
expose a barrier knob for devices with power-loss protection.

**Effort/risk.** Low (measurement); informs the others.

---

## 4. Expected end state

With **§3.1 + §3.2 + §3.3** (the P0 set):

| | today | after P0 |
|---|--:|--:|
| single autocommit write (NORMAL durability) | ~4.0 ms | ~0.1–0.3 ms |
| single autocommit write (FULL durability, 1 client) | ~4.0 ms | ~4.0 ms (1 fsync) |
| durable write throughput, 16 concurrent clients | ~250/s (lock-serialised) | ~2–4 k/s (group commit) |
| `create` ops/s (the ORM integration, NORMAL) | ~250 | ~5–10 k |

This closes the bulk of the 40× gap and makes PowDB's writes competitive with Postgres, while
keeping `Full` durability the default and correct.

---

## 5. Risks & non-goals

- **Durability is sacred.** `Full` stays the default; async/`NORMAL` is opt-in with a clearly
  documented, bounded loss window. Crash-recovery (`read_all`/`checkpoint`) must be re-validated for
  every mode, especially the lock/fsync reordering in §3.3.
- **Not** about reads (already competitive) or about MVCC/row-level locking (a much larger effort —
  group commit + finer fsync handling gets most of the win first).

## 6. Validation

Re-run a cross-engine ORM benchmark after each P0
item. Targets: `create`/`update` p50 < 0.3 ms in NORMAL mode; durable-write throughput scaling with
client concurrency in FULL mode. Add a concurrent-writer throughput bench (current suite is
single-connection and therefore *understates* the lock-serialisation problem).

---

## Appendix — measurement method

- Cross-engine ORM harness: 5 orgs / 100 users / 1 000 posts / 5 000 comments; one connection;
  warmup 30 + 200 measured iterations; p50/p95/p99. Apple-Silicon mac, local loopback, PowDB 0.6.2.
- Raw-client numbers: `@zvndev/powdb-client` 0.6.1, autocommit single statements, 200 iterations.

> Separate from this perf work, two correctness bugs on the fast UPDATE path were filed: #117
> (type-mismatched param panics the server) and #118 (int→float UPDATE stores raw i64 bits). Those
> are being addressed on `fix/update-fast-path-coerce-value-type`.
