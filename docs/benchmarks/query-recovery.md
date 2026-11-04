# Query recovery evaluator

This evaluator covers two engine-local read workloads against released PowDB
0.29.0 (`7cc4b548ac94d9379aa362c3121314b8c2479039`). It is not a database
leaderboard, a write-throughput test, or serverless/ORM qualification.

## Workloads and acceptance

A deterministic million-row `Comment` table contains integer IDs, non-unique
note IDs, user IDs, a 128-byte body, integer scores, float ratings, and datetime
microseconds. The ID and note ID columns have unique and non-unique indexes,
respectively. Fixture creation uses offline table insertion and checkpointing;
its time is excluded. No WAL-Off mode or increased memory limit is used.

The same evaluator source runs against control and candidate engines. Five
alternating paired rounds vary the 20 membership keys and exercise mutable and
read-only engines separately. Each sample opens its engine and obtains EXPLAIN
before query timing. Open time is retained separately; results are checked
outside the timed region. These are process-local query latencies with a shared
OS page cache, not cold-storage or application end-to-end latencies.

| Workload | Required result |
| --- | --- |
| `IN` lookup and ID projection | Exact matching IDs, indexed candidate plan, at least 10x lower median query latency than the fixed control |
| Constant-group COUNT/SUM/AVG/MAX | Exact values at the unchanged 256 MiB query limit; control errors remain in the evidence |

This gate deliberately addresses a bounded recovery slice. Passing it does not
replace the existing benchmark regression suite, workspace CI, or release gates.

## Reproduce

Run the **query recovery** Actions workflow. It uses free GitHub-hosted Linux,
builds both revisions with the same compiler/flags before timing, and retains
raw JSON plus commits, compiler version, architecture and SHA-256 provenance.
Evaluator/workflow pull requests also run this check automatically. The default
control remains the released revision above; changing it changes the comparison.

For local runs, first build the identical evaluator against that control and
retain its binary outside the candidate's target directory. Then run:

```sh
POWDB_QUERY_RECOVERY_BASELINE=/absolute/path/to/frozen-control \
  cargo run --release --locked -p powdb-bench --bin query-recovery-gate -- \
  --rows 1000000 --repetitions 5
```

The evaluator refuses identical binaries and fewer than five paired rounds.
Both sides must use matching compiler, target flags and resources. Do not time
while other compilation or workload jobs compete for the same machine.

## Optimizations and boundaries

Selective literal or bound-parameter membership can use a balanced union of
existing index probes. Coerced duplicate keys are removed and the complete
original predicate is rechecked. Broad lists, unsupported types/scopes, overflow
tables, low memory allowances and mutation inputs retain existing paths. The
memory estimate is conservative eligibility, not complete whole-query peak-memory
accounting. Unordered results have no new ordering guarantee; explicit ordering
retains its semantics. There is no disk-format change.

Eligible constant-integer-group aggregates fold fixed-width columns directly
into accumulators instead of retaining input rows or decoding unused inline
strings. The same implementation serves mutable and read-only execution.
Overflow tables, filtered/grouped-expression inputs, DISTINCT, join provenance
and variable-width aggregate arguments remain on existing paths. Dirty views
still refresh or require writable escalation. Numeric overflow, NULL, empty
input, HAVING and cancellation behavior have regression coverage.

Separate allocator tests measure total successful application allocation and
reallocation bytes during eligible queries at 1K and 20K rows. They assert less
than 1 MiB and no material row-count growth; this is not an RSS measurement or
a claim about every aggregate. Test-feature-only work counters record selected
scan visits, index probes and heap fetches. They are partial diagnostics, not
general EXPLAIN ANALYZE or storage-I/O counters, and compile out of default builds.
