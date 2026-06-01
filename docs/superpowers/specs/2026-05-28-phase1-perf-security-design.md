# Phase 1 — Performance + Security Hardening

**Status:** design draft (2026-05-28)
**Driver:** Product review findings + deployment readiness (AWS / Cloudflare / Railway)
**Blocks:** Phase 2 (deployment + DX), Phase 3 (engine upgrades)

## Guiding principle: verify, don't assert

Every workstream below has a **measurable goal** and a **verification command**. A
workstream is not "done" until the verification command has been run and its output
confirms the goal. No claims of improvement without a before/after number captured
from an actual run. (This is a hard requirement — a prior session reported benchmark
results it never ran.)

## Correction to the product review

The review claimed "statement-level transactions only — the undo log is not wired
in." **This is wrong.** PowDB has `BEGIN`/`COMMIT`/`ROLLBACK` with the undo log wired
through the executor; `crates/query/src/executor/tests.rs:3713+` verifies rollback
undoes inserts, updates, and deletes, and all 582 tests pass. Multi-statement ACID
already works. No action needed there. The real concurrency limitation is the
single-writer `Arc<RwLock<Engine>>`, which Phase 1 optimizes around rather than
replacing (true multi-writer MVCC is deferred).

---

## WS1 — Insert batch performance

### Problem
`insert_batch_1k`: PowDB 2.7μs vs SQLite 331ns (~10x slower). Root-cause analysis
(2026-05-28) found three contributors:

1. **Production bug:** `HeapFile::insert` calls `disable_mmap()` → `libc::munmap`
   on *every* insert (`crates/storage/src/heap.rs:346`), even on the hot-page fast
   path where no new page is allocated. A syscall per row. The munmap is only needed
   when the file actually grows (new page allocation at `heap.rs:391`).
2. **Unfair benchmark:** the loop runs full `execute_powql` per row (re-lex,
   canonicalize, plan-cache mutex lock, plan clone) while SQLite reuses a compiled
   prepared statement. The `InsertFast` prepared path (`prepared.rs:309-335`) exists
   for exactly this but the benchmark never calls it.
3. **Benchmark artifact:** `format!()` builds a new query string inside the timed
   loop (`powql.rs:516`).

### Design
- **Fix the munmap-per-insert bug:** in `HeapFile::insert`, only `disable_mmap()`
  when a new page is allocated, not on the hot-page write path. This is a real
  production win (affects every insert, not just the benchmark).
- **Add a batch-insert API:** `Engine::insert_batch(table, rows)` that dispatches
  once and shares one group-commit window, mirroring SQLite's transaction-wrapped
  batch. Surfaced via a PowQL multi-row insert or an explicit batch entry point.
- **Fix the benchmark to be fair:** use the prepared/`InsertFast` path and
  pre-generate query strings (match the `gen_queries` pattern other write benches
  use at `powql.rs:166`).

### Success criteria
- `insert_batch_1k` per-row cost improves measurably after the munmap fix — capture
  before/after from `cargo run --release -p powdb-compare`.
- `insert_single` also improves, since it goes through the same `HeapFile::insert` path.
- No regression on any other workload (bench regression gate stays green).
- All 582 tests still pass.

### Verification
```bash
# Before: capture baseline
cargo run --release -p powdb-compare 2>&1 | tee /tmp/ws1-before.txt
# After implementing the fix:
cargo run --release -p powdb-compare 2>&1 | tee /tmp/ws1-after.txt
# Confirm insert_batch_1k and insert_single improved, nothing regressed:
cargo bench -p powdb-bench && cargo run -p powdb-bench --bin compare
cargo test --workspace
```
Goal is reached when insert_batch_1k and insert_single are faster in after vs before
and the regression gate passes.

---

## WS2 — Per-query memory limits

### Problem
No memory budget per query. `MAX_SORT_ROWS = 10M` (`executor/mod.rs:45`) and
`MAX_JOIN_ROWS = 1M` (`mod.rs:40`) are blunt row-count caps; GROUP BY and IN-list
materialization have no cap at all. A crafted query can OOM the server — unacceptable
for AWS/Railway/Cloudflare where the process has a hard memory ceiling and gets
OOM-killed.

### Design
- Add `POWDB_QUERY_MEMORY_LIMIT` env var, default 256 MB, plumbed through server
  config and `Engine` construction.
- Replace blunt row-count caps with a per-query **byte budget accumulator**: each
  materialization point (sort buffer, join build side, GROUP BY hash table, IN-list
  Vec) adds its estimated byte size and checks against the budget.
- New `QueryError::MemoryLimitExceeded { limit_bytes, requested_bytes }` returned
  cleanly (no panic, no partial state). Disk-spill is explicitly **deferred to
  Phase 3**.
- Keep the existing row-count caps as a cheap secondary guard.

### Success criteria
- A query that would materialize >256 MB returns `MemoryLimitExceeded`, not an OOM
  kill or panic.
- The limit is configurable and the default is 256 MB.
- Normal queries under budget are unaffected (no measurable perf regression on the
  read benchmarks).

### Verification
```bash
# New tests must exist and pass:
cargo test -p powdb-query -- memory_limit
# Manual: a large sort/group on a big fixture returns the error, process survives.
# Perf: confirm read workloads unchanged
cargo run --release -p powdb-compare 2>&1 | tee /tmp/ws2-after.txt
```
Goal reached when an over-budget query errors cleanly (verified by a test that
asserts the error variant) and read-path benchmarks show no regression.

---

## WS3 — Heap page checksums

### Problem
WAL records have CRC32 integrity (`wal.rs`), but heap data pages do not. A bitflip
during write-back or on disk corrupts a page silently — a database killer.

### Design
- Add a CRC32 field to the page header (reuse `crc32fast`, already a dependency).
- Compute on write-back (in the dirty-page flush path), verify on read.
- **Backward compatibility:** a format-version flag in page-zero. Existing data
  files written without checksums must still open (validate-if-present, or a
  one-time upgrade on first write). New files always checksum.
- On checksum mismatch, return `StorageError::PageCorrupt` (the variant already
  exists per the security audit) rather than returning garbage rows.

### Success criteria
- A corrupted page (flipped byte) is detected on read and surfaces `PageCorrupt`.
- Existing pre-checksum data files still open and read correctly.
- Write throughput regression from the CRC computation is < 5% on insert workloads
  (CRC32 via crc32fast SIMD is cheap; measure to confirm).

### Verification
```bash
# New corruption test (flip a byte in a page, assert PageCorrupt on read):
cargo test -p powdb-storage -- page_checksum
# Compatibility test: open a file written before checksums, assert it reads.
cargo test -p powdb-storage -- checksum_backward_comp
# Perf delta:
cargo bench -p powdb-bench && cargo run -p powdb-bench --bin compare
```
Goal reached when corruption is detected (test passes), old files still open (test
passes), and the insert-path regression is < 5% (gate green).

---

## WS4 — Security hardening

### Problem (from security audit, 2026-05-27)
- **mmap/write race** (`heap.rs:282-346`): concurrent reads via mmap and writes can
  see inconsistent pages between `enable_mmap()`/`disable_mmap()` boundaries.
- **Password in memory:** `expected_password: Option<String>` (`handler.rs:133`)
  not zeroized on drop — leaks via core dump.
- **TLS optional with password:** the server warns but doesn't enforce TLS when a
  password is set, so credentials can transit in cleartext.

### Design
- **Close the mmap race:** invalidate/disable mmap on the write path itself, not
  just at explicit enable/disable boundaries, so a reader can never observe a torn
  page mid-write. (WS1's munmap change must be coordinated with this — same file.)
- **Zeroize password:** wrap the password in `zeroize::Zeroizing<String>` so it's
  wiped on drop.
- **TLS enforcement:** add `POWDB_REQUIRE_TLS` (default off for backward compat);
  when set, startup hard-fails if a password is configured without TLS cert/key.
  Document it in the production checklist.

### Success criteria
- No torn-page read possible under concurrent read+write (verified by a stress test
  that hammers reads while writing, asserting no `PageCorrupt`/garbage).
- Password memory is zeroized on drop (verified structurally — type is `Zeroizing`).
- With `POWDB_REQUIRE_TLS=1` and a password but no TLS, the server refuses to start.

### Verification
```bash
cargo test -p powdb-storage -- concurrent_mmap_write   # stress test, no torn reads
cargo test -p powdb-server -- require_tls              # startup refusal test
cargo test --workspace                                 # nothing else breaks
```
Goal reached when the concurrency stress test passes repeatably (run it 3x) and the
TLS-enforcement startup test passes.

---

## WS5 — Unwrap reduction

### Problem
~427 non-test `unwrap()` calls. Many are provably infallible, but those on fallible
paths (parsing, WAL replay, catalog load, wire-protocol decode) can panic a
production server on malformed input or a corrupt file.

### Design
- Audit the ~427 calls. Categorize:
  - **Fallible/external input** (parser, lexer, wire protocol, WAL/catalog load,
    file I/O): convert to `Result` propagation with a typed error.
  - **Provably infallible** (e.g., `HashMap.get()` right after `insert`, regex on a
    static string): replace with `expect("invariant: <why>")` documenting the
    invariant, or leave if already clear.
- Prioritize the server/storage open + query parse paths (reachable from untrusted
  input) over CLI/bench/test code.

### Success criteria
- Zero `unwrap()` on the wire-protocol decode path and the database-open path.
- A malformed query / corrupt file produces a `QueryError`/`StorageError`, never a
  panic (verified by tests feeding garbage).
- All 582 tests still pass; clippy clean.

### Verification
```bash
# Count before/after on production code:
grep -rn 'unwrap()' crates/{storage,query,server}/src --include='*.rs' | grep -v test | wc -l
# Fuzz/garbage tests for parse + open paths:
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
# The existing fuzz targets must still find no crashes:
cd crates/query && cargo +nightly fuzz run fuzz_parser -- -max_total_time=60
```
Goal reached when the production-path unwrap count drops materially, garbage-input
tests assert errors (not panics), and fuzz runs clean.

---

## WS6 — Postgres comparison benchmarks

### Problem
The `compare` crate has `engines/postgres.rs` but Postgres isn't part of the default
comparison run, so we only benchmark against SQLite. Deployment targets compare PowDB
to Postgres, so we need apples-to-apples numbers.

### Design
- Wire the Postgres engine into the default `powdb-compare` run, with graceful skip
  when no server is reachable (the harness already has a `[skipped]` pattern).
- Add a `docker-compose.yml` (or compose snippet) under `crates/compare/` or
  `examples/` to spin up a local Postgres for the comparison.
- Document the env (`POWDB_BENCH_PG_URL`) and the one-command local flow.

### Success criteria
- `cargo run --release -p powdb-compare` includes a Postgres column when a server is
  reachable, and cleanly skips (not errors) when not.
- A documented one-command way to spin up Postgres and run the comparison locally.

### Verification
```bash
# Without Postgres: clean skip, run still succeeds.
cargo run --release -p powdb-compare 2>&1 | grep -i postgres
# With Postgres (via the new compose):
docker compose -f <compose> up -d
POWDB_BENCH_PG_URL=postgres://... cargo run --release -p powdb-compare
```
Goal reached when the run shows a Postgres column with the server up and a clean skip
line with it down.

---

## Workstream coordination (worktrees / agents)

To avoid merge conflicts on shared files:

| Worktree | Workstreams | Shared files |
|---|---|---|
| A | WS1 + WS3 + WS4-mmap | `crates/storage/src/heap.rs`, `page.rs` (all touch heap/page) |
| B | WS2 | `crates/query/src/executor/*` |
| C | WS4-server (zeroize, TLS) | `crates/server/src/{handler,main}.rs` |
| D | WS5 | broad but mostly additive (error plumbing) — land last to rebase onto A/B/C |
| E | WS6 | `crates/compare/*` (isolated) |

Each worktree lands as its own PR to `main` (no direct pushes). Every PR must pass
the existing CI (clippy + fmt + test + miri + asan) and the bench regression gate.
WS5 (D) rebases last because it touches error types across crates that A/B/C modify.

## Out of scope for Phase 1 (Phase 2 / 3)
- Deployment example files, local-dev bootstrap, README reframe, PowDB-vs-SQLite
  guide → **Phase 2**.
- Windows `pread`/`pwrite`, disk-spill external sort, cost-based optimizer with
  catalog access, true multi-writer MVCC → **Phase 3**.
