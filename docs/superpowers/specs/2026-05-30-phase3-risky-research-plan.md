# Phase 3 — Risky Engine Upgrades (research-first)

**Status:** RISKY / research in progress (2026-05-30)
**Posture:** Phase 3 touches load-bearing engine internals (planner, sort path, page format on Windows). Regressions here can erase PowDB's headline 3-10x read-perf wins. Do NOT plan or implement until this research document is filled and reviewed.

## Why Phase 3 is risky (not just Phase 2-with-different-content)

1. **Cost-based optimizer with catalog access** — currently the planner is a pure function. Adding catalog-driven cost estimation changes plan shapes; existing compiled-predicate fast paths pattern-match on specific shapes (`Filter(SeqScan)` etc., per CLAUDE.md). A new plan shape that the executor doesn't recognize silently drops to the slow generic path, regressing the 3-10x agg wins.
2. **Disk-based / external-merge sorting** — adds a new I/O path under the executor, interacts with WAL + page cache, must respect the memory budget shipped in Phase 1, and is the natural extension of `QueryError::MemoryLimitExceeded` from "error cleanly" to "spill cleanly."
3. **Windows pread/pwrite** — looks small (one TODO in `disk.rs`) but Windows file I/O semantics differ enough (overlapped I/O, sharing modes) to risk subtle durability bugs that the existing test suite won't catch on macOS/Linux CI.
4. *(Deferred from Phase 1: true multi-writer MVCC — even larger blast radius; only revisit if the single-writer ceiling becomes a real bottleneck.)*

## Required before scoping any Phase 3 work

A grounded research dossier (this document) covering, for each subsystem:
- How comparable engines solve it (SQLite, DuckDB, Postgres, RocksDB, LMDB, sled, redb) with citations and version specifics
- The constraints those engines design around vs PowDB's (slotted pages, B+tree, WAL, mmap, single-writer RwLock, pure planner)
- Concrete adaptation plans: what would the API/data-flow look like in PowDB specifically
- Quantified regression risks: which workloads in the existing 15-bench harness could regress, and which guard tests we need before any code change
- A go/no-go recommendation per subsystem with the cheapest viable starting point

## Status

- [x] Document skeleton created
- [ ] Deep research agent dispatched
- [ ] Research dossier filled (sections below)
- [ ] Reviewer pass on the research dossier
- [ ] Kirby go/no-go decision per subsystem

## Research dossier (to be filled by the deep-research agent)

### Subsystem A: Windows file I/O port
*(to be filled — comparative analysis of how SQLite/RocksDB/LMDB/redb handle Windows file I/O, recommendation for PowDB's `disk.rs`)*

### Subsystem B: Disk-spill external-merge sort
*(to be filled — comparative analysis: SQLite's TEMP files, DuckDB's spillable buffer manager, Postgres' tape sort. How to integrate with PowDB's WAL + memory budget. What goes into the spill format)*

### Subsystem C: Cost-based query optimizer with catalog access
*(to be filled — comparative analysis: SQLite's stat4, Postgres' planner, DuckDB's optimizer. How catalog access can be added without breaking PowDB's compiled-fast-path pattern matching. What stats are cheapest to maintain incrementally)*

### Subsystem D: True multi-writer concurrency (lowest priority)
*(to be filled — comparative analysis: SQLite WAL-mode concurrent readers, Postgres MVCC, LMDB single-writer model. Whether PowDB's existing undo-log MVCC scaffolding can be lit up without rewriting the executor)*

### Cross-cutting: regression safety
*(to be filled — which of the 15 existing benchmarks are most vulnerable to each Phase 3 subsystem, what guard tests + bench thresholds we need BEFORE any Phase 3 code change)*
