# Unindexed integer membership evaluator

This gate measures native PowQL `IN` and `NOT IN` scalar counts over an unindexed
integer column. It is a paired engine-local read experiment, not a competitor
ranking, SQL-frontend feature, ORM benchmark, or durable-write throughput test.

## Fixed contract

The control is the reviewed v0.29.1 release-candidate engine at
`8508546560f7a280792d6ba37058f71f98e85e53`, not a moving `main`. The identical
evaluator runs against control and candidate, built with matching compiler and
flags on the same machine before any timing starts.

Offline seeded fixtures contain 100K and 1M rows, integer buckets, an ID,
user text, two 128-byte text fields, a score and datetime. No index exists on
the tested bucket. Fixture generation and checkpoint are outside timing and
make no statement-write durability claim. Queries retain Full mode and the
default 256 MiB memory allowance.

Five alternating pairs vary twenty membership keys, with `IN` and `NOT IN`
measured separately on mutable and read-only engines. Timing includes parsing,
lowering and execution, but excludes engine open, EXPLAIN and oracle checks.
Open time, plans, raw samples, exact expected counts and failures are retained.

Acceptance requires exact successful candidate results in every round and
at least 2x lower median latency for every case where controls succeed. Small
100K controls must succeed. Failed large controls remain errors, never a timing
speedup ratio; the frozen control currently succeeds at both sizes.

Separate deterministic allocator tests require less than 1 MiB of total query
allocation and no material growth from 1K to 20K rows. This measures allocator
traffic, not peak RSS or a general query-memory guarantee. Existing workspace,
compatibility, cancellation and broad performance checks remain separate gates.

## Scope

The optimization supports an Int column with at most 128 original integer/null
literal entries. Keys are sorted and deduplicated once. Null tested values
remain false even for `NOT IN`; explicit `not (...)` keeps its existing behavior.
Other types, mixed numeric/string lists, dynamic expressions, qualified fields
and larger lists retain existing paths. Legacy/v1 row checks and the existing
v2 fallback remain in force, as do overflow-table safeguards.

The SQL frontend currently explicitly rejects SQL `IN` lists; this change does
not add syntax or claim SQL membership acceleration. An unindexed query still
visits its input rows. The improvement avoids repeated row decoding and
allocation, not the scan itself.

## Release validation

The [November 6 release preflight](2026-11-06-v0.29.2-preflight.md) retains fresh
100K/1M measurements, raw control/candidate samples, compiler and binary
provenance, and the broader correctness and packaging checks. Ratios from
earlier runs belong to their own hosts and builds; use the dated report when
citing a result. Native read results do not establish competitor rankings or
durable-write throughput.

## Reproduce

Run the **unindexed membership recovery** workflow for retained paired samples,
CPU/RAM/compiler/flag provenance and binary/evaluator hashes. It runs on matching
pull requests and supports manual dispatch once merged.

For local measurements, build the evaluator against the immutable control,
retain that binary outside the candidate target directory, then run:

```sh
cargo build --release --locked -p powdb-bench --bin unindexed-membership-gate
target/release/unindexed-membership-gate \
  --baseline-bin /absolute/path/to/frozen-control --rows 1000000 --repetitions 5
```

The driver refuses missing/identical baselines, invalid bounds, unsafe fixture
reuse, mismatched fixture markers and invalid measurement records. Do not run
compilation or other workloads concurrently with timing.
