# PowDB Product Cycle — Post-Turbine v0.7.0 Adoption

_2026-06-28. Inputs: the Turbine ORM team's v0.7.0 adoption report + cross-engine
benchmarks, and a PowDB-side correctness/safety audit that landed the 0.7.1
fixes. This doc is the PowDB product team's plan: what we proved, what's left,
and the release sequence._

## 1. Position

v0.7.0 turned PowDB's Turbine support from "technically works, heavily hedged"
into "speaks PowDB natively." The "beat SQLite" goal is **met on reads** in
embedded mode, and networked is now **Postgres-class on every operation**. One
strategic gap remains: **embedded write durability is not tunable from JS**, so
embedded writes are pinned to `Full` (one fsync/commit, ~4 ms). The engine proves
the overhead is ~nil (embedded reads are 0.006 ms); the write gap is a missing
knob, not an engine cost.

**The wedge to close:** expose the durability mode to the embedded addon and
PowDB beats SQLite on writes too, completing the local-first story.

## 2. The benchmark payoff (Turbine `CROSS-ENGINE-RESULTS.md`)

| Workload | Result |
|---|---|
| Embedded `findUnique` | **0.006 ms = SQLite parity** (was 14× slower networked) |
| Embedded nested read | **0.314 ms — beats SQLite (0.368) and Postgres (0.407)** |
| Networked `create` | **0.042 ms — 96× faster than 0.6.2, 3× faster than Postgres** |
| Networked `update` | 0.068 ms — beats Postgres |
| Networked `createMany` | 0.466 ms ≈ Postgres; seed 965 ms → 49 ms |
| **Embedded writes** | ⚠️ **fsync-bound ~4 ms** (create 3.99 / update 3.98 / createMany 15) — the only gap |

## 3. What shipped / is shipping

**v0.7.0 (released, all channels):** `RETURNING` (PowQL+SQL), column `DEFAULT`,
`auto`-increment, `WalSyncMode::Normal` (off-lock group-commit flusher), Unix
domain socket transport, embedded mode (`powdb` crate + `@zvndev/powdb-embedded`
addon). Retired Turbine's three biggest debts (reselect→RETURNING, float-literal
workaround, hedged adapter).

**v0.7.1 (this cycle, on `fix/v0.7.1-correctness-hardening`, TDD, suite green):**
- 🔴 **SQL `count(*)` aggregation fix** — *PowDB-side audit finding, not in
  Turbine's list.* Ungrouped `SELECT count(*)/sum/avg/min/max` lowered to a row
  projection and returned one **null row per source row** instead of a scalar.
  Documented + README headline feature; silent wrong answers on the server
  `QuerySql` path and embedded, since the SQL frontend shipped (v0.5.0). Now
  lowers to PowQL's aggregate form; multi-aggregate/joined/DISTINCT ungrouped
  shapes return a clear error instead of garbage.
- 🟠 **Embedded `open()` panic-safety** (Turbine #3) — a corrupt heap/index
  header panicked deep in open and (under `panic=unwind`, i.e. the addon) could
  abort the host. Facade `open`/`open_with_memory_limit` now `catch_unwind` →
  `Error::OpenPanicked`, matching the per-query crash-only contract.
- 🟠 **Data-directory lock** (Turbine #3) — a PID-based lock file refuses a
  second open from a *different live process* (concurrent writers corrupt the
  heap/WAL), while allowing same-PID/dead-PID takeover so the `mem::forget`
  crash-recovery suite still passes. Cross-process protection, not in-process.
- 🧹 version drift fix (#5): `bindings/node/Cargo.toml` 0.6.2 → 0.7.1.

## 4. Prioritized backlog

| # | Item | Type | Pri | Owner | Plan |
|---|---|---|---|---|---|
| A | **Embedded `setSyncMode` / `openWithMemoryLimit` napi methods** | perf | **P1** | PowDB | ✅ **done in 0.7.1.** Thin `#[napi]` methods over the facade (`set_sync_mode_str` parses `"full"/"normal"/"off"`, TDD'd in the facade). Unlocks embedded `Normal` writes (~0.01–0.02 ms) → **beats SQLite on writes**. The single highest-ROI item. |
| B | **Packaging: all platforms via CI + musl** | packaging | **P1** | PowDB | 0.7.0 addon shipped 3/5 prebuilds (Intel-mac + Windows dropped; the CI count guard was bypassed by the token-bootstrap). Republish via CI; add `x86_64-unknown-linux-musl` for Alpine/distroless. Windows still blocked (Unix-only engine). |
| C | **count(*) fix** | correctness | **P0** | PowDB | ✅ done in 0.7.1. |
| D | **open panic-safety + data-dir lock** | safety | P2→done | PowDB | ✅ done in 0.7.1 (Turbine #3). |
| E | **Auto-increment server PKs** | feature | P2 | Turbine | Blocked by cross-engine `ColumnMetadata.hasDefault` conflating DEFAULT vs serial/generated. Needs a serial/generated split in introspect+codegen. PowDB side (`auto` columns) already ships. |
| F | **OIDC trusted publishing** | release | P2 | PowDB | v0.7.0 published via direct tokens after OIDC config mismatched on both registries (×6 crates + ×2 npm). Fix the per-package config so 0.7.x is token-less; rotate the leaked tokens. |
| G | **manyToMany / nested writes** | feature | P3 | Turbine | Still unsupported in the adapter. |
| H | **Windows engine port** | platform | P3 | PowDB | Storage uses raw `libc` mmap/`std::os::unix`; deferred Phase 3. Gates Windows prebuilds. |

## 5. Release sequence

**0.7.1 — correctness + safety + embedded write knob (this PR):** items **A + C
+ D** + version-drift fix. Item A (`setSyncMode`) was folded in because the
embedded addon republishes for the facade changes anyway — so the headline write
win ships now, no second republish. Cut as soon as CI is green.

**0.7.2 — packaging (item B):** republish the addon through CI for all buildable
platforms + musl, once trusted publishing (F) is fixed so it's token-less.

**0.8 — platform + features:** Windows engine port (H) → Windows prebuilds;
Turbine-side serial/generated split (E) for auto-increment PKs.

## 6. Strategic takeaway

PowDB is now genuinely strong: **networked = Postgres-class, embedded reads =
SQLite-class.** The one move that completes the story — and the cheapest high-
impact thing on this list — is exposing embedded durability (item A). Ship it in
0.7.1 and PowDB beats SQLite on both reads and writes in-process, which is the
local-first pitch in one sentence.
