# Deployment + Sync Strategy

Date: 2026-06-05
Version baseline: PowDB v0.4.4 (live on crates.io)
Scope: how to *deploy* PowDB into real applications, and what a *sync engine* would actually require. Covers two ideas explicitly: (1) embed PowDB in native apps with a built-in bulk-sync process (local-first / offline-capable pattern), and (2) embed PowDB directly into a backend server "in an interesting way." This is a planning/research deliverable — **no engine code changes are proposed here, only designed.**

Companion doc: [`distributed-and-tenancy-roadmap.md`](./distributed-and-tenancy-roadmap.md) covers server-to-server replication, Raft, sharding, and multi-tenancy. This doc covers the *client/embedding* side and the local-first sync engine. They share one asset — the LSN'd WAL — and the recommendations are designed to compose.

---

## Executive summary

- **PowDB already is the hard part of an embedded database**: an in-process Rust `Engine` (`powdb_query::executor::Engine`) with a real, crash-safe, LSN-stamped page-redo WAL (`crates/storage/src/wal.rs`) and checkpoint-on-shutdown. The two deployment ideas are mostly about *exposing* and *extending* what exists, not rebuilding it.
- **Idea 2 (embed in a Rust backend) is a ship-this-week story** and is the single highest-leverage move: co-locate the `Engine` in the API server, link it directly, and the ~170 ms-per-query remote-tunnel latency we measured collapses to sub-ms because there is no network. This requires **zero new engine primitives** — it is documentation, a thread-safety contract, and an example. Non-Rust backends need an FFI/WASM surface (M effort) that does not exist today.
- **Idea 1 (local-first sync) is a real-project, not a weekend.** PowDB's WAL is *physical page-redo*, which maps almost perfectly onto the **Turso/libSQL "embedded replica" model** (ship WAL frames, server is source of truth, rebase local writes). It maps *poorly* onto the logical-CRDT model (RxDB / cr-sqlite / SQLite session) because PowDB has **no logical row-level change log and no per-row version metadata today** — that is the main thing sync forces PowDB to grow.
- **Recommended sync engine to build first: server-authoritative WAL-frame replication with client-side rebase ("PowSync v0"),** modeled on Turso. It reuses the existing LSN'd WAL almost verbatim, needs no CRDT theory, and gives offline reads + single-writer-wins offline writes. Multi-master CRDT sync is explicitly deferred to research-grade.
- **Do not build:** multi-master CRDT merge, a logical CDC stream with vector clocks, or anything that requires the planner to become distribution/policy-aware. Those break load-bearing invariants (planner purity, mmap scans) for a payoff most deployments never need.

---

## Part 0: What PowDB actually has today (ground truth)

Everything below is verified against the v0.4.4 source, not assumed. The whole strategy stands on these five facts.

| Primitive | Where | What it gives us | What it does NOT give us |
|---|---|---|---|
| Embeddable `Engine` | `crates/query/src/executor/mod.rs:252` (`pub struct Engine`), `execute_powql`, `execute_powql_readonly`, `catalog()/catalog_mut()` | In-process, sub-ms, no network. **This is the embedded DB.** | Any `&mut`/`&` concurrency story of its own — callers wrap it in `Arc<RwLock<…>>` (the server does exactly this in `main.rs:289`). |
| LSN'd page-redo WAL | `crates/storage/src/wal.rs` | Append-only, monotonic `lsn`, CRC'd records, `Insert/Update/Delete/Commit/Rollback` + DDL types, idempotent **per-page** redo (page skips a record if its on-disk LSN ≥ record LSN). | A *logical* row-change feed. WAL records carry raw encoded payloads + `tx_id` + `lsn`, not `(table, primary-key, before, after, version)`. There is no public "subscribe to changes since LSN N" API. |
| Checkpoint on clean shutdown | `Engine`/`Catalog` `Drop` → `catalog.checkpoint()` (`catalog.rs:454`); `truncate()` after | Flushes heap pages, truncates WAL. Clean restart is fast. | A *retained* WAL history for replication — checkpoint **truncates** the WAL, so a replica that hasn't caught up loses its source. Sync needs the WAL (or a copy of frames) to be retained until acked. |
| TCP server, one process = one DB | `crates/server/src/{main.rs,handler.rs,protocol.rs}` | A working binary wire protocol, TLS, auth (single `POWDB_PASSWORD`), rate-limit, `Arc<RwLock<Engine>>`, read/write lock split (`dispatch_query`, `handler.rs:147`). | Multi-DB routing (the handshake `db_name` is a cosmetic label), per-user auth, request pipelining (strictly request/response). |
| `RowId` + heap addressing | `catalog.rs` (`insert → RowId`, `get/update/delete(table, rid)`) | A stable-ish physical row handle within a run. | A durable, replication-safe *logical* identity. `RowId` is a heap slot address, not a user-facing primary key, and is not guaranteed stable across compaction/rewrite. Logical sync needs a real PK. |

**The single most important consequence:** PowDB's change-capture today is *physical* (pages and frames), not *logical* (rows). That one fact decides which sync model is cheap (Turso-style frame shipping) and which is expensive (RxDB/CRDT-style row merge).

---

## Part 1: The 2026 local-first landscape — mechanisms, and which transfer

I researched eight systems. They cluster into **three mechanism families**. The only question that matters for PowDB is *what unit of change is shipped*, because PowDB already produces one of those units (frames) and produces none of the others.

### Family A — Physical frame/page shipping, server-authoritative

**Turso / libSQL embedded replicas + offline writes.** Local writes land in SQLite's WAL; on connectivity the client **pushes WAL frames** to the cloud, and **pulls remote WAL frames** to become byte-for-byte identical to the remote. The remote is the source of truth, which is *why* they use physical pages for the pull. Local writes are reconciled with one of four strategies: `FAIL_ON_CONFLICT`, `DISCARD_LOCAL`, `REBASE_LOCAL` (re-apply local frames on top of the pulled state, git-rebase style), `MANUAL_RESOLUTION`. Concurrent writers resolve as "first push wins" — last-writer-wins at the server. ([Turso offline writes](https://turso.tech/blog/introducing-offline-writes-for-turso), [embedded replicas](https://docs.turso.tech/features/embedded-replicas/introduction))

- **Unit shipped:** physical WAL frames.
- **Conflict model:** server-authoritative; local writes rebased or discarded.
- **Fit for PowDB: EXCELLENT.** PowDB's WAL *is* an LSN'd frame log with idempotent per-page redo. This is the closest analog to PowDB of anything in the landscape, by a wide margin.

### Family B — Logical row-change log + checkpoint cursor, client-resolves conflicts

**RxDB.** The cleanest blueprint. A **checkpoint** is a tiny `{ id, updatedAt }` cursor. `pull(checkpoint)` returns "all docs written after this checkpoint" + a new checkpoint. `push` sends `{ assumedMasterState, newForkState }` per doc; the server returns the real master state on any divergence and **the client resolves the conflict** and re-pushes. Deletes are tombstones (`_deleted: true`), never physical removals, so deletions replicate. Idempotent via write-id/timestamp. ([RxDB replication](https://rxdb.info/replication.html))

**SQLite session extension.** Records changes into a **changeset/patchset** keyed by primary key (a patchset stores PK + new values only). Applied on another DB via a changeset iterator with a conflict handler returning `OMIT / ABORT / REPLACE`. ([sessionintro](https://sqlite.org/sessionintro.html))

**PowerSync.** Reads the source DB's logical replication / CDC stream into a service, partitions rows into **buckets** via SQL-like **sync rules**; each client subscribes only to the buckets its parameters select (dynamic partial replication). Writes go *through your backend*, so you own conflict logic, auth, and validation. ([sync rules from first principles](https://www.powersync.com/blog/sync-rules-from-first-principles-partial-replication-to-sqlite))

- **Unit shipped:** logical row changes keyed by primary key, with a version/timestamp.
- **Conflict model:** client-side (RxDB) or backend-defined (PowerSync) or handler-callback (session).
- **Fit for PowDB: POOR today, the natural *second* step.** PowDB emits none of this: no per-row version, no tombstone table, no "changes since checkpoint" query, no stable logical PK in the change record. Every one of those is net-new.

### Family C — Multi-master CRDT merge, no central authority

**cr-sqlite (vlcn).** Runtime SQLite extension; every column becomes a CRDT (LWW register by default, also counters, fractional-index, multi-value, RGA). Merge unions rows + consults a delete log; a row is merged column-by-column. LWW order is `col_version`, then value, then `site_id` ("largest write wins"). Two offline DBs merge with no conflict and no server. ([cr-sqlite](https://github.com/vlcn-io/cr-sqlite), [column CRDTs](https://vlcn.io/docs/cr-sqlite/crdts/column-crdts))

**ElectricSQL** writes straight to Postgres and merges with CRDTs; **Realm/Atlas Device Sync** and **Ditto** are mature commercial multi-master sync engines (Ditto is peer-to-peer mesh, no server required). **RxDB** also offers P2P (WebRTC) replication. The common thread: every column/field carries CRDT metadata, and convergence is guaranteed without a coordinator.

- **Unit shipped:** CRDT deltas with per-column version vectors / Hybrid Logical Clocks.
- **Conflict model:** mathematically conflict-free merge (no "losing" write is ever rejected, but LWW columns silently drop).
- **Fit for PowDB: RESEARCH-GRADE.** This demands per-column versioning, an HLC, a delete-tombstone log, and a merge executor — effectively a second storage format alongside the heap. It also contradicts the "fewer layers = faster" thesis: every row gets fatter and every write does bookkeeping. Defer.

### One-line landscape verdict

> The cheapest faithful sync for PowDB is **Family A (Turso-style frame shipping)** because PowDB already produces the exact unit it ships. Family B is the right *evolution* once users need partial/selective sync and per-row conflict UX. Family C is a different product.

---

## Part 2: Deployment models

Each model: mechanism → what PowDB already supports → what must be built (S = days, M = weeks, L = months) → risks → verdict.

### Model 1 — Co-located embedded `Engine` in a Rust backend  ⟶ *Idea 2, primary*

**Mechanism.** The API server adds `powdb-query` as a crate dependency and holds `Arc<RwLock<Engine>>` directly (exactly what `powdb-server` already does in `main.rs:289`). Queries are in-process function calls — `engine.read().execute_powql_readonly(q)` for reads, `engine.write().execute_powql(q)` for writes (mirror `handler.rs::dispatch_query`). The WAL and heap live on the API server's disk. **No socket, no wire protocol, no serialization round trip.**

**Already supported.** Everything. This is literally how `powdb-server` works minus the TCP layer. The read/write `RwLock` split is already proven in `dispatch_query` (`handler.rs:147`): read-only PowQL takes `.read()` so concurrent scans parallelize; mutations take `.write()`.

**Must be built.**
- (S) A short "embedding PowDB" guide + a worked example crate showing the `Arc<RwLock<Engine>>` pattern, the read/write dispatch, and graceful-shutdown checkpoint (drop the `Engine`).
- (S) A documented **thread-safety contract**: `Engine` is not internally synchronized; the caller owns the `RwLock`. Make this a first-class doc, not folklore — it's the #1 footgun for an embedder.
- (S/M, optional) A thin `EngineHandle` convenience wrapper that encapsulates the lock + read/write dispatch so every embedder doesn't re-derive `dispatch_query`.

**Risks.** Low. (a) The single `RwLock` serializes all writes — fine for one process, and identical to the server today. (b) Embedders can deadlock themselves by holding a read guard and asking for a write guard; the wrapper + docs mitigate. (c) A panic inside `execute_powql` poisons the lock; the server already handles this (`lock poisoned` → error). Document the recovery.

**Verdict: SHIP-SOON.** This is the flagship deployment story and it costs documentation + an example, not engine work. It is *the* answer to the 170 ms remote-client latency we measured: don't be remote.

---

### Model 2 — FFI / WASM embedding for non-Rust backends (Node/Bun/Python/Go)  ⟶ *Idea 2, reach*

**Mechanism.** Expose the `Engine` across a language boundary so a Node/Bun/Python/Go backend gets the same in-process sub-ms calls without TCP. Two sub-targets:
- **C-ABI (`cdylib`)**: a small `extern "C"` surface (`powdb_open`, `powdb_query`, `powdb_free`, error out-params) consumed by Node-API/N-API, Bun FFI, Python `ctypes`/PyO3, or Go cgo.
- **WASM (`wasm32`)**: a `wasm-bindgen` build for in-browser / edge-runtime embedding (this is also the on-ramp to the local-first story for web apps).

**Already supported.** The core is `#![no_std]`-friendly in spirit (pure Rust, no C deps per the README), which is a *good omen* for both targets — but **neither a C-ABI crate nor a WASM target exists today.** mmap-based heap scanning (`try_for_each_row_raw`) is the chief portability question for WASM, which has no `mmap`.

**Must be built.**
- (M) A `powdb-ffi` crate: stable C-ABI, ownership/lifetime discipline across the boundary, string/error marshaling, a header (`cbindgen`). Mirror the wire protocol's existing string/result encoding so the shape is familiar.
- (M/L) A WASM target: replace or feature-gate mmap scanning with a buffer-backed reader for `wasm32` (the heap's `try_for_each_row_raw` is the load-bearing abstraction to fork). This is the bulk of the work and overlaps with the local-first browser story.
- (S, per language) Thin idiomatic wrappers (npm package — one already exists as a *TCP* client `@zvndev/powdb-client`; an in-process FFI variant would be new; a PyO3 wheel; a Go module).

**Risks.** (a) FFI is an `unsafe` surface — every panic must be caught at the boundary (`catch_unwind`) or it's UB. (b) WASM-without-mmap may regress the very scan fast paths that are PowDB's whole pitch; benchmark before claiming parity. (c) Concurrency across FFI: the host language's threads must respect the `RwLock` contract — easy to get wrong from JS/Python.

**Verdict: REAL-PROJECT.** High value (it's how PowDB reaches the 90% of backends that aren't Rust), but it's weeks-to-months and the WASM mmap question is genuine engineering, not glue. Do the C-ABI first (smaller, unblocks Node/Bun/Python/Go servers); treat WASM as the bridge to Model 4.

---

### Model 3 — Sidecar embedding (the `powdb-server` you already have)  ⟶ *Idea 2, pragmatic fallback*

**Mechanism.** Run `powdb-server` as a sidecar on the same host as the app (Unix domain socket or loopback TCP), so the non-Rust backend talks to it over the existing wire protocol but with no real network hop. This is the **zero-new-code** way for a non-Rust backend to get most of the latency win *today*, before the FFI crate exists.

**Already supported.** Entirely — it's the shipping server. The only gap: the wire protocol is TCP-only; a **Unix-domain-socket** transport would shave the loopback TCP overhead and is an S-sized addition (the `handle_connection` generic is already transport-agnostic over `AsyncRead + AsyncWrite`).

**Must be built.** (S) Optional UDS listener in `main.rs`. (S) Document the sidecar pattern (same pod/host, UDS, shared `--data-dir` owned solely by the sidecar — never share a data dir between two processes).

**Risks.** Still pays serialization + one context switch per query (request/response, **no pipelining** — every query is a full round trip). Faster than remote TCP, slower than true in-process. Two processes must never open the same `--data-dir`.

**Verdict: SHIP-SOON** (as the interim answer for non-Rust backends while Model 2 cooks). It's a doc + a small UDS patch.

---

### Model 4 — Local-first embedded replica with built-in bulk sync  ⟶ *Idea 1, the headline*

**Mechanism (recommended variant, detailed in Part 3).** Embed the `Engine` in a native app (desktop/mobile/edge via Model 1 or Model 2's WASM/FFI). The app reads/writes its **local** PowDB at sub-ms. A built-in sync process ships the local WAL's frames to a central PowDB server and pulls the server's frames back, with the **server as source of truth** and **local writes rebased** on pull — the Turso model, implemented on PowDB's own LSN'd WAL.

**Already supported.**
- The LSN'd, CRC'd, append-only WAL with idempotent per-page redo — *this is 70% of a frame-shipping replicator.*
- `read_all()` already parses the WAL into typed records; replay logic already exists in `catalog.rs::replay_wal`.
- `set_next_lsn_at_least` (`wal.rs:170`) already exists precisely to keep LSNs monotonic after replaying foreign frames — the exact primitive a replica applier needs.

**Must be built.** (see Part 3 for the design)
- (M) **WAL retention / frame log**: today checkpoint *truncates* the WAL. A replica source must retain frames until the peer acks them. Add a retained, segmented frame log (or copy frames to a `sync/` dir on append) with a "min unacked LSN" watermark.
- (M) **A sync protocol**: `push(frames since server_lsn)` / `pull(frames since local_lsn)` message types on the wire protocol (or a dedicated replication port, mirroring the read-replica plan in the companion roadmap).
- (M) **Rebase-on-pull**: when local has un-pushed frames and the server advanced, re-apply local frames on top of the pulled state (or `DISCARD_LOCAL`). Because redo is idempotent per-page-LSN, the applier is mostly there; the *rebase* (re-stamping local writes with new LSNs after the server's frames) is the genuinely new logic.
- (M) **Identity / partitioning**: "who syncs what." v0 = **whole-database replica only** (one app ↔ one server DB, full copy). Selective/partial sync (PowerSync-style buckets) is a later, logical-layer feature — frames can't be partially shipped because a page mixes rows.
- (S) **Schema/version handshake**: refuse to apply frames across incompatible catalog versions; DDL records are already in the WAL (`DdlCreateTable` etc.), so the applier can detect schema drift.

**Risks.**
- **Frame shipping forces whole-DB (or whole-page) granularity.** You cannot ship "only this user's rows" with physical frames — a 4 KB page holds many rows. So Model 4-physical = full-database embedded replicas (great for single-user apps, an admin's local mirror, edge caches). **Per-user partial sync needs Family B (logical), which is the next epoch.**
- **Single-writer story only.** Server-authoritative + rebase gives correct *eventual* state but the last writer's intent wins; true multi-user concurrent editing of the same rows wants CRDTs (don't).
- **WAL format is now a wire contract.** The moment frames cross machines, `WAL_HEADER_SIZE` and the record layout are a compatibility surface. Version it.
- **Checkpoint vs retention tension.** The truncate-on-checkpoint behavior is load-bearing for local crash recovery; retention must be added *beside* it without breaking the clean-restart path.

**Verdict: REAL-PROJECT (one quarter for v0, full-DB replicas).** It is the most exciting idea and it's genuinely tractable *because* the WAL already is a frame log. But it is weeks-to-a-quarter, not a sprint, and the partial-sync version people will eventually ask for is a *second*, larger project.

---

### Model 5 — Read-replica fan-out from a central server  ⟶ *Idea 2, scale*

**Mechanism.** The companion roadmap's Options D/A: a primary streams WAL to N read-replicas. Relevant *here* because the same retained-frame-log + applier built for Model 4 is the exact machinery for server-side read replicas. **Build the frame log once; it powers both the local-first replica and the read-replica fan-out.**

**Verdict: REAL-PROJECT**, and a strong reason to build the retained frame log early — it's the shared substrate for the entire replication story. See [`distributed-and-tenancy-roadmap.md`](./distributed-and-tenancy-roadmap.md) Options D & A.

---

## Part 3: The sync engine I'd build first — "PowSync v0" (server-authoritative WAL-frame replication)

**Thesis:** build Family A (Turso model) on PowDB's existing LSN'd WAL. It is the only sync design where PowDB already emits the exact unit being shipped, so it reuses the WAL, the replay path, and `set_next_lsn_at_least` instead of inventing a logical change log, an HLC, and a CRDT merge executor.

### Topology + consistency model

- **One central PowDB server = source of truth** for one database (consistent with "one process = one DB").
- **N embedded replicas** (native apps / edge nodes), each a *full copy* of that database.
- **Reads**: always local, sub-ms, offline-capable.
- **Writes**: applied locally first (instant), then pushed. **Server-authoritative**: on conflict, server state wins and local frames are **rebased** (re-applied) or **discarded** per a configured policy (`REBASE_LOCAL` default, `DISCARD_LOCAL` / `FAIL_ON_CONFLICT` options) — the Turso strategy set.
- **Guarantee**: eventual convergence to server state; a replica that pushed successfully sees its own write immediately (read-your-writes locally).

### Data structures (new)

```
// Retained frame log — replaces "truncate on checkpoint" for sync-enabled DBs.
// Frames are exactly today's WAL records, kept until acked.
struct FrameLog {
    segments: Vec<Segment>,        // append-only, rolled at size N
    min_unacked_lsn: u64,          // frames below this are safe to GC
}
struct Frame {                     // == existing WalRecord on the wire
    tx_id: u64,
    record_type: WalRecordType,    // Insert/Update/Delete/Commit/DDL...
    lsn: u64,                      // monotonic, already assigned by Wal::append
    data: Vec<u8>,                 // raw encoded payload (unchanged)
}

// Per-replica sync cursor (persisted on the replica, and tracked on the server).
struct SyncState {
    db_id: Uuid,                   // which database this replica mirrors
    server_lsn: u64,               // highest server LSN this replica has applied (pull cursor)
    pushed_lsn: u64,               // highest local LSN the server has acked (push cursor)
    schema_version: u64,           // catalog generation; refuse cross-version apply
}
```

`server_lsn` / `pushed_lsn` are the direct analog of RxDB's **checkpoint** cursor — but expressed as LSNs, which PowDB already mints, instead of `{id, updatedAt}`.

### Protocol sketch (new wire messages, request/response — fits the existing no-pipelining model)

```
HELLO_SYNC   { db_id, schema_version, server_lsn, pushed_lsn }
  → SYNC_OK  { server_lsn, schema_version }            // or SCHEMA_MISMATCH → client must re-bootstrap

PULL         { since_lsn }                              // replica asks for server frames
  → FRAMES   { frames: [Frame], new_server_lsn, more: bool }   // batched; `more` drives the loop like RxDB's batch-size cursor

PUSH         { base_server_lsn, frames: [Frame] }       // replica sends its local frames
  → PUSH_OK  { acked_through_lsn }                       // fast path: server appended cleanly
  → REBASE   { server_lsn }                              // server advanced past base_server_lsn:
                                                         //   replica must PULL, rebase locals, re-PUSH
```

### Algorithm (replica side)

1. **Bootstrap**: if `db_id` unknown, pull a checkpoint snapshot (a copy of the heap + catalog) + the WAL tail, set `server_lsn`. (Snapshot transfer = same machinery a cold read-replica needs; build once.)
2. **Local write**: `engine.execute_powql(...)` as normal — frames land in the local `FrameLog` with local LSNs above `pushed_lsn`.
3. **Pull**: `PULL{since: server_lsn}` → apply frames via the **existing replay path** (`replay_wal` logic), which is already idempotent by per-page LSN. Call `set_next_lsn_at_least(new_server_lsn+1)` (the primitive already exists, `wal.rs:170`).
4. **Push**: `PUSH{base: server_lsn, frames: local frames above pushed_lsn}`.
   - `PUSH_OK` → advance `pushed_lsn`.
   - `REBASE` → server moved on: PULL the gap, then **rebase**: re-execute/re-stamp the local frames on top of the new server state with fresh LSNs, then re-PUSH. (This is the one genuinely new piece of logic; everything else is reused.)
5. **GC**: server advances `min_unacked_lsn` to the slowest replica's `pushed_lsn`; frames below are truncated as today.

### Why this is the right first bet

- **Reuses the crown-jewel asset.** The WAL, `read_all`, `replay_wal`, idempotent per-page redo, and `set_next_lsn_at_least` were all built for local durability and *coincidentally* are 70% of a frame-shipping replicator. No CRDT, no HLC, no logical change log.
- **Honest about its limit.** Full-DB replicas + single-writer-wins. That's genuinely useful (offline desktop/mobile apps with one owner, edge read caches, an admin's local mirror) and it doesn't pretend to be Google-Docs multiplayer.
- **It's the same substrate as server read-replicas** (companion roadmap D/A). One frame log, two products.

### What PowSync v0 explicitly does NOT do (and what would be v1/v2)

| Want | Needs | Epoch |
|---|---|---|
| Per-user / partial sync (only my rows) | Logical row-change log keyed by **real PK** + buckets/sync-rules (PowerSync model) | v1 (Family B) |
| Per-row conflict UX ("server changed this row, pick one") | Logical changes + RxDB-style `assumedMasterState`/`newForkState` checkpoint protocol | v1 (Family B) |
| Concurrent multi-user editing, no central server | Per-column CRDTs + HLC + tombstone log (cr-sqlite model) | v2 (Family C) — research |

---

## Part 4: Phased roadmap

```
Phase 0 — Embed-in-backend (Idea 2 core). SHIP-SOON. Days→1-2 wks.
  • Doc: "Embedding PowDB" — Arc<RwLock<Engine>> pattern, read/write dispatch, shutdown checkpoint.
  • Thread-safety contract as first-class docs.
  • Optional EngineHandle wrapper (encapsulate lock + dispatch).
  • Optional Unix-domain-socket transport for the sidecar fallback (Model 3).
  → Fast win. Directly kills the 170ms remote-client latency for Rust backends.

Phase 1 — Retained frame log (shared substrate). M, ~3-4 wks.
  • FrameLog beside the WAL; retention to min_unacked_lsn; keep truncate-on-checkpoint for non-sync DBs.
  • Snapshot+tail bootstrap.
  → Unlocks BOTH local-first replicas (Model 4) AND server read-replicas (companion roadmap D/A).

Phase 2 — PowSync v0 (Idea 1, full-DB embedded replicas). M/L, ~1 quarter.
  • PULL/PUSH/REBASE protocol on the wire.
  • Replica applier (reuse replay_wal + set_next_lsn_at_least).
  • Rebase-on-pull; conflict policy (REBASE_LOCAL default).
  • Schema-version handshake.
  → Server-authoritative offline-capable embedded DB.

Phase 3 — FFI / WASM reach (Idea 2 reach). M/L, parallel with 2.
  • powdb-ffi C-ABI crate (unblocks Node/Bun/Python/Go in-process + sidecar-free).
  • WASM target (fork try_for_each_row_raw off mmap) — also the browser on-ramp for Phase 4.

Phase 4 — Logical sync layer (Family B). L, a quarter+, only if asked.
  • Real PK in change records, per-row version, tombstone log, "changes since checkpoint" query.
  • RxDB-style checkpoint protocol; PowerSync-style buckets for partial sync.
  → Per-user partial sync + per-row conflict UX. Big project. Don't start until a user needs it.
```

**If betting one quarter:** Phase 0 (days) → Phase 1 (the frame log, because it's the shared substrate for everything replication) → Phase 2 (PowSync v0). That delivers the in-process backend story *and* offline-capable full-DB replicas, on the existing WAL, without breaking a single invariant.

---

## Part 5: Don't build this yet / here be dragons

- **Multi-master CRDT sync (cr-sqlite / Ditto model).** It demands per-column versioning, an HLC, a tombstone log, and a merge executor — a second storage discipline bolted onto every row and every write. It also *directly contradicts* the "fewer layers = faster" thesis: fatter rows, bookkeeping on every mutation. Build it only if a paying customer needs offline multi-user concurrent editing with no server, and even then prototype it as a separate experiment, not in the hot path.
- **Logical CDC with vector clocks, before a real PK exists.** PowDB's `RowId` is a heap slot, not a stable logical identity (`catalog.rs`). Any logical sync stands on a real primary key concept; designing CDC before that is designing on sand. Land PK-as-identity first (it's also needed for partial sync and for RLS in the companion roadmap).
- **Partial/selective sync over physical frames.** A 4 KB page mixes many rows; you cannot ship "just this tenant's rows" as frames. Trying to make frame-shipping selective is a dead end — partial sync is inherently a *logical* feature (Family B). Don't promise it on the v0 frame-shipping engine.
- **Anything that makes the planner distribution- or policy-aware.** Same red line as the companion roadmap: the planner is pure (no catalog access, `crates/query/src/planner.rs`) and the executor lowers at runtime. Sync lives *below* the query layer (WAL/heap), so PowSync v0 respects this. Any sync design that needs the planner to know about replicas or buckets has wandered into Family C/sharding territory — stop and reconsider.
- **Sharing a `--data-dir` between two processes** (e.g., app + sidecar both opening the same heap). The heap is mmap'd and the WAL assumes a single writer; two openers will corrupt. Sidecar owns the dir exclusively; the app talks to the sidecar, never the files.
- **Treating the WAL record layout as private once frames cross machines.** The moment Phase 1 ships, `WAL_HEADER_SIZE` and the record format are a versioned wire contract. Add a format version to the sync handshake before the first cross-machine frame, not after.

---

## Appendix — Source map (landscape research, 2026)

| System | Mechanism (unit shipped) | Conflict model | Relevance to PowDB |
|---|---|---|---|
| Turso / libSQL | Physical WAL frames; rebase local on pull | Server-authoritative, LWW; REBASE/DISCARD/FAIL/MANUAL | **Direct template for PowSync v0** |
| SQLite session ext | Logical changeset/patchset (PK-keyed) | Conflict handler: OMIT/ABORT/REPLACE | Model for a future logical change log |
| RxDB | Logical docs + `{id,updatedAt}` checkpoint cursor | Client-side resolve; tombstones; idempotent | **Blueprint for Family B checkpoint protocol** |
| PowerSync | CDC → buckets via sync rules | Backend-defined; partial replication | Model for partial/selective sync (v1) |
| cr-sqlite | Per-column CRDT deltas + delete log | Conflict-free merge (LWW: col_version→value→site_id) | Family C; deferred/research |
| ElectricSQL | CRDT merge, writes straight to Postgres | Conflict-free | Family C |
| Realm / Atlas, Ditto | Multi-master CRDT (Ditto = P2P mesh) | Conflict-free, no server (Ditto) | Family C; different product |

**Sources:**
- Turso offline writes — https://turso.tech/blog/introducing-offline-writes-for-turso
- Turso embedded replicas — https://docs.turso.tech/features/embedded-replicas/introduction
- RxDB replication protocol — https://rxdb.info/replication.html
- SQLite session extension — https://sqlite.org/sessionintro.html
- PowerSync sync rules / partial replication — https://www.powersync.com/blog/sync-rules-from-first-principles-partial-replication-to-sqlite
- ElectricSQL vs PowerSync — https://powersync.com/blog/electricsql-vs-powersync
- cr-sqlite — https://github.com/vlcn-io/cr-sqlite ; column CRDTs — https://vlcn.io/docs/cr-sqlite/crdts/column-crdts
</content>
</invoke>
