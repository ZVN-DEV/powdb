# Backup and Restore

PowDB takes a crash-consistent, blake3-verified full snapshot of a database directory and rebuilds a fresh one from it. Use it to capture a recoverable copy of your data before an upgrade, a risky migration, or on a regular schedule.

The baseline is a **full snapshot**: the whole database, all tables, in one operation. On top of that, PowDB supports **incremental (differential) backups** and **coarse point-in-time restore** by chaining a full base with one or more increments (see [Incremental backup & point-in-time restore](#incremental-backup--point-in-time-restore)). Fine-grained (sub-increment) PITR and cloud sync are planned for later phases (see [Limitations](#limitations)).

---

## Commands

Two CLI subcommands handle both directions.

### Back up

`backup` snapshots the database in `--data-dir` into a destination directory:

```bash
powdb-cli --data-dir ./powdb_data backup ./backups/2026-06-06
```

Output:

```
backed up 3 files (49152 bytes) at lsn 128 -> ./backups/2026-06-06
```

> **Back up offline.** `backup` opens the data directory with its own catalog handle and checkpoints it (which truncates the shared `wal.log`). It does **not** coordinate with any other process holding the same directory. Do **not** run `backup` against a `--data-dir` that a live `powdb-server` (or another CLI) currently has open — concurrent access can corrupt **both** the snapshot and the live database. Stop the server first (or snapshot a directory nothing else is using). Online, serve-while-backing-up snapshots are a planned later phase (see [Limitations](#limitations)).

### Restore

`restore` rebuilds a fresh data directory from a backup:

```bash
powdb-cli restore ./backups/2026-06-06 ./restored_data
```

Output:

```
restored backup ./backups/2026-06-06 -> ./restored_data
```

The destination must be a fresh or empty directory. Restore does not merge into a live database — see [Limitations](#limitations).

---

## Incremental backup & point-in-time restore

A full snapshot copies every file every time. For a database that changes a little between backups, that is wasteful. PowDB's **incremental backup** captures only the 4 KB pages that changed since a chosen full backup.

### The differential model

The CLI uses a **differential-since-full** model: every incremental is diffed against the same **full base**, not against the previous increment. Each incremental compares the current page LSNs against the base's `source_lsn` and stores only pages whose `page_lsn` is newer. Whole files that can't be paged (the catalog) are copied in full when they change.

Because every increment is independent of the others, you can keep a series of increments against one full base and restore *any one of them* on top of the base — no intermediate increment is required. (The underlying `restore_chain` can also apply several increments in order; the differential model just means you usually only need one.)

### Take an incremental backup

Pass `--base <FULL_BACKUP_DIR>` to `backup`:

```bash
# 1. Full base, e.g. nightly
powdb-cli --data-dir ./powdb_data backup ./backups/full

# 2. Later, an incremental of just what changed since that full
powdb-cli --data-dir ./powdb_data backup ./backups/inc-1 --base ./backups/full
```

Output:

```
incremental backup: 2 changed files (1 whole, 1 paged), 7 delta pages, base lsn 128 -> lsn 240 -> ./backups/inc-1
```

An incremental directory holds an `increment.json` manifest plus a `<name>.delta` sidecar for each paged file (each delta packs the changed pages: a 4-byte page index followed by the 4 KB page, repeated). The catalog is copied whole when it changed.

### Chain restore (coarse PITR)

Restore the full base and apply the increment(s) with `--apply` (repeatable, applied in the order given):

```bash
# Restore the full base plus one increment
powdb-cli restore ./backups/full ./restored --apply ./backups/inc-1
```

Output:

```
restored backup ./backups/full + 1 increment(s) -> ./restored
  applied ./backups/inc-1
```

**Coarse PITR.** Keep periodic increments (say one per hour) against your full base. To recover to a chosen point in time, restore the full base and apply the increment(s) up to the target point. The granularity is the increment cadence — restore lands you at the state captured by the last increment you apply, not at an arbitrary instant within it.

```bash
# Restore to the state at inc-2's capture time
powdb-cli restore ./backups/full ./restored --apply ./backups/inc-1 --apply ./backups/inc-2
```

### Chain verification

A chain restore is checked before it writes a usable database:

- **LSN continuity.** Each increment records the `source_lsn` of the base it was built on. The chain is rejected unless every increment's recorded base LSN equals the running LSN of the base-plus-applied-increments so far — you can't accidentally apply an increment to the wrong base or in the wrong order.
- **blake3 per delta.** Every delta sidecar (and every whole-file copy) is hashed with blake3 and compared against the manifest before it is applied.
- **Open-to-validate.** As with a full restore, the rebuilt directory is opened through the catalog at the end, so post-restore writes survive a subsequent crash.

---

## What's in a backup

A backup directory contains a copy of the database's durable files plus a manifest:

| File | Contents |
|---|---|
| `catalog.bin` | Schema registry (tables, columns, indexes). |
| `*.heap` | One heap file per table — the row data. |
| `*.idx` | One index file per B+tree index. |
| `manifest.json` | Integrity record (see below). |

The active write-ahead log (`wal.log`) is **intentionally not copied**. Backup checkpoints first, which truncates the WAL, so the copied files already reflect every committed write.

The `manifest.json` records, for each copied file, its name, byte length, and blake3 hash. It also records a `source_lsn`: the page-LSN high-water mark the snapshot is consistent at. This is the log sequence number through which the backup's data is guaranteed durable — the same number printed by `backup` (`at lsn 128` above).

---

## Guarantees

**Crash-consistent.** Backup calls the engine's `checkpoint()`, which flushes every dirty heap page and index to disk and truncates the WAL. The result is a clean-shutdown on-disk image — there is no partial or in-flight write to replay. Backup then copies those durable files. The checkpoint holds an exclusive borrow on the catalog, so writes are briefly quiesced while it runs.

**Integrity-verified.** Every copied file is hashed with blake3 at backup time, and the hash and size are stored in `manifest.json`. On restore, PowDB re-hashes each file and compares it against the manifest **before writing it to the destination**. A tampered or corrupted backup fails with an integrity error and writes nothing usable:

```
Error: restore failed: integrity check failed for users.heap: blake3 mismatch (backup is corrupt)
```

**Restore validates by opening the database.** After writing the verified files, restore opens the rebuilt directory through the catalog. Opening resets the engine's LSN counter above the restored data's high-water mark, so writes made *after* a restore correctly survive a subsequent crash. This is covered by a regression test (`crates/backup/tests/restore_durability.rs`). If the restored directory cannot be opened, restore reports an error.

---

## Round-trip

A complete capture-and-recover cycle:

```bash
# 1. You have a database with some data
powdb-cli --data-dir ./powdb_data
#   powql> type T { required id: int }
#   powql> insert T { id := 1 }

# 2. Back it up
powdb-cli --data-dir ./powdb_data backup ./snap

# 3. Later, rebuild it into a fresh directory
powdb-cli restore ./snap ./powdb_data_restored

# 4. The restored directory opens and reads identically,
#    and is safe to write to
powdb-cli --data-dir ./powdb_data_restored
#   powql> T
#   powql> insert T { id := 2 }   -- durable, survives a crash
```

---

## Limitations

Full snapshots, incremental (differential) backups, and coarse point-in-time restore are available today. The following limits are real today:

- **Offline / single-writer only.** Backup has no cross-process locking. The target directory must not be open in a live `powdb-server` or another CLI while you back it up. Online (serve-while-backing-up) snapshots are a future phase.
- **Whole-database only.** Backup snapshots every table. There is no per-table backup.
- **Restore is offline and needs a fresh destination.** Restore writes into a fresh or empty directory; it does not merge into a running database. If a restore fails partway, the destination may be left partial — discard it and restore again into a clean directory.
- **Same engine version.** A backup is restorable by the same PowDB engine version that wrote it. There is no cross-version on-disk format guarantee yet. (The manifest carries a format version and refuses an unrecognized one.)
- **Coarse PITR only.** Point-in-time restore lands you at the state captured by an increment, so its granularity is your increment cadence. **Fine-grained (sub-increment) PITR** — replaying to an arbitrary instant via WAL archiving — and **cloud sync** are not in this release.

The design for the upcoming incremental / PITR / cloud-sync phases lives in [`docs/design/2026-06-05-backup-pitr-sync-migrations-plan.md`](design/2026-06-05-backup-pitr-sync-migrations-plan.md).
