# Changelog

All notable changes to PowDB will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Full snapshot backup and restore (`powdb-cli backup` / `powdb-cli restore`), blake3-verified, crash-consistent via checkpoint. New `powdb-backup` crate.

## [0.4.5] - 2026-06-05

### Added
- **Multi-row INSERT.** `insert T { a := 1 }, { a := 2 }, { a := 3 }` inserts
  many rows in a single statement. This is the fastest durable way to bulk-load:
  one statement = N WAL appends + **one fsync** (vs one fsync per single-row
  autocommit statement), and over a network connection it's **one round trip**
  instead of N — the right tool for remote bulk writes. Semantics:
  - **All-or-nothing on validation** — if any row is missing a required field,
    names an unknown column, or has an uncoercible value, the whole statement
    fails and no rows are written (rows are validated before any insert).
  - The whole batch is charged against `POWDB_QUERY_MEMORY_LIMIT`, so an
    over-large batch errors cleanly instead of exhausting memory.
  - Single-row `insert T { … }` is unchanged (still hits the prepared
    byte-patch fast path). Covered by a new `crates/query/tests/multi_row_insert.rs`
    suite (correctness, atomicity, transaction batching, plan-cache literal
    substitution across rows, 1000-row batches, memory budget, crash recovery).

## [0.4.4] - 2026-06-04

Critical durability release. Three distinct data-loss bugs found by a full
production test pass (server mode, ~40K rows, driven as a new user) — all
reproduced by tests that fail on 0.4.3 and pass here. **v0.4.1, v0.4.2, and
v0.4.3 are all yanked. Upgrade to 0.4.4.**

### Fixed — data loss (P0)

- **Writes issued after a restart/crash-recovery were silently lost on the next crash.** After a clean shutdown (or recovery) the WAL is truncated, but `Wal::open` reset the LSN counter to `1` while heap pages kept the high LSNs stamped during the previous session. New writes reused already-persisted LSNs, so the next crash's replay skipped them as "already applied."

  **Fix:** `Catalog::open` now restores `next_lsn` to `max(page LSN across all tables) + 1` on every open (new `Wal::set_next_lsn_at_least`). LSNs are monotonic across restarts. Guard: `test_writes_after_crash_recovery_survive_second_crash`, `test_repeated_restart_write_crash_cycles`.

- **Crash after a partial page flush duplicated rows and orphaned deletes/updates.** Two compounding causes: (1) `Insert` WAL records carried a placeholder RowId, so replay re-`insert`ed at a fresh slot, duplicating already-persisted rows; (2) the idempotency check used a single per-table max LSN, which could wrongly skip a low-LSN record stranded on an unflushed page because a *different* flushed page advertised a higher LSN. Compounding both, `DiskManager::allocate_page` zero-extends the file, and a zero page reads with a malformed header (`free_start = 0`), so replay packed re-inserted rows differently and later Update/Delete records (carrying the original RowIds) missed.

  **Fix:** ARIES-style per-page redo. `Insert` now logs its real RowId (heap insert first, then log) and replay places the row at that exact RowId via the new `HeapFile::insert_at` / `Page::insert_at_slot`, so every later record stays correctly targeted regardless of how pages were flushed. Replay skips a record iff its *target page's* LSN ≥ the record's LSN. Newly grown pages are initialised as valid empty pages, never zero pages. Guard: `test_mixed_mutations_survive_crash`, `test_cross_table_mutations_with_ddl_survive_crash`.

- **Prepared-statement inserts bypassed the WAL entirely.** The `execute_prepared` insert fast path called the raw `Table::insert` (heap only) and never logged a WAL record, so every prepared insert was lost on a crash.

  **Fix:** the fast path now routes through the new WAL-logging `Catalog::insert_by_slot` (keeps the O(1) prepared-time slot resolution). Guard: `test_prepared_insert_survives_crash`.

### Fixed — correctness

- **`count(T filter .x in (<subquery>))` silently returned 0.** The `count` fast path evaluated the predicate against raw row bytes without materialising the subquery. Both the read (`execute_plan_readonly`) and write (`execute_plan`) count fast paths now fall through to the generic path when the predicate contains a subquery. Guard: `test_count_with_in_subquery`.
- **Materialized-view auto-refresh did not fire for `count(View)` or `View filter …`,** so those access shapes returned stale data after an underlying mutation — contradicting the documented "no stale reads." Both shapes now refresh a dirty view first. Guard: `test_materialized_view_autorefresh`.
- **`cast(expr as type)` documentation didn't match the parser.** The reference showed `cast(.age as str)`, but the parser accepts `cast(.age, "str")`. Docs corrected to the implemented syntax.

### Added
- `crates/query/tests/durability.rs` — a permanent durability contract suite covering restart-then-crash, repeated crash-loops, mixed mutations, cross-table DDL, and prepared inserts. Every test fails on 0.4.3 and passes here. This is the gate the prior three releases lacked.

### Process note
v0.4.1–v0.4.3 all shipped data-loss bugs despite green unit/integration/clippy/miri/ASan/audit/criterion gates, because none of those gates exercised crash recovery with real mutation mixes and restarts. The new durability suite runs in CI and is the standing guard against this class of regression.

## [0.4.3] - 2026-06-02

P0 data-loss fix. Both v0.4.1 and v0.4.2 are affected; upgrade immediately and
consider yanking those versions if you published artifacts that depend on them.

### Fixed
- **`alter add column` + `update` + `alter add index` corrupted the heap on crash recovery.** WAL replay re-applied the pre-DDL `Insert` records to a heap whose rows had already been rewritten into the post-DDL layout, producing a mixed-layout heap that panicked on the next projection with `range start index N out of range for slice of length M` in `powdb-storage/src/row.rs`. Reproducible by following only the README's documented PowQL flow.

  **Root cause:** the alter paths rewrote every row through `rewrite_rows_for_schema_change` but never bumped the heap pages' LSNs, so the LSN-based idempotency check in `Catalog::replay_wal` saw `max_page_lsn == 0` and re-injected every pre-DDL row.

  **Fix:** `alter_table_add_column` and `alter_table_drop_column` now stamp every heap page with the DDL record's LSN after the rewrite (via the new `HeapFile::stamp_all_pages_min_lsn` + `Wal::last_appended_lsn`), and the WAL replay handlers for `DdlAddColumn` / `DdlDropColumn` do the same. Replay now correctly recognises every pre-DDL row record as already-applied and skips it. Verified via the new crash-recovery integration test `test_alter_add_column_then_index_survives_crash` in `crates/query/tests/wal_recovery_executor.rs`, which fails on 0.4.2 with the exact panic from the bug report and passes after the fix.

### Process note
This bug shipped because the v0.4.1 / v0.4.2 release gates were self-referential — full unit/integration/clippy/miri/ASan/cargo-audit/criterion/examples-smoke were green, but no release gate ran the README's own documented usage flow against the published binary with a real server restart in between. The post-release smoke test caught it on the first restart. The release process will add a "README-flow smoke against the installed crates.io binary, including a kill -9 + restart" step before the next publish.

## [0.4.2] - 2026-06-01

Documentation + example pinning patch surfaced by the v0.4.1 post-release audit. No engine changes.

### Fixed
- **Install instructions pinned to 0.4.0 instead of the current release.** README's production checklist and the PowDB-vs-SQLite guide both contained `cargo install … --version 0.4.0 --locked`, sending new users to the old version. Now points at 0.4.2.
- **Deployment examples defaulted to `ghcr.io/zvndev/powdb:latest`.** The AWS ECS Fargate Terraform module (`aws-ecs/variables.tf`) and the Cloudflare Tunnel docker-compose now pin to `v0.4.2` for example reproducibility.

### Known issues
- `@zvndev/powdb-client` npm package is stuck at 0.3.3 — the publish workflow has no npm step. Tracking in #68.

## [0.4.1] - 2026-06-01

Phase 1 (perf + security hardening) + Phase 2 (deployment + DX), shipped together.

### Added
- **CRC32 heap page checksums** with backward-compatibility — pages stamped on flush, verified on cold reads. New `HeapFile::verify_integrity()` for on-demand full-file scrub. Zero per-read overhead on the mmap fast path (documented trade-off — the on-demand scrub is the honest guarantee against silent disk bit-rot).
- **Per-query memory budget** — `POWDB_QUERY_MEMORY_LIMIT` (default 256 MiB) bounds materialization for sort, join, GROUP BY, and IN-list. New `QueryError::MemoryLimitExceeded` returned cleanly (no panic). RAII reentrancy guard so nested `execute_powql` (view refresh) cannot reset the outer budget.
- **`POWDB_REQUIRE_TLS`** — startup gate that refuses to bind when a password is set without TLS configured. Off by default for backward compat.
- **Postgres comparison benchmark** — `powdb-compare` now includes Postgres with a graceful skip when unreachable, plus `crates/compare/docker-compose.yml` for a one-command local PG.
- **`scripts/dev.sh`** — one-command local boot (`up | repl | bench | down`) with free-port discovery, tmp data dir, and `rm -rf` guard against non-tmp paths.
- **Deployment examples** — AWS ECS Fargate + EFS Terraform module, Cloudflare Tunnel docker-compose, Railway `railway.toml`, plus a refreshed Fly.io example with the new env vars and properly sized memory budget vs concurrency.
- **CI `examples-smoke` job** — Terraform validate + docker compose config + `dev.sh up/down` lifecycle on every PR.
- **`docs/powdb-vs-sqlite.md`** — honest when-to-use-which guide with side-by-side feature and benchmark tables.
- **Phase 3 risky-research dossier** — `docs/superpowers/specs/2026-05-30-phase3-risky-research-plan.md` with per-subsystem go/no-go verdicts (Windows file I/O port, disk-spill external sort, cost-based optimizer plumbing — multi-writer MVCC explicitly no-go).

### Fixed
- **`BufferPool::ensure_loaded` panicked on a corrupt page** and skipped CRC verification — now uses `from_bytes_verified` and returns `PageCorrupt`. The four `BufferPool` write paths now stamp CRCs.
- **Postgres `SUM(BIGINT)` cast bug** — `SUM` returns `NUMERIC` in PostgreSQL; the comparison engine now casts to `::bigint` to match the deserialization target.
- **Insert benchmark stability** — the `insert_single`/`insert_batch_1k` workloads now use the prepared/`InsertFast` path with literals pre-built outside the timed loop. Eliminates a ~13× run-to-run swing that previously made the insert workloads unmeasurable.
- **Cloudflared example placeholder** — added an inline `# REPLACE THIS` comment so deployers cannot silently ship a config that 404s on the wrong hostname.
- **Fly.io memory contradiction** — the example previously set `POWDB_QUERY_MEMORY_LIMIT=256 MiB` on a 256 MiB VM with `hard_limit=200` connections, the exact OOM scenario the README warns against. Limit dropped to 64 MiB, hard_limit to 32, and the comment now shows the per-query × concurrency math.

### Security
- **Password zeroization** — both the configured `POWDB_PASSWORD` and the client-supplied candidate from the wire are wrapped in `zeroize::Zeroizing<String>` so they are wiped on drop.
- **Production-path `unwrap()` driven to zero** on the wire-protocol decode path, DB-open path (WAL/catalog/page), and view-file path. Garbage inputs now return errors, never panic. Fuzz: 3.2 M iterations clean.
- **Reduced server panic surface** by converting 23 production-code `unwrap()`s on fallible paths to typed `Result`/`PageCorrupt` errors. Provably-infallible call sites converted to `.expect("invariant: …")` with documented invariants.

### Changed
- **README reframed** to lead with the compiled-predicate engine architecture (pure-Rust embedded DB, compiled byte-level predicates + zero-copy mmap + plan cache). PowQL is now positioned as the DX front door rather than the headline thesis. Production checklist now documents per-query (not global) semantics of `POWDB_QUERY_MEMORY_LIMIT`.
- **Test count** grew from **582 to 612** (+30 tests across all changes). All gates remained green throughout: `cargo test --workspace`, clippy, fmt, miri, AddressSanitizer, cargo audit, criterion regression gate, and the new examples-smoke job.

## [0.4.0] - 2026-05-26

### Added
- **Explicit transactions** — `BEGIN`, `COMMIT`, and `ROLLBACK` statements for grouping multiple operations into atomic units. Uncommitted changes are discarded on `ROLLBACK` or connection close
- **Benchmark tuning** — promoted `powql_filter_only` and `projection` workloads to NOISY (10%) threshold to reduce false-positive regressions

### Fixed
- Critical bugs and review findings from the 0.4.0 audit (executor, storage edge cases)
- Upgraded `rcgen` 0.13 to 0.14 (drops transitive `ring` dependency)

### Changed
- Bumped all crate versions to 0.4.0
- Rebaselined benchmarks after hardening; relaxed `insert_single` ratio threshold (16 to 300)
- Documentation and packaging improvements from smoke-audit findings

## [0.3.1] - 2026-05-18

### Added
- **LSN-tagged WAL replay** — pages carry monotonic LSNs; replay skips already-applied records, eliminating data duplication on crash recovery
- **CRC32 checksums on catalog.bin** — the catalog file now has integrity checking, matching WAL and btree index files
- **StorageError enum** — typed error handling for the storage crate (replaces raw `io::Result`)
- **Bounds validation on page slots** — `iter_page_slots` returns `None` for corrupt entries instead of panicking
- **Bounds checks on unsafe executor macros** — guards against UB from corrupt row data in `agg_int_loop!`/`agg_float_loop!`
- **Plaintext password warning** — server logs a loud warning when password auth is enabled without TLS
- **Pre-auth payload limit** — CONNECT messages capped at 4KB (was 64MB), blocking pre-auth memory exhaustion
- **67 new tests** — B+tree edge cases (27), buffer pool (12), catalog corruption (10), WAL CRC rejection (14), TLS connections (4)
- **Landing page** — static docs site in `site/` for GitHub Pages with benchmarks, getting started, PowQL reference
- **Docker image CI** — release workflow now pushes to ghcr.io/zvndev/powdb on tag
- **Crates.io publish workflow** — `workflow_dispatch` for automated cargo publish in dependency order
- **Dependabot** — weekly checks for cargo, npm, and GitHub Actions dependencies
- **MSRV declared** — `rust-version = "1.75"` in workspace Cargo.toml
- **Crate-level `//!` docs** on query and server crates

### Fixed
- Flaky `connection_management` tests — replaced hand-rolled temp dirs with `tempfile::tempdir()`
- `// SAFETY:` comments on all unsafe blocks in storage and query crates

### Changed
- Updated SECURITY.md with TLS documentation, supported versions, auth mechanisms
- Updated benchmark numbers to latest run
- Added CHANGELOG entry for v0.3.0
- GitHub repo topics added for discoverability

## [0.3.0] - 2026-05-18

### Added

- **DDL WAL replay**: schema mutations (CREATE TABLE, DROP TABLE, ADD COLUMN,
  DROP COLUMN) are now logged to the WAL with record types 6-9. Crash recovery
  replays DDL operations idempotently — if the table/column already exists or
  was already dropped, replay skips gracefully. WAL records are flushed
  immediately before filesystem mutations for durability.
- **CRC32 checksums on B+ tree nodes**: every serialized B+ tree node now
  includes a CRC32 checksum (last 4 bytes), verified on load. Returns
  `io::Error(InvalidData)` on mismatch, protecting against silent corruption of
  index data on disk.
- `TypeId::from_u8()` convenience method on storage types.
- Doc-tests added across lexer, parser, executor, and storage modules.

### Fixed

- **Compiled predicates**: bounds-checked `CompiledLeaf::eval()` — no panics on
  corrupt row data.
- Bounds checks in sort+limit fast path and mmap heap scan slot directory reads.
- UTF-8 slicing bug found by fuzzer: `&s[..20]` replaced with
  `&s[..s.floor_char_boundary(20)]` in token display.
- Clippy `collapsible_if` lint in connection management tests.

### CI

- Added Miri job (scoped to non-mmap modules: btree, page, row, types, tx,
  view).
- Added AddressSanitizer job (hard gate, leak detection disabled for mmap
  regions).
- Fixed fuzz workflow `cargo-fuzz` install (removed `--locked` to avoid stale
  transitive deps).
- Updated required status check names in branch protection.

## [TS client 0.3.3] - 2026-05-10

### Fixed

- Report intentional protocol truncation errors for malformed short
  `ResultRows` and `ResultOk` frames instead of surfacing raw Node buffer
  range errors.

### Changed

- Pin local package-manager metadata to `pnpm@10.29.3` so audit and install
  tooling match the v9 lockfile used by the TypeScript client workspace.

## [TS client 0.3.1] - 2026-05-09

### Fixed

- Added Ping/Pong protocol support — client auto-replies Pong on server
  health check Ping frames instead of throwing "unknown message type"

## [0.2.0] - 2026-05-09

### Security

- Fixed path traversal vulnerability via table names
- Added TLS support for encrypted connections
- Added authentication rate limiting
- Added row size validation to prevent u16 overflow corruption
- Replaced unsafe UTF-8 decoding with safe alternative

### Added

- CLI meta-commands: `.tables`, `.schema`, `.timing`, `.help`, `.quit`
- Tab completion for PowQL keywords
- Persistent command history
- Query timing display
- Health check (Ping/Pong) protocol message
- Cross join and sort row count safety limits
- String literal size limit (16 MB)
- `--version` flag for CLI and server binaries
- `cargo install powdb-cli` / `cargo install powdb-server` install path

### Fixed

- Version strings now use Cargo.toml version instead of hardcoded "0.1.0"
- NULL values display as "NULL" instead of "{}"
- UUID values display in full standard format
- B-tree load validates file size to prevent OOM
- mmap slot access validates bounds

### Changed

- README repositioned around compiled predicates and PowQL ergonomics
- Server `--password` flag removed (use `POWDB_PASSWORD` env var)
- Human-readable error messages in parser
- PowQL docs updated with math, date/time, window functions, CAST, EXPLAIN, UPSERT

## [TS client 0.3.0] - 2026-04-16

TypeScript client (`@zvndev/powdb-client`) production-readiness release.
Server is unaffected — this is a client-only version bump. No breaking
changes to the 0.2.x API surface; all additions are additive.

### Added

- **Structured errors**: every error thrown by the client is now a
  `PowDBError` with a stable `.code` (`connect_failed`, `auth_failed`,
  `query_failed`, `aborted`, `size_exceeded`, `protocol_error`, `closed`,
  `timeout`, `type_coercion_failed`). Callers can branch on `.code` without
  string-matching messages. `isPowDBError(err)` narrows `unknown` for use
  in catch blocks.
- **Typed rows**: `client.queryTyped(query, schema, opts?)` coerces the
  server's string wire format into JS values using a caller-supplied
  schema (`int`, `float`, `bool`, `str`, `datetime`, `uuid`). Int values
  that exceed `Number.MAX_SAFE_INTEGER` are promoted to `bigint`;
  datetime microseconds are converted via `BigInt` division to avoid
  float precision loss at year-2262 boundaries. Bytes columns are
  intentionally unsupported until the wire protocol grows a binary type.
- **Polling watch**: `client.watch(query, { intervalMs, onRows, onError?,
  stopOnError? })` re-runs a query on an interval and delivers rows to a
  callback. Guards against pile-up when a query is slower than the
  interval; returns `{ stop() }` to cancel. Uses `handle.unref()` so the
  watcher doesn't keep the event loop alive on its own.
- **Observability hooks**: `Client` now extends `EventEmitter`. Emits
  `"query"` (with `{ query, durationMs, ok, kind?, error? }`) after every
  query and `"close"` (with `{ error }`) exactly once per socket. Designed
  to be dropped into OpenTelemetry, pino, or a Prometheus exporter.
- **Pool connect retries**: `Pool` now retries transient connect-phase
  failures (`connect_failed`, `timeout`) with exponential backoff. New
  options: `connectRetries` (default 3), `connectBackoffMs` (default
  100), `connectMaxBackoffMs` (default 2_000). Auth failures, size
  violations, and other non-transient errors are never retried — they'd
  just fail the same way.

### Changed

- All raw `Error` throws in the client have been replaced with
  `PowDBError`. Existing `instanceof Error` checks still work; new code
  should prefer `instanceof PowDBError` + `.code` branching.

## [TS client 0.2.0] - 2026-04-16

TypeScript client (`@zvndev/powdb-client`) hardening release. Server is
unaffected — this is a client-only version bump.

### Added

- **Safe query composition**: new `powql` tagged template plus `escapeLiteral`,
  `escapeIdent`, and `ident()` helpers. The tagged template escapes literals
  and validates identifiers, neutralising PowQL-injection attacks (the same
  class of issue as SQL injection).
- **Connection pool**: new `Pool` class with `acquire`/`release`/`withClient`,
  FIFO waiter queue, and `acquireTimeoutMs`.
- **TLS support**: `tls` option on `Client.connect` accepts `true` or a
  `tls.ConnectionOptions` object.
- **Cancellation**: `client.query(q, { signal })` accepts an `AbortSignal`;
  cancelling does not tear down the socket — the reply is discarded so other
  in-flight queries keep working.
- **Frame size limits**: decoder enforces `MAX_PAYLOAD_SIZE` (64 MiB),
  `MAX_ROWS` (10M), and `MAX_COLUMNS` (4096) matching the server.
- **Version check**: on connect, warns once per `host:port` if the server's
  major version differs from the client's.
- **TCP keepalive**: `setKeepAlive(true, 30_000)` enabled on every connection
  so dead peers are detected on idle sockets.

### Fixed

- **O(n²) receive buffer**: the previous `Buffer.concat` on every chunk caused
  quadratic CPU on large result sets. Replaced with a lazy chunk queue that
  only coalesces when needed to decode a frame.

### Changed

- Minimum Node.js version is now explicitly `>=18` (was previously implicit).

## [0.1.2] - 2026-04-16

Hardening release: all known fuzz-reachable panics in the query layer are now
errors, and the CI gate has been tightened with cargo-audit and blocking fuzz
smoke runs.

### Fixed

- **Lexer**: integer literals wider than `i64::MAX` now return `LexError`
  instead of panicking (#25, closes #24)
- **Parser**: unterminated projection/assignment/argument/type-decl bodies at
  EOF (`nn{`, `z{`, etc.) now return `ParseError` instead of panicking via
  out-of-bounds indexing (#25, closes #26)
- **Executor**: `ORDER BY` on an unknown column now returns an error instead
  of panicking (#22)

### Security

- Bumped `rustls-webpki` 0.103.10 → 0.103.12 to pick up fixes for
  RUSTSEC-2026-0098 / RUSTSEC-2026-0099 (name-constraint bypass) (#25)

### CI

- New `cargo audit` job on every PR — blocks merges on known advisories (#23)
- New fuzz smoke workflow: `fuzz_lexer`, `fuzz_parser`, `fuzz_roundtrip` each
  run 60s on PRs that touch the query front-end, and nightly at 07:00 UTC.
  Blocking on failure (#23, #25)

## [0.1.1] - 2026-04-14

Post-launch polish: TS client test coverage, engine bug fixes surfaced by
end-to-end testing, and documentation sync.

### Added

- **TS client**: 53 end-to-end tests covering DDL, insert, filter, projection,
  aggregates, joins, GROUP BY/HAVING, subqueries, updates, deletes, and error
  paths (#18)
- **AGENTS.md**: user-facing primer with PowQL-vs-SQL cheat sheet, footgun
  table, and performance notes for AI assistants and new users (#20)

### Fixed

- **Parser**: `= null` and `!= null` now desugar to `IS NULL` / `IS NOT NULL`
  instead of being rejected (#19)
- **Executor**: `HAVING` on post-projection group queries now filters groups
  correctly (#19)
- **Parser**: statements with trailing tokens (e.g. `User match T on ...`,
  `User create_index .col`) now error cleanly instead of silently parsing as a
  bare-source query and dropping the rest (#19)
- **Executor**: DDL statements (`alter ... add index`, `alter ... add column`,
  `alter ... drop column`) now return an affected-count result instead of an
  empty row set (#19)

### Changed

- **Docs**: `README.md`, `docs/getting-started.md`, and `docs/POWQL.md` updated
  to use current syntax everywhere — `alter T add index .col` (not
  `create_index`), `alter T add column` (not `add_column`), `sum(T { .x })`
  (not `sum(T | .x)`), and `T1 as a inner join T2 as b on ...` (not `match`) (#20)

## [0.1.0] - 2026-04-12

Initial release of PowDB — a from-scratch database engine with PowQL query language.

### Added

- **Storage engine**: slotted 4KB pages, heap files, B+ tree indexes (disk-persisted), WAL with group commit, buffer pool with clock-sweep eviction, mmap-based scanning
- **Row format**: compact binary encoding with 1-byte type tags, variable-length strings, support for Int, Float, Str, Bool, DateTime, UUID, Bytes types
- **Query language (PowQL)**: lexer, recursive-descent parser, pure-function planner, executor with compiled predicates
  - Schema: `type T { required field: type, ... }`
  - Insert: `insert T { field := value, ... }`
  - Query pipeline: `T filter .field > val order .field desc limit N { .f1, .f2 }`
  - Aggregates: `count()`, `sum()`, `avg()`, `min()`, `max()` with GROUP BY / HAVING
  - Joins: INNER, LEFT OUTER, RIGHT OUTER (rewritten to LEFT), CROSS
  - Subqueries: IN, EXISTS, NOT IN, NOT EXISTS (correlated and uncorrelated)
  - Window functions: ROW_NUMBER, RANK, DENSE_RANK, SUM/AVG/MIN/MAX OVER
  - Set operations: UNION, UNION ALL
  - EXPLAIN for query plan inspection
  - CAST, CASE/WHEN, BETWEEN, LIKE, IS NULL/IS NOT NULL
  - Scalar functions: UPPER, LOWER, LENGTH, TRIM, SUBSTRING, CONCAT, ABS, ROUND, CEIL, FLOOR, SQRT, POW, NOW, EXTRACT, DATE_ADD, DATE_DIFF
  - UPSERT with ON CONFLICT
  - ALTER TABLE ADD/DROP COLUMN
  - Materialized views (CREATE VIEW, REFRESH VIEW, DROP VIEW)
- **Plan cache**: FNV-1a canonical hashing, literal substitution at lookup time
- **Executor fast paths**: compiled predicates for zero-decode filtering, fused scan+update, fused scan+delete, sort+limit, project+limit, aggregation fast paths
- **Plan lowering**: `RangeScan` → `Filter(SeqScan)` for unindexed columns at execution time
- **TCP server**: Tokio-based, binary wire protocol, password authentication, connection limits, graceful shutdown
- **CLI**: rustyline REPL with embedded and remote modes
- **TypeScript client**: `@zvndev/powdb-client` with full wire protocol support
- **Benchmarks**: criterion regression gate (20 workloads, per-workload thresholds), wide comparison suite vs SQLite/Postgres/MySQL
- **CI**: clippy + fmt + test workflow, criterion regression gate workflow
- **Performance**: 1.3x–10.8x faster than SQLite across all 15 comparison workloads at 100K rows

### Performance (100K rows, vs SQLite)

| Workload | Ratio |
|---|---|
| point_lookup_indexed | 3.8x faster |
| scan_filter_count | 6.7x faster |
| agg_min | 10.8x faster |
| agg_sum | 9.2x faster |
| update_by_filter | 3.2x faster |
| delete_by_filter | 1.3x faster |
