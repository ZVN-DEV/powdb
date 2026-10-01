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

The script uses `python3` from the developer environment for aggregation. Run
`scripts/paired-bench.sh --selftest` after edits; it feeds synthetic JSON into
the same evaluator and checks win, protected regression, noisy spread, under-5,
settings mismatch, and contamination cases without executing benchmark binaries.

The script alternates both revision order and engine order. It reports raw
arithmetic means for every run, then medians, min/max, absolute spread, and
relative spread. It does not invent percentiles from five means.

A Full-mode candidate is durable-publishable only if:

1. all driver correctness checks pass;
2. at least five paired runs were collected;
3. baseline/candidate labels are distinct and binary hash provenance is present,
   singular per ref, and distinct between refs;
4. profile, mode, fixture, settings, and platform metadata match across records;
5. the run is not marked contaminated with `--contaminated`;
6. no PowDB workload exceeds the fixed 20% relative-spread policy;
7. at least one write workload median improves by 25% or the point-read median
   improves by 30%; and
8. point-read plus protected scan/aggregate medians do not regress by more than
   10%.

`--require-improvement` makes the script exit nonzero when the fixed engineering
thresholds fail. This is separate from durable publishability: Off-mode output
can report `diagnostic_pass` for the same improvement contract, but it is never a
durable-write claim.

The script leaves baseline files and benchmark baselines untouched. It uses only
temporary databases created by the driver and has no external database URL path.
