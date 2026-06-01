# Phase 3 — Risky Engine Upgrades (research-first)

**Status:** RISKY / research complete, awaiting Kirby go/no-go (2026-05-30)
**Posture:** Phase 3 touches load-bearing engine internals (planner, sort path, page format on Windows). Regressions here can erase PowDB's headline 3-10x read-perf wins. Do NOT plan or implement until the per-subsystem decisions below are made.

## Executive summary — per-subsystem verdict

| Subsystem | Verdict | Blast radius | Cheapest start | Notes |
|---|---|---|---|---|
| **A. Windows file I/O** | **GO** | Smallest | `#[cfg(windows)] FileExt::seek_read/seek_write` shim in `disk.rs` + Windows CI lane. Defer mmap. | std-only, no new dep. ~1–3 day effort. |
| **B. Disk-spill external sort** | **GO** (after guard bench) | Medium | Spill-on-`MemoryLimitExceeded` only. Reuse Phase 1 memory budget as the switch point. K-way merge (cap 64-way) over length-prefixed encoded rows. In-memory hot path must remain byte-for-byte unchanged. | Postgres-style design. Risks 4 of 15 benches (sort/agg) — needs plan-shape snapshot guard first. |
| **C. Cost-based optimizer w/ catalog access** | **CONDITIONAL GO** | **Highest** | Ship the `CatalogStats` trait + `NoStats` default + `ANALYZE` syntax as a **separate PR** from the first optimizer rule. Plan-shape stability snapshot tests for all 15 bench queries must land FIRST. The first rule (index vs scan) is then Phase 3.5/4. | The compiled fast paths pattern-match on plan shape — any reshape silently drops the 3-10x wins. |
| **D. True multi-writer MVCC** | **NO-GO** | Very large | n/a — defer until a measured multi-reader/multi-writer workload justifies it. | Per-row visibility check would shrink agg wins to 1.5-3x. PowDB has no concurrent benches today. |

**Recommended Phase 3 scope** (per the research): A + B + the *plumbing* of C (no first optimizer rule yet). D deferred.

## Why Phase 3 is risky (the architectural reason)

1. **Cost-based optimizer with catalog access** — currently the planner is a pure function. Adding catalog-driven cost estimation changes plan shapes; existing compiled-predicate fast paths pattern-match on specific shapes (`Filter(SeqScan)` etc., per CLAUDE.md). A new plan shape that the executor doesn't recognize silently drops to the slow generic path, regressing the 3-10x agg wins.
2. **Disk-based / external-merge sorting** — adds a new I/O path under the executor, interacts with WAL + page cache, must respect the memory budget shipped in Phase 1, and is the natural extension of `QueryError::MemoryLimitExceeded` from "error cleanly" to "spill cleanly."
3. **Windows pread/pwrite** — looks small (one TODO in `disk.rs`) but Windows file I/O semantics differ enough (overlapped I/O, sharing modes, mmap+delete interaction) to risk subtle durability bugs the existing macOS/Linux test suite won't catch.
4. *(Deferred from Phase 1: true multi-writer MVCC — even larger blast radius; only revisit if the single-writer ceiling becomes a real bottleneck.)*

## Research dossier

The research below was produced by a dedicated deep-research agent (2026-05-30) drawing on SQLite, DuckDB, Postgres, RocksDB, LMDB, and redb design docs, plus CIDR/VLDB/ICDE papers. Citations inline.

### Subsystem A — Windows file I/O port

**How comparable engines solve this**

- **SQLite (`os_win.c`)** uses `CreateFile` with `FILE_SHARE_READ | FILE_SHARE_WRITE`, *synchronous* I/O (not overlapped — overlapped semantics are unsound with stack `OVERLAPPED` structs), and `FlushFileBuffers` for durability. Locking is byte-range via `LockFileEx` ([SQLite locking v3](https://sqlite.org/lockingv3.html), [os_win.c](https://github.com/sqlite/sqlite/blob/master/src/os_win.c)).
- **RocksDB** uses `FlushFileBuffers` on Windows, groups commits to amortize the cost ([RocksDB WAL Performance](https://github.com/facebook/rocksdb/wiki/WAL-Performance)).
- **LMDB** uses mmap on both Unix and Windows; durability is `msync` / `FlushViewOfFile` + `FlushFileBuffers`.
- **redb** uses `std::os::windows::fs::FileExt::seek_read`/`seek_write` — same shape as PowDB's POSIX path ([redb design](https://github.com/cberner/redb/blob/master/docs/design.md)).

**Rust crate options**

1. **`std` (`FileExt::seek_read`/`seek_write`)** — In-tree, zero dep. Caveat: not safely combinable with `FILE_FLAG_OVERLAPPED` ([rust-lang/rust#81357](https://github.com/rust-lang/rust/issues/81357)), but safe with default synchronous handles. Per-call kernel transition overhead is small relative to 4KB I/O.
2. **`rustix`** — Cross-platform syscall wrapper. Adds a dep; worth it only if PowDB plans other rustix features.
3. **`positioned-io` / `positioned-io2`** — Provides `ReadAt`/`WriteAt` traits. Useful for abstracting over `&File` vs `Cursor<Vec<u8>>` in tests but otherwise overkill ([positioned-io2](https://github.com/surban/positioned-io2)).

**Recommendation:** stick with `std`. Minimal port: `#[cfg(windows)] use std::os::windows::fs::FileExt` and a tiny inline wrapper renaming `read_exact_at`/`write_all_at` ↔ `seek_read`/`seek_write`. No external dep.

**Hidden gotchas**

1. **`FILE_SHARE_DELETE`** — Windows default sharing prevents renaming/deleting an open file. WAL checkpoint rename/truncate could surface this. Use `OpenOptions::share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)` on Windows.
2. **mmap blocks deletion even with `FILE_SHARE_DELETE`** ([Sublime HQ — Use mmap with care](https://www.sublimetext.com/blog/articles/use-mmap-with-care)). PowDB's mmap scan path will silently break "delete table" on Windows. **Defer mmap on Windows for v1.**
3. **Antivirus interactions.** Defender briefly holds files during scan; SQLite explicitly retries with backoff in `winRetryIoerr`. Real source of CI flakiness for naive ports.
4. **Durability semantics.** `FlushFileBuffers` flushes the disk cache on consumer Windows for non-volatile media; SQLite documents Windows guarantees as stronger by default but slower. Keep `sync_data` (not `sync_all`).
5. **`seek_read` partial reads.** Like POSIX `read`. PowDB's existing `read_exact_at` loops internally; `std` provides this for both platforms.

**Risk for PowDB**

I/O is exercised by every test. Specific guard tests needed:
- Re-run of `test_create_and_read_page` / `test_reopen_file` on Windows CI.
- Crash-recovery test: kill process between `write_page` and `flush`, reopen, verify CRC32 catches torn writes. **Open question:** Windows write granularity differs from Linux — torn-write window may differ. NTFS 4KB cluster typically gives atomicity, but not guaranteed across configs.
- Concurrent-read stress: ≥4 threads issuing `read_page` on a shared `DiskManager` to confirm Windows `seek_read` is thread-safe on a shared handle.

**Go/no-go:** **GO**, defer mmap path.
**Cheapest start:** `#[cfg(windows)]` branch in `disk.rs` + Windows GitHub Actions lane in `ci.yml` running `cargo test --workspace`.
**Regressions to watch:** `insert_single` latency (FlushFileBuffers has higher per-call cost than fdatasync historically).

### Subsystem B — Disk-spill external-merge sort

**How comparable engines solve this**

- **Postgres** uses balanced k-way merge via `tuplesort.c` + `logtapes.c`. Switched from polyphase merge (Knuth 5.4.2D) because modern "tapes" — temp files — are cheap. Run generation is quicksort; merging is balanced k-way. `work_mem` is the per-query, per-sort budget ([Postgres Parallel External Sort](https://wiki.postgresql.org/wiki/Parallel_External_Sort), [tuplesort.c](https://github.com/postgres/postgres/blob/master/src/backend/utils/sort/tuplesort.c)).
- **DuckDB** (Kuiper 2021 / 2025) uses a *unified row layout* in fixed-size buffer-managed blocks (~256KB), MSD radix sort (Ska Sort) on a normalized 64-bit prefix with pdqsort fallback, and *page-by-page* spilling rather than all-or-nothing. The buffer manager — not the sort op — decides when to spill ([Kuiper DBDBD2021](https://www.wis.ewi.tudelft.nl/assets/DBDBD2021_submissions/DBDBD2021_paper_8.pdf), [DuckDB external sorting](https://duckdb.org/2021/08/27/external-sorting), [DuckDB sorting 2025](https://duckdb.org/2025/09/24/sorting-again)).
- **SQLite** uses a temp B-tree for sort, doubling as the GROUP BY structure. Operationally simple, slower than dedicated sort.

The literature converges on **k-way balanced merge with quicksort run generation**. PowDB does not need DuckDB's radix-sort sophistication for v1.

**Design space for PowDB**

For v1, **option 1: external merge sort over flat spill format** (Postgres-style). Quicksort each in-memory run, write to a temp file, k-way merge at the end. The `sort_limit10` bench is already streamed via bounded `BinaryHeap<K>` and stays untouched.

**Concrete design**

- **Spill location:** `spill/` under `POWDB_DATA`, unique tempfile per query (PID + query ID + run index). Configurable via `POWDB_SPILL_DIR`. Cleaned via guard struct; on crash, sweep on engine open.
- **Spill format:** length-prefixed `encode_selective` rows. **Not** the slotted page format, **not** WAL-logged — these are scratch files.
- **Memory budget interaction:** when `mem_budget::charge()` would exceed budget, the sort operator sorts its in-memory buffer with `pdqsort`, writes a run, drops the buffer (releasing budget), continues reading.
- **Merge fan-in:** cap at 64-way; multi-pass for larger sorts. (Postgres uses `work_mem / TAPE_BUFFER_OVERHEAD`; the 64-way cap is a reasonable simplification for PowDB v1.)
- **Per-page I/O target:** same code path as data-file reads. Realistic target: 1M-row sort+merge in <2x in-memory sort cost.

**Performance commitment**

At small N (≤ budget) there must be **zero perf cost**. The in-memory path `let mut buf = Vec::new(); for row in input { buf.push(row); } buf.sort()` must remain byte-for-byte. The spill-capable path wraps it: `if charge(buf.len()).is_err() { switch to spill mode }`. This is the architectural commitment.

**Risk matrix**

- `scan_filter_sort_limit10` — bounded top-K, already streamed. Untouched if spill only activates on budget overflow.
- `agg_sum`/`agg_avg`/`agg_min`/`agg_max` — single-group, no sort, no risk.
- A hypothetical `large_unindexed_sort` — **needs to be added as a guard bench BEFORE spill code lands**.

**Guard tests needed before code change**

- 10M-row sort bench at smaller memory budget than dataset. Today errors; after spill must succeed within 2x in-memory perf at 10x dataset size.
- Plan-shape snapshot: `scan_filter_sort_limit10` produces same `PlanNode` tree before/after.
- `MemoryLimitExceeded` guard confirming unbounded sort either still errors or now spills deterministically without leaking FDs.

**Go/no-go:** **GO** after guard benches land.
**Cheapest start:** spill-on-overflow for generic sort path only (not group-by, not aggregation, not the Project/Limit/Sort fast path). Cap fan-in at 64. Reuse `pdqsort`.
**Regressions to watch:** `scan_filter_sort_limit10`, `agg_*`.

### Subsystem C — Cost-based query optimizer with catalog access

**Why this is the highest-blast-radius subsystem**

`crates/query/src/executor/plan_exec.rs` has at least eight pattern-matching fast paths keyed on plan-tree *shape*: `Project(Limit(Sort(Filter(SeqScan))))`, `Project(Limit(Filter(SeqScan)))`, `Project(Filter(SeqScan))`, `Aggregate(SeqScan)`, `Aggregate(Filter(SeqScan))`, `Count(Filter(SeqScan))`, `Update(Filter(SeqScan))`, and literal-only update fast paths. These are the source of the 3-10x SQLite wins. Any planner change that reshapes the tree (introduces `Materialize`, swaps `Filter(SeqScan)` for `IndexScan(SeqScan)`, reorders predicates) silently drops queries into the slow generic path — a correctness-preserving perf regression, hard to catch in unit tests, easy in benches.

**How comparable engines solve cost-based optimization**

- **SQLite (`stat4` + ANALYZE):** per-index histograms (up to 24 samples per index in stat4, vs older stat1's single rowcount). Critically, SQLite's planner is *plan-shape stable* — ANALYZE chooses among existing physical plan templates, not whether to materialize intermediates ([SQLite ANALYZE](https://sqlite.org/lang_analyze.html), [optoverview](https://sqlite.org/optoverview.html)).
- **Postgres (`pg_statistic`):** per-column most-common-values + histogram (default 100 buckets), distinct-count, null fraction, correlation. Auto-vacuum runs ANALYZE. Significantly more expensive than SQLite to collect and plan against ([Postgres planner stats](https://www.postgresql.org/docs/current/planner-stats.html)).
- **DuckDB:** cardinality-driven join enumeration via DPhyp. Famous failure mode: zero cardinality estimates from missing Parquet column stats produce disastrous join orders ([DuckDB join ordering](https://www.alibabacloud.com/blog/duckdb-internals---part-7-join-reordering-optimization_602899), [issue #11638](https://github.com/duckdb/duckdb/issues/11638)). The lesson: cost-based reordering is fragile when stats are wrong.

**Minimum viable cost-based step for PowDB**

The only credible MVP that protects the fast paths is one that **does not change plan shape**, only chooses between equally-shaped alternatives that both have fast paths:

> Choose between `IndexScan(table, col, range)` and `Filter(SeqScan(table), predicate)` based on a cardinality estimate. Both shapes already have executor fast paths.

Cheap stats:
1. **Row count per table** — already cheap to maintain incrementally.
2. **Per-indexed-column distinct count** — cheap on B+ tree build; harder incremental. For v1, recompute on `ANALYZE` only.
3. *(Optional, defer)* Per-column min/max.

A 4-bucket equidepth histogram is the natural next step but Phase 4+, not Phase 3.

**Threading catalog access into the pure planner**

```rust
trait CatalogStats {
    fn row_count(&self, table: &str) -> Option<u64>;
    fn distinct_count(&self, table: &str, col: &str) -> Option<u64>;
}
fn plan(stmt: &Statement, stats: &dyn CatalogStats) -> PlanNode;
```

Default `NoStats` always returns `None`. Existing call sites pass `&NoStats` — planner falls back to current heuristics. Tests pass a `MockStats`; production passes a `&Engine` adapter. **Phase 3 ships only this plumbing + `ANALYZE` syntax. The first actual rule is Phase 3.5/4.**

**Most vulnerable benchmarks**

- `point_lookup_indexed` — already uses index path; risk = optimizer chooses scan over index.
- `scan_filter_count`, `scan_filter_project_top100` — use `Filter(SeqScan)` fast paths. Risk = optimizer reshapes to materialize. **Highest concern.**
- `multi_col_and_filter` — both predicates compiled. Risk = optimizer splits into stacked Filters; compiled predicate doesn't fire.

**Guard tests required BEFORE code lands**

1. **Plan-shape stability snapshot test** for each of the 15 bench queries. Single most important guard.
2. **Compiled-predicate fire counter** — instrument compile_predicate, count fires, assert against baseline.
3. **Stats-driven path test** — populate `MockStats`, assert a *different* plan is chosen. The "we actually used the stats" assertion.

**Go/no-go:** **CONDITIONAL GO** — only if all three guard tests above ship first and pass.
**Cheapest start:** `CatalogStats` trait + `NoStats` default + single `ANALYZE` PowQL statement. No optimizer rule yet.
**Open question:** does PowDB's catalog already track per-table row counts? If yes, plumbing is trivial. Read `catalog.rs` to confirm.

### Subsystem D — True multi-writer concurrency

**How comparable engines trade off**

- **SQLite WAL mode:** concurrent readers + single writer; writers append to WAL, readers peek. Complexity is in WAL-index shared memory + checkpoint logic ([SQLite WAL](https://sqlite.org/wal.html), [Fly.io SQLite internals](https://fly.io/blog/sqlite-internals-wal/)). PowDB already approximates this via `Arc<RwLock<Engine>>` + WAL — without snapshot isolation.
- **Postgres MVCC:** row-versioned with `xmin`/`xmax`. Truly concurrent writers at cost of dead-tuple bloat + vacuum complexity. Order of magnitude more complex than WAL-mode SQLite.
- **LMDB:** single writer + COW reader snapshots. Each writer creates a new root; readers pin a root and walk immutable B-tree. Closest match to PowDB's design philosophy ([How LMDB works](https://xgwang.me/posts/how-lmdb-works/)).
- **redb:** mirrors LMDB exactly: COW B-trees, MVCC via root snapshots ([redb design](https://github.com/cberner/redb/blob/master/docs/design.md)).

**Minimum credible step for PowDB**

The LMDB/redb COW model differs from the existing 57-line `mvcc.rs` undo-log scaffolding (Postgres-style in-place + undo records, no visibility check, no transaction manager, no executor integration). Either path — wiring undo log through every executor read OR rearchitecting B+ tree as COW — is **4-6 weeks minimum** and touches every executor read path.

**Risk to PowDB's perf wins**

3-10x agg wins come from zero-copy mmap scans + byte-level compiled predicates. MVCC visibility = per-row branch + per-row dereference. Even aggressively optimized: 5-15 ns per row. On a 10M-row scan, **50-150ms overhead vs current ~30-50ms baseline.** **Agg wins would shrink to 1.5-3x.**

LMDB-style COW avoids per-row check but requires rearchitecting the B+ tree, incompatible with PowDB's in-place page updates.

**Bench coverage needed (doesn't exist today)**

- Multi-reader/single-writer throughput (N readers + 1 writer, p50/p99).
- Long-running-reader test (start reader, do M writes, measure read latency stability — tests version-chain bloat).
- Snapshot-isolation correctness (reader sees consistent view across concurrent writer).

**Go/no-go:** **NO-GO** for Phase 3.

Three reasons:
1. **No measured need.** No bench shows read contention is a bottleneck.
2. **Blast radius = entire executor.** Every read path consults visibility; compiled-predicate fast paths all need MVCC-aware variants.
3. **`Arc<RwLock<Engine>>` already gives concurrent readers.** What's missing is snapshot isolation under concurrent writes, which only matters once concurrent writes exist.

**If reconsidered:** the cheapest start is *not* lighting up the undo log — it's adding a multi-reader bench first, measuring contention, and only then deciding. The `mvcc.rs` scaffolding should remain a marker for future intent, not load-bearing.

### Cross-cutting: regression safety

**Per-benchmark risk matrix**

| Benchmark | A (Windows) | B (Spill sort) | C (CBO) | D (MVCC) |
|---|---|---|---|---|
| point_lookup_indexed | low | none | **HIGH** | medium |
| point_lookup_nonindexed | low | none | medium | medium |
| scan_filter_count | low | none | **HIGH** | **HIGH** |
| scan_filter_project_top100 | low | none | **HIGH** | **HIGH** |
| scan_filter_sort_limit10 | low | low (bounded) | **HIGH** | **HIGH** |
| agg_sum/avg/min/max | low | none | medium | **HIGH** |
| multi_col_and_filter | low | none | **HIGH** | **HIGH** |
| insert_single | medium (FlushFileBuffers) | none | low | medium |
| insert_batch_1k | medium | none | low | medium |
| update_by_pk | low | none | low | medium |
| update_by_filter | low | none | medium | **HIGH** |
| delete_by_filter | low | none | medium | **HIGH** |

**Guard tests / bench thresholds / fuzz targets needed BEFORE any Phase 3 code change**

1. **Tighten the bench regression gate** — switch the at-risk benches from p50 to p99 with a 5% threshold.
2. **Plan-shape stability snapshot test** (critical for C). Serialize `PlanNode` with `Debug`, snapshot for all 15 queries.
3. **Compiled-predicate fire counter** — assert expected number of fires per bench.
4. **Windows CI lane** (required before A).
5. **Large-sort bench** (required before B). 10x memory budget. Today errors; after spill must complete <2x linear extrapolation.
6. **Multi-reader concurrent bench** — only if D reconsidered.
7. **Planner fuzz target** — random PowQL → assert parses-or-errors-cleanly + plan shape matches known fast-path shapes OR documented generic.
8. **Crash-recovery test on Windows** — SIGKILL between write+flush, reopen, verify CRC catches torn pages.

### Open questions requiring PowDB-specific measurement

1. **Does PowDB's catalog track per-table row counts?** Determines whether Subsystem C's stats plumbing is trivial or its own mini-project. Read `catalog.rs`.
2. **Is the 3-10x SQLite ratio Linux-only?** Windows port may shift the absolute, but ratio should hold (SQLite-on-Windows has the same `FlushFileBuffers` penalty).
3. **NTFS write-atomicity at 4KB?** CRC32 catches torn writes either way — but worth documenting.
4. **Is `mvcc.rs` undo log used anywhere?** If unused, deleting it reduces cognitive load.
5. **Does the bench regression gate use p50 or p99?** Material to threshold-tightening recommendation.

## Sources

[SQLite os_win.c](https://github.com/sqlite/sqlite/blob/master/src/os_win.c) · [SQLite Locking v3](https://sqlite.org/lockingv3.html) · [SQLite WAL](https://sqlite.org/wal.html) · [SQLite ANALYZE](https://sqlite.org/lang_analyze.html) · [SQLite Query Optimizer Overview](https://sqlite.org/optoverview.html) · [Fly.io SQLite Internals](https://fly.io/blog/sqlite-internals-wal/) · [Postgres tuplesort.c](https://github.com/postgres/postgres/blob/master/src/backend/utils/sort/tuplesort.c) · [Postgres Parallel External Sort](https://wiki.postgresql.org/wiki/Parallel_External_Sort) · [Postgres Planner Statistics](https://www.postgresql.org/docs/current/planner-stats.html) · [DuckDB External Sorting (2021)](https://duckdb.org/2021/08/27/external-sorting) · [DuckDB Redesigning Sort 2025](https://duckdb.org/2025/09/24/sorting-again) · [Kuiper Efficient External Sorting](https://www.wis.ewi.tudelft.nl/assets/DBDBD2021_submissions/DBDBD2021_paper_8.pdf) · [Kuiper Robust External Hash Aggregation ICDE 2024](https://duckdb.org/pdf/ICDE2024-kuiper-boncz-muehleisen-out-of-core.pdf) · [DuckDB Join Reordering Internals](https://www.alibabacloud.com/blog/duckdb-internals---part-7-join-reordering-optimization_602899) · [How LMDB works](https://xgwang.me/posts/how-lmdb-works/) · [redb design](https://github.com/cberner/redb/blob/master/docs/design.md) · [RocksDB WAL Performance](https://github.com/facebook/rocksdb/wiki/WAL-Performance) · [FlushFileBuffers (Microsoft Learn)](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers) · [rust-lang/rust#81357 FileExt soundness on Windows](https://github.com/rust-lang/rust/issues/81357) · [std::os::windows::fs::FileExt](https://doc.rust-lang.org/std/os/windows/fs/trait.FileExt.html) · [positioned-io2](https://github.com/surban/positioned-io2) · [Sublime HQ — Use mmap with care](https://www.sublimetext.com/blog/articles/use-mmap-with-care)
