# PowDB Direction & Hardening Roadmap

_Date: 2026-06-14 · Driven by the 2026-06-14 product review (scored 7/10) + two code-grounded
feasibility investigations. Supersedes the "no SQL, ever" positioning._

## Decisions (locked)

1. **Query language: DUAL — keep PowQL native, ADD a SQL frontend** that compiles to the same
   plan tree. This reverses the prior "never add SQL" stance. Rationale below.
2. **Sequencing: ship a fast security patch (v0.4.9) first**, then a phased v0.5.0 for the big work.

## Why the thesis changed

The old thesis — _"removing the SQL parse/plan tier makes queries faster"_ — is **contradicted by
our own benchmarks**: parse-dominated queries (point lookups, inserts) roughly **tie** SQLite,
while execution-dominated queries (aggregates/scans) win **5–10x**. If parse-removal were the lever,
the small queries would win biggest. They win least. **The moat is the compiled-predicate execution
engine, not the language.**

Investigation confirmed the engine's AST (`Statement`/`Expr` in `crates/query/src/ast.rs`) is a
**clean, frontend-agnostic relational IR** — PowQL is just one surface syntax over it. `plan_statement()`
(`planner.rs:49`) is pure; the plan cache keys on a **normalized token hash, not raw text**
(`canonicalize.rs:60`). So a SQL frontend that targets the same AST reuses the planner, executor, and
plan cache **100% unchanged**, and gets **identical plan-cache amortization** — SQL parse cost is paid
once, exactly like PowQL. This is **not the per-query "translation tier"** the old thesis feared.

**New thesis:** _"A compiled-execution, pure-Rust embedded+server engine — reach it via SQL or PowQL."_
PowQL stays as the native language and as the (still-unproven) "tiny grammar an LLM can be fully taught
in one prompt" differentiator.

---

## v0.4.9 — Security patch (fast, this week)

Small, low-risk, fast to verify. The live Fly instance is exposed; these are trivially craftable.

| # | Fix | Location | Action |
|---|-----|----------|--------|
| 1 | **`i64::MIN / -1` division panic** (remote crash via `panic=abort`) | `crates/query/src/executor/eval.rs:666` | `checked_div`; return `Value::Empty` on `None` (matches sibling arms) |
| 2 | **Unbounded `BinaryHeap::with_capacity(limit)`** from raw user LIMIT (allocator abort) | `crates/query/src/executor/plan_exec.rs:2446,2516` | `with_capacity(limit.min(CAP))`; heap still grows on demand |
| 3 | **Data dir + data files world-readable** (heap/WAL/btree hold all row data) | `crates/storage/src/catalog.rs:147`, `crates/query/src/executor/mod.rs:299` | create dir `0700`; heap/WAL files `0600` on Unix |
| 4 | (bundle, cheap) **latent overflow panics** — live the moment `overflow-checks` is enabled | `eval.rs:408,500-525,608`, `plan_exec.rs:3247` | `saturating_*`/`checked_*` |

Tests: each crash input no longer aborts; perms asserted. Pre-publish smoke against built binary →
publish 6 crates → release.yml durability gate. Bump 0.4.8 → 0.4.9.

---

## v0.5.0 — Real transactions + stable format + dual QL (phased)

### Phase A — Real multi-statement transactions (headline)

**Current state is ~70% built, not inert.** Real defects:
- **Bug A (killer):** server drops the write lock between statements (`handler.rs:585`); `in_transaction`
  is a single shared flag on `Engine`, not per-connection → cross-connection capture + dirty reads.
  Multi-statement txns are broken for any networked client.
- **Bug B:** rollback (`catalog.rs:563`) assumes uncommitted writes live only in memory, but the heap
  evicts dirty pages to disk mid-txn → a >1-page txn can't cleanly roll back. Masked by tiny tests.
- **Bug C:** no `Commit` marker written; replay is redo-only ignoring txn boundaries
  (`catalog.rs:321`) → crash mid-txn (WAL auto-flushes every 64 records) replays a torn txn.

**Design: redo + txn markers + per-connection session holding the write lock for txn duration.
NO MVCC** (single-writer makes write-lock-for-duration = serializable isolation).

- A1. WAL: ensure/emit `Begin`/`Commit`/`Rollback` records (`Commit`/`Rollback` types already exist,
  `wal.rs:8-18`); thread the session's active `tx_id` through catalog mutation methods instead of
  per-row `next_tx()`.
- A2. Commit appends a real `Commit{tx_id}` marker, then `sync_wal()` (the durability point).
- A3. **Two-pass replay** (`catalog.rs:249`): pre-scan committed `tx_id`s; redo only committed records
  (`tx_id==0` = autocommit/DDL = always committed). ~30–40 LOC. **MUST NOT regress the v0.4.x
  data-loss guards in `crates/query/tests/durability.rs`.**
- A4. **Per-connection session in server** (`handler.rs`): move `in_transaction`+`tx_id` off `Engine`;
  hold the write lock across statements for an open txn (likely `tokio::sync::RwLock` owned guards).
  **Robust connection-drop/timeout cleanup — an orphaned write lock wedges the whole DB.** (riskiest)
- A5. **Rollback soundness (Bug B):** checkpoint-on-begin barrier so reopen reconstructs the pre-txn
  heap. (riskiest correctness corner)
- A6. Tests: crash-before-commit → txn vanishes; crash-after-commit-fsync → survives; multi-statement
  txn over the server; concurrent-connection isolation; rollback of a multi-page write-set;
  reader-blocks-during-write-txn.
- Documented trade-off: a long write txn blocks readers (acceptable for v1).

Scope: ~400–650 LOC, concentrated in `catalog.rs` recovery + `handler.rs` lock model.

### Phase B — On-disk format stability (toward a 1.0 guarantee)

Half-versioned today: B+tree (`BIDX` v1) and catalog (`BCAT` v3) have magic+version+reject-unknown.
**Heap page, heap file, row encoding, WAL have none** (row encoding = highest blast radius, silent
misdecode). Template to replicate: `crates/backup/src/manifest.rs:25` `validate_version()`.

- B1. Add magic+version+reject-unknown guards to heap page (flag bit / version), WAL (file magic),
  row encoding (tie version to catalog version).
- B2. Heap **file superblock** (reserve a `PageType::Meta=5` page) — page 0 is live data today, so
  prefer a backward-compatible sentinel/sniff over a forced migration.
- B3. `docs/FORMAT.md` — document every on-disk format + the version policy.
- B4. Compat tests: unknown-version file rejected with a clear error; existing 0.4.x files still open
  (same cross-version discipline as the v0.4.8 backward-compat test).
- B5. **Public format-stability policy**: from v0.5.0, on-disk format changes are versioned + migrated,
  never silently breaking. Dated promise in README/SECURITY.

### Phase C — SQL frontend (dual QL)

Contained: a "second lexer+parser targeting the same AST." Does NOT ripple into planner/executor.

- C1. SQL lexer + recursive-descent parser → existing `Statement`/`Expr`. New files
  `crates/query/src/sql_lexer.rs`, `sql_parser.rs`.
- C2. `canonicalize_sql` for plan-cache parity (literal collection order must match the plan-tree walk,
  `plan_cache.rs:118-130`).
- C3. `Engine::execute_sql` mirroring `execute_powql` (`mod.rs:420`). Dialect selected via
  wire handshake flag / CLI flag / TS-client option.
- C4. **Scope v1 SQL to what the AST already represents** (WHERE/SELECT/FROM/joins/aggregates/window/
  IN+EXISTS subqueries/DML/DDL/UNION). Defer the ripple features with clear "unsupported" errors:
  CTEs/derived tables, correlated subqueries, FULL OUTER, ORDER/GROUP BY expressions. (These need
  `QueryExpr.source: String → relation enum`, which is the only change that touches the pure planner —
  deferred to backlog.)
- C5. Docs/site reposition: "PowDB speaks SQL and PowQL." Demote "no SQL = faster" everywhere.
- C6. Tests: SQL → same plan as equivalent PowQL; plan-cache-hit parity; unsupported-feature errors.

### Phase D — Cleanup / honesty (small, alongside)

- D1. **Remove dead `BufferPool`** (clock-sweep eviction is unused; hand-rolled caching is the real
  path) — or wire it in. Recommend remove.
- D2. **Parser error positions + "did you mean"**: `UnexpectedToken` carries no position
  (`parser.rs:36`); add caret/position + fuzzy table/column suggestions (catalog has the names). Big
  DX win.
- D3. **CLI keyword-completion drift**: `POWQL_KEYWORDS` missing window/upsert keywords; add a test
  asserting parity with the lexer token set.
- D4. **`db` default-name inconsistency**: CLI defaults `main`, TS client defaults `default`. Unify.

### Repositioning (docs/marketing — gated on Phase C)

Demote "no SQL = faster" → "compiled pure-Rust engine; SQL or PowQL." Update README headline, site,
`docs/gtm-strategy.md`, `docs/powdb-vs-sqlite.md`. Pre-empt the HN critique by stating the
parse-overhead truth ourselves in the launch post.

---

## PowQL differentiation — the v0.6 north star (PROPOSED, pending week-1 experiment)

**Why PowQL-as-shipped adds nothing:** the differentiated vision from `docs/design/powql-language-design.md`
(links-not-joins, nested object-graph results, set-based nullability) was **designed but never wired** —
`link`/`let`/`match` are reserved keywords with no grammar; `Value` is flat (no list/struct variant);
only the `Empty`-not-`NULL` semantics shipped. The team built the SQL-equivalent subset (joins,
subqueries, window fns) and deferred every part that *isn't* SQL. So the shipped language is "SQL
reordered" — which is pure burden. The original thesis wasn't empty; its payload was never loaded.

**What the precedent research (PRQL, Malloy, Datomic, Cypher/GQL, jq, dplyr, LLM-SQL) proves:**
> Every genuinely *structural* non-SQL win in history comes from the **engine / storage / data model**,
> never from the syntax. Pipeline syntax (PRQL, dplyr, jq) wins developer *love* but zero capability —
> PRQL compiles to SQL, has ~11k stars, and has displaced nothing.

PowDB's one rare asset is that it **owns the whole stack**, so it can make engine-rooted capabilities
*guaranteed default semantics on every deployment* — which the compiles-to-SQL competitors structurally
cannot (Malloy's flagship nested-results feature is **absent on Postgres/MySQL** because it doesn't own
its engine). Ranked wedges, all engine-owned, all within PowDB's relational domain:

| Wedge | Truly structural? | Notes |
|---|---|---|
| **1. Correct-by-default aggregation across relationships** (Malloy "symmetric aggregates") | ◑ correctness-DX, not "impossible" | SQL is **wrong by default** on one-to-many join aggregation (fan-out double-counts: `avg` 8.67 vs true 12.92). But SQL *can* be made correct (hash-and-subtract) — so it's "right-by-default" DX, not a capability SQL lacks. Cheapest to build, most visceral demo. |
| **2. Native nested / non-1NF result graphs** (EdgeQL/Malloy/GraphQL) | ✗ verbosity, NOT structural | SQL *already does* nested fetch (`json_agg`/`jsonb_build_object`); GraphQL/Drizzle/Prisma do it too. **EdgeQL bet its whole product on exactly this as a language and DIED (shut down Dec 2025, acqui-hired to Vercel to NOT do databases)** — founder named the non-SQL language itself as the core liability. DX win, not a moat. Build only if the experiment proves it matters. |
| 3. Time-travel / as-of (Datomic) | ✅ structural, but storage-model | The one in-domain *genuinely* structural option — but PowDB's WAL is append-**heavy** not append-**only** (truncated after replay), so true as-of needs *retaining* history = a storage-cost dial + architectural change. Weak acquisition feature (Datomic 0.1% mindshare, declining, even when free). Niche audit/debug, not the lead. |
| ❌ Graph traversal (Cypher/GQL) | ✅ structural, but off-domain | Genuinely structural (ISO/IEC 39075, 2024) but not PowDB's domain unless it pivots to graphs. Off the table. |
| ⚠️ LLM-friendly grammar | ✗ probably net-negative | Downgraded from the GTM bet. Clean grammar fixes *syntax* (minority error class) + deletes dialect/NULL ambiguity, but does **nothing** for schema-linking (#1 text-to-SQL error) and **forfeits SQL's training priors** (novel "Rel" scored 40.8% vs 60–80% SQL). Test before believing; do not market as a lead claim. |

**Reality check (KQL/SPL/EdgeQL precedent stream) — the tough truth:** non-SQL languages survive ONLY
when (1) they express something SQL is structurally worse at AND that something lives in the **engine /
data model, not the syntax**, AND (2) that something is the **central job of a defensible niche**. KQL &
SPL passed both (exploratory, schema-loose telemetry — the pipeline's exploration "prefix property" + a
purpose-built engine). **EdgeQL failed both — general-purpose app-backend, which is SQL's home turf — and
died.** PowDB is *also* general-purpose transactional = SQL's home turf = where the ecosystem tax (no BI,
no ORMs, no transferable SQL knowledge, hiring friction) is densest and killed EdgeQL. **Pipeline syntax
is provably NOT a moat** — Google bolted `|>` onto SQL (VLDB 2024), PRQL transpiles to SQL, Splunk's own
SPL2 compiles to SQL. So:

- **The dual-SQL decision is not just "nice" — it is the survival on-ramp EdgeQL added too late.** It makes
  the ecosystem tax opt-in instead of mandatory. This is the single most important strategic move.
- **PowQL's wedges (1, 2) are real DX improvements, NOT structural moats.** They justify offering PowQL
  *alongside* SQL; they do **not** justify betting the product on the language. Betting on the language in
  SQL's home turf is the EdgeQL failure path.
- **The only genuinely structural in-domain option is time-travel (#3)** — and it's niche + needs an
  append-only storage commitment. A deliberate, separate bet, not the headline.

**Honest thesis to stand behind:** *"PowDB's moat is the engine — compiled-execution speed, pure-Rust,
owns the whole stack. PowQL is a better-DX optional surface (right-by-default aggregates, nested results)
for those who want it; SQL is the on-ramp so nobody pays an ecosystem tax to adopt us. We are not betting
the product on the language — that's how EdgeQL died on SQL's turf."*

**Falsifiable week-1 experiment (do before committing v0.6 engineering):**
1. **Killer demo — fan-out aggregate:** one-to-many (`User`→`posts`); show SQL `avg(...)` returning the
   inflated wrong answer vs PowQL graph-aware `avg` returning the correct one **by default**.
2. **Nested fetch:** implement `User { .name, posts: .posts { .title } }` → nested JSON for one shape;
   measure vs the SQL `JOIN`+regroup / `json_agg` equivalent on ergonomics, round-trips (N+1), correctness.
3. (Optional, separate) LLM head-to-head: PowQL vs SQL generation accuracy on identical schemas — to
   confirm/kill the LLM angle with data, not assumption.
If wedges 1–2 are meaningfully better on real app patterns → invest in v0.6. If marginal → keep PowQL a
thin optional surface and lead with engine + SQL.

---

## Backlog (v0.5.x / v0.6 — not blocking)

Composite indexes · B+tree merge-on-delete + slotted-page compaction (space reclamation under churn) ·
correlated/scalar subqueries · CTEs/derived tables (the AST source-enum change) · **MVCC** (only if
multi-writer concurrency becomes a goal — the next big architectural lift) · Windows support ·
vector type (if pursuing the AI-memory positioning) · HTTP `/healthz` + metrics · TS-client pool
dead-socket detection · fuzzy error suggestions everywhere.

## Execution model (per orchestration preference)

PM (Claude) + disjoint-lane dev agents + reviewer + bug-hunter, no worktrees, stacked into one PR per
release. v0.4.9 is small (direct). For v0.5.0, lanes: **Transactions** (storage/catalog/wal + server)
and **Format** (storage/catalog/wal) both touch storage → sequence or single lane; **SQL frontend**
(new isolated query files) and **Cleanup** (cli/ts/parser-errors) are disjoint and parallelizable.
