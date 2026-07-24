# Wide-bench snapshot, 2026-07-24

The first re-run of the PowDB vs SQLite wide bench since 2026-05-18 (v0.3.1).
The numbers published in `README.md`, `docs/powdb-vs-sqlite.md`, and `site/`
had gone sixteen minor releases without being re-measured, and two workload
families were measured through a code path users cannot reach. Both problems
are fixed here.

This file exists because `crates/compare/results.csv` is gitignored (it is a
bench artifact, rewritten on every run).

## Methodology

- Harness: `cargo run --release -p powdb-compare`
- Fixture: 100,000 rows on each engine, identical schema
  (`id int, name str, age int, status str, email str, created_at int`)
- Engines: PowDB (in-process) + SQLite (in-process, `bundled`). Postgres and
  MySQL were skipped (no server reachable, MySQL feature off).
- Both engines run entirely in RAM: PowDB in `WalSyncMode::Off`, SQLite in
  `:memory:`. This isolates query-engine cost from disk cost. It is **not** a
  durability comparison; see [Durable writes](#durable-writes) below.
- **5 full runs; the table reports the per-workload median of the 5.** A single
  run was not trustworthy on this machine (see [Measurement noise](#measurement-noise)).
- Commit: `a090568`

### Hardware and toolchain

| | |
|---|---|
| Machine | Apple M5 Max, 18 cores, 128 GB RAM |
| OS | macOS 26.5.1 (build 25F80) |
| Toolchain | rustc 1.97.0 (2d8144b78 2026-07-07) |
| Profile | `--release` |

**These are laptop numbers, not CI numbers.** They were captured on a
developer machine running a normal desktop workload, not on the single-tenant
Depot runner that `crates/bench/baseline/main.json` is pinned to. That baseline
file was deliberately not touched by this capture. Treat everything here as
indicative of ratios on one machine, not as a regression gate.

## What changed in the harness

Two workload families previously bypassed PowDB's own front end while SQLite
paid full `prepare_cached` cost on every workload. Those rows were not
reachable by a user typing PowQL, so they have been rewired to the normal path
(`crates/compare/src/engines/powdb.rs`):

1. **The four non-count aggregates** (`agg_sum`, `agg_avg`, `agg_min`,
   `agg_max`) hand-built a `PlanNode::Aggregate` in Rust and called
   `execute_plan` directly, under a comment stating it "bypasses the parser and
   planner entirely". That workaround existed because the parser could not
   attach a column to a non-count aggregate. It can now: the parser lifts a
   trailing single-field projection into the aggregate argument
   (`parser.rs::test_parse_sum_with_field_projection`), so these run as real
   PowQL, e.g. `min(User { .created_at })`.

2. **`point_lookup_indexed`** called `tbl.index("id").lookup_int(id)` and
   decoded the row by hand, skipping lex, parse, plan, and execute. It now runs
   `User filter .id = <n> limit 1 { .name }`.

The repo's own gate file `crates/bench/baseline/thesis-ratios.json` already
quantified what was being skipped: `powql_point_over_btree_lookup`, the full
PowQL front-end overhead over a raw B-tree probe, sits at **6.17x**. The
measurement below independently reproduces that: the PowQL point lookup costs
1,650 ns against a raw probe that used to report ~62 ns in this same harness.

`insert_single` is also included in the published tables for the first time. It
was measured all along and omitted from every published copy, which mattered
because it is a workload PowDB wins.

## Raw results (ns/op, median of 5 runs)

| workload | PowDB | SQLite | ratio (SQLite/PowDB) | verdict |
|---|---:|---:|---:|:---|
| update_by_pk                  |            66 |           500 | 7.59x | WIN |
| agg_min                       |       266,011 |     1,773,160 | 6.67x | WIN |
| agg_max                       |       270,846 |     1,535,437 | 5.67x | WIN |
| agg_sum                       |       280,851 |     1,567,104 | 5.58x | WIN |
| agg_avg                       |       516,448 |     1,817,720 | 3.52x | WIN |
| scan_filter_count             |       480,759 |     1,472,722 | 3.06x | WIN |
| point_lookup_nonindexed       |       116,996 |       321,298 | 2.75x | WIN |
| scan_filter_sort_limit10      |     2,679,845 |     6,734,454 | 2.51x | WIN |
| insert_single                 |           394 |           790 | 2.01x | WIN |
| multi_col_and_filter          |     1,747,491 |     3,455,189 | 1.98x | WIN |
| update_by_filter              |     2,657,597 |     5,083,403 | 1.91x | WIN |
| delete_by_filter              |     1,654,722 |     1,950,222 | 1.18x | tied |
| scan_filter_project_top100    |         7,998 |         8,859 | 1.11x | tied |
| insert_batch_1k               |           232 |           257 | 1.11x | tied |
| **point_lookup_indexed**      |     **1,650** |       **208** | **0.13x** | **LOSS (7.9x slower)** |

**Score: 11 wins, 3 roughly tied, 1 loss.**

## What the published numbers claimed

The tables that shipped in `README.md`, `docs/powdb-vs-sqlite.md`, and
`site/index.html` were identical to each other and stale. Every headline ratio
was overstated:

| workload | published | measured | overstatement |
|---|---:|---:|---|
| agg_min                  | 9.9x | 6.67x | +48% |
| agg_max                  | 8.9x | 5.67x | +57% |
| agg_sum                  | 8.1x | 5.58x | +45% |
| agg_avg                  | 5.7x | 3.52x | +62% |
| scan_filter_count        | 5.1x | 3.06x | +67% |
| scan_filter_sort_limit10 | 3.7x | 2.51x | +47% |
| update_by_filter         | 3.1x | 1.91x | +62% |
| **point_lookup_indexed** | **3.0x faster** | **7.9x slower** | **sign error** |

The point-lookup row is the important one. It was never a 3x win. It was a raw
B-tree probe timed against SQLite's full prepared-statement path, and the
"3.0x faster" claim inverted once PowDB was made to run its own query language.

The published tables also credited the run to an "M1", which no longer matched
the machine, and omitted `insert_single` entirely.

## Honest framing

**Where PowDB genuinely wins.** The compiled-predicate engine does what it says
on scan-shaped work: aggregates land at 3.5-6.7x and a filtered count at 3.1x.
These wins come from compiled byte-level predicates and zero-copy mmap scans,
not from PowQL's syntax. A SQL query lowered through the SQL frontend runs the
same plan on the same executor and gets the same numbers.

**Where PowDB loses, and why it matters.** `point_lookup_indexed` is 7.9x
slower than SQLite. Once the index has been probed the work is trivial (the
raw probe was ~62 ns), so essentially the entire 1,650 ns is PowDB front-end
cost: lex, parse, canonicalize, plan-cache lookup, literal substitution. SQLite
amortizes that away with a prepared statement and pays roughly 208 ns end to
end. For an application whose hot path is "fetch one row by id", PowDB is
currently the wrong choice, and no amount of scan throughput compensates.

**A methodology asymmetry worth naming.** SQLite's adapter uses
`prepare_cached` for every read workload. PowDB's read workloads (except the
prepared write paths) go through `execute_powql` with a fresh query string, so
they pay canonicalization on every call. For workloads measured in hundreds of
microseconds this is under 1% and irrelevant. For the sub-microsecond point
lookup it is essentially the whole measurement. PowDB does expose
`prepare` / `execute_prepared`, and a driver using it would close much of that
specific gap; that path was deliberately **not** substituted in just for the
one workload where PowDB looks bad, because per-workload methodology switching
is what produced the misleading numbers in the first place. Anyone wanting the
prepared-path number should measure it rather than assume it, and it has not
been measured here.

**Writes are competitive, not dominant.** `insert_single` at 2.0x and
`insert_batch_1k` at 1.1x are honest small wins in an in-memory configuration.
They say nothing about durable throughput.

## Measurement noise

This was captured on a working laptop, not an idle benchmark host, and it
showed. Across the 5 runs:

- Read workloads were stable: the spread between the fastest and slowest run
  stayed inside roughly ±13% of the median.
- Write workloads had severe outliers. One run reported `insert_single` at
  113.8 us and `insert_batch_1k` at 113.6 us, roughly 300x and 600x their
  medians, from what was almost certainly a background disk stall. Those runs
  are exactly why this snapshot reports a median of 5 rather than a single run.

If you re-run this, expect the absolute numbers to move and the verdicts to
hold. The one verdict worth re-checking on your own hardware is
`point_lookup_indexed`, since it is dominated by a fixed per-query cost rather
than by data volume.

## Durable writes

Everything above runs with durability off on both sides. In production PowDB
defaults to `WalSyncMode::Full`, where every autocommit statement fsyncs before
returning, so a single-row insert is fsync-bound.

Batching in a transaction collapses the batch into one fsync. That claim now
has a committed repro instead of a reference to unpublished internal
benchmarks:

```bash
cargo run --release -p powdb-compare --example write_batching
```

Measured on the same machine, three consecutive runs: **55.5x, 48.8x, 50.4x**,
e.g. 249 rows/sec in autocommit against 13,841 rows/sec in one transaction, at
identical durability. The ratio is a property of your disk's fsync rate
multiplied by your batch size, not a fixed property of PowDB, so measure your
own hardware before quoting it.

## Reproducibility

```bash
cargo run --release -p powdb-compare                          # 15-workload table
cargo run --release -p powdb-compare --example write_batching # durable batching
```

The first rewrites `crates/compare/results.csv`. Run it several times and take
medians; a single run on a busy machine is not meaningful for the
sub-microsecond workloads.
