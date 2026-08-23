# AGENTS.md: PowDB & PowQL for AI assistants and humans

This file exists so that an agent (or a person) who has never seen PowDB can walk into a task and write correct code on the first try. If you update the language or the wire protocol, update this file in the same commit.

**Authoritative references:** [`docs/POWQL.md`](docs/POWQL.md) is the full language reference. [`docs/getting-started.md`](docs/getting-started.md) is the tutorial. This file is the 5-minute version: opinionated, with the footguns called out.

---

## What PowDB is

PowDB is an embeddable database engine written from scratch in Rust. Its native query language is **PowQL**; since v0.5.0 it also accepts a supported subset of **SQL** through a frontend that lowers to the same PowQL AST (see `docs/SQL.md`). PowQL is the native, fastest path. The core thesis:

> Most of what a SQL engine does is *translate your query* into something executable. We remove that tier. PowQL is designed so the parser's AST **is already a plan tree**: no rewriting, no cost-based planning, no bytecode VM.

The measurable result: 3-7x faster than SQLite on aggregate and scan workloads, and slower on indexed point lookups, where the front-end cost dominates the actual probe (measured 1.65us against SQLite's 208ns). The current numbers, the methodology, and the workloads PowDB loses are in `docs/benchmarks/2026-07-24-wide-bench-snapshot.md`.

### When PowDB is the right choice

- **Embedded / edge / serverless workloads** where query latency is a tight budget and you don't want SQLite's quirks.
- **Single-node analytics** over tables that fit on disk. The scan path is zero-syscall (mmap) and filters are compiled to byte-level predicates.
- **You control both sides** (the DB and the app). PowDB has no Postgres wire protocol, no ODBC, no legacy compatibility. The client is a TCP binary protocol or an in-process Engine.
- **You want to read the code.** Eleven crates, ~145K lines of Rust (~102K outside integration-test files), no generated parsers, no plan-language IR.

### When it's *not* the right choice

- You need a drop-in Postgres/MySQL replacement. PowDB has a SQL frontend (a supported subset that lowers to PowQL, see `docs/SQL.md`), but it is **not** wire-compatible with Postgres/MySQL, does not implement full SQL, and has no ODBC/JDBC or Postgres wire protocol. Treat SQL as a convenience surface, not a compatibility layer; PowQL remains the native path.
- You need multi-node replication, sharding, or Raft-style consensus. Single-node only today.
- You need fine-grained ACLs, row-level security, or multi-tenant isolation. Auth is named users with coarse roles (admin/readwrite/readonly) since 0.4.5, and shared-password mode is still available, but nothing per-table or per-row.
- You need user-defined functions, stored procedures, or triggers.

---

## PowQL in 60 seconds

PowQL reads **left-to-right in execution order**: source → filter → order → limit → projection.

```powql
User filter .age > 25 order .age desc limit 10 { .name, .age }
```

That's:
1. Start with the `User` table.
2. Keep rows where `age > 25`.
3. Sort descending by `age`.
4. Take the first 10.
5. Project `name` and `age`.

Compare SQL: `SELECT name, age FROM User WHERE age > 25 ORDER BY age DESC LIMIT 10`. Same result, inside-out reading order.

### The five things that trip people up

1. **Equality is `=`, not `==`.** Assignment in insert/update is `:=`.
2. **Field refs inside operators have a leading dot:** `.age`, `.city`. Naked identifiers are tables or aliases.
3. **The statement keyword for CREATE TABLE is `type`**. There is no `create table`.
4. **Projection is trailing braces**, not `SELECT`. `User { .name, .age }` is a projection, not row construction.
5. **DDL uses `alter`**, not pseudo-verbs. `alter User add column`, `alter User drop column`, `alter User add index .col`.

---

## PowQL ↔ SQL cheat sheet

| Task | PowQL | SQL |
|---|---|---|
| Define a table | `type User { required name: str, age: int }` | `CREATE TABLE User (name TEXT NOT NULL, age INT)` |
| Drop a table | `drop User` | `DROP TABLE User` |
| Add a column | `alter User add column status: str` | `ALTER TABLE User ADD COLUMN status TEXT` |
| Drop a column | `alter User drop column status` | `ALTER TABLE User DROP COLUMN status` |
| Create an index | `alter User add index .email` | `CREATE INDEX ON User (email)` |
| Unique column | `type User { unique email: str }` | `CREATE TABLE User (email TEXT UNIQUE)` |
| Add unique constraint | `alter User add unique .email` | `CREATE UNIQUE INDEX ON User (email)` |
| Insert | `insert User { name := "Alice", age := 30 }` | `INSERT INTO User (name, age) VALUES ('Alice', 30)` |
| Scan a table | `User` | `SELECT * FROM User` |
| Filter | `User filter .age > 30` | `SELECT * FROM User WHERE age > 30` |
| Project | `User { .name, .age }` | `SELECT name, age FROM User` |
| Alias a column | `User { n: .name }` | `SELECT name AS n FROM User` |
| Order | `User order .age desc` | `SELECT * FROM User ORDER BY age DESC` |
| Limit / Offset | `User limit 10 offset 20` | `SELECT * FROM User LIMIT 10 OFFSET 20` |
| Distinct | `User distinct { .city }` | `SELECT DISTINCT city FROM User` |
| Count all | `count(User)` | `SELECT COUNT(*) FROM User` |
| Filtered aggregate | `count(User filter .age > 30)` | `SELECT COUNT(*) FROM User WHERE age > 30` |
| Sum a column | `sum(User { .age })` | `SELECT SUM(age) FROM User` |
| Group + aggregate | `User group .city { .city, n: count(.name) }` | `SELECT city, COUNT(name) n FROM User GROUP BY city` |
| HAVING | `User group .city { .city, n: count(.name) } having n >= 2` | `SELECT city, COUNT(*) n FROM User GROUP BY city HAVING n >= 2` |
| Inner join | `User as u inner join Order as o on u.id = o.user_id { u.name, o.total }` | `SELECT u.name, o.total FROM User u JOIN Order o ON u.id = o.user_id` |
| Left join | `User as u left join Order as o on u.id = o.user_id` | `SELECT ... FROM User u LEFT JOIN Order o ON ...` |
| Declare a link | `link Post.user -> User on user_id = id` | *(no SQL equivalent: a persisted, named relationship)* |
| Traverse a to-one link | `Post as p { p.id, p.user.name }` | `SELECT p.id, u.name FROM Post p JOIN User u ON p.user_id = u.id` |
| Traverse a to-many link | `User as u { u.name, posts: u.posts { title } }` | *(PowQL only: one row per parent, children as a JSON array)* |
| Nested projection | `User as u { u.name, posts: Post as p filter p.user_id = u.id { p.title } }` | *(PowQL only)* |
| IN subquery | `User filter .id in (Order filter .total > 100 { .user_id })` | `SELECT * FROM User WHERE id IN (SELECT user_id FROM Order WHERE total > 100)` |
| EXISTS | `User filter exists (Order filter .user_id = User.id)` | `SELECT * FROM User WHERE EXISTS (SELECT 1 FROM Order o WHERE o.user_id = User.id)` |
| UNION | `(A filter ...) union (B filter ...)` | `SELECT ... UNION SELECT ...` |
| NULL check | `User filter .age = null` / `.age != null` | `WHERE age IS NULL` / `IS NOT NULL` |
| Update | `User filter .id = 1 update { age := 31 }` | `UPDATE User SET age = 31 WHERE id = 1` |
| Update with expr | `User update { age := .age + 1 }` | `UPDATE User SET age = age + 1` |
| Delete | `User filter .age < 18 delete` | `DELETE FROM User WHERE age < 18` |
| Upsert (key must be `unique`) | `upsert User on .id { id := 1, name := "Alice" }` | `INSERT ... ON CONFLICT (id) DO UPDATE ...` |
| CASE | `case when .age > 30 then "old" else "young" end` | `CASE WHEN age > 30 THEN 'old' ELSE 'young' END` |
| Materialized view | `materialize OldUsers as User filter .age > 28` | `CREATE MATERIALIZED VIEW OldUsers AS ...` |

### Things that look right but do **not** parse

| Don't write | Write instead |
|---|---|
| `create table T { ... }` | `type T { ... }` |
| `insert into T { ... }` | `insert T { ... }` |
| `name: string!` | `required name: str` |
| `name = "Alice"` (in insert) | `name := "Alice"` |
| `.city == "NYC"` | `.city = "NYC"` |
| `string`, `varchar`, `text` | `str` *(unknown names silently coerce to `str`: footgun)* |
| `User match T on ...` | `User inner join T on ...` (*`match` is not a keyword*) |
| `User create_index .col` | `alter User add index .col` |
| `User add_column x: int` | `alter User add column x: int` |
| `NULL` | `null` (lowercase) |
| `AND`, `OR`, `NOT` | `and`, `or`, `not` (lowercase) |
| `User.posts` (bare link navigation) | alias the table and label the block: `User as u { posts: u.posts { title } }`; a to-one link reads inline: `Post as p { p.user.name }` |
| `let x := ...` | not yet implemented |
| `count: count(.name)` (aggregate keyword as alias) | fails `expected alias name`; `sum:` fails too; use `n:`, `cnt:`, `total:` |

---

## Type system

Canonical type names: `str`, `int`, `float`, `bool`, `datetime`, `uuid`, `bytes`.

**Footgun:** the executor's type resolver falls back to `TypeId::Str` for any unknown name (`crates/query/src/executor/`), so `string`, `varchar`, or a typo silently produces a Str column with no error. Always use the canonical names above.

`required` is a prefix keyword on the field, not a `!` suffix: `required name: str`, never `name: str!`. `unique` is a sibling prefix keyword (`required unique email: str`, either order) that auto-creates a unique B+tree index and enforces no duplicate non-null values on insert/update/upsert.

**Footgun (since 0.4.7):** `upsert <T> on .<col>` requires `.col` to be **unique**: declare it `unique` in the `type`, or run `alter <T> add unique .<col>` first. Upserting on a non-unique column is now a hard error (this closed a bug where upsert could silently create duplicate keys). `alter add unique` first scans for existing duplicates and fails if any are present; it also rejects a column that already has a non-unique index (no in-place upgrade). Null values are exempt from `unique`.

---

## Why PowDB is fast (the short version)

These are the design moves that buy the speedup. Understanding them keeps you from accidentally undoing them:

1. **Planner is a pure function.** It does not touch the catalog: it emits `RangeScan`/`IndexScan` speculatively. The executor lowers them to `Filter(SeqScan)` at runtime only when no index exists on the column; otherwise it walks the B+tree directly (unique indexes: raw column-value keys; non-unique indexes: composite `(value, rid)` keys via `BTree::range_rids`, heap-fetching matched rows and rechecking exclusive bounds). This keeps the parser → plan pipeline allocation-free for cache hits.
2. **Plan cache hashes canonical PowQL.** Literals are substituted at lookup time (FNV-1a hash, `crates/query/src/plan_cache.rs`). A repeated `User filter .id = <N>` reuses the same plan for all N.
3. **Compiled integer predicates.** `Filter(SeqScan)` on simple numeric predicates compiles into a branch-free byte-level check that skips full row decoding. See `execute_plan` fast paths in `crates/query/src/executor/` (module dir).
4. **mmap-based scans.** The storage layer exposes `try_for_each_row_raw` over memory-mapped heap files. Early termination is a `return ControlFlow::Break`.
5. **Slotted 4KB pages + persistent B+tree indexes.** Standard, but the index format (BIDX, binary) is crash-safe and survives restart with no rebuild.
6. **WAL with group commit at statement boundaries.** Writes are durable by default; throughput is maintained by batching.

If you're changing a hot path, run the regression gate locally: `cargo bench -p powdb-bench && cargo run -p powdb-bench --bin compare`.

---

## Talking to PowDB

### Embedded (in-process)

```rust
use powdb_query::executor::Engine;

let mut engine = Engine::new("./powdb_data")?;
engine.execute_powql("type User { required name: str, age: int }")?;
engine.execute_powql(r#"insert User { name := "Alice", age := 30 }"#)?;
let result = engine.execute_powql("User filter .age > 25 { .name, .age }")?;
```

### CLI / REPL

```bash
cargo run --release -p powdb-cli                      # embedded REPL
cargo run --release -p powdb-cli -- --remote host:5433 --password <pw>
```

**The REPL buffers lines until braces/parens balance**, so multi-line `type`/`insert` paste works; a statement still cannot span two separately-submitted balanced lines.

### TCP server

```bash
cargo run --release -p powdb-server -- --port 5433 --data-dir ./powdb_data
```

Binary length-prefixed framing. **Don't use `nc` or `telnet`**: the server will hang on its `read_exact`.

### TypeScript client

```bash
npm install @zvndev/powdb-client
```

```ts
import { Client } from "@zvndev/powdb-client";

const client = await Client.connect({ host: "localhost", port: 5433 });
const r = await client.query("User filter .age > 25 { .name, .age }");
if (r.kind === "rows") console.table(r.rows);
await client.close();
```

**Parameter binding (`$1`..`$N`).** Pass untrusted values as positional parameters instead of interpolating them into the query string. Placeholders are 1-based `$N` (not `?`, because `??` is the COALESCE operator). Binding happens at the token level on the server: each `$N` is replaced with the literal token for the matching value before parsing, so an injection-shaped string is inert data and can never change the query's shape.

```ts
// Values pass as the second argument, in $1, $2, … order.
await client.query("insert User { name := $1, email := $2, age := $3 }", [name, email, age]);
const r = await client.query("User filter .email = $1 { .name }", [email]);
// null binds PowQL null; numbers bind int when integral, float otherwise.
await client.query("insert User { name := $1, age := $2 }", ["Dana", null]);
```

The params form uses the `QueryWithParams` (0x04) wire message and requires `powdb-server >= 0.4.7`. The plain no-params `query(q)` form is unchanged.

Return shapes:
- `{ kind: "rows", columns: string[], rows: string[][] }`: SELECT-like queries
- `{ kind: "scalar", value: string }`: aggregates
- `{ kind: "ok", affected: bigint }`: mutations and DDL

For lossless values, use `queryNative()` (PowQL) or `querySqlNative()` (SQL). Native results
preserve Empty, booleans, exact integers, floats, UUIDs, datetime microseconds, raw bytes, and
PJ1 JSON without routing the value through text. Integers outside JavaScript's safe range and
datetime values are returned as `bigint`; bytes are `Uint8Array`; JSON is decoded recursively.
The legacy `query()` and `querySql()` result shapes above remain unchanged.

---

## Writing queries that perform

- **Point lookup on an indexed column** is the fast path. `User filter .email = "alice@example.com" { .name }` costs ~200ns parse, ~100ns plan, ~800ns execute with a warm cache. JSON paths can also be indexed with `alter Post add index (.data->slug)` or SQL `CREATE INDEX post_slug ON Post ((data->'slug'))` and serve equality, range, and ordered reads.
- **Sort+limit without an index** uses a top-k heap in the executor, not a full sort. `User order .age desc limit 10` is O(N log K).
- **Joins** use hash join when `on` contains an equi-predicate (`u.id = o.user_id`), including compound predicates with residual conditions. Pure non-equi joins use a bounded nested loop and fail before execution when the estimated pair count exceeds the safety limit. Put the smaller table on the **right**: the hash table is built over the right side.
- **Projections before aggregates save work.** `sum(User filter .active = true { .amount })` is cheaper than decoding the whole row.
- **`count(*)` is free**: it reads the live-row count from the heap header, no scan.

---

## What's available vs. what's planned

Available in released PowDB (v0.25.0): joins (inner/left/right/cross, compound-predicate hash joins + bounded nested loops), GROUP BY + HAVING, symmetric PowQL aggregates with `raw` opt-out, expression-valued aggregate/group/order keys, DISTINCT, UNION / UNION ALL, subqueries (IN, EXISTS, correlated), nested projections (PowQL-only shaped results: one row per parent with correlated children as a native JSON array, with per-parent `filter` / `order` / `limit` / `offset`), entity links (PowQL-only relationship traversal: `link Post.user -> User on user_id = id`, then `p.user.name` for a to-one hop or a labeled block `posts: u.posts { title }` for a to-many, with `schema links` / `describe <Type>` introspection), CASE, LIKE, BETWEEN, IN-list, JSON paths and persistent path indexes, SQL `->` / `->>` JSON operators, window functions (ROW_NUMBER, RANK, DENSE_RANK, SUM/AVG/COUNT/MIN/MAX over partition), arithmetic, string/math/datetime scalars, CAST, COALESCE (`??`), materialized views with auto-refresh, upsert, multi-row INSERT, prepared queries with literal substitution, explicit transactions (`begin` / `commit` / `rollback`), concurrent autocommit reads, cooperative query cancellation, additive native typed wire results, password auth + multi-user auth (named users, admin/readwrite/readonly roles), TLS (`POWDB_TLS_CERT` / `POWDB_TLS_KEY`), WAL + crash recovery, persistent indexes, backup/restore (full/incremental/PITR, offline), SQL frontend (supported subset lowered to PowQL, see `docs/SQL.md`).

Planned (design doc only, don't use): `let` bindings, UDFs, per-row permissions, replication.

---

## For contributors

Build: `cargo build --workspace`. Test: `cargo test --workspace`. Lint: `cargo clippy --workspace --all-targets -- -D warnings`. Format: `cargo fmt --all`.

CI gates on `main` all live in `.github/workflows/ci.yml`: clippy/fmt/test on a 2-OS matrix, plus miri, ASan, cargo audit, cargo-deny, MSRV, version consistency, cross-version on-disk compatibility, fuzz-corpus replay, examples / TS-client / Node-addon / embedded-sync smokes, and a gitleaks secret scan. Every one of them is wired into a single `ci-success` aggregator job (`needs:` every job, fails if any fails), and that aggregator is the one required status check on `main`. A job that is not in its `needs:` list fails the `ci-success needs completeness` check, so new jobs cannot silently stop gating. `.github/workflows/bench.yml` (criterion + regression gate) is **manual-only** (`workflow_dispatch`) and NOT a merge gate; run the gate locally instead (see above).

Internal docs:
- `CLAUDE.md`: codebase guide for Claude Code (architecture, crate graph, common patterns)
- `CONTRIBUTING.md`: contribution workflow
- `SECURITY.md`: vulnerability reporting + threat model
- `docs/design/`: long-form language / engine design docs

When in doubt about what the parser accepts, **run it** against `cargo run --release -p powdb-cli`. This file is the 5-minute version; `docs/POWQL.md` is the reference; the parser is the truth.
