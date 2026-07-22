# Catalog v7: persisted entity links (relationship traversal)

**Status:** design, pending approval before implementation.
**Author:** rotation (PM) + powql-lab runs 1-3.
**Scope:** on-disk catalog format bump v6 -> v7 to persist relationship (link)
metadata, unlocking PowQL relationship traversal in both directions. This is a
format change and is treated with the highest rigor: design first, TDD, and a
`kill -9`/restart durability smoke on the real binary before it ships.

---

## 1. Why

Three independent powql-lab investigations asked "why PowQL over SQL?" Two
converged on the same answer: **relationship traversal is the differentiator.**
Run 1 (nested results) already shipped in v0.18.0. Runs 2-3 prototyped the two
remaining halves and proved them, but they sit on lab branches because the link
registry is **in-memory only** and cannot survive a restart or serve a second
connection. Catalog v7 gives that metadata a durable home. Nothing about the
language surface is speculative anymore; this doc is about persisting and
shipping what the lab validated.

### The two read surfaces (already prototyped)
- **N:1 scalar hop** — `Order as o { o.total, o.user.name }` reads one column
  through a relationship. SQL spelling is a JOIN written solely to read a column.
- **1:N nested block** — `User as u { u.name, u.orders { total, status } }`
  returns a native JSON array of shaped child rows per parent, building directly
  on the v0.18.0 nested-projection machinery. SQL spelling fans out rows the
  client must regroup.

### The correctness wedge SQL cannot make by default
A scalar hop through a **non-unique** key is a hard error, never a silent
fan-out. SQL's `JOIN` silently multiplies rows in the same situation. This is a
"correct by default" stance consistent with the two-valued filter semantics we
just shipped in v0.18.2.

---

## 2. What we persist

A **link** is metadata declared on an owner type:

```
link <name> -> <TargetType> on <local_key> = <target_key>
```

Example, declared on `Order`:
```
link user -> User on user_id = id
```

Each stored link record carries:

| Field | Type | Notes |
|---|---|---|
| `name` | string | The traversal name (`o.user...`). Unique per owner type. |
| `target_type` | string | Must resolve to an existing table at declare time. |
| `local_key` | string | Column on the owner type. |
| `target_key` | string | Column on the target type. |
| `kind` | u8 enum | `0 = ToOne` (N:1 scalar), `1 = ToMany` (1:N block). |

`kind` is derived at declare time from whether `target_key` is backed by a
unique index/constraint on the target (unique target key => `ToOne`, else
`ToMany`), and stored explicitly so query planning never re-derives it. A
`ToOne` link whose target key later loses uniqueness is caught at query time by
the non-unique-hop error, not silently downgraded.

Links are **read-only metadata**: they add no storage, no secondary structures,
no triggers. They are a naming layer over columns that already exist. Deleting
a link is metadata-only. Dropping a table or column that a link references is
refused while the link exists (same integrity discipline as indexes).

---

## 3. On-disk format (v6 -> v7)

The catalog is a hand-rolled binary file: `MAGIC | version:u16 | n_tables:u32 |
<table entries...> | CRC32`. It already uses a documented **field-reading
staircase**: each version appends fields that older files simply default. v7
follows that exact contract by appending one new section **after** the table
entries and **before** the CRC:

```
... existing table entries ...
n_links: u32                      # 0 for any pre-v7 file (defaulted by reader)
repeat n_links:
    owner_type:  len-prefixed str
    name:        len-prefixed str
    target_type: len-prefixed str
    local_key:   len-prefixed str
    target_key:  len-prefixed str
    kind:        u8
CRC32
```

**Reading:** `read_catalog_file_with_max_version` already accepts every version
in `1..=CATALOG_VERSION` and defaults absent fields. For a v6 file the reader
stops after the table entries and defaults `n_links = 0`. For a v7 file it reads
the links section. No change to magic or CRC framing. The existing
`version > max_supported_version` guard already rejects a v7 file on an old
binary with a clear "unsupported catalog version" error (not a crash, not
corruption).

**Writing / activation is lazy**, identical to how v6 activates on the first
expression index:
- A database that never declares a link **stays at v6.** Opening, reading, and
  writing it is byte-for-byte unchanged. Existing users are unaffected forever
  unless they opt in by declaring a link.
- The **first** `link ...` declaration flips `active_catalog_version` to 7 and
  persists through the existing `persist_at_activation_boundary()` path, with
  the same rollback-on-failure semantics already proven for expression indexes
  (on any persist error the version and in-memory state revert and the temp
  files are removed).

---

## 4. Migration, rollback, forward-compat

- **Forward (v6 -> v7):** automatic and free. There is no data rewrite; a v6
  catalog is a valid v7 catalog with zero links. The bump happens only on first
  link declaration. No user action, no dump-and-reload. This mirrors the BTREE
  v3 migration shipped in v0.16.
- **Old binary opening a v7 DB:** refused with the existing clear error
  (`unsupported catalog version: 7`). Pre-1.0 policy already documents that
  minor versions may change on-disk formats, and SECURITY/README carry that
  caveat. A user who wants to stay on the old binary simply never declares a
  link.
- **Downgrade:** no automatic downgrade (consistent with every prior bump).
  Because activation is lazy, the blast radius is limited to databases that
  actually adopted links.
- **Backup/restore & sync:** the backup manifest copies `catalog.bin` verbatim,
  so links ride along automatically; a restored v7 catalog is opened and
  validated exactly like today. Sync stamps segments with the **active** catalog
  version (`read_active_catalog_version`), which already reads the persisted
  header, so a link-using database advertises v7 to peers and a link-free one
  still advertises v6.

---

## 5. Query surface (from lab prototypes)

Parser/AST work already exists on `powql-lab/scalar-links` and
`powql-lab/entity-links` and is ported, not re-invented:

- **DDL:** `link <name> -> <Target> on <local> = <target>` as a bare declaration
  and as an `alter type` action. Declaring a link validates that both tables and
  both columns exist and that `name` does not collide with a column or another
  link on the owner.
- **Scalar read:** `o.user.name`, multi-hop `o.user.company.name`. Resolved
  against the link registry at execution time (planner stays pure). Evaluated
  with one key->value map per hop.
- **Block read:** `u.orders { ... }` desugars onto the existing nested-projection
  executor; the correlation predicate comes from the link instead of being
  hand-written.
- **Kind mismatch is an error:** a block through a `ToOne` link, or a scalar path
  through a `ToMany` link, is a clean, pinned error message.
- **Non-unique scalar hop is an error:** the correctness wedge above.

### Nullability (settled by the lab, restated)
Missing at any hop yields an **empty value; rows never drop.** A `ToOne` hop
whose local key is NULL, or whose target row is absent, evaluates `o.user.name`
to Empty (the same Empty our v0.18.2 two-valued filter semantics already treat
as "never matches a comparison"). A childless `ToMany` parent gets `[]`, exactly
as nested projections already do. This deliberately avoids re-introducing SQL's
three-valued-logic surface.

---

## 6. Test & verification plan (TDD, findings-become-tests)

Every item is written failing-first.

**Storage (catalog):**
1. v7 round-trip: declare links, serialize, deserialize, identical registry.
2. Staircase back-compat: a hand-written v6 catalog opens at v6 with zero links;
   after one link declaration it persists as v7 and reopens with the link.
3. Activation rollback: injected persist failure reverts version 7 -> 6 and
   leaves no temp files (mirror the expression-index rollback test).
4. Old-binary refusal: a v7 file fails `read_catalog_file_with_max_version(_, 6)`
   with the exact "unsupported catalog version: 7" message.
5. CRC still validates over the extended payload (tamper a link byte -> reject).

**Durability (real binary):**
6. `kill -9` after declaring a link + inserting rows, then restart: the link and
   the rows both survive WAL replay.
7. Backup a v7 database, restore into a fresh dir, links present and traversable.

**Query (both frontends where applicable):**
8. Scalar hop, multi-hop, ToMany block, kind-mismatch error, non-unique-hop
   error, NULL/missing-at-hop -> Empty, childless -> `[]`.
9. Plan cache: a link-traversal query shape caches and re-executes correctly
   (guard against the literal-substitution class of bug we fixed before).

**Release gate:** the standard post-publish live-registry smoke, plus a
0.18.x -> 0.19.0 migration leg (open a real v6 database written by the installed
0.18.2 binary, declare a link, confirm v7 activation and traversal).

---

## 7. What this is NOT

- Not a foreign-key constraint system: links do not enforce referential
  integrity on write, they name a read path. (FK enforcement could be a later,
  separate opt-in.)
- Not a storage-layout change: no new files, no row-format change, no index
  required beyond what the user already has.
- Not a planner-semantics change: resolution is execution-time, the planner
  stays pure (a core PowDB invariant).
- Not a Postgres/MySQL wire feature: this is PowQL-native.

---

## 8. Rollout

1. This design approved.
2. Storage: v7 format + lazy activation + tests (1-5), no query surface yet.
3. Query: port lab parser/AST/executor, tests (8-9).
4. Durability + backup + sync tests (6-7), migration leg.
5. Docs: POWQL.md links section, on-disk-format note, README differentiation
   line rewritten around traversal.
6. Ship as **0.19.0** (minor bump: new format + new language surface), full
   release train + post-publish smoke + migration leg.

Estimated core diff ~440 non-test lines per the lab prototype, plus the storage
format layer and the test matrix above.
