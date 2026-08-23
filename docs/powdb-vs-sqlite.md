# PowDB vs SQLite: When to use which

SQLite is the default embedded database for very good reasons: 25+ years of
battle-testing, billions of deployments, and a SQL surface every tool on the
planet understands. PowDB is a newer, pure-Rust embedded database built around
a compiled query execution engine that delivers 3-7x speedups on aggregates
and 1-3.7x on filtered scans, and loses to SQLite by roughly 16x on indexed
point lookups. This guide is written so an evaluator can decide honestly
between the two.

## When to choose PowDB

- **Your stack is pure Rust and you want to keep it that way.** PowDB's
  storage and query engines are 100% Rust with no `libsqlite3-sys`, no
  `bindgen`, and no C toolchain in the build path. The two *binaries* are the
  exception: `powdb-server` and `powdb-cli` reach TLS through `tokio-rustls`,
  which pulls `aws-lc-sys` and therefore needs a C compiler and `cmake`.
  Neither crate declares any Cargo features, so there is no `tls` feature to
  turn off and no C-free build of the binaries today. `cargo install
  powdb-cli` works on Linux and macOS. **Windows is not supported** and does not compile: the heap's mmap
  scan path is Unix-only. SQLite has no such gap, and if you ship on Windows
  that alone decides this evaluation.
- **Your workload is read-heavy or aggregate-heavy.** The compiled predicate
  engine compiles filter expressions into byte-level operations that skip
  full row decoding. On the benchmarks below, that translates to 3.7-7.7x on
  the aggregates (`MIN`, `MAX`, `SUM`, `AVG`) and 3.7x on
  `scan + filter + count`. Other scan-shaped queries gain less: 2.6x on
  sort+limit, 2.0x on a multi-column AND filter, and nothing at all on a
  small top-100 projection. It does not help point lookups, where SQLite
  wins by roughly 16x.
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
  subset of SQL through a frontend (since v0.5.0, see [SQL.md](SQL.md)), but
  it speaks its own binary wire protocol, not the Postgres/MySQL wire or a
  JDBC/ODBC surface. Every ORM, DB browser, analytics tool, notebook, and BI
  dashboard talks to SQLite out of the box; none of them can point at PowDB.
  If you need Metabase, DBeaver, or a JDBC driver on your data, SQLite is the
  answer.
- **Battle-testing matters more than peak performance.** SQLite has 25+
  years of production deployment, decades of OSS-Fuzz coverage, and a test
  suite that is famously larger than the codebase itself. PowDB ships
  property tests + 9 fuzz targets (`crates/query/fuzz/`), but is pre-1.0
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
| Build dependencies        | Engine crates: Cargo only. `powdb-server`/`powdb-cli`: C toolchain + `cmake` (`aws-lc-sys`, not optional) | C toolchain |
| Query language            | PowQL (pipeline) + supported SQL subset via frontend | SQL (industry standard)                               |
| Storage model             | Slotted-page heap + B+tree indexes                   | B-tree of B-trees                                     |
| Memory-mapped reads       | Yes (zero-syscall scan path)                         | Optional (`PRAGMA mmap_size`)                         |
| Write-ahead log           | Yes (statement-boundary group commit)                | Yes (WAL mode)                                        |
| Compiled predicates       | Yes (byte-level filters, plan cache w/ literal sub)  | Bytecode VM (VDBE)                                    |
| MVCC                      | No (single-writer, parallel readers via RwLock)      | No (single-writer, WAL-mode readers don't block)      |
| Joins                     | Nested-loop + hash (equi-join)                       | Nested-loop + merge + hash                            |
| Window functions          | ROW_NUMBER, RANK, DENSE_RANK, SUM/AVG/MIN/MAX OVER   | Full set                                              |
| Server mode               | Yes (binary wire protocol, TLS, auth)                | Not in core (extensions exist)                        |
| Fuzz testing              | 9 cargo-fuzz targets (lexer, parser, roundtrip, SQL, PJ1, wire, WAL replay, execute, catalog open) | OSS-Fuzz, decades of corpora |
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
`:memory:` -- neither engine fsyncs. This is the methodology
disclosed in the project README. It favors both engines' in-memory paths
equally; on-disk durable comparisons will move the numbers and are tracked
separately.

Median of 5 runs on an Apple M5 Max laptop (macOS 26.5.1, rustc 1.97.0),
commit `e3dfa71`, measured 2026-08-15. **Laptop numbers, not CI numbers.**
Full methodology and per-run spread:
[2026-07-24 snapshot](benchmarks/2026-07-24-wide-bench-snapshot.md).

| Workload                            | PowDB    | SQLite   | Result                 |
|-------------------------------------|----------|----------|------------------------|
| Aggregate MIN                       | 221 us   | 1.70 ms  | PowDB 7.7x faster      |
| Aggregate MAX                       | 217 us   | 1.47 ms  | PowDB 6.8x faster      |
| Aggregate SUM                       | 234 us   | 1.45 ms  | PowDB 6.2x faster      |
| Update by primary key               | 60 ns    | 272 ns   | PowDB 4.5x faster      |
| Aggregate AVG                       | 455 us   | 1.70 ms  | PowDB 3.7x faster      |
| Scan + filter + count               | 380 us   | 1.40 ms  | PowDB 3.7x faster      |
| Non-indexed point lookup            | 101 us   | 319 us   | PowDB 3.2x faster      |
| Scan + filter + sort + limit 10     | 2.46 ms  | 6.41 ms  | PowDB 2.6x faster      |
| Multi-column AND filter             | 1.58 ms  | 3.21 ms  | PowDB 2.0x faster      |
| Update by filter (10K rows)         | 2.36 ms  | 4.54 ms  | PowDB 1.9x faster      |
| Insert single row                   | 380 ns   | 638 ns   | roughly tied           |
| Scan + filter + project top 100     | 8.1 us  | 8.9 us  | roughly tied           |
| Delete by filter (10K rows)         | 1.57 ms  | 1.75 ms  | roughly tied            |
| Insert batch (1K rows)              | 242 ns   | 214 ns   | roughly tied            |
| **Indexed point lookup**            | **3.17 us** | **202 ns** | **SQLite 15.7x faster** |

The wins are where the compiled-predicate engine is designed to win:
aggregates, at 3.7-7.7x. The scan-shaped workloads are a weaker story than
"aggregate and scan" suggests: only `scan_filter_count` (3.7x) lands in that
range, while sort+limit is 2.6x, the multi-column AND filter is 2.0x, and
project-top-100 is a tie. Three write rows (`delete_by_filter`,
`insert_batch_1k`, and `insert_single`) sit close enough to parity that they
change sign depending on what else the machine is doing, so read them as ties
rather than as wins in either direction. An honest comparison should not
pretend otherwise.

**Read the point-lookup row before you decide.** PowDB is roughly 15x *slower*
than SQLite at fetching one row by indexed id. Almost all of that 3.17 us is
PowDB's own front end (lex, parse, canonicalize, plan-cache lookup); the
B-tree probe underneath is tens of nanoseconds. SQLite pays roughly 202 ns
end to end because a prepared statement amortizes its parser away. This gap
has widened: it was published as 7.9x against an older engine, and five
independent re-measurements of the current one, across two machines, landed
from 10x to 20x, most of them above 15x. If your hot path is single-row fetches,
that is the number that should decide this evaluation, and it points at
SQLite.

One caveat on the two insert rows. PowDB writes into a real temporary
directory while SQLite runs in `:memory:`, so PowDB's insert numbers are
sensitive to competing disk I/O in a way SQLite's are not. Re-running these
while the same laptop was busy compiling degraded PowDB's inserts by 30-100x
while every other row moved by under 2x, and SQLite's inserts did not move at
all. The figures above are from an otherwise-idle machine; treat them as an
upper bound on what a loaded host will give you.

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
  wrote. Pin a version (`cargo install powdb-cli --version 0.25.0 --locked`)
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
