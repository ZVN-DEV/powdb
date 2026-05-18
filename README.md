# PowDB

A Rust-native embedded database with compiled query execution and PowQL -- a query language designed for how developers actually think.

PowDB compiles filter expressions into byte-level operations that skip full row decoding, producing 3-10x speedups over SQLite on aggregate and scan workloads. Its pipeline query language (PowQL) reads left to right -- no `SELECT ... FROM ... WHERE` juggling -- and the engine is pure Rust end-to-end, no C FFI required.

## Why PowQL?

PowQL replaces SQL's inside-out clause structure with a left-to-right pipeline. You name the table, chain operations, and project fields -- all in reading order.

| Task | SQL | PowQL |
|---|---|---|
| Filter + project | `SELECT name, age FROM User WHERE age > 25` | `User filter .age > 25 { .name, .age }` |
| Sort + limit | `SELECT * FROM User ORDER BY age DESC LIMIT 10` | `User order .age desc limit 10` |
| Aggregate with filter | `SELECT AVG(age) FROM User WHERE city = 'NYC'` | `avg(User filter .city = "NYC" { .age })` |
| Group + having | `SELECT status, COUNT(*) FROM User GROUP BY status HAVING COUNT(*) > 5` | `User group .status having count(*) > 5 { .status, count(*) }` |

PowQL uses `.field` dot syntax for column references, `:=` for assignments, and `"double quotes"` for strings. The pipeline reads like a sentence: *"User, filter age greater than 25, order by name, limit 10, give me name and age."*

Full language reference: [docs/POWQL.md](docs/POWQL.md) | Getting started: [docs/getting-started.md](docs/getting-started.md)

## Install

```bash
# Install from crates.io
cargo install powdb-cli
cargo install powdb-server

# Or build from source
git clone https://github.com/zvndev/powdb
cd powdb
cargo build --release
```

Requires Rust stable (1.80+). This builds all crates: the storage engine, query engine, TCP server, CLI, and benchmarks.

## Benchmark: PowDB vs SQLite (100K rows, M1)

PowDB's compiled predicate engine excels at read-heavy aggregate and scan workloads. Write performance is an active area of improvement.

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

Both engines use in-memory mode (PowDB: `WalSyncMode::Off`, SQLite: `:memory:`). Full results in `crates/compare/`.

## PowQL

PowQL reads left to right. You name the table, apply operations, and project fields -- all in one pipeline.

```
-- Define a schema
type User {
  required name: str,
  required email: str,
  age: int
}

-- Insert
insert User { name := "Alice", email := "alice@example.com", age := 30 }

-- Query pipeline: source -> filter -> order -> limit -> projection
User filter .age > 25 order .age desc limit 10 { .name, .age }

-- Aggregates
count(User filter .age > 25)
sum(User { .age })
avg(User filter .city = "NYC" { .age })

-- Joins
User as u inner join Team as t on u.team_id = t.id { u.name, team_name: t.name }

-- GROUP BY + HAVING
User group .city { .city, avg_age: avg(.age) } having avg_age > 30

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
| `POWDB_DATA` | `./powdb_data` | Data directory (heap files, WAL, catalog, indexes) |
| `POWDB_PASSWORD` | *(none)* | Require this password on connect (set as env var) |
| `RUST_LOG` | `info` | Log level (`debug`, `trace` for per-query timings) |

## Features

**Storage engine**
- Slotted-page heap with 4KB pages
- B+tree indexes with crash-safe persistence (BIDX binary format)
- Write-ahead log with statement-boundary group commit
- Crash recovery (WAL replay + page-zero recovery + index rebuild)
- Memory-mapped reads (zero-syscall scan path)
- Compiled integer predicates (branch-free filter at the byte level)
- Thread-safe concurrent reads via pread(2)/pwrite(2)

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
- Password authentication via `POWDB_PASSWORD` env var

**Pure Rust**
- Zero C FFI -- no C compiler, no `libsqlite3-sys`, no bindgen
- Single `cargo install` on any platform Rust supports
- Small dependency tree compared to full-featured alternatives

## Architecture

```
crates/
  storage/   Heap files, B+tree, WAL, catalog, page cache, row encoding
  query/     Lexer, parser, planner, executor (Engine), plan cache
  server/    Tokio TCP server + binary wire protocol
  cli/       Interactive REPL (embedded + remote modes)
  bench/     Criterion benchmarks + regression gate
  compare/   PowDB vs SQLite wide-bench harness
```

The engine is `powdb_query::executor::Engine`. It owns a `Catalog` (which owns `Table`s, each backed by a `HeapFile` + optional `BTree` indexes) and a `Wal`. The server wraps it in `Arc<RwLock<Engine>>` for concurrent access.

## Benchmarks

PowDB has a CI-enforced regression gate that blocks PRs to `main` if any workload regresses beyond its threshold. Run locally:

```bash
cargo bench -p powdb-bench              # criterion suite (~60s)
cargo run --release -p powdb-bench --bin compare   # regression gate
```

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
