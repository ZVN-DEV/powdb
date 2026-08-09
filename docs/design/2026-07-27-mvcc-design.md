# MVCC design proposal: Frame-Versioned Snapshots (options and a recommendation)

**Status:** proposed, pending owner decisions
**Date:** 2026-07-27
**Baseline:** v0.21.0
**Relationship to the roadmap:** refines the MVCC sketch in
`docs/design/2026-07-26-road-to-1.0.md` (0.28.0-0.30.0); if adopted, that
document must be updated in the same change.

## The recommendation

**Recommendation: single writer. Multi-writer refused permanently, at 1.0 and
after.**

This is a recommendation, not a settled decision: MVCC scope is open question 1
of `2026-07-26-road-to-1.0.md`, and Open Question 1 below puts the same question
to the owner. Everything downstream is written as though the recommendation were
accepted, because the design cannot be evaluated otherwise.

One write transaction at a time, database-wide. This is not a staging decision to
revisit later: it is the assumption the entire design is bought with. Multi-writer
would require per-row versions, a lock manager, deadlock detection, and write-write
conflict detection, none of which exist here and none of which this work is a step
toward. That would be a major version, not a 1.0 migration event.

The payoff for accepting that refusal is the whole thesis:

> Once writers are serialized, snapshot readers need no row versions at all.

Two versions of a row only need to coexist if two transactions can concurrently
write it. With one writer, a reader needs a stable *physical* image of the pages it
touches, and PowDB already keeps exactly that in memory for other reasons
(`hot_page` + `dirty_buffer`, `heap.rs:236-249`, with a read-precedence chain that
already prefers memory over the file at four call sites).

**Chosen design: Frame-Versioned Snapshots (FVS).** Committed page images, versioned,
living inside the 0.27.0 buffer pool frame. Version chains are a field on the frame.

No row visibility fields. No row format v3. No undo log. No before-images in the WAL.
No new WAL record type. No separate reader engine. No copy-on-write B+tree arena.

## Why page-level and not row-level

Three designs competed: a committed page overlay, row-versioned MVCC with undo
chains, and a staged path. Three judges scored them under different lenses.

| Lens | Winner |
|---|---|
| Correctness and provability | Page overlay |
| Performance and read scaling | Epoch snapshots (page overlay placed in the buffer pool) |
| Migration and risk | Page overlay |

Effectively 3-0 for page-granularity over row versions. The performance judge's
preference differed only on *where the overlay lives* and *when to escalate*, both of
which are grafts rather than a different design, and both were taken.

**The decisive finding, verified in code.** Row-level MVCC multiplies the visibility
predicate across roughly eight byte-level row producers, including two `unsafe`
raw-pointer loops whose SAFETY argument is written in terms of body offsets, *while
simultaneously* changing the prefix length those sites compute. There are five prefix
sites, and the fifth is a trap:

```rust
// crates/storage/src/row.rs:120-126
pub fn row_is_v2(data: &[u8]) -> bool {
    data.len() >= ROW_PREFIX_SIZE
        && &data[0..4] == ROW_MAGIC
        && u16::from_le_bytes([data[4], data[5]]) == ROW_FORMAT_VERSION_V2
}
```

That is an **exact match**. A v3 row returns `false`, routes to the v0/v1 zero-copy
compiled path, computes `base = ROW_PREFIX_SIZE = 6`, and reinterprets the visibility
fields as the body's `total_size` and null bitmap.

It does not error. It returns **plausible wrong values** on the compiled-predicate and
prepared fast paths, which are the paths the benchmarks exercise. Introducing row
version v3 without unifying all five prefix sites first is the exact compound of the
two failure modes this project has bled from across three releases.

Page-level visibility has nothing to diverge: the decision is made one level *below*
the row decoder, on whole 4096-byte page bytes. The compiled-predicate leaves, both
`unsafe` aggregate loops, every `FastPatch.field_off`, all five prefix sites,
`iter_page_slots` and `slot_bytes_from_page` are untouched. **One implementation of
the visibility rule instead of eight.**

## The isolation guarantee

**Name: one-copy serializable (1SR).** Not strict serializability. Three modes, each
individually nameable, because a level users cannot name is a level we cannot support.

| Mode | Guarantee |
|---|---|
| Autocommit statement (default) | One snapshot at statement start. Snapshot-consistent *within* the statement, which is not true today. Across statements a session sees **read committed**, which is what autocommit scope means. |
| `begin` (read-write) | **Serializable.** Holds the single write slot for its duration. Snapshot taken at write-slot acquisition, not at BEGIN. |
| `begin read only` | **Snapshot isolation locally, serializable globally.** Reads a consistent prefix of the serial writer history. |

Taking the read-write snapshot at *slot acquisition* rather than at BEGIN is a graft
from the losing row-MVCC design, and it is what keeps lost update and write skew
unreachable when a writer waits for the slot.

**Structurally excluded**, by construction and not by detection (no SSI, no dependency
graph, no predicate or gap locks, no serialization-failure retry): dirty read,
non-repeatable read within a snapshot, phantom within a snapshot, read skew within a
snapshot, write skew, lost update between transactions, the Fekete read-only anomaly,
and transaction-id wraparound (there are no persisted ids at all).

### Anomalies explicitly permitted

Each of these will go in `docs/ISOLATION.md`, which this work will create.
Absence must be a documented decision.

1. **Staleness.** A `begin read only` transaction serializes at its snapshot and may
   not observe a commit another session was already acknowledged for. Same property as
   Postgres REPEATABLE READ and SQLite WAL readers. Not permitted for a single
   autocommit read, which always takes the newest published version.
2. **Cross-statement phantoms and non-repeatable reads in autocommit.** Transaction
   scope, not an isolation defect, but users read it as one, so it is named.
3. **Application-level lost update across two autocommit statements.** No
   `SELECT FOR UPDATE`, no row lock, no advisory lock, and none planned. The supported
   answer is `begin ... commit`, which is exclusive and therefore *is* the lock
   manager. Documented with a worked example.
4. **Visibility can precede durability under `Normal` and `Off`.** A reader can observe
   a write a subsequent OS crash erases, within the 10 ms `NORMAL_FSYNC_INTERVAL`.
   Exactly Postgres `synchronous_commit=off`.
5. **Cross-session read-your-writes is not guaranteed for autocommit writes.** The
   TxGate permit is released *before* the durability ticket is awaited
   (`handler.rs:2114-2118`, `:2187`), so a reader admitted in that window may take a
   snapshot omitting a just-acknowledged write. Legal (a consistent prefix) but a
   genuinely new observable: today the write lock makes it impossible.
6. **`SnapshotTooOld`.** A snapshot is a budgeted resource. Past the budget the oldest
   is invalidated and its next read returns a typed error, never a partially advanced
   view.
7. **`DdlBlockedByReaders`** and DDL-driven snapshot invalidation. A long report can be
   killed by an `alter`, and the error says so.
8. **Snapshots are process-local.** `Engine::open_read_only`, and therefore
   `powdb-backup`, sees the last checkpoint plus WAL replay, not the live overlay.
   SQLite WAL gives cross-process readers through shared memory; we deliberately do not.
9. **DDL is outside the serial order.** Non-transactional, refused inside a
   transaction, and a bounded reader barrier.

### The honesty caveat

This must appear in the first paragraph of the release notes, not buried:

> This work buys read concurrency and correctness. **Write throughput does not improve
> by one percent.**

## Release plan

| Release | Goal |
|---|---|
| 0.22.0 (in progress) | Add the pre-work that is cheap now and catastrophic later, and fix the read baseline so every later MVCC number is measured against a sane one |
| 0.23.0 | Catalog v8 + BTREE v4 as a single coordinated migration event. Spend the one event, spend it once |
| 0.25.0 | Schema evolution must be reachable by the 0.30.0 DDL barrier. No new MVCC mechanism |
| 0.26.0 | Cursors carry a snapshot from day one |
| 0.27.0 | One page cache, multi-version-capable frames, one visibility choke point, Send/Sync argument re-derived **before** anything depends on it |
| 0.28.0 | Session-scoped transactions, layered private tier, O(transaction) ROLLBACK, **statement atomicity**, bounded WAL |
| 0.29.0 | Version chains active, index visibility correct, concurrency proof machinery |
| 0.30.0 | An open transaction stops excluding readers. One named tested model for server and embedded |
| 0.31.0 | Publish the ceilings, harden the concurrency argument. No new mechanism |
| 1.0.0 | The refusals become versioned, tested guarantees |

**Migration budget: one event, not two.** Row format v3 is retired unspent. The
roadmap's Event 2 is cancelled, and catalog v8's `mvcc_enabled` and `next_tx_id_hwm`
sections become zero-written insurance rather than load-bearing.

**Statement atomicity lands in 0.28.0**, which is what finally closes the shipped
known limitation where an `update` can durably store NULL in a `required` column.

## Notable refusals

Beyond multi-writer:

- Lock manager, deadlock detection, row/gap/next-key locks, `SELECT FOR UPDATE`,
  `NOWAIT`, `SKIP LOCKED`, advisory locks, serialization-failure retry
- SSI and dependency-graph tracking (write skew needs two concurrent read-write
  transactions, which cannot occur)
- Lock promotion. A transaction declares read-only or read-write at BEGIN; a write
  inside `begin read only` is a typed error, never a silent upgrade
- Row-level versioning, and therefore row format v3
- An undo log, before-images in the WAL, compensation log records, any new WAL record
  type. The abort path is "drop the page images", not "walk a chain of inverses"
- Time travel, `AS OF`, named snapshots, durable snapshots surviving restart. Exposing
  them makes the GC horizon user-controlled and therefore unbounded
- Unbounded snapshot lifetime. There is no configuration in which memory grows without
  a typed error
- Cross-process snapshots
- Transactional DDL. DDL's destructive steps are file unlinks and a catalog rename, not
  page mutations, so shadow paging cannot roll them back
- Index-only scans. Every probe under a snapshot must recheck against the snapshot's
  heap page, because the tree is a superset of the snapshot's entry set
- **A copy-on-write B+tree arena with a free list**, refused against the performance
  judge's own proposal: it is the only mechanism across all three designs whose failure
  mode has no detector
- **A unique-key pre-image side map**, refused against the winning design's own
  proposal. Bespoke index side-channels are precisely where this project's wrong
  answers have come from
- Paged B+trees, **conditionally**: honest only if the no-ratchet test passes and arena
  compaction lands. The ceiling gets published either way
- User-visible savepoints, as surface area rather than mechanism
- Reducing page-granularity version cost. One changed 46-byte row pins a 4096-byte
  image; the amplification gets published rather than hidden

## How this gets proven

Tied directly to the 0.22.0 assurance machinery being built now:

- **`crates/oracle` gains a snapshot dimension.** It already runs each case three ways
  against SQLite; add snapshot reads to the comparison.
- **`access_path_equivalence.rs` gains a third dimension** beyond catalog state and
  physical path.
- **A new concurrency/serializability oracle**, because neither existing instrument is
  concurrent. Randomized concurrent workload, recorded history, checked for a valid
  serial order.
- **A `visible_page` bypass gate.** The build fails if any row producer obtains page
  bytes outside the choke point, with a **negative test proving the gate itself fires**.
- **The per-heap durable watermark assertion**, checked at every disk fall-through and
  re-asserted after recovery.
- **The Postgres PG-14 regression test**: an invalidated snapshot returns
  `SnapshotTooOld` and never rows from a partially advanced view.
- **The no-ratchet test**: N update cycles on an indexed column with no long reader must
  return `BTree::stats().total_entries` to its starting value.
- **A loud-failure test for the bit-15 delete mark**: code that fails to mask it must
  produce an out-of-range slot index and abort, not a wrong row.
- **Cross-version compat CI gains three legs**, including that an FVS-written directory
  with `mvcc_enabled` never set opens byte-identically on the last pre-MVCC release.
- **loom and miri moved earlier and made real.** Loom covers pin/latch/eviction ordering
  as a 0.27.0 exit criterion, where the ownership model changes, not in the final release.
- **Crash tests with a non-empty overlay.**
- **Published benchmark numbers as exit criteria, never adjectives.** Depot only, per the
  standing rebaseline policy.

## Open questions for the owner

1. **Is the multi-writer refusal genuinely permanent?** Everything rests on it, and
   unlike the other refusals it is not a step toward anything. If multi-writer might be
   wanted within two years, this plan is the wrong plan.
2. **Is "read scaling and correctness, zero write throughput improvement" worth four
   dedicated releases?** If the complaint actually driving MVCC is concurrent *write*
   scaling, none of the three designs addresses it.
3. **Page-granularity amplification against real workload shape.** One 60 s reader
   against 2,000 scattered single-row commits/s retains roughly 480 MB and then gets
   killed. That figure is arithmetic (2,000 x 4,096 x 60), not a workload
   measurement. Is that the shape of the workloads we care about?
4. **Snapshot-pressure default: invalidate-oldest or block-writer?** I chose
   invalidate-oldest, because refusing the writer turns a long `SELECT` into a write
   denial of service.
5. **What composite unique-key regression is unacceptable?** `lookup_int` is today an
   allocation-free `binary_search_by`; the composite form adds work to the hot path.
6. **Does `powdb-sync`'s apply path route through the single writer slot?** The
   serializability claim rests on that slot being exclusive across *every* mutation
   path, and nothing in the type system enforces it today.
7. **Should the mmap hoist ship in 0.22.0 standalone?** The server never calls
   `enable_mmap`, so every scan pays a fresh mmap plus a full page-fault walk plus
   munmap, informally reported at roughly 37x single-threaded overhead. That number
   is an external observation, not reproduced in this repo's bench suite, so treat it
   as a reason to measure rather than as a measurement. Fixing it first means later
   MVCC numbers are measured against a sane baseline.
8. **Keep catalog v8's `mvcc_enabled` and `next_tx_id_hwm` reserved at all,** given FVS
   retires row versions? Recommend yes, zero-written, as cheap insurance.

## Method

Two readers mapped the transaction/WAL/recovery and row/index/catalog internals against
the actual code. Three designs were then produced independently, scored by three judges
under different lenses, and synthesized with the losers' best ideas grafted in. Claims
about current behavior were spot-verified against the code before being recorded here;
the `row_is_v2` exact-match trap was confirmed firsthand.
