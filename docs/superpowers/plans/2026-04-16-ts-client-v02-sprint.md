# Sprint Plan — PowDB TS Driver v0.2
Generated: 2026-04-16
Based on: conversation review of `clients/ts/` (see session thread)

## Sprint Goal
Ship a driver that is safe to put in front of untrusted input and robust enough for production TCP conditions (NAT, TLS, backpressure, large responses).

## Success Criteria
- [ ] PowQL injection is prevented by a first-class safe-interpolation API
- [ ] Client-side limits match server-side (`MAX_PAYLOAD`, `MAX_ROWS`, `MAX_COLUMNS`) — untrusted server can't OOM Node
- [ ] Frame reassembly is not O(n²) on chunk count
- [ ] Optional TLS connection (`tls: true`)
- [ ] `setKeepAlive(true, 30_000)` on the socket — long-lived conns survive NAT
- [ ] `AbortSignal` support on `Client.query()`
- [ ] Simple `Pool` for concurrent workloads
- [ ] `engines` field in `package.json`
- [ ] README documents the new APIs; demo updated to use `powql` tagged template

## Out of scope (by design)
- Streaming result cursors — needs server-side chunked row delivery (protocol v2)
- Auto-typed rows — server sends strings only; type coercion at the client is heuristic and wrong often enough to be worse than `parseInt`
- Auto-reconnect — needs idempotency classification; design-heavier than this sprint fits

## Dev Tracks

### Track 1 — Client internals rework
**Files owned:** `src/index.ts`, `src/protocol.ts`, additions to `test/client.test.ts`
**Tasks:**
- [ ] T1-01 (P1): Fix O(n²) frame reassembly in `onData()`. Use a list-of-buffers + byteLength counter; only `Buffer.concat` when `tryDecode` needs contiguous bytes across chunk boundaries.
- [ ] T1-02 (P1): Enforce client-side limits in `protocol.ts#decodePayload`: `MAX_PAYLOAD_SIZE = 64 * 1024 * 1024`, `MAX_ROWS = 10_000_000`, `MAX_COLUMNS = 4096`. Match server `crates/server/src/protocol.rs` constants. Throw a descriptive error, don't silently truncate.
- [ ] T1-03 (P1): Add optional `tls: boolean | tls.ConnectionOptions` to `ClientOptions`. When true, `openSocket` uses `tls.connect` instead of `net.Socket`. Keep the same timeout semantics.
- [ ] T1-04 (P2): Call `socket.setKeepAlive(true, 30_000)` after the `connect` event so NAT/LB idle timeouts don't silently kill long-lived conns.
- [ ] T1-05 (P2): Add `signal?: AbortSignal` to `Client.query()`. If the signal fires before the reply arrives, reject the pending promise with a `DOMException("aborted", "AbortError")` and keep the socket alive (the server reply will still arrive and be discarded — document this).
- [ ] T1-06 (P3): Log a warning if `serverVersion`'s major differs from the client's `package.json` major. Keep the connection.

**Do NOT touch:**
- `src/escape.ts` (Track 2 creates)
- `src/pool.ts` (Track 2 creates)
- `README.md`, `package.json`, `demo/demo.ts` (lead architect handles on main)

### Track 2 — Additive safety + pool
**Files owned:** `src/escape.ts` (new), `src/pool.ts` (new), `test/escape.test.ts` (new), `test/pool.test.ts` (new)
**Tasks:**
- [ ] T2-01 (P0): Create `src/escape.ts` exporting:
  - `escapeLiteral(value: string | number | boolean | null): string` — safely renders scalars for PowQL string/numeric contexts. Strings: `"` → `""` and wrap in `"..."`. Numbers: `Number.isFinite` check. Null: `null`. Bool: `true`/`false`. Throws on any other type.
  - `escapeIdent(name: string): string` — validate as `[A-Za-z_][A-Za-z0-9_]*`, throw otherwise. Do NOT wrap in anything; PowQL identifiers aren't quotable.
  - `powql` tagged template function: `` powql`insert ${users} { name := ${name} }` `` — interpolates idents for identifier positions and literals for value positions. Identifiers can't be distinguished from literals positionally, so the ergonomic rule: anything passed to `powql` runs through `escapeLiteral`; identifier interpolation requires `powqlIdent(name)` wrapper to opt in.
- [ ] T2-02 (P0): `test/escape.test.ts` — table-driven cases: `"`-injection string, embedded backslashes, `null`, non-finite numbers, object/array rejection, identifier regex boundary cases. Use `node:test` or `node:assert` to match the existing test style (no framework).
- [ ] T2-03 (P1): Create `src/pool.ts` exporting a `Pool` class:
  - `constructor(opts: ClientOptions & { max?: number /* default 10 */; min?: number /* default 0 */ })`
  - `async acquire(): Promise<Client>` — returns from idle pool or creates new up to `max`; waits otherwise.
  - `release(c: Client): void` — returns to idle. If client's socket is dead, discard instead.
  - `async withClient<T>(fn: (c: Client) => Promise<T>): Promise<T>` — acquire/release wrapper.
  - `async close(): Promise<void>` — drains all idle, rejects pending acquires.
  - Track live count + idle; tests for acquire-over-max blocking, release returning client, broken-client discard, `close` draining.
- [ ] T2-04 (P1): `test/pool.test.ts` — unit tests that mock out `Client.connect` where possible (or use a local harness). Skip e2e pool test if no live server.

**Do NOT touch:**
- `src/index.ts` — Track 1 owns
- `src/protocol.ts` — Track 1 owns
- Any existing file — Track 2 is additive only

### Lead architect (handles on main while tracks run)
- README — document `tls`, `signal`, `powql`, `Pool`
- `package.json` — add `"engines": { "node": ">=18" }`, bump version to `0.2.0`
- `demo/demo.ts` — replace template-literal interpolation with `powql` tagged template to model the safe pattern
- `CHANGELOG.md` (repo root) — add 0.2.0 entry

## Review slate (after merge)
- PM agent — every task against the code
- Security agent — re-verify injection is closed; check TLS/AbortSignal for foot-guns
- Code quality agent — lint, no regressions, patterns match

## File conflict matrix
| File | Track 1 | Track 2 | Architect |
|------|:---:|:---:|:---:|
| `src/index.ts` | X | | |
| `src/protocol.ts` | X | | |
| `src/escape.ts` (new) | | X | |
| `src/pool.ts` (new) | | X | |
| `test/client.test.ts` | X | | |
| `test/escape.test.ts` (new) | | X | |
| `test/pool.test.ts` (new) | | X | |
| `README.md` | | | X |
| `package.json` | | | X |
| `demo/demo.ts` | | | X |
| `CHANGELOG.md` | | | X |

Zero overlap.
