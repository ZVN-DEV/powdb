# Changelog

## 0.8.0 — EXPERIMENTAL (not published)

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
