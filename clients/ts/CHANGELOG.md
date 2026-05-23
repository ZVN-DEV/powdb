# Changelog

All notable changes to `@zvndev/powdb-client`.

## Compatibility

| Client version | Compatible PowDB server | Notes |
|---|---|---|
| 0.3.x | 0.3.x | Wire protocol v1. Minor server bumps may add new opcodes; client tolerates unknown response codes by surfacing `PowDBError`. |

The client warns on major-version mismatch with the server during the
handshake. Within `0.x`, treat any minor-version skew between client and
server as best-effort and pin both ends.

## [0.3.4] — 2026-05-22

### Added
- LICENSE file shipped in the npm tarball.
- `peerDependencies: @types/node >=18` (optional) so type-check works in
  fresh projects without a separate `@types/node` install.
- `homepage` and `bugs` fields in `package.json`.

### Changed
- Tarball `files` allowlist tightened to `dist`, `README.md`, `LICENSE`,
  `CHANGELOG.md`. No more shipping `src/`, `*.map`.
- Expanded npm keywords for discoverability.

## [0.3.3] — 2026-05-16

Initial published release tracked here. Earlier 0.3.0 / 0.3.1 / 0.3.2 were
iterative publishes during the v0.3 release week.

### Features
- Typed `Client.connect`, `client.query`, `withClient` pool helper.
- Structured `PowDBError` with stable error codes.
- `AbortSignal` support for query cancellation.
- TLS via `tls: { ca, rejectUnauthorized }`.
- Typed-row coercion via `query<T>(...)`.
- Password authentication via `password` option.
- Strongly-typed `EventEmitter<ClientEvents>` (`connect`, `close`, `error`).
