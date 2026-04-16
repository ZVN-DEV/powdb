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

## Cancellation

Pass an `AbortSignal` to cancel a query:

```typescript
const ctrl = new AbortController();
setTimeout(() => ctrl.abort(), 1000);

try {
  await client.query("slow_query(...)", { signal: ctrl.signal });
} catch (err) {
  if (err.name === "AbortError") { /* cancelled */ }
}
```

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

Throws on server errors.

### `client.close()`

Sends a disconnect message and closes the TCP socket.

### `client.serverVersion`

The PowDB server version string (e.g., `"0.2.0"`). On connect, the client warns once per `host:port` if the server's major version differs from the client's.

### `Pool` (class)

Constructor options extend `ClientOptions` with:

| Option | Type | Default | Description |
|---|---|---|---|
| `max` | `number` | `10` | Maximum concurrent connections |
| `acquireTimeoutMs` | `number` | `30000` | How long `acquire()` waits before rejecting (pass `0` to disable) |

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

## License

MIT
