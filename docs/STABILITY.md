# Stability policy

What PowDB promises to keep working across an upgrade, and what it does not.

PowDB is pre-1.0. `CHANGELOG.md` says the project "adheres to Semantic
Versioning", and under SemVer a `0.y.z` project is allowed to break anything in
a minor bump. That is technically accurate and practically useless: it tells you
nothing about whether your data directory survives `cargo install
powdb-cli --version 0.21.0`. This page is the actual promise.

[FORMAT.md](FORMAT.md) documents the *mechanics* (magics, version numbers, the
deprecation floor). This page documents the *commitment*.

## Summary

| Surface | Across a patch (`0.21.0` to `0.21.1`) | Across a minor (`0.21` to `0.22`) |
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

The binary protocol is versioned by **message tag**, not by a negotiated
protocol number. New message types get new `u8` tags (`crates/server/src/protocol.rs`);
existing frames do not change shape. A peer that receives a tag it does not know
fails with `unknown message type: 0x..` rather than misparsing the frame.

Practically:

- An **older client** against a **newer server** works for every feature it
  already knew about. It cannot use features that need new frames.
- A **newer client** against an **older server** works only if it stays on the
  frames that server understands. The TypeScript client handles the analogous
  problem for schema features by declaring a catalog ceiling
  (`SUPPORTED_CATALOG_VERSION`, currently 7) and refusing a server whose catalog
  is newer than it can represent.

`ConnectOk` carries the server's version string, so a client can decide for
itself.

There is no protocol version negotiation today. That is a real gap relative to
the on-disk guarantee, and closing it is a 1.0 requirement (below).

## APIs

The Rust crates, the Node addon, the TypeScript client, and the CLI are all
**allowed to break in a minor release** while PowDB is pre-1.0. Pin exact
versions:

```bash
cargo install powdb-cli --version 0.21.0 --locked
```

```json
{ "dependencies": { "@zvndev/powdb-client": "0.21.0" } }
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

T update { n := .n / .z }        -- 4 rows affected, no error
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

- **Wire protocol version negotiation**, so client and server agree on a
  feature set at connect time instead of discovering a mismatch as an unknown
  tag.
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
