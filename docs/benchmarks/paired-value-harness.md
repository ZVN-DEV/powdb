# Paired value benchmark harness

This harness exists for reproducible control/candidate evidence, not for broad
database shootouts. It compares explicit `powdb-compare-paired` binaries built
from two revisions and records machine-readable provenance with every run.

## Modes

- `full` is the publishable mode. PowDB uses a file-backed temporary database
  with `WalSyncMode::Full`; SQLite uses a file-backed temporary database with
  WAL journal mode and `synchronous=FULL`.
- `off` is a labeled diagnostic. PowDB uses WAL-off and SQLite uses `:memory:`.
  Off results can guide engineering work, but they are not durable-write claims.

## Workloads

The value-v1 profile times:

- varied-key indexed point reads;
- changed-value primary-key updates;
- prepared single-row inserts;
- bounded prepared insert batches;
- protected scan-filter counts;
- protected aggregate sums.

Correctness checks are outside the timed loops. The driver verifies fixture
point reads, scan counts, aggregate sums, mutation readback, post-mutation row
count/sum, and file-backed reopen parity. A run with failed checks is invalid
and must not be published.

## Pairing and evaluation

Run the script with explicit binaries:

```bash
scripts/paired-bench.sh \
  --baseline-bin /path/to/baseline/powdb-compare-paired \
  --candidate-bin /path/to/candidate/powdb-compare-paired \
  --mode full \
  --runs 5 \
  --output paired-full.json
```

The script alternates both revision order and engine order. It reports raw
arithmetic means for every run, then medians, min/max, and spread. It does not
invent percentiles from five means.

A Full-mode candidate is publishable only if:

1. all driver correctness checks pass;
2. at least one write workload median improves by 25% or the point-read median
   improves by 30%; and
3. protected scan/aggregate medians do not show a repeatable regression above
   10%.

The script leaves baseline files and benchmark baselines untouched. It uses only
temporary databases created by the driver and has no external database URL path.
