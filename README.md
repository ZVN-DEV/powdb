# PowDB

[![CI](https://github.com/zvndev/powdb/actions/workflows/ci.yml/badge.svg)](https://github.com/zvndev/powdb/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/powdb-cli.svg)](https://crates.io/crates/powdb-cli)
[![docs.rs](https://img.shields.io/docsrs/powdb-query)](https://docs.rs/powdb-query)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue)](https://github.com/zvndev/powdb/blob/main/Cargo.toml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**PowDB is a pure-Rust embedded database with a compiled query execution engine that delivers 3-10x SQLite on aggregate and scan workloads.**

- **Performance** -- compiled byte-level predicates, zero-copy mmap scans, and a plan cache with literal substitution. Filter and aggregate paths skip full row decoding.
- **Platform** -- 100% pure-Rust core, no C dependencies, embeddable and server modes, single `cargo install` on every platform Rust supports.
- **DX** -- PowQL is the front door: a left-to-right pipeline syntax that reads like an iterator chain.

Evaluating PowDB? Start with the honest comparison: [PowDB vs SQLite -- when to use which](docs/powdb-vs-sqlite.md).

## How it works

**Compiled predicate engine.** Filter expressions on integer columns are compiled into branch-free, byte-level operations that run directly against the encoded row bytes. The executor pattern-matches on `Filter(SeqScan)` plan shapes and dispatches to fast paths that never decode columns they don't need. On scan + filter + aggregate workloads, this is where the 3-10x SQLite wins come from.

**Plan cache + tight planner-executor contract.** The planner is pure (no catalog access) and produces a canonical `PlanNode` tree; the cache hashes the canonical shape with FNV-1a and substitutes literals at lookup time, so repeat queries skip lex/parse/plan entirely. Range scans without a matching index are lowered to `Filter(SeqScan)` at execution time, keeping the planner stateless and the executor's fast paths fireable.

**Zero-copy mmap scans.** Heap files are memory-mapped and scanned via `try_for_each_row_raw`, a zero-syscall, zero-allocation iterator over raw row bytes. Combined with the compiled predicates, a full scan + filter + count never copies a row.

**PowQL -- the front door.** PowQL replaces SQL's inside-out clause structure with a left-to-right pipeline. You name the table, chain operations, and project fields, all in reading order.

| Task | SQL | PowQL |
|---|---|---|
| Filter + project | `SELECT name, age FROM User WHERE age > 25` | `User filter .age > 25 { .name, .age }` |
| Sort + limit | `SELECT * FROM User ORDER BY age DESC LIMIT 10` | `User order .age desc limit 10` |
| Aggregate with filter | `SELECT AVG(age) FROM User WHERE city = 'NYC'` | `avg(User filter .city = "NYC" { .age })` |
| Group + having | `SELECT status, COUNT(name) FROM User GROUP BY status HAVING COUNT(name) > 5` | `User group .status having count(.name) > 5 { .status, n: count(.name) }` |

PowQL uses `.field` dot syntax for column references, `:=` for assignments, and `"double quotes"` for strings. The pipeline reads like a sentence: *"User, filter age greater than 25, order by name, limit 10, give me name and age."*

Full language reference: [docs/POWQL.md](https://github.com/zvndev/powdb/blob/main/docs/POWQL.md) | Getting started: [docs/getting-started.md](https://github.com/zvndev/powdb/blob/main/docs/getting-started.md) | Backup &amp; restore: [docs/backup-and-restore.md](https://github.com/zvndev/powdb/blob/main/docs/backup-and-restore.md)

## Install

```bash
# From crates.io (Rust 1.93+)
cargo install powdb-cli
cargo install powdb-server

# TypeScript client (Node 18+) — version is kept in lockstep with the workspace by scripts/check-version-consistency.sh
npm install @zvndev/powdb-client

# Prebuilt binaries (linux x86_64, macos aarch64)
# https://github.com/zvndev/powdb/releases/latest

# Docker
docker pull ghcr.io/zvn-dev/powdb:latest

# Or build from source
git clone https://github.com/zvndev/powdb
cd powdb
cargo build --release
```

Requires Rust 1.93+. This builds all crates: the storage engine, query engine, TCP server, CLI, and benchmarks. TLS support in `powdb-server` pulls `aws-lc-sys`, which requires a C toolchain (`cmake`); disable the default `tls` feature for a fully-Rust build.

## Benchmark: PowDB vs SQLite (100K rows, M1)

PowDB's compiled predicate engine excels at read-heavy aggregate and scan workloads. For durable write throughput, batch writes in a transaction — see [Write throughput & durability](#write-throughput--durability).

| Workload | PowDB | SQLite | Result |
|---|---|---|---|
| Aggregate MIN | 236 us | 2,340 us | **9.9x faster** |
| Aggregate MAX | 236 us | 2,100 us | **8.9x faster** |
| Aggregate SUM | 231 us | 1,870 us | **8.1x faster** |
| Update by primary key | 55 ns | 412 ns | **7.5x faster** |
| Aggregate AVG | 401 us | 2,300 us | **5.7x faster** |
| Scan + filter + count | 381 us | 1,950 us | **5.1x faster** |
| Scan + filter + sort + limit 10 | 2.66 ms | 9.77 ms | **3.7x faster** |
| Update by filter (10K rows) | 2.16 ms | 6.77 ms | **3.1x faster** |
| Indexed point lookup | 93 ns | 282 ns | **3.0x faster** |
| Multi-column AND filter | 2.22 ms | 4.70 ms | **2.1x faster** |
| Insert batch (1K rows) | 238 ns | 320 ns | **1.3x faster** |
| Delete by filter (10K rows) | 1.76 ms | 2.35 ms | **1.3x faster** |
| Scan + filter + project top 100 | 9.6 us | 12.7 us | **1.3x faster** |
| Non-indexed point lookup | 350 us | 432 us | **1.2x faster** |

PowDB is fastest where it matters most: the compiled predicate engine avoids full row decoding during scans and aggregates, delivering 3-10x gains on analytical queries. Point lookups benefit from a minimal parse-plan-execute pipeline. Write performance is competitive with SQLite across the board.

Both engines use in-memory mode (PowDB: `WalSyncMode::Off`, SQLite: `:memory:`). Full results in `crates/compare/`. These are *in-memory* numbers; for the durable (fsync) write story, see [Write throughput & durability](#write-throughput--durability) below.

### Write throughput & durability

PowDB is durable by default. The embedded `Engine` and `powdb-server` both run in `WalSyncMode::Full`: every mutating statement appends to the write-ahead log and `fdatasync`s before the call returns, so an acknowledged write has reached stable storage. Reads pay zero fsync cost.

The one thing worth knowing: **a single-row `insert` in autocommit costs one fsync.** That caps single-row autocommit at your disk's fsync rate (a few hundred rows/sec on a typical SSD) — not an engine limit, just the price of durability per statement. The fix is to **batch writes in a transaction**, which collapses the whole batch into a single fsync at `commit`:

```
-- ~hundreds of rows/sec: one fsync per row
insert User { id := 1, name := "a" }
insert User { id := 2, name := "b" }
...

-- ~50x faster, still fully durable: one fsync for the whole batch
begin
insert User { id := 1, name := "a" }
insert User { id := 2, name := "b" }
... thousands of rows ...
commit
```

On a 2026 laptop SSD this is the difference between ~290 rows/sec (autocommit) and ~15,600 rows/sec (one transaction) — a 54x speedup, with identical crash-safety either way (the fsync just happens once, at `commit`, instead of per row). Always wrap bulk loads and write bursts in a transaction.

`WalSyncMode::Off` (used by the benchmark harness to compare against SQLite `:memory:`) disables the WAL entirely and is **not durable** — never use it in production.

## PowQL

PowQL reads left to right. You name the table, apply operations, and project fields -- all in one pipeline.

```
-- Define a schema
type User {
  required name: str,
  required email: str,
  age: int
}

-- Insert (single row)
insert User { name := "Alice", email := "alice@example.com", age := 30 }

-- Insert many rows in one statement (one fsync, one round trip, all-or-nothing)
insert User
  { name := "Bob",   email := "bob@example.com",   age := 22 },
  { name := "Carol", email := "carol@example.com", age := 41 }

-- Query pipeline: source -> filter -> order -> limit -> projection
User filter .age > 25 order .age desc limit 10 { .name, .age }

-- Aggregates
count(User filter .age > 25)
sum(User { .age })
avg(User filter .city = "NYC" { .age })

-- Joins
User as u inner join Team as t on u.team_id = t.id { u.name, team_name: t.name }

-- GROUP BY + HAVING
User group .city having avg(.age) > 30 { .city, avg_age: avg(.age) }

-- Subqueries
User filter .id in (Order filter .total > 100 { .user_id })

-- Set operations
(User filter .age > 30) union (User filter .city = "NYC")

-- Mutations
User filter .age < 18 delete
User filter .id = 1 update { age := 31 }

-- DDL
alter User add column score: int
alter User drop column score
alter User add index .email
drop User
```

## Run

### Embedded (CLI / REPL)

```bash
powdb-cli
# or from source:
cargo run --release -p powdb-cli
```

Opens an interactive REPL with tab completion, command history, and meta-commands (`.tables`, `.schema`, `.timing`, `.help`). Data is stored in `./powdb_data/` by default.

### Server mode

```bash
powdb-server --port 5433 --data-dir ./powdb_data
# or from source:
cargo run --release -p powdb-server -- --port 5433 --data-dir ./powdb_data
```

Listens on TCP with a binary wire protocol. Connect via the CLI:

```bash
powdb-cli --remote localhost:5433
```

Or the TypeScript client:

```typescript
import { Client } from "@zvndev/powdb-client";

const client = await Client.connect({ host: "localhost", port: 5433 });
const result = await client.query("User filter .age > 25 { .name, .age }");
if (result.kind === "rows") console.table(result.rows);
```

### Environment variables

| Variable | Default | Description |
|---|---|---|
| `POWDB_PORT` | `5433` | TCP port for the server |
| `POWDB_BIND` | `127.0.0.1` | Interface to bind; set `0.0.0.0` behind an IPv4 platform proxy (Railway, Docker, ECS). On **Fly.io** use `[::]` instead — its `.internal` network and `fly proxy` route over IPv6, so `0.0.0.0` makes the proxy reset the connection |
| `POWDB_DATA` | `./powdb_data` | Data directory (heap files, WAL, catalog, indexes) |
| `POWDB_PASSWORD` | *(none)* | Shared password required on connect when no named users are defined (set as env var) |
| `POWDB_ADMIN_USER` / `POWDB_ADMIN_PASSWORD` | *(none)* | Bootstrap an `admin` user on startup when both are set and that user does not yet exist (password never logged) |
| `POWDB_TLS_CERT` / `POWDB_TLS_KEY` | *(none)* | Paths to PEM cert + key; when both are set the server serves TLS |
| `POWDB_REQUIRE_TLS` | *(off)* | When set (`1`/`true`), refuse to start if a password is configured without TLS |
| `POWDB_IDLE_TIMEOUT` | `300` | Seconds before an idle connection is closed |
| `POWDB_QUERY_TIMEOUT` | `30` | Per-query timeout in seconds |
| `POWDB_QUERY_MEMORY_LIMIT` | `268435456` | Per-query memory budget in bytes (256 MiB); over-budget queries error instead of OOM-killing the server |
| `RUST_LOG` | `info` | Log level (`debug`, `trace` for per-query timings) |

### Production checklist

Before exposing `powdb-server` beyond `127.0.0.1`:

- [ ] Configure authentication. Either set `POWDB_PASSWORD` to a strong shared secret, or define named users with roles (`powdb-cli --data-dir <dir> useradd …`; connect with `--user`). The server logs a `WARN` on startup when neither is configured and will accept any connection. See [Multi-user authentication](docs/getting-started.md#multi-user-authentication).
- [ ] Enable TLS via `POWDB_TLS_CERT` and `POWDB_TLS_KEY` (or run behind a TLS-terminating proxy). Set `POWDB_REQUIRE_TLS=1` to make the server refuse to start with a password but no TLS, so credentials can never transit in cleartext by misconfiguration.
- [ ] Bind to a specific interface with `--bind` rather than `0.0.0.0` if you can.
- [ ] Mount `POWDB_DATA` on a persistent, durable volume. WAL replay assumes the directory is not wiped between restarts.
- [ ] Pin the version (`cargo install powdb-server --version 0.4.8 --locked` or the matching ghcr tag). PowDB is pre-1.0; minor bumps may change on-disk formats.
- [ ] Wrap bulk loads and write bursts in a transaction (`begin` … `commit`) — one fsync per batch instead of per row, ~50x write throughput with identical durability. See [Write throughput & durability](#write-throughput--durability).
- [ ] Size `POWDB_QUERY_MEMORY_LIMIT` for your host's RAM: it bounds a **single** query's materialization, not aggregate concurrent usage, so the 256 MiB default times many simultaneous connections can still exceed the process ceiling and get OOM-killed on memory-capped hosts (Railway/Fly/small AWS). Lower it accordingly.

For a self-hostable starting point, see [`examples/deploy/fly.toml`](https://github.com/zvndev/powdb/blob/main/examples/deploy/fly.toml).

## Features

**Storage engine**
- Slotted-page heap with 4KB pages
- B+tree indexes with crash-safe persistence (BIDX binary format)
- Write-ahead log with statement-boundary group commit
- Crash recovery (WAL replay + page-zero recovery + index rebuild)
- Memory-mapped reads (zero-syscall scan path)
- Compiled integer predicates (branch-free filter at the byte level)
- Thread-safe concurrent reads via pread(2)/pwrite(2)
- Backup & restore: full + incremental + coarse point-in-time recovery (offline; `powdb-cli backup` / `restore` — see [docs/backup-and-restore.md](docs/backup-and-restore.md))

**Query engine**
- PowQL parser + planner + executor with plan cache (FNV-1a hashing, literal substitution)
- Joins (nested-loop + hash join for equi-joins)
- GROUP BY, HAVING, DISTINCT
- UNION / UNION ALL
- Subqueries (IN, EXISTS)
- Expressions in projections and filters (arithmetic, string ops, BETWEEN, LIKE, IN-list)
- COUNT, SUM, AVG, MIN, MAX, COUNT DISTINCT
- ORDER BY (multi-column), LIMIT, OFFSET
- Window functions (ROW_NUMBER, RANK, DENSE_RANK, SUM/AVG/MIN/MAX OVER)
- CAST, CASE/WHEN, COALESCE (`??`)
- Scalar functions: UPPER, LOWER, LENGTH, TRIM, SUBSTRING, CONCAT, ABS, ROUND, CEIL, FLOOR, SQRT, POW, NOW, EXTRACT, DATE_ADD, DATE_DIFF
- Materialized views with automatic dirty tracking
- UPSERT with ON CONFLICT
- Prepared queries with literal substitution
- EXPLAIN for query plan inspection

**DDL**
- `type` (create table), `drop` (drop table)
- `alter <T> add column`, `alter <T> drop column` (with full heap rewrite)
- `alter <T> add index` (B+tree, persisted)

**Server**
- Tokio async TCP with `Arc<RwLock<Engine>>` for parallel readers
- Binary wire protocol (length-prefixed framing)
- TLS support for encrypted connections
- Authentication: shared password (`POWDB_PASSWORD`) or named users with roles (argon2id-hashed)

**Pure Rust core**
- No SQL parsing layer, no `libsqlite3-sys`, no bindgen
- Storage, query, and CLI are 100% Rust
- TLS (`powdb-server` only) pulls `aws-lc-sys`; disable the `tls` feature for a C-free build
- Single `cargo install` on any platform Rust supports

## Architecture

```
crates/
  storage/   Heap files, B+tree, WAL, catalog, page cache, row encoding
  query/     Lexer, parser, planner, executor (Engine), plan cache
  auth/      User store, roles, argon2id password hashing
  backup/    Offline backup/restore (full, incremental, PITR)
  server/    Tokio TCP server + binary wire protocol
  cli/       Interactive REPL (embedded + remote modes)
  bench/     Criterion benchmarks + regression gate
  compare/   PowDB vs SQLite wide-bench harness
```

The engine is `powdb_query::executor::Engine`. It owns a `Catalog` (which owns `Table`s, each backed by a `HeapFile` + optional `BTree` indexes) and a `Wal`. The server wraps it in `Arc<RwLock<Engine>>` for concurrent access.

## Benchmarks

PowDB has a benchmark regression gate that compares every workload against checked-in baselines. Run it locally before and after touching a hot path:

```bash
cargo bench -p powdb-bench              # criterion suite (~60s)
cargo run --release -p powdb-bench --bin compare   # regression gate
```

The gate also runs on-demand in CI via `workflow_dispatch` (`.github/workflows/bench.yml`) — it is **not** a required PR gate, because shared-runner noise makes it unreliable as a blocking check. The required PR gates live in `ci.yml`.

Run the PowDB vs SQLite comparison bench:

```bash
cargo run --release -p powdb-compare    # prints table + writes results.csv
```

## Tests

```bash
cargo test --workspace
```

## License

MIT License. See [LICENSE](LICENSE) for details.
