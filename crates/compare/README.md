# powdb-compare

Wide comparison benchmark: PowDB vs SQLite, plus an optional Postgres column and
a feature-gated MySQL column. Runs 15 workloads (point lookups, scans, filters,
aggregates, inserts, updates, deletes) over a 100K-row fixture and prints a
side-by-side table plus PowDB-relative ratios.

## Quick start (PowDB vs SQLite only)

```bash
cargo run --release -p powdb-compare
```

SQLite (in-memory) is always included. Postgres and MySQL are optional: if no
server is reachable the run prints a `[skipped]` line and continues — it never
fails because an external database is down.

For an isolated local comparison, set `POWDB_BENCH_PG_URL=skip`; do not point
the harness at an application database, because fixture setup is destructive.

## Workload contract

Point lookups use deterministic, varying keys. Primary-key updates spread keys
across the fixture and assign a fresh value on each operation; filter updates alternate two
statuses. Thus updates change stored values rather than repeatedly assigning
the same value to the same row. Results are consumed through `black_box`.
`cargo test -p powdb-compare` checks key diversity, mutation outcomes and adapter
results against an independent fixture model for both PowDB and SQLite.

Before 2026-09-19, the lookup seed was constant inside each timed loop, and
primary-key updates repeatedly wrote `42` to one key. Old measurements of
those workloads are not comparable with this corrected runner. Write workloads
share a fixture, so changing the primary-key workload also changes the rows
matched by the subsequent filter update.

This is an engine-cost diagnostic, not a durability or statistical benchmark:
PowDB uses a temporary on-disk database with **WAL completely disabled**, SQLite
uses `:memory:`, and each cell is an arithmetic mean from one timed loop. It
does not report confidence intervals. Repeat on a quiet host and report the
spread; never use these results to promise durable-write throughput.

## Paired value-profile harness

`powdb-compare-paired` is the machine-readable paired harness used for
publishable PowDB-vs-SQLite evidence. It is separate from `compare-engines` so
the old diagnostic runner remains available.

The publishable mode is `--mode full`: PowDB uses a file-backed temporary
database with `WalSyncMode::Full`, while SQLite uses a file-backed temporary
database with `journal_mode=WAL` and `synchronous=FULL`. The `--mode off` profile
is deliberately labeled diagnostic: PowDB uses WAL-off and SQLite uses
`:memory:`. Do not mix Full and Off numbers in one claim.

Each driver invocation emits a single JSON document on stdout:

```bash
cargo run --release -p powdb-compare --bin powdb-compare-paired -- \
  --engine powdb \
  --mode full \
  --engine-ref candidate \
  --engine-hash "$(shasum -a 256 target/release/powdb-compare-paired | awk '{print $1}')"
```

The JSON includes provenance (`engine_ref`, binary hash, mode/storage labels),
fixture/settings, raw per-run arithmetic means in `mean_ns_per_op`, and untimed
correctness checks. The timed operations use varied keys, changed-value
primary-key updates, prepared single-row inserts, bounded prepared insert
batches, and protected scan/aggregate workloads. Correctness checks run outside
the timed loops and include fixture parity, mutation readback, row-count/sum
agreement, and reopen parity for file-backed modes.

Use the pairing script for baseline/candidate evidence:

```bash
scripts/paired-bench.sh \
  --baseline-bin /path/to/baseline/powdb-compare-paired \
  --candidate-bin /path/to/candidate/powdb-compare-paired \
  --baseline-ref baseline \
  --candidate-ref candidate \
  --mode full \
  --runs 5 \
  --output paired-full.json
```

The script requires `python3` for JSON aggregation and synthetic selftests:

```bash
scripts/paired-bench.sh --selftest
```

The script alternates baseline/candidate order and PowDB/SQLite engine order
across at least five runs, then reports raw run means plus medians/min/max,
absolute spread, and relative spread. A Full run is durable-publishable only
when all correctness checks pass, baseline/candidate labels and binary hashes
are distinct and singular, profile/settings/fixture/platform metadata matches,
the run is not marked contaminated, no PowDB workload exceeds the fixed 20%
relative-spread policy, the candidate improves either a write median by at least
25% or point-read median by at least 30%, and point-read plus protected
scan/aggregate medians do not regress by more than 10%.

Use `--require-improvement` when the command should exit nonzero unless the
fixed 25% write / 30% point-read / 10% protected-regression engineering
thresholds pass. Use `--contaminated --contamination-note <why>` to keep a run
valid but explicitly non-publishable. Off-mode results can pass the engineering
improvement contract, but they remain diagnostic and are never durable claims.

## With Postgres

A pinned local Postgres is provided via Docker Compose. The credentials and
database name match the URL the harness tries by default, so no env var is
needed:

```bash
# 1. bring up Postgres (pinned postgres:16.4-bookworm)
docker compose -f crates/compare/docker-compose.yml up -d

# 2. run the comparison — Postgres now appears as a column
cargo run --release -p powdb-compare

# 3. tear it down
docker compose -f crates/compare/docker-compose.yml down
```

To point at an existing Postgres instead, set `POWDB_BENCH_PG_URL`:

```bash
POWDB_BENCH_PG_URL=postgresql://user:pass@host:5432/db \
  cargo run --release -p powdb-compare
```

Set `POWDB_BENCH_PG_URL=skip` to deliberately bypass Postgres even if one is
running.

## With MySQL (feature-gated)

```bash
POWDB_BENCH_MYSQL_URL=mysql://user:pass@host:3306/db \
  cargo run --release -p powdb-compare --features mysql
```

## Environment variables

| Variable | Effect |
|---|---|
| `POWDB_BENCH_PG_URL` | Override the Postgres URL, or `skip` to bypass Postgres |
| `POWDB_BENCH_MYSQL_URL` | Override the MySQL URL (requires `--features mysql`) |
