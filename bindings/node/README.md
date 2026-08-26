# @zvndev/powdb-embedded

Embedded [PowDB](https://github.com/ZVN-DEV/powdb) for Node — run the database
engine **in-process**, no server and no socket. The SQLite-shaped front door to
PowDB: a single function call, no network round-trip, works offline.

```js
import { Database } from "@zvndev/powdb-embedded";

const db = Database.open("./data");

db.query("type User { required name: str, age: int }");
const inserted = db.query(`insert User { name := "Ada", age := 36 } returning`);
const rows = db.query("User filter .age > 18 { .name, .age }");
const count = db.querySql("SELECT count(*) FROM User"); // SQL frontend too
```

## API

- `Database.open(dir)` — open or create a database at `dir`.
- `Database.openWithMemoryLimit(dir, limitBytes)` — open with an explicit
  per-query memory budget (caps sort/join/GROUP BY materialization).
- `Database.openReadOnly(dir)`: open a **quiescent** directory (a restored
  backup or a checkpointed replica) read-only for snapshot serving. Reads work;
  every mutation throws a terminal read-only error, and the directory is never
  mutated. N read-only handles across processes can serve the same directory
  concurrently. A non-empty (unrecovered) WAL is refused. See
  [Read-only snapshot serving](https://github.com/ZVN-DEV/powdb/blob/main/docs/read-only-serving.md).
- `Database.openReadOnlyWithMemoryLimit(dir, limitBytes)`: read-only open with
  an explicit per-query memory budget.
- `db.query(powql)`: run a PowQL statement (string-typed cells).
- `db.querySql(sql)` — run a SQL statement (lowered to PowQL).
- `db.queryReadonly(powql)` — run a read-only statement.
- `db.queryNative(powql)`: run a PowQL statement and get **lossless typed**
  cells (see [Typed results](#typed-lossless-results)).
- `db.querySqlNative(sql)`: typed variant of `querySql`.
- `db.queryReadonlyNative(powql)`: typed variant of `queryReadonly`.
- `db.queryWithParams(powql, params)`: run PowQL with positional `$1..$N`
  parameters and get typed cells. Supported param types: `number`, `bigint`,
  `string`, `boolean`, `null`.
- `db.applyRetainedUnits(request)` — apply one sync retained-unit chunk from
  `@zvndev/powdb-client` to a bootstrapped embedded replica.
- `db.setSyncMode(mode)` — set WAL durability: `"full"` | `"normal"` | `"off"`.
- `db.isPoisoned()` — `true` if a previous call panicked (reopen the database).
- `db.close()` — flush, checkpoint, and release the data-directory lock. See
  below.

Opening the same directory twice in one process throws — a single process must
share one handle, not two engines over the same files.

## Closing

`db.close()` flushes and checkpoints the database (unless the handle is
poisoned), then releases the data-directory lock so another process — or
another handle in this one — can open it. Any call after `close()` throws
`database is closed`; closing twice throws the same error.

```js
const db = Database.open("./data");
db.query("type T { required id: int }");
db.close(); // deterministic flush + lock release

Database.open("./data"); // now free to reopen (same or another process)
```

Closing is optional — dropping the last reference lets the garbage collector
run the same cleanup — but Node does not guarantee *when* a finalizer runs, so
call `close()` when you need the lock or the final `"normal"`-mode commits
flushed at a known point.

`applyRetainedUnits` is the native adapter used by `@zvndev/powdb-sync` after a
replica has been restored from a sync bootstrap. It expects the database
identity and format metadata from the primary plus the contiguous retained
units returned by `syncPull(...)`. `databaseId` can be either a 32-character
hex string or a 16-byte `Uint8Array`, matching the `@zvndev/powdb-sync`
adapter contract. Retained unit `data` accepts `Uint8Array` or `Buffer` bytes:

```js
const result = db.applyRetainedUnits({
  sinceLsn: 42n,
  databaseId: "00112233445566778899aabbccddeeff",
  primaryGeneration: 1n,
  walFormatVersion: 1,
  catalogVersion: 7, // the newest catalog format this replica can read
  segmentFormatVersion: 1,
  units: pull.units,
});

console.log(result.throughLsn, result.unitsApplied);
```

## Write performance / durability

By default the database runs in `"full"` durability — one `fsync` per commit,
the safest mode, but each write waits on the disk. For write-heavy workloads that
tolerate a small, bounded crash-loss window, switch to `"normal"`: the `fsync`
moves to an off-lock background flusher, so commits return at memory speed.

```js
const db = Database.open("./data");
db.setSyncMode("normal"); // fast writes; bounded crash-loss window
```

- `"full"` (default) — fsync every commit; no loss on crash; slowest writes.
- `"normal"` — background fsync; a crash may lose only the last few ms of
  commits; much faster writes.
- `"off"` — no durability; tests/benchmarks only.

Results match the `@zvndev/powdb-client` `QueryResult` shape, so embedded and
networked code paths are interchangeable:

```ts
type QueryResult =
  | { kind: "rows";    columns: string[]; rows: string[][] }
  | { kind: "scalar";  value: string }
  | { kind: "ok";      affected: bigint }
  | { kind: "message"; message: string };
```

Every cell here is a string, using the same wire rendering as the server. `json`
columns come back as canonical JSON text (keys sorted bytewise, no whitespace),
so `JSON.parse(cell)` reconstructs the document. This string path is lossy by
design: bytes render as a `<N bytes>` placeholder, and a JSON `null`, an SQL
NULL, and the string `"null"` all render identically. When those distinctions
matter, use the typed API below.

## Typed (lossless) results

`queryNative` / `querySqlNative` / `queryReadonlyNative` / `queryWithParams`
return the same result shape but with **typed** cells (`WireValue`), matching
the `@zvndev/powdb-client` `WireValue` union so embedded and networked code read
results the same way:

```ts
type WireValue =
  | { type: "empty" } // a missing / absent cell, distinct from a JSON null
  | { type: "int";      value: bigint }  // full i64 range, never rounded
  | { type: "float";    value: number }
  | { type: "bool";     value: boolean }
  | { type: "str";      value: string }
  | { type: "datetime"; value: bigint }  // microseconds since the Unix epoch
  | { type: "uuid";     value: Uint8Array }  // raw 16 bytes (matches the client)
  | { type: "bytes";    value: Buffer }  // raw bytes, losslessly
  | { type: "json";     value: NativeJson; pj1: Uint8Array }; // parsed + raw PJ1
```

```js
const db = Database.open("./data");
db.query("type Doc { required id: int, body: json }");
db.query(`insert Doc { id := 1, body := "null" }`);

const [cell] = db.queryNative("Doc filter .id = 1 { .body }").rows[0];
// { type: "json", value: null, pj1: <Uint8Array> }: a JSON null,
// which is NOT { type: "empty" } (a missing cell).

// Positional parameters are substituted as literal tokens before parsing, so
// untrusted input can never change the query's shape.
const hits = db.queryWithParams("Doc filter .id = $1 { .id }", [1]);
```

For a `json` cell, `value` is the parsed document. A JSON-internal integer in
JS's safe range is a `number`; one outside it (but within i64) widens to
`bigint`, so its exact value survives, the same rule the networked client uses.
An integer beyond i64 range is the one lossy case (it decodes to a `number`);
for that, read `pj1` instead: it is always the raw canonical PJ1 bytes (the
lossless on-wire form), so a caller can decode the exact value directly.

A `uuid` cell's `value` is the raw 16 bytes (a `Uint8Array`), byte-identical to
the networked `@zvndev/powdb-client` `WireValue`. Render the canonical
`8-4-4-4-12` hex from those bytes if you need the string form.

## Feature parity with the server

Embedded executes statements through the same engine as `powdb-server`, so
engine features (indexes including expression indexes, JSON `->` querying and
ordering/grouping, transactions, `explain`, durability modes, positional
parameters, typed results) behave identically in-process. The differences are
transport-level only: the server adds the network protocol, auth, and
cross-client admission. Use embedded for one-process/local-first workloads and
`@zvndev/powdb-client` when many clients share a database over the network.

## Supported platforms

Prebuilt native binaries ship for:

| Platform | Target triple |
| --- | --- |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| Linux x64 (glibc) | `x86_64-unknown-linux-gnu` |
| Linux arm64 (glibc) | `aarch64-unknown-linux-gnu` |

There is no source fallback, so `require()` throws a load error on any other
platform (Windows, Intel macOS, musl/Alpine). Use the networked
[`@zvndev/powdb-client`](https://github.com/ZVN-DEV/powdb) there instead.

## Safety

A query that panics is caught at the boundary and surfaced as a thrown JS error
— it never aborts the host process. After an internal panic the handle is
poisoned; reopen the database (committed data is recovered from the WAL).

## License

MIT
