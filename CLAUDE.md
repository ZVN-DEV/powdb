# PowDB: Claude Code Guide

## Quick Start

```bash
cargo build --workspace            # build everything
cargo test --workspace             # run all tests (tens of minutes: roughly 35-45 on an M-series laptop, longer cold)
cargo run --release -p powdb-compare  # benchmark vs SQLite (100K rows)
cargo bench -p powdb-bench         # criterion benchmarks (24 benches, 22 gated workloads; ~5 min of measurement,
                                   # plus a cold compile of the bench targets)
```

Watch mode: `bacon` (or `cargo watch -x "check --workspace"`) for a recheck-on-save loop.

## Architecture

PowDB is a from-scratch database engine. Its native query language is **PowQL**, a left-to-right pipeline syntax whose parser AST is already a plan tree, so there is no cost-based rewriting tier. Since v0.5.0 PowDB also ships a **SQL frontend**: a SQL parser that lowers a supported subset (`crates/query/src/sql.rs`) to the same PowQL AST and shares the plan cache. Both languages run on one planner/executor. The default wire `Query` message stays PowQL for backward compatibility; SQL goes through `Engine::execute_sql(...)` (embedded Rust) or the `QuerySql` wire path. See `docs/SQL.md` for the supported subset.

### Crate Dependency Graph

Eleven crates: eight published, three `publish = false` tooling crates.

```
Published (crates.io):
powdb-cli    ──→ powdb-server + powdb-query + powdb-storage + powdb-backup + powdb-auth + powdb-sync
powdb-server ──→ powdb-query + powdb-storage + powdb-auth + powdb-sync
powdb-query  ──→ powdb-storage
powdb-storage ─→ (no inter-crate deps)
powdb        ──→ powdb-storage + powdb-query + powdb-sync   (embedded facade crate)
powdb-sync   ──→ powdb-storage                              (retained-unit sync substrate)
powdb-auth   ←── powdb-server, powdb-cli   (user store + roles; no inter-crate deps)
powdb-backup ←── powdb-cli                 (depends on powdb-storage + powdb-sync; powdb-query is dev-only)

Not published (publish = false; nothing depends on them):
powdb-bench   ──→ powdb-storage + powdb-query + powdb-server + powdb-auth   (criterion suite + regression gate)
powdb-compare ──→ powdb-storage + powdb-query   (wide bench vs SQLite/Postgres/MySQL; bundles SQLite from C)
powdb-oracle  ──→ powdb-storage + powdb-query   (differential correctness oracle vs SQLite; bundles SQLite from C)
```

`crates/oracle` loads the same seeded fixture into PowDB and SQLite, then runs every
generated case three ways (native PowQL, PowDB's SQL frontend, SQLite) and compares full
result sets: column names, every value, every value's type, and row order whenever the query
fixes one. Accepted differences are enumerated in `crates/oracle/known_divergences.toml`, each
with a reason and an owning `file:line`. `cargo test -p powdb-oracle` runs the fixed-seed CI
gate; `cargo run -p powdb-oracle --bin powdb-oracle -- --budget 5000` widens the search locally.

### Query Pipeline

```
PowQL text → Lexer (token stream) → Parser (AST) → Planner (PlanNode tree) → Executor (results)
SQL text   → SQL frontend (crates/query/src/sql.rs) ──→ same PowQL AST ──↗
```

- **Lexer** (`crates/query/src/lexer.rs`): Tokenizes PowQL input
- **Parser** (`crates/query/src/parser.rs`): Recursive descent, produces `Statement` AST
- **Planner** (`crates/query/src/planner.rs`): Pure function (no catalog access), produces `PlanNode` tree. Speculatively emits `RangeScan` for range inequalities
- **Executor** (`crates/query/src/executor/` module dir): Runs plans against the storage engine. Has fast paths for common patterns (count, project+limit, sort+limit, agg, update, delete). Lowers `RangeScan` → `Filter(SeqScan)` at runtime when no index exists
- **Plan Cache** (`crates/query/src/plan_cache.rs`): FNV-1a hash, stores canonical plans, substitutes literals at lookup time

### Storage Engine

- **Slotted Pages** (`crates/storage/src/page.rs`): 4KB pages with slot directory
- **Heap Files** (`crates/storage/src/heap.rs`): Variable-length row storage, mmap-based scanning
- **B+ Tree** (`crates/storage/src/btree.rs`): Disk-persisted indexes, created per-column
- **WAL** (`crates/storage/src/wal.rs`): Write-ahead log with group commit
- **Catalog** (`crates/storage/src/catalog.rs`): Schema registry, table/index management

## Key Design Decisions

1. **Planner is pure**: no catalog access. This means `RangeScan` is emitted speculatively; the executor does plan lowering at runtime based on actual index availability
2. **Compiled predicates**: `Filter(SeqScan)` fast paths compile filter expressions into byte-level operations that skip full row decoding
3. **PowQL-native, SQL-as-frontend**: PowQL is the native language and its AST *is* the plan tree. SQL is supported only as a frontend that lowers a subset to the PowQL AST (`crates/query/src/sql.rs`, see `docs/SQL.md`); it adds no second execution path. PowDB uses its own binary wire protocol, so do not add a Postgres/MySQL wire-protocol compatibility layer
4. **Zero-copy scanning**: mmap-based heap scans with `try_for_each_row_raw` for early termination

## Test Commands

```bash
cargo test --workspace                    # all tests
cargo test -p powdb-query                 # query crate only
cargo test -p powdb-query -- executor     # executor tests only
cargo test -p powdb-storage               # storage crate only
```

## Benchmark Commands

```bash
# Wide comparison (PowDB vs SQLite, 100K rows, 15 workloads)
cargo run --release -p powdb-compare

# Criterion regression suite (24 benches, 22 gated workloads; ~5 min of measurement)
cargo bench -p powdb-bench

# Check against regression baselines
cargo run -p powdb-bench --bin compare

# Reset baselines after intentional changes
./scripts/update-bench-baseline.sh
```

## Common Patterns

### Adding a new PowQL keyword
1. Add token variant to `crates/query/src/token.rs`
2. Add lexer rule to `crates/query/src/lexer.rs`
3. Add parser production to `crates/query/src/parser.rs`
4. Add plan node (if needed) to `crates/query/src/plan.rs`
5. Add planner case to `crates/query/src/planner.rs`
6. Add executor case to `crates/query/src/executor/` (start in `mod.rs` / `plan_exec.rs`)

### Adding an executor fast path
Fast paths match on specific `PlanNode` shapes in `execute_plan()`. Pattern-match the plan tree and handle it before the generic recursive executor. Always verify with benchmarks.

### The executor fast paths pattern-match on `Filter(SeqScan)`
If the planner emits a different shape for the same logical operation, the fast paths won't fire. Use `lower_unindexed_range_scans` as a template for plan lowering.

## CI

Eight workflow files. Only the first gates merges.

**Merge gate**
- `.github/workflows/ci.yml`: clippy + fmt + test + doctest (+ ASan, miri, cargo audit, cargo-deny, MSRV, version consistency, cross-version on-disk compat, fuzz-corpus replay, examples smoke, ts-client, node-addon, embedded-sync-js, internal-content-guard, secret-scan). All jobs feed a single **`ci-success`** aggregator job (`needs:` every job, fails if any fails); **`ci-success`** is the one required status check on `main`, so the whole matrix gates merges. Add new jobs to its `needs:` list, or the `ci-needs-completeness` job fails the build.

**Not merge gates**
- `.github/workflows/fuzz.yml`: cargo-fuzz targets. **Separate** from ci.yml (PR-triggered + nightly cron at 07:00 UTC + `workflow_dispatch`); not part of the required check set above.
- `.github/workflows/bench.yml`: criterion microbenchmark suite. **Manual-only (`workflow_dispatch`), NOT a required gate.** Runs on a Depot single-tenant runner (`depot-ubuntu-24.04-4`, tmpfs temp DBs), so numbers are comparable run-to-run; `crates/bench/baseline/main.json` must only ever be rebaselined from a Depot run of this workflow, never from a laptop. It is not a gate because shared-runner and single-tenant timing noise makes a wall-clock threshold an unreliable blocking check, not because of what `powdb-bench` links: it depends on `powdb-storage`, `powdb-query`, `powdb-server` **and** `powdb-auth`, so it does compile server and auth code that the normal suite also covers. Run it on demand: `gh workflow run bench.yml`.
- `.github/workflows/release.yml`: fires on a `v*` tag with full test suite, prebuilt `powdb-cli` / `powdb-server` binaries for Linux x86_64 and macOS arm64, the ghcr Docker image, the GitHub Release, and the `@zvndev/powdb-client` + `@zvndev/powdb-sync` npm publishes.
- `.github/workflows/publish.yml`: publishes the eight crates.io crates in dependency order (`workflow_dispatch`, token-less via OIDC). `dry_run` defaults to **true**, so a real publish must pass `-f dry_run=false`.
- `.github/workflows/publish-node-addon.yml`: publishes `@zvndev/powdb-embedded`, the prebuilt native addon, one runner per platform (`workflow_dispatch`). Run this **before** the post-publish smoke, which installs the addon.
- `.github/workflows/post-publish-smoke.yml`: installs an already-published version from the live registries and exercises it as a new user would (`workflow_dispatch`). Deliberately outside `ci-success`: a registry outage must never block a merge.
- `.github/workflows/pages.yml`: deploys `site/` to GitHub Pages on pushes that touch it.
