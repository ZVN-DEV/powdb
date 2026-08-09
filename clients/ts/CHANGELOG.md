# Changelog

## Unreleased

## 0.23.0 - 2026-08-09

No client changes. Version moves in lockstep with the engine.

Two server-side changes alter what existing SQL returns, so they are worth
knowing about from the client:

- `"double quotes"` in SQL now delimit an **identifier**, not a string, so
  `SELECT "name" FROM Author` returns the column rather than the literal text
  `name` once per row. Use single quotes for string literals.
- `= NULL` and `<> NULL` in SQL now match nothing, as they do in every other
  engine. They previously returned the `IS NULL` rows.

## 0.22.0 - 2026-08-03

- **Wire protocol version negotiation.** `Connect` now carries a hello block
  (protocol version range, catalog format ceiling, named features) and the
  client reads the server's hello from `ConnectOk`. New exports:
  `CLIENT_CAPABILITIES`, `PROTOCOL_VERSION_NEGOTIATED`, `WIRE_FEATURE`;
  new `Client.protocolVersion` and `Client.hasFeature(name)`. Against a
  pre-0.22 server the hello is ignored as trailing bytes and everything
  behaves as before; `legacyHandshake: true` reproduces the old frame
  byte-for-byte for testing.
- **`requireFeatures` connect option**: fail the connection with a typed
  error if the server does not offer a named feature, instead of failing
  later mid-session.
- **Error class 10** is decoded and surfaced as error kind
  `protocol_version` when the two sides cannot agree during the handshake.

## 0.21.0 - 2026-07-27

## 0.20.0 - 2026-07-25

- **Security (medium): bounded result allocation.** A hostile or MITM'd server
  could declare 10,000,000 single-column rows backed by a ~40 MB frame of empty
  cells and make the client allocate roughly 1.9 GB (measured), since a cell
  costs 4 bytes on the wire but far more as a JS value. Result decoding now
  enforces `MAX_RESULT_CELLS` (2,000,000 cells per frame) on both the legacy and
  native paths. Results larger than that must be paged with `limit`/`offset`.
- `queryTyped` is now generic and takes positional `$N` parameters:
  `queryTyped<User>(q, schema, params?, opts?)`. Typed rows and injection-safe
  binding were previously mutually exclusive. The old
  `queryTyped(q, schema, opts?)` form is unchanged.
- New `queryObjects<Row>(query, params?, opts?)`: object rows keyed by column
  name on the LOSSLESS native path, so no schema is needed (bytes stay bytes,
  JSON stays recursive data, out-of-range integers stay `bigint`).
  `querySqlObjects<Row>(query, opts?)` is the SQL counterpart. New `NativeRow`
  type.
- New SQL composition helpers in `escape.ts`: `sql` tagged template,
  `sqlIdent`, `escapeSqlLiteral`, `escapeSqlIdent`. PowDB's SQL frontend has NO
  parameter binding on any wire frame, so string concatenation was previously
  the only way to build SQL with user input. These escape for PowDB's own SQL
  lexer (`''` doubling plus backslash escaping, identifiers validated rather
  than quoted). Escaping is weaker than binding: prefer PowQL `$N` parameters
  for untrusted input.

## 0.19.1 - 2026-07-24

- `SUPPORTED_CATALOG_VERSION` raised from 6 to 7: sync/replica clients are now
  accepted by servers whose catalog activated v7 (entity links, appended links
  section before the CRC). The client treats catalog payloads as opaque bytes,
  so no decoding change; `assertServerCatalogVersionSupported` simply accepts
  v7. Engine-side 0.19.1 also ships link introspection (`schema links`,
  extended `describe`) whose results flow through the existing `query` path.

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
