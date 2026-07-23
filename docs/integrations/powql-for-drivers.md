# PowQL for Driver and ORM Implementers

This is the stable mapping surface for building a database driver or ORM adapter
on PowDB, over either the binary wire protocol or the embedded Node addon. It
tells you the frame layout, the sanctioned lossless result surface, how an ORM's
query builder should lower to PowQL, the null semantics you must surface, the
`explain` contract, the admission and error behavior a driver has to expose, and
the compatibility guarantees you can build on. You should not have to
reverse-engineer any of this from `--help`.

Related references:

- [PowQL language reference](../POWQL.md): the query language you generate.
- [SQL frontend](../SQL.md): the supported SQL subset, if you target SQL instead.
- [`@zvndev/powdb-client` README](../../clients/ts/README.md): the reference wire client.
- [`@zvndev/powdb-embedded` README](../../bindings/node/README.md): the reference in-process addon.

This document is versioned with the workspace. See [Versioning](#versioning) for
the compatibility doctrine.

---

## 1. Wire protocol essentials

PowDB speaks its own binary protocol over TCP (or TLS, or a Unix domain socket).
It is not Postgres- or MySQL-compatible; do not attempt to bolt a foreign wire
protocol onto it.

### Frame layout

Every message is one frame:

```
[type:u8][flags:u8][len:u32 LE][payload:len bytes]
```

- `type` is the message tag (tables below).
- `flags` is currently always `0` and reserved for future additive use: read
  it, do not assume it stays zero forever, but no flag bit is defined today.
- `len` is the payload length in bytes, little-endian. The whole frame is
  `6 + len` bytes.

A string inside a payload is length-prefixed:

```
[len:u32 LE][utf-8 bytes]
```

Frames arrive strictly FIFO: one response frame per request frame, in the order
you sent them. This is what makes request pipelining safe: you may write N
request frames back-to-back and read N response frames in the same order.

Size limits enforced on decode (a driver should enforce the same):

| Limit | Value |
|---|---|
| Max payload per frame | 64 MiB |
| Max rows per result frame | 10,000,000 |
| Max columns per result | 4,096 |
| Max bound parameters per request | 4,096 |

### Handshake: Connect / ConnectOk

Open with a `Connect` (`0x01`) frame, receive `ConnectOk` (`0x02`) or `Error`
(`0x0A`).

`Connect` payload, in order:

1. `db_name`: length-prefixed string.
2. `password`: length-prefixed string. Length `0` means "no password".
   Omitting the field entirely (payload ends after `db_name`) also means none.
3. `username`: length-prefixed string, appended after the password. Length `0`
   or an absent field means "no username".

The password and username fields are append-only extensions: a client that
sends only `db_name` + `password` is byte-identical to the pre-username frame,
and a client that sends only `db_name` is the oldest shape. Send `username`
only for named-user (multi-user) auth; omit it for shared-password or open
servers.

`ConnectOk` payload is a single length-prefixed `version` string (the server's
semver, e.g. `"0.14.0"`). There is no catalog-version field in the handshake;
see [Version compatibility](#version-compatibility) for what the reference
client actually checks.

### Request and result frames

| Tag | Name | Direction | Payload |
|---|---|---|---|
| `0x01` | Connect | → | db_name, password?, username? |
| `0x02` | ConnectOk | ← | version string |
| `0x03` | Query | → | PowQL string |
| `0x04` | QueryWithParams | → | PowQL string + params (see below) |
| `0x05` | QuerySql | → | SQL string |
| `0x07` | ResultRows | ← | string-typed rows (legacy) |
| `0x08` | ResultScalar | ← | one string value (legacy) |
| `0x09` | ResultOk | ← | `affected:u64 LE` |
| `0x0A` | Error | ← | message string |
| `0x0B` | ResultMessage | ← | status message string (DDL etc.) |
| `0x10` | Disconnect | → | (empty) |
| `0x11` / `0x12` | Ping / Pong | ↔ | (empty) |
| `0x13` | QueryNative | → | PowQL string |
| `0x14` | QueryWithParamsNative | → | PowQL string + params |
| `0x15` | QuerySqlNative | → | SQL string |
| `0x16` | ResultRowsNative | ← | typed rows (see below) |
| `0x17` | ResultScalarNative | ← | one typed value |

A request tag has a fixed response family: `0x03`/`0x04`/`0x05` answer with a
legacy result (`0x07`/`0x08`/`0x09`/`0x0A`/`0x0B`); `0x13`/`0x14`/`0x15` answer
with a native result (`0x16`/`0x17`/`0x09`/`0x0A`/`0x0B`). A mutation answers
`ResultOk`, DDL answers `ResultMessage`, an aggregate answers a scalar, a
rows-returning query answers rows, and any failure answers `Error`.

Tags in the `0x20`–`0x25` range are private, authenticated sync-protocol frames
for the embedded replica product. They are not part of the stable driver
surface; ignore them unless you are building the replica layer.

### The legacy result frames are string-typed and FROZEN

`ResultRows` (`0x07`) and `ResultScalar` (`0x08`) render every cell as a
length-prefixed string using the engine's `to_wire_string` rendering. This is
lossy by construction, and it will not change: existing ORM drivers depend on
the exact bytes, and a golden byte-identity test pins the frame. State this
plainly to your users:

- **Bytes** render as the placeholder `<N bytes>` (the raw bytes are not
  recoverable from the legacy frame).
- **An absent value (Empty), a JSON `null`, and the string `"null"` all render
  as the identical string `null`.** A legacy-frame decoder cannot tell these
  three apart. This is not a bug to work around on the legacy frame; it is the
  reason the native frames exist.
- Integers render as decimal strings, UUIDs as canonical `8-4-4-4-12` hex, JSON
  as canonical JSON text (keys sorted bytewise, no whitespace).

If your ORM needs any distinction the string rendering erases, use the native
typed frames below. Do not try to retrofit types onto the legacy frame.

### The native typed frames are the sanctioned lossless surface

`ResultRowsNative` (`0x16`) and `ResultScalarNative` (`0x17`) preserve each
cell's storage type. This is the surface a correct driver should build on.

**`ResultRowsNative` payload:**

```
[col_count:u16 LE]
[col_name:string] × col_count      -- strict UTF-8
[row_count:u32 LE]
[cell] × (row_count × col_count)   -- row-major
```

**`ResultScalarNative` payload:** a single `[cell]`.

**Cell format:**

```
[type_id:u8][body_len:u32 LE][body:body_len bytes]
```

The nine `type_id` values and their exact body encodings:

| type_id | Type | Body |
|---|---|---|
| `0` | Empty | length `0`, no bytes: an absent value |
| `1` | Int | 8 bytes, LE `i64` |
| `2` | Float | 8 bytes, LE `f64` (IEEE 754) |
| `3` | Bool | 1 byte, `0` or `1` (any other byte is a decode error) |
| `4` | Str | `body_len` bytes of UTF-8 (strict; invalid UTF-8 is a decode error) |
| `5` | DateTime | 8 bytes, LE `i64`, microseconds since the Unix epoch |
| `6` | Uuid | exactly 16 bytes |
| `7` | Bytes | `body_len` raw bytes, verbatim |
| `8` | Json | `body_len` bytes of canonical PJ1 (binary JSON), validated on decode |

`Empty` (`type_id 0`) is the null/absent cell and is distinct from a `Json`
cell whose value is JSON `null`. Preserve that distinction: it is the whole
point of the native frame (see [Null semantics](#3-null-semantics)).

A `Json` cell body is canonical PJ1, not JSON text. A driver may decode it to a
native value, but should also expose the raw PJ1 bytes: they are the exact
on-wire binary form, useful for byte-identical storage, hashing, or
re-encoding, and they carry JSON-internal integers beyond a host language's
safe-integer range that a decode to floats would lose. The reference client
exposes both (`value` plus `pj1`) on its `WireValue` JSON variant.

Decoders reject malformed cells: a wrong `body_len` for a fixed-width type,
invalid UTF-8 in a string, an out-of-range boolean, invalid PJ1, an unknown
`type_id`, or trailing bytes after the declared payload all fail rather than
silently truncating.

### Parameterized request frames

`QueryWithParams` (`0x04`) and `QueryWithParamsNative` (`0x14`) carry positional
`$N` parameters. Payload:

```
[query:string]
[param_count:u16 LE]
[param] × param_count
```

Each param is a 1-byte tag plus a body:

| tag | Type | Body |
|---|---|---|
| `0` | Null | (none): binds PowQL `null` |
| `1` | Int | 8 bytes, LE `i64` |
| `2` | Float | 8 bytes, LE `f64` |
| `3` | Bool | 1 byte |
| `4` | Str | length-prefixed UTF-8 |

**Parameters can carry only these five types: null, int, float, bool, string.**
There is no parameter tag for bytes, UUID, datetime, or a JSON document. Bind a
datetime as its integer microseconds (tag `1`); bind a JSON document as its text
string (tag `4`) and let the column validate and canonicalize it. The server
binds each `$N` at the **token level**: the value is substituted as a literal
token before parsing: so an injection-shaped string is inert data and can never
change the query's shape. A `$N` with no matching argument, or a `$0`, is a
clean parse error. `$` placeholders are 1-based; `?` is not a placeholder and
`??` is the coalesce operator.

### Error frames

`Error` (`0x0A`) carries a single length-prefixed human-readable message string.
It is the response to any failed request on any dialect. See
[Error taxonomy](#6-error-taxonomy) for the message families you may match on.

---

## 2. Mapping an ORM query builder to PowQL

PowQL is a left-to-right pipeline: `Table [distinct] [filter …] [group … [having
…]] [order …] [limit …] [offset …] { projection }`. Map your builder's clauses
onto that shape. Everything below is verified against the language reference;
see [POWQL.md](../POWQL.md) for the full grammar.

### Fields, filters, ordering, grouping

- **Field reference** is dot-prefixed: `.name`, `.age`. Over a join, qualify
  with the alias: `u.name`, `o.total`.
- **filter** takes a boolean expression: `Table filter .age > 25 and .status =
  "active"`. Comparison operators are `= != < > <= >=`; logical `and or not`;
  plus `in (...)`, `not in (...)`, `between … and …`, `like`, `is null`, `is
  not null`, and `??` (coalesce).
- **order** takes one or more keys with optional `asc`/`desc`: `order .age desc,
  .name asc`.
- **group / having** aggregate per key: `group .status having count(.name) > 5 {
  .status, n: count(.name) }`.
- **projection** is `{ ... }` with optional aliases and computed expressions:
  `{ full_name: .name, double_age: .age * 2 }`.

String literals use double quotes. Assignment (insert/update/upsert) uses `:=`,
not `=`. A keyword used as an identifier must be backtick-quoted (`` `order`
``); a dotted field reference like `.order` bypasses keyword lookup.

#### Equality is type-strict; range comparison coerces numerically

The comparison operators split into two coercion regimes, and a driver's
query generator must bind literals with this in mind:

- **`=` and `!=` compare values type-strictly.** `Int(7)`, `Float(7.0)`, and
  `Str("7")` are three different values; comparing across types is simply
  `false` (never an error). One softening applies to stored columns only: an
  int literal against a `float` column is widened to float before comparing
  (`.price = 7` matches `7.0`), because the widening is lossless. The reverse
  does not hold (`.qty = 7.0` against an `int` column matches nothing), and
  strings never compare equal to numbers anywhere.
- **`< > <= >=` and `between` coerce numerically.** Int and float compare by
  promoting the int to a float (monotonic even above 2^53, though promotion
  can lose precision there), against both stored columns and JSON leaves.

JSON path leaves get no schema softening: the extracted scalar keeps the exact
type in the document (a JSON `7` is int, a JSON `7.0` is float; PJ1 preserves
the distinction), and `=` compares it strictly. `.data->g = 7` does NOT match
a document holding `{"g": 7.0}`; `.data->g = 7.0` does. This is the case most
likely to bite an ORM whose language conflates int and float (JavaScript): a
driver binding a JS number against a JSON leaf must decide intentionally
whether to emit `7` or `7.0`, ideally by round-tripping the type it wrote.

The practical rule: bind every literal in the type actually stored. For
stored columns the declared type is authoritative; for JSON leaves the
document's own scalar type is. Convert stringly-typed user input before
binding rather than relying on the engine to coerce, because under `=` it
will not.

### JSON path operator `->`

For a `json` column, `->` walks into the document by object key or array index
and binds tighter than every other operator:

```
Post { author: .data->author->name }     -- object key
Post { first: .data->tags->0 }            -- array index (0-based)
Post filter .data->views > 10             -- extract, then compare
Post order .data->score desc
Post group .data->author->name { author: .data->author->name, views: sum(.data->views) }
```

`->` works in filters, order keys, group keys, projections, and aggregate
arguments: everywhere an expression is allowed. A key that is not a bare
identifier is written as a double-quoted string: `.data->"has spaces!"`.

### Index DDL, including expression indexes

Map your schema/migration layer to these:

```
alter User add index .email                     -- column index
alter User add unique .email                    -- unique column index
alter Post add index (.data->author->name)      -- expression (JSON path) index
alter Post add unique (.data->external_id)       -- unique expression index
alter Post drop index (.data->author->name)
```

Parentheses are required for a path index and are reserved for that syntax.
`add index`/`add unique` accept `if not exists`; `drop index` accepts `if
exists`. `drop index` only supports expression (JSON path) indexes: dropping a
stored-column index (`alter User drop index .email`) returns an error, so a
migration layer must model column indexes as create-only. Point lookups and range filters use an index automatically: there is
no hint syntax. Expression indexes cover equality, range, and ordered
`order path limit K` scans.

### Upsert

```
upsert User on .email { name := "Alice", email := "alice@example.com", age := 30 }
  on conflict { age := 30 }
```

The `on` column must be `unique` (declared `unique` or given a unique index), or
the upsert is rejected. Without `on conflict`, a conflict replaces the row with
the given assignments; with it, only the conflict assignments are applied.

### Transactions

Transaction control is ordinary statements, not a protocol extension:

```
begin
insert Order { user_id := 1, total := 99.95 }
User filter .id = 1 update { order_count := .order_count + 1 }
commit          -- or: rollback
```

Transactions are per-connection; other connections never see uncommitted rows.
Nesting is an error (`begin` inside an open transaction). A connection that
closes with an open transaction is rolled back implicitly. Inside a transaction
the WAL fsync is deferred to `commit`, so wrapping a bulk load in one
transaction is dramatically faster while staying fully durable: surface this to
your users as the batching primitive.

If your driver pipelines statements, do **not** embed your own `begin`/`commit`
in a pipelined script: a trailing `commit` is already on the wire when an
earlier statement's error reply arrives, so it would commit partial work. Drive
transaction control by waiting for each reply, or model it the way the reference
client's `execScript({ transactional: true })` does.

---

## 3. Null semantics

There are three distinct "nothing" values a driver must handle, and they are
only fully distinguishable on the native surface.

1. **Empty**: an absent value / SQL-style null. Native `type_id 0`.
2. **JSON `null`**: a `json` cell whose document is the literal `null`. Native
   `type_id 8` (Json) with a PJ1 `null` body.
3. **The string `"null"`**: an ordinary `str` cell. Native `type_id 4`.

On the legacy string frame all three render as the identical string `null`
(see above). On the native frame they are three different cells. If your driver
needs to tell them apart, read via the native frames and expose the raw cell
tags: mirror the reference client's `queryNativeRaw`, which returns the raw
tagged union without collapsing Empty / JSON-null / `"null"` to one value.

### The scalarized path-leaf collapse affects every API

One collapse happens **inside the engine, before any wire format**, native
included: extracting a scalar with `->` maps both a *missing* path and an
*explicit JSON `null`* at the leaf to Empty.

```
Post { kind: json_type(.data->author) }
Post filter .data->views > 10
```

So `.data->maybe->leaf` yields the same Empty whether the key was absent or
present-but-null. Two escape hatches recover the distinction, and a driver
should document both:

- **`json_type(.data->maybe->leaf)`** inspects the raw JSON node: it returns the
  string `"null"` for an explicit JSON null and the empty set for a missing
  path. It also distinguishes `"object"`, `"array"`, `"string"`, `"number"`,
  and `"bool"`.
- **Project the enclosing `json` value** instead of scalarizing past it: a whole
  `json` cell preserves `null` exactly (it comes back as a Json cell, not
  Empty).

This mirrors the "Null vs missing" section of the
[client README](../../clients/ts/README.md): do not contradict it. The engine
deliberately does not add a distinct JSON-null scalar value; `json_type()` and
whole-value projection are the supported way to recover the distinction.

---

## 4. The `explain` contract

Prefix any query with `explain` to get its plan instead of executing it:

```
explain Post filter .model_id = $1 and .is_published = true and .data->ns->value = $2 { .id }
```

`explain` returns a normal rows result with a **single column named `plan`**.
Each row is one line of the indented plan tree. Consume it like any other query
result.

**As of v0.14, `explain` reflects the lowered, executed plan**, not the raw
planner output. The engine runs the same runtime lowering it uses for execution
before formatting, so what you see is what runs. In particular, a conjunction
filter whose predicate has an indexed conjunct now shows an index scan with a
residual `Filter` on top, instead of a bare `SeqScan`:

```
Filter predicate=…
  ExprIndexScan table=Post path=.data->ns->value index_id=7 key=…
```

The residual `Filter` re-checks the remaining conjuncts on each fetched row, so
selecting an index is a pure performance decision: results are identical with
or without the index present.

**Stable** across minor versions: the plan **node names** (`SeqScan`,
`IndexScan`, `RangeScan`, `ExprIndexScan`, `ExprRangeScan`,
`OrderedExprIndexScan`, `Filter`, `Project`, `Sort`, `Limit`, `Offset`,
`Aggregate`, `GroupBy`, `Distinct`, `NestedLoopJoin`, `AliasScan`) and the
**tree-shape semantics** (which node sits above which). A join line also reports
its `strategy` as `hash`, `hash+residual`, or `nested-loop-bounded`.

**Not a stable API**: the exact spacing, indentation, and the order or spelling
of a node's key=value fields may change between minor versions. Parse `explain`
defensively: match on node names and structure, never on exact byte layout.
`explain` is a diagnostic surface, not a machine contract; treat it as
human-readable.

---

## 5. Admission and execution semantics to surface

A driver should surface these behaviors to its users rather than hide them,
because they change how an application should retry and pool connections.

- **Single-writer admission.** Writes on one server process funnel through an
  admission gate: a write acquires exclusive admission, readers do not.
  Contended writes **queue** behind the in-flight writer rather than failing;
  a contended write is a wait, not an error. In the current implementation the
  gate admits in FIFO order, so a queued writer is not starved by later
  readers, but that ordering is **observed behavior, not a documented
  guarantee**: the contract is only that a wait is bounded by the configured
  timeout (below). Do not build a driver feature that depends on admission
  order. (The gate exists only in the server; it disappears for the in-process
  embedded addon, where your single handle owns the engine.)
- **Concurrent reads.** Read-classified statements admit concurrently and scan
  in parallel; they never take the writer's exclusive admission.
- **Bounded autocommit wait.** A bare (non-transaction) write that waits too
  long for the gate returns a bounded error rather than blocking forever:
  `transaction gate timeout after {ms}ms waiting for concurrent transaction to
  complete`. An explicit `begin` that waits too long for the transaction gate
  fails the same way. Treat these as transient/retryable.
- **Bounded join rejection.** A pure nested-loop join whose candidate-pair count
  exceeds the safety bound is rejected **before execution** with a message
  naming the fix: `nested-loop join would evaluate N candidate pairs, above the
  M pair limit; add an equi-key to ON, index/filter an input, reduce the joined
  row counts, or raise the cap via POWDB_MAX_NESTED_LOOP_PAIRS`. A join result
  that grows past the row limit is rejected as `join result exceeds row limit`.
- **Timeout and cancellation.** A query past its per-query deadline returns
  `query timeout after {ms}ms`. A query cancelled because the issuing client
  disconnected returns `query cancelled by client disconnect`. Both are clean
  early returns that release locks; a driver should treat a client-initiated
  cancel as final and never auto-retry it.
- **Read-only roles.** "Read-only" today is per-connection RBAC, enforced
  server-side: a write or DDL statement from a read-only role is rejected with
  `permission denied: role '<role>' cannot execute write statements` (or
  `schema-definition statements`), and the connection stays usable for reads.
  `ReadonlyNeedsWrite` is an internal execution sentinel a driver never sees on
  the wire: the server transparently escalates a read-classified statement that
  turns out to need a write when the connection's role allows it, so you do not
  handle it.
- **Read-only snapshot mode.** Distinct from RBAC: a database opened read-only
  for snapshot serving (`powdb-server --readonly`, embedded `openReadOnly`)
  rejects every mutation and DDL statement with
  `readonly mode: statement requires a writer` regardless of the connection's
  role. This refusal is terminal for the statement but the connection stays
  usable for reads. A driver should classify it separately from the RBAC
  `permission denied: role` family: RBAC means "this user may not write here",
  snapshot mode means "nothing can write here; route writes to the primary".

### Version compatibility

The `ConnectOk` frame carries the server's semver string. The reference client
warns once per host when the server's **major** version differs from the
client's, but does not refuse to connect: a driver should follow the same
best-effort posture rather than hard-gating on the version string.

A separate ceiling governs on-disk/sync format compatibility:
`SUPPORTED_CATALOG_VERSION` in the reference client (currently `6`) is the
highest catalog format the client can read. The reference behavior a driver
should copy: accept a reported catalog version at or below your maximum, and
refuse a newer one (you cannot read a format from the future). This ceiling is
carried in the sync-bootstrap metadata, not in the ordinary query handshake, so
most drivers only need the advisory semver check above.

---

## 6. Error taxonomy

Every failure arrives as an `Error` (`0x0A`) frame carrying one human-readable
message string. **The messages are prose, not error codes.** Only the families
below are documented and quasi-stable enough to match on; treat every other
message as opaque and surface it to the caller unparsed.

| Family | Match on | Nature |
|---|---|---|
| Query timeout | `query timeout after` | transient |
| Client cancellation | `query cancelled by client disconnect` | final (do not retry) |
| Admission-gate timeout | `transaction gate timeout after` | transient |
| Bounded join | `nested-loop join would evaluate` / `join result exceeds row limit` | not transient: fix the query |
| Unique constraint | `unique constraint violation on` | not transient |
| Permission (RBAC) | `permission denied: role` | not transient |
| Read-only snapshot mode | `readonly mode: statement requires a writer` | not transient: route writes to the primary |

Match by a stable **substring**, not the whole message: the variable parts
(millisecond counts, pair counts, table/column/role names) are interpolated and
will differ per occurrence. If a message does not match one of these families,
do not attempt to classify it; pass it through. The reference client maps these
onto a small stable `.code` set (`timeout`, `aborted`, `query_failed`, …); a
driver in another language should expose an equivalent code set backed by these
substrings.

---

## Versioning

This document is versioned with the PowDB workspace, and behavior added in a
given release is called out inline (for example, `explain` reflecting the
lowered plan "as of v0.14"). When a release changes anything on this surface,
the change is noted here per release.

The wire protocol evolves **additively only**. New capability is added as a new
message tag or as fields appended to the end of an existing frame's payload
(the way `username` was appended to `Connect`, and the way the native `0x13`–
`0x17` frames were added beside the frozen legacy ones). An existing frame's
shape is never mutated: a tag's meaning, field order, and encoding are fixed
once shipped. A decoder that does not recognize a new tag rejects it with a
clean "unknown message type" error rather than misreading it. Build on this: a
driver written against a given version keeps decoding every frame that version
understood, indefinitely.
