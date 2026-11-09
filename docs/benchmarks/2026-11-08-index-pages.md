# Bounded index-page diagnostic: 2026-11-08

An ascending page over a matching unique integer index now stops at its requested rows. The prior path collected all matching index keys, fetched their rows and sorted them before applying the limit. This diagnostic measures that mechanism against unchanged main, not an overall database ranking or an isolated-host speed claim.

## Scope

```powql
Page filter .id >= 0 order .id limit 100 { .id, .payload }
```

The initial optimization handles one ascending unique integer key, a literal limit, and an optional plain-field projection. Numeric bounds may be inclusive, exclusive or one-sided. Unfiltered keys must be required because unique indexes omit nulls. Offsets, residual predicates, other orders/types and missing/non-unique indexes retain their existing paths. Computed projections remain in the normal projection evaluator, whose bounded ordered input may independently qualify.

The storage visitor borrows keys, seeks to the lower bound and stops without allocating a full match list. It checks cancellation on every tree node, including empty leaves. Table row fetches reconstruct overflow values. Query memory admission charges fetched/projected rows as they are produced; the result is still materialized, not a streaming cursor.

## Provenance and method

- Control production code: `a3a83225653d42e73a1ee1ba2b620769f832245b` (main).
- Candidate production code: `e9f924d`; the same diagnostic driver is added to the control tooling only.
- Separate build directories, optimized default-feature builds, no test instrumentation; Rust 1.99.1 and no rustflags override.
- Shared Fjord host: AMD RYZEN AI MAX+ 395 w/ Radeon 8060S. Each measured process was pinned to CPU 8; no core reservation, desktop shutdown or exclusive hardware was used.
- Five alternating control/candidate pairs. Each binary loads fresh 1K/10K/100K fixtures, checkpoints them, warms each case three times, then records seven calls per case/surface. Both binaries are built before measurement.
- Each result is checked for columns, all 100 ordered IDs and every payload. Summary cells are medians of five per-run medians. Timed execution includes result materialization; verification is outside timing.
- `all_qualify` starts at zero; `last_100` seeks to the final 100 keys. Output cardinality is fixed. No competing database or durable write is timed.

## Observations

![Fixed-output page latency versus qualifying keys](2026-11-08-index-pages.png)

All values are microseconds. These are local diagnostic medians; shared-host variance remains visible in the raw samples.

| Fixture rows | Surface | All qualify: control | All qualify: candidate | Last 100: control | Last 100: candidate |
| --- | --- | ---: | ---: | ---: | ---: |
| 1,000 | Mutable | 766.546 | 50.435 | 93.265 | 58.871 |
| 1,000 | Read-only | 889.476 | 52.949 | 96.420 | 59.141 |
| 10,000 | Mutable | 7,159.972 | 45.335 | 59.281 | 56.917 |
| 10,000 | Read-only | 6,552.623 | 42.850 | 62.718 | 63.709 |
| 100,000 | Mutable | 74,206.769 | 42.780 | 60.112 | 61.425 |
| 100,000 | Read-only | 72,055.329 | 44.994 | 61.345 | 59.492 |

The candidate removes growth with qualifying range cardinality in this fixed-output probe. Cases that already qualified only 100 keys stay in roughly the same latency range; the samples do not establish a consistent improvement for those cases. This does not establish scaling beyond the recorded fixtures, behavior under concurrent writers, or superiority to another database.

Raw observations, binary hashes, host/compiler details and pair order: [JSON evidence](2026-11-08-index-pages.json). SHA-256 of the JSON: `7c12f9e2af25e8e736a3c5ad1d2ecefe141d5685d3a5c963e930e8f5e76b3628`.

## Correctness and verification

- Tests prove a 17-row eligible page delivers 17 index entries and attempts 17 heap fetches on mutable and read-only execution, with no sequential row visits. Counters are absent from default production builds.
- Fast/forced-generic parity covers inclusive/exclusive bounds, empty and reversed ranges, integer extremes, nullable keys, aliased output columns, overflow rows, deletes/key updates, restart, zero/huge limits, and unsupported shapes.
- Low-budget tests accept small pages and reject large pages with typed memory-limit errors. Cancellation tests stop during traversal and across empty leaves.
- The existing cross-product access-path equivalence corpus includes the new path. The full local quality sweep passes (workspace tests/doc tests, strict Clippy, TypeScript live tests and package/import smokes); cargo-audit and gitleaks are unavailable locally and remain CI checks.
- Regression tests also cover a pre-existing projection/limit literal-cache ordering bug discovered during parity checks. Ambiguous cache shapes now replan.
- Broad Criterion regression validation is recorded separately when completed. No baseline or threshold is changed.

## Reproduce

```bash
cargo run --release -p powdb-bench --bin index-page-probe > index-pages.json
# Optional: POWDB_INDEX_PAGE_SIZES=1000,10000,100000
cargo test -p powdb-query --test bounded_index_page --test plan_cache_literal_order
cargo test -p powdb-query --test access_path_equivalence
cargo test -p powdb-query --lib bounded_index_page
cargo test -p powdb-storage --lib visitor_
```

Use separate build directories for control/candidate binaries. Record source revisions, flags and binary hashes; retain samples rather than selecting the fastest run.
