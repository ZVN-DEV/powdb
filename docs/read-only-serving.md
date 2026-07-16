# Read-Only Snapshot Serving

PowDB can open a **quiescent** data directory read-only and serve reads from it,
either through the server (`powdb-server --readonly`) or embedded
(`Database.openReadOnly`). This is tier 1 of the replica story: serve a fixed
snapshot (a restored backup or a checkpointed replica) with no write gate at all,
then swap to the next snapshot when you want fresher data.

A read-only open never mutates a **data file**. Every heap, index, and WAL file
is opened with a read-only descriptor, no permissions are changed, no WAL is
replayed or truncated, and nothing is checkpointed on close. That is what lets
several read-only processes serve the same directory at once.

The one thing a read-only open *does* write is lock **metadata**: on a writable
directory it drops a short-lived PID file under `readers/` (see
[Concurrency and locking](#concurrency-and-locking)) so a read-write open knows
the directory is being served and refuses to start. That file is coordination
state, not database content, and is removed on close; the never-mutate promise
covers your data. On a **non-writable** directory (a `0o555` snapshot mount or a
read-only filesystem) even that metadata cannot be written, so the reader lock
is skipped entirely and the open is byte-for-byte non-mutating (see
[Serving a non-writable directory](#serving-a-non-writable-directory)).

## When to use it

Use read-only serving for **published, mostly-static content**: a CMS-style read
workload, an edge rendering node, an analytics replica. Per-op reads are the
engine's own cost (no write-gate serialization in this mode), the whole database
is one directory, and reloading a new snapshot is a process-level swap.

It is **stale-by-design**: the data you serve is exactly the snapshot you opened,
frozen until you swap directories. That is the point. If you need a live tail,
use the embedded single-process replica (one handle applies retained units and
serves `queryReadonly`); live apply-while-serving in a shared directory is a
deliberate non-goal for this tier.

## The blessed flow: backup to serve

1. **Back up** the primary (offline, or from a replica). See
   [Backup and Restore](./backup-and-restore.md).

   ```bash
   powdb-cli --data-dir ./powdb_data backup ./snapshots/2026-07-15
   ```

2. **Restore** into a fresh directory. `restore` opens and validates the
   snapshot, which guarantees it is WAL-clean (quiescent).

   ```bash
   powdb-cli restore ./snapshots/2026-07-15 ./serve/current
   ```

3. **Serve read-only**, either over the wire:

   ```bash
   powdb-server --readonly --data-dir ./serve/current --port 5433
   ```

   or embedded:

   ```js
   import { Database } from "@zvndev/powdb-embedded";
   const db = Database.openReadOnly("./serve/current");
   const r = db.queryNative("Article filter .published = true { .title }");
   ```

   or embedded in Rust:

   ```rust
   use powdb::Database;
   let db = Database::open_read_only("./serve/current")?;
   let rows = db.query_readonly("count(Article)")?;
   ```

## Refresh with the swap-directory pattern

To publish a newer snapshot without downtime, pull or restore the **next**
snapshot beside the current one, open it, switch traffic, then close the old
directory. The switch is atomic at the process level and needs no engine changes.

```
serve/
  current/   <- readers are here
  next/      <- restore the new snapshot here, open it, then flip
```

1. `powdb-cli restore ./snapshots/2026-07-16 ./serve/next`
2. Start a new read-only server (or open a new embedded handle) on `./serve/next`.
3. Move new traffic to it (load balancer, or swap the embedded handle your app
   holds).
4. Drain and stop the old server / close the old handle, then delete
   `./serve/current` and rename `next` to `current`.

Because each read-only open leaves its directory byte-identical, the old and new
directories never interfere, and a crash of either process (even `kill -9`)
mutates nothing.

## Concurrency and locking

Read-only handles take a **shared reader lock** (a PID file named
`<pid>.<entropy>` under `readers/` in the data directory):

- N read-only processes may serve the same directory concurrently.
- A read-write open (`powdb-server` without `--readonly`, `Database::open`,
  `powdb-cli backup`) **refuses to start** while a live reader is present, and a
  reader refuses to start while a live writer holds the directory. Cross-process
  torn reads stay impossible rather than "unlikely": admission is
  TOCTOU-hardened (each side publishes its own lock file, then re-checks for the
  opposite party and backs off if it appeared), so a reader and a writer are
  never both admitted even under a dead-heat race.
- Crashed readers/writers (dead PIDs) are reclaimed automatically, so a crash
  never wedges the directory. The `<entropy>` in each reader file name keeps a
  recycled PID from ever colliding with a stale file a crashed predecessor left
  behind.

Never run a read-write process against a directory that read-only servers are
using. Serve a **copy** (a restored snapshot), not the primary's live directory.

### Serving a non-writable directory

If the data directory is not writable (a `0o555` snapshot mount, or a read-only
filesystem), the reader lock cannot be written. Rather than fail the open, the
engine falls back to a **lock-less** read-only open and logs a warning:

```
data directory is not writable; reader lock skipped, writer exclusion is not
enforced for this process
```

This is safe: a writer cannot start against a non-writable directory either (its
own `LOCK` write would fail), so there is no writer to exclude in the first
place. The trade-off is only that this process does not *record* a reader lock;
because no writer can appear, nothing is lost. The open is also strictly
non-mutating in this mode: not even the lock metadata is written, so the
directory stays byte-for-byte identical.

## What is refused, and why

A read-only engine has no writer, so it refuses:

- every mutating statement (insert / update / delete / DDL / `begin`), and
- any read that would need a writer.

The error is terminal and names the mode:

```
readonly mode: statement requires a writer (this database was opened read-only
for snapshot serving; refresh materialized views before snapshotting a read-only
directory)
```

### Refresh materialized views before snapshotting

A materialized view that is **dirty** (its base tables changed since the last
refresh) cannot be refreshed read-only, because a refresh is a write. A query
over a stale view is therefore refused rather than silently escalating. This is
the one behavior change from the read-write path that operators must learn once:
**refresh every materialized view on the primary before you take the snapshot you
intend to serve read-only.** After a clean refresh, the view is a normal table in
the snapshot and reads exactly like any other.

## Refusing an unrecovered directory

Read-only serving requires a quiescent directory. If the WAL is not empty (the
directory has un-checkpointed writes, e.g. it was copied from a live or crashed
process rather than restored), the open is refused:

```
cannot open read-only: the WAL is not empty (the directory has un-checkpointed
writes). Open the directory once with a read-write engine to recover, or restore
from a backup, then serve it read-only
```

Recover it once with a read-write open (which replays and truncates the WAL), or
restore a backup, then serve the recovered directory.
