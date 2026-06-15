# PowDB — Master Remaining-Work Backlog

_Date: 2026-06-15 · The single "come back and run from here" doc._
_Status snapshot after the v0.4.9 security patch shipped. Cross-references the two deep planning docs:_
- _`docs/strategy/2026-06-14-direction-and-hardening-roadmap.md` — the strategy + thesis._
- _`docs/strategy/2026-06-14-worklist-and-build-map.md` — the deep task tables, [PROPOSED]/[DECIDE] detail._

This file is the index of **everything not yet done**, both the product roadmap and the operational/infra
loose ends. Phases are condensed-but-actionable; open the worklist doc for the full design rationale.

---

## ✅ Shipped (for context — do not redo)

- **v0.4.9 security patch** (2026-06-15, PR #91): fixed 2 remote-DoS crashes (`i64::MIN/-1` division panic
  in `eval.rs`; huge-`LIMIT` prealloc in `plan_exec.rs`) + `0700` data dir. 6 crates on crates.io, GitHub
  release + binaries + ghcr `v0.4.9`, Fly redeployed + live-verified. See `project_v049_security` memory.
- v0.4.8 production-hardening (RBAC lattice, durability smoke gate, dead-code removal). v0.4.7 and earlier.

## Locked strategic decisions (recap)

1. **Dual frontend** — keep PowQL native, ADD a SQL frontend to the same plan tree. (Reverses "no SQL ever.")
2. **The moat is the ENGINE**, not the language. PowQL = better-DX optional surface; SQL = the on-ramp.
3. **Consolidation-engine thesis** (Kirby's): native search/vectors/live/views/docs, transactionally
   consistent, no sync tax — the strongest moat, engine-rooted. Adopt as headline once validated.

---

# PART A — Product roadmap (the big phases)

## Phase 1 — v0.5.0 "the foundation" (NEXT) — 4 lanes

### 1A · Real transactions  `[storage + server]`  ~400–650 LOC
Transactions are ~70% built, broken in 3 places. Single-writer ⇒ **no MVCC needed** (hold write lock for
txn lifetime + redo-with-commit-markers + two-pass replay = serializable).
- **1A.1 Bug A** — per-connection txn state + hold write lock `begin`→`commit`/`rollback`. Today
  `in_transaction` is a shared `Engine` flag and the lock drops between statements (`handler.rs:585`) →
  cross-connection capture + dirty reads over the wire. ~150–250 LOC. **Riskiest (connection-drop cleanup
  must release the lock or the DB wedges).**
- **1A.2 Bug B** — rollback soundness across page eviction. Heap evicts dirty pages mid-txn → >1-page txns
  can't roll back. Fix via before-images / page-level undo log (checkpoint-on-begin barrier). ~150–250 LOC.
  **Design the undo log so it _could_ be retained → feeds time-travel later.**
- **1A.3 Bug C** — write a `Commit` WAL marker; two-pass replay (pass 1 collect committed tx_ids, pass 2
  redo only committed; tx_id==0 = autocommit). ~100–150 LOC. **Must NOT regress `durability.rs` guards.**
- **1A.4 tests** — crash-before-commit (vanishes) / after-commit (survives); multi-page rollback;
  concurrent-connection isolation; reader-blocks-during-write-txn.
- ⚠️ **Check the `origin/explicit-transactions` branch first** — prior transaction work may already exist.

### 1B · Stable on-disk format  `[storage]`
B+tree (`BIDX v1`) + catalog (`BCAT v3`) already versioned. Four formats are NOT. Template: `backup/src/manifest.rs:25`.
- **1B.1 Row encoding** 🔴 — highest blast radius (silent misdecode, zero guard). Magic+version. DO FIRST.
- **1B.2 Heap pages** 🟠 — magic/version guard (flag bit; "bit clear = v1" sentinel preserves 0.4.x files).
- **1B.3 WAL** 🟠 — file magic+version + reject-unknown.
- **1B.4 Heap file superblock** 🔴 **[DECIDE]** — page 0 is live data today, so "no version = v1" doesn't
  work here. Superblock (reserve `PageType::Meta=5`) vs sentinel — **permanent choice, get eyes on it.**
- **1B.5** — cross-version read tests + a `format_version` introspection command.
- _Prereq for every new on-disk structure below (search/vector/doc indexes inherit versioning from day 1)._

### 1C · SQL frontend  `[query — new isolated files]`
AST is already a clean frontend-agnostic IR; `plan_statement()` is pure; plan cache keys on normalized
token hash. A SQL parser → same AST reuses planner+executor+cache 100%.
- **1C.1** SQL lexer + parser → existing `Statement`/`Expr` (cover shipped surface: select/insert/update/
  delete, joins, subqueries, window fns, aggregates, UNION).
- **1C.2 [DECIDE]** dialect dispatch — connection param / statement sniff / separate port?
- **1C.3** parity test matrix (same query PowQL≡SQL → identical plan → identical results).
- **1C.4** document the dialect boundary.
- _Defer the ripple features (CTEs/derived tables, correlated subqueries, FULL OUTER, ORDER/GROUP BY
  exprs) — they need `QueryExpr.source: String → relation enum`, which touches the planner. → backlog._

### 1D · Cleanup  `[cli/query/ts]`
- **1D.1 [DECIDE]** BufferPool dead code — wire it in (replace hand-rolled caching) OR delete. Lean: delete
  unless clock-sweep measurably beats the hand-rolled cache under memory pressure. **Don't ship both.**
- **1D.2** Parser error positions + "did you mean" — `UnexpectedToken` carries no position (`parser.rs:36`);
  add caret + fuzzy table/column suggestions (catalog has the names). Big DX win.
- **1D.3** CLI keyword-completion drift — `POWQL_KEYWORDS` missing window/upsert keywords; add a parity test.
- **1D.4** `db` default-name inconsistency — CLI defaults `main`, TS client defaults `default`. Unify.

## "The magic" — M.* (build-back of the designed-but-unwired PowQL) — v0.6 territory
Keystone = **nested `Value` types**; everything routes through it.
- **M.1 [DECIDE]** `Value::List` + `Value::Struct` (needs 1B.1 row versioning). Field-name storage interned
  vs positional — permanent.
- **M.2 [DECIDE]** nested wire-protocol encoding (breaks old clients; do it once).
- **M.3** executor result-shaping (emit Struct/List from the `{ }` projection).
- **M.4 [DECIDE]** `link`/`multi link` schema types (real data-model addition).
- **M.5** link-traversal grammar + planner (lowers to existing joins).
- **M.6** nested projection grammar (`User { .name, posts: .posts { .title } }`).
- **M.7** `let` bindings · **M.8 [DECIDE]** `match` operator (pattern-match vs FTS search — don't corner the
  keyword) · **M.9** `??` default operator.
- **M.10** graph-aware ("symmetric") aggregation — correct-by-default `avg`/`sum` across links (SQL is wrong
  by default: 8.67 vs true 12.92). Needs links (M.4–M.6).
- ⚠️ Honest framing: M.* are **better-DX surfaces alongside SQL, NOT a product bet** (nested fetch isn't
  structural — `json_agg` does it; EdgeQL died betting a general-purpose DB on it). Validate first ↓.

## Phase 2 — v0.6 PowQL differentiation experiment (GATE — do before heavy M.* work)
One week, falsifiable, one query shape each:
- **2.1** fan-out aggregate demo (SQL wrong vs PowQL right-by-default). Cheapest, most visceral. Needs minimal M.4/M.10.
- **2.2** nested fetch demo vs SQL `json_agg`/JOIN+regroup (ergonomics, N+1, correctness). Needs minimal M.1–M.6.
- **2.3** (optional) LLM head-to-head PowQL vs SQL gen accuracy. ⚠️ Don't market "LLMs love PowQL" without data.
- **Decision rule:** meaningfully better → invest in full M.*; marginal → keep PowQL thin, lead with engine + SQL.

## The Consolidation Engine — the stronger moat (v0.6/0.7 big bet)
Engine-rooted capabilities people currently run a 2nd system for (and pay a sync tax). All depend on **1B**
(format versioning) and most on **M.1** (nested Values). Ranked:
- **§S Native full-text search** 🥇 — inverted index maintained in the same txn as the write ⇒ never stale.
  Tasks: S.1 inverted-index struct **[DECIDE format]**, S.2 analyzer pipeline, S.3 BM25 ranking,
  S.4 transactional maintenance (the whole moat), S.5 `match` query surface **[DECIDE]**, S.6 tombstone
  compaction (don't repeat the B+tree delete leak). **Killer demo:** insert+search in one txn; rollback → gone from search too.
- **§V Native vectors** 🥈 — V.1 vector column type **[DECIDE]**, V.2 ANN index (HNSW/IVF; start brute-force),
  V.3 distance ops + `ORDER BY emb <-> $q LIMIT k`, V.4 **hybrid FTS+vector** (the differentiated combo).
- **§L Live queries** 🥉 — L.1 subscribe-to-query **[DECIDE protocol]**, L.2 change detection (reuse txn
  dirty-set from 1A), L.3 push frames (needs M.2). ⚠️ app-backend territory — feature not identity.
- **§MV Materialized views** — MV.1 plain views (cheap, do early) → MV.2 materialized → MV.3 incremental
  maintenance (engine feature, not a language differentiator).
- **§D Document columns** — D.1 JSONB column (reuses M.1) → D.2 path access + GIN-style index (reuses §S) →
  D.3 schema-on-read. Scope as "relational engine that also speaks documents," NOT Mongo.

---

# PART B — Operational / infra backlog (not in the feature worklist)

## Open dependabot PRs (triage + merge) — as of 2026-06-15
- **#95** bump `zeroize` 1.8.2 → 1.9.0 (auth crate — security-adjacent; review then merge)
- **#94** bump `postgres` 0.19.13 → 0.19.14 (patch group; dev-only, `powdb-compare`)
- **#93** update `dtolnay/rust-toolchain` action pin (CI)
- **#92** bump `@types/node` 25.9.2 → 25.9.3 (TS client dev dep)
- _Run CI on each, batch-merge the green ones. None are blockers._

## Stale branches to triage / clean up
- **`origin/explicit-transactions`** — ⚠️ likely prior v0.5.0-1A work; **diff it before starting transactions.**
- **`origin/chore/bench-depot-runner`** — the bench→Depot migration (↓), ready, never merged.
- `origin/chore/gold-standard-prod-hardening`, `origin/chore/ts-client-0.5.0`, `origin/release/0.4.7`,
  `origin/release/0.4.8`, `origin/fix/fly-bind-ipv6`, `origin/smoke-audit-fixes`,
  `origin/claude/audit-drivers-platform-BaXOH`, `origin/fix/v0.4.9-security-patch` (merged — safe to delete) —
  **audit + delete the merged/abandoned ones** to de-clutter.

## Bench → Depot migration (from memory `project_bench_depot_migration`)
- Root cause of flaky bench gate = GitHub shared runners. `chore/bench-depot-runner` moves `bench.yml` to a
  Depot single-tenant runner (`depot-ubuntu-24.04-4`, tmpfs). **Blocked on:** Depot app install on the org +
  one Depot run to rebaseline `baseline/main.json`. (Baseline must ONLY ever be rebaselined from a Depot run,
  never the laptop.) Finish + merge this branch.

## Gold-standard audit leftovers (from memory `project_audit_backlog`)
- Test utilities/helpers crate · broader `unwrap()` reduction in request paths · further release automation.
  (MSRV job + CI matrix already shipped in v0.4.8.) Low priority, polish.

## Cross-platform — Windows support
- Only code TODO in the tree: `crates/storage/src/disk.rs:10` — implement `FileExt` via
  `std::os::windows::fs::FileExt`. Engine is Unix-only today. Widens adoption; not blocking.

## Misc
- **Fly `powdb-example` password** is currently a known TEST value (`/tmp/powdb-fly-pw.txt`). Rotate to a
  fresh random secret if the example app ever holds real data (one restart).
- **Keeper for secrets** (memory `reference_keeper_secrets`) — Kirby wants Keeper Security storing ZVN tokens
  (crates.io, npm, etc.). Not yet set up.
- TS client `@zvndev/powdb-client` is at 0.5.0; `CLIENT_VERSION` constant fixed on main but the published
  0.5.0 npm artifact still carries the old 0.4.0 string (cosmetic — server checks major only).

---

# PART C — Open [DECIDE] decisions (permanent / schema / format — need Kirby)

1. **1B.4** heap-file superblock vs sentinel (permanent on-disk choice).
2. **1C.2** how clients select SQL vs PowQL dialect.
3. **1D.1** wire in BufferPool vs delete it.
4. **M.1/M.2** nested `Value` field-name storage (interned vs positional) + nested wire encoding (permanent).
5. **M.4** `link`/`multi link` schema types.
6. **M.8 vs S.5** does `match` mean pattern-match or search-match? Don't corner the keyword.
7. **Thesis call** — adopt the consolidation-engine framing as the headline differentiation? (Recommendation: yes.)

---

# PART D — Recommended sequencing for the next session

```
1. Quick wins (any time): triage+merge dependabot #92–95; delete merged branches; diff explicit-transactions.
2. v0.5.0 foundation — start 1A (transactions) ‖ 1B (format), since both touch storage decide ONE lane or
   sequence; 1C (SQL) and 1D (cleanup) parallelize independently. Land M.1 nested Values + MV.1 plain views
   here if cheap (they unlock the consolidation engine).
3. GATE: Phase-2 experiment (fan-out + nested fetch) → decide if full M.* is worth it.
4. BIG BET: §S native transactional FTS → §V vectors + hybrid. (If Phase-2 is marginal, §S becomes the
   headline feature instead of PowQL.) Either way, the engine is the moat.
5. Background/polish: bench→Depot finish, Windows, audit leftovers, Keeper secrets.
```

**Bias:** §S (native transactional full-text search) is the single most defensible, most-demanded, best-fit
item in the whole plan. The engine is the moat — everything here serves that.
