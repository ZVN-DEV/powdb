# Statement rollback optimization — 2026-10-08

**Verdict: fixed optimization targets passed; full release gate still failed.**
The existing failed-statement guarantees, dirty-page limits and benchmark
thresholds are unchanged. This work targets avoidable WAL-Off rollback costs;
it does not claim faster durable Full/Normal writes or competitive rankings.

Candidate production code: `85da647c2fcd6da286a83e2db782c2c838373b00`.
Control: `f70bb7c633b7ba5425f89ab877305477393932d8`, which already includes the
new rollback protection. This measures an optimization of the same failure
contract, unlike comparing against the older pre-protection release control.

## Cause and change

Each Off-mode statement previously cloned table-wide free-space vectors,
even when inserting or changing one row. Inline 4KB before-images also made
hash-map growth expensive, and filtered update/delete scans captured pages
before knowing whether a row matched.

The candidate records sparse first-write allocator originals and only original
stack entries actually popped. Rollback restores exact bucket order, including
stale entries; transaction-only push/pop churn does not grow the undo journal.
Page images are individually boxed and charged, repeated-page capture is
deduplicated, and filtered scans capture only before mutation. No-op writes are
still performed. CRC capture errors and rollback-I/O failures release their
before-image reservations after payload ownership ends.

## Five paired local rounds

Native arm64 macOS, Rust 1.97.0, default portable release flags, WAL Off.
The same diagnostic harness was compiled against both engines; both binaries
were built before timing. Order alternated by round. Each timing is the median
of five per-run arithmetic means, not a request-latency percentile.

| Workload | Control | Candidate | Lower cost |
|---|---:|---:|---:|
| Insert 20K rows, starting at 100K | 749 ns/op | 550 ns/op | 26.6% |
| Insert 20K rows, starting at 1M | 2,575 ns/op | 526 ns/op | 79.6% |
| Same-value filtered update, 100K rows | 3.081 ms/op | 1.918 ms/op | 37.7% |
| Changing-value filtered update, 100K rows | 3.030 ms/op | 1.884 ms/op | 37.8% |

The 1M-row candidate insert samples have a 35.4% relative spread; that result
needs confirmation. Smaller-fixture insert improvement does **not** meet the
fixed 50% growing-insert target. The continuously growing Criterion fixture is
different from either bounded fixture, and the unchanged Depot result decides
whether that target is met. Filtered-update diagnostics exceed the fixed 25%
target but do not alone clear the release gate.

[Raw scaling samples](2026-10-08-rollback-scaling.json) retain all 40 samples.
Their `working-tree` candidate label describes the measured build; its
production heap code is the candidate commit above. The final example uses an
equivalent parity idiom for strict Clippy. Frozen binary SHA-256s:

```text
control   ed6d9902df2adab1f27ba78e56160f05f2570618daa491dc6602d026b06d6b1c
candidate 0d55eb66ee05214bcaeb6b9fc94b08270ed2ecd2ceaff1329f93045ecb52c725
```

The existing paired driver separately ran five rounds with 100K fixture rows,
500K point reads, 20K writes, 1K scans and 100-row batches. Its protected median
changes were indexed reads +2.14%, changing-value point updates -10.63%,
bounded batches -3.42%, aggregate sum +7.50% and filtered count +4.49%.
No protected median exceeded +10%. Several spread warnings remain; the raw
driver verdict is `diagnostic_pass`, not publication-grade evidence.
[Raw paired-driver results](2026-10-08-rollback-protected.json) preserve the
warnings, correctness/reopen checks and evaluation criteria. Those driver
criteria are not a replacement for this tranche's fixed evaluator.

## Allocation, memory and correctness

A cold one-page Off statement at 1K versus 100K rows allocated 4,999 bytes in
both cases, with zero net retained application-allocator bytes after commit.
The identical test fails against the old engine: 19,075 versus 34,475 bytes
allocated, with 1,880 versus 17,280 net retained bytes. This measures allocator
activity, not process RSS or a hard total-memory bound.

One separate 100K-row/1K changing-update process sample measured maximum RSS
69,599,232 -> 56,934,400 bytes and peak memory footprint
67,961,288 -> 55,247,256 bytes. It is a single sample, not a memory gate or
timing result. It does not establish a platform-independent memory reduction.

New regressions cover stale buckets, bounded churn, overflow reuse/reserve,
earlier buffered commits, auto IDs, multiple tables, selective small-budget
updates/deletes, zero matches, CRC capture failure and rollback-I/O accounting.
Local storage unit tests (266 passed, one existing ignored), the four public
Engine churn cases, the allocator regression, strict workspace Clippy,
formatting, version consistency, testing-feature isolation, CI-needs
completeness and the missing-docs ratchet pass. Independent review approved
the final source with no findings. A frozen final-source, nonroot Linux
workspace build and full test run passed: 2,742 tests, zero failures,
five ignored across 208 test/doc-test targets. The native query testing-feature
suite also passed: 1,485 tests, zero failures, three ignored across 100 targets.
[Hosted CI 37798779012](https://github.com/ZVN-DEV/powdb/actions/runs/37798779012)
passed on `26e8280`, which contains the final production code. Required
`ci-success` includes both OS test/lint jobs, MSRV, Miri shards, ASan,
cross-version compatibility, release-profile corruption suites, package
smokes and the remaining repository guards. Later changes in this tranche
are benchmark evidence only and receive their own fresh CI run.

## Real tradeoffs

- Sparse undo is more complex than cloning vectors. Mutation sites must record
  originals before change; exact-state and churn regressions lock that behavior.
- Each captured page has an individual allocation. Payload remains charged and
  is freed at transaction end; there is no retained uncharged page-image cache.
- A destructive overflow `reserve` operation takes a one-time original-list
  fallback; that uncommon path can still scale with its original free list.
- Rollback still costs proportional to touched state and writes restored pages.
  An I/O failure is still an error, not a successful rollback.

## Unchanged Depot gate

[Run 37798233522](https://github.com/ZVN-DEV/powdb/actions/runs/37798233522)
completed both suites and the comparator. Every same-instance workload passed
its existing regression limit. The fixed >=50% growing-insert and >=25%
filtered-update targets also passed, with no protected workload over +10%.
The combined gate nevertheless **failed the unchanged absolute baseline**;
this document grants no gate exception, baseline reset or release clearance.

Runner `depot-ubuntu-24.04-4`, x86_64, Rust 1.99.0 (`b940084d7`, 2026-09-28),
`RUSTFLAGS=-C target-cpu=x86-64-v2`. Both revisions use the same environment.
These are Criterion medians, not the per-run arithmetic means above.

| Hosted workload | Safe control | Optimized candidate | Lower cost |
|---|---:|---:|---:|
| Growing-table insert | 2,765 ns | 434 ns | 84.29% |
| Filtered update | 2.628 ms | 1.659 ms | 36.88% |
| Indexed update | 2,321 ns | 1,910 ns | 17.70% |
| 1,000-row batch | 266.40 us | 224.21 us | 15.84% |
| Filtered delete | 163.39 us | 136.14 us | 16.68% |

The largest protected read increase is full-row filter +9.93%, just below its
10% limit; it is not an improvement and needs attention in subsequent work.
All four thesis-ratio checks pass, which does not override the absolute
workload failures. Those failures are storage insert-10K +10.80%, full-row
filter +12.86%, top-100 projection +12.96%, growing insert +82.19%, filtered
update +55.34% and filtered delete +34.13% against the recorded baseline.
That historical baseline has a different compiler; do not treat it as an
identical same-instance causal comparison.

[Raw hosted estimates and deltas](2026-10-08-rollback-depot-safe.json) preserve
all 23 workloads, confidence intervals and other statistics. The growing
insert fixture is nonstationary and inserts more rows for the faster engine;
do not advertise its 434 ns median as fixed-table-size insert latency.

A second unchanged run,
[37798935336](https://github.com/ZVN-DEV/powdb/actions/runs/37798935336), compares
the same production code (evidence-only head `26e8280`) with pinned original
release control `9294a9a9bb0c8cda281e9de71655aceb3861aba7`. Both suites completed;
the combined gate failed. Only three same-instance workload limits fail:

| Remaining same-instance failure | Original release control | Candidate | Change | Limit |
|---|---:|---:|---:|---:|
| Growing-table insert | 276 ns | 417 ns | +50.91% | 10% |
| Filtered update | 1.224 ms | 1.446 ms | +18.17% | 10% |
| Filtered delete | 115.58 us | 129.49 us | +12.03% | 10% |

Indexed update is +9.03% (1,824 -> 1,989 ns), within the 10% limit, and batches
are -14.36%. Every read and storage workload passes the same-instance limit.
The separate absolute check still fails top-100 projection, growing insert,
indexed update, filtered update and filtered delete; all four thesis ratios
pass. No check was disabled or threshold changed.
[Raw original-release comparison](2026-10-08-rollback-depot-release.json)
preserves all 23 workloads and estimates.

The two candidate runs differ in timing (for example filtered updates 1.659
versus 1.446 ms); compare each candidate only with its own same-instance
control, not across jobs. Same-instance control reduces cross-machine noise
but does not eliminate run-order, thermal or within-run variability.

The original control does not have the same failed-statement contract. Before
images, budget accounting and undo tracking buy that protection; this tranche
removes demonstrated unnecessary copying. It does **not** prove the residual
cost is unavoidable or assign an exact fraction to each safety mechanism.
Next bounded work should profile the residual single-row snapshot/capture
allocations (about 141 ns/iteration gap in this nonstationary run), then
filtered mutation page-copy work. No correctness rollback or gate waiver is
justified by this record alone.

### Maintainer release disposition — 2026-10-08

After these results were disclosed, the maintainer explicitly requested merging
and releasing the outstanding work. v0.29.0 therefore ships with the known
WAL-Off regressions above documented in its changelog. This is a release
decision, not performance clearance: the failed benchmark verdicts remain
unchanged, residual cost is not proved unavoidable, and correctness CI,
package validation and protected publishing approvals remain mandatory.
The subsequent [final release preflight](2026-10-08-final-release-gate.md)
records the integrated v0.29.0 build: four same-instance write failures remain
and the release warning is updated to that run, without rewriting these earlier
measurements.

```bash
gh workflow run bench.yml --ref codex/powdb-rollback-performance \
  -f control_ref=f70bb7c633b7ba5425f89ab877305477393932d8
```

For local reproduction, build `snapshot_scaling` with identical release flags
in candidate and control trees (copy only that example into the control), then
alternate explicit frozen binaries for five rounds. Do not share stale Cargo
artifacts across archived source trees: preserved source mtimes can reuse the
wrong engine. Use separate targets or force a complete source rebuild. The
unchanged full gate, not this diagnostic, remains authoritative.
