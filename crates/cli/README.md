# powdb-cli reference

`powdb-cli` is PowDB's shell: an interactive REPL plus a one-shot/scripting
mode. It talks to either an embedded data directory (no server) or a remote
`powdb-server` over TCP/TLS.

```bash
cargo install powdb-cli      # or use the release binary / Docker image
powdb-cli --help
```

## Modes

| Command | What it does |
| --- | --- |
| `powdb-cli ./mydata` | Embedded REPL against the data dir `./mydata` |
| `powdb-cli --remote 127.0.0.1:5433 --password secret` | Remote REPL |
| `powdb-cli -c 'count(User)'` | One-shot: run statements and exit |
| `powdb-cli --exec-file dump.powql` | One-shot from a file (`-` for stdin) |

Embedded mode takes an exclusive lock on the data directory, so it refuses to
open a directory a live `powdb-server` owns.

A bare argument is the data directory, but only when it is unambiguous: an
explicit `-d`/`--data-dir` always wins, and a word close to a real subcommand
is treated as a typo rather than silently becoming a new empty database.

```
$ powdb-cli -d /var/lib/powdb usrs
Error: unexpected argument: usrs
note: did you mean the `users` subcommand?
try --help
```

## One-shot execution: `--exec` and `--exec-file`

`--exec <QUERY>` (short `-c`) runs one or more statements and exits.
`--exec-file <PATH>` reads the same thing from a file, or from stdin when the
path is `-`, which sidesteps the `ARG_MAX` ceiling on large loads. The two are
mutually exclusive.

**Statements are separated by `;`, never by newlines.**

```bash
powdb-cli -d ./mydata -c 'type User { required name: str }; insert User { name := "ada" }'
powdb-cli -d ./mydata --exec-file dump.powql
cat dump.powql | powdb-cli -d ./mydata --exec-file -
```

A newline continues the current statement, which is what lets a PowQL pipeline
span several lines:

```powql
User
  filter .age > 25
  order .age
  limit 5;
```

Because of that, a file of newline-separated statements with no `;` parses as
one statement and fails. The CLI detects that exact shape and says so:

```
Error: unexpected trailing token near token 11: 'insert'
note: this input has 2 non-empty lines and no `;`, so it was parsed as ONE statement.
note: statements are separated by `;`, not by newlines ...
```

Splitting is statement-aware: a `;` inside a string literal or a `#` comment is
not a separator. Segments that are only comments and whitespace are skipped, so
a dump that opens or ends with a comment line still exits 0. Execution stops at
the first failing statement and exits 1. Meta-commands (`.tables` and friends)
are REPL-only and are rejected here.

## SQL

PowQL is the native language; SQL is a frontend that lowers a supported subset
onto the same planner (see `docs/SQL.md`). Both are reachable from the CLI:

```bash
# one-shot: treat --exec / --exec-file input as SQL
powdb-cli -d ./mydata --sql -c 'SELECT name, age FROM User WHERE age > 25'

# remote works too, over the QuerySql wire frame
powdb-cli -r 127.0.0.1:5433 --password secret --sql -c 'SELECT count(*) FROM User'
```

In the REPL:

```
powql> .sql SELECT name FROM User     # run one statement as SQL
powql> .sql                           # switch the session to SQL; prompt becomes `sql> `
sql> SELECT name FROM User WHERE age > 25
sql> .powql                           # switch back to PowQL
```

`--sql` also starts the REPL in SQL mode.

## Output formats

`--format <table|json|csv>` sets one-shot rendering; `.mode <table|json|csv>`
does the same inside the REPL. `table` is the default human format.

```bash
$ powdb-cli -d ./mydata --format json -c 'User { .name, .age }'
{"columns":["name","age"],"rows":[["ada",36],["bob",24]]}

$ powdb-cli -d ./mydata --format json -c 'count(User)'
{"value":2}

$ powdb-cli -d ./mydata --format csv -c 'User { .name, .age }'
name,age
ada,36
bob,24
```

JSON output is one document per statement on a single line, so multi-statement
runs stream as JSON Lines and pipe straight into `jq`. Cells keep their JSON
type (int, float, bool, null, and json documents inline); uuid, datetime, and
bytes render as strings exactly as the table view shows them.

`--format json` is transport-independent: the same query over the same data
produces byte-identical JSON embedded and remote, so a script written against
an embedded database keeps working when you point it at a server. Remote mode
gets this by asking the server for typed results, which every server from
v0.22.0 on advertises during the handshake. Against an older one the CLI notes
on stderr that it is falling back; every remote cell is then a JSON string
except the NULL sentinel, which stays JSON `null`.

Integers are written as JSON numbers with every digit preserved, including
values beyond 2^53. Consumers that parse JSON numbers as IEEE doubles (`jq`
without `--sort-keys`-style big-number handling, `JSON.parse`) lose precision
on those; `--format csv` sidesteps it.

CSV follows RFC 4180: a field containing a comma, quote, CR, or LF is quoted
and embedded quotes are doubled.

Other result shapes in JSON: `{"affected":N}` for writes, `{"created":"Name"}`
for a type declaration, `{"message":"..."}` for everything else. Errors stay on
stderr and set exit code 1.

## REPL

Multi-line input: the REPL keeps reading while `(`, `{`, or a string literal is
unbalanced, and shows a `  ...> ` continuation prompt. Two escape hatches:

- `.cancel` (or `\c`) discards the partial statement. It is the one command
  recognized inside a continuation, so it always works.
- Ctrl-C also clears the buffer on a terminal.

If input ends (Ctrl-D or a closed pipe) while a statement is still unterminated,
the CLI warns that the buffered lines were discarded rather than exiting
silently. Piped sessions additionally get a note when they first enter a
continuation, since they never see the prompt.

### Meta-commands

| Command | Embedded | Remote | Description |
| --- | --- | --- | --- |
| `.help` | yes | yes | List meta-commands |
| `.tables` | yes | no | List tables |
| `.schema <TABLE>` | yes | no | Columns, types, required flags |
| `.sql [STMT]` | yes | yes | Run STMT as SQL, or switch the session to SQL |
| `.powql` | yes | yes | Switch the session back to PowQL |
| `.mode <FMT>` | yes | yes | `table`, `json`, or `csv` |
| `.cancel` / `\c` | yes | yes | Discard an unterminated statement |
| `.timing` | yes | yes | Toggle per-query timing |
| `.quit` / `.exit` | yes | yes | Leave the REPL |

History is kept in `~/.powdb_history`. Tab completion covers PowQL keywords and
meta-commands.

## Connection options

| Flag | Env fallback | Meaning |
| --- | --- | --- |
| `-d, --data-dir <PATH>` | | Embedded data dir (default `./powdb_data`) |
| `-r, --remote <HOST:PORT>` | | Connect to a server instead |
| `--db <NAME>` | | Database name (default `default`) |
| `--password <PW>` | `POWDB_PASSWORD` | Password for remote auth |
| `-u, --user <NAME>` | | Username for multi-user remote auth |
| `--tls` | `POWDB_TLS=1` | Encrypt the remote connection |
| `--tls-ca <PATH>` | `POWDB_TLS_CA` | Trust this root CA instead of the web roots (implies `--tls`, flag or env) |
| `--tls-server-name <N>` | `POWDB_TLS_SERVER_NAME` | Certificate name to verify against (implies `--tls`, flag or env) |

Prefer the env fallbacks for secrets: a `--password` on the command line is
visible in the process list.

## Offline subcommands

These operate directly on `--data-dir` with no server running. See
`powdb-cli --help` for the full argument lists.

| Subcommand | Purpose |
| --- | --- |
| `backup <DEST> [--base <FULL>]` | Full or incremental (differential) snapshot |
| `restore <BKP> <DEST> [--apply <INC>]... [--sync-strip\|--sync-preserve\|--sync-fork]` | Rebuild a data dir, optionally chaining increments |
| `sync-enable` | Create the sync identity so backups can bootstrap replicas |
| `sync-bootstrap <BKP> <REPLICA_DIR> <REPLICA_ID>` | Restore a replica and publish its cursor |
| `sync-status [REPLICA_ID]` | Primary-side cursor, lag, and repair action |
| `useradd <NAME> --role <ROLE> --password <PW>` | Create a user (`admin`, `readwrite`, `readonly`) |
| `userdel <NAME>` / `passwd <NAME>` / `users` | Manage the user store |
| `sweep <TABLE\|all>` | Reclaim orphaned overflow pages |

`useradd` and `passwd` also read the new password from `POWDB_NEW_PASSWORD`.

The user-admin subcommands work before the first server start: `useradd`
creates the data directory (owner-only, `0700`) if it does not exist yet, so
provisioning a user can be the very first thing you do on a fresh install.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success |
| 1 | Runtime failure (query error, connection failure, backup failure) |
| 2 | Usage error (bad flag, missing argument) |
