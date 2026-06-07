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
