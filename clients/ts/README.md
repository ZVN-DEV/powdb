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
  "User { .id, .name, .age, .active, .created_at }",
  {
    id: "int",        // number — or bigint if > Number.MAX_SAFE_INTEGER
    age: "int",
    active: "bool",
    created_at: "datetime",
    // columns not in the schema (like `name`) pass through as strings
  },
);

rows[0].age;         // typeof number
rows[0].created_at;  // instanceof Date
```

Supported column types: `int` | `float` | `bool` | `str` | `datetime` | `uuid`.
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

If you abort with a custom reason (`ctrl.abort(myError)`) and that reason is
an `Error`, it is thrown as-is instead. Otherwise the client throws a
`PowDBError` with `code === "aborted"`.

The socket stays open — the server's reply is silently discarded so other in-flight queries keep working.

## API

### `Client.connect(options)`

Returns a `Promise<Client>`. Options:

| Option | Type | Default | Description |
|---|---|---|---|
| `host` | `string` | *(required)* | Server hostname or IP |
| `port` | `number` | *(required)* | Server port |
| `dbName` | `string` | `"default"` | Database name |
| `password` | `string \| null` | `null` | Server password (if auth is enabled) |
| `connectTimeoutMs` | `number` | `5000` | Connection timeout in milliseconds |
| `tls` | `boolean \| tls.ConnectionOptions` | `false` | Enable TLS; `true` uses system defaults, or pass a `tls.connect` options object |

### `client.query(query, opts?)`

Sends a PowQL query and returns a `Promise<QueryResult>`:

- `{ kind: "rows", columns: string[], rows: string[][] }` — for SELECT-like queries
- `{ kind: "scalar", value: string }` — for aggregates (`count`, `sum`, `avg`, etc.)
- `{ kind: "ok", affected: bigint }` — for mutations (`insert`, `update`, `delete`)

`opts.signal?: AbortSignal` — aborts the returned promise (see Cancellation above).

Throws a `PowDBError` (see Structured errors above) on any failure.

### `client.queryTyped(query, schema, opts?)`

Like `query()`, but coerces each row's string values to JS types using the
supplied schema and returns `Promise<TypedRow[]>`. See Typed rows above.

### `client.watch(query, options)`

Re-runs `query` every `intervalMs` and passes each `QueryResult` to
`onRows`. Returns `{ stop(): void }`. See Polling watch above.

### `client.on("query", handler)` / `client.on("close", handler)`

`Client` extends `EventEmitter`. See Observability above.

### `client.close()`

Sends a disconnect message and closes the TCP socket.

### `client.serverVersion`

The PowDB server version string (e.g., `"0.4.4"`). On connect, the client warns once per `host:port` if the server's major version differs from the client's.

### `Pool` (class)

Constructor options extend `ClientOptions` with:

| Option | Type | Default | Description |
|---|---|---|---|
| `max` | `number` | `10` | Maximum concurrent connections |
| `acquireTimeoutMs` | `number` | `30000` | How long `acquire()` waits before rejecting (pass `0` to disable) |
| `connectRetries` | `number` | `3` | How many times to retry transient connect failures before giving up (set `0` to disable) |
| `connectBackoffMs` | `number` | `100` | Initial delay between connect retries; doubles each attempt |
| `connectMaxBackoffMs` | `number` | `2000` | Cap on the exponential backoff |

Methods: `acquire()`, `release(client)`, `destroy(client)`, `withClient(fn)`, `close()`.
Getters: `size`, `idle`, `closed`.

### Safety helpers

- `powql` — tagged template; escapes literals, validates identifiers
- `ident(name)` — wrap a string so `powql` treats it as an identifier
- `escapeLiteral(value)` — render a JS value as a PowQL literal
- `escapeIdent(name)` — validate an identifier (throws `TypeError` on invalid)

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
