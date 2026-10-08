# Release preflight performance gate — 2026-10-07

**Verdict: failed. The candidate is not performance-cleared for release.**
The regression thresholds and baseline were not changed or bypassed.

[Depot run 37619414494](https://github.com/ZVN-DEV/powdb/actions/runs/37619414494)
built and ran the control and candidate on the same instance, with the same
compiler and flags. Both suites completed; the final comparator failed both
the absolute baseline check and same-instance comparison. All four thesis
ratio checks passed, which does not override the workload failures.

## Reproduction and scope

```bash
gh workflow run bench.yml --ref codex/powdb-value-update \
  -f control_ref=9294a9a9bb0c8cda281e9de71655aceb3861aba7
```

The evaluated candidate is `1534a97fcd7a23fbce6cf0e3db5b917742cd251a`.
Use that exact commit when reproducing this record; the branch can advance.
Runner: `depot-ubuntu-24.04-4`, x86_64, Rust 1.99.0
(`b940084d7`, 2026-09-28), `RUSTFLAGS=-C target-cpu=x86-64-v2`.
The absolute baseline records Rust 1.98.1; the same-instance comparison below
uses Rust 1.99.0 for both revisions and is the stronger evidence of regression.

The query-engine fixtures use **WAL Off**. These are not default Full-mode
durable-write numbers, SQLite comparisons, or client latency percentiles.
Criterion's median nanoseconds per iteration are compared here. The growing
insert fixture is intentionally not stationary: it keeps adding unique rows
to a populated table. Its result must not be advertised as fixed-size point
insertion latency.

[Raw Criterion estimates and computed deltas](2026-10-07-release-gate.json)
preserve all 23 artifact workloads, including confidence intervals and the
other statistics. They are not a replacement benchmark baseline.

## Same-instance failures

| Workload | Control | Candidate | Change | Existing limit |
|---|---:|---:|---:|---:|
| Filter, full rows | 11.813 ms | 13.890 ms | +17.58% | 10% |
| Non-indexed point lookup | 585.16 us | 645.48 us | +10.31% | 10% |
| Selective conjunction path | 5.035 us | 5.672 us | +12.66% | 7% |
| Growing-table single insert | 242.01 ns | 2,681.33 ns | +1,007.92% | 10% |
| 1,000-row insert batch | 199.40 us | 247.94 us | +24.34% | 10% |
| Indexed update | 1.608 us | 2.298 us | +42.87% | 10% |
| Filtered update | 1.074 ms | 2.438 ms | +126.99% | 10% |
| Filtered delete | 106.55 us | 155.73 us | +46.16% | 10% |

The indexed query-read median changes by +1.75%; the regressions are not
uniform across the engine. Small near-limit read differences need confirmation
and profiling; the much larger write differences must not be dismissed as noise.

## Correctness finding before this run

The preceding run stopped during growing inserts at the 256 MiB dirty-page
budget, before a gate verdict. Engine autocommit pinned the new statement while
old committed pages still occupied the buffer, preventing the usual spill.
The candidate repairs that boundary: settle WAL generations, flush committed
heap/header/index state before the next implicit pin, and retain the WAL log.
Six tiny-budget regressions cover all WAL modes, rollback, prior-row survival,
retained history and issued deferred durability tickets. Individual statements
and explicit transactions still have their original size limit.

That repair makes the suite complete; it does **not** make this gate green.
The control also predates the new failed-statement rollback guarantee, so the
comparison includes the cost of stronger correctness rather than an identical
failure contract. This explains why a tradeoff is plausible, not why a failed
release criterion can be silently waived.

## Next decision

Profile and reduce statement rollback/snapshot costs while retaining the new
failure guarantees; investigate the read failures separately. This is a focused
performance tranche, not a baseline refresh. No general speedup or durable-write
gain is supported by this record. A release exception requires an explicit
maintainer decision; this preflight does not grant one.
