# Changelog

## Unreleased

### Fixed

- Every entry point reports a transport failure as `remote_unavailable`.
  `status()` and `syncNow()` used to let the underlying client's error escape
  with its own code, so a caller branching on `PowDBSyncErrorCode` saw codes that
  are not in that union.
- `DEFAULT_MAX_PULL_UNITS` raised from 512 to 4096, matching the server's own
  chunk ceiling. Paired with the server-side fix below, a transaction larger than
  the pull window is now served instead of wedging the replica.

### Changed

- `LocalApplyRequest` extends the new `NormalizedSyncIdentity` rather than the
  lenient caller-facing `SyncIdentity`, so `primaryGeneration` is `bigint` and
  not `bigint | number`. A replica always normalizes the identity before calling
  the local adapter, so this is what was already passed at runtime; the wider
  type only made the obvious adapter body
  (`(request) => local.applyRetainedUnits(request)`) fail to type-check against
  `@zvndev/powdb-embedded`.

### Engine side

- A committed transaction larger than the pull window can now be pulled. The
  chunk used to be cut at `maxUnits`, the cut landed inside the transaction, the
  primary refused it, and every retry cut in the same place, so the replica was
  wedged for good while `status` reported `repairAction: "pull"` with
  `lastSyncError: null`. `maxUnits` is a hint now and the chunk runs on to the
  commit or rollback that closes the transaction, bounded by the byte budget. A
  transaction too large for the byte budget answers with a rebootstrap status
  naming it, instead of an error the replica would retry forever.
- A live primary archives committed history on demand when a replica calls
  `status` or `pull`. Retained segments were previously written only by a
  checkpoint, and the only checkpoint a running primary performed was on graceful
  shutdown, so a replica polling a live primary was told `awaitArchive` forever.
- `status` reports `rebootstrap` when the tail a replica is about to pull holds a
  record V1 embedded sync cannot apply, with the reason naming DDL, instead of
  leaving the replica to discover it one failed pull at a time.

## 0.27.0 - 2026-08-26

- `SUPPORTED_CATALOG_VERSION` raised from 6 to 7. The engine's catalog format
  has been v7 (persisted entity links, activated lazily per database) since
  PowDB 0.19.0, so a replica that stated this package's ceiling in its identity
  was refused by any primary whose database had activated v7, and
  `assertServerCatalogVersionSupported` rejected such a primary. The package
  treats catalog payloads as opaque bytes, so there is no decoding change.
  `test/sync.test.ts` now reads `CATALOG_VERSION` out of
  `crates/storage/src/catalog/mod.rs` and fails when the two disagree, so the
  ceiling cannot silently fall behind again. The README identity example and
  the e2e test now state `SUPPORTED_CATALOG_VERSION` instead of a literal `5`.

## 0.26.0 - 2026-08-23

No package API changes. The version moves in lockstep with the engine, and
the exact peer pins on `@zvndev/powdb-client` and `@zvndev/powdb-embedded`
moved to 0.26.0 with it.

## 0.25.0 - 2026-08-16

No package API changes. Engine side, the primary's replica-cursor lock
(`upsert_replica_cursor` and friends) now waits up to 30 seconds with
jittered exponential backoff instead of giving up after 5 seconds, so a
replica pushing its cursor under ordinary contention no longer receives a
spurious `WouldBlock` refusal for a lock that was merely in use.

## 0.24.0 - 2026-08-15

First published release of `@zvndev/powdb-sync`, in lockstep with the engine
and with exact peer pins on `@zvndev/powdb-client` and
`@zvndev/powdb-embedded`. The pre-publication status described under 0.8.0
below no longer applies.

## 0.8.0 (experimental, pre-publication)

> **Status: experimental / beta-gated. NOT published to npm.** This is the
> Embedded Sync Milestone 0 substrate. `@zvndev/powdb-sync` stays unpublished
> until the Milestone-1 gates pass (crash matrix RF-04/RF-11/PH-02,
> concurrent-read-during-apply RA-01/RA-09, version-compat rejection RA-03,
> handshake SP-01/SP-05, perf PH-05, fuzz PH-07 — see `docs/embedded-sync.md`).
> The version tracks PowDB workspace v0.8.0 so the package builds and tests in
> lockstep; it does not imply a public release. Pin matching
> `@zvndev/powdb-client` and `@zvndev/powdb-embedded` versions.

- Initial experimental `@zvndev/powdb-sync` package.
- Adds primary-authoritative embedded-replica orchestration around local
  readonly queries, retained-unit pull/apply/ack, stale status, DDL rejection,
  and typed unknown write outcomes.
- Adds `startBackgroundSync(...)`, a small scheduler around explicit
  `syncNow()` with immediate or interval operation, overlap prevention,
  stop/abort support, and result/error callbacks.
- Adds `test:native` coverage for the real `@zvndev/powdb-embedded`
  `Database.applyRetainedUnits(...)` adapter behind the sync control loop.
- Adds `test:e2e` coverage for backup bootstrap through `powdb-cli`, real
  `powdb-server`/`@zvndev/powdb-client` write-forward, native local readonly
  reads, retained-unit pull/apply/ack, and post-snapshot row convergence.
- Documents the experimental CLI bridge used for dogfooding:
  `powdb-cli sync-enable` and `powdb-cli sync-bootstrap`.
