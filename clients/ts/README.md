# @zvndev/powdb-client

TypeScript client for [PowDB](https://github.com/zvndev/powdb) — speaks the native binary wire protocol over TCP (or TLS).

## Install

```bash
npm install @zvndev/powdb-client
```

## Usage

```typescript
import { Client, powql, ident } from "@zvndev/powdb-client";

const client = await Client.connect({
  host: "localhost",
  port: 5433,
});

// Create a table
await client.query("type User { required name: str, required email: str, age: int }");

// Insert data — use `powql` to interpolate values safely
const name = 'O"Brien';
const age = 30;
await client.query(powql`insert User { name := ${name}, email := ${"alice@example.com"}, age := ${age} }`);

// Query
const result = await client.query(powql`User filter .age > ${25} { .name, .age }`);
if (result.kind === "rows") {
  console.table(result.rows);
}

// Aggregates
const count = await client.query(powql`count(${ident("User")})`);
if (count.kind === "scalar") {
  console.log(`Total users: ${count.value}`);
}

await client.close();
```

## Safe query composition

**Never build PowQL with template literals or string concatenation of untrusted values.** PowQL has its own injection class — the same risk as SQL injection.

Use the `powql` tagged template. Values are escaped as literals by default; wrap identifiers (table/column names) in `ident(...)`.

```typescript
import { powql, ident, escapeLiteral, escapeIdent } from "@zvndev/powdb-client";

// powql — recommended. Interpolations are escaped automatically.
const q = powql`${ident("User")} filter .city = ${city} and .age > ${age} { .name }`;

// Manual escaping — for when you need raw strings.
escapeLiteral("O\"Brien");  // → "\"O\\\"Brien\""
escapeIdent("User");        // → "User" (throws on invalid)
```

`escapeLiteral` accepts `string | number | bigint | boolean | null`. It rejects `NaN`/`Infinity`, `undefined`, objects, arrays, symbols, and `Date` — convert those yourself before passing them in.

### Parameter binding (`$N`)

For the strongest separation between code and data, pass values as positional `$N` parameters instead of interpolating them. The server binds each placeholder at the **token level** — a string becomes a literal token, never interpolated text — so an injection-shaped value is inert and can never change the query's shape. Placeholders are 1-based (`?` is not a placeholder; `??` is the COALESCE operator).

```typescript
// Values are passed as the second argument, in $1, $2, … order.
await client.query("insert User { name := $1, email := $2, age := $3 }", [
  name,
  email,
  age,
]);

const r = await client.query("User filter .email = $1 { .name }", [email]);

// null binds PowQL null; numbers bind as int when integral, float otherwise;
// bigint always binds as int.
await client.query("insert User { name := $1, age := $2 }", ["Dana", null]);
```

`QueryParam` is `string | number | bigint | boolean | null`. The params form sends the `QueryWithParams` (0x04) wire message and **requires powdb-server >= 0.4.7**. The plain `query(q)` and `query(q, { signal })` forms are unchanged.

## Multi-statement scripts (`execScript`)

`execScript` runs a whole `;`-separated PowQL script down one connection,
**pipelined**: every statement is written back-to-back without waiting for
the previous reply, so an N-statement script costs one round trip instead of
N. Splitting is statement-aware with the exact semantics of the CLI's script
path — `;` inside `"..."` string literals or `#` comments never splits, and
empty statements are dropped. (The splitter is exported as
`splitStatements(script)` if you need it standalone.)

```typescript
const results = await client.execScript(`
  type User { required name: str, age: int };
  insert User { name := "Alice", age := 30 };  # seed; more to come
  insert User { name := "Bob;Bobby", age := 25 };
  count(User)
`);
// results: QueryResult[] in statement order — results[3].value === "2"
```

By default execution is **fail-fast**: the first failed statement rejects the
promise with a `PowDBScriptError` carrying `statementIndex`, the failing
`statement` text, and the successful `results` so far. Because dispatch is
pipelined, statements already on the wire when the error reply arrives
(typically the whole script) still execute server-side — use
`transactional: true` if you need all-or-nothing behavior. Do **not** embed
`begin`/`commit` in the script yourself: the trailing `commit` is already on
the wire when an error reply arrives, so it commits the partial work.

With `transactional: true`, `execScript` opens the transaction itself, waits
for every statement's reply, and only then sends `commit` — or `rollback` if
any statement failed — so no statement's effect survives a failure.
Transactional scripts may not contain their own `begin`/`commit`/`rollback`,
and the option is mutually exclusive with `continueOnError`. On failure the
`PowDBScriptError`'s `results` are the replies received before rollback —
their effects are not persisted.

```typescript
// Either every statement commits, or none do.
await client.execScript(seedScript, { transactional: true });
```

```typescript
import { isPowDBScriptError } from "@zvndev/powdb-client";

try {
  await client.execScript(script);
} catch (err) {
  if (isPowDBScriptError(err)) {
    console.error(`statement ${err.statementIndex} failed: ${err.statement}`);
    console.error(`${err.results.length} statements succeeded before it`);
  }
}
```

With `continueOnError: true`, every statement runs regardless of failures and
you get a dense per-statement outcome array instead of a throw:

```typescript
const outcomes = await client.execScript(script, { continueOnError: true });
for (const o of outcomes) {
  if (o.ok) console.log(o.result.kind);
  else console.warn(`failed: ${o.statement} — ${o.error.message}`);
}
```

`Pool.execScript(script, opts?)` does the same through a pool, checking out a
single connection for the whole script (so statement order — and
`transactional: true` — holds).

## Eager (pipelined) connect

`Client.connect` normally performs a blocking Connect→ConnectOk handshake, so
every fresh connection pays a protocol round trip before its first query.
With `eager: true` the promise resolves as soon as the socket is open and the
Connect frame is written; queries issued immediately are queued right behind
the Connect frame on the wire, and the server answers everything in order.
For connection-per-request callers this removes the handshake round trip
entirely:

```typescript
const client = await Client.connect({ host, port, eager: true });
// No extra round trip — this query rides directly behind the Connect frame.
const result = await client.query("count(User)");
```

If the handshake then fails (bad password, protocol mismatch), every queued
query rejects with the handshake error (`code: "auth_failed"`, etc.) and the
client closes. Two things to know:

- `client.serverVersion` is `""` until the ConnectOk arrives.
- `await client.ready()` waits for the handshake outcome explicitly — it
  resolves on ConnectOk and rejects with the handshake error. (For non-eager
  clients it has already settled by the time `connect` returns.)

## Experimental embedded sync protocol helpers

The client exposes experimental low-level helpers for the private authenticated
embedded-sync frames used by the in-progress replica product:

```typescript
client.on("sync", ({ operation, durationMs, ok, stale, repairAction }) => {
  metrics.histogram("powdb_sync_ms", durationMs, {
    operation,
    ok: String(ok),
    repairAction: repairAction ?? "error",
  });
});

const status = await client.syncStatus("replica-a");

if (status.repairAction === "pull" && status.lastAppliedLsn !== null) {
  const pull = await client.syncPull({
    replicaId: "replica-a",
    sinceLsn: status.lastAppliedLsn,
    maxUnits: 4096,
    maxBytes: 16n * 1024n * 1024n,
    databaseId: "0123456789abcdeffedcba9876543210",
    primaryGeneration: 1n,
    walFormatVersion: 1,
    catalogVersion: 1,
    segmentFormatVersion: 1,
  });

  // Apply `pull.units` with the embedded replica layer, then acknowledge.
  const lastUnit = pull.units.at(-1);
  if (lastUnit) {
    await client.syncAck({
      replicaId: "replica-a",
      appliedLsn: lastUnit.lsn,
      remoteLsn: pull.status.remoteLsn,
    });
  }
}
```

These methods are intentionally not a complete or stable
`@zvndev/powdb-sync` package. They expose the authenticated server
pull/status/ack protocol so the embedded replica layer can be built and tested
without hand-rolled frame code. Pin matching client/server versions while using
them; the stable product boundary will be the sync package. Servers must have
sync enabled and reject unauthenticated or `readonly` users.

## Authentication

For servers using the legacy shared password (`POWDB_PASSWORD`), pass
`password` alone. For servers with named users (multi-user mode), pass
`user` + `password`:

```typescript
const client = await Client.connect({
  host: "localhost",
  port: 5433,
  user: "alice",          // named user (multi-user mode)
  password: "s3cret",
});
```

Roles are enforced server-side: a `readonly` user's writes are rejected with
a `PowDBError` (`code: "query_failed"`, message containing `permission
denied`) — the connection stays usable for reads. A missing or wrong
`user`/`password` rejects the handshake with `code: "auth_failed"`.

Version matrix for multi-user mode:

| | Requirement |
|---|---|
| Client | ≥0.4.0 (adds the `user` option) |
| Server | ≥0.4.6 (enforces roles; 0.4.5 accepted usernames but did not enforce `readonly`) |

When `user` is omitted the Connect frame is byte-identical to the 0.3.x
client's, so legacy shared-password and no-auth servers work unchanged.

## Connection pooling

For multi-query workloads (web servers, batch jobs), use `Pool`:

```typescript
import { Pool } from "@zvndev/powdb-client";

const pool = new Pool({
  host: "localhost",
  port: 5433,
  max: 10,
});

// Acquire, use, release — or let `withClient` handle it.
const rows = await pool.withClient(async (client) => {
  const r = await client.query("User { .name }");
  return r.kind === "rows" ? r.rows : [];
});

await pool.close();
```

## TLS

```typescript
const client = await Client.connect({
  host: "db.example.com",
  port: 5433,
  tls: true,                        // system defaults
  // or: tls: { ca: fs.readFileSync("ca.pem") }
});
```

## Typed rows

The wire protocol serialises every value as a string. If you want JS types
back (numbers, `Date`, booleans), call `queryTyped` with a schema:

```typescript
import { Client } from "@zvndev/powdb-client";

const client = await Client.connect({ host: "localhost", port: 5433 });

const rows = await client.queryTyped(
  "User { .id, .name, .age, .active, .created_at, .prefs }",
  {
    id: "int",        // number — or bigint if > Number.MAX_SAFE_INTEGER
    age: "int",
    active: "bool",
    created_at: "datetime",
    prefs: "json",    // parsed with JSON.parse into an object/array/scalar
    // columns not in the schema (like `name`) pass through as strings
  },
);

rows[0].age;         // typeof number
rows[0].created_at;  // instanceof Date
rows[0].prefs;       // parsed JSON value (object, array, or scalar)
```

Supported column types: `int` | `float` | `bool` | `str` | `datetime` | `uuid` | `json`.

`json` columns render as canonical JSON text on the wire (keys sorted
bytewise, no whitespace) and are parsed with `JSON.parse`. A JSON `null`
document decodes to `null`. Unlike the other typed columns, a parse failure
does not throw: it returns the raw string, so a malformed or non-canonical
cell can never turn a readable result into an exception (canonical server
output always parses, so this fallback is inert in normal use).

Bytes columns are intentionally unsupported (the wire format is lossy —
it renders `<N bytes>`) and throw on coercion. Declare `str` if you just
want the placeholder.

## Structured errors

Every error thrown by the client is a `PowDBError` with a stable `.code`:

```typescript
import { Client, PowDBError, isPowDBError } from "@zvndev/powdb-client";

try {
  await client.query("bogus");
} catch (err) {
  if (isPowDBError(err)) {
    switch (err.code) {
      case "connect_failed":
      case "timeout":
        // transient — safe to retry
        break;
      case "auth_failed":
      case "query_failed":
      case "protocol_error":
        // not transient — surface to the caller
        break;
      case "aborted":
        // caller asked to stop — never retry
        break;
    }
  }
}
```

The full taxonomy: `connect_failed`, `auth_failed`, `query_failed`,
`aborted`, `size_exceeded`, `protocol_error`, `closed`, `timeout`,
`type_coercion_failed`.

`execScript` failures throw `PowDBScriptError`, a `PowDBError` subclass whose
`code` mirrors the failing statement's error and which adds
`statementIndex`, `statement`, and `results` (see Multi-statement scripts
above). Narrow with `isPowDBScriptError(err)`.

## Polling watch

For simple change-polling (the server doesn't ship a subscription
protocol yet), `watch` re-runs a query on an interval and invokes a
callback with the latest rows:

```typescript
const handle = client.watch("User filter .active = true { .id, .name }", {
  intervalMs: 1000,
  onRows: (result) => {
    if (result.kind === "rows") {
      console.log(`${result.rows.length} active users`);
    }
  },
  onError: (err) => {
    console.error("watch error:", err);
  },
});

// ...later
handle.stop();
```

If a query takes longer than `intervalMs` the next tick is skipped rather
than piling up. The watcher does not keep the event loop alive on its own
(it uses `timer.unref()`).

## Observability

`Client` is an `EventEmitter`. Wire it into your logger or metrics
pipeline:

```typescript
client.on("query", ({ query, durationMs, ok, kind, error }) => {
  metrics.histogram("powdb_query_ms", durationMs, { ok: String(ok) });
});

client.on("close", ({ error }) => {
  if (error) logger.warn({ err: error }, "powdb connection lost");
});
```

Events:

| Event | Payload | Fires when |
|---|---|---|
| `query` | `{ query, durationMs, ok, kind?, error? }` | After every query completes (success or failure) |
| `sync` | `{ operation, replicaId, durationMs, ok, status?, stale?, repairAction?, units?, advanced?, error? }` | After every sync status/pull/ack request completes |
| `close` | `{ error: Error \| null }` | Exactly once per socket, on normal or error close |

## Cancellation

Pass an `AbortSignal` to cancel a query:

```typescript
import { isPowDBError } from "@zvndev/powdb-client";

const ctrl = new AbortController();
setTimeout(() => ctrl.abort(), 1000);

try {
  await client.query("slow_query(...)", { signal: ctrl.signal });
} catch (err) {
  if (isPowDBError(err) && err.code === "aborted") { /* cancelled */ }
}
```

A plain `ctrl.abort()` throws a `PowDBError` with `code === "aborted"`. If you
abort with a custom `Error` reason (`ctrl.abort(myError)`), that error is thrown
as-is; any non-`Error` reason is wrapped in a `PowDBError` (`code === "aborted"`).

The socket stays open — the aborted query's reply is silently discarded when it
arrives, and every other in-flight query still receives its own result.

## API

### `Client.connect(options)`

Returns a `Promise<Client>`. Options:

| Option | Type | Default | Description |
|---|---|---|---|
| `host` | `string` | *(required)* | Server hostname or IP |
| `port` | `number` | *(required)* | Server port |
| `dbName` | `string` | `"default"` | Database name |
| `password` | `string \| null` | `null` | Password — shared (`POWDB_PASSWORD`) or the named user's |
| `user` | `string` | *(omitted)* | User name for multi-user servers (see Authentication above). Omit for shared-password / no-auth servers |
| `connectTimeoutMs` | `number` | `5000` | Connection timeout in milliseconds |
| `tls` | `boolean \| tls.ConnectionOptions` | `false` | Enable TLS; `true` uses system defaults, or pass a `tls.connect` options object |
| `eager` | `boolean` | `false` | Resolve as soon as the Connect frame is written instead of waiting for ConnectOk; queries pipeline behind the handshake (see Eager connect above) |

> **Multi-user servers:** requires client ≥0.4.0 (`user` option) and server
> ≥0.4.6 (enforced roles). See the version matrix under Authentication.

### `client.query(query, params?, opts?)`

Sends a PowQL query and returns a `Promise<QueryResult>`:

- `{ kind: "rows", columns: string[], rows: string[][] }` — for SELECT-like queries
- `{ kind: "scalar", value: string }` — for aggregates (`count`, `sum`, `avg`, etc.)
- `{ kind: "ok", affected: bigint }` — for mutations (`insert`, `update`, `delete`)

`params?: QueryParam[]` — positional values bound to `$1`, `$2`, … placeholders (see Parameter binding above; requires server ≥0.4.7). When omitted, the plain query path is used. The legacy two-argument `query(q, { signal })` form is still accepted — an array second argument is treated as params, an object as options.

`opts.signal?: AbortSignal` — aborts the returned promise (see Cancellation above).

Throws a `PowDBError` (see Structured errors above) on any failure.

### `client.querySql(query, opts?)`

Runs a statement through the server's **SQL frontend** and returns a
`Promise<QueryResult>` — the same result shape as `query()`. The plain
`query()` method stays PowQL for wire compatibility; `querySql()` sends the
dedicated `QuerySql` wire message. Requires server ≥0.5.0 (the SQL frontend was
added in 0.5.0); see `docs/SQL.md` for the supported subset. `opts.signal?:
AbortSignal` cancels the query (see Cancellation above).

```typescript
const result = await client.querySql("SELECT name, age FROM User WHERE age > 27");
```

### `client.queryTyped(query, schema, opts?)`

Like `query()`, but coerces each row's string values to JS types using the
supplied schema and returns `Promise<TypedRow[]>`. See Typed rows above.

### `client.execScript(script, opts?)`

Splits `script` into statements (statement-aware — see Multi-statement
scripts above) and executes them all down this connection, pipelined.
Returns `Promise<QueryResult[]>` in statement order; rejects with a
`PowDBScriptError` on the first failed statement. With
`opts.transactional: true`, runs the whole script atomically (commit only
after every reply; rollback on any failure; embedded
`begin`/`commit`/`rollback` rejected; mutually exclusive with
`continueOnError`). With `opts.continueOnError: true`, returns
`Promise<ScriptStatementOutcome[]>` (`{ statement, ok: true, result }` or
`{ statement, ok: false, error }`) and never rejects for statement failures.
`opts.signal?: AbortSignal` aborts the remaining statements.

### `client.ready()`

Resolves once the Connect→ConnectOk handshake has completed; rejects with
the handshake error if it failed. Only interesting for eager connects — for
default connects it has settled before `Client.connect` returns.

### `client.watch(query, options)`

Re-runs `query` every `intervalMs` and passes each `QueryResult` to
`onRows`. Returns `{ stop(): void }`. See Polling watch above.

### `client.on("query", handler)` / `client.on("close", handler)`

`Client` extends `EventEmitter`. See Observability above.

### `client.close()`

Sends a disconnect message and closes the TCP socket.

### `client.serverVersion`

The PowDB server version string (e.g., `"0.4.4"`). On connect, the client warns once per `host:port` if the server's major version differs from the client's. For an eager connect this is `""` until the ConnectOk arrives (await `client.ready()` to guarantee it is populated).

### `Pool` (class)

Constructor options extend `ClientOptions` with:

| Option | Type | Default | Description |
|---|---|---|---|
| `max` | `number` | `10` | Maximum concurrent connections |
| `acquireTimeoutMs` | `number` | `30000` | How long `acquire()` waits before rejecting (pass `0` to disable) |
| `connectRetries` | `number` | `3` | How many times to retry transient connect failures before giving up (set `0` to disable) |
| `connectBackoffMs` | `number` | `100` | Initial delay between connect retries; doubles each attempt |
| `connectMaxBackoffMs` | `number` | `2000` | Cap on the exponential backoff |

Methods: `acquire()`, `release(client)`, `destroy(client)`, `withClient(fn)`, `execScript(script, opts?)`, `close()`.
Getters: `size`, `idle`, `closed`.

### Safety helpers

- `powql` — tagged template; escapes literals, validates identifiers
- `ident(name)` — wrap a string so `powql` treats it as an identifier
- `escapeLiteral(value)` — render a JS value as a PowQL literal
- `escapeIdent(name)` — validate an identifier (throws `TypeError` on invalid)
- `splitStatements(script)` — the statement-aware script splitter used by `execScript`

## Limits

The client enforces the same frame limits as the server and throws on violation:

- `MAX_PAYLOAD_SIZE` — 64 MiB per frame
- `MAX_ROWS` — 10,000,000 rows per result
- `MAX_COLUMNS` — 4,096 columns per result

## Requirements

- Node.js 18+ (uses `node:net`, `node:tls`)
- A running PowDB server (`cargo run --release -p powdb-server`)

For local development, `npm test` and `npm run test:pool` start a disposable
PowDB server automatically when `POWDB_PORT` is not set. Set `POWDB_HOST` and
`POWDB_PORT` to run those tests against an existing server.

## License

MIT
