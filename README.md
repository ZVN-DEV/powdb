# PowDB

[![CI](https://github.com/ZVN-DEV/powdb/actions/workflows/ci.yml/badge.svg)](https://github.com/ZVN-DEV/powdb/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/powdb-cli.svg)](https://crates.io/crates/powdb-cli)
[![docs.rs](https://img.shields.io/docsrs/powdb-query)](https://docs.rs/powdb-query)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue)](https://github.com/ZVN-DEV/powdb/blob/main/Cargo.toml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://github.com/ZVN-DEV/powdb/blob/main/LICENSE)

**PowDB is a pure-Rust embedded database whose query language returns shaped results: one row per parent with its children nested inside, no join fan-out and no JSON text round-trip. Its compiled execution engine measures 3-7x SQLite on aggregates and 1-3.7x on filtered scans, and roughly 16x slower than SQLite on indexed point lookups.**

- **Performance** -- compiled byte-level predicates, zero-copy mmap scans, and a plan cache with literal substitution. Filter and aggregate paths skip full row decoding.
- **Platform** -- 100% pure-Rust core, no C dependencies, embeddable and server modes, installed with a single `cargo install` on Linux and macOS. **Windows is not supported** (the storage engine's mmap scan path is Unix-only); see [Platform support](#platform-support).
- **DX** -- PowQL is the front door: a left-to-right pipeline syntax that reads like an iterator chain.

Website: **[zvn-dev.github.io/powdb](https://zvn-dev.github.io/powdb/)**

Evaluating PowDB? Start with the honest comparison: [PowDB vs SQLite -- when to use which](https://github.com/ZVN-DEV/powdb/blob/main/docs/powdb-vs-sqlite.md). What an upgrade may and may not break: [docs/STABILITY.md](https://github.com/ZVN-DEV/powdb/blob/main/docs/STABILITY.md).

## What PowDB is for

PowDB is a single-writer embedded engine with truly parallel reads. Every writer takes the whole write-admission gate; there is no MVCC. That shape (a permanent, deliberate one) decides where the engine shines and where it does not.

**Reach for PowDB when:**

- **Single-writer embedded app state in Rust or Node.** One process owns the data, reads run in parallel, and you want the engine in-process (Rust crate or the `@zvndev/powdb-embedded` Node addon) with lossless typed results and injection-inert parameters.
- **Local agent or tool memory.** Local-first and single-process. Freshness comes from swapping in a new snapshot in seconds, not from tailing a live feed.
- **Read-only edge snapshot serving.** Restore a backup and serve the directory read-only from N processes, with no write gate at all; refresh by restoring a newer snapshot beside it and swapping.
- **Per-tenant, process-isolated databases.** One directory and one writer per tenant, rather than many tenants contending on one shared database.
- **CI and test databases.** Fast to create, cheap to throw away.
- **Bulk ingest plus read-heavy internal tools.** Batch-load in a transaction, then serve dashboards and internal queries off a read-mostly database.

**Use something else when:**

- **Many concurrent clients share one read-write database.** That is Postgres's home turf. PowDB serializes writers through one admission gate, so on a shared read-write database at concurrency, read latency amplifies through that gate. Use Postgres.
- **You need live replication or sync across nodes.** Use Turso.
- **Your workload is analytical column-crunching over one big dataset.** Use DuckDB.

For the concurrency numbers behind the boundary above (single-request cost versus behavior at concurrency 10), decomposed with a reproducible script, see [docs/benchmarks/concurrency-decomposition.md](https://github.com/ZVN-DEV/powdb/blob/main/docs/benchmarks/concurrency-decomposition.md).

## How it works

**Compiled predicate engine.** Filter expressions on integer columns are compiled into branch-free, byte-level operations that run directly against the encoded row bytes. The executor pattern-matches on `Filter(SeqScan)` plan shapes and dispatches to fast paths that never decode columns they don't need. On aggregate workloads this is where the 3-7x SQLite wins come from; on scan-shaped workloads the same machinery buys 1-3.7x. It does nothing for point lookups, where PowDB is roughly 16x slower than SQLite.

**Plan cache + tight planner-executor contract.** The planner is pure (no catalog access) and produces a canonical `PlanNode` tree; the cache hashes the canonical shape with FNV-1a and substitutes literals at lookup time, so repeat queries skip lex/parse/plan entirely. Range scans without a matching index are lowered to `Filter(SeqScan)` at execution time, keeping the planner stateless and the executor's fast paths fireable.

**Zero-copy mmap scans.** Heap files are memory-mapped and scanned via `try_for_each_row_raw`, a zero-syscall, zero-allocation iterator over raw row bytes. Combined with the compiled predicates, a full scan + filter + count never copies a row.

**PowQL -- the front door.** PowQL replaces SQL's inside-out clause structure with a left-to-right pipeline. You name the table, chain operations, and project fields, all in reading order.

| Task | SQL | PowQL |
|---|---|---|
| Filter + project | `SELECT name, age FROM User WHERE age > 25` | `User filter .age > 25 { .name, .age }` |
| Sort + limit | `SELECT * FROM User ORDER BY age DESC LIMIT 10` | `User order .age desc limit 10` |
| Aggregate with filter | `SELECT AVG(age) FROM User WHERE city = 'NYC'` | `avg(User filter .city = "NYC" { .age })` |
| Group + having | `SELECT status, COUNT(name) FROM User GROUP BY status HAVING COUNT(name) > 5` | `User group .status having count(.name) > 5 { .status, n: count(.name) }` |
| Follow a relationship | `SELECT o.total, u.name FROM Order o JOIN User u ON o.user_id = u.id` | `Order as o { o.total, o.user.name }` (after `link Order.user -> User on user_id = id`) |

PowQL uses `.field` dot syntax for column references, `:=` for assignments, and `"double quotes"` for strings. The pipeline reads like a sentence: *"User, filter age greater than 25, order by name, limit 10, give me name and age."*

Two capabilities have no SQL spelling at all, and are where PowQL earns its keep over SQL: **nested projections** (correlated children as a native JSON array, one row per parent) and **entity links** (declare a relationship once, then traverse it by name, with a scalar hop through a non-unique key refused as an error instead of silently multiplying rows).

**Already think in SQL?** Since v0.5.0 PowDB also accepts a supported subset of SQL through a frontend that lowers to the same PowQL plan tree (and shares the plan cache), see [docs/SQL.md](https://github.com/ZVN-DEV/powdb/blob/main/docs/SQL.md). PowQL remains the native, fastest path.

Full language reference: [docs/POWQL.md](https://github.com/ZVN-DEV/powdb/blob/main/docs/POWQL.md) | SQL frontend: [docs/SQL.md](https://github.com/ZVN-DEV/powdb/blob/main/docs/SQL.md) | Getting started: [docs/getting-started.md](https://github.com/ZVN-DEV/powdb/blob/main/docs/getting-started.md) | Backup &amp; restore: [docs/backup-and-restore.md](https://github.com/ZVN-DEV/powdb/blob/main/docs/backup-and-restore.md) | Driver/ORM implementers: [docs/integrations/powql-for-drivers.md](https://github.com/ZVN-DEV/powdb/blob/main/docs/integrations/powql-for-drivers.md) | Wire error codes: [docs/errors.md](https://github.com/ZVN-DEV/powdb/blob/main/docs/errors.md) | CLI reference (`--exec`, `--exec-file`, `--sql`, `--format`, REPL meta-commands): [crates/cli/README.md](https://github.com/ZVN-DEV/powdb/blob/main/crates/cli/README.md) | Stability and upgrade policy: [docs/STABILITY.md](https://github.com/ZVN-DEV/powdb/blob/main/docs/STABILITY.md)

## Install

```bash
# From crates.io (Rust 1.93+)
cargo install powdb-cli
cargo install powdb-server

# TypeScript client (Node 18+): version is kept in lockstep with the workspace by scripts/check-version-consistency.sh
npm install @zvndev/powdb-client

# In-process Node addon: embed the engine directly, no server (prebuilt for macOS arm64, Linux x64-gnu, Linux arm64-gnu; other targets build from source)
npm install @zvndev/powdb-embedded

# Prebuilt binaries (linux x86_64, macos aarch64)
# https://github.com/ZVN-DEV/powdb/releases/latest

# Docker
docker pull ghcr.io/zvn-dev/powdb:latest

# Or build from source
git clone https://github.com/ZVN-DEV/powdb
cd powdb
cargo build --release
```

Requires Rust 1.93+. This builds all crates: the storage engine, query engine, TCP server, CLI, and benchmarks.

**Building `powdb-server` or `powdb-cli` requires a C toolchain and `cmake`, and there is currently no way to opt out.** Both reach TLS through `tokio-rustls`, which pulls `aws-lc-sys`. Neither crate declares any Cargo features, so `--no-default-features` is a silent no-op: there is no `tls` feature to turn off. Making TLS optional is real work we have not done yet.

The whole-workspace `cargo build --release` above compiles more than the engine: it also builds `powdb-compare`, our SQLite/Postgres comparison harness, which bundles SQLite from C source via `rusqlite` and links `postgres`. Neither ships in any published crate. The engine libraries themselves (`powdb`, `powdb-storage`, `powdb-query`, `powdb-auth`, `powdb-backup`, `powdb-sync`) pull no C at all, so `cargo build --release -p powdb` needs no C toolchain.

### Platform support

| Platform | Status |
|---|---|
| Linux x86_64 | Supported (prebuilt binary + `cargo install`) |
| Linux aarch64 | Supported (`cargo install`, multi-arch Docker) |
| macOS aarch64 (Apple silicon) | Supported (prebuilt binary + `cargo install`) |
| macOS x86_64 (Intel) | Builds from source; no prebuilt binary |
| **Windows** | **Not supported. Does not compile.** |

Windows is not a "build it yourself" case, it is a hard compile failure. The
heap's memory-mapped scan path in `crates/storage/src/heap.rs` calls
`libc::mmap` / `libc::munmap` and `std::os::unix::io::AsRawFd` with no
platform gate, so `cargo check -p powdb-storage --target
x86_64-pc-windows-msvc` fails with 21 errors before anything else is
attempted. Porting it needs a Windows file-mapping backend for that path. The rest of the storage
layer is already portable (`crates/storage/src/disk.rs` handles both
platforms), so the gap is narrow, but it is real today.

### Apple silicon / arm64

As of **v0.10.0**, the published `ghcr.io/zvn-dev/powdb` image is multi-arch (`linux/amd64` + `linux/arm64`), so it runs natively on Apple silicon and ARM servers (e.g. Graviton). Alternatively, install the native binary directly, which builds in under a minute:

```bash
cargo install powdb-server
```

## Benchmark: PowDB vs SQLite (100K rows, Apple M5 Max laptop)

PowDB's compiled predicate engine is strongest on read-heavy aggregates, and gives a smaller and more variable win on filtered scans. All 15 workloads are listed, including the four where PowDB ties or loses. For durable write throughput, batch writes in a transaction, see [Write throughput & durability](#write-throughput--durability).

These are **single-request latencies**: one query at a time, measuring per-query cost, not throughput under many simultaneous clients. On a shared read-write database at concurrency, reads amplify through the write-admission gate (PowDB has no MVCC); that behavior is decomposed in [docs/benchmarks/concurrency-decomposition.md](https://github.com/ZVN-DEV/powdb/blob/main/docs/benchmarks/concurrency-decomposition.md). See also [What PowDB is for](#what-powdb-is-for).

| Workload | PowDB | SQLite | Result |
|---|---|---|---|
| Aggregate MIN | 221 us | 1.70 ms | **7.7x faster** |
| Aggregate MAX | 217 us | 1.47 ms | **6.8x faster** |
| Aggregate SUM | 234 us | 1.45 ms | **6.2x faster** |
| Update by primary key | 60 ns | 272 ns | **4.5x faster** |
| Aggregate AVG | 455 us | 1.70 ms | **3.7x faster** |
| Scan + filter + count | 380 us | 1.40 ms | **3.7x faster** |
| Non-indexed point lookup | 101 us | 319 us | **3.2x faster** |
| Scan + filter + sort + limit 10 | 2.46 ms | 6.41 ms | **2.6x faster** |
| Multi-column AND filter | 1.58 ms | 3.21 ms | **2.0x faster** |
| Update by filter (10K rows) | 2.36 ms | 4.54 ms | **1.9x faster** |
| Insert single row | 380 ns | 638 ns | roughly tied |
| Scan + filter + project top 100 | 8.1 us | 8.9 us | roughly tied |
| Delete by filter (10K rows) | 1.57 ms | 1.75 ms | roughly tied |
| Insert batch (1K rows) | 242 ns | 214 ns | roughly tied |
| **Indexed point lookup** | **3.17 us** | **202 ns** | **15.7x SLOWER** |

Reproduce with `cargo run --release -p powdb-compare`.

The compiled predicate engine avoids full row decoding during scans and aggregates. That is worth 3.7-7.7x on the four aggregates, but only 1.0-3.7x on the four scan-shaped workloads, so read the rows rather than a single headline multiplier. These wins come from compiled predicates and mmap scans, not from PowQL's syntax: the same query written in SQL lowers to the same plan and gets the same numbers.

**PowDB loses the indexed point lookup, badly, and by more than we used to publish.** Once the index is probed the remaining work is trivial, so nearly the whole 3.17 us is PowDB's own front end (lex, parse, canonicalize, plan-cache lookup) while SQLite amortizes that away with a prepared statement. The previous table put this at 7.9x against an older engine. Nine independent re-measurements of the current engine, across two machines, ranged from 10x to 20x, most of them above 15x. This row got worse and we had been understating it by roughly 2x. If your hot path is "fetch one row by id", SQLite is the better engine and scan throughput will not compensate.

Neither engine fsyncs (PowDB: `WalSyncMode::Off`, SQLite: `:memory:`), which isolates query-engine cost from durability cost and is not a durability comparison; for that see [Write throughput & durability](#write-throughput--durability). Median of 5 runs on an Apple M5 Max (macOS 26.5.1, rustc 1.97.0), commit `e3dfa71`, 2026-08-15. **These are laptop numbers, not CI numbers.** One caveat specific to the write rows: PowDB writes to a real temp directory while SQLite is `:memory:`, so `insert_single`, `insert_batch_1k`, and `delete_by_filter` are sensitive to whatever else is touching the disk. Measured under a heavy concurrent build on the same laptop, those rows moved by 30-100x while every other row moved by less than 2x, and two of them changed sign. The table above is from the quietest run we could get, but this machine was not fully idle, so treat the three write rows as the least reliable and re-measure them yourself before relying on them. Full methodology, per-run spread, and what changed in the harness: [docs/benchmarks/2026-07-24-wide-bench-snapshot.md](https://github.com/ZVN-DEV/powdb/blob/main/docs/benchmarks/2026-07-24-wide-bench-snapshot.md).

### Write throughput & durability

PowDB is durable by default. The embedded `Engine` and `powdb-server` both run in `WalSyncMode::Full`: every mutating statement appends to the write-ahead log and `fdatasync`s before the call returns, so an acknowledged write has reached stable storage. Reads pay zero fsync cost.

The one thing worth knowing: **a single-row `insert` in autocommit costs one fsync.** That caps single-row autocommit at your disk's fsync rate (a few hundred rows/sec on a typical SSD). That is not an engine limit, just the price of durability per statement. The fix is to **batch writes in a transaction**, which collapses the whole batch into a single fsync at `commit`:

```
# ~hundreds of rows/sec: one fsync per row
insert User { id := 1, name := "a" }
insert User { id := 2, name := "b" }
...

# ~50x faster, still fully durable: one fsync for the whole batch
begin
insert User { id := 1, name := "a" }
insert User { id := 2, name := "b" }
... thousands of rows ...
commit
```

On a 2026 laptop SSD this is the difference between ~290 rows/sec (autocommit) and ~15,600 rows/sec (one transaction), a 54x speedup, with identical crash-safety either way (the fsync just happens once, at `commit`, instead of per row). Always wrap bulk loads and write bursts in a transaction.

`WalSyncMode::Off` (used by the benchmark harness to compare against SQLite `:memory:`) disables the WAL entirely and is **not durable**: never use it in production.

## PowQL

PowQL reads left to right. You name the table, apply operations, and project fields -- all in one pipeline.

```
# Define a schema (auto-increment id, a default, a required field)
type User {
  unique auto id: int,
  required name: str,
  required email: str,
  status: str default "active",
  age: int,
  city: str
}

# Insert (single row)
insert User { name := "Alice", email := "alice@example.com", age := 30 }

# Insert many rows in one statement (one fsync, one round trip, all-or-nothing).
# Keep the whole statement on one line in the CLI REPL, which buffers input
# across lines only while braces/parens stay open.
insert User { name := "Bob", email := "bob@example.com", age := 22 }, { name := "Carol", email := "carol@example.com", age := 41 }

# returning: get the affected rows back in the same statement (here the auto id)
insert User { name := "Dave", email := "dave@example.com", age := 33 } returning

# Query pipeline: source -> filter -> order -> limit -> projection
User filter .age > 25 order .age desc limit 10 { .name, .age }

# Aggregates
count(User filter .age > 25)
sum(User { .age })
avg(User filter .city = "NYC" { .age })

# Joins
User as u inner join Team as t on u.team_id = t.id { u.name, team_name: t.name }

# GROUP BY + HAVING
User group .city having avg(.age) > 30 { .city, avg_age: avg(.age) }

# JSON paths can be filtered, grouped, aggregated, ordered, and indexed
Post group .data->category { .data->category, total: sum(.data->amount) }
alter Post add index (.data->published_at)

# Subqueries
User filter .id in (Order filter .total > 100 { .user_id })

# Set operations
User filter .age > 30 union User filter .city = "NYC"

# Mutations
User filter .age < 18 delete
User filter .id = 1 update { age := 31 }
User filter .id = 1 update { age := 32 } returning   # post-update rows back

# DDL
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

### Read-only snapshot serving

Serve a quiescent data directory (a restored backup or a checkpointed replica)
read-only, with no write gate at all:

```bash
powdb-server --readonly --data-dir ./serve/current --port 5433
```

Reads are served; every mutation returns a terminal read-only error. N read-only
processes can serve the same directory concurrently (a read-write open refuses
while live readers exist, and vice versa), and a read-only open never mutates the
directory. Embedded, use `Database.openReadOnly(dir)` (Node) or
`Database::open_read_only(dir)` (Rust). See
[Read-only snapshot serving](https://github.com/ZVN-DEV/powdb/blob/main/docs/read-only-serving.md) for the backup to restore
to serve flow, the swap-directory refresh pattern, and the requirement to refresh
materialized views before snapshotting.

### Environment variables

| Variable | Default | Description |
|---|---|---|
| `POWDB_PORT` | `5433` | TCP port for the server |
| `POWDB_BIND` | `127.0.0.1` | Interface to bind; set `0.0.0.0` behind an IPv4 platform proxy (Railway, Docker, ECS). On **Fly.io** use `[::]` instead, because its `.internal` network and `fly proxy` route over IPv6, so `0.0.0.0` makes the proxy reset the connection |
| `POWDB_DATA` | `./powdb_data` | Data directory (heap files, WAL, catalog, indexes) |
| `POWDB_PASSWORD` | *(none)* | Shared password required on connect when no named users are defined (set as env var) |
| `POWDB_ADMIN_USER` / `POWDB_ADMIN_PASSWORD` | *(none)* | Bootstrap an `admin` user on startup when both are set and that user does not yet exist (password never logged) |
| `POWDB_TLS_CERT` / `POWDB_TLS_KEY` | *(none)* | Paths to PEM cert + key; when both are set the server serves TLS |
| `POWDB_REQUIRE_TLS` | *(off)* | When set (`1`/`true`), refuse to start if a password is configured without TLS |
| `POWDB_IDLE_TIMEOUT` | `300` | Seconds before an idle connection is closed |
| `POWDB_QUERY_TIMEOUT` | `30` | Per-query deadline in seconds; cooperative cancellation stops supported scan, join, group, and mutation-discovery work and releases server admission promptly |
| `POWDB_QUERY_MEMORY_LIMIT` | `268435456` | Per-query memory budget in bytes (256 MiB); over-budget queries error instead of OOM-killing the server |
| `POWDB_TX_WAIT_TIMEOUT_MS` | `5000` | Max milliseconds a `begin` waits for a concurrent explicit transaction before failing with a timeout error instead of queueing indefinitely |
| `POWDB_TX_MAX_LIFETIME_MS` | `300000` | Max milliseconds one connection may hold an **open explicit transaction**. An explicit transaction holds the single write-admission gate for its whole lifetime, so the server bounds that lifetime: past it, the transaction is **rolled back** (uncommitted writes are lost), the gate is released, and the connection gets a class-3 `timeout` error and is closed. If the budget expires while a reply is being written and the write cannot finish, the connection is closed without the error frame, so treat an unexpected close during an open transaction as a timeout. The clock starts at `begin` and nothing the client sends extends it. This applies to legitimate long transactions too, so raise it (e.g. `3600000`) for servers running hour-long migrations, and split bulk loads that will not fit. `0` disables the bound and restores the pre-0.22 behavior in which one client can hold the gate, and therefore block every other connection including readers, for as long as it likes. Reaps are counted at `powdb_tx_reaped_total` |
| `POWDB_DB_NAME` | *(accept any)* | When set, the single database name this server serves; a CONNECT that explicitly names a different database is rejected |
| `POWDB_MAX_NESTED_LOOP_PAIRS` | `6400000` | Fallback nested-loop join candidate-pair cap; a pure non-equi join whose estimated pair count exceeds it fails before execution |
| `POWDB_DIRTY_PAGE_BUDGET` | `268435456` | Ceiling in bytes (256 MiB) on unflushed heap pages held in memory, shared across every table. Inside an explicit transaction those pages cannot be spilled without breaking `rollback`, so a transaction that exceeds the budget is refused with a typed error instead of growing until the process is OOM-killed. Raise it for very large bulk-load transactions, lower it on memory-capped hosts |
| `POWDB_SOCKET` | *(off)* | Path for an additional Unix-domain-socket listener served alongside the TCP listener |
| `POWDB_SYNC_MODE` | `full` | WAL durability: `full` (fsync before ack, fully durable) \| `normal` (bounded loss window on OS crash/power loss only, ~15-40x faster writes) \| `off` (no durability, bench-only) |
| `POWDB_METRICS_ADDR` | *(off)* | When set to `host:port` (e.g. `127.0.0.1:9090`), serve a Prometheus `/metrics` endpoint on a separate listener. **Unauthenticated**: bind it to localhost or a private network, never the public internet |
| `POWDB_READONLY` | *(off)* | When set (`1`/`true`), serve the data directory read-only (snapshot serving); mutations are refused. Same as `--readonly`. See [Read-only snapshot serving](https://github.com/ZVN-DEV/powdb/blob/main/docs/read-only-serving.md) |
| `RUST_LOG` | `info` | Log level (`debug`, `trace` for per-query timings) |

### Production checklist

Before exposing `powdb-server` beyond `127.0.0.1`:

- [ ] Configure authentication. Either set `POWDB_PASSWORD` to a strong shared secret, or define named users with roles (`powdb-cli --data-dir <dir> useradd …`; connect with `--user`). The server logs a `WARN` on startup when neither is configured and will accept any connection. See [Multi-user authentication](https://github.com/ZVN-DEV/powdb/blob/main/docs/getting-started.md#multi-user-authentication).
- [ ] Enable TLS via `POWDB_TLS_CERT` and `POWDB_TLS_KEY` (or run behind a TLS-terminating proxy). Set `POWDB_REQUIRE_TLS=1` to make the server refuse to start with a password but no TLS, so credentials can never transit in cleartext by misconfiguration.
- [ ] Bind to a specific interface with `--bind` rather than `0.0.0.0` if you can.
- [ ] If you enable the `POWDB_METRICS_ADDR` Prometheus endpoint, keep it on localhost or a private network, because it is unauthenticated and exposes operational counts (connection, query, and auth-failure totals).
- [ ] Mount `POWDB_DATA` on a persistent, durable volume. WAL replay assumes the directory is not wiped between restarts.
- [ ] **Run under a process supervisor with auto-restart.** PowDB is crash-only by design: the release profile sets `panic = "abort"`, so on an unrecoverable error the server exits immediately rather than limping along on possibly-corrupt shared state. WAL replay rolls the data directory forward to the last consistent state on the next start, but only if something restarts the process. Use systemd `Restart=always`, Docker `restart: unless-stopped`, a Kubernetes Deployment, Fly `auto_start_machines`, Railway `restartPolicyType = "ON_FAILURE"`, or an ECS service with `desired_count`. Every template in [`examples/deploy/`](https://github.com/ZVN-DEV/powdb/blob/main/examples/deploy/README.md) ships with auto-restart already wired in.
- [ ] Pin the version (`cargo install powdb-server --version 0.25.0 --locked` or the matching ghcr tag). Pin to a release that is still supported: [SECURITY.md](https://github.com/ZVN-DEV/powdb/blob/main/SECURITY.md) ships security fixes only for the latest minor series. PowDB is pre-1.0; minor bumps may add on-disk format versions. An older directory always opens on a newer release, but not the reverse. See [docs/STABILITY.md](https://github.com/ZVN-DEV/powdb/blob/main/docs/STABILITY.md).
- [ ] Wrap bulk loads and write bursts in a transaction (`begin` … `commit`): one fsync per batch instead of per row, ~50x write throughput with identical durability. See [Write throughput & durability](#write-throughput--durability). Run schema changes (`type`, `alter`, `drop`, `link`, `materialize`) **outside** the transaction: DDL is not transactional and is refused inside `begin`/`commit`. See [docs/POWQL.md](https://github.com/ZVN-DEV/powdb/blob/main/docs/POWQL.md#ddl-is-not-transactional).
- [ ] Size `POWDB_QUERY_MEMORY_LIMIT` for your host's RAM: it bounds a **single** query's materialization, not aggregate concurrent usage, so the 256 MiB default times many simultaneous connections can still exceed the process ceiling and get OOM-killed on memory-capped hosts (Railway/Fly/small AWS). Lower it accordingly.
- [ ] Size `POWDB_DIRTY_PAGE_BUDGET` the same way. It bounds the unflushed pages one explicit transaction holds in memory, so a bulk load bigger than the budget is refused (`cannot buffer more of this transaction`) rather than OOM-killing the server. Split the load into several transactions, or raise the budget if the host has the RAM. Like the query budget it is per-transaction, not aggregate.

For a self-hostable starting point, see [`examples/deploy/fly.toml`](https://github.com/ZVN-DEV/powdb/blob/main/examples/deploy/fly.toml).

## Features

**Storage engine**
- Slotted-page heap with 4KB pages
- B+tree indexes with crash-safe persistence (BIDX binary format), including scalar JSON-path indexes
- Write-ahead log with statement-boundary group commit
- Crash recovery (WAL replay + page-zero recovery + index rebuild)
- Memory-mapped reads (zero-syscall scan path)
- Compiled integer predicates (branch-free filter at the byte level)
- Thread-safe concurrent reads via pread(2)/pwrite(2), with shared server admission for autocommit reads
- Backup & restore: full + incremental + coarse point-in-time recovery (offline; `powdb-cli backup` / `restore`, see [docs/backup-and-restore.md](https://github.com/ZVN-DEV/powdb/blob/main/docs/backup-and-restore.md))

**Query engine**
- PowQL parser + planner + executor with plan cache (FNV-1a hashing, literal substitution)
- SQL frontend: a supported subset of SQL lowered to the PowQL AST, including `->` / `->>` JSON paths and shared plan caching ([docs/SQL.md](https://github.com/ZVN-DEV/powdb/blob/main/docs/SQL.md))
- Joins (hash join with compound-`ON` residuals, plus bounded nested-loop fallback)
- Nested projections (PowQL-only): one row per parent with correlated children as a native JSON array, per-parent order/limit, multi-level nesting ([docs/POWQL.md](https://github.com/ZVN-DEV/powdb/blob/main/docs/POWQL.md#nested-projections-shaped-results))
- Entity links (PowQL-only): declare a relationship once (`link Order.user -> User on user_id = id`) then traverse it by name: scalar `o.user.name`, or a labeled block `orders: u.orders { ... }` (the block needs a field label, which names the JSON array it returns). A scalar hop through a non-unique key is a hard error, never a silent fan-out ([docs/POWQL.md](https://github.com/ZVN-DEV/powdb/blob/main/docs/POWQL.md#entity-links-relationship-traversal))
- GROUP BY, HAVING, DISTINCT
- UNION / UNION ALL
- Subqueries (IN, EXISTS)
- Expressions in projections, filters, group keys, aggregate arguments, and order keys (arithmetic, JSON paths, string ops, BETWEEN, LIKE, IN-list)
- COUNT, SUM, AVG, MIN, MAX, COUNT DISTINCT, with symmetric PowQL join semantics and explicit `raw` opt-out
- ORDER BY (multi-expression), LIMIT, OFFSET
- Window functions (ROW_NUMBER, RANK, DENSE_RANK, SUM/AVG/MIN/MAX OVER)
- CAST, CASE/WHEN, COALESCE (`??`)
- Scalar functions: UPPER, LOWER, LENGTH, TRIM, SUBSTRING, CONCAT, ABS, ROUND, CEIL, FLOOR, SQRT, POW, NOW, EXTRACT, DATE_ADD, DATE_DIFF
- Materialized views with automatic dirty tracking
- UPSERT with ON CONFLICT
- `returning` on insert / update / delete (affected rows back in one round trip)
- `default` column values and `auto` (auto-increment) integer columns
- Prepared queries with literal substitution
- EXPLAIN for query plan inspection

**DDL**
- `type` (create table), `drop` (drop table)
- `alter <T> add column`, `alter <T> drop column` (with full heap rewrite)
- `alter <T> add index` / `add unique` for stored columns and scalar JSON paths; `drop index` for JSON-path (expression) indexes only

**Server**
- Tokio async TCP with `Arc<RwLock<Engine>>` for parallel readers
- Binary wire protocol (length-prefixed framing), with opt-in native typed rows/scalars for exact Bytes and PJ1 JSON
- Cooperative query deadlines and disconnect cancellation
- TLS support for encrypted connections
- Authentication: shared password (`POWDB_PASSWORD`) or named users with roles (argon2id-hashed)

**Pure Rust core**
- No `libsqlite3-sys`, no bindgen, no embedded C SQL engine in any published crate (the SQL frontend is pure-Rust and lowers to PowQL). The repo-root `cargo build` also compiles the `powdb-compare` benchmark harness, which *does* bundle SQLite from C; it is `publish = false` and ships nowhere
- Storage, query, auth, backup, sync, and the `powdb` embedded facade are 100% Rust with no C dependency
- `powdb-server` and `powdb-cli` pull `aws-lc-sys` (a C library needing `cmake`) through `tokio-rustls`. **There is no `tls` feature to disable** and no C-free build of those two today
- Single `cargo install` on Linux and macOS (see [Platform support](#platform-support); Windows does not compile)

## Architecture

```
crates/
  storage/   Heap files, B+tree, WAL, catalog, page cache, row encoding
  query/     Lexer, parser, planner, executor (Engine), plan cache
  powdb/     Embedded facade crate: the engine in-process, no server
  sync/      Retained replication-unit substrate (experimental, opt-in)
  auth/      User store, roles, argon2id password hashing
  backup/    Offline backup/restore (full, incremental, PITR)
  server/    Tokio TCP server + binary wire protocol
  cli/       Interactive REPL (embedded + remote modes)
  bench/     Criterion benchmarks + regression gate (publish = false)
  compare/   PowDB vs SQLite wide-bench harness (publish = false)
  oracle/    Differential correctness oracle: runs the same fixture and query
             through PowQL, the SQL frontend, and SQLite, and compares full
             result sets (publish = false)
```

The engine is `powdb_query::executor::Engine`. It owns a `Catalog` (which owns `Table`s, each backed by a `HeapFile` + optional `BTree` indexes) and a `Wal`. The server wraps it in `Arc<RwLock<Engine>>` for concurrent access.

## Benchmarks

PowDB has a benchmark regression gate that compares every workload against checked-in baselines. Run it locally before and after touching a hot path:

```bash
cargo bench -p powdb-bench              # criterion suite: 23 benchmarks, ~5 min of
                                        # measurement, plus compile on a cold target
cargo run --release -p powdb-bench --bin compare   # regression gate
```

The gate also runs on-demand in CI via `workflow_dispatch` (`.github/workflows/bench.yml`). It is **not** a required PR gate, because shared-runner noise makes it unreliable as a blocking check. The required PR gates live in `ci.yml`.

Run the PowDB vs SQLite comparison bench:

```bash
cargo run --release -p powdb-compare    # prints table + writes results.csv
```

## Tests

```bash
cargo test --workspace
```

## License

MIT License. See [LICENSE](https://github.com/ZVN-DEV/powdb/blob/main/LICENSE) for details.
