# PowDB Write-Performance — Implementation Plan

- **Date:** 2026-06-27
- **Status:** Approved 2026-06-27 — executing. §7 decisions locked.
- **Companion to:** [`2026-06-27-write-performance-design.md`](./2026-06-27-write-performance-design.md) (cross-engine ORM findings)
- **Scope:** Turn the verified write-path diagnosis into a sequenced, testable implementation. Reads are competitive and out of scope except §Phase 4.

---

## 1. TL;DR

The ORM integration's benchmark found PowDB writes ~40× slower than Postgres (~4 ms/write, ~250 ops/s) while reads are competitive. I **independently verified the root cause in the source** (§2): every autocommit statement does a real `fsync`, and that `fsync` runs **inside the exclusive engine write lock**, so all writers serialize on it. Durability is also hardcoded binary (`Full`/`Off`) with no safe middle mode, and the existing `set_sync_mode` knob is never wired to config.

The plan is three composable P0 changes — (1) a config-wired async/`NORMAL` durability mode, (2) real group commit, (3) move fsync out of the write lock — landing single-write latency at ~0.1–0.3 ms and making durable throughput scale with concurrency, **while keeping `Full` the default**. P1/P2 add `RETURNING`, server-generated IDs, a bulk path, and server-side nested reads.

**Recommended delivery:** ship the already-merged #117/#118 correctness fixes as **v0.6.3** now; do the perf work as **v0.7.0** in phases, each gated by the durability suite + the Depot bench + a re-run of the ORM integration's harness.

---

## 2. Verified diagnosis

Each finding re-checked against the current `main` source (not taken on faith):

| # | Finding | Source (verified) | Status |
|---|---|---|---|
| A | Per-statement autocommit `fsync` | `executor/mod.rs:508/526/542/566` → `catalog.rs:575 commit_autocommit` → `wal.rs:280 flush` → `:294 sync_data` | ✅ confirmed |
| B | `fsync` held **inside** exclusive `engine.write()` | `handler.rs:370-372` (`.write()` then `execute_powql`, which mutates + commits + fsyncs) | ✅ confirmed |
| C | Durability binary, default `Full` | `wal.rs:112` `enum WalSyncMode { #[default] Full, Off }` | ✅ confirmed |
| D | `set_sync_mode` exists but **unwired** | defined `wal.rs:180`; **zero** references in `crates/server`, `crates/cli` | ✅ confirmed |
| E | Batch coalescing only helps within one statement | `wal.rs:268-271` auto-flush on `pending >= batch_size`; independent autocommit stmts each call `flush()` | ✅ confirmed |

**Bottleneck model:** `cost(write) ≈ in-memory mutation (~77 µs, see #57) + 1 fsync (~3.9 ms)`, and the fsync is serialized by the global write lock → aggregate write throughput ≈ `1 / fsync ≈ 250/s` regardless of client concurrency.

**Helpful pre-existing structure:** `wal.rs:flush()` *already* separates `BufWriter::flush` (always) from `sync_data()` (Full only), and recovery (`read_all` + CRC) already replays buffered-but-unsynced records. That makes a `Normal` mode a small delta, not a rewrite.

---

## 3. The fix model (how the P0s compose)

- **§Phase 1 (latency):** stop fsyncing on the commit path for workloads that opt in → single-write 4 ms → ~0.1–0.3 ms.
- **§Phase 2 (throughput, two coupled changes):** move fsync out of the lock + group-commit → in **Full** (still fully durable) mode, N concurrent writers share ~1 fsync instead of N serial fsyncs.

Phase 1 is the single-client latency win; Phase 2 is the concurrency/throughput win and keeps full durability. They are independent wins that stack.

---

## 4. Phased implementation

Every phase is **TDD (RED→GREEN)**, keeps `Full` the default, preserves the deliberate `panic = "abort"` crash-only design, and is validated by the durability suite + a crash/restart smoke + the ORM integration re-run before it's called done.

### Phase 0 — Measurement & guardrails (S, do first)
**Goal:** make the win measurable and the risk visible before touching the commit path.
- Add a **concurrent-writer throughput benchmark** to `powdb-bench` (current suite is single-connection and *understates* the lock-serialization problem). Baseline on the **Depot** runner only (never the laptop — bench baselines are CI-hardware).
- Add lightweight commit-path timing (records/fsync, fsync latency) behind the existing metrics endpoint.
- §3.8 investigation: confirm whether macOS `sync_data()` resolves to `F_FULLFSYNC` and measure fsync cost on Linux/NVMe vs macOS, so Phase 1/2 targets are calibrated.
- **Deliverable:** a reproducible "before" number for create/update p50 + concurrent throughput.

### Phase 1 — `WalSyncMode::Normal` + config wiring (M) **[P0 latency]**
**Goal:** opt-in async durability with a bounded, documented loss window.
- Add a third `WalSyncMode` variant (name TBD — see Decisions): commit acks after `BufWriter::flush` (bytes in OS cache); a **background flusher** thread fsyncs on a cadence (time and/or record count). Loss window = unsynced tail (process crash safe; OS-crash/power-loss loses only the tail — SQLite `NORMAL` / PG `synchronous_commit=off` semantics).
- **Wire `set_sync_mode` to config** (server flag + env; the knob already exists, it's just never called) and, if we choose, a per-session `PRAGMA`/`SET`.
- Flusher must interact correctly with `checkpoint()` and shutdown (drain on `SIGTERM` — there's already a SIGTERM-drain path to extend).
- **Files:** `storage/src/wal.rs` (mode + flusher), `storage/src/catalog.rs` (commit path), `server` config plumbing, `cli` flag.
- **Tests:** durability suite gains a per-mode crash+restart case proving the loss-window contract (Normal recovers everything fsynced; loses only the unsynced tail on a kill *before* the flusher runs); `smoke-release.sh` extended to run its kill -9/restart assertion in each mode.
- **Impact:** single autocommit write **4 ms → ~0.1–0.3 ms**; `create` ~250 → ~5–10 k ops/s.
- **Risk:** Medium — durability-semantics change. Mitigated by default-Full + explicit loss-window docs + per-mode recovery tests.

### Phase 2 — Group commit + fsync outside the lock (M-L, coupled) **[P0 throughput]**
**Goal:** in `Full` mode, durable throughput scales with concurrency.
- Under `engine.write()`: do in-memory mutation + WAL **append** only (~tens of µs); release the lock; perform `flush()/fsync` **outside** the exclusive section.
- Real **group commit**: concurrent committers append, park on a shared commit batch; one flusher fsyncs the batch and wakes all; ack each client only after its LSN is durable (in Full).
- **Files:** `server/src/handler.rs` (lock scope), `storage/src/wal.rs` + `catalog.rs` (commit queue / LSN ack), executor commit hook.
- **Tests:** concurrent-writer bench shows throughput rising with client count; a correctness test that a client ack in `Full` implies its record is durable (no ack-before-fsync); crash test that an un-acked in-flight write is the only thing that can be lost.
- **Impact:** 16 concurrent writers ≈ 1 fsync/batch ≈ ~10–16× aggregate; single-writer latency unchanged.
- **Risk:** Medium-High — changes the locking/commit/visibility contract. Define the visibility model explicitly (a reader must not observe a not-yet-durable write in Full, or document the chosen model). This is the phase to be most careful with.

### Phase 3 — ORM ergonomics (M each) **[P1]**
- **`RETURNING`** on insert/update/upsert/delete; executor already has the affected rows in hand; this is mostly PowQL surface + wire. Removes the ORM integration's mandatory reselect (−1 RTT/write; big over a network).
- **Server-generated IDs / column `DEFAULT`s** — identity/sequence + `DEFAULT` exprs (e.g. `uuid()`); allocation must be crash-safe + WAL-logged. Pairs with `RETURNING` for "insert, get id back."
- **Bulk-ingest path** — make `createMany` first-class (one WAL batch, deferred/bulk index maintenance, optional COPY-style stream); resolve the `insert_single` regression in **#57** (77 µs vs 3.6 µs).

### Phase 4 — Nested reads (L) **[P2]**
- Server-side join or JSON aggregation so ORMs do single-query nested fetches instead of N+1. Reads are already fast → scale/depth concern, lowest priority. Note: PowQL's "nested results" was previously identified as a real differentiation wedge — this phase overlaps that, so worth scoping together.

---

## 5. Release & sequencing

1. **v0.6.3 (now):** the merged #117/#118 fast-path coercion fixes (one a remote DoS). Independent of all perf work. *Blocked only on the one-time trusted-publisher setup from the OIDC migration (#116).*
2. **v0.7.0 (perf):** Phase 0 → Phase 1 → Phase 2, each its own PR, each behind the durability gate. This is the headline "writes are now competitive" release.
3. **v0.7.x / v0.8.0:** Phase 3 (RETURNING first — highest ORM value), then Phase 4.

Each perf PR re-runs the cross-engine ORM harness and the Depot bench; no perf claim ships without a before/after from CI-grade hardware.

---

## 6. Validation & guardrails

- **Durability is sacred.** `Full` stays default. Async/`Normal` is opt-in with a documented bounded loss window. Recovery (`read_all`/`checkpoint`) re-validated for **every** mode — especially the Phase 2 lock/fsync reordering.
- **Crash-only preserved.** `panic = "abort"` stays; the background flusher and group committer must fail loud, not corrupt.
- **Per-mode crash test.** Extend `smoke-release.sh` to run its kill -9 + restart WAL-replay assertion under each mode (this is the gate whose absence caused the v0.4.1–0.4.3 yanks).
- **Bench provenance.** Baselines only from the Depot single-tenant runner; never rebaseline from the laptop.
- **Acceptance targets:** create/update p50 < 0.3 ms in Normal; durable-write throughput rising with client concurrency in Full; zero durability-suite regressions.

---

## 7. Decisions (locked 2026-06-27)

1. **Mode name & control surface → `Normal`, with both config and per-session control.** Completes the recognizable `Full` / `Normal` / `Off` triad (SQLite vocabulary → least surprise, best docs). Wire it to server config (flag + env) in Phase 1, and add a per-session `PRAGMA sync_mode = full|normal|off` as a Phase 1 follow-on — the mode is designed for per-connection override from the start so an app can run most writes `Normal` and force `Full` on the few that must be durable-now.
2. **Default loss window → hybrid, whichever comes first: fsync every 10 ms OR every 64 records.** Both tunable via config. Bounds the crash-loss window to ~10 ms of writes — aggressive enough for the throughput target, small enough to be a respectable production default for the opt-in mode. (`Full` = fsync every commit; `Off` = never.)
3. **Release grouping → v0.6.3 now (correctness), perf as v0.7.0 in phases.** Each perf phase is its own PR behind the durability gate.
4. **`RETURNING` pulled forward → developed in parallel with the P0 perf work** (disjoint code areas: PowQL surface + executor vs WAL/lock), shipping in v0.7.0. It's the highest-value ORM ergonomic and unblocks the ORM integration's reselect removal.
5. **MVCC / row-level locking → confirmed non-goal** for this effort. Group commit + finer-grained fsync first; revisit MVCC only if a concrete need survives these wins.

### Rationale (the "best DB possible" lens)
The P0 set is the 40× write win and keeps `Full` the safe default — that's the production-readiness bar. `Normal` + group commit together give both single-write latency *and* concurrent throughput, which is what makes the engine feel "epic" under real app load. `RETURNING` + server IDs remove the per-write round-trip tax that ORMs pay. Everything is gated by the durability suite + a per-mode crash test + Depot benches, so speed never comes at the cost of the data-loss guarantees that define a database.

---

> Tracking issues created per phase (see the §5 release plan). Phase 0 + Phase 1 are in flight; both design docs are committed under `docs/design/`.
