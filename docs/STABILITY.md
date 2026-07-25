# Stability policy

What PowDB promises to keep working across an upgrade, and what it does not.

PowDB is pre-1.0. `CHANGELOG.md` says the project "adheres to Semantic
Versioning", and under SemVer a `0.y.z` project is allowed to break anything in
a minor bump. That is technically accurate and practically useless: it tells you
nothing about whether your data directory survives `cargo install
powdb-cli --version 0.20.0`. This page is the actual promise.

[FORMAT.md](FORMAT.md) documents the *mechanics* (magics, version numbers, the
deprecation floor). This page documents the *commitment*.

## Summary

| Surface | Across a patch (`0.19.1` to `0.19.2`) | Across a minor (`0.19` to `0.20`) |
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

So a 0.18 directory opened by 0.19 comes back at the version it was already at,
with no silent advance, and only moves to catalog v7 when you declare your first
link. That behavior is pinned by
`crates/storage/tests/catalog_v7_migration.rs`, which reopens a genuinely closed
v6 database, asserts it is still v6, then declares a link and asserts the
upgrade persists across another reopen.

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
returns.** PowDB is young enough that shipped releases have had genuinely wrong
answers, and fixing them takes priority over bug-for-bug compatibility. v0.18.2
made a missing value never satisfy any filter comparison (`=`, `!=`, `<`, `<=`,
`>`, `>=`), matching the documented two-valued behavior; before that fix,
`filter .f != x` could match rows whose `.f` was absent. Any query relying on
the old behavior returns a different result set on 0.18.2 and later. Changes of
that class are called out in `CHANGELOG.md`. Read it before upgrading if you
depend on exact result sets.

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
cargo install powdb-cli --version 0.19.1 --locked
```

```json
{ "dependencies": { "@zvndev/powdb-client": "0.19.1" } }
```

Breaking changes are listed in `CHANGELOG.md` under the release that made them.

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
