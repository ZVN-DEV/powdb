# Data-Protection Features Plan: Backup, PITR, Cloud Sync, Migrations

Date: 2026-06-05
Version baseline: PowDB v0.4.5
Scope: **server / cloud-hosted PowDB protecting its own data dir.** Designs four
features — incremental backup, point-in-time restore (PITR), cloud sync (DR),
and schema migrations — and how they share machinery.
Status: **planning deliverable. No engine code changed.** All file/function
references verified against the v0.4.5 source.

Out of scope (set aside by Kirby; covered in the companion doc): local-first
embedded replicas, offline multi-writer, per-user partial sync, CRDTs,
"PowSync." See [`2026-06-05-deployment-and-sync-strategy.md`](./2026-06-05-deployment-and-sync-strategy.md)
for that space. This doc is the *server-side, full-database, physical-artifact*
counterpart and deliberately does **not** duplicate it. Where the two touch (the
LSN'd WAL is the shared asset), this doc cross-references rather than restates.

---

## 0. Ground truth (verified against v0.4.5)

Every design decision below stands on these confirmed facts. File/line refs are
load-bearing.

| Fact | Where | Consequence for this plan |
|---|---|---|
| **Every page stamps the LSN of its last mutation** in header bytes `[8..16]`. | `page.rs:8-16` (header doc), `page.rs:173 lsn()`, `page.rs:464 page_lsn(&[u8])` (reads LSN from a raw page slice, no `Page` alloc). | Page-LSN diffing is real and cheap. This is the substrate for incremental backup AND cloud-sync DR. |
| **Pages are enumerable per heap**, `0..disk.num_pages()`, and each page's LSN is readable on disk via `disk.read_page(id)` → `page_lsn(buf)`. | `disk.rs:74 num_pages()`, `disk.rs:59 read_page()`, `heap.rs:1419 page_lsn(page_id)`, `heap.rs:1379 max_page_lsn()`. | A backup can iterate every page of every heap and select `page_lsn > last_backup_lsn`. `max_page_lsn()` already computes the global high-water LSN — exactly the cut a snapshot records. |
| **The WAL is truncated on checkpoint** (and checkpoint runs on graceful shutdown via `Engine`/`Catalog` `Drop`). | `catalog.rs:454 checkpoint()` → `wal.truncate()` (`wal.rs:331`); `catalog.rs:464-466`. Also truncated after recovery replay (`catalog.rs:442`). | Today the WAL is a **crash-recovery log, not retained history**. Fine-grained PITR is the one feature that requires *archiving WAL segments before truncation* — a new substrate. |
| **WAL records carry a monotonic `lsn`** and per-page idempotent redo: replay skips a record iff its target page's on-disk LSN `>=` record LSN. | `wal.rs:46-56` (`WalRecord`), `catalog.rs:234 replay_wal()`, `catalog.rs:262/282/296` (the `page_lsn(rid.page_id) >= rec.lsn` skip). | WAL archive replay is *already idempotent and page-targeted* — fine PITR reuses `replay_wal`'s exact algorithm, just sourced from archived segments. |
| **Restore MUST reset `next_lsn = max(page LSN)+1`.** Open already does this. | `catalog.rs:199-205`: `max_page_lsn` over all tables → `wal.set_next_lsn_at_least(max+1)`. `wal.rs:170 set_next_lsn_at_least`. The v0.4.3 P0 was an LSN-reset-on-restore bug. | Any restore path that rebuilds a data dir and then calls `Catalog::open` **inherits this correctness for free** — provided the rebuilt pages carry their true LSNs. This is the single most important restore invariant. |
| **DDL is WAL-logged and crash-safe**, and `alter add/drop column` rewrites the heap in place and stamps a barrier LSN. | `wal.rs:8-18` (`DdlCreateTable/DropTable/AddColumn/DropColumn`), `catalog.rs:1244 alter_table_add_column`, `catalog.rs:1304 stamp_all_pages_min_lsn`, `catalog.rs:570 create_table`, `catalog.rs:1180 drop_table`. | The migration framework builds on existing DDL primitives. Each DDL op flushes the WAL immediately (`catalog.rs:586,1188,1257`) — so a migration is a *sequence of already-durable DDL ops*, not a new transactional primitive. **Caveat below: no multi-statement DDL atomicity exists.** |
| **DDL does NOT run inside a rollback-able transaction.** `Begin/Commit/Rollback` exist but DDL paths flush the WAL and `persist()` immediately, outside any txn. | `parser.rs:233-241`, `plan_exec.rs:1541 Begin / 1553 Commit / 1568 Rollback`, `catalog.rs:548 rollback_to_last_sync` (reopens from disk + replays durable WAL). `alter_table_add_column` calls `self.persist()` at the end (`catalog.rs:1309`) — not undoable by `Rollback`. | A migration runner **cannot** rely on engine-level transactional rollback across multiple DDL steps. Safety must come from: (a) one-DDL-per-migration where possible, (b) a pre-migration snapshot/backup as the rollback mechanism, and (c) recorded version state so a half-applied migration is detectable. |
| `Engine` wraps `Catalog`; server holds `Arc<RwLock<Engine>>`. Admin operations need `&mut Engine` → the write lock. | `executor/mod.rs:252 Engine`, `:1813 catalog()`, `:1817 catalog_mut()`, `server/main.rs:275 Engine::with_memory_limit`, `handler.rs:147 dispatch_query` read/write split. Config via `POWDB_*` env vars (`main.rs:71-102`). | Backup/restore/sync/migrate hooks live behind the **write lock** (they quiesce or mutate the data dir). New `POWDB_*` env vars configure them. |
| On-disk layout is per-table heap files + one shared WAL + catalog file + per-index btree files. | `catalog.rs:61 catalog.bin` (magic `BCAT`, **version 3**), `catalog.rs:67 wal.log`, heaps `<Table>.heap` (`catalog.rs:1194`), indexes `<Table>_<col>.idx` (`catalog.rs:1204`). Pages have CRC + checksum flag (`page.rs:37 FLAG_HAS_CHECKSUM`). | Physical artifacts are tied to on-disk format. Backups/sync must record `catalog_version` + a page-format version and refuse cross-incompatible restores. Indexes are *derivable* — `rebuild_indexes_from_heap` (`catalog.rs:435`) means backups can skip `.idx` files and rebuild on restore. |

**The one-sentence consequence:** PowDB already produces *physical page-LSN diffs* and an
*idempotent page-targeted WAL*. Three of the four features (backup, DR sync,
coarse PITR) are assembly of existing parts; only **fine PITR** needs a genuinely
new substrate (a retained WAL archive), and **migrations** are an orthogonal
manager that touches none of the physical machinery.

---

## 1. Decomposition: which features share machinery

Three substrates, four features.

| Substrate | What it is | Built on | Features that need it |
|---|---|---|---|
| **S1 — Snapshot + page-LSN diff** | A consistent base copy of the data dir tagged with its `snapshot_lsn`, plus the ability to ship only pages whose `page_lsn > last_lsn`. | `page_lsn()`, `max_page_lsn()`, `disk.read_page`, `disk.num_pages` — all existing. New: a snapshot/diff format + writer/reader. | Incremental backup (1), Cloud-sync DR (3), Coarse PITR (2a). |
| **S2 — Retained WAL archive** | WAL segments copied to a log store **keyed by LSN range** *before* `checkpoint()` truncates them (Postgres `archive_command` style). Replayed onto a restored base to reach an exact LSN/timestamp. | `wal.read_all()`, `replay_wal()`'s idempotent per-page redo — existing. New: archive-before-truncate hook, segment naming, LSN→segment index, retention/GC. | Fine PITR (2b) **only**. |
| **S3 — Migration manager** | A versioned schema-evolution runner: a `_migrations` record in the catalog, ordered up/down PowQL DDL scripts, an idempotent applier, `migrate`/`rollback` surface. | Existing DDL primitives (`create_table`, `alter_table_add_column/drop_column`, `create_index`, `drop_table`). New: version store + runner + command surface. | Schema migrations (4) **only**. Orthogonal to S1/S2. |

**Reuse map:** S1 is the workhorse — backup, DR, and coarse PITR are three
*policies* over the same snapshot+diff engine (one-shot, scheduled-to-cloud, and
restore-to-nearest respectively). S2 is optional and bolts onto S1 to upgrade
coarse PITR into fine PITR. S3 shares nothing physical with S1/S2 and can be
built in parallel by a different person.

---

## 2. The hard part first: consistent snapshot design

A naive `cp -r data_dir/` of a **live** PowDB instance is torn: heaps are being
written page-by-page (`heap.rs flush_all_dirty`), the WAL is appending, the
catalog file is being rename-swapped (`catalog.rs:600 persist`), and `.idx`
files lag the heap until checkpoint. We need a point-in-time-consistent image of
the data dir **and** the single `snapshot_lsn` it corresponds to, without a long
stop-the-world.

### Options considered

| Option | Mechanism | Pros | Cons |
|---|---|---|---|
| **A. Checkpoint-then-copy under write lock** | Acquire `Engine` write lock → `catalog.checkpoint()` (flush all dirty heap pages + `save_dirty_indexes` + `wal.truncate`) → record `snapshot_lsn = max_page_lsn()` across all heaps → copy the now-quiescent files → release lock. | Trivial to reason about: after `checkpoint()` the on-disk heaps are **fully consistent and the WAL is empty** (`catalog.rs:447-449` doc). The copied image is exactly a clean-shutdown image, which `Catalog::open` already handles perfectly. No torn pages — nothing is mutating during the copy. `snapshot_lsn` is unambiguous. | Stop-the-world for the copy duration. Mitigated by copying into a hardlink/`reflink` snapshot of the dir (instantaneous on APFS/Btrfs/XFS) and copying *out of band* after releasing the lock. |
| **B. Page-LSN consistent cut (no quiesce)** | Don't quiesce. Iterate every page; for each, read its LSN. Define the cut as `snapshot_lsn = min over pages of (a globally-consistent frontier)`. | No write pause. | **Rejected.** There is no global page-version snapshot isolation in the heap; a scan racing with `flush_all_dirty` can read page N at LSN 100 and page N+1 at LSN 50, with an in-flight multi-page op torn across them. PowDB has no MVCC page versions to reconstruct a consistent cut from. Correctness can't be established without a quiesce or COW. |
| **C. Copy-on-write FS snapshot only** | `btrfs subvolume snapshot` / APFS clone / ZFS snapshot of the data dir with no DB cooperation. | Instant, no DB code. | The image is **crash-consistent, not clean**: it includes a non-empty WAL and possibly torn dirty pages. That is *acceptable* because `Catalog::open` runs `replay_wal()` on it — but only if the WAL fsync ordering guarantees hold across the FS snapshot boundary, and only if the page CRCs catch torn writes (`page.rs:114 from_bytes_verified`). Fragile to reason about; ties us to specific filesystems. |

### Recommendation: **Option A — checkpoint-then-copy, with an FS-clone fast path.**

Concretely, a new `Engine::snapshot(dest)` (or a `Catalog::snapshot`) does:

1. Take the **write lock** (admin op; readers already serialize through the
   `RwLock` in `handler.rs`).
2. `catalog.checkpoint()` — flushes every dirty heap page, persists dirty
   indexes, **truncates the WAL** (`catalog.rs:454`). The data dir is now a
   clean-shutdown image.
3. `snapshot_lsn = max over tables of heap.max_page_lsn()` (`heap.rs:1379`). This
   is the LSN the snapshot is valid as of. Record it in the snapshot manifest.
4. **Copy strategy:** prefer `reflink`/clone (APFS/Btrfs/XFS) or a hardlink tree
   for an O(1) point-in-time clone, then **release the write lock immediately**
   and stream the cloned files to the backup target out of band. Fall back to a
   plain recursive copy under the lock only when the FS doesn't support cloning.
5. Manifest records: format versions (`catalog_version=3`, page-format/checksum
   epoch), `snapshot_lsn`, per-file sizes + CRCs, and the set of `.heap` files
   (indexes are *omitted* and rebuilt on restore — see §3).

**Why A over C:** A produces an image `Catalog::open` treats as a clean shutdown
(empty WAL, no replay, no torn-page risk) — the simplest possible restore
contract, and it makes `snapshot_lsn` exact. The FS-clone step removes A's only
real downside (lock-held copy time). C's crash-consistent image is strictly
harder to validate for the same outcome.

**Quiesce budget:** with reflink/clone, the write lock is held only for
`checkpoint()` + the clone syscall — milliseconds to low-seconds depending on
dirty-set size, independent of total DB size. This is the "brief write-quiesce"
the prompt asks for, achieved via checkpoint rather than a custom barrier.

---

## 3. Per-feature designs

For each: mechanism · have vs build (S = days, M = weeks, L = months) · surface
area · crash-consistency · version-compat.

### Feature 1 — Incremental backup  (substrate S1)

**Hypothesis confirmed:** incremental backup = **page-LSN diffing, not a retained
log.** A backup chain is one *base* snapshot (§2) at `base_lsn`, then a series of
*increments* each shipping only the pages whose `page_lsn > last_backup_lsn`.
PowDB already stamps page LSNs and enumerates pages, so **no new change-log is
needed.**

**Mechanism.**
- **Base backup** = a §2 consistent snapshot. Manifest stores `base_lsn`.
- **Incremental backup** = take a fresh §2 consistent snapshot at `snapshot_lsn`;
  for every heap, for `page_id in 0..num_pages`, read the page and include it iff
  `page_lsn(buf) > last_backup_lsn`. Emit `{table, page_id, page_bytes}` records
  plus a manifest `{prev_lsn: last_backup_lsn, snapshot_lsn, catalog.bin}`. The
  catalog file is small — ship it whole every increment (captures schema/DDL
  changes for free). Indexes are omitted; rebuilt on restore.
- **Page deletes / heap shrink:** heaps only grow page count (`disk.allocate_page`
  `disk.rs:41`); `drop_table` removes whole `.heap` files. So an increment also
  records the *current set of heap files* and any that disappeared since
  `prev_lsn` (a dropped table). New pages in a grown heap are naturally captured
  (their LSN > last_backup_lsn).

**Have vs build.**
- HAVE (free): page LSNs (`page.rs:464`), enumeration (`disk.rs:74`),
  `max_page_lsn` (`heap.rs:1379`), per-page read (`disk.rs:59`), consistent
  snapshot via `checkpoint()`.
- BUILD: **(M)** a `powdb-backup` module/crate — snapshot driver, page-diff
  walker, backup-set on-disk format (`base.manifest` + `NNNN.incr` segment files),
  and a chain validator (each increment's `prev_lsn` must equal the previous
  segment's `snapshot_lsn`). **(S)** the backup-set writer/reader. **(S)** CLI
  surface.

**Surface area.**
- New files: `crates/backup/` (or a module in `server`) with
  `snapshot.rs`, `diff.rs`, `manifest.rs`, `format.rs`.
- New on-disk format: a *backup set* directory — `manifest.json`-ish header
  (text or the existing custom binary style), `base/` (cloned data dir or packed
  pages), `incr-<base_lsn>-<snapshot_lsn>.seg` files of `{table,page_id,bytes}`.
- New CLI/admin commands (not PowQL — these are operator ops, see §3 note on
  surface): `powdb-server backup --full <dest>`, `backup --incremental <dest>`.
  Reuse the existing CLI-flag parser in `main.rs:104-218`.
- Server hook: an admin entrypoint behind the write lock; or a separate
  short-lived process that opens the data dir read-mostly + asks the running
  server to `checkpoint` via a new admin message.
- Env: `POWDB_BACKUP_DEST`, `POWDB_BACKUP_RETENTION`.

**Crash-consistency.** The snapshot is clean (§2), so each segment is internally
consistent. Page CRCs (`page.rs:114`) detect any bit-rot in transit/at-rest.
A backup interrupted mid-write leaves an incomplete final segment; the chain
validator rejects it (its `snapshot_lsn` is never committed to the manifest until
the segment is fully written + fsync'd).

**Version-compat.** Manifest pins `catalog_version` (3) and a page-format epoch
(presence of `FLAG_HAS_CHECKSUM`, `page.rs:37`). Restore refuses a backup set
whose `catalog_version` is newer than the running binary supports. Because pages
are shipped as raw bytes, a page-format change is a **breaking** backup-format
change — bump a `backup_format_version` and gate restore on it.

---

### Feature 2 — Point-in-time restore (PITR)  (S1 = coarse, S1+S2 = fine)

**Hypothesis confirmed: two tiers.**

#### 2a. COARSE PITR — restore to nearest snapshot (S1 only)

**Mechanism.** Given a backup chain, pick the latest base+increments whose
`snapshot_lsn <= target`. **Rebuild** a fresh data dir:
1. Materialize the base (copy `base/` heaps + `catalog.bin`).
2. Apply each increment in order: for each `{table,page_id,bytes}`, write the
   page into the table's heap at `page_id` (extending the heap file as needed).
   Because every shipped page carries its true LSN in `[8..16]`, the rebuilt
   heaps end up with correct per-page LSNs automatically.
3. Delete `.idx` files (or just don't create them).
4. Run `Catalog::open(new_dir)`. This:
   - replays the (empty) WAL — no-op, since snapshots ship a clean image;
   - **computes `max_page_lsn` and sets `next_lsn = max+1`** (`catalog.rs:199-205`)
     — the v0.4.3 restore-LSN bug is fixed *for free* because we route through
     the same open path;
   - `rebuild_indexes_from_heap` is available (`catalog.rs:435`) — call it (or let
     `Table::open_with_indexes` rebuild missing `.idx` on first open,
     `catalog.rs:175`).

**RPO = snapshot interval.** You can only land on an LSN that some snapshot
captured. Simple, robust, no WAL retention.

**Build: (S)** the restore/rebuild driver (mostly the inverse of the diff
walker). **Recommend building this FIRST** — it is the validation harness for the
whole backup feature (a backup you can't restore is worthless).

#### 2b. FINE PITR — snapshot + replayed WAL archive (S1 + S2)

**Mechanism.** Restore the nearest snapshot at `base_lsn <= target` (2a), then
**replay archived WAL records** with `base_lsn < lsn <= target_lsn` (or up to a
target *timestamp*, mapped to an LSN via the archive index) onto the restored
heaps. Replay reuses `replay_wal`'s exact idempotent per-page algorithm
(`catalog.rs:234`): a record is applied iff `page_lsn(rid.page_id) < rec.lsn`.
After replay, `Catalog::open` again sets `next_lsn = max_page_lsn+1`.

**This is the one place a retained WAL earns its keep.** Today
`checkpoint()`/recovery **truncate** the WAL (`catalog.rs:442,465`), so the
history needed for fine PITR is destroyed. S2 must **archive WAL segments before
truncation**:

- **Archive-before-truncate hook (S2 core).** Before `wal.truncate()` runs in
  `checkpoint()` (and in `replay_wal`'s post-replay truncate), copy the WAL's
  records to a **log store keyed by LSN range**: `wal-<startLSN>-<endLSN>.seg`.
  Since `wal.read_all()` (`wal.rs:239`) already parses records with their LSNs,
  the archiver reads them, writes a segment, fsyncs it, *then* allows truncate.
  This requires a small change to the checkpoint path to call an injected
  archiver — designed here, not implemented.
- **Segment rotation alternative (cleaner, M):** instead of "copy the whole WAL
  at checkpoint," rotate the WAL into numbered segments and archive a segment as
  soon as it's full + fsync'd, so the archive lag is bounded by segment size, not
  checkpoint interval. This is the Postgres model and the better long-term shape;
  the copy-at-checkpoint version is the cheap first cut.
- **Archive index:** an LSN→segment map + optional `commit-LSN → wall-clock`
  table (derived from WAL `Commit` records / append time) so a *timestamp* target
  resolves to an LSN.

**Have vs build.**
- HAVE: idempotent page-targeted replay (`replay_wal`), LSN-stamped records
  (`wal.rs:54`), CRC-validated WAL reads (`wal.rs:306-316`).
- BUILD: **(L)** WAL archiving — the before-truncate hook, segment store,
  LSN/timestamp index, **retention/GC**, and a replay-to-target driver that stops
  at `target_lsn`. The "stop at target" needs a bounded variant of `replay_wal`
  (today it replays everything in the file).

**Crash-consistency.** Archive a segment *and fsync it* **before** permitting the
truncate — otherwise a crash between truncate and archive loses history (a
silent PITR gap). This ordering is the crux of S2 correctness. Replay onto a
restored base is safe because it is idempotent and page-targeted (re-running it
can't double-apply).

**Version-compat.** WAL record format (`wal.rs:8-18` type enum,
`WAL_HEADER_SIZE=25`) is the compat surface. Archived segments must record the
WAL format version; a record-type addition is backward-compatible (old replay
ignores unknown types — but note `wal.rs:271` *stops* replay on an unknown type,
so a forward-compat reader must be tolerant). Pin a `wal_archive_version`.

**Recommendation:** ship **coarse PITR first** (it falls out of backup+restore
for near-zero extra cost), and build **fine PITR last** — it carries the only
genuinely new, genuinely risky substrate (retention, archive-before-truncate
ordering, timestamp mapping).

---

### Feature 3 — Cloud sync (DR replica / off-site backup)  (substrate S1)

**Hypothesis largely confirmed: cloud-sync-DR = the page-diff mechanism on a
timer/continuous, full-database replication to a cloud target.** It is
**"scheduled incremental backup to S3-like storage" + the restore path** — *not*
a new streaming protocol, for the DR use case.

**Mechanism.** Run Feature 1's incremental backup on a schedule (or continuously,
bounded by checkpoint cadence) and push base + increment segments to an
object-store target (S3-compatible). DR failover = run Feature 2 (restore) from
the cloud-held backup chain into a fresh PowDB instance. RPO = backup interval;
RTO = restore time.

**Streaming vs scheduled — assessment.** True low-RPO streaming would ship WAL
frames continuously (the Turso-style model in the companion doc). That is the
*local-first replica* shape Kirby set aside, and it requires retained WAL +
ack-based retention — i.e. it would actually be built on **S2**, not S1. For the
**DR** goal here, **scheduled S1 page-diff is the right answer**: it needs no
streaming transport, no replica-catch-up protocol, and reuses backup verbatim.
If sub-minute RPO is later required, upgrade by pushing S2 WAL segments to the
same object store between snapshots (this is exactly fine-PITR's archive, pushed
off-site) — a clean, additive evolution.

**Have vs build.**
- HAVE: everything Feature 1 has; the §2 snapshot; the restore path (Feature 2a).
- BUILD: **(M)** an object-store uploader (S3 API — likely a thin dependency or
  pre-signed-PUT approach to avoid a heavy SDK), a **scheduler** (interval timer
  in the server, or an external cron invoking the backup CLI — prefer external
  cron first, zero new server complexity), and a **manifest-in-cloud** layout so a
  cold restore can discover the latest valid chain. **(S)** a `restore --from
  s3://...` path.

**Surface area.** `crates/backup/cloud.rs`; env `POWDB_SYNC_TARGET`,
`POWDB_SYNC_INTERVAL`, `POWDB_SYNC_CREDENTIALS`. Server hook: optional background
task spawned in `main.rs` after `Engine` init (`main.rs:275`), guarded so it only
takes the write lock for the brief checkpoint window (§2).

**Crash-consistency.** Each pushed segment is atomic-or-absent in the object store
(complete-then-commit-manifest). A failed upload retries; the cloud manifest only
advances when a segment is fully durable remotely. The DR replica is always
restorable to the last *fully-uploaded* increment.

**Version-compat.** Same as Feature 1 (raw pages → page-format-pinned). Cross-
version DR (restore onto a newer binary) is allowed only when the newer binary
declares it can read the older `catalog_version`/page epoch.

**Boundary with the companion doc:** that doc's "PowSync v0 / Turso-style frame
replication" is the *client-facing, partial, bidirectional* story and is
**explicitly different** from this server-authoritative, full-DB, push-only DR.
This doc does not design frame streaming; it designs scheduled physical backup to
cloud. They could later share S2's archived WAL segments as a transport.

---

### Feature 4 — Schema migrations  (substrate S3, orthogonal)

**Hypothesis confirmed: migrations are orthogonal** to all physical machinery — a
schema-version framework over existing DDL.

**Mechanism.**
- A **version store**: a reserved system table `_migrations` (rows:
  `version:int, name:str, applied_at:datetime, checksum:str`) created via the
  existing `create_table` path, OR a small dedicated record appended to
  `catalog.bin`. **Recommend a real `_migrations` table** — it's WAL-logged,
  crash-safe, and queryable, and needs only a naming reservation (no underscore-
  table convention exists today; `validate_identifier` already permits a leading
  `_`, `catalog.rs:24`). Reserve the `_` prefix for system tables.
- **Migration scripts**: ordered files `NNNN_name.up.powql` / `NNNN_name.down.powql`
  containing PowQL DDL (`type ...` create, `alter T add column`, `alter T drop
  column`, `alter T add index`, `drop T` — all confirmed in `parser.rs:1531`,
  `plan_exec.rs:1413-1463`).
- **Runner**: on `migrate`, read applied versions from `_migrations`, find pending
  scripts in order, and for each: execute its DDL statements, then insert a
  `_migrations` row. On `rollback`, run the `.down` script for the latest applied
  version and delete its row.
- **Idempotency**: the runner skips any version already in `_migrations`; checksum
  mismatch (script changed after apply) is a hard error.

**The transactionality problem (assessed honestly).** PowDB has **no multi-
statement DDL atomicity**: each DDL op flushes the WAL and `persist()`s
immediately (`catalog.rs:586,1188,1257,1309`), and `Rollback` only rewinds *data*
mutations by reopening from disk (`catalog.rs:548`) — it does **not** undo a
committed `alter`/`create`/`drop`. Therefore:
- A migration with **one** DDL statement is effectively atomic (that single op is
  WAL-logged and crash-safe on its own — e.g. `alter_table_add_column` stamps a
  barrier LSN and flushes, `catalog.rs:1304-1307`).
- A migration with **multiple** DDL statements can **half-apply** if it fails or
  crashes midway. The framework must handle this, since the engine won't:
  1. **Per-statement progress isn't recorded by default** → make multi-statement
     migrations *resumable/idempotent by construction* (e.g. guard each step:
     "add column X if absent" — `alter_table_add_column` already errors
     `AlreadyExists`, `catalog.rs:1248`, which the runner treats as "already
     done").
  2. **Recommend a pre-migration snapshot** (§2) as the real rollback mechanism:
     `migrate` takes a §2 backup first; on failure, operators restore it. This is
     why migrations, though logically orthogonal, are *operationally* paired with
     backup — and a reason to build backup before migrations.
  3. Record an in-progress marker (`_migrations` row with `applied_at = NULL` /
     status column) so a crashed half-migration is *detectable* on next start and
     the runner refuses to proceed until resolved.

**`add column` on a populated table.** Confirmed safe: `alter_table_add_column`
rewrites every row to the new layout via `rewrite_rows_for_schema_change` and
refuses `required` columns on non-empty tables (`catalog.rs:1272-1281`,
`:1289-1296`). The migration framework must surface this constraint (a migration
adding a required column to a populated table will *correctly* fail; the script
should add it nullable + backfill via `update` + (future) add constraint).
This rewrite is O(rows) — large-table migrations are a heavy, lock-held op; the
runner should warn and operators should snapshot first.

**Have vs build.**
- HAVE: all DDL primitives + WAL-logging + crash-safe single-DDL ops + heap-
  rewrite backfill.
- BUILD: **(M)** the runner (version store, ordered-script loader, apply/rollback,
  checksum, in-progress marker), **(S)** `_migrations` table reservation, **(S)**
  command surface.

**Surface area.**
- New: `crates/migrate/` or a server module — `runner.rs`, `store.rs`, `script.rs`.
- Commands: operator CLI `powdb-server migrate [--to N]`, `migrate rollback`,
  `migrate status`. (DDL stays PowQL inside the scripts; the *runner* is a CLI
  tool, not new PowQL syntax — though a thin `migrate`/`rollback` PowQL verb is
  possible later.)
- Server hook: behind the write lock (DDL mutates the catalog).
- Env: `POWDB_MIGRATIONS_DIR`.

**Crash-consistency.** Single-DDL migrations: inherited from the engine. Multi-
DDL: resumable-by-construction + in-progress marker + pre-migration snapshot.

**Version-compat.** `_migrations` is just data — fully forward/backward compatible
across `catalog_version`. Migration *scripts* are PowQL text — compatible as long
as the DDL surface is stable.

---

## 4. Restore paths — explicit per feature

The restore invariant that ties everything together (and the v0.4.3 P0):

> **After any rebuild, the data dir MUST be opened via `Catalog::open`, which sets
> `next_lsn = max(page LSN over all heaps) + 1` (`catalog.rs:199-205`). Never set
> `next_lsn` to 1 or to a snapshot constant on a restored dir.** As long as every
> restored/rebuilt page carries its true LSN in header `[8..16]`, routing through
> `Catalog::open` makes the LSN correct automatically. This is *the* reason
> Option A's "image looks like a clean shutdown" design is chosen — open already
> does the right thing.

| Feature | Restore input | Rebuild steps | `next_lsn` correctness |
|---|---|---|---|
| **Full backup** | base snapshot | copy heaps + `catalog.bin` into fresh dir; drop `.idx`; `Catalog::open` (rebuilds indexes, sets `next_lsn`). | Free via open: pages carry snapshot LSNs → `max_page_lsn+1`. |
| **Incremental** | base + increments | materialize base; apply each `{table,page_id,bytes}` increment by writing the page at `page_id` (heap auto-extends); drop `.idx`; `Catalog::open`. | Free via open: each shipped page's true LSN lands in its header → `max_page_lsn+1`. |
| **Coarse PITR** | chain ≤ target | as incremental, stopping at the last increment with `snapshot_lsn ≤ target`. | Free via open. |
| **Fine PITR** | chain ≤ target + WAL archive | restore nearest base; **bounded** replay of archived records with `base_lsn < lsn ≤ target_lsn` using `replay_wal`'s page-targeted idempotent redo; `Catalog::open`. | Replay stamps pages with record LSNs (`catalog.rs:273/287/301`); open then sets `next_lsn = max+1`. |
| **Cloud DR** | chain pulled from object store | same as incremental/coarse, sourced from S3. | Free via open. |
| **Migration rollback** | `.down` script (or pre-migration snapshot) | run down-DDL via engine; or restore the §2 pre-migration snapshot. | Snapshot path: free via open. Down-script path: normal DDL, no LSN concern. |

---

## 5. Phased roadmap (fastest valuable win first)

| Phase | Deliverable | Substrate | Size | Why here |
|---|---|---|---|---|
| **P1** | **Full snapshot backup + restore** (consistent snapshot §2 + materialize + `Catalog::open`). | S1 | M | First valuable win and the *validation harness* for everything else: a backup you can't restore is worthless, so backup and restore ship together. Proves the §2 quiesce and the LSN-on-restore invariant end-to-end. |
| **P2** | **Page-LSN incremental backup + incremental restore.** | S1 | M | The headline efficiency win; reuses P1's snapshot + restore. Coarse PITR falls out for nearly free (it's "restore the chain up to target"). |
| **P3** | **Cloud target push (DR)** — scheduled incremental → object store + `restore --from cloud`. | S1 | M | Off-site DR is high operator value and is *pure assembly* of P1+P2 plus an uploader + scheduler (prefer external cron first). No new correctness surface. |
| **P4** | **Migration framework.** | S3 | M | Orthogonal; can run **in parallel** with P1–P3 (different person, no shared code). Sequenced after backup so "snapshot before migrate" is available as the rollback mechanism. |
| **P5** | **Fine-grained WAL-archive PITR.** | S2 | L | Last and riskiest: the only feature needing a new retained substrate, archive-before-truncate ordering, retention/GC, and timestamp→LSN mapping. Build only when RPO = snapshot-interval is proven insufficient. |

**Ordering justification.** P1 is first because restore validates backup and
exercises the single most dangerous invariant (LSN reset). P2/P3 are stacked
reuse of P1 with rising operator value and no new correctness surface. P4 is
parallelizable and intentionally *after* backup so migrations get a real rollback
story. P5 is deferred because retained-WAL is the one place we add a brand-new,
fsync-ordering-sensitive, GC-bearing subsystem — and coarse PITR already covers
most real RPO needs.

---

## 6. Hard parts — don't ship half

| Risk | Why it bites | Mitigation |
|---|---|---|
| **Torn snapshots** | A live data-dir copy is torn: heaps mid-flush, WAL appending, `catalog.bin` mid-rename, `.idx` lagging. | §2 Option A: `checkpoint()` first (clean image, empty WAL), then reflink/clone, then release lock. Never copy a live dir without quiescing. |
| **Restore LSN correctness (the v0.4.3 P0)** | Setting `next_lsn` wrong on a restored dir → next writes reuse LSNs ≤ stamped page LSNs → next crash's replay silently skips them = data loss. | **Always** finish restore by routing through `Catalog::open` (`catalog.rs:199-205`), and ensure every restored page carries its true LSN. Add a restore-time assertion: `next_lsn > max_page_lsn`. |
| **Cross-version artifact compatibility** | Backups/sync ship **raw pages** tied to the on-disk format (`catalog_version=3`, page checksum epoch, WAL record format). A format bump silently breaks restore. | Pin `backup_format_version`, `catalog_version`, page-epoch, `wal_archive_version` in every manifest. Restore refuses unknown/newer versions. CI test that restores last-release backups. |
| **Archive-before-truncate ordering (P5)** | `checkpoint()`/recovery truncate the WAL (`catalog.rs:442,465`). If truncate races ahead of archive, history is lost → silent PITR gap. | Archive segment + **fsync** must complete *before* truncate is permitted. Treat archive failure as a hard error that blocks checkpoint (or degrades to "no fine PITR" with a loud warning). |
| **Retention / GC of archived segments (P5)** | Unbounded WAL/segment retention fills the disk/bucket; over-aggressive GC breaks the chain (deletes a segment a base still needs). | Retention keyed to backup chain: never GC a WAL segment whose LSN range is `> oldest retained base_lsn`. GC base+increment chains by policy (`POWDB_BACKUP_RETENTION`), oldest-first, never breaking a chain in use. |
| **Multi-statement migration half-apply (P4)** | No engine-level multi-DDL atomicity; a crash mid-migration leaves a partial schema. | Resumable-by-construction DDL (guarded "if absent"), in-progress marker in `_migrations`, mandatory pre-migration §2 snapshot as rollback. Refuse to proceed past a detected half-migration. |
| **Large-table `alter` cost** | `add/drop column` rewrites every row (`rewrite_rows_for_schema_change`) under the write lock — O(rows), blocking. | Runner warns on row count; operators snapshot first; document the lock-held window. (Online schema change is out of scope.) |

### What each feature does NOT give you

- **Incremental backup** does **not** give point-in-time-to-the-second recovery
  (RPO = snapshot interval), nor logical/row-level backup (it's physical pages),
  nor partial-table backup.
- **Coarse PITR** does **not** let you land between snapshots. **Fine PITR** does,
  but only back to the oldest retained base + archive — not "forever."
- **Cloud sync (DR)** does **not** give a live hot-standby, query-able replica, or
  bidirectional/partial sync. It is push-only off-site backup; failover is a
  *restore*, not an instant cutover. (Live replica / frame streaming = companion
  doc's territory.)
- **Migrations** do **not** give multi-statement transactional DDL, online
  (non-blocking) schema change, or automatic data backfill beyond what the
  scripted PowQL DDL + the existing heap rewrite already do.

---

## 7. New surface area summary

| Kind | Items |
|---|---|
| New crates/modules | `crates/backup/` (snapshot, diff, manifest, format, cloud), `crates/migrate/` (runner, store, script). |
| On-disk formats | Backup set (`manifest` + `base/` + `incr-*.seg`); WAL archive (`wal-<lsn>-<lsn>.seg` + LSN/timestamp index) [P5]; `_migrations` system table. |
| Engine/Catalog additions (designed, not built) | `Catalog::snapshot(dest) -> snapshot_lsn`; an archive-before-truncate hook in `checkpoint()` [P5]; a bounded `replay_to(target_lsn)` variant of `replay_wal` [P5]; reuse `max_page_lsn`, `rebuild_indexes_from_heap`, `set_next_lsn_at_least`. |
| Commands (operator CLI, not PowQL) | `backup --full/--incremental`, `restore [--to <lsn|ts>] [--from <path|s3>]`, `migrate [--to N] / rollback / status`. |
| Server hooks | Admin ops behind the write lock; optional cloud-push background task spawned post-`Engine::with_memory_limit` (`main.rs:275`). |
| Env vars | `POWDB_BACKUP_DEST`, `POWDB_BACKUP_RETENTION`, `POWDB_SYNC_TARGET`, `POWDB_SYNC_INTERVAL`, `POWDB_SYNC_CREDENTIALS`, `POWDB_MIGRATIONS_DIR`, (`POWDB_WAL_ARCHIVE_DIR` [P5]). |

---

## 8. Critical files for implementation

- `crates/storage/src/catalog.rs` — `open` (LSN reset on restore, `:199`), `checkpoint`/`replay_wal` (snapshot + WAL archive hook + bounded replay), DDL primitives (migrations).
- `crates/storage/src/heap.rs` — `max_page_lsn` (`:1379`), `page_lsn` (`:1419`), `insert_at`/`set_page_lsn`, enumeration over `disk.num_pages()` (the page-diff + restore engine).
- `crates/storage/src/page.rs` — page header LSN `[8..16]` + `page_lsn(&[u8])` (`:464`) + CRC (`:114`) — the diff/integrity substrate.
- `crates/storage/src/wal.rs` — `read_all`, `truncate`, `lsn`, `set_next_lsn_at_least` (WAL archive + fine PITR replay).
- `crates/server/src/main.rs` + `handler.rs` — lifecycle, write-lock admin hooks, env/CLI surface for backup/restore/sync/migrate.
