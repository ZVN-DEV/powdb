# Benchmark corrections: September 19, 2026

This page records why several older PowDB-vs-SQLite benchmark rows were
withdrawn instead of being reused as current claims.

The short version: the historical table still documents a dated snapshot for
aggregate and scan-shaped workloads, but lookup and update rows affected by the
issues below are not publishable current evidence. They should stay visible as
withdrawn rows, not quietly disappear and not be replaced by guessed ratios.

## What was wrong

The older comparison runner had two workload-shape problems:

- Point-lookup loops repeatedly queried one key because the seed was constant
  inside the loop. Those rows described repeated access to the same key, not a
  varied-key point-lookup workload.
- Primary-key updates repeatedly wrote the same value to the same row. Filter
  updates also needed value variation, and the preceding point-update workload
  changed their input fixture.

Those mistakes made the old lookup/update ratios too specific to trust as
general claims. They are now withdrawn in the README, site, and SQLite
comparison page.

## What changed in the runner

The comparison runner now varies lookup keys, visits different update keys, and
changes assigned update values. Regression coverage checks workload diversity
and resulting data in both engines so the same repeated-key/repeated-value shape
does not silently return.

The corrected runner is the right local tool for exploratory measurement:

```bash
cargo run --release -p powdb-compare
```

Exploratory output is still not enough for a new public ratio by itself. Shared
machine load, fixture state, and run ordering can dominate small differences,
especially for write rows.

## What remains usable from the historical table

The aggregate and scan-shaped rows remain a dated snapshot of the published
methodology:

- 100K-row fixture
- one query at a time
- PowDB in `WalSyncMode::Off`
- SQLite in `:memory:`
- median of five runs on an Apple M5 Max laptop
- commit `e3dfa71`, measured 2026-08-15

Those numbers are laptop measurements, not CI measurements, and they are not a
durability comparison because neither engine fsyncs in that table. They should
be introduced as workload-specific historical evidence, never as "PowDB is
fastest vs SQLite."

Historical table, archived from the main README/site:

| Workload | PowDB | SQLite | Result |
|---|---|---|---|
| Aggregate MIN | 221 us | 1.70 ms | PowDB 7.7x faster |
| Aggregate MAX | 217 us | 1.47 ms | PowDB 6.8x faster |
| Aggregate SUM | 234 us | 1.45 ms | PowDB 6.2x faster |
| Update by primary key | — | — | Withdrawn: repeated-key workload |
| Aggregate AVG | 455 us | 1.70 ms | PowDB 3.7x faster |
| Scan + filter + count | 380 us | 1.40 ms | PowDB 3.7x faster |
| Non-indexed point lookup | — | — | Withdrawn: repeated-key workload |
| Scan + filter + sort + limit 10 | 2.46 ms | 6.41 ms | PowDB 2.6x faster |
| Multi-column AND filter | 1.58 ms | 3.21 ms | PowDB 2.0x faster |
| Update by filter (10K rows) | — | — | Withdrawn: repeated-value workload |
| Insert single row | 380 ns | 638 ns | roughly tied |
| Scan + filter + project top 100 | 8.1 us | 8.9 us | roughly tied |
| Delete by filter (10K rows) | 1.57 ms | 1.75 ms | roughly tied |
| Insert batch (1K rows) | 242 ns | 214 ns | roughly tied |
| Indexed point lookup | — | — | Withdrawn: repeated-key workload |

## What must happen before publishing replacement ratios

Replacement lookup/update numbers should come from the paired benchmark harness
work, not from a one-off local run. The intended publication shape is:

- explicit baseline and candidate binaries
- equivalent file-backed SQLite WAL + `synchronous=FULL` profile where durable
  writes are being compared
- varied lookup keys and changed update values
- alternating baseline/candidate run order on the same host and fixture
- correctness checks outside the timed region
- raw samples and provenance committed with the report

Until that evidence exists, public copy should say the affected rows are
withdrawn pending controlled remeasurement and should not publish new ratios for
point lookups or update workloads.
