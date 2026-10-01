# Write-bookkeeping profile — 2026-10-01

This is local engineering evidence, not a new PowDB-versus-SQLite speed claim.
The experiment compares the integrity-fixed engine `4e6a8e4` with the optimized
engine at `3998891`, using the same paired driver (`b7dd073`). Binary hashes,
raw samples and settings are retained in the accompanying JSON artifacts.

Host: Apple M5 Max, 128 GiB RAM, macOS 26.5.1 (25F80), rustc 1.97.0
(`2d8144b78`). Portable default build flags; no benchmark fingerprint override,
threshold change, or checked-in performance-baseline reset.

## Accepted diagnostic workload

Five paired rounds alternate both revision and engine order. The longer
WAL-Off run used 20,000 fixture rows, 500,000 point reads, 20,000 changing
updates/single inserts, 2,000 protected scans/aggregates, and 100-row batches.
Every driver correctness and reopen check passed.

| PowDB workload | Control median of run means | Candidate median of run means | Observation |
|---|---:|---:|---|
| Prepared single insert | 1,065.354 ns/op | 657.608 ns/op | 38.3% lower engine cost |
| Changed-value point update | 1,269.963 ns/op | 648.060 ns/op | 49.0% lower median; spread warning |
| Bounded batch insert | 337.481 ns/row | 332.904 ns/row | Similar |

The selected single-insert workload meets the fixed >=25% improvement criterion.
Its relative spread is 2.3% for control and 5.3% for candidate. Protected point
read/scan/aggregate median regressions are below 10%. Earlier five-round sets
also observed roughly 34–37% lower single-insert cost.

The complete longer-run report still flags 24.9% spread in candidate point
updates. Keep that warning: the entire report is **not publication-grade**.
WAL Off writes no WAL and has no crash durability; none of these numbers
promises durable-write throughput. These are medians of arithmetic run means,
not per-operation percentiles or confidence intervals.

## Full mode

The separate five-round Full profile uses file-backed PowDB Full and file-backed
SQLite WAL with `synchronous=FULL`, with pragma readback validation. All result
checks passed. It does not meet the improvement threshold: update and single
insert medians were approximately unchanged/slightly slower, and several
workloads have substantial spread. The evaluator correctly marks it
`not_publishable`; no Full-mode throughput improvement is claimed.

## What changed

Full/Normal transactions no longer allocate unused before-image mementos;
their rollback still uses WAL recovery. WAL-Off successful statements reuse
metadata buffer allocations, while page-image maps are dropped and dirty-page
budget charges released. Recovery/atomicity tests remain required; correctness
was not exchanged for these diagnostic savings.

See [paired harness instructions](paired-value-harness.md) for reproduction and
the explicit limitations of both profiles. The original withdrawn comparisons
remain in [the historical correction record](2026-09-19-benchmark-corrections.md).
