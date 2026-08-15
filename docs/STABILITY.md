# Stability policy

What PowDB promises to keep working across an upgrade, and what it does not.

PowDB is pre-1.0. `CHANGELOG.md` says the project "adheres to Semantic
Versioning", and under SemVer a `0.y.z` project is allowed to break anything in
a minor bump. That is technically accurate and practically useless: it tells you
nothing about whether your data directory survives `cargo install
powdb-cli --version 0.23.0`. This page is the actual promise.

[FORMAT.md](FORMAT.md) documents the *mechanics* (magics, version numbers, the
deprecation floor). This page documents the *commitment*.

## Summary

| Surface | Across a patch (`0.23.0` to `0.23.1`) | Across a minor (`0.23` to `0.24`) |
|---|---|---|
| Data directory, read forward | Compatible | **Compatible** |
| Data directory, read backward (downgrade) | Usually | **Not supported** |
| On-disk format version numbers | Unchanged | May be added |
| PowQL / SQL surface | Additive only | Additive; a behavior fix may change results |
| Wire protocol frames | Additive only | Additive only |
| Rust crate APIs (`powdb-*`) | Additive only | **May break** |
| Node / TypeScript client API | Additive only | **May break** |
| CLI flags and subcommands | Additive only | **May break** |

## Your data directory

**A directory written by an older release opens on a newer release.** This is
the one guarantee PowDB takes seriously pre-1.0, and it is enforced by the
format policy in [FORMAT.md](FORMAT.md#format-version-support-policy): every
release reads every on-disk version any previous release could write, through an
explicit compatibility branch, and no branch may be removed until at least 4
minor versions after the release that superseded it.

Upgrading does **not** rewrite your data. New format versions activate *lazily*,
on the first operation that actually needs them:

- catalog v6 on the first expression (JSON-path) index, since v0.13.0
- catalog v7 on the first entity link, since v0.19.0
- heap v3 and row v2 on the first overflow spill, since v0.11.0
- b+tree v3 by rebuilding pre-v3 non-unique column indexes on first writable
  open, since v0.16.0

So a 0.19 directory opened by 0.20 comes back at the version it was already at,
with no silent advance, and only moves to catalog v7 when you declare your first
link. That behavior is pinned by
`crates/storage/tests/catalog_v7_migration.rs`, which reopens a genuinely closed
v6 database, asserts it is still v6, then declares a link and asserts the
upgrade persists across another reopen.

A second caveat, added in v0.22.0, this one about *meaning* rather than layout:
the catalog v7 links section still carries a per-link cardinality byte, but the
engine no longer reads it. Link cardinality is derived from whether the target
key has a unique index, at the moment of the read, so `link` before
`alter <Target> add unique .<key>` and the reverse order now behave identically
instead of freezing the link as to-many forever. The byte keeps its offset and
its declare-time value, so files move in both directions between 0.19-0.21 and
0.22 unchanged, and nothing is rewritten on open. What changed is that a
database whose stored byte disagrees with its schema now reports the schema.
If you have a tool that reads `catalog.bin` directly, stop trusting that byte
and derive cardinality from the target key's index instead. See
[FORMAT.md](FORMAT.md#entity-link-cardinality-byte-catalog-v7).

One caveat on "opens", added in v0.20.0: the heap open scan now verifies every
page checksum and fails closed. A directory that already contained a corrupt
page opened on 0.19 and surfaced the damage later, on the read that touched it;
on 0.20 and later it refuses to open at all with a `PageCorrupt` error. That is
a deliberate trade of a partial-data incident for a loud total one, but it means
an upgrade can turn a silently damaged directory into one that will not start.
There is no salvage mode: restore from a backup. See
[FORMAT.md](FORMAT.md#page-checksums).

**Downgrade is not supported.** Once a lazy bump has fired, an older binary
that does not know that version will refuse the directory with an
`unsupported ... version` error. It fails loudly; it does not misread your data.
A database that never triggers a bump does still open on the older binary, but
treat that as luck, not as a promise. If you need to go back, restore a backup
taken before the upgrade.

**Recommended upgrade procedure**

1. Stop every process holding the directory. `backup` refuses a directory that
   another process has open (since v0.18.1).
2. `powdb-cli --data-dir <dir> backup <dest>`. Keep it until you are satisfied.
3. Install the new release and open the directory.
4. On any failure, restore the backup with the **old** release.

Backups follow the same rule as directories: restorable by the release that
wrote them and by any later release, not by an earlier one. See
[backup-and-restore.md](backup-and-restore.md).

## Query languages

PowQL and the SQL subset grow additively. New keywords, clauses, and functions
appear in minor releases; existing queries are expected to keep parsing.

The exception worth naming: **a correctness fix can change what your query
returns, or turn a statement that used to succeed into an error.** PowDB is
young enough that shipped releases have had genuinely wrong answers, and fixing
them takes priority over bug-for-bug compatibility. Three releases have already
done this:

- **v0.18.2** made a missing value never satisfy any filter comparison (`=`,
  `!=`, `<`, `<=`, `>`, `>=`), matching the documented two-valued behavior.
  Before that fix, `filter .f != x` could match rows whose `.f` was absent.
- **v0.20.0** turned four more silent wrong answers into errors or corrected
  results: an unknown column in `filter` or a projection is now
  `column '<name>' not found` instead of NULL, a type-mismatched comparison is
  now an error instead of true for every row, a negative `limit` is now an error
  instead of being ignored, and `count(T { .col })` / SQL `COUNT(col)` count
  non-null values instead of rows. The same release fixed `datetime`
  comparisons, which had been comparing type tags rather than timestamps and so
  returned the wrong rows on every access path.
- **v0.21.0 refuses DDL inside an explicit transaction.** This is the one change
  in the list that breaks a statement *sequence* rather than a result set, so it
  is called out separately below.

A query relying on any of the old behaviors returns a different result set, or
an error, on the release that fixed it. Changes of that class are called out in
`CHANGELOG.md`. Read it before upgrading if you depend on exact result sets.

### Breaking: DDL inside an explicit transaction is refused (v0.21.0)

**DDL is not transactional in PowDB, and must be run outside `begin` /
`commit`.** Before v0.21.0 a DDL statement inside an explicit transaction was
accepted; from v0.21.0 it is refused with

```
cannot run <verb> inside an explicit transaction: DDL is not transactional
in PowDB, commit or roll back first
```

where `<verb>` is one of `create table`, `create index`, `drop index`,
`create link`, `drop link`, `drop table`, `alter table add column`, or
`alter table drop column`. In PowQL that covers `type`, `materialize`,
`alter ... add/drop column`, `alter ... add index/unique`, `alter ... drop
index`, `link` / `alter ... add link`, and `drop`. (`drop link` is guarded in
the catalog but has no PowQL statement yet, so no query can reach it today.)

This is a breaking change: a script that ran `begin`, then a schema change, then
`commit` used to succeed and now stops at the DDL statement. It was made because
the old behavior silently destroyed data. DDL applies immediately and
irreversibly (`drop` unlinks the heap and rewrites the catalog; `alter` rewrites
rows in place), while `rollback` restores the catalog from disk, which by then
already reflects the DDL. `begin` / `drop` / `rollback` therefore reported
success at every step and left the table permanently gone.

The migration is mechanical: move schema changes out of the transaction. DML
(`insert`, `update`, `delete`, `upsert`) and reads inside a transaction are
unaffected. Because each DDL statement now commits on its own, a migration that
mixes schema and data changes is not atomic; plan each DDL step to be
independently safe to re-run. Making DDL transactional is deliberately deferred.
See [POWQL.md](POWQL.md#ddl-is-not-transactional).

## Wire protocol

The binary protocol is versioned two ways: by **message tag**, and since
v0.22.0 by a **negotiated protocol version and feature set** exchanged in the
handshake. New message types get new `u8` tags
(`crates/server/src/protocol.rs`); existing frames do not change shape. A peer
that receives a tag it does not know fails with `unknown message type: 0x..`
rather than misparsing the frame.

### Version negotiation (since v0.22.0)

`Connect` and `ConnectOk` each carry an optional hello block: a supported
protocol version range, a catalog format ceiling, and a set of named features.
The server answers with the negotiated version and the **agreed** feature set,
which is the intersection of both sides. Anything absent from that set must not
be used on the connection.

If the two ranges do not overlap, the server answers an `Error` frame with
error class `10` (`ProtocolVersion`) **instead of** `ConnectOk` and closes the
connection. That is the guarantee worth having: a version mismatch surfaces
during the handshake, never as an unknown tag mid-session.

**This is backward compatible in both directions, and there is no forced
upgrade order.** Protocol version `1` means "sent no hello block", which is
exactly what every release through v0.21.0 does:

- A **v0.21.0 client** against a **v0.22.0 server** negotiates protocol `1` and
  receives a `ConnectOk` that is byte-identical to the old one.
- A **v0.22.0 client** against a **v0.21.0 server** sends its hello (which the
  old server ignores as trailing bytes), gets a bare `ConnectOk` back, and
  treats the server as protocol `1`.

A newer client only refuses an older server when it explicitly asks for
something that server cannot provide (`requireProtocolVersion` /
`requireFeatures` in the TypeScript client), and that refusal also happens
during the handshake.

The catalog format ceiling folds into the same exchange: the server states the
highest catalog format it writes, and a client whose ceiling is lower refuses
during the handshake rather than failing later. In the TypeScript client this
ceiling now lives in `CLIENT_CAPABILITIES`, with `SUPPORTED_CATALOG_VERSION`
derived from it.

Adding capability later does not require another handshake change: a new
capability is a new feature name, and a new hello field is appended after the
feature list, which both decoders skip.

The full byte layout is documented for driver authors in
[integrations/powql-for-drivers.md](integrations/powql-for-drivers.md#protocol-version-negotiation).

The Rust and TypeScript implementations of this format are independent
hand-written mirrors, so the handshake bytes are pinned in a shared vector file
(`crates/server/tests/wire_vectors/handshake.txt`) that both sides decode,
re-encode, and compare against on every CI run. It also pins the feature-name
list and the error-class numbering, so neither can change in one first-party
codec without the other failing CI. Its scope is the bytes on the wire: it does
not check that a new feature name or error class has been documented anywhere.

## APIs

The Rust crates, the Node addon, the TypeScript client, and the CLI are all
**allowed to break in a minor release** while PowDB is pre-1.0. Pin exact
versions:

```bash
cargo install powdb-cli --version 0.23.0 --locked
```

```json
{ "dependencies": { "@zvndev/powdb-client": "0.23.0" } }
```

Breaking changes are listed in `CHANGELOG.md` under the release that made them.

## Known limitations

Gaps that are real, reproducible, and not yet fixed. They are listed here rather
than left to be discovered, because "correct by default" is only worth anything
if the exceptions are named.

### `update` can store NULL in a `required` column

**`insert` refuses a NULL in a `required` column. `update` does not, and the NULL
is durable.** The two paths disagree, so a `required` column is not an invariant
you can rely on after an update.

```
type T { required n: int, required z: int }
insert T { n := 10, z := 1 }, { n := 20, z := 2 }, { n := 30, z := 0 }, { n := 40, z := 4 }

T update { n := .n / .z }        # 4 rows affected, no error
```

Reopened in a fresh process:

```
 n    | z
------+---
 10   | 1
 10   | 2
 NULL | 0     <-- required column, now NULL, on disk
 10   | 4
```

while the same value through the insert path is refused:

```
insert T { n := null, z := 9 }
Error: column 'n' is required but no value was provided
```

Any update assignment that evaluates to a missing value hits this: division by
zero (as above), arithmetic against a column that is already NULL, and an
explicit `n := null`.

**Why it is not fixed yet.** The obvious fix is to refuse the row. PowDB has no
statement-level atomicity: an `update` writes rows as it walks them, and there is
no undo for the rows already written. Refusing row 3 of 4 mid-scan would leave
rows 1 and 2 updated, row 3 unchanged, and row 4 never visited, and report an
error. That is a **torn write** -- a worse failure than the one it fixes, and it
was implemented, measured, and deliberately reverted during the v0.21.0 sprint
for exactly that reason. Storing the NULL at least leaves the statement's effect
consistent with what it reported.

**The real fix is statement-level atomicity** (buffer the statement's writes and
apply or discard them as a unit), which is the same prerequisite as transactional
DDL. Until that lands there is no correct place to put the check.

**What to do in the meantime.** Filter the rows that would produce a missing
value out of the update (`T filter .z != 0 update { n := .n / .z }`), or check
for NULLs afterwards (`count(T filter .n is null)` does find them, even though
the column is `required`). Wrapping the update in a transaction
does not help: `rollback` undoes the whole statement, but nothing detects the
NULL for you.

## What is not covered

- **Performance is not a stability surface.** Query plans and their costs change
  between releases. Re-measure your own workload after upgrading rather than
  assuming a published number still holds.
- **`powdb-sync` is experimental** and opt-in behind the `sync-enable` feature.
  Its retained-segment format is explicitly not part of the stable data
  directory and may change without the guarantees on this page.
- **Windows** is not a supported platform at all (see the README).

## What 1.0 requires

1.0 is the point at which the API surfaces get the same treatment the on-disk
format already gets. It is not scheduled. These are the conditions:

- ~~**Wire protocol version negotiation**~~, shipped in v0.22.0 (above).
- **A settled PowQL surface**, with a deprecation path for syntax rather than
  removal in a minor.
- **Stable Rust and TypeScript APIs** under normal SemVer, so a minor release
  cannot break a compile.
- **Online backup**, so capturing a recoverable copy no longer requires stopping
  every writer.
- **A downgrade story**, either backward-compatible readers or a supported
  export/import path.

Until then, the honest summary is: your data is safe to carry forward, and
everything you compile or link against is not yet promised.
