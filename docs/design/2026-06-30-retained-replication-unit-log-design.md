# Retained Replication-Unit Log Design

Date: 2026-06-30
Status: First implementation gate

## Decision

PowDB's first retained replication unit is the existing WAL record, stored in a new immutable segment envelope owned by `powdb-sync`.

This does not make `wal.log` itself a sync log. The current WAL remains the crash-recovery log and may still be truncated by checkpoint. Sync-capable checkpoint and recovery paths now archive durable WAL records into retained-unit segments before any checkpoint/recovery truncate can destroy history.

## Current Constraints

- `crates/storage/src/wal.rs` defines WAL record type, LSN, tx id, payload, CRC, and WAL format version.
- `crates/storage/src/catalog.rs` assigns LSNs, replays WAL records with per-page LSN checks, flushes pages and indexes during checkpoint, then truncates `wal.log`.
- `crates/backup/src/full.rs` takes full snapshots by checkpointing first and recording `source_lsn`.
- `crates/backup/src/incremental.rs` uses page-LSN deltas, not retained WAL.
- `crates/server/src/protocol.rs` is an append-only message protocol; sync will need new protocol messages or a dedicated sync port later.

The design must preserve all of those behaviors until sync is explicitly enabled.

## Segment Format

`powdb-sync` writes immutable retained-unit segment files:

```text
retained-<start_lsn>-<end_lsn>.prul
```

Each file contains:

1. segment header: magic, segment format version, current WAL format, the producing database's **active** catalog format, record count, start LSN, end LSN, primary generation, database id,
2. WAL-record units in strict contiguous LSN order,
3. footer magic plus a segment CRC over every preceding byte.

Each retained unit stores the same logical fields as `WalRecord`:

- `tx_id`
- `record_type`
- `lsn`
- `data`

The segment validates:

- magic and version,
- WAL format equality (strict),
- catalog format compatibility as a **range**, not equality (see below),
- non-zero database id and primary generation,
- non-empty unit list,
- known record type for the current WAL format,
- strictly contiguous LSNs,
- per-unit CRC,
- file-level footer CRC,
- no trailing bytes and no truncated record.

### Catalog format is a compatibility annotation, not identity

The catalog format version is versioned lazily: a database stays on the legacy
format (v5) on disk until the first expression index activates v6
(`active_catalog_version`). Segment identity therefore follows one rule:

- **Producers stamp the database's active catalog version**, not the binary's
  compile-time maximum. A database that never activated v6 keeps stamping v5, so
  an older replica (a v0.12 client stating catalog format v5) still matches.
- **Consumers accept older and reject newer.** `validate` accepts any catalog
  format in `1..=CATALOG_VERSION` and rejects only a version newer than the
  binary understands, with a message naming both the segment's version and the
  binary maximum. Segment-chain readers compare the strict lineage fields
  (database id, primary generation, WAL format) for equality but treat catalog
  format as a monotonic annotation: an **increase** across a chain (v5 then v6,
  activation mid-stream) is accepted; a **decrease** is rejected.
- **The primary's pull gate** checks the replica-stated catalog format against
  the database's active format: the replica states the maximum format it can
  read, and the primary accepts `replica_max >= active` and rejects a replica
  whose maximum is older than the data it would receive (rebootstrap with an
  upgraded replica required). Database id, primary generation, WAL format, and
  segment format remain strict-equality checks.

## Atomic Publish

Publishing a segment must be durable before any future checkpoint hook can truncate source history:

1. create/tighten the segment directory as owner-only,
2. write a temp file in the segment directory,
3. write all bytes,
4. `sync_all` the file,
5. create the final `.prul` name with a same-directory no-clobber hard link,
6. fsync the directory on Unix,
7. remove the temp file and fsync the directory again on Unix.

Readers only consider final `.prul` files. Temp files are ignored and can be cleaned by a later maintenance pass.

The no-clobber publish rule is intentional: segment ranges are immutable, so a second writer for an already-published range must fail instead of replacing the existing file.

Readers reject oversized segment files before allocation. Segment payloads contain WAL record data, so retained history must stay in private directories and must fail closed on implausible file sizes before the sync protocol can expose this path.

## Sync Metadata

`powdb-sync` also owns a hidden `.powdb-sync/` state directory below the database directory. This keeps sync state below the query layer and out of `powdb-query`.

Current metadata files:

- `identity.json`: versioned database identity with non-zero `database_id` and `primary_generation`.
- `replica-cursors.json`: versioned primary-side retention cursors keyed by replica id.

The identity file is created once with no-clobber publish semantics and a secure OS random database id. A concurrent creator that loses the race reads the already-published identity instead of replacing it. If secure randomness is unavailable, identity creation fails closed.

Cursor updates are mutable and serialized by a metadata lock file so concurrent replica updates cannot lose each other. The lock records owner PID and creation time; on Unix, a later writer reclaims the lock when the recorded owner process is gone, and invalid old lock files are treated as stale instead of permanently blocking cursor progress. The update path writes temp JSON, `sync_all`s the temp file, renames over the cursor file, then fsyncs the metadata directory on Unix. Active cursors define the minimum retained LSN as `min(applied_lsn + 1)`; retired cursors are ignored. Cursor files reject duplicate replica ids, unsupported characters, unsupported metadata versions, and LSN overflow.

`powdb-backup` snapshots now preserve sync fork-safety metadata when a database already has sync identity. Full and incremental manifests carry database identity, primary generation, source LSN, catalog hash, WAL format, catalog format, and retained-segment format. Default restore verifies the sync metadata but strips `.powdb-sync/identity.json`, so ordinary restored data dirs remain safe for the plain PowDB engine lifecycle. Same-lineage disaster recovery is explicit through `PreserveSyncIdentity`, which recreates only `.powdb-sync/identity.json` from the manifest and does not copy mutable cursor files, locks, retained segments, or arbitrary hidden sync state. Clone/fork restore is explicit through `ForkWithNewSyncIdentity`, which verifies the source sync snapshot metadata but mints a fresh destination identity. Legacy backups without sync metadata still restore as ordinary non-sync backups unless a caller explicitly requests fork mode.

## Retention GC

`powdb-sync` can now prune retained segment files below the active replica
cursor boundary. The boundary is `minimum_retained_lsn`: the smallest next LSN
needed by any active replica. A segment is deleted only when its entire
`end_lsn` is lower than that boundary. Segments crossing the boundary are kept
intact rather than rewritten.

GC is fail-closed. Before deleting any segment, it validates every candidate
segment's checksum, identity, filename/header LSN range, and retained tail
continuity for the history active replicas still need. A gap, overlap, corrupt
segment, or wrong database identity blocks the whole GC pass before any file is
removed. GC runs under the same cursor metadata lock as cursor publication, and
active cursor publication checks the retained tail before accepting a cursor. If
the requested next LSN is already below retained history, publication fails with
a rebootstrap-required error instead of creating an impossible active cursor. If
there are no active cursors, GC is a conservative no-op.

## What This Slice Does Not Do

This slice exposes only a private authenticated server status/pull/ack control plane. It does not create the public JS package, write-forward path, sync metrics, or concurrent-read chunked apply loop.

Those are later gates. The current substrate proves that PowDB can persist, validate, read, apply, and privately serve a retained WAL-record tail by LSN range while keeping the default plain engine lifecycle intact. It also has a backup-based cold-start bootstrap helper that restores a sync snapshot, archives the primary's live WAL tail, validates retained-tail continuity and V1 applyability, rejects unsupported DDL tails before restore/cursor publication, cleans up a restored replica directory if cursor publication fails, and publishes the primary-side cursor. The retained-tail apply fixtures prove post-snapshot insert/update/delete row convergence, index-backed lookup on a restored replica, durable local apply-state, safe-watermark replay while the catalog still matches that watermark, complete-marker recovery after storage reaches the target LSN, fail-closed repair when the catalog advanced only partway without complete state, unsupported DDL fail-closed behavior, and explicit transaction-split rejection before storage replay. The private server pull/ack path now uses the same V1 applyability boundary so batching limits or bad acknowledgements cannot advance a replica cursor to a transaction-cut LSN. Retention pressure policy now reports max-byte pressure, retires inactive cursors by explicit age policy, and supports an operator retain-LSN override that forces lagging cursors to rebootstrap.

Concurrent-read chunked apply, write-forward, sync metrics, public JS client integration, corrupt-state repair, and index/unique DDL propagation are still later gates. Private authenticated server pull/status/ack wire messages now exist as an internal control plane.

`read_units_since` is identity-aware: callers must provide the expected database id and primary generation, and every segment in the scanned directory must match that identity before any units are returned. It also validates the whole returned history range: segment filenames must match segment headers, returned LSNs must be contiguous, gaps fail closed, overlaps fail closed, and segment-directory access errors fail closed.

## Next Gates

1. Add write-forward with idempotency or typed `commit_outcome_unknown` on top of the private authenticated status/pull/ack control plane.
2. Add concurrent-read chunked retained-tail repair fixtures after cold-start bootstrap.
3. Add crash-injection coverage around segment publish, checkpoint, cursor update, recovery, and GC interruption.
4. Add replica apply tests that prove local reads see either the old applied LSN or a fully applied committed LSN.
5. Add public JS client support for the private sync frames only after retained history, snapshot identity, bootstrap, and chunked apply are proven.
