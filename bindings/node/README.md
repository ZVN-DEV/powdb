# @zvndev/powdb-embedded

Embedded [PowDB](https://github.com/zvndev/powdb) for Node — run the database
engine **in-process**, no server and no socket. The SQLite-shaped front door to
PowDB: a single function call, no network round-trip, works offline.

```js
import { Database } from "@zvndev/powdb-embedded";

const db = Database.open("./data");

db.query("type User { required name: str, age: int }");
const inserted = db.query(`insert User { name := "Ada", age := 36 } returning`);
const rows = db.query("User { name, age } filter .age > 18");
const count = db.querySql("SELECT count(*) FROM User"); // SQL frontend too
```

## API

- `Database.open(dir)` — open or create a database at `dir`.
- `Database.openWithMemoryLimit(dir, limitBytes)` — open with an explicit
  per-query memory budget (caps sort/join/GROUP BY materialization).
- `db.query(powql)` — run a PowQL statement.
- `db.querySql(sql)` — run a SQL statement (lowered to PowQL).
- `db.queryReadonly(powql)` — run a read-only statement.
- `db.setSyncMode(mode)` — set WAL durability: `"full"` | `"normal"` | `"off"`.
- `db.isPoisoned()` — `true` if a previous call panicked (reopen the database).

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

## When to use embedded vs the server

- **Embedded** (this package): one process, in-process speed, local-first apps,
  CLIs, desktop/mobile, tests. Like SQLite.
- **Server** (`@zvndev/powdb-client`): many clients over the network, shared
  database. Like Postgres.

Same engine, two front doors.

## Safety

A query that panics is caught at the boundary and surfaced as a thrown JS error
— it never aborts the host process. After an internal panic the handle is
poisoned; reopen the database (committed data is recovered from the WAL).

## License

MIT
