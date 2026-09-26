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
