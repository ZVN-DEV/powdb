# Changelog

## 0.7.2

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
