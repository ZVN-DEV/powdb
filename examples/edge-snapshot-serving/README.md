# Edge snapshot serving

A runnable, self-contained worked example of PowDB's **blessed read-scaling
pattern**: back up a writer primary, restore the snapshot into a serving
directory, and serve it from N read-only `powdb-server` processes. Refresh by
restoring a newer snapshot beside the current one and swapping it in.

This is tier 1 of the replica story. It scales reads horizontally without an
MVCC engine and without a live replication protocol, because every read-only
process serves a **frozen, quiescent directory** and takes no write-admission
gate at all. For the engine-level contract behind this pattern, see
[`docs/read-only-serving.md`](../../docs/read-only-serving.md) and
[`docs/backup-and-restore.md`](../../docs/backup-and-restore.md).

## Run it

```bash
./run.sh
```

The script builds `powdb-cli` and `powdb-server` (release), then runs the whole
cycle against loopback ports 5602/5603, printing `PASS`/`FAIL` for every checked
step and exiting nonzero if any check fails. All scratch data lives in a
gitignored `.work/` directory beside the script. To skip the build and use
prebuilt binaries:

```bash
POWDB_BIN=/path/to/target/release ./run.sh
```

There is **no separate `powdb-backup` binary**: backup and restore are
subcommands of `powdb-cli` (`powdb-cli --data-dir <DIR> backup <DEST>` and
`powdb-cli restore <SRC> <DEST>`). The script uses those.

## The story, step by step

```
                    powdb-cli backup            powdb-cli restore
  ┌───────────┐    (offline snapshot)   ┌────────────┐   ┌──────────────────┐
  │  primary  │ ─────────────────────▶  │ snapshots/ │──▶│  serve/current   │
  │ (writer)  │                         │  1-full    │   │ (quiescent copy) │
  └───────────┘                         └────────────┘   └──────────────────┘
        │                                                    ▲          ▲
        │ writes keep advancing                              │          │
        ▼                                          powdb-server    powdb-server
  ┌───────────┐  backup --base   ┌──────────────┐   --readonly      --readonly
  │  primary  │ ───────────────▶ │ snapshots/   │    :5602            :5603
  │ (6 rows)  │  (incremental)   │ 2-increment  │
  └───────────┘                  └──────┬───────┘
                                        │ restore --apply
                                        ▼
                                 ┌──────────────┐   rename next -> current,
                                 │  serve/next  │   restart readers on it
                                 └──────────────┘   (the atomic swap)
```

1. **Writer primary.** A single read-write owner of `./primary`. In this demo it
   is the embedded `powdb-cli`; in production it is your app or a `powdb-server`
   running without `--readonly`. It creates an `Article` table and seeds rows.
2. **Back up.** `powdb-cli --data-dir ./primary backup ./snapshots/1-full`
   checkpoints the directory (truncating the WAL) and copies the durable,
   blake3-verified files. Backup is **offline**: nothing else may hold `./primary`
   open while it runs. Here the embedded writer has already exited, so the
   directory is quiescent.
3. **Restore into a serving directory.**
   `powdb-cli restore ./snapshots/1-full ./serve/current` opens and validates the
   snapshot, which guarantees it is WAL-clean and therefore safe to serve.
4. **Serve read-only.** Two `powdb-server --readonly --data-dir ./serve/current`
   processes on ports 5602 and 5603. A read-only open never mutates a data file
   and takes only a shared reader lock, so N of them serve the same directory at
   once.
5. **Verify reads and write refusal.** Both servers return the seeded data; a
   point read by unique `id` goes through the index. Any mutating statement
   (`insert`, `delete`, ...) is refused with a terminal error and a nonzero exit,
   because a read-only engine has no writer.
6. **Writer advances.** The primary takes more writes. The read-only servers keep
   serving the **frozen** snapshot (still the old row count): this staleness is
   by design.
7. **Incremental backup + restore beside.**
   `backup ./snapshots/2-increment --base ./snapshots/1-full` captures only the
   4 KB pages that changed. `restore ./snapshots/1-full ./serve/next --apply
   ./snapshots/2-increment` chain-restores the full base plus the increment into
   a **new** directory next to the current one.
8. **Atomic swap.** Drain and stop the readers on the old directory, `mv` the new
   directory into place, and restart the readers on it. The swap is atomic at the
   process level; because each read-only open leaves its directory byte-identical,
   the old and new directories never interfere. The refreshed readers now serve
   the fresh row count.

## The freshness contract

- **Stale-by-design.** Each read-only process serves exactly the snapshot it
  opened, frozen until you publish a new one and swap. There is no live tail.
- **Freshness is your snapshot cadence.** How current the edge is equals how
  often you run backup -> restore -> swap. The mechanical cost of one cycle is
  small: a backup is a checkpoint plus a file copy, a restore is a verified copy
  plus one validating open, and the swap is a directory rename. On a small
  database on a laptop this whole cycle runs in **on the order of a second**
  (roughly ~1.6s at this scale in local runs); it grows with database size
  because backup and restore copy the changed bytes. Incremental backups keep
  the copied bytes proportional to what changed, not to the whole database.
- **Refresh materialized views before you snapshot.** A materialized view that
  is dirty cannot be refreshed on a read-only server (a refresh is a write), so a
  read over a stale view is refused rather than silently escalating. Refresh
  every materialized view on the primary before taking the snapshot you intend to
  serve. See [`docs/read-only-serving.md`](../../docs/read-only-serving.md#refresh-materialized-views-before-snapshotting).

## What this pattern is NOT

- **Not live replication.** There is no streaming applier and no shared-directory
  tail. Readers do not converge on the primary between swaps; they jump from one
  snapshot to the next. If you need a live-ish tail in a single process, that is
  the embedded single-process replica, a different tier: not this one.
- **Not a way to write through the edge.** Read-only servers refuse every
  mutation. All writes go to the primary; the edge only serves.
- **Not multi-writer or MVCC.** This pattern exists precisely because PowDB is
  single-writer with no MVCC. It scales the read side by making many independent,
  frozen copies, not by making one directory safe for concurrent read-write use.
  For many concurrent clients over one shared read-write database, use Postgres.
- **Not online backup.** Backup is offline; do not run it against a directory a
  live server currently has open. Snapshot the primary while it is quiesced (or
  snapshot a directory nothing else is using), then serve the restored copy.

## Files

| File          | What it is                                                    |
| ------------- | ------------------------------------------------------------- |
| `run.sh`      | The end-to-end demo. Self-contained; prints PASS/FAIL.        |
| `.gitignore`  | Ignores the `.work/` scratch directory the demo creates.      |
