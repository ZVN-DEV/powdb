# Changelog

## 0.8.0 - 2026-07-02

- Released in lockstep with PowDB workspace v0.8.0 (Embedded Sync Milestone 0).
- Added **experimental** low-level authenticated embedded-sync protocol helpers:
  `client.syncStatus`, `client.syncPull`, and `client.syncAck`.
- Added TypeScript wire support for the private sync status/pull/ack frames
  (`0x20`-`0x25`), retained-unit payloads, sync repair actions, and the
  `sync` observability event.
- These sync helpers are experimental and beta-gated: pin matching client/server
  versions. Plain `query(powql)` / `querySql(sql)` frames are unchanged.

## 0.5.1 - 2026-06-17

- Kept the npm client release in lockstep with PowDB workspace v0.5.1.
- No TypeScript API changes; this is a dependency/version-alignment release.


All notable changes to `@zvndev/powdb-client`.

## Compatibility

| Client version | Compatible PowDB server | Notes |
|---|---|---|
| 0.8.x | Matching sync-enabled 0.8.x server | Adds experimental private authenticated embedded-sync helpers (`syncStatus`, `syncPull`, `syncAck`) over wire frames `0x20`-`0x25`. Pin matching client/server versions for these helpers until the stable `@zvndev/powdb-sync` package boundary exists; plain `query(powql)` and `querySql(sql)` frames remain unchanged. |
| 0.5.x | 0.4.7+ | Adds `client.query(powql, params)` for `$N` parameter binding. The `QueryWithParams` (`0x04`) wire message is only understood by server ≥0.4.7; parameterized queries against an older server error out. Plain `query(powql)` calls remain compatible with 0.3.x–0.4.x. |
| 0.4.x | 0.3.x – 0.4.x | Wire protocol v1 plus the optional Connect `username` field. **Multi-user mode requires client ≥0.4.0 AND server ≥0.4.6** (the server enforces roles as of 0.4.6; 0.4.5 accepted the username but did not enforce `readonly`). When `user` is omitted the Connect frame is byte-identical to the 0.3.x shape, so legacy shared-password and no-auth servers work unchanged. |
| 0.3.x | 0.3.x – 0.4.x | Wire protocol v1. The client warns only on a major-version mismatch, so any `0.x` server connects. Minor server bumps may add new opcodes; the client tolerates unknown response codes by surfacing `PowDBError`. Pin both ends. **Caveat:** the 0.3.x client has no `user` option, so it cannot authenticate to a 0.4.5+ server running in **multi-user mode** (the server requires a username once any named user is defined). Shared-password mode works fine. |

The client warns on major-version mismatch with the server during the
handshake. Within `0.x`, treat any minor-version skew between client and
server as best-effort and pin both ends.

## [0.5.0] — 2026-06-10

### Added
- **Parameter binding** — `client.query(powql, params)` accepts a second
  argument of positional values bound to `$1`, `$2`, … placeholders in the
  PowQL text. Values are sent as a typed parameter list in the new
  `QueryWithParams` (`0x04`) wire message and bound at the token level on the
  server, so they are injection-inert and byte-faithful (a value containing
  PowQL syntax is stored verbatim, never re-parsed). Requires server ≥0.4.7.

### Compatibility
- Parameterized queries require **server ≥0.4.7**. Plain `query(powql)` (no
  params) is unchanged and still works against 0.3.x–0.4.x servers.

## [0.4.0] — 2026-06-09

### Added
- `user` option on `ClientOptions` (and therefore `PoolOptions`) for
  multi-user authentication. The username is sent as a length-prefixed
  string appended after the password in the Connect frame, mirroring
  `crates/server/src/protocol.rs`. When omitted, the frame stays
  byte-identical to the 0.3.x legacy shape, so older servers are unaffected.
- Live integration suite (`pnpm run test:auth`) covering readwrite/readonly
  roles, `permission denied` on readonly writes, and `auth_failed` on
  missing user / wrong password / unknown user.

### Changed
- Version jumps to **0.4.0** (semver-minor for a feature in 0.x), which also
  intentionally aligns the client's minor with the server's: multi-user mode
  end-to-end requires client ≥0.4.0 and server ≥0.4.6.
- The exported `Message` Connect variant now carries `username: string | null`.
  Callers constructing Connect frames directly via `encode(...)` must add the
  field (pass `null` for legacy behaviour).

### Fixed
- `sourceMap`/`declarationMap` disabled in `tsconfig.json` — the tarball no
  longer ships 12 dead `.map` files (they pointed at `src/`, which the 0.3.4
  `files` allowlist already excluded, so they were broken references).

## [0.3.5] — 2026-06-05

### Fixed
- Support server wire message type 0x0b (`MSG_RESULT_MSG`): DDL,
  transaction-control, and view-refresh statements now resolve to
  `{ kind: 'message', message }` instead of crashing the connection.
  Required for compatibility with powdb-server 0.4.x.

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
- Typed `Client.connect`, `client.query`, and the `Pool.withClient` helper.
- Structured `PowDBError` with stable error codes.
- `AbortSignal` support for query cancellation.
- TLS via `tls: true` or a `tls.ConnectionOptions` object.
- Typed-row coercion via `client.queryTyped(query, schema)`.
- Password authentication via the `password` option.
- Strongly-typed `EventEmitter<ClientEvents>` emitting `query` and `close`.
