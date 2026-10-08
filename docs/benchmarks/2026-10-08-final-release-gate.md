# v0.29.0 final performance preflight — 2026-10-08

**Verdict: failed. Release is authorized with known performance costs, not
performance-cleared.** No baseline, threshold, budget or fixture was changed.

[Depot run 37831411672](https://github.com/ZVN-DEV/powdb/actions/runs/37831411672)
completed both suites and the unchanged comparator on the integrated release
branch. Candidate `7cf94c535d58c0fe421e5ccdc0f002e6ac0a2d79`; original control
`9294a9a9bb0c8cda281e9de71655aceb3861aba7`. Later changes are benchmark notes
only, not production code or package-version changes.

Both revisions used x86_64 runner `depot-ubuntu-24.04-4`, Rust 1.99.0
(`b940084d7`, 2026-09-28), and `RUSTFLAGS=-C target-cpu=x86-64-v2`.
Query fixtures use **WAL Off**; these are not default Full-mode durable writes,
SQLite comparisons, or client latency percentiles. Values are Criterion median
nanoseconds per iteration. The growing-insert fixture is nonstationary and must
not be advertised as fixed-table-size insert latency.

## Same-instance failures

| Workload | Original control | Release candidate | Change | Limit |
|---|---:|---:|---:|---:|
| Growing insert | 248 ns | 420 ns | +69.19% | 10% |
| Indexed update | 1,758 ns | 1,966 ns | +11.80% | 10% |
| Filtered update | 1.152 ms | 1.444 ms | +25.30% | 10% |
| Filtered delete | 113.53 us | 135.53 us | +19.37% | 10% |

Every read/storage workload passes its same-instance limit. The 1,000-row
batch is +6.64%, within 10%. All four thesis ratios pass, which does not
override the workload failures. The separate historical absolute gate fails
top-100 projection, growing insert, indexed update, filtered update and delete.
That older absolute baseline uses a different compiler.

[Complete raw estimates and deltas](2026-10-08-final-release-gate.json) preserve
all 23 workloads, confidence intervals and other statistics.

## Interpretation and release decision

The original control predates the stronger failed-statement rollback contract.
Before-images, allocator undo and budget accounting retain that protection.
The preceding optimization removed demonstrated avoidable copying, reducing
growing inserts 84.29% and filtered updates 36.88% against the already-safe
engine in its own same-instance run. See
[optimization evidence](2026-10-08-rollback-optimization.md).

Final candidate costs are close to the previous original-control comparison
(for example inserts 420 versus 417 ns, filtered updates 1.444 versus
1.446 ms), but control costs differ. Compare each candidate only with its own
same-instance control; different jobs cannot be combined. Same-instance runs
reduce cross-machine differences but do not eliminate order, thermal or
within-run variability. Neither the residual cost nor its exact decomposition
has been proved unavoidable.

After the previous failures were disclosed, the maintainer explicitly requested
merging and releasing the outstanding work. The final warning is updated to
the four failures above. This is a release decision, not a green benchmark:
required correctness CI, package validation, protected publication and live
registry smokes remain mandatory. Remaining optimization work starts with
single-row capture allocation and filtered mutation page-copy overhead.

```bash
gh workflow run bench.yml --ref codex/powdb-release-ready \
  -f control_ref=9294a9a9bb0c8cda281e9de71655aceb3861aba7
```

The branch may advance with documentation; use the exact evaluated candidate
above when reproducing the recorded result.
