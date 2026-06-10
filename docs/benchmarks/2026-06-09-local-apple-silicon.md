# Local bench snapshot — 2026-06-09 (Apple Silicon)

Captured from `crates/compare/results.csv` (2026-06-09, macOS arm64, release
build). This file preserves the run because `results.csv` is in `.gitignore`
(intentionally: it's a bench artifact, rewritten on every run).

> **These are local laptop numbers, NOT the CI regression baseline.**
> `crates/bench/baseline/main.json` is captured on CI hardware (GitHub
> ubuntu x86) and must never be compared against — or rebaselined from —
> arm64 laptop runs.

## Methodology

- Harness: `cargo run --release -p powdb-compare`
- Fixture: 100,000 rows on each engine, identical schema
- Engines: PowDB (in-process, `WalSyncMode::Off`) + SQLite (in-process,
  `:memory:`) — both entirely in RAM
- Each workload: ns/op reported as median

## Results

| workload                   | PowDB    | SQLite   | ratio (SQLite÷PowDB) |
| ---                        |     ---: |     ---: |                 ---: |
| point_lookup_indexed       | 54 ns    | 190 ns   | 3.5x |
| point_lookup_nonindexed    | 239.5 us | 301.9 us | 1.3x |
| scan_filter_count          | 233.4 us | 1.31 ms  | 5.6x |
| scan_filter_project_top100 | 6.4 us   | 8.2 us   | 1.3x |
| scan_filter_sort_limit10   | 2.10 ms  | 6.02 ms  | 2.9x |
| agg_sum                    | 187.7 us | 1.40 ms  | 7.5x |
| agg_avg                    | 280.2 us | 1.65 ms  | 5.9x |
| agg_min                    | 137.0 us | 1.60 ms  | 11.7x |
| agg_max                    | 142.6 us | 1.38 ms  | 9.7x |
| multi_col_and_filter       | 1.36 ms  | 3.05 ms  | 2.2x |
| insert_single              | 297 ns   | 627 ns   | 2.1x |
| insert_batch_1k            | 141 ns   | 203 ns   | 1.4x |
| update_by_pk               | 42 ns    | 261 ns   | 6.3x |
| update_by_filter           | 1.74 ms  | 4.42 ms  | 2.5x |
| delete_by_filter           | 1.18 ms  | 1.59 ms  | 1.3x |

**Score: PowDB faster on all 15 workloads on this run.** Contrast with the
[2026-04-07 snapshot](2026-04-07-wide-bench-snapshot.md) (5 wins, 10 losses)
— the write-path and point-lookup losses identified there have since been
closed.

## Reproducibility

```bash
cargo run --release -p powdb-compare
# rewrites crates/compare/results.csv
```

Re-running will produce slightly different absolute numbers (±10-15% on an
idle laptop), but the verdicts are stable.
