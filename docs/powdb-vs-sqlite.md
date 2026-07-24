# PowDB vs SQLite — When to use which

SQLite is the default embedded database for very good reasons: 25+ years of
battle-testing, billions of deployments, and a SQL surface every tool on the
planet understands. PowDB is a newer, pure-Rust embedded database built around
a compiled query execution engine that delivers 3-7x speedups on aggregate
and scan workloads, and loses to SQLite on indexed point lookups. This guide is written so an evaluator can decide honestly
between the two.

## When to choose PowDB

- **Your stack is pure Rust and you want to keep it that way.** PowDB's
  storage and query engines are 100% Rust with no `libsqlite3-sys`, no
  `bindgen`, no C toolchain in the build path (TLS in `powdb-server` is the
  one optional exception via `aws-lc-sys`, and the `tls` feature can be
  disabled for a C-free build). `cargo install powdb-cli` works on Linux and
  macOS. **Windows is not supported** and does not compile: the heap's mmap
  scan path is Unix-only. SQLite has no such gap, and if you ship on Windows
  that alone decides this evaluation.
- **Your workload is read-heavy or aggregate-heavy.** The compiled predicate
  engine compiles filter expressions into byte-level operations that skip
  full row decoding. On the benchmarks below, that translates to 3-7x wins
  on `MIN`, `MAX`, `SUM`, `AVG`, `scan + filter + count`, and similar
  scan-shaped queries. It does not help point lookups, where SQLite wins.
- **You are already on tokio.** `powdb-server` is async-native and wraps the
  engine in `Arc<RwLock<Engine>>` so parallel readers don't block each other.
  Note the boundary: there is no MVCC, so a writer (and especially a
  long-lived explicit transaction) takes the whole admission gate and blocks
  readers for its lifetime. PowDB fits a single writer with many reads, not
  many concurrent clients sharing one read-write database; for that, use
  Postgres.
- **You want to embed without a C toolchain.** Useful on Wasm-adjacent
  targets, on minimal container images, and in environments where pulling
  `cmake` into the build is friction.
- **You like the pipeline syntax.** PowQL reads left to right -- source,
  then operations, then projection. If that matches how you think about
  data, it cuts cognitive load. (Linked: [POWQL.md](POWQL.md).)

## When to choose SQLite

- **SQL tool / driver compatibility matters.** PowDB accepts a supported
  subset of SQL through a frontend (since v0.5.0 — see [SQL.md](SQL.md)), but
  it speaks its own binary wire protocol, not the Postgres/MySQL wire or a
  JDBC/ODBC surface. Every ORM, DB browser, analytics tool, notebook, and BI
  dashboard talks to SQLite out of the box; none of them can point at PowDB.
  If you need Metabase, DBeaver, or a JDBC driver on your data, SQLite is the
  answer.
- **Battle-testing matters more than peak performance.** SQLite has 25+
  years of production deployment, decades of OSS-Fuzz coverage, and a test
  suite that is famously larger than the codebase itself. PowDB ships
  property tests + 4 fuzz targets (`crates/query/fuzz/`), but is pre-1.0
  and the on-disk format may shift.
- **You need broad tool / language ecosystem support.** Bindings exist for
  essentially every language. PowDB has a TypeScript client, an in-process
  Node addon (`@zvndev/powdb-embedded`), and a Rust API today.
- **You're already shipping the C toolchain.** If your build already
  compiles `aws-lc`, `openssl`, or any other C dep, the
  `libsqlite3-sys` cost is zero.
- **You want full MVCC, online backups, or any of the decade-of-features
  SQLite has.** PowDB 0.4.5 shipped role-based users (admin / readwrite /
  readonly) and offline full/incremental backup with coarse point-in-time
  recovery, but there is still no MVCC and no *online* backup -- backups
  require stopping the server.

## Side-by-side feature table

| Capability                | PowDB                                                | SQLite                                                |
|---------------------------|------------------------------------------------------|-------------------------------------------------------|
| Implementation language   | 100% Rust core                                       | C                                                     |
| Platform support          | Linux + macOS (Windows does not compile)             | Everywhere, including Windows                         |
| Build dependencies        | Cargo only (TLS optional `aws-lc-sys`)               | C toolchain                                           |
| Query language            | PowQL (pipeline) + supported SQL subset via frontend | SQL (industry standard)                               |
| Storage model             | Slotted-page heap + B+tree indexes                   | B-tree of B-trees                                     |
| Memory-mapped reads       | Yes (zero-syscall scan path)                         | Optional (`PRAGMA mmap_size`)                         |
| Write-ahead log           | Yes (statement-boundary group commit)                | Yes (WAL mode)                                        |
| Compiled predicates       | Yes (byte-level filters, plan cache w/ literal sub)  | Bytecode VM (VDBE)                                    |
| MVCC                      | No (single-writer, parallel readers via RwLock)      | No (single-writer, WAL-mode readers don't block)      |
| Joins                     | Nested-loop + hash (equi-join)                       | Nested-loop + merge + hash                            |
| Window functions          | ROW_NUMBER, RANK, DENSE_RANK, SUM/AVG/MIN/MAX OVER   | Full set                                              |
| Server mode               | Yes (binary wire protocol, TLS, auth)                | Not in core (extensions exist)                        |
| Fuzz testing              | 4 cargo-fuzz targets (lexer, parser, roundtrip, SQL) | OSS-Fuzz, decades of corpora                          |
| Crash recovery            | WAL replay + page-zero recovery + index rebuild      | WAL/rollback journal                                  |
| Backup                    | Offline full/incremental + coarse PITR (0.4.5)       | Online backup API, `.backup`, VACUUM INTO             |
| On-disk format stability  | Pre-1.0, may shift                                   | Stable for decades                                    |
| Production deployments    | Pre-1.0                                              | Billions                                              |

The MVCC, fuzz testing, mmap, and WAL claims for SQLite are drawn from the
SQLite docs (sqlite.org/wal.html, sqlite.org/testing.html). PowDB's are
visible in `crates/storage/src/wal.rs`, `crates/storage/src/heap.rs`, and
`crates/query/fuzz/`.

## Benchmarks

Numbers below are from `cargo run --release -p powdb-compare` against the
same dataset (100K rows), with PowDB in `WalSyncMode::Off` and SQLite in
`:memory:` -- both engines running entirely in RAM. This is the methodology
disclosed in the project README. It favors both engines' in-memory paths
equally; on-disk durable comparisons will move the numbers and are tracked
separately.

Median of 5 runs on an Apple M5 Max laptop (macOS 26.5.1, rustc 1.97.0),
commit `a090568`. **Laptop numbers, not CI numbers.** Full methodology and
per-run spread: [2026-07-24 snapshot](benchmarks/2026-07-24-wide-bench-snapshot.md).

| Workload                            | PowDB    | SQLite   | Result                 |
|-------------------------------------|----------|----------|------------------------|
| Update by primary key               | 66 ns    | 500 ns   | PowDB 7.6x faster      |
| Aggregate MIN                       | 266 us   | 1.77 ms  | PowDB 6.7x faster      |
| Aggregate MAX                       | 271 us   | 1.54 ms  | PowDB 5.7x faster      |
| Aggregate SUM                       | 281 us   | 1.57 ms  | PowDB 5.6x faster      |
| Aggregate AVG                       | 516 us   | 1.82 ms  | PowDB 3.5x faster      |
| Scan + filter + count               | 481 us   | 1.47 ms  | PowDB 3.1x faster      |
| Non-indexed point lookup            | 117 us   | 321 us   | PowDB 2.7x faster      |
| Scan + filter + sort + limit 10     | 2.68 ms  | 6.73 ms  | PowDB 2.5x faster      |
| Insert single row                   | 394 ns   | 790 ns   | PowDB 2.0x faster      |
| Multi-column AND filter             | 1.75 ms  | 3.46 ms  | PowDB 2.0x faster      |
| Update by filter (10K rows)         | 2.66 ms  | 5.08 ms  | PowDB 1.9x faster      |
| Delete by filter (10K rows)         | 1.65 ms  | 1.95 ms  | roughly tied           |
| Scan + filter + project top 100     | 8.0 us   | 8.9 us   | roughly tied           |
| Insert batch (1K rows)              | 232 ns   | 257 ns   | roughly tied           |
| **Indexed point lookup**            | **1.65 us** | **208 ns** | **SQLite 7.9x faster** |

The headline wins are exactly where the compiled-predicate engine is
designed to win: aggregates and filtered scans where avoiding full row
decoding pays off. On `insert_batch_1k` and `delete_by_filter` and the
smallest projection workloads the two are effectively tied; an honest
comparison should not pretend otherwise.

**Read the point-lookup row before you decide.** PowDB is 7.9x *slower* than
SQLite at fetching one row by indexed id. Almost all of that 1.65 us is
PowDB's own front end (lex, parse, canonicalize, plan-cache lookup); the
B-tree probe underneath is tens of nanoseconds. SQLite pays roughly 208 ns
end to end because a prepared statement amortizes its parser away. If your
application's hot path is single-row fetches, that is the number that should
decide this evaluation, and it points at SQLite.

Two of these workloads used to be measured through a code path a user could
not reach (the aggregates hand-built a plan node; the indexed point lookup
called the B-tree directly), which is why previously published figures were
40-60% higher and why the point lookup was previously, wrongly, reported as a
3.0x win. That is fixed; see the snapshot doc for the before-and-after.

### A note on durable writes

The table above runs both engines in RAM (`WalSyncMode::Off`,
`:memory:`), which isolates query-engine cost from disk cost. In
production, PowDB defaults to `WalSyncMode::Full`: every autocommit
statement fsyncs the write-ahead log, so a single-row insert is
fsync-bound -- on a real SSD that's roughly a few hundred autocommit
inserts per second, comparable to SQLite in its default durable mode.
The fix is the same on both engines: batch writes in a transaction.
Wrapping inserts in `begin` / `commit` shares one fsync across the whole
batch and runs ~50x faster on PowDB while staying fully durable. Bulk
loads should always use a transaction -- see
[Transactions](POWQL.md#transactions).

Run it yourself:

```bash
cargo run --release -p powdb-compare
```

Results land in `crates/compare/results.csv`.

## Caveats and roadmap

- **PowDB is pre-1.0.** New on-disk format versions appear across minor
  versions, though every release still reads everything earlier releases
  wrote. Pin a version (`cargo install powdb-cli --version 0.19.1 --locked`)
  and expect to re-bench on upgrades until 1.0. What a minor version may and
  may not break is spelled out in [STABILITY.md](STABILITY.md); the
  version/magic mechanics are in [FORMAT.md](FORMAT.md).
- **SQLite is the safe default.** Decades of production exposure, an
  enormous test suite, and tools everywhere. If you're not sure, you
  probably want SQLite.
- **PowDB's sweet spot is read-heavy and aggregate-heavy Rust apps.**
  Embed it in a service that already speaks Rust, where the win on scans
  and aggregates compounds across the request path, and where the
  pure-Rust build chain matters. That is the workload PowDB is built for.

If your evaluation lands somewhere in between, the honest answer is: ship
SQLite, measure your bottleneck, and reach for PowDB if and when an
aggregate or scan path is the long pole.
