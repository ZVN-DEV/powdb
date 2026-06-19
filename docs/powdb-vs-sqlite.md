# PowDB vs SQLite — When to use which

SQLite is the default embedded database for very good reasons: 25+ years of
battle-testing, billions of deployments, and a SQL surface every tool on the
planet understands. PowDB is a newer, pure-Rust embedded database built around
a compiled query execution engine that delivers 3-10x speedups on aggregate
and scan workloads. This guide is written so an evaluator can decide honestly
between the two.

## When to choose PowDB

- **Your stack is pure Rust and you want to keep it that way.** PowDB's
  storage and query engines are 100% Rust with no `libsqlite3-sys`, no
  `bindgen`, no C toolchain in the build path (TLS in `powdb-server` is the
  one optional exception via `aws-lc-sys`, and the `tls` feature can be
  disabled for a C-free build). `cargo install powdb-cli` works on every
  platform Rust supports.
- **Your workload is read-heavy or aggregate-heavy.** The compiled predicate
  engine compiles filter expressions into byte-level operations that skip
  full row decoding. On the benchmarks below, that translates to 3-10x wins
  on `MIN`, `MAX`, `SUM`, `AVG`, `scan + filter + count`, and similar
  scan-shaped queries.
- **You are already on tokio.** `powdb-server` is async-native and wraps the
  engine in `Arc<RwLock<Engine>>` so parallel readers don't block each
  other.
- **You want to embed without a C toolchain.** Useful on Wasm-adjacent
  targets, on minimal container images, and in environments where pulling
  `cmake` into the build is friction.
- **You like the pipeline syntax.** PowQL reads left to right -- source,
  then operations, then projection. If that matches how you think about
  data, it cuts cognitive load. (Linked: [POWQL.md](POWQL.md).)

## When to choose SQLite

- **SQL compatibility matters.** Every ORM, DB browser, analytics tool,
  notebook, language client, and BI dashboard speaks SQL. PowDB does not.
  If you need to point Metabase, DBeaver, or a JDBC driver at your data,
  SQLite is the answer.
- **Battle-testing matters more than peak performance.** SQLite has 25+
  years of production deployment, decades of OSS-Fuzz coverage, and a test
  suite that is famously larger than the codebase itself. PowDB ships
  property tests + 3 fuzz targets (`crates/query/fuzz/`), but is pre-1.0
  and the on-disk format may shift.
- **You need broad tool / language ecosystem support.** Bindings exist for
  essentially every language. PowDB has a TypeScript client and a Rust
  client today.
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
| Build dependencies        | Cargo only (TLS optional `aws-lc-sys`)               | C toolchain                                           |
| Query language            | PowQL (pipeline; left-to-right)                      | SQL (industry standard)                               |
| Storage model             | Slotted-page heap + B+tree indexes                   | B-tree of B-trees                                     |
| Memory-mapped reads       | Yes (zero-syscall scan path)                         | Optional (`PRAGMA mmap_size`)                         |
| Write-ahead log           | Yes (statement-boundary group commit)                | Yes (WAL mode)                                        |
| Compiled predicates       | Yes (byte-level filters, plan cache w/ literal sub)  | Bytecode VM (VDBE)                                    |
| MVCC                      | No (single-writer, parallel readers via RwLock)      | No (single-writer, WAL-mode readers don't block)      |
| Joins                     | Nested-loop + hash (equi-join)                       | Nested-loop + merge + hash                            |
| Window functions          | ROW_NUMBER, RANK, DENSE_RANK, SUM/AVG/MIN/MAX OVER   | Full set                                              |
| Server mode               | Yes (binary wire protocol, TLS, auth)                | Not in core (extensions exist)                        |
| Fuzz testing              | 3 cargo-fuzz targets (lexer, parser, roundtrip)      | OSS-Fuzz, decades of corpora                          |
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
same dataset (100K rows) on an Apple M1, with PowDB in `WalSyncMode::Off`
and SQLite in `:memory:` -- both engines running entirely in RAM. This is
the methodology disclosed in the project README. It favors both engines'
in-memory paths equally; on-disk durable comparisons will move the numbers
and are tracked separately.

| Workload                            | PowDB    | SQLite   | Result            |
|-------------------------------------|----------|----------|-------------------|
| Aggregate MIN                       | 236 us   | 2,340 us | PowDB 9.9x faster |
| Aggregate MAX                       | 236 us   | 2,100 us | PowDB 8.9x faster |
| Aggregate SUM                       | 231 us   | 1,870 us | PowDB 8.1x faster |
| Update by primary key               | 55 ns    | 412 ns   | PowDB 7.5x faster |
| Aggregate AVG                       | 401 us   | 2,300 us | PowDB 5.7x faster |
| Scan + filter + count               | 381 us   | 1,950 us | PowDB 5.1x faster |
| Scan + filter + sort + limit 10     | 2.66 ms  | 9.77 ms  | PowDB 3.7x faster |
| Update by filter (10K rows)         | 2.16 ms  | 6.77 ms  | PowDB 3.1x faster |
| Indexed point lookup                | 93 ns    | 282 ns   | PowDB 3.0x faster |
| Multi-column AND filter             | 2.22 ms  | 4.70 ms  | PowDB 2.1x faster |
| Insert batch (1K rows)              | 238 ns   | 320 ns   | PowDB 1.3x faster |
| Delete by filter (10K rows)         | 1.76 ms  | 2.35 ms  | roughly tied      |
| Scan + filter + project top 100     | 9.6 us   | 12.7 us  | roughly tied      |
| Non-indexed point lookup            | 350 us   | 432 us   | roughly tied      |

The headline wins are exactly where the compiled-predicate engine is
designed to win: aggregates and filtered scans where avoiding full row
decoding pays off. On `insert_batch_1k`, PowDB is faster after Phase 1
stabilization but not by an order of magnitude -- SQLite's writer is
mature. On `delete_by_filter` and the smallest projection workloads the
two are effectively tied; an honest comparison should not pretend
otherwise.

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

- **PowDB is pre-1.0.** The on-disk format may shift across minor versions.
  Pin a version (`cargo install powdb-cli --version 0.6.0 --locked`) and
  expect to re-bench / re-import on upgrades until 1.0.
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
