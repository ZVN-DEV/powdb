# PowDB — Beating SQLite: Latency & Transport Design Spec

- **Date:** 2026-06-27
- **Status:** Proposed (follow-up to the write-performance spec, after #121 landed)
- **Author:** Turbine ORM cross-engine benchmark findings
- **Goal under test:** *PowDB should be faster than SQLite.* This spec measures how far off that is after the write-perf work, finds why, and proposes what closes it.

---

## 1. Where things stand after #121

#121 (`WalSyncMode::Normal` + off-lock flusher) made writes **~17–21× faster** (single write 4 ms → ~0.2 ms) — a huge win, and it closed most of the *Postgres* gap. But the bar is **SQLite**, and against SQLite PowDB is still slower on **every** operation:

| op (p50, ms) | **SQLite** | PowDB #121 Normal | PowDB vs SQLite |
|---|--:|--:|--:|
| findUnique by PK | 0.006 | 0.086 | **14× slower** |
| findMany filter+order+limit | 0.086 | 0.204 | 2.4× slower |
| nested read | 0.363 | 0.435 | 1.2× slower |
| create (single) | 0.015 | 0.233 | 15× slower |
| createMany (100) | 1.067 | 4.683 | 4.4× slower |
| update (increment) | 0.011 | 0.190 | 17× slower |

`createMany` is even slower than Postgres (0.435 ms). So there is real work left.

---

## 2. Root cause: it's the transport, not the engine

Measured the per-op **floor** with the raw client against a 1-row table — both a trivial aggregate and a PK lookup land at the same number:

```
count(FL)        p50 = 0.0456 ms     (trivial engine work)
select by pk     p50 = 0.0411 ms     (indexed 1-row read)
```

They're equal ⇒ **the engine work is negligible; ~0.04 ms is pure client↔server round-trip** (TCP loopback + protocol encode/decode + syscalls + context switches).

**This is the whole story.** SQLite's *entire* findUnique is **0.006 ms — 7× below PowDB's wire floor.** SQLite is an in-process library: a function call, zero syscalls, zero serialization. PowDB is a TCP server: every op pays the round-trip no matter how fast the engine is.

> **A networked server cannot beat an in-process library on single-op latency.** The floor isn't the B-tree or the WAL — it's the socket. Tuning the engine further won't move single-op latency below ~0.04 ms while the wire is in the path.

Confirmed gaps in the current transport (`crates/server`, client):
- **TCP only** — no Unix domain socket (the same-host/"embedded" case still pays the full TCP/IP stack).
- **No pipelining** — strictly request→response; a client can't put N queries on the wire in one flush. (N+1 nested reads therefore cost N round-trips.)
- **No in-process / embedded mode** — `Engine` exists (`powdb_query::executor::Engine`, wrapped by the server as `Arc<RwLock<Engine>>`) but is not exposed as a linkable library / FFI / native addon. "embedded-database" is only a CLI keyword.
- **Text wire format** — every value crosses the wire as a string (`rows: string[][]`); ints/floats/bools/dates are stringified and re-parsed on both ends.

---

## 3. Which "faster than SQLite" claims are winnable

There are four latency regimes. Be explicit about which one the goal means:

| regime | vs SQLite | winnable? | how |
|---|---|---|---|
| **Networked single-op latency** | in-process, 0.006 ms | ❌ structurally not | wire round-trip (~0.04 ms) > SQLite's whole query |
| **Embedded single-op latency** | in-process | ✅ yes | run the engine in-process (no socket) — §4.1 |
| **Concurrent write throughput** | single-writer file lock | ✅ yes | SQLite serializes all writers; group commit + real concurrency wins — §4.4 |
| **Bulk writes** | row-by-row, its weak spot | ✅ yes | one-statement bulk + RETURNING + #57 fix — §4.3 |

**Takeaway:** "faster than SQLite" is achievable in 3 of 4 regimes — but **not** networked single-op latency. Either build an embedded mode (§4.1), or position the claim on embedded / concurrency / bulk and stop benchmarking networked single-op against an in-process library (apples-to-oranges).

---

## 4. Suggestions (prioritised, each grounded)

### 4.1 [P0 — the headline] In-process / embedded mode

**What.** Expose the engine to run *inside the application process*, no socket:
- a clean **Rust library API** over `Engine` (it already exists behind the server's `RwLock`);
- a **C ABI / `cdylib`** for FFI;
- a **Node native addon** (`napi-rs`/`neon`) so JS apps (and Turbine) call the engine directly.

**Why.** This is the *only* way to beat SQLite on single-op latency — it deletes the ~0.04 ms wire floor. With the round-trip gone, single-op latency drops to roughly the measured engine work (~0.005–0.01 ms), i.e. **parity with or better than SQLite's 0.006 ms**, while keeping PowDB's real storage engine, indexes, and (with Phase 2) concurrency.

**Impact (estimated).** findUnique 0.086 → ~0.008 ms (~10×); create (Normal) 0.233 → ~0.02–0.05 ms. Brings every op to SQLite's neighborhood or below.

**Effort/risk.** Medium-High but mostly *surface*, not engine: the `Engine` is already encapsulated; the work is a safe public API, a binding layer (napi), lifecycle/threading (the `RwLock` model maps fine to an in-proc single handle), and packaging. Biggest care: panic-safety across the FFI boundary (the engine is `panic = "abort"` crash-only — an embedded host can't abort the whole app), so wrap calls and return errors, never unwind/abort across FFI.

**Turbine fit.** Turbine already abstracts the driver behind `PgCompatPool`; a `PowdbEmbeddedPool` over the native addon drops in with no API change — same `turbinePowDB(...)`, just an in-process handle instead of a host/port.

### 4.2 [P0/P1] If/while networked: attack the round-trip

- **Unix domain sockets** for same-host clients. No TCP/IP stack, no loopback checksums — typically ~2× lower RTT than TCP loopback, dropping the floor from ~0.04 ms to ~0.02 ms. Low effort (tokio `UnixListener`; client gets a `path` option). The common "embedded-ish" deployment is same-host, so this helps the majority case.
- **Pipelining / request batching.** Let a client send N queries in one flush and receive N replies (the Postgres extended-protocol pipeline model — which Turbine *already* implements in `pipeline.ts`/`pipeline-submittable.ts`). Amortizes the round-trip across many ops: an N+1 nested read collapses from N round-trips to ~1; bulk ops stop paying per-statement RTT. High leverage for §4.5 and createMany.
- **Binary wire protocol.** Replace `string[][]` with a typed/columnar binary encoding so ints/floats/bools/dates aren't stringified+reparsed. Cuts CPU on both ends; the win scales with result size (big for bulk reads / wide rows).

### 4.3 [P1] createMany — beat SQLite's weak spot

createMany is SQLite's **weakest** op (1.067 ms, row-by-row) — the easiest place to actually pass it. Today PowDB is 4.4× *slower*. Causes and fixes:
- **Turbine reselect** (no `RETURNING`) adds a whole second round-trip + an `IN (100)` scan. → **`RETURNING`** (already in flight on `feat/returning-insert`) removes it.
- **Per-row insert cost** is ~0.047 ms/row (4.68 ms / 100) — ~13× the 3.6 µs target in **#57**. → fix the `insert_single` regression and add a **bulk/COPY ingest path** (one WAL batch — already coalesced in Normal mode — plus deferred/bulk index maintenance).
- **Projected:** 3.6 µs/row × 100 = 0.36 ms + no reselect ⇒ **~0.4 ms, beating SQLite's 1.067 ms.** A concrete, winnable target.

### 4.4 [P1] Concurrent throughput — where a server *should* win

SQLite allows one writer at a time (database-level write lock) and readers block writers without WAL. PowDB, once **Phase 2 (group commit + fsync-outside-the-lock)** from the write-perf spec lands, can fsync-batch many concurrent committers and let readers run lock-free. **This is the dimension where "faster than SQLite" is most defensible** — measure it: the concurrent-writer bench (write-perf Phase 0) should show PowDB's aggregate write throughput rising with client count while SQLite's stays flat. Lead the GTM claim with this, not single-op latency.

### 4.5 [P1] Nested reads — collapse the N+1

PowDB has no JSON aggregation / server-side join, so an ORM does N+1 loader round-trips (1.2× SQLite today, will worsen with depth/scale). Two paths:
- **Pipelining (§4.2)** turns the N+1 loaders into ~1 round-trip — cheapest fix, no new query semantics.
- **Server-side join / JSON aggregation** — one statement returns the nested shape (what SQLite does in-process). Bigger lift; better asymptotically.

---

## 5. Recommended order

1. **Unix sockets** (§4.2) — small, helps every same-host op now (~2× on the floor).
2. **`RETURNING`** (§4.3, in flight) + **#57 insert fix** — makes createMany beat SQLite.
3. **Pipelining** (§4.2/§4.5) — collapses N+1 and bulk RTT.
4. **Embedded mode** (§4.1) — the headline; the only way to win networked-style single-op latency, and the natural "SQLite replacement" story.
5. **Concurrent-throughput bench + positioning** (§4.4) — claim the win where it's real.

## 6. Honest positioning

Don't benchmark a TCP server's single-op latency against an in-process SQLite and expect to win — that's a category error. PowDB beats SQLite **embedded** (no wire), on **concurrent write throughput** (no single-writer lock), and on **bulk writes** (SQLite's weak path). Pick those fights; build the embedded mode to own the first one outright.

---

## Appendix — measurement

- Cross-engine harness: `turbine-orm/benchmarks/cross-engine.ts`, 6,105-row seed, single connection, p50 over 150–200 iters, Apple-Silicon mac, local loopback. SQLite = `node:sqlite` (in-process). PowDB = #121 binary, `POWDB_SYNC_MODE=normal`.
- Transport floor: raw `@zvndev/powdb-client`, 2,000 iters, 1-row table — `count` and PK-select both ≈ 0.04 ms ⇒ floor is the round-trip, not the engine.
