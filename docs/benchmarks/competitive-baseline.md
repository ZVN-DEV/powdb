# Cross-engine baseline: relational reads v1

This collector measures equivalent typed results through Rust client APIs. It
compares PowDB with SQLite and DuckDB as embedded engines, and PostgreSQL and
MySQL as local servers. It is a read profile, not a general database ranking or
a test of write durability, concurrency, recovery or adversarial security.

The registered workload catalog is [`cases.py`](../../scripts/competitive/cases.py).
It includes bound and ad hoc point queries, missing keys, scans, ranges,
AND/OR filters, membership, nulls, top-k sorting, offset and keyset pagination,
aggregates, GROUP BY/HAVING, joins with residual predicates, joins with grouping,
correlated EXISTS, IN subqueries, CASE, exact 64-bit integers, windows, and narrow
and wide result delivery. Every query and every rejected/error result is retained.

## Versions and cohorts

[`engines.json`](../../scripts/competitive/engines.json) pins SQLite 3.54.1,
DuckDB 2.0.0, PostgreSQL 18.6, MySQL 8.4.13 LTS and PowDB 0.29.2.
Archives are hash-verified; server images use immutable digests. SQLite is built
with portable `-O3`; DuckDB uses the upstream release library. Rust workers use
the same toolchain and flags. This compares these builds, not the best possible
compiler tuning of each engine.

| Cohort | Participants | Surface |
| --- | --- | --- |
| Embedded native | SQLite, DuckDB, PowDB | SQL for competitors, PowQL for PowDB |
| Embedded SQL | SQLite, DuckDB, PowDB | SQL, including PowDB's supported subset |
| Local server native | PostgreSQL, MySQL, PowDB | Unix sockets; PowQL for PowDB |
| Local server SQL | PostgreSQL, MySQL, PowDB | Unix sockets; SQL |

Server and embedded scores are separate. An embedded function call and a server
round trip have different costs. Native PowQL support does not establish support
in PowDB's SQL frontend. The collector records rejections instead of translating
a rejected SQL probe into PowQL behind the scenes.

## Reproduce

On Linux x86-64 with Rust, Python 3.12+, a C compiler, pkg-config and development
headers required by the comparison clients:

```bash
export RUSTFLAGS='-C target-cpu=x86-64-v2'
bash scripts/competitive/build.sh /absolute/disk-backed/work
export LD_LIBRARY_PATH=/absolute/disk-backed/work/lib
python3 -m unittest discover -s scripts/competitive -p 'test_*.py'
python3 scripts/competitive/run.py \
  --worker-dir /absolute/disk-backed/work/bin \
  --temp-dir /absolute/disk-backed/work/data \
  --output /absolute/disk-backed/work/observations.json \
  --rows 10000 --rounds 5
python3 scripts/competitive/score.py \
  /absolute/disk-backed/work/observations.json \
  --output /absolute/disk-backed/work/scorecard.json
```

A shared desktop run is diagnostic. `--isolated-host` is an explicit operator
assertion, not a way to detect other tenants or certify hardware isolation.
The manual `competitive` Actions workflow uses separate hosted jobs for the two
cohorts. Builds and downloads finish before measurements. Workers and their
owned server share one pinned CPU; the controller uses another physical core.
Hosted VMs still have invisible host contention. Qualification can fail.

For servers, pass `--engines
sqlite,postgres,postgres-aa,mysql,powdb-server,powdb-server-sql`, six rounds and the
three image values from `engines.json` through `--postgres-image`, `--mysql-image`
and `--powdb-image`. Docker access is required. Every server is a disposable,
private container with networking disabled and a unique socket/data directory.
The collector never attaches to a running database. Local test authentication is
intentionally simple within this boundary; TLS/auth overhead is not measured.

## Fixture and measurement boundaries

Each engine gets the same deterministic file-backed fixture. `BenchRow` has a
unique signed 64-bit ID, group/bucket/amount/score integers, name text, a nullable
integer and 128-byte text payload. `BenchGroup` has 64 rows. All get the same
logical indexes: unique row ID, nonunique bucket, unique group ID. SQLite maps
the 64-bit ID to its native `INTEGER PRIMARY KEY`. Other physical layouts remain
engine-specific. Compound `(score,id)` pagination is deliberately unindexed;
separate ID pagination tests exercise the existing primary-key index.

Fixture inserts run in an explicit transaction outside timing. The loader exits
before the measured worker starts. Checkpointing precedes measurement where the
adapter exposes it; PowDB's server persists fixture writes at statement/commit
boundaries without a benchmark-only checkpoint API. Every round creates new
fixtures. Round order rotates engines and workloads, rather than always warming
one participant first. There is one explicit warmup per workload. Filesystem
caches are not dropped; these are not certified cold-cache tests.

Point keys use a permutation stride coprime to the fixture size. The first 1,000
requests visit 1,000 different keys, including at adversarial fixture sizes.
Bound points prepare once outside timing and bind a fresh key for each request.
Ad hoc cases include query parsing in timing. Literal formatting happens outside
timing for every participant. This avoids the repeated-key cache artifact that
invalidated earlier point ratios.

Every adapter converts the same primitive result types into typed JSON values.
Complete ordered rows are hashed with a length-delimited BLAKE3 digest. Samples
are compared against same-round SQLite, plus independent fixture invariants for
counts, sums, exact integers and representative rows. This is a correctness
oracle for this fixture, not proof of general SQL equivalence. AVG alone allows
1e-12 relative/absolute scalar rounding differences. No lossy integer-to-float
normalization is allowed.

Two delivery modes have different timing boundaries:

- **Materialized:** retain the complete application result. Typed row conversion
  and collection are timed; digesting happens afterward. First available
  application row is reported at completion because this API returns a collection.
- **Consume:** retain only a digest and up to three preview rows. Row conversion
  and hashing are timed. First-row latency is measured at the visitor callback.
  SQLite steps rows; DuckDB uses `stream_arrow`, not its materializing `query()`;
  PostgreSQL fetches bounded 256-row portals for consumption (including explicit
  transaction/portal round trips) and uses its normal complete-result query API
  for materialization; MySQL uses its result iterator.
  PowDB currently materializes its engine result and native wire frame before
  visiting rows. Calling it `consume` does not turn PowDB into a streaming engine.

Unknown/unsupported types are adapter errors. The v1 fixture tests exact int64,
text, nulls and aggregate floats. UUID, decimal semantics, datetime, binary/JSON
round trips, nested shapes, cancellation and backpressure need dedicated profiles.

## Memory and settings

A sampler reads Linux process-tree PSS every 10 ms, including the owned server
and measured client. PSS apportions shared resident pages; summed RSS would
count shared pages repeatedly. Unmapped filesystem cache is excluded. Short
allocation peaks can be missed. This is **sampled resident memory**, not total
machine memory, allocator high-water usage or a guaranteed peak. Reading `/proc`
can fail for a container process; such memory remains unknown, never zero.

Delivery cases reopen the client/embedded worker before each delivery mode,
then warm up that mode. This prevents a preceding materialization's retained
client heap from being attributed to streaming. Owned servers retain their
normal resident caches/buffers. Other cases can retain state from previous
workloads. Protocol and result-summary serialization are inside the memory
sampling interval. Loading allocations are excluded by the separate loader. Use delivery-case memory scores as
a workload-specific observation, not an independent allocation attribution.

Actual settings accompany each record: versions, durability, memory and thread
settings where available. Defaults differ: PowDB's query budget is 256 MiB;
DuckDB's default memory limit depends on host RAM; SQLite has a page cache;
PostgreSQL has shared buffers and work memory; MySQL has an InnoDB buffer pool.
Server containers have a 2 GiB cgroup cap. These do not establish equal effective
memory budgets. Larger-than-memory, spill and equal-budget tests are separate
work. One query worker is used, and query parallelism is disabled where supported.
Full durability is retained for PowDB; SQLite uses WAL/FULL, PostgreSQL keeps
fsync/synchronous commit/full-page writes, MySQL uses durable InnoDB commit and
binlog sync settings. Read performance does not prove equivalent crash safety.

## Scores and refusals

`score.py` emits every capability outcome and raw timing summary. A common-case
latency score is `100 * geometric_mean(fastest_median / engine_median)` across
explicitly listed common cases. 100 means fastest on every included case. It is
sensitive to the registered workload mix; it is not a universal DB rating.
Unsupported cases reduce coverage and are listed as exclusions. A score on fewer
common cases must be read alongside its coverage, never substituted for it.

Qualification requires a clean source checkout, matching build provenance,
complete catalog, at least five rounds, full request counts, correct typed
results and no worker failures. Each included workload must have at most 20%
relative range across round medians. Identical SQLite worker binaries run as
SQLite and SQLite-A/A; local servers also run PostgreSQL-A/A. A/A median latency
must differ by at most 10% on each included case. Missing or noisy evidence
withholds the aggregate. A wrong result disqualifies that engine's score.

Delivery PSS scores use the analogous ratio of smallest to candidate sampled
PSS. They require valid samples in every round, at least three samples per
measurement interval and an A/A PSS difference no greater than 20%. Unknown
memory and unqualified runs produce no memory rating. Per-request p95/p99 are
reported only with at least 1,000 actual observations, not from round medians.
Results below the registered request count remain diagnostic.

Never rerun only losing/noisy cases and replace the original evidence. Preserve
the original observation file and qualification reasons. A new full run is new
evidence, not a correction to the first run.

## Capability and security interpretation

The companion [capability matrix](competitive-capabilities.md) separates measured
query support from documented controls and untested behavior. Security controls
are requirement coverage, not a vulnerability probability, audit, penetration
test or claim that one implementation language is inherently secure. Recovery,
contention, durability and security need behavioral and fault-injection evidence
before their presence becomes a qualified score.
