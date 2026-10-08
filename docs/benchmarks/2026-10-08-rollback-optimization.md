# Statement rollback optimization — 2026-10-08

**Status: local diagnostic improvement, not release performance clearance.**
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
Rust 1.95 workspace build and full test run passed: 2,742 tests, zero failures,
five ignored across 208 test/doc-test targets. Fresh hosted CI and the native
query testing-feature suite are still running.

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
compares the candidate with the safe engine above on the same instance. It
also runs the unchanged absolute baseline check. Its verdict is pending;
this document grants no gate exception, baseline reset or release clearance.

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
