# Changelog

## Unreleased

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
