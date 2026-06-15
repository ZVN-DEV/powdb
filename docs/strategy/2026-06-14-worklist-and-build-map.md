# PowDB / PowQL — Deep Worklist & Build Map

_Date: 2026-06-14 · Companion to `docs/strategy/2026-06-14-direction-and-hardening-roadmap.md`._
_Purpose: a single implementation-ready task doc to execute from. Captures everything from the 2026-06-14 product review + the two code-grounded feasibility investigations + the four PowQL-precedent research streams, **plus** the unbuilt "magic" fully specced, **plus** a new consolidation-engine direction (native search / vectors / live queries / views / documents)._

## How to read this doc

- **[FACT]** = grounded in the actual codebase (file/line refs from the investigations).
- **[PROPOSED]** = a design recommended; not yet in the code. Schema/format proposals are flagged **[DECIDE]** because they're hard to change later — get eyes on these before building.
- **LOC** estimates are rough, for sequencing only.
- Phases are ordered by the locked sequencing: security → v0.5.0 → v0.6 → v0.7+. Don't let the exciting stuff jump the queue ahead of the security patch.

## Locked decisions (recap)

1. **Dual frontend** — keep PowQL native, add a SQL frontend compiling to the same plan tree. Reverses the old "no SQL ever" stance.
2. **Security patch (v0.4.9) ships first**, then phased v0.5.0.
3. Branch + PR per task (no direct push). Live Fly instance is exposed — security is genuinely urgent.

---

# Phase 0 — v0.4.9 Security patch (GREENLIT · do first · independent of all strategy)

The live Fly instance has two trivially-craftable remote-crash bugs. This is a standalone, mergeable patch.

| # | Task | Detail | Status |
|---|---|---|---|
| 0.1 | Fix division-by-zero panic | `eval.rs:666` — guard the divisor; return a query error, not a panic. Remote DoS. **[FACT]** | ✅ |
| 0.2 | Fix `LIMIT` pre-allocation crash | Attacker supplies a huge `LIMIT` → pre-alloc OOM/panic. Cap or lazily allocate. Remote DoS. **[FACT]** | ✅ |
| 0.3 | Tighten data-dir permissions | Restrict on-disk data directory perms (0700). **[FACT]** | ✅ |
| 0.4 | Regression tests for 0.1–0.3 | Craft the malicious queries as tests so they can't regress. | ✅ |
| 0.5 | Branch + PR + deploy | Ship to the live instance once merged. | ☐ |

**Acceptance:** both malicious queries return clean query-level errors; fuzz the arithmetic + `LIMIT`/`OFFSET` paths briefly while you're in there.

---

# Phase 1 — v0.5.0 The foundation (transactions + format + SQL + cleanup)

This is the big release. Four workstreams; **1A and 1B can parallelize**, 1C depends on nothing, 1D is cleanup.

## 1A — Real transactions

**Review said "inert placeholders." That was wrong** — transactions are ~70% built and broken in three specific places. Single-writer architecture means **no MVCC needed**. Hold the write lock for the transaction's lifetime + redo-with-commit-markers + two-pass replay → serializable isolation. ~400–650 LOC total. **[FACT]**

| # | Task | Detail | LOC |
|---|---|---|---|
| 1A.1 | **Bug A — per-connection txn state + lock lifetime** | Server drops the write lock *between* statements (`handler.rs:585`), and `in_transaction` is a single shared flag on `Engine`, not per-connection. Result over the wire: connection X's `begin` captures connection Y's writes; readers see uncommitted dirty pages. Fix: (a) move txn state to per-connection; (b) acquire the write lock on `begin` and hold it until `commit`/`rollback`. **[FACT]** | ~150–250 |
| 1A.2 | **Bug B — rollback soundness across page eviction** | Rollback assumes uncommitted writes live only in memory, but the heap evicts dirty pages to disk mid-txn, so any txn touching >1 page can't cleanly roll back. Masked today only because tests insert 1–2 rows. Fix: track the txn's dirty page set / undo info so eviction doesn't lose rollback ability (before-images or page-level undo log). **[FACT]** | ~150–250 |
| 1A.3 | **Bug C — commit markers + two-pass replay** | No `Commit` marker is written; replay is redo-only and ignores txn boundaries, so a crash mid-txn (WAL auto-flushes every 64 records) replays a *torn* transaction. Fix: write a `Commit` WAL record; replay in two passes (pass 1: scan for committed txn IDs; pass 2: redo only committed records). **[FACT]** | ~100–150 |
| 1A.4 | Multi-page, multi-row, abort, and crash-recovery tests | Current tests insert 1–2 rows and hide B & C. Add: txn touching many pages + rollback; commit then crash-replay; begin → crash (no commit) → must NOT replay; concurrent connections interleaving. | — |

**Acceptance:** ACID for multi-statement txns over the wire; a killed process mid-txn never leaves a torn write; concurrent connections never see each other's uncommitted data.

> **Note for later (1A.2 ↔ backlog):** the before-image/undo approach chosen here is the same machinery time-travel (Phase 2/§Backlog) would build on. If as-of queries are at all likely, design the undo log so it *could* be retained rather than discarded — flag at implementation time.

## 1B — Stable on-disk format

Half-versioned today. B+tree (`BIDX v1`) and catalog (`BCAT v3`) already have magic + version + reject-unknown guards. Four things don't. Template to copy: `backup/src/manifest.rs:25`. **[FACT]**

| # | Task | Detail | Priority |
|---|---|---|---|
| 1B.1 | **Row encoding** | Highest blast radius — silent misdecode, zero guard. Add magic + version. Do this FIRST. **[FACT]** | 🔴 |
| 1B.2 | **Heap pages** | Add magic/version guard. **[FACT]** | 🟠 |
| 1B.3 | **WAL** | Add magic/version + reject-unknown. **[FACT]** | 🟠 |
| 1B.4 | **Heap file (superblock)** | Special case: "no version field ⇒ v1" works everywhere *except* here, because page 0 is live data. Needs a superblock or sentinel. **[FACT] [DECIDE]** — superblock vs. sentinel is a permanent format choice; get eyes on it. | 🔴 |
| 1B.5 | Cross-version read tests + a `format_version` introspection command | Prove old files reject cleanly and current files round-trip. | 🟢 |

**Acceptance:** every persisted structure has magic + version + reject-unknown; an unknown version fails loudly, never silently misdecodes. **This is also the prerequisite for every new on-disk structure below** (search/vector indexes, document columns) — they inherit the versioning discipline from day one.

## 1C — SQL frontend

The engine's `Statement`/`Expr` AST is already a clean, frontend-agnostic relational IR; `plan_statement()` is pure; the plan cache keys on a normalized token hash, not raw text. A SQL parser targeting the same AST reuses planner + executor + plan cache **100% unchanged**, and gets identical plan-cache amortization. **[FACT]**

| # | Task | Detail |
|---|---|---|
| 1C.1 | SQL lexer + parser → existing AST | Target `Statement`/`Expr` directly. Cover the shipped surface first (select/insert/update/delete, joins, subqueries, window fns, aggregates, UNION). |
| 1C.2 | Frontend dispatch | Detect/route SQL vs PowQL per statement or per connection. **[DECIDE]** how a client picks the dialect (connection param? statement sniff? separate port?). |
| 1C.3 | Parity test matrix | Same query in PowQL and SQL → identical plan tree → identical results. This matrix also becomes the differentiation benchmark harness in Phase 2. |
| 1C.4 | Document the dialect boundary | What SQL is/isn't supported; where PowQL-only features (Phase 2) have no SQL equivalent. |

**Acceptance:** a standard SQL client can talk to PowDB for the shipped feature set; SQL and PowQL produce identical plans for equivalent queries.

## 1D — Cleanup

| # | Task | Detail |
|---|---|---|
| 1D.1 | **Resolve BufferPool dead code** | A fully-implemented clock-sweep eviction module sits unused next to the real hand-rolled caching. A reviewer will (correctly) flag the confusing parallel implementation. **[DECIDE]:** wire it in (replace hand-rolled caching) **or** delete it. Recommendation: if the hand-rolled cache is load-bearing and tested, delete the BufferPool to kill confusion; if BufferPool is genuinely better (clock-sweep eviction is a real improvement under memory pressure), wire it in and delete the hand-rolled path. Don't ship both. **[FACT]** |

---

# The Divergence Deep-Dive — original intent vs. what shipped, and how to build the magic back

The exciting original idea isn't *gone*, it was **designed and never wired**. Below is the full map of the gap and the concrete build plan to close it.

## What happened (the honest narrative)

`docs/design/powql-language-design.md` describes a genuinely differentiated language. The team shipped the subset of it that **overlaps SQL** (joins, subqueries, window functions, aggregates, UNION) and **reserved every part that doesn't** (links, nested results, `let`, `match`, `??`). So what shipped reads as "SQL with the clauses reordered" — pure burden — while the differentiating payload sits in three reserved keywords and a design doc. **The thesis wasn't empty; its payload was never loaded.** **[FACT]**

## The gap table

| Designed (the exciting part) | Shipped? | What's actually missing in code |
|---|---|---|
| **Links, not joins** — `user.posts` traverses a relationship as first-class | ❌ | `Token::Link` exists but the 3 refs are keyword-spelling in a *display* function (`out.push_str("link")`), **not grammar**. No parse rule, no planner support, no link schema type. **[FACT]** |
| **Nested object-graph results** — `User { name, posts: .posts { title } }` | ❌ | `Value` enum is **flat**: `Int/Float/Bool/Str/DateTime/Uuid/Bytes/Empty`. **No `List`/`Struct` variant** ⇒ nested results are literally unrepresentable. Wire protocol assumes flat rows. **[FACT]** |
| **Deep / reverse link traversal** | ❌ | No link infra at all. **[FACT]** |
| **`let` bindings, `match` operator, `??` default** | ❌ | Reserved keywords, no grammar behind them. **[FACT]** |
| **Set-based nullability** (`{}` not `NULL`, no 3-valued logic) | ✅ | Shipped via `Value::Empty`. The one differentiated thing that actually exists. **[FACT]** |
| Joins / subqueries / window fns / aggregates / UNION | ✅ | This is SQL, reordered. |

## What it takes to build the magic (the foundation unlocks everything)

There's one keystone: **the nested `Value` types**. Build `Value::List` and `Value::Struct` and you unlock nested results, links, AND (see consolidation section) document columns and richer search results. Everything routes through this. **[PROPOSED]**

### Foundation task: nested Value model **[PROPOSED] [DECIDE — schema/format]**

| # | Task | Detail |
|---|---|---|
| M.1 | Add `Value::List(Vec<Value>)` and `Value::Struct(Vec<(FieldName, Value)>)` | The single highest-leverage change. Touches: value encoding (needs 1B.1 row-format versioning first), comparison/ordering rules for nested values, display/serialization. **[DECIDE]** field-name storage (interned? positional?) — permanent. |
| M.2 | Wire-protocol: nested rows | Current protocol assumes flat rows. Add a nested/structured row encoding (length-prefixed recursive, or a JSON-ish frame). Version it. **[DECIDE]** — protocol change, breaks old clients; do it once. |
| M.3 | Result-shaping in the executor | Let the executor emit a `Struct`/`List`-shaped result, not just flat tuples. The "shape" comes from the query (the `{ ... }` projection block). |

### Magic feature 1: Links (relationships as first-class) **[PROPOSED]**

| # | Task | Detail |
|---|---|---|
| M.4 | `link` / `multi link` schema types | A column type that points at another table's PK (single) or many (reverse). Catalog gets a link descriptor. **[DECIDE — schema]** keep Kirby in the loop; this is a real data-model addition. |
| M.5 | Link-traversal grammar + planner | Parse `.posts`, `user.posts`, reverse links, deep traversal. Planner turns a traversal into the join it already knows how to execute (links are sugar + shape over joins under the hood — reuse the executor). |
| M.6 | Nested projection grammar | `User { .name, posts: .posts { .title } }` → executor emits a `Struct` with a nested `List` of `Struct`s (uses M.1–M.3). |

### Magic feature 2: language ergonomics **[PROPOSED]**

| # | Task | Detail |
|---|---|---|
| M.7 | `let` bindings | Bind intermediate expressions/subresults within a query. Grammar + scope handling in the planner. |
| M.8 | `match` operator | **NOTE:** `match` is also the natural keyword for full-text search (see consolidation §S). Decide whether `match` means pattern-match (the design doc) or search-match — or both via overload. **[DECIDE]** — don't paint the keyword into a corner. |
| M.9 | `??` default operator | Coalesce-with-default, clean against set-based `{}` nullability. Small. |

### The right-by-default aggregate wedge (Malloy "symmetric aggregates") **[PROPOSED]**

| # | Task | Detail |
|---|---|---|
| M.10 | Graph-aware aggregation | SQL is **wrong by default** on one-to-many join aggregation (fan-out double-counts: `avg` returns 8.67 vs the true 12.92). Because PowDB retains the join/link graph, `avg`/`sum` over a link can be correct by default. This is the most visceral demo: "SQL gives you the wrong number; PowQL gives you the right one." Requires links (M.4–M.6) so the engine knows the relationship cardinality. |

### ⚠️ Honest framing to keep attached to all of M.* (from the precedent research)

- Nested fetch is **not structural** — Postgres `json_agg`/`jsonb_build_object` already returns nested objects; GraphQL/Drizzle/Prisma occupy that space. **EdgeQL bet its whole product on exactly this as a language and shut down (Dec 2025)**, founder naming the non-SQL language itself as the core liability on SQL's home turf.
- Pipeline syntax is **provably not a moat** — Google bolted `|>` onto SQL (VLDB 2024); PRQL transpiles to SQL; Splunk's SPL2 compiles to SQL.
- ∴ **Build M.* as better-DX surfaces offered _alongside_ SQL, not as a product bet.** The dual-SQL decision is the survival on-ramp EdgeQL added too late. Validate with the Phase-2 experiment before committing the bigger M.* engineering.

---

# Phase 2 — v0.6 PowQL differentiation experiment (gated · falsifiable · do before committing M.* heavy work)

Prove the wedges clear the (now higher) bar before investing. One week, end-to-end on one query shape each.

| # | Experiment | Measures |
|---|---|---|
| 2.1 | **Fan-out aggregate demo** | One-to-many (`User`→`posts`); SQL `avg(...)` returns inflated wrong answer vs PowQL graph-aware `avg` returns correct, **by default**. Cheapest + most visceral. (Needs minimal M.4/M.10.) |
| 2.2 | **Nested fetch demo** | `User { .name, posts: .posts { .title } }` → nested JSON for one shape; measure vs SQL `JOIN`+regroup / `json_agg` on ergonomics, round-trips (N+1), correctness. (Needs minimal M.1–M.6.) |
| 2.3 | (Optional) **LLM head-to-head** | PowQL vs SQL generation accuracy on identical schemas. ⚠️ Prior evidence: novel relational language "Rel" scored 40.8% vs 60–80% SQL — clean grammar fixes *syntax* but forfeits SQL training priors and doesn't help schema-linking (#1 text-to-SQL error). Cheap to test; **do not market "LLMs love PowQL"** without this. |

**Decision rule:** wedges meaningfully better on real app patterns → invest in full M.*. Marginal → keep PowQL thin, lead with engine + SQL.

---

# The Consolidation Engine — the stronger differentiation thesis (NEW · "people hate syncing separate systems")

The search-index point isn't a side feature — it points at a **better moat than the PowQL language**, consistent with the one law every precedent stream agreed on:

> Every genuinely structural non-SQL win in history comes from the **engine / storage / data model**, never the syntax.

Search, vectors, live queries, materialized views, and documents are all **engine capabilities**, and they all share one property: today people run a **second system** for them and pay a brutal **sync tax** (CDC, dual writes, cache invalidation, eventual-consistency bugs). PowDB **owns the whole stack**, so it can offer these **transactionally consistent with the base data, in one process, no sync** — which is exactly what a bolt-on external system structurally cannot. That's a real moat, and it's engine-rooted.

**Proposed thesis upgrade:** *"PowDB is the consolidation engine — the things you currently run a second system for (search, vectors, live updates, materialized derived state, documents) are native and transactionally consistent because we own the whole stack. SQL is the on-ramp; PowQL is better DX on top; the engine is the moat."* This subsumes the PowQL story instead of replacing it.

All five features below depend on **1B (format versioning)** for their on-disk structures and most depend on **M.1 (nested Values)**. Ranked by leverage × fit × effort.

## §S — Native full-text search index 🥇 (lead idea — strongest fit)

**The pain (real and universal):** apps run Elasticsearch / Meilisearch / Typesense *beside* Postgres and sync documents in via CDC or dual writes. It's operationally heavy, perpetually out of sync, and a top source of "why is search showing deleted records" bugs. SQL `LIKE`/views don't solve ranked inverted-index search; Postgres FTS exists but is weak and bolted-on.

**Why PowDB wins:** an inverted index maintained **in the same transaction as the write** ⇒ search results are never stale, never out of sync, no pipeline. That's the thing external engines can't give you.

| # | Task | Detail | **[tag]** |
|---|---|---|---|
| S.1 | Inverted-index structure | Term → posting list (row-id + positions). New paged on-disk structure; magic+version per 1B. | **[PROPOSED] [DECIDE — format]** |
| S.2 | Analyzer pipeline | Tokenize → lowercase → stopwords → stemming. Make analyzers configurable per index. | **[PROPOSED]** |
| S.3 | Ranking | BM25 default (TF-IDF fallback). | **[PROPOSED]** |
| S.4 | Transactional maintenance | Index updates ride the same WAL/txn as the base write (depends on 1A). **This is the whole moat — get it right.** | **[PROPOSED]** |
| S.5 | Query surface | PowQL `match` operator (ties to M.8 — decide keyword semantics) + a SQL function/operator for the SQL frontend. Returns rows ranked by score. | **[PROPOSED] [DECIDE]** |
| S.6 | Delete/update handling | Inverted indexes accumulate tombstones; note the existing B+tree never merges on delete (space leak) — don't repeat that. Plan posting-list compaction from the start. | **[PROPOSED]** |

**Killer demo:** insert a row and search for it **in the same transaction** — it's there, ranked, consistent. Then `rollback` — it's gone from search too. No external engine can do that.

## §V — Native vector / embedding search 🥈 (on-trend · fits AI-integration focus)

**The pain:** everyone bolts on pgvector or a separate vector DB (Pinecone/Weaviate/Qdrant) for RAG/semantic search — another system, another sync.

| # | Task | Detail | **[tag]** |
|---|---|---|---|
| V.1 | Vector column type | Fixed-dim float vector (uses M.1 `Value::List` foundation or a dedicated `Value::Vector`). **[DECIDE]** | **[PROPOSED]** |
| V.2 | ANN index | HNSW or IVF-flat. Paged + versioned per 1B. Start with brute-force exact for correctness, add ANN for scale. | **[PROPOSED]** |
| V.3 | Distance ops + query surface | Cosine / L2 / dot; `ORDER BY embedding <-> $q LIMIT k` in SQL and a PowQL equivalent. | **[PROPOSED]** |
| V.4 | **Hybrid search** | Combine §S full-text + §V vector in one ranked query (the modern RAG pattern). This is the genuinely differentiated combo — FTS + vector + transactional consistency in one engine is rare. | **[PROPOSED]** |

**Why it's high-value:** "transactional Postgres-class engine + native FTS + native vectors + no sync" is a *very* current, very real consolidation story.

## §L — Live queries / subscriptions 🥉 (real pain · strong owned-stack fit)

**The pain:** to react to data changes apps poll the DB, run LISTEN/NOTIFY hacks, or stand up a separate message queue / Convex / Electric. Owning the stack means PowDB can push.

| # | Task | Detail | **[tag]** |
|---|---|---|---|
| L.1 | Subscribe-to-query API | Client registers a query; server pushes updated results on relevant commits. | **[PROPOSED] [DECIDE — protocol]** |
| L.2 | Change detection | Diff against the committed write set (the txn dirty-set from 1A is reusable here). Start coarse (re-run query on any matching-table commit), refine to incremental later. | **[PROPOSED]** |
| L.3 | Wire-protocol push frames | Needs the nested/structured protocol (M.2) for shaped live results. | **[PROPOSED]** |

⚠️ Caveat: this nudges PowDB toward an app-backend story (where the ecosystem tax bit EdgeQL). Offer it as a feature on the engine, not as the identity. Lower priority than §S/§V.

## §MV — Materialized views with incremental maintenance

**The pain:** the cache-invalidation problem — people manually maintain Redis caches / summary tables and they drift.

| # | Task | Detail | **[tag]** |
|---|---|---|---|
| MV.1 | Plain views | Store the query AST under a name; expand at plan time. Cheap, easy win — do this early, it's nearly free. | **[PROPOSED]** |
| MV.2 | Materialized views | Persist results; refresh on demand. | **[PROPOSED]** |
| MV.3 | Incremental view maintenance | Update the materialized result transactionally on base-table writes (Materialize's insight). Real engine work; **expressible in SQL too, so it's an engine feature, not a language differentiator** — value it as consolidation, not as a PowQL wedge. | **[PROPOSED]** |

## §D — Schema-less / document columns

**Honest recommendation:** don't build a Mongo competitor; **do** add document columns, because the cost is *already paid* by M.1. Once `Value::Struct`/`Value::List` exist for nested results, a JSONB-style document column type + path indexing is a small marginal step — and it composes with §S (search *inside* documents) and §V (embed documents) into the consolidation story. The risk is identity dilution ("are we relational or a doc store?"), so scope it as **"relational engine that also speaks documents,"** not "Mongo."

| # | Task | Detail | **[tag]** |
|---|---|---|---|
| D.1 | Document/JSONB column type | Reuses M.1 nested Values. Store arbitrary nested docs in a typed column. | **[PROPOSED] [DECIDE — scope]** |
| D.2 | Path access + path indexes | `doc.a.b[0]` access; GIN-style index on document paths (reuses §S inverted-index infra). | **[PROPOSED]** |
| D.3 | Schema-on-read queries | Query documents without rigid schema, mixed with relational columns in one table. | **[PROPOSED]** |

**Recommendation:** ship D.1 opportunistically once M.1 lands; gate D.2/D.3 on real demand. Don't lead with "schema-less" — lead with "you can mix structured + document columns in one transactional table, and search both."

## Other recurring pains considered (logged, not yet scheduled)

- **Time-travel / as-of** — genuinely structural and on-brand for a WAL, but PowDB's WAL is append-*heavy* not append-*only* (truncated after replay), so true as-of needs retaining history = a storage-cost dial + architectural change. Weak acquisition feature (Datomic ~0.1% mindshare, declining, even when free). **Niche audit/debug positioning, not a headline.** Design 1A.2's undo log so it *could* feed this.
- **Graph traversal (Cypher/GQL)** — genuinely structural but off-domain unless PowDB pivots to graphs. Off the table.
- **Connection-level write scaling** — the single write-RwLock caps write concurrency. Real eventual ceiling; out of scope for now, note it.

---

# Backlog (not blocking any release)

| Item | Why it matters |
|---|---|
| Composite indexes | Multi-column lookups; common real need. **[FACT — known gap]** |
| B+tree merge-on-delete + rebalance | Space leak under churn today (never merges). **[FACT — known gap]** Also informs §S.6 (don't repeat the leak in the inverted index). |
| Slotted-page compaction | Space reclamation under churn. **[FACT — known gap]** |
| Correlated / scalar subqueries | Currently unsupported. **[FACT — known gap]** |
| Cross-platform (currently Unix-only) | Windows support widens adoption. **[FACT — known gap]** |

---

# Open decisions (schema/format/permanent — keep Kirby in the loop)

1. **1B.4** — heap-file superblock **vs.** sentinel for format versioning. _(Permanent on-disk choice.)_
2. **1C.2** — how clients select SQL vs PowQL dialect (connection param / statement sniff / separate port).
3. **1D.1** — wire in BufferPool **or** delete it. (Lean: delete unless clock-sweep measurably beats the hand-rolled cache under memory pressure.)
4. **M.1/M.2** — nested `Value` storage (interned vs positional field names) + nested wire-protocol encoding. _(Permanent; unlocks everything below it.)_
5. **M.4** — `link`/`multi link` schema types — a real data-model addition.
6. **M.8 vs S.5** — does `match` mean pattern-match (design doc) or search-match (FTS)? Don't corner the keyword.
7. **Thesis call** — adopt the **consolidation-engine** framing (search/vectors/live/views/docs, transactionally consistent, owns the stack) as the headline differentiation, with PowQL as DX-on-top and SQL as the on-ramp? Recommendation: **yes** — it's engine-rooted (where real moats live), answers the "people hate syncing a second system" insight, and doesn't bet the product on the language the way EdgeQL fatally did.

---

# Suggested sequencing

```
NOW    →  v0.4.9  Security patch (Phase 0)                    [greenlit, days]
THEN   →  v0.5.0  1A txns ‖ 1B format ‖ 1C SQL ‖ 1D cleanup   [the foundation]
              └─ land M.1 nested Values + MV.1 plain views here if cheap (they unlock everything)
GATE   →  v0.6    Phase-2 experiment (fan-out + nested fetch) [decide if M.* is worth it]
BIG BET→  v0.6/7  §S native FTS  →  §V vectors + hybrid        [the consolidation moat]
LATER  →  §L live queries · §MV IVM · §D documents · backlog
```

**Bias:** §S (native transactional full-text search) is the single most defensible, most-demanded, best-fit item in this whole doc. If the Phase-2 PowQL experiment comes back marginal, **§S becomes the headline feature, not PowQL.** Either way, the engine is the moat.
