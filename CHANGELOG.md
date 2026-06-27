# Changelog

All notable changes to PowDB will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **`insert`/`update`/`delete ... returning`** (write-performance Phase 3) — a
  write statement ending with `returning` now returns the affected rows (all
  columns) as a result set instead of a modified-count, so a client no longer
  needs a follow-up `SELECT` to read the rows back (removes the ORM reselect
  round-trip). `insert` returns the inserted rows, `update` the **post-update**
  image, `delete` the **pre-delete** image. Works for single- and multi-row
  writes and over the wire (the rows come back on the existing rows path).
  `returning` is opt-in: without it, every existing fast path (byte-patch,
  fused single-pass scan/update/delete) is unchanged.
- **`Normal` WAL durability mode** (write-performance Phase 1) — a third
  `WalSyncMode` between `Full` and `Off`. Commits are acknowledged once their
  WAL record reaches the OS page cache (no per-commit fsync); a background
  flusher fsyncs on a ~10 ms interval. A process crash loses nothing; an OS
  crash / power loss can lose only the unsynced tail (≤ one interval). This is
  SQLite `synchronous=NORMAL` / Postgres `synchronous_commit=off` semantics and
  removes the per-write fsync from the latency path (~15–40× faster single-row
  writes). Select it with `POWDB_SYNC_MODE=full|normal|off` (default `full` —
  no durability change unless opted in). Addresses the turbine-orm write-latency
  finding; see `docs/design/2026-06-27-write-performance-*`.

### Security
- **Fixed a remotely-triggerable denial-of-service in the in-place `UPDATE`
  fast path** (#117). Assigning a value whose type does not match a fixed-size
  column (e.g. a `str` into a `float` column) reached
  `unreachable!("all_fixed_nonnull guard lied")` and, under the deliberate
  `panic = "abort"` profile, took the whole server process down for every
  connection. The fast path now coerces each assignment to the column's
  declared type before writing bytes and returns a typed `TypeError` on a
  genuine mismatch. Reported by the turbine-orm integration.

### Fixed
- **`UPDATE` now coerces an integer assigned to a `float` column to `f64`**
  (#118) instead of writing the raw i64 bit pattern (which read back as a
  denormal such as `5e-323`) — silent numeric corruption on the in-place
  update fast path. `INSERT` already coerced correctly; the byte-patch
  `UPDATE` path bypassed it. Fix covers both the indexed and the fused
  `Filter(SeqScan)` update paths. Reported by the turbine-orm integration.

## [0.6.2] - 2026-06-26

### Security
- **Fixed three remotely-triggerable denial-of-service vectors** found in the
  2026-06-26 code review (all are process aborts/exhaustion under the deliberate
  `panic = "abort"` profile, so a connected — and in the first case
  unauthenticated — client could disrupt the server):
  - **Pre-auth memory amplification** (`crates/server/src/protocol.rs`): a
    ~12-byte `RESULT_ROWS` frame declaring up to 10M zero-column rows forced a
    ~240 MB allocation during the pre-auth read. The decoder now bounds the
    row/column/param preallocation by the actual remaining payload and rejects
    counts that cannot fit.
  - **Parser stack overflow** (`crates/query/src/parser.rs`): chained unary
    prefixes (`not not … .x`, `exists …`) recursed through `parse_primary`
    without the nesting-depth guard. The depth limit now covers that recursion.
  - **`LIKE` stack overflow / exponential backtracking**
    (`crates/query/src/executor/eval.rs`): `col like "…"` used a recursive
    matcher that overflowed on long inputs and backtracked exponentially on
    patterns like `%a%a%`. Replaced with an iterative two-pointer matcher
    (O(n·m) time, O(1) stack).

### Fixed
- **`avg()` over a column containing NULLs** returned the wrong value on the
  generic aggregate path: it divided by the total row count instead of the count
  of non-null values, and disagreed with the compiled fast path. Both paths now
  divide by the contributing count and return the empty value when there are
  none. (`crates/query/src/executor/{plan_exec,mod}.rs`)
- **Correlated and `IN`-subquery comparisons over `datetime`, `uuid`, `bytes`,
  and NULL values** silently coerced those values to `0`, producing wrong
  results. Such values are now carried verbatim through subquery substitution
  via a runtime-only AST node. (`crates/query/src/executor/eval.rs`)

### Documentation
- Reconciled the SQL story across `README.md`, `AGENTS.md`, and `CLAUDE.md`:
  these onboarding docs previously asserted "no SQL / no translation layer"
  even though the SQL frontend shipped in v0.5.0. They now describe SQL as a
  supported-subset frontend that lowers to the PowQL AST (PowQL remains the
  native path) and link `docs/SQL.md`. `CLAUDE.md` no longer instructs agents
  to refuse SQL work.
- Refreshed the `SECURITY.md` supported-versions table to the 0.6.x line.

### Internal
- CI now feeds every job (clippy/fmt/test, miri, asan, MSRV, examples-smoke,
  ts-client, secret-scan, audit) into a single `ci-success` aggregator job so
  branch protection can require one check and still gate on the whole matrix —
  closing the gap where safety jobs could fail without blocking a merge.
- `scripts/check-version-consistency.sh` now also asserts `SECURITY.md` lists
  the current minor series as supported, so the supported-versions table can't
  drift behind the shipping release.
- Regression tests for all five code-review fixes (parser depth, LIKE
  adversarial input, wire-decode amplification, avg-over-NULL generic path,
  correlated subquery over datetime/NULL).

## [0.6.1] - 2026-06-19

### Security
- `powdb-server` now applies `POWDB_REQUIRE_TLS=1` to every credentialed auth
  mode: shared-password auth, named-user auth, and first-run admin bootstrap
  auth. Operators can no longer accidentally enable named users or bootstrap
  admin credentials over plaintext TCP when TLS is required.
- Backup restore now rejects unsafe manifest file names before writing files:
  absolute paths, `..`, path separators, Windows drive/ADS-style colons,
  unexpected file names, and mismatched incremental delta targets are all
  refused. This closes the path-traversal restore class for full and
  incremental backup archives.
- Remote row responses are now capped before wire serialization. Oversized
  materialized result sets return an actionable `result too large` error
  instead of letting one query allocate unbounded response memory.

### Fixed
- Query timeout handling no longer claims a blocking query was aborted while
  the underlying work can still continue in the blocking executor. The timeout
  counter now represents threshold breaches, and the handler waits for the work
  to finish before replying so timed-out writes cannot keep mutating detached
  from the connection lifecycle.
- Deployment examples and release docs no longer pin stale container/image
  versions in active examples; they now point at the current `v0.6.1` release
  line.

### Known limitations
- Full cooperative query cancellation is still planned work rather than part of
  this hardening patch. See
  `docs/strategy/2026-06-19-query-cancellation-hardening.md` for the scoped
  implementation plan.

## [0.6.0] - 2026-06-19

### Added
- `powdb-server` can serve a Prometheus `/metrics` endpoint on a separate, opt-in HTTP listener (`--metrics-addr` / `POWDB_METRICS_ADDR`, e.g. `127.0.0.1:9090`). Exposes the four golden signals plus security and build identity: `powdb_connections_active`, `powdb_connections_accepted_total`, `powdb_queries_total{result}`, `powdb_queries_in_flight`, `powdb_query_duration_seconds` (histogram), `powdb_query_timeouts_total`, `powdb_query_memory_limit_exceeded_total`, `powdb_auth_failures_total`, `powdb_tls_handshake_failures_total`, `powdb_build_info`, and `powdb_uptime_seconds`. Zero new dependencies; scrapes are lock-free (never touch the engine lock). The endpoint is unauthenticated — bind it to localhost or a private network.

### Fixed
- `powdb-server` now drains gracefully on **SIGTERM**, not just SIGINT (Ctrl-C). `docker stop`, Kubernetes pod termination, and systemd all send SIGTERM — previously the process was killed by the signal, skipping the connection drain and `catalog.checkpoint()`. Committed data was still recovered via WAL replay on restart, but in-flight queries were cut and the WAL was left un-checkpointed (slower recovery).

## [0.5.1] - 2026-06-17

### Changed
- Bumped `zeroize` 1.8.2 → 1.9.0, the dependency used to scrub secrets from memory in `powdb-auth` and `powdb-server`.

### Internal
- Pinned the fuzz workflow to an explicit `nightly` toolchain so cargo-fuzz builds survive `dtolnay/rust-toolchain` SHA bumps.
- Routine dev-dependency bumps (`postgres` test harness, `@types/node`) and a remaining-work backlog docs refresh. No runtime or wire-protocol changes.

## [0.5.0] - 2026-06-16

### Added
- **Connection-scoped explicit transactions over TCP.** `BEGIN` now owns the transaction on the connection that opened it; other connections wait behind the transaction gate until `COMMIT`, `ROLLBACK`, disconnect, or timeout, preventing cross-connection visibility into uncommitted state.
- **Self-identifying storage formats.** Heap files, pages, rows, and WAL files now carry magic/version metadata, reject unknown future versions, preserve legacy-read compatibility, and expose format introspection docs in `docs/FORMAT.md`.
- **SQL frontend (explicit dialect).** A production SQL subset lowers into the existing PowQL AST/plan path with `Engine::execute_sql`, read-only SQL execution, wire protocol `QuerySql` (`0x05`), TypeScript `querySql`, SQL/PowQL plan-cache parity, unsupported-feature errors, and `docs/SQL.md`.

### Changed
- **Bench baselines refreshed for the v0.5.0 foundation.** The release gate now compares against post-format-versioning/post-SQL measurements; the `scan_filter_count_over_btree_lookup` thesis ratio was relaxed only because `btree_lookup` improved materially while aggregate absolute performance remained inside its gate.
- Remote CLI default database naming now matches the TypeScript client default (`default`).

### Fixed
- WAL replay now honors transaction commit boundaries, so uncommitted records do not replay after crash recovery.
- Rollback now discards uncommitted WAL writer spillover before reopening/replay, including multi-page dirty inserts.
- Raw aggregate/order fast paths now account for the `PROW` row-format prefix before reading null bitmaps and fixed-width values. This restores integer/float aggregate and sort correctness after row versioning.
- Parser diagnostics now include clearer trailing-token positions and typo suggestions for statement-like keywords.
- The concurrent-read corruption regression remains a real indexed `heap.get`/`disk.read_page` race check but is bounded enough to complete in full workspace/release runs.
- The unused storage `BufferPool` module and tests were removed to eliminate dead API surface before 0.5.0.
- Unix/Windows disk-positioned I/O paths now use platform `FileExt` helpers instead of a Windows TODO.

### Verification
- Release gate passed locally before publishing: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo check --workspace`, full `cargo test --workspace -- --nocapture`, TypeScript client runtime tests, release-binary durability smoke, nightly fuzz target builds/short runs, and the bench comparator after rebaseline.

## [0.4.9] - 2026-06-15

### Security
- **Fixed two remotely-craftable denial-of-service crashes** surfaced by the
  2026-06-14 product review. Both are process aborts under the deliberate
  `panic = "abort"` release profile, so an unauthenticated-but-connected client
  could take the server down with a single query:
  - **Integer-division overflow** (`crates/query/src/executor/eval.rs`): `i64::MIN / -1`
    panics even in release. Division now uses `checked_div` and returns the
    empty set on overflow or divide-by-zero, matching the other arithmetic arms.
  - **Unbounded `LIMIT` pre-allocation** (`crates/query/src/executor/plan_exec.rs`):
    the sort+limit fast path reserved a top-N heap of the raw user `LIMIT` up
    front (e.g. `order .x limit 99999999999` → multi-terabyte reservation →
    allocator abort). The pre-allocation is now capped; the heap still grows on
    demand to the true limit.
- **Data directory is now created `0700` on Unix** (`powdb_storage::create_data_dir_secure`).
  Previously the directory — and the heap/WAL/index files holding all row data —
  were created under the default umask and were world/group readable. `auth.json`
  was already `0600`; this extends the same posture to the table data, matching
  PostgreSQL's owner-only data-directory model.

### Internal
- Regression tests for all three fixes in `crates/query/tests/safety_limits.rs`
  (the malicious division and huge-`LIMIT` queries, plus a `0700` permission
  assertion) so they cannot silently regress.

## [0.4.8] - 2026-06-10

### Added
- **RBAC now enforces the full permission lattice.** The server maps each
  statement to the capability it needs — reads → `Read`, row mutations
  (insert/update/delete/upsert) → `Write`, schema changes (create/alter/drop
  type or view) → `Ddl` — and checks it against the user's role. The
  `readwrite` role now explicitly carries `Ddl` (application users create and
  evolve their own tables), so **this is behavior-preserving**: readwrite and
  admin keep full access, readonly is still read-only, and any authenticated
  role may still run read-only queries. `Admin` remains reserved for user/role
  management (CLI-only).
- **Automated post-publish durability smoke** (`scripts/smoke-release.sh`),
  wired as a required gate in `release.yml`: installs the built binaries, runs
  the README PowQL flow over the wire, then `kill -9`s the server and restarts
  it to assert WAL replay recovers every row and the unique constraint still
  holds. This is the exact gate whose absence caused the v0.4.1–0.4.3 data-loss
  yanks; it now runs on every tagged release.
- **MSRV build job** in CI that compiles the workspace with the pinned 1.93
  toolchain (the previous job only checked that the version string matched the
  docs).

### Changed
- **Resource-limit errors now reach remote clients verbatim.** Sort, join, and
  per-query memory-budget errors (e.g. "sort input exceeds row limit — add a
  LIMIT clause") were being masked to the generic "query execution error" by
  the wire sanitizer. They carry actionable guidance and leak no internal
  state, so they are now on the safe-error allowlist.

### Fixed
- CLI `--help` showed a remote one-shot example using a `|` pipe operator that
  PowQL does not have; corrected to the whitespace-pipeline syntax so the
  example runs as written.
- CI `cargo audit` no longer fails on three `postgres`-only RUSTSEC advisories
  whose entire dependency path is confined to the `publish = false`
  `powdb-compare` benchmark crate (scoped ignore in `.cargo/audit.toml` + the
  audit action, with provenance comments). No shipping crate is affected.
- Dockerfile dependency-cache stage now copies the `powdb-auth` and
  `powdb-backup` manifests it was silently missing, so the cached layer covers
  the full server/CLI dependency closure.
- TypeScript client version drift: the `CLIENT_VERSION` handshake constant,
  the built `dist`, and the README now all agree with `package.json` (0.5.0),
  and a CI job asserts they can't diverge again.

### Internal
- Documented `panic = "abort"` as a deliberate **crash-only** design: on a
  panic the server exits fast and a supervisor restarts it, with WAL replay
  recovering to a consistent state — safer for a stateful engine than
  unwinding into a poisoned lock. Every deploy example is confirmed to run
  under an auto-restart policy, and the requirement is now documented in
  `examples/deploy/README.md`.
- Promoted the CI lint policy into `[workspace.lints]` (`clippy::all = deny`)
  so `cargo clippy` fails locally with the same rules CI enforces.
- Removed ~190 LOC of dead, never-wired snapshot-isolation scaffolding
  (`storage::mvcc`, `storage::tx`) that was shipping in the `powdb-storage`
  crate; the live engine uses `RwLock` concurrency.
- Refreshed stale `powdb-auth` doc-comments that claimed the crate was "not
  yet wired into the server or CLI" — it has enforced auth/RBAC since 0.4.6.

## [0.4.7] - 2026-06-10

### Added
- **Parameter binding over the wire (`$1`..`$N`).** Clients can send a query
  template plus positional values instead of interpolating untrusted input
  into the query string. Placeholders are 1-based `$N` (not `?` — `??` is the
  COALESCE operator). Binding happens at the **token level** on the server:
  each `$N` is replaced with the literal token for its value before parsing,
  so an injection-shaped string is inert data and can never change the query's
  shape. New wire message `QueryWithParams` (`0x04`) — a pure protocol
  addition; existing messages and pre-0.4.7 clients are unaffected. The
  TypeScript client gains `client.query(powql, params?)`. Engine API:
  `Engine::execute_powql_with_params` / `execute_powql_readonly_with_params`.
- **Unique constraints.** Declare a column unique with the `unique` field
  modifier (`type User { required unique email: str }`) or add one to an
  existing table with `alter User add unique .email` (which scans for existing
  duplicates first and fails if any exist). Declaring `unique` auto-creates a
  unique B+tree index; enforcement is a storage-layer pre-check shared by the
  plain, prepared, and upsert write paths, so duplicates are rejected with
  `unique constraint violation on <table>.<column>` before anything is written
  or WAL-logged. The constraint survives restart (persisted in the catalog +
  rebuilt on WAL replay).
- **Range scans use B+tree indexes.** `>`, `>=`, `<`, `<=`, and `between` on an
  indexed column now traverse the index (unique: raw keys; non-unique:
  composite `(value, rid)` keys) instead of always falling back to a full
  scan — roughly 7× faster on a selective range over 100K rows. NULLs are
  correctly excluded and exclusive bounds are honored.
- **`EXPLAIN` shows the executed plan.** Because the planner is pure (no
  catalog access), it emits speculative `IndexScan`/`RangeScan` nodes; the
  executor lowers them at runtime when no index exists. `EXPLAIN` now applies
  the same lowering before printing, so it shows `Filter(SeqScan)` for an
  unindexed column instead of a misleading `IndexScan`.
- **Multi-line REPL input.** The `powdb-cli` REPL buffers lines until braces
  and parentheses balance (outside string literals), so multi-line `type` and
  `insert` statements can be pasted or typed across lines.
- **Agent-DX evaluation harness** (`scripts/agent-eval/`). A model-agnostic,
  offline harness that scores how well an LLM writes PowQL given only
  `AGENTS.md` and a 10-table schema, with a parallel SQLite baseline for
  comparison. Not wired into CI.

### Changed
- **BREAKING:** `upsert <T> on .col` now requires `.col` to be a `unique`
  column. Declare it with the `unique` modifier or `alter <T> add unique .col`.
  This fixes a bug where `upsert ... on .id` followed by a plain
  `insert` of the same id silently produced duplicate rows.

### Fixed
- Lowering an unindexed equality update to `Filter(SeqScan)` exposed a fused
  scan-update path that swallowed `update_hinted` errors and still counted the
  row as modified — which bypassed the v0.4.6 oversized-row guard for that
  path. Errors now propagate as `StorageError`; all three `oversized_rows`
  tests pass.

## [0.4.6] - 2026-06-09

### Fixed
- **Oversized rows no longer kill the server (remote DoS).** Inserting or
  updating a row whose encoded size exceeds one 4 KB page (`MAX_ROW_DATA_SIZE`,
  4070 bytes) previously hit a `panic!` in the heap layer, which — combined with
  `panic = "abort"` — terminated the entire server process. Any connected client
  could take down the database with a single ~5 KB string insert. The heap now
  rejects oversized rows with a graceful `row too large: N bytes exceeds max M
  bytes` query error before anything is written or WAL-logged (an oversized
  update can no longer poison WAL replay), and the connection and server keep
  running. There is still no large-object/overflow-page support — values over
  ~4 KB are rejected, not stored.
- **`readonly` role is now enforced at the query layer.** Previously the role
  was authenticated and stored but never checked: a `readonly` user could
  insert, update, delete, and drop tables. The server now classifies each
  parsed statement and rejects writes (DML, DDL, view DDL, and transaction
  control) from `readonly` principals with `permission denied: role 'readonly'
  cannot execute write statements`. Unknown roles fail closed. Shared-password
  mode, open mode, and embedded use are unaffected.
- **NULL values now arrive as `null` on the wire instead of `{}`.** The server
  serialized SQL NULLs as `{}`, which the remote CLI displayed verbatim and
  which broke the TS client's documented `"null"` sentinel for typed-row
  decoding. The wire serialization is now the bareword `null`, and the remote
  REPL renders it as `NULL`, matching embedded mode.
- **Window aggregates without `order` now compute the whole-partition value.**
  `avg(.x) over (partition .d)` previously returned a running aggregate per
  row (frame = partition start → current row) even with no `order` clause; per
  standard semantics the frame is now the entire partition. Ordered windows
  keep the running-frame behavior; ranking functions are unchanged.

### Documentation
- Ecosystem-wide accuracy sweep: site pages synced to v0.4.5 (banners were
  v0.2.0, MSRV corrected to 1.93), crates.io homepage URL fixed (was a 404),
  README/CONTRIBUTING/AGENTS no longer claim the bench suite is a CI merge
  gate, SECURITY.md documents both auth modes and the ≤0.4.5 readonly caveat,
  RELEASES.md covers all six crates + the Docker image, deploy examples fixed
  (`fly.toml` was missing `POWDB_BIND=0.0.0.0`), TS client docs document the
  multi-user-server incompatibility, and AGENTS.md gained small-model-tested
  gotchas (reserved aggregate keywords as aliases; line-oriented REPL).

## [0.4.5] - 2026-06-09

### Added
- **Multi-user authentication (operator surface).** The server now authenticates
  per-connection `(username, password)` against a persisted user store
  (`auth.json`, argon2id hashes only) when any users are defined. New CLI flag
  `powdb-cli --user <NAME>` carries the username on remote connect, and new
  offline user-admin subcommands edit the data dir's user store without a running
  server: `useradd <NAME> [--role <ROLE>] [--password <PW>]` (role defaults to
  `readwrite`; password may come from `POWDB_NEW_PASSWORD`), `userdel <NAME>`,
  `passwd <NAME> [--password <PW>]`, and `users` (lists name + role, never
  hashes). The server can bootstrap an initial admin from `POWDB_ADMIN_USER` /
  `POWDB_ADMIN_PASSWORD` on startup when that user does not yet exist. Added
  `UserStore::set_password`. **Backward compatible:** with no users defined, the
  single shared-password model (`POWDB_PASSWORD` / `--password`) still applies.
  New `powdb-auth` crate.
- **Full snapshot backup & restore.** `powdb-cli backup <dest>` takes a
  crash-consistent, blake3-verified full snapshot (checkpoint-then-copy of
  `catalog.bin` + every heap + every index, plus an integrity `manifest.json`
  recording each file's hash/size and the page-LSN the snapshot is consistent
  at); `powdb-cli restore <backup> <dest>` re-verifies every file against the
  manifest before writing and rebuilds a fresh data dir, validating by
  reopening (which preserves the post-restore LSN invariant so writes made
  after a restore survive a later crash). Offline / single-writer in this
  release — do not back up a directory a live server has open. Incremental
  backup, point-in-time restore, and cloud sync are planned (see
  `docs/design/2026-06-05-backup-pitr-sync-migrations-plan.md`). New
  `powdb-backup` crate; guide at `docs/backup-and-restore.md`.
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
- **Incremental backup & chain / point-in-time restore.** `powdb-cli backup
  <dest> --base <full_dir>` writes a **differential** backup against a full
  base — only the 4 KB heap/index pages whose `page_lsn` is newer than the
  base's high-water mark are stored (in `<name>.delta` sidecars), with the
  catalog copied whole when it changed. `powdb-cli restore <full> <dest>
  --apply <inc>...` chain-restores a full base plus one or more increments in
  the order given, enabling **coarse point-in-time restore** (recover to the
  state captured by the increment you stop at). The chain is verified before it
  writes a usable database: **page-LSN continuity** (each increment's recorded
  base LSN must match the running LSN) plus **blake3** on every delta /
  whole-file copy, then a reopen-to-validate. Fine-grained (sub-increment) PITR
  via WAL archiving and cloud sync remain future work. See
  `docs/backup-and-restore.md`.

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
