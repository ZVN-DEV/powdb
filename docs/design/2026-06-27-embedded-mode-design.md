# PowDB — Embedded Mode Design

- **Date:** 2026-06-27
- **Status:** Proposed — needs Kirby's sign-off on the marked decisions (§7)
- **Follow-up to:** `2026-06-27-beating-sqlite-latency-design.md` (the transport analysis)
- **Goal:** Run the PowDB engine **in-process** (no socket) so it can beat SQLite on single-op latency and serve as the local store for **local-first / cloud-synced native apps**.

---

## 1. Why

The transport analysis showed PowDB's per-op floor is ~0.04 ms of **pure wire round-trip** (TCP loopback + protocol encode/decode + syscalls). SQLite's *entire* findUnique is 0.006 ms because it's an **in-process library** — a function call, no socket. **A networked server cannot beat an in-process library on single-op latency.** The only way to win that fight is to delete the wire: run the engine inside the application process.

Two payoffs, in priority order:

1. **Local-first native apps.** The headline use case. A React Native / Electron / Tauri / desktop / mobile app links the engine directly and reads/writes a **local** copy at in-process speed, fully functional offline. A later sync layer (§6) replicates to a cloud primary. This is the Turso/libSQL embedded-replica model and the reason embedded engines dominate app storage.
2. **SQLite-parity latency.** With the wire gone, single-op latency drops to the measured engine work (~0.005–0.01 ms) — parity with or better than SQLite — while keeping PowDB's real storage engine, indexes, and durability.

The server stays. Embedded is a **second front door** to the same engine, not a replacement: server = multi-client / remote (Postgres-shaped); embedded = single-process / local (SQLite-shaped).

---

## 2. What already exists

`powdb_query::executor::Engine` is already a clean, in-process Rust API:

```rust
Engine::new(data_dir) -> io::Result<Engine>
Engine::with_memory_limit(data_dir, bytes)
engine.execute_powql(&str) -> Result<QueryResult, QueryError>
engine.execute_sql(&str)   -> Result<QueryResult, QueryError>
engine.execute_powql_readonly(&str)            // &self, no write lock
engine.execute_powql_with_params(&str, &[..])  // prepared/parameterised
engine.set_wal_sync_mode(WalSyncMode)          // full | normal | off
```

So "embedded mode for Rust" is ~90% done — it needs a documented, stable facade and packaging, not new engine work. The real build is the **language bindings** (the app market is JS/Swift/Kotlin, not Rust).

---

## 3. The one hard problem: `panic = "abort"` vs FFI

The workspace sets `[profile.release] panic = "abort"` (Cargo.toml) — a **deliberate crash-only design**: a panic mid-mutation under unwinding could poison a lock or tear a write, so the server turns any panic into a fast clean process exit and lets a supervisor restart it (WAL replay recovers).

**That model is wrong for an embedded host.** If the engine aborts, it takes the *host app* down with it. An embedded library must convert a panic into a recoverable error, never abort the process.

### Resolution (build mechanics validated 2026-06-27)

The binding artifact is a **separate Cargo workspace** with its own `[profile.release] panic = "unwind"`, depending on `powdb-query` (+ `powdb-storage`) via path deps. When that artifact is built, Cargo recompiles those crates under `unwind` for it — independent of the server's `abort` build. *Validated:* a throwaway `cdylib` with `panic = "unwind"` + a path dep on `powdb-query` + `catch_unwind` compiled cleanly.

Every engine call at the binding boundary is wrapped:

```rust
match catch_unwind(AssertUnwindSafe(|| engine.execute_powql(q))) {
    Ok(r)  => r.map_err(to_js_error),
    Err(_) => { handle.poison(); Err(js_error("engine panicked; reopen the database")) }
}
```

**Crash-only at the handle level, not the process level:**
- A caught panic **poisons the handle** — every later call on it returns an error until the host reopens.
- The poisoned handle is dropped **without a clean checkpoint** (a panic mid-mutation may have left in-memory pages torn; flushing them would persist garbage). We discard the in-memory state and rely on **WAL replay on reopen** to reconstruct a consistent on-disk state — the exact crash-only contract, just scoped to one handle instead of the OS process.

This preserves the durability guarantees ([[project_v043_durability_p0]]) without aborting the host. The server keeps `panic = "abort"` unchanged.

---

## 4. Architecture

```
                       ┌──────────────────────────────┐
   Rust app ──────────►│  powdb (facade crate)        │
                       │   Database::open / .query     │
                       └──────────────┬───────────────┘
                                      │ (same Engine)
   Node app ──► @zvndev/powdb ──► napi addon ──────────► powdb_query::executor::Engine
   (RN/Electron)        (separate workspace,            (+ powdb_storage: heap, WAL,
                         panic=unwind, catch_unwind)      B+tree, catalog)
```

### 4.1 Phase 1a — Node native addon (the prize)
- Built with **napi-rs** (modern, prebuilt-binary tooling, N-API ABI stability across Node versions; preferred over neon).
- Lives in its **own workspace** (e.g. `bindings/node/`), path-deps on the engine crates, `panic = "unwind"`.
- JS API mirrors the existing TS client so it drops into Turbine:

```ts
import { Database } from "@zvndev/powdb"; // (name = §7 decision)
const db = Database.open("./data");       // in-process, no server
db.query(`insert User { name := "Ada" } returning`);
db.querySql("SELECT * FROM User WHERE id = ?", [1]);
db.close();
```

- `catch_unwind` + handle poisoning per §3. Synchronous API first (matches SQLite's `better-sqlite3` ergonomics and the engine's blocking nature); an async/worker-thread variant can come later if needed.

### 4.2 Phase 1b — Rust facade crate `powdb`
- A thin, documented crate that re-exports the embedded surface as `powdb::Database` so `cargo add powdb` is the obvious embedded entry point (today the engine hides inside `powdb-query`, which reads as an internal compiler stage).
- Optional/low priority relative to the Node addon, but it's the canonical Rust embedded API and what the C ABI (§4.3) wraps.

### 4.3 Phase 1c — C ABI (`cdylib`) — later
- A `#[no_mangle] extern "C"` surface over the facade for Swift/Kotlin/other FFI (native mobile). Same panic discipline. Deferred until there's a concrete consumer.

---

## 5. Packaging & distribution (Node addon)

- **Prebuilt binaries per platform** published with the npm package (napi-rs's standard `@scope/pkg-<triple>` optionalDependencies layout), built in CI for: macOS arm64 + x64, Linux x64 (glibc) + arm64, Windows x64. Build-from-source fallback via `npm install` if no prebuilt matches.
- CI: a new matrix job cross-compiles the addon and attaches artifacts; publish on tag (reuse the **OIDC trusted-publishing** standard already in place — [[reference_oidc_trusted_publishing]] — so no new tokens).
- Versioned in lockstep with the other crates.

---

## 6. Phase 2 (future, separate effort): local-first cloud sync

Embedded mode is the **prerequisite**; sync is a bigger, later layer. Sketch so we leave the seam:

- **Model:** local in-process replica per device + a cloud **primary**. Reads hit the local replica (in-process speed); writes forward to the primary; the primary streams committed changes back to replicas. (Turso/libSQL embedded replicas.)
- **Building blocks PowDB already has:** the WAL (`crates/storage/src/wal.rs`) is the change log to ship; the `powdb-backup` crate already does backup/restore + PITR over that log. Replication = "ship WAL records primary→replica + apply."
- **Open questions for that phase:** write path (forward-to-primary vs local-write-with-async-push), conflict/ordering model (single-writer primary is simplest — matches SQLite's one-writer reality), sync transport/auth, schema-change propagation.
- **Not in scope now.** Flagged only so the embedded API (handle lifecycle, WAL access) doesn't foreclose it.

---

## 7. Decisions (locked 2026-06-27)

| # | Decision | Choice |
|---|---|---|
| D1 | **Scope** | **Both** the Rust facade (`crates/powdb`) and the Node addon (`bindings/node/`), built back-to-back in dependency order (facade first — the addon wraps it). |
| D2 | **npm package name** | **`@zvndev/powdb-embedded`** (network client stays `@zvndev/powdb-client`). |
| D3 | **Prebuilt platforms** | **Full matrix:** macOS arm64+x64, Linux x64+arm64, Windows x64, with build-from-source fallback. |
| D4 | **Sync (Phase 2)** | Confirmed **north star**, **not** built now — leave the seam (handle lifecycle, WAL access). |

Taken as decided: separate-workspace + `panic = "unwind"` + `catch_unwind` + handle-poisoning, napi-rs over neon, sync-first blocking API, lockstep versioning, OIDC publish ([[reference_oidc_trusted_publishing]]).

---

## 8. Test plan (TDD)

- **Rust (facade + engine):** open → write → read → close; reopen-after-close durability; readonly handle.
- **Panic safety:** a deliberately-panicking call returns an error (not abort), poisons the handle, and a reopened handle recovers all committed rows via WAL replay. *This is the load-bearing test* — it proves the crash-only-at-handle contract.
- **Node addon:** a JS test (in the existing TS test harness) opens an embedded DB, runs create/insert/`returning`/count/`querySql` with params, asserts results; closes and reopens.
- **Parity:** the same query set returns identical results embedded vs. over the wire.

---

## 9. Rollout

1. `bindings/node/` workspace + napi skeleton, engine wrapped with catch_unwind + handle poisoning. (D1)
2. JS API surface + TDD suite incl. the panic-safety + reopen test.
3. CI prebuilt-binary matrix + OIDC publish wiring.
4. `powdb` Rust facade crate (parallel, low risk).
5. Turbine `turbinePowDB({ embedded })` adapter (in the turbine repo).
6. (Later) C ABI; (later) Phase 2 sync.
