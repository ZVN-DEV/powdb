# powdb-bench

The performance regression gate. Not published to crates.io.

This crate holds two things that are easy to confuse:

1. **A criterion suite** that measures 23 workloads across the storage layer and
   the PowQL executor.
2. **A comparator** that reads the most recent criterion run and fails if any of
   the 22 gated workloads moved outside its tolerance, or if a ratio between two
   workloads exceeded its ceiling.

The suite measures. The comparator judges. They are separate binaries so a
developer can measure freely on a laptop without pretending the numbers are
comparable to the checked-in baseline.

## Quick start

```bash
cargo bench -p powdb-bench            # measure (about 5 minutes, plus a cold compile)
cargo run -p powdb-bench --bin compare  # judge the run just measured
```

The comparator exits 0 on pass and 1 on regression. It prints every workload
with its baseline, its observed value, and the verdict.

## Why this is not a merge gate

`bench.yml` is `workflow_dispatch` only. Wall-clock timing on a shared CI runner
is noisy enough that a blocking threshold produces more false failures than real
catches. Run it deliberately:

```bash
gh workflow run bench.yml
```

The workflow runs on a Depot single-tenant runner (`depot-ubuntu-24.04-4`) with
temp databases on tmpfs, so numbers are comparable run to run.

## The baseline is Depot-only

`baseline/main.json` may only be rebaselined from a Depot run of `bench.yml`.
Never from a laptop. An arm64 laptop is roughly twice as fast as the x86 runner,
so a laptop rebaseline silently doubles every tolerance and the gate stops
catching anything.

This is enforced, not just documented. `main.json` records a fingerprint of the
machine that produced it: `runner`, `rustflags`, and `arch`. The comparator
compares that fingerprint against the machine it is running on and refuses to
judge across a mismatch. `arch` is measured from the comparator binary itself
rather than asked of `rustc`, so the two cannot disagree.

To rebaseline after an intentional change, run `bench.yml` on the branch
(`gh workflow run bench.yml --ref <branch>`), let it finish, then write the
baseline from that run's uploaded criterion estimates:

```bash
./scripts/update-bench-baseline-from-depot.sh <run id>
```

Every number, the runner label, the `RUSTFLAGS` and the toolchain come from the
run's artifact and job log; nothing is measured on the machine running the
script, and the document records the run in `source_run`. The script prints the
old and new value of every gated workload so the commit message can say which
change moved which one. `./scripts/update-bench-baseline.sh` is the same
extraction run on the Depot runner itself; on a laptop it would record the
laptop's arch and the comparator would refuse the result on Depot, which is the
gate working.

Both scripts stage the new file but do not commit, and neither touches
`baseline/thesis-ratios.json`: raising a ratio ceiling is a separate, deliberate
commit.

Both `main.json` and the gate drifted once already: the baseline written at
v0.13.0 was carried untouched through v0.27.0 while `bench.yml` was never run on
a release branch, so a 22x regression on one workload and a 70% one on another
went unmeasured for fourteen releases. The release checklist's "run the perf
gate on the release branch" step exists because of that.

## The workload list has one source of truth

`WORKLOADS` in `src/bin/compare.rs` is the list that gates. Everything else asks
that list rather than keeping a copy, because the failure mode is silent: the
comparator treats a workload with no baseline entry as a first-run capture, so it
prints a number and passes. A workload dropped from `main.json` therefore stops
being guarded without anything going red.

Two checks keep this honest:

- A unit test asserts that the gated set and the `main.json` workload set are
  exactly equal, in both directions. It runs under `cargo test --workspace`.
- `scripts/update-bench-baseline.sh` asks the comparator for the list
  (`compare --list-workloads`) instead of hand-copying it.

`scripts/ci/bench-gate-selftest.sh` does keep its own copy, deliberately. It
drives the built binary against synthetic criterion output to prove every verdict
the comparator can reach is actually reachable, and a drift there turns its
"missing workload" case from synthetic into real, which is the point.

One bench, `range_scan_indexed`, is measured but not gated. It exists to watch
indexed range selectivity, and its variance is too wide for a threshold.

## The other binaries

None of these feed the gate or the baseline. Each prints its own numbers.

| Binary | What it does |
|---|---|
| `smoke-bench` | Order-of-magnitude sanity check on the native engine. No statistics, no warm-up, just wall clock. |
| `json-filter` | JSON path-filter latency: compiled inline leaf against the decode fallback against a flat-column ceiling. |
| `phase0-read-baseline` | Starts a real TCP server and reports remote read latency plus 1, 2, 5 and 10 client scaling. |
| `expression-index-release-gate` | Fixed-seed proof for JSON-path indexes and bounded ordered expression-index scans. Emits JSON and enforces relative speedup gates in release builds. |
| `compound_join_scaling` | Join scaling driver. |

Run any of them in release mode:

```bash
cargo run --release -p powdb-bench --bin smoke-bench
```

## Related

`powdb-compare` is a different crate. It compares PowDB against SQLite, Postgres
and MySQL over a 100K-row fixture. See `crates/compare/README.md`.
