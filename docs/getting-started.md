# Getting Started with PowDB

PowDB is a high-performance database engine with its own query language called PowQL. This tutorial walks you through installing PowDB, creating a table, inserting data, querying it, and running in server mode. It takes about 10 minutes.

---

## 1. Install

The easiest way to install PowDB is from crates.io:

```bash
cargo install powdb-cli
cargo install powdb-server
```

Or build from source (requires Rust 1.93+; install from [rustup.rs](https://rustup.rs/) if needed):

```bash
git clone https://github.com/zvndev/powdb.git
cd powdb
cargo build --release
```

This builds the CLI, server, query engine, and storage engine.

Check that installation worked:

```bash
powdb-cli --version
```

---

## 2. Start the REPL

Launch the interactive PowQL shell:

```bash
powdb-cli
# or from source:
cargo run --release -p powdb-cli
```

You should see:

```
PowDB v0.8.0 — embedded mode
Data directory: ./powdb_data
Type PowQL queries. Use Ctrl-D to exit. Type .help for commands.

powql>
```

Data is stored in `./powdb_data/` by default. You can change it with `--data-dir`:

```bash
cargo run --release -p powdb-cli -- --data-dir ./my_project_data
```

---

## 3. Create a Table

PowDB uses the `type` keyword to define tables. Fields prefixed with `required` cannot be null.

```
powql> type User { required name: str, required email: str, age: int }
```

Output:

```
type User created
```

That's it. No `CREATE TABLE`, no column types in parentheses. Fields have a name and a type, separated by a colon.

Supported types: `str`, `int`, `float`, `bool`, `datetime`, `uuid`, `bytes`.

---

## 4. Insert Data

Insert rows with the `insert` keyword. Fields are assigned with `:=`.

```
powql> insert User { name := "Alice", email := "alice@example.com", age := 30 }
```

Output:

```
1 row affected
```

Let's add a few more people:

```
powql> insert User { name := "Bob", email := "bob@example.com", age := 25 }
1 row affected

powql> insert User { name := "Charlie", email := "charlie@example.com", age := 35 }
1 row affected

powql> insert User { name := "Diana", email := "diana@example.com", age := 28 }
1 row affected

powql> insert User { name := "Eve", email := "eve@example.com", age := 22 }
1 row affected

powql> insert User { name := "Frank", email := "frank@example.com", age := 40 }
1 row affected
```

Fields without `required` can be omitted -- they default to null:

```
powql> insert User { name := "Grace", email := "grace@example.com" }
1 row affected
```

> **Multi-row insert:** you can also insert many rows in one statement by separating row blocks with commas -- `insert User { ... }, { ... }, { ... }`. One statement means one WAL fsync and one network round trip, and validation is all-or-nothing. See [INSERT in the PowQL reference](POWQL.md#insert).

> **Note:** Each autocommit `insert` fsyncs to the write-ahead log for durability, which caps single-row inserts at roughly a few hundred per second on real disks. For bulk loads, wrap many inserts in a `begin` / `commit` transaction -- they share a single fsync at commit and run dozens of times faster, still fully durable. See [Transactions](POWQL.md#transactions).

---

## 5. Query Basics

### Select all rows

Just type the table name:

```
powql> User
```

Output:

```
 name    | email                | age
---------+----------------------+----
 Alice   | alice@example.com    | 30
 Bob     | bob@example.com      | 25
 Charlie | charlie@example.com  | 35
 Diana   | diana@example.com    | 28
 Eve     | eve@example.com      | 22
 Frank   | frank@example.com    | 40
 Grace   | grace@example.com    | NULL
(7 rows)
```

The `NULL` for Grace's age means null (she was inserted without an age).

### Filter rows

Use `filter` with a condition. Fields are referenced with a dot prefix:

```
powql> User filter .age > 25
```

Output:

```
 name    | email                | age
---------+----------------------+----
 Alice   | alice@example.com    | 30
 Charlie | charlie@example.com  | 35
 Diana   | diana@example.com    | 28
 Frank   | frank@example.com    | 40
(4 rows)
```

### Project specific fields

Use `{ }` braces to select which fields to return:

```
powql> User { .name, .age }
```

Output:

```
 name    | age
---------+----
 Alice   | 30
 Bob     | 25
 Charlie | 35
 Diana   | 28
 Eve     | 22
 Frank   | 40
 Grace   | NULL
(7 rows)
```

You can combine filter and projection:

```
powql> User filter .age > 25 { .name, .age }
```

Output:

```
 name    | age
---------+----
 Alice   | 30
 Charlie | 35
 Diana   | 28
 Frank   | 40
(4 rows)
```

---

## 6. Sorting and Limiting

PowQL operations chain left to right in a pipeline. Add `order` and `limit` to sort and cap the results:

```
powql> User order .age desc limit 3 { .name, .age }
```

Output:

```
 name    | age
---------+----
 Frank   | 40
 Charlie | 35
 Alice   | 30
(3 rows)
```

You can sort ascending (the default) or descending:

```
powql> User order .age asc limit 3 { .name, .age }
```

Output:

```
 name  | age
-------+----
 Eve   | 22
 Bob   | 25
 Diana | 28
(3 rows)
```

---

## 7. Aggregations

PowQL wraps aggregate functions around the query pipeline.

### Count

```
powql> count(User)
```

Output:

```
7
```

Count with a filter:

```
powql> count(User filter .age > 25)
```

Output:

```
4
```

### Average

```
powql> avg(User { .age })
```

Output:

```
30
```

### Sum

```
powql> sum(User filter .age > 25 { .age })
```

Output:

```
133
```

Other aggregate functions: `min()`, `max()`.

---

## 8. Updates

Use `update` after an optional `filter` to modify rows. Assignments use `:=`:

```
powql> User filter .name = "Alice" update { age := 31 }
```

Output:

```
1 row affected
```

Verify it worked:

```
powql> User filter .name = "Alice" { .name, .age }
```

Output:

```
 name  | age
-------+----
 Alice | 31
(1 row)
```

You can also use expressions that reference the current row value:

```
powql> User filter .name = "Bob" update { age := .age + 1 }
```

Output:

```
1 row affected
```

---

## 9. Create an Index

Indexes speed up lookups on frequently queried columns. Use `alter ... add index` to build a B+tree index:

```
powql> alter User add index .email
```

Output:

```
index on 'User.email' created
```

Indexed columns are used automatically for point lookups and range scans -- no query hints needed.

JSON paths can be indexed too. Wrap the full path in parentheses so it is
unambiguous from a stored-column index:

```powql
type Post { required id: int, data: json }
alter Post add index (.data->author->name)
alter Post add unique (.data->external_id)
```

A path index accelerates equality and range filters and can satisfy a bounded
sort such as `Post order .data->score desc limit 10`. Indexed path values must
be scalars; objects and arrays are rejected. Missing paths and JSON null are
allowed and sort last.

---

## 10. Delete

Use `delete` after a `filter` to remove matching rows:

```
powql> User filter .age < 25 delete
```

Output:

```
1 row affected
```

Verify Eve was removed:

```
powql> User { .name, .age }
```

Output:

```
 name    | age
---------+----
 Alice   | 31
 Bob     | 26
 Charlie | 35
 Diana   | 28
 Frank   | 40
 Grace   | NULL
(6 rows)
```

To delete all rows in a table (use with care):

```
User delete
```

---

## 11. Server Mode

So far we've been running PowDB in embedded mode -- the CLI talks directly to the storage engine. Run PowDB as a server when a few processes need to share one single-writer database over the wire, or to serve a read-only snapshot to many readers. (For many concurrent clients on one shared read-write database, use Postgres; see "What PowDB is for" in the README.)

### Start the server

In one terminal:

```bash
powdb-server --port 5433 --data-dir ./powdb_data
# or from source:
cargo run --release -p powdb-server -- --port 5433 --data-dir ./powdb_data
```

Output:

```
powdb server listening addr=127.0.0.1:5433 data_dir=./powdb_data auth=false ...
```

### Connect from a client

In another terminal, connect with the CLI in remote mode:

```bash
powdb-cli --remote localhost:5433
# or from source:
cargo run --release -p powdb-cli -- --remote localhost:5433
```

Output:

```
PowDB v0.8.0 — remote mode
Connecting to localhost:5433 ...
Connected to db `default` (server v0.8.0)
Type PowQL queries. Use Ctrl-D to exit.

powql>
```

From here, the same PowQL statements work as embedded mode -- DDL statements return the same friendly status messages too (e.g. `type User created`). The server handles concurrent readers and uses a write-ahead log for durability.

The TypeScript client also offers `queryNative()` and `querySqlNative()` for
lossless typed results. Use them when JSON, exact bytes, large integers, or
Empty-versus-string distinctions should not pass through the legacy string
result format:

```typescript
const result = await client.queryNative(
  "Post filter .id = $1 { .id, .data }",
  [42],
);
```

### Password authentication

To require a password, set the `POWDB_PASSWORD` environment variable:

```bash
# Start the server with a password
POWDB_PASSWORD=mysecret powdb-server

# Connect with the password
powdb-cli --remote localhost:5433 --password mysecret
```

### Multi-user authentication

PowDB also supports named users with roles, layered on top of the
single-shared-password model. The auth model is **backward compatible**:

- **No users defined** → the legacy single shared-password model applies
  (set `POWDB_PASSWORD`, connect with `--password`). If neither a password nor
  any users are configured, all connections are accepted (development default).
- **One or more users defined** → the server authenticates each connection's
  `(username, password)` against the user store (`auth.json` in the data dir),
  and the shared password is no longer used.

Users live in the data directory's `auth.json` (argon2id hashes only — never
plaintext, `0600` on Unix). Manage them offline with the CLI, pointing at the
**same `--data-dir` the server uses**:

```bash
# Create users (role defaults to readwrite if --role is omitted).
# Built-in roles: admin, readwrite, readonly.
powdb-cli --data-dir ./powdb_data useradd alice --role admin --password s3cret
powdb-cli --data-dir ./powdb_data useradd bob   --role readonly --password hunter2

# The password may also come from the environment (handy in scripts/CI):
POWDB_NEW_PASSWORD=s3cret powdb-cli --data-dir ./powdb_data useradd carol --role readwrite

# List users (shows name + role only — never password hashes).
powdb-cli --data-dir ./powdb_data users

# Change a password.
powdb-cli --data-dir ./powdb_data passwd bob --password newpw

# Remove a user.
powdb-cli --data-dir ./powdb_data userdel bob
```

Connect as a named user with `--user`:

```bash
powdb-cli --remote localhost:5433 --user alice --password s3cret
```

> The user-admin subcommands edit the data dir directly and require no running
> server. Edit them while the server is stopped (or before first start); the
> server loads `auth.json` at startup.

#### Zero-CLI admin bootstrap

For containerized / first-boot deployments you can bootstrap an initial admin
purely from the environment. When `POWDB_ADMIN_USER` **and**
`POWDB_ADMIN_PASSWORD` are both set and that user does not already exist, the
server creates it with role `admin` and persists it on startup (the password is
never logged):

```bash
POWDB_ADMIN_USER=root POWDB_ADMIN_PASSWORD=changeme powdb-server --data-dir ./powdb_data
```

After the admin exists, use `passwd` / `useradd` to manage the rest, and stop
relying on the bootstrap env vars.

### Concurrent transactions

Autocommit read-only queries share server admission and can run concurrently.
Writers and explicit transactions take exclusive admission, so a read observes
either the complete state before a write or the complete state after it, never
a partial mutation.

Explicit transactions (`begin` ... `commit`) are **serialized across
connections**: one process runs one explicit transaction at a time. When a
connection issues `begin` while another connection's transaction is still open,
it **queues** rather than failing — so a burst of concurrent writers all commit
instead of erroring out.

Bare **autocommit** writes (an `insert`/`update`/`delete` with no surrounding
`begin`) share the same gate: while one connection holds an explicit
transaction open, another connection's autocommit write waits for it too. This
keeps every writer serialized and durable.

The wait is bounded for **both** paths. If a `begin` **or** a bare autocommit
write waits longer than the configured window for the other transaction to
finish, it fails with a clear error (`transaction gate timeout after 5000ms
waiting for concurrent transaction to complete`) instead of hanging
indefinitely, so a stalled or held-open transaction can never wedge other
writers forever. Tune the window with `--tx-wait-timeout-ms` (or
`POWDB_TX_WAIT_TIMEOUT_MS`; default `5000`):

```bash
powdb-server --tx-wait-timeout-ms 2000
```

The `powdb_tx_gate_timeouts_total` metric counts these timeouts from both the
explicit-`begin` and autocommit paths (see `--metrics-addr`).

> **Note on admission order:** in the current implementation the gate admits
> waiters in FIFO order, so a queued writer is not starved by later readers.
> That ordering is observed behavior, not a documented guarantee; the
> contract is only that any wait is bounded by the timeout above.

### Serving a named database

One server process serves a single database. By default it accepts any
`db_name` a client sends (the name is informational). To pin the process to a
name and **reject** a client that explicitly asks for a different database, set
`--db-name` (or `POWDB_DB_NAME`):

```bash
powdb-server --db-name prod
```

A `CONNECT` for `prod` (or the client default `default`, or an empty name) is
accepted; a `CONNECT` explicitly naming another database is rejected with
`unknown database '<name>'; this server serves 'prod'`. Leaving `--db-name`
unset preserves the existing accept-anything behavior.

---

## What's Next

This tutorial covered the basics: tables, inserts, queries, aggregates, updates, indexes, and deletes. PowDB supports much more, including joins, group by, subqueries, materialized views, and set operations.

See the full language reference: [PowQL Reference](POWQL.md)

Running in production? Set up backups: [Backup & restore](backup-and-restore.md) covers full and incremental backups plus coarse point-in-time recovery (offline -- stop the server first).
