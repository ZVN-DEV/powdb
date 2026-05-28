# Phase 1 Perf + Security Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:test-driven-development for every code task. Steps use checkbox (`- [ ]`) syntax. Capture before/after evidence from REAL runs — never claim an improvement you didn't measure.

**Goal:** Land the 6 Phase 1 workstreams from the spec (insert_batch perf, memory limits, page checksums, security hardening, unwrap reduction, Postgres benchmarks), each as its own PR to `main`, each verified by an actual command run.

**Architecture:** 5 parallel worktrees (A–E) off `main`. Each worktree owns disjoint files where possible; WS5 (worktree D) rebases last. Every change is TDD: failing test → implement → passing test → commit. Every workstream ends with its spec verification command run and output captured.

**Tech Stack:** Rust workspace (powdb-storage, powdb-query, powdb-server, powdb-compare, powdb-bench), criterion, crc32fast, zeroize, tokio, libc mmap.

**Spec:** `docs/superpowers/specs/2026-05-28-phase1-perf-security-design.md`

**Universal definition of done (every worktree):**
- `cargo test --workspace` passes (582+ tests)
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- Bench regression gate green: `cargo bench -p powdb-bench && cargo run -p powdb-bench --bin compare`
- The workstream's specific verification command(s) run, output captured in the PR description
- PR opened to `main` (never push to main directly)

---

## Worktree A — WS1 (insert_batch perf) + WS3 (page checksums) + WS4-mmap (race)

All three touch `crates/storage/src/heap.rs` / `page.rs`, so they share one worktree to avoid conflicts. Do them in this order: WS1 munmap fix first (smallest, unblocks the perf number), then WS4-mmap (depends on understanding the same write path), then WS3 (page header change is the largest).

### Task A0: Baseline capture (before any change)

- [ ] **Step 1: Capture the current benchmark numbers**

Run: `cargo run --release -p powdb-compare 2>&1 | tee /tmp/A-baseline.txt`
Expected: a table with `insert_batch_1k` and `insert_single` rows. Record those two numbers — they are the before-values WS1 must beat.

- [ ] **Step 2: Confirm the test baseline is green**

Run: `cargo test -p powdb-storage`
Expected: all pass. This is the regression floor.

### Task A1: Fix munmap-per-insert (WS1)

**Files:** Modify `crates/storage/src/heap.rs` (the `insert` method, ~line 305–391; `disable_mmap` ~line 323–346).

- [ ] **Step 1: Read the current insert + mmap code.** Read `heap.rs` lines 230–400. Identify the hot-page fast path (page has room, ~line 350) vs the new-page allocation path (~line 391). Confirm `disable_mmap()` is currently called unconditionally at the top of `insert` (~line 346).

- [ ] **Step 2: Write a failing/observing test.** Add a test in `heap.rs`'s test module (or `crates/storage/tests/`) that inserts many rows into a heap with mmap enabled and asserts the rows are all readable afterward (correctness guard for the change). If a syscall-counting harness isn't feasible, this correctness test plus the benchmark delta is the evidence.

Run: `cargo test -p powdb-storage -- insert_with_mmap` → expect it to define the behavior.

- [ ] **Step 3: Implement.** Move `disable_mmap()` so it only fires when a new page is actually allocated (the file-growth path), not on the hot-page write path. The invariant to preserve: mmap must never cover a stale/short region after the file grows. Document the invariant in a comment.

- [ ] **Step 4: Run tests.** `cargo test -p powdb-storage` → all pass.

- [ ] **Step 5: Measure.** `cargo run --release -p powdb-compare 2>&1 | tee /tmp/A1-after.txt`. Compare `insert_batch_1k` and `insert_single` vs `/tmp/A-baseline.txt`. **Goal: both faster.** If not faster, STOP and diagnose — do not proceed claiming success.

- [ ] **Step 6: Commit.** `git add -A && git commit -m "perf(storage): only munmap on page growth, not every insert"`

### Task A2: Close the mmap/write race (WS4-mmap)

**Files:** Modify `crates/storage/src/heap.rs`.

- [ ] **Step 1: Read** the mmap enable/disable boundaries (~line 282–346) and every write entry point (insert/update/delete). Map where a concurrent reader holding an mmap pointer could observe a page mid-write.

- [ ] **Step 2: Write a failing stress test** in `crates/storage/tests/` that spawns reader threads scanning while a writer thread inserts/updates, asserting no torn/garbage rows and no `PageCorrupt`. Run it; expect it to expose the race (or at least exercise the path).

Run: `cargo test -p powdb-storage -- concurrent_mmap_write`

- [ ] **Step 3: Implement.** Invalidate/guard mmap on the write path so a reader can't observe a torn page. Coordinate with A1's change (same file). Preserve the A1 perf win — if the fix reintroduces a per-insert syscall, find a cheaper synchronization (e.g., only invalidate when the written page is within the mapped region, or gate mmap reads behind the engine's existing RwLock).

- [ ] **Step 4: Run the stress test 3×.** `for i in 1 2 3; do cargo test -p powdb-storage -- concurrent_mmap_write || break; done` → passes all 3.

- [ ] **Step 5: Re-measure perf.** `cargo run --release -p powdb-compare 2>&1 | tee /tmp/A2-after.txt` — confirm A1's insert gains survived.

- [ ] **Step 6: Commit.** `git commit -am "fix(storage): close mmap/write race window on the write path"`

### Task A3: Heap page checksums (WS3)

**Files:** Modify `crates/storage/src/page.rs` (header layout + read/write), `heap.rs` (flush path), possibly `disk.rs`. Read `page.rs` fully first.

- [ ] **Step 1: Read `page.rs`** to learn the exact current header layout (where the slot directory starts, what bytes are free in the header). Decide where a 4-byte CRC32 fits without breaking existing offsets, and how to version it (a format flag so pre-checksum files still open).

- [ ] **Step 2: Write the corruption-detection test (failing).** In `crates/storage/tests/`, write a page, flip a byte in the data region, read it back, assert `StorageError::PageCorrupt`.

Run: `cargo test -p powdb-storage -- page_checksum` → FAIL (no checksum yet).

- [ ] **Step 3: Write the backward-compat test (failing/observing).** Construct a page in the pre-checksum format (or open a fixture), assert it still reads without error.

Run: `cargo test -p powdb-storage -- checksum_backward_comp`

- [ ] **Step 4: Implement.** Add the CRC32 field + version flag. Compute on write-back, verify on read. Validate-if-present for old format. Use `crc32fast`.

- [ ] **Step 5: Run both tests + full storage suite.** `cargo test -p powdb-storage` → all pass.

- [ ] **Step 6: Measure write regression.** `cargo bench -p powdb-bench && cargo run -p powdb-bench --bin compare`. **Goal: insert-path regression < 5%.** If ≥ 5%, optimize (batch the CRC, only checksum on flush not per-row) before claiming done.

- [ ] **Step 7: Commit + open PR.** `git commit -am "feat(storage): CRC32 heap page checksums with backward-compat"`. Open PR to `main` with `/tmp/A-baseline.txt` vs after numbers and the < 5% regression evidence in the body.

---

## Worktree B — WS2 (per-query memory limits)

**Files:** `crates/query/src/executor/mod.rs` (caps at line 40/45), `executor/plan_exec.rs` (sort/join/group/IN-list materialization), `crates/query/src/result.rs` (error enum), `crates/server/src/main.rs` (env plumbing), `crates/query/src/executor/tests.rs`.

### Task B1: Add the error variant + budget type

- [ ] **Step 1: Read** `result.rs` (`QueryError`), `executor/mod.rs:37-53` (MAX_JOIN_ROWS/MAX_SORT_ROWS), and the materialization points in `plan_exec.rs` (sort buffer, join build, GROUP BY, IN-list).

- [ ] **Step 2: Write failing test.** In `executor/tests.rs`, build a query/fixture that materializes over a tiny configured limit (e.g., set limit to 1KB) and assert it returns `QueryError::MemoryLimitExceeded`.

Run: `cargo test -p powdb-query -- memory_limit` → FAIL.

- [ ] **Step 3: Implement** `QueryError::MemoryLimitExceeded { limit_bytes, requested_bytes }` and a lightweight per-query byte accumulator the executor threads through sort/join/group/IN-list. Default limit 256 MB; injectable for the test.

- [ ] **Step 4: Run test** → PASS. Then `cargo test -p powdb-query` → all pass.

- [ ] **Step 5: Commit.** `git commit -am "feat(query): per-query memory budget with MemoryLimitExceeded"`

### Task B2: Plumb POWDB_QUERY_MEMORY_LIMIT through the server

- [ ] **Step 1: Read** `crates/server/src/main.rs:23-50` (the env-var parsing block).

- [ ] **Step 2: Implement** reading `POWDB_QUERY_MEMORY_LIMIT` (default 256 MB), passing it into `Engine` construction.

- [ ] **Step 3: Test.** Add/extend a server test asserting the limit is applied from env. `cargo test -p powdb-server`.

- [ ] **Step 4: Measure read-path no-regression.** `cargo run --release -p powdb-compare 2>&1 | tee /tmp/B-after.txt` — confirm read workloads unchanged vs main.

- [ ] **Step 5: Commit + PR** to `main` with the read-path no-regression evidence.

---

## Worktree C — WS4-server (password zeroize + TLS enforcement)

**Files:** `crates/server/src/handler.rs` (password field ~line 133, `constant_time_eq`), `crates/server/src/main.rs` (startup, TLS args ~line 44-49, 244-253), `crates/server/Cargo.toml` (add `zeroize`).

### Task C1: Zeroize the password in memory

- [ ] **Step 1: Read** `handler.rs` around the `expected_password` field and `main.rs` where the password is read from env (~line 33).

- [ ] **Step 2: Add `zeroize` dep** to `crates/server/Cargo.toml`.

- [ ] **Step 3: Implement** wrapping the password in `zeroize::Zeroizing<String>` from the point of read through storage in `ConnOpts`. Adjust `constant_time_eq` call sites to borrow bytes.

- [ ] **Step 4: Test.** `cargo test -p powdb-server` → all pass (structural change; existing auth tests must still pass).

- [ ] **Step 5: Commit.** `git commit -am "security(server): zeroize password in memory on drop"`

### Task C2: POWDB_REQUIRE_TLS enforcement

- [ ] **Step 1: Write failing test.** In `crates/server/tests/`, assert that with `POWDB_REQUIRE_TLS=1` + a password set + no TLS cert/key, server startup returns an error / refuses to bind.

Run: `cargo test -p powdb-server -- require_tls` → FAIL.

- [ ] **Step 2: Implement** the `POWDB_REQUIRE_TLS` env (default off for back-compat). On startup, if set and password present without TLS cert+key, hard-fail with a clear message.

- [ ] **Step 3: Run test** → PASS. `cargo test --workspace`.

- [ ] **Step 4: Doc.** Update the README production checklist (note `POWDB_REQUIRE_TLS`). (Minimal doc line; full reframe is Phase 2.)

- [ ] **Step 5: Commit + PR** to `main`.

---

## Worktree D — WS5 (unwrap reduction) — REBASE LAST

Land after A, B, C merge (it touches error types they may change). Start by rebasing onto updated `main`.

**Files:** broad — prioritize `crates/server/src/protocol.rs` (wire decode), `crates/storage/src/{wal.rs,catalog.rs,disk.rs}` (open/replay), `crates/query/src/{lexer.rs,parser.rs}` (already mostly Result-based — verify).

### Task D1: Audit + categorize

- [ ] **Step 1: Count baseline.** `grep -rn 'unwrap()' crates/{storage,query,server}/src --include='*.rs' | grep -v test | wc -l` → record N.

- [ ] **Step 2: Categorize** each into (a) fallible/external-input path → convert to `Result`, or (b) provably infallible → `expect("invariant: …")`. Write the list into the PR description.

### Task D2: Convert the untrusted-input paths (TDD per path)

- [ ] **Step 1: Wire protocol.** Write a test feeding malformed bytes to `protocol::Message::read_from`, assert it returns an error (not panic). Run → FAIL if any unwrap panics. Convert those unwraps to `Result`. Run → PASS.

- [ ] **Step 2: DB-open / WAL replay.** Write a test opening a truncated/corrupt WAL or catalog file, assert `StorageError` (not panic). Convert. Run → PASS.

- [ ] **Step 3: Commit per path** (`security: remove panicking unwrap on wire decode`, etc.).

### Task D3: Verify

- [ ] **Step 1: Recount.** Same grep → confirm N dropped materially; **zero** unwraps remain on the protocol-decode and DB-open paths.
- [ ] **Step 2:** `cargo test --workspace` + `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] **Step 3: Fuzz.** `cd crates/query && cargo +nightly fuzz run fuzz_parser -- -max_total_time=60` → no crashes.
- [ ] **Step 4: PR** to `main` with before/after counts + the categorized list.

---

## Worktree E — WS6 (Postgres comparison benchmarks)

**Files:** `crates/compare/src/main.rs` (engine wiring, the `[skipped]` pattern), `crates/compare/src/engines/postgres.rs` (verify it's complete), new `crates/compare/docker-compose.yml`, `crates/compare/README.md` or `examples/`.

### Task E1: Wire Postgres into the default run

- [ ] **Step 1: Read** `compare/src/main.rs` (how SQLite and the optional MySQL engine are wired, the skip pattern) and `engines/postgres.rs` (is it a complete `BenchEngine` impl or a stub?).

- [ ] **Step 2: Implement** including the Postgres engine in the default run with a graceful skip when `POWDB_BENCH_PG_URL` is unset/unreachable (mirror the MySQL `[skipped]` handling). If `postgres.rs` is a stub, complete it against the `BenchEngine` trait.

- [ ] **Step 3: Verify skip path.** `cargo run --release -p powdb-compare 2>&1 | grep -i postgres` → shows a clean skip line (no error) when no PG server.

### Task E2: Local Postgres bring-up

- [ ] **Step 1: Add** `crates/compare/docker-compose.yml` spinning up a pinned Postgres with a known URL.

- [ ] **Step 2: Verify with-server path.** `docker compose -f crates/compare/docker-compose.yml up -d` then `POWDB_BENCH_PG_URL=postgres://… cargo run --release -p powdb-compare` → Postgres column appears with real numbers. Capture output.

- [ ] **Step 3: Doc** the one-command flow in `crates/compare/README.md`.

- [ ] **Step 4: Commit + PR** to `main` with both the skip-line and with-server outputs captured.

---

## Final integration (after all PRs merge)

- [ ] All 5 PRs merged to `main`.
- [ ] On `main`: `cargo test --workspace` + clippy + fmt + full bench regression gate green.
- [ ] `cargo run --release -p powdb-compare` captured — confirm insert_batch_1k/insert_single improved vs the original `/tmp/A-baseline.txt` and nothing regressed.
- [ ] Dispatch bug-hunter agent over the merged diff (per Kirby's request).
