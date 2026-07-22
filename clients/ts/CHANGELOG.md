# Changelog

## 0.19.0 - 2026-07-22

- Version-alignment release in lockstep with workspace v0.19.0 (entity links:
  PowQL relationship traversal, persisted via catalog format v7). No TypeScript
  API changes; link declaration and traversal are PowQL/SQL-text features that
  flow through the existing `query`/`querySql` paths.

## 0.18.2 - 2026-07-21

- Version-alignment release in lockstep with workspace v0.18.2 (engine
  correctness patch: PowQL single-table aliased qualified-ref resolution, and
  two-valued filter semantics so a missing value never matches a comparison).
  No TypeScript API changes.

## 0.18.1 - 2026-07-21

- Version-alignment release in lockstep with workspace v0.18.1 (engine
  correctness patch: SQL qualified-ref resolution, planner range bounds,
  plan-cache literal ordering, live-backup lock, `not` precedence, nested
  float fidelity). Servers now send error class 8 (constraint_violation) for
  unique violations, which this client already mapped. No TypeScript API
  changes.

## 0.18.0 - 2026-07-20

- Version-alignment release in lockstep with workspace v0.18.0 (nested
  projections: a PowQL projection field can embed a correlated child query and
  return a native JSON array of shaped child objects per parent row). The
  nested column arrives as an ordinary JSON value, so existing JSON handling
  applies unchanged. No TypeScript API changes.

## 0.17.0 - 2026-07-19

- **Typed wire error codes.** Servers >= 0.17.0 append a stable one-byte error
  class to error frames; the client maps it onto `PowDBErrorCode` (`timeout`,
  `size_exceeded`, `auth_failed`, ...) instead of collapsing every server
  error to `query_failed`, and exposes the raw class as
  `PowDBError.wireErrorClass`. Fully backward compatible: classless frames
  from older servers behave exactly as before, and unknown future classes
  fall back to `query_failed`. See the repo's `docs/errors.md` for the code
  table.

## 0.16.0 - 2026-07-18

- Version-alignment release in lockstep with workspace v0.16.0 (NUL-safe
  composite index keys: fixes wrong rows from non-unique string indexes on
  values with embedded NUL bytes; old-format indexes are rebuilt automatically
  on open). No TypeScript API changes.

## 0.15.0 - 2026-07-16

- Version-alignment release in lockstep with workspace v0.15.0 (per-index
  statistics and cardinality-aware conjunction index choice). No TypeScript
  API changes; `explain` result rows now include selectivity tokens
  (`est_rows=... entries=... distinct=...`) on chosen index-scan lines.

## 0.14.0 - 2026-07-16

- Version-alignment release in lockstep with workspace v0.14.0 (conjunction
  index selection, embedded typed results, read-only snapshot serving). No
  TypeScript API changes; servers in read-only mode return a terminal
  "readonly mode: statement requires a writer" error for mutations, which
  surfaces through the existing error path.

## 0.13.0 - 2026-07-15

- Added the **lossless native typed result API**: `queryNative(powql)`,
  `querySqlNative(sql)`, and low-level `queryNativeRaw(...)` returning tagged
  `WireValue` cells. Native results preserve exact cell types: 64-bit ints as
  `bigint`, raw `bytes`, and JSON documents as parsed values plus the exact
  PJ1 bytes (`pj1`). Empty (SQL NULL / missing) is distinct from JSON `null`
  and from the string `"null"` in raw results.
- Added native parameterized queries over the new typed request frames.
- Added `SUPPORTED_CATALOG_VERSION` and `assertServerCatalogVersionSupported`
  so clients fail with a clear error against servers newer than they support.
- Legacy `query()` / `querySql()` string results are unchanged and remain
  byte-compatible; see README for when to prefer the native API.

## 0.12.0 - 2026-07-14

- Typed API: `"json"` column kind for the server's native JSON (PJ1) type.
  JSON cells arrive as canonical JSON text on the legacy protocol.
- No breaking changes; released in lockstep with workspace v0.12.0.

## 0.11.0 - 2026-07-13

- Version-alignment release in lockstep with workspace v0.11.0 (overflow
  pages, grouped aggregates). No TypeScript API changes.

## 0.10.0 - 2026-07-13

- Version-alignment release in lockstep with workspace v0.10.0. No
  TypeScript API changes.

## 0.9.0 - 2026-07-12

- Added `execScript(...)`: transactional, statement-aware script execution
  with all-or-nothing semantics.
- Connect is now pipelined and eager, reducing first-query latency.

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
