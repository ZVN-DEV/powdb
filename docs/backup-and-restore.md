# Backup and Restore

PowDB takes a crash-consistent, blake3-verified full snapshot of a database directory and rebuilds a fresh one from it. Use it to capture a recoverable copy of your data before an upgrade, a risky migration, or on a regular schedule.

This is **full snapshot** backup: the whole database, all tables, in one operation. Incremental backup, point-in-time restore, and cloud sync are planned for later phases (see [Limitations](#limitations)).

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

The server does not need to be stopped, but backup briefly quiesces writes for the duration of the checkpoint (see [Guarantees](#guarantees)). Point it at the same `--data-dir` your CLI or server uses.

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

This is the first phase of backup support. The following limits are real today:

- **Whole-database only.** Backup snapshots every table. There is no per-table backup.
- **Restore is offline and needs a fresh destination.** Restore writes into a fresh or empty directory; it does not merge into a running database. If a restore fails partway, the destination may be left partial — discard it and restore again into a clean directory.
- **Same engine version.** A backup is restorable by the same PowDB engine version that wrote it. There is no cross-version on-disk format guarantee yet. (The manifest carries a format version and refuses an unrecognized one.)
- **Full snapshot only.** Incremental backup, point-in-time restore (PITR), and cloud sync are not in this release.

The design for the upcoming incremental / PITR / cloud-sync phases lives in [`docs/design/2026-06-05-backup-pitr-sync-migrations-plan.md`](design/2026-06-05-backup-pitr-sync-migrations-plan.md).
