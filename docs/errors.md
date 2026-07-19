# Wire error classes

Since v0.17, every error frame (`MSG_ERROR`, tag `0x0A`) the server sends carries a stable one-byte error class after the length-prefixed message string. The class lets clients branch on error *kind* (retry a timeout, surface an auth failure, report a parse error against the query text) without string-matching the message.

## Wire format and compatibility

The `MSG_ERROR` payload is:

```
[message_len: u32 LE][message: UTF-8 bytes][class: u8]   (v0.17+ servers)
[message_len: u32 LE][message: UTF-8 bytes]              (older servers)
```

The class byte is a pure trailing extension, so both directions stay compatible:

- **Old client, new server.** Every first-party decoder (Rust `Message::decode`, the CLI, the TypeScript client) reads the message by its length prefix and ignores trailing payload bytes, so old clients see the same message they always did.
- **New client, old server.** The byte is simply absent; clients treat that as "no class" and fall back to their pre-class behavior.

## Class codes

These numeric values are **stable wire contract**: they are never renumbered or reused. New classes are only appended. Clients must treat unknown values the same as `internal` (0).

| Code | Name | Meaning | Typical causes | TS client `PowDBError.code` |
|------|------|---------|----------------|------------------------------|
| 0 | `internal` | Unclassified or internal server error | Lock poisoning, internal task failure, WAL durability sync failure, protocol misuse, server shutdown notice | `query_failed` |
| 1 | `parse` | The query text failed to lex or parse | Syntax errors, unterminated strings, unsupported constructs, excessive nesting | `query_failed` |
| 2 | `execution` | Planning or execution failed | Unknown table or column, type mismatch, view/index errors, `cannot begin` while a transaction is active | `query_failed` |
| 3 | `timeout` | A time budget elapsed | Per-query timeout, transaction-gate wait timeout, idle-connection timeout | `timeout` |
| 4 | `limit_exceeded` | A memory or size limit was exceeded | Sort/join row caps, per-query memory budget, query text too large, result too large | `size_exceeded` |
| 5 | `readonly_refused` | The server serves a read-only snapshot and the statement requires a writer | Any mutation (or a read that needs a writer, e.g. a stale materialized view) against `powdb-server --readonly` | `query_failed` |
| 6 | `auth_failed` | Authentication or database selection failed at CONNECT time | Wrong password, unknown user, unknown database name | `auth_failed` |
| 7 | `rate_limited` | Too many failed authentication attempts from this address | Repeated bad passwords; wait and retry later | `auth_failed` |
| 8 | `constraint_violation` | A constraint rejected the write | Unique index violation | `query_failed` |
| 9 | `cancelled` | Execution was cancelled cooperatively | The issuing client disconnected mid-query | `query_failed` |

The authoritative Rust definition is `ErrorClass` in `crates/server/src/protocol.rs`; the TypeScript mirror is `WIRE_ERROR_CLASS` in `clients/ts/src/errors.ts`.

## Client-side mapping (TypeScript)

The TypeScript client maps the class onto its existing `PowDBErrorCode` taxonomy (last column above) via `errorCodeForWireClass(...)`, so `err.code` becomes `"timeout"`, `"size_exceeded"`, or `"auth_failed"` where the class warrants it instead of a blanket `"query_failed"`. The raw byte is also exposed as `err.wireErrorClass` for callers that want the finer distinctions (`readonly_refused` and `constraint_violation` both map to `query_failed` but remain distinguishable through the raw class).

Errors from servers that predate the class byte keep the historical behavior: `err.code === "query_failed"` and `err.wireErrorClass === undefined`.

## Message sanitization policy

The class byte is orthogonal to the message *text*. The server only forwards an error message verbatim when it starts with one of a fixed allowlist of safe prefixes (`SAFE_ERROR_PREFIXES` in `crates/server/src/handler.rs`); everything else is replaced with the generic string `query execution error`. This prevents internal details (I/O paths, panic payloads, storage internals) from leaking to clients, while messages derived purely from the client's own query (parse errors, unknown tables, resource-limit guidance) pass through unchanged.

Because the class is derived from the typed error at the point the response is built, never from the message text, a sanitized generic message still carries an accurate class. One internal string is special: `__POWDB_READONLY_NEEDS_WRITE__` is a retry sentinel used inside the server's read-to-write lock escalation and must never cross the wire; a regression test (`crates/server/tests/wire_error_codes.rs`) asserts a read-only server refuses a write with the operator-facing `readonly mode: ...` message and class 5, sentinel-free.
