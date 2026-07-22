# PowDB on-disk format policy

PowDB v0.5.0 makes persisted formats explicit: every durable structure has a
magic/version boundary and unknown versions fail loudly before decode. Legacy
0.4.x data without a new marker is treated as version `0` only where a
compatibility reader exists.

## Current format versions

| Structure | Magic | Current writer | Legacy accepted | Unknown behavior |
| --- | --- | ---: | ---: | --- |
| Catalog (`catalog.bin`) | `BCAT` | 5; 6 once the first expression index activates it (since v0.13.0); 7 once the first entity link is declared (since v0.19.0) | 1, 2, 3, 4 (and 5/6 where the newer format was never activated) | reject `unsupported catalog version` |
| Catalog LSN sidecar (`catalog.lsn`) | none (raw `u64` LE) | n/a | absent (durable LSN reads as 0) | n/a (value-only file) |
| B+tree index (`*.idx`) | `BIDX` | 1 (unique column indexes), 2 (expression indexes, since v0.13.0), 3 (non-unique column indexes, since v0.16.0) | 1, 2; a non-unique column index found below 3 is rebuilt from the heap on open | reject `unsupported btree version` |
| Heap file (`*.heap`) | `PHEAP` in page-0 superblock | 2; 3 once the first overflow chain is written (since v0.11.0) | 1 (no superblock, pre-v0.5.0) | reject `unsupported heap format version` |
| Heap page | flags high nibble | 1 | 0 | reject `unsupported page format version` |
| Row payload | `PROW` prefix | 1 (fully inline rows); 2 only for rows carrying a spilled value (since v0.11.0) | 0 (no prefix, pre-v0.5.0) | reject `unsupported row format version` |
| WAL (`wal.log`) | `PWAL` file header | 1 | 0 (records at byte 0, pre-v0.5.0) | reject `unsupported WAL format version` |
| Retained sync segment (experimental) | `PRUL` header + `RULF` footer | 1 | none | reject `unsupported retained segment format version` |

## Heap files

New heap files reserve page 0 for a `PageType::Meta` superblock. Data pages
start at page id 1. The superblock stores `PHEAP`, heap format version, flags,
page size, and the first data page id. Existing heap files whose page 0 is not a
meta superblock are opened as legacy heap v1.

## Rows

New row bytes are `PROW` + `u16 version` + the legacy compact row body. The body
still begins with its `u16` body length, followed by the null bitmap, fixed
region, variable-offset table, and variable data. This keeps legacy row decoding
available while giving new rows a front-door version guard.

## WAL

New WAL files start with `PWAL`, `u16 version`, and `u16 flags`; records begin at
byte 8. Legacy WAL files without the magic continue to replay from byte 0. A bad
or future `PWAL` version is rejected instead of being interpreted as a record.

## Catalog LSN sidecar

Since v0.8.0 the data directory holds a `catalog.lsn` sidecar next to
`catalog.bin`: an 8-byte little-endian `u64` recording the durable page-LSN
high-water mark, written via a temp-file-and-rename. It carries no magic or
version — it is a value-only file. A database written before v0.8.0 has no
sidecar; its durable LSN reads as `0`, and the file is created on the next
durable write, so older databases and backups open unchanged.

## Retained sync segment (experimental)

The experimental embedded-sync substrate (v0.8.0, `powdb-sync`, opt-in via
`sync-enable`) writes retained replication-unit log segments guarded by a `PRUL` header
(magic + `u16` format version, currently `1`) and a trailing `RULF` footer.
An unrecognized magic or a future version is rejected rather than decoded.
This format is not part of the stable data directory and is only present when
embedded sync is enabled.

## Compatibility rule

Readers may accept older versions only through an explicit compatibility branch.
They must never silently reinterpret a recognized magic with an unknown version.
Any new durable structure must add its magic/version constants and tests before
it is used by production code.

## Format version support policy

What the current release (v0.19.0) supports:

- **Reads:** every on-disk version listed in the table above, which is every
  version any released PowDB has ever written. No released data directory is
  currently unreadable by the current release.
- **Writes:** always the current version for anything newly written. Some
  version bumps are activated lazily so that untouched databases stay openable
  by older binaries: catalog v6 on the first expression index (v0.13.0),
  catalog v7 on the first entity link (v0.19.0), heap v3 and row v2 on the
  first overflow spill (v0.11.0), and the b+tree v3 rebuild of pre-v3
  non-unique column indexes on first writable open (v0.16.0). A database that
  never declares an entity link stays at its current catalog version and opens
  unchanged on an older binary.

The commitment:

1. Every release reads every version a previous release could write, through
   an explicit compatibility branch, until that branch is retired under the
   deprecation mechanism below.
2. Writers always emit the current version (lazily-activated versions count as
   current once triggered); no release writes a superseded version.
3. Unknown or future versions always fail loudly with an
   `unsupported ... version` error, never a silent misread.

Deprecation mechanism. A legacy read branch may be removed only when all of
the following hold:

- At least **4 minor versions** have shipped since the last release whose
  writer could produce that version (the superseding release). Example: a
  format superseded in v0.16.0 keeps its read branch through at least v0.19.x
  and may be removed in v0.20.0 at the earliest.
- A **stepping-stone release** exists that migrates the structure to a current
  version automatically (rewrite-on-open, lazy bump on first touch, or index
  rebuild-on-open), or, failing that, a documented offline migration exists
  (back up with the old release, restore with the new one).
- The release that removes the branch **names the stepping-stone in its
  release notes** ("data directories written before vX must first be opened by
  a release in the vX..vY range"), and after removal the version is rejected
  loudly, never misread.

Index files (`BIDX`) are a special case: an index is always rebuildable from
the heap, so a superseded index version may be served by automatic
rebuild-on-open instead of a byte-level compatibility read (this is how pre-v3
non-unique column indexes are handled since v0.16.0). The rebuild path itself
follows the same 4-minor-version retention rule before any removal.

Current status: nothing is scheduled for removal. The oldest branches (heap
v1, row v0, WAL v0, catalog v1-v4, all superseded by v0.7.0 or earlier) have
long passed the 4-minor-version floor but stay in place because they are small
and tested; any future removal will follow the mechanism above and be called
out in the changelog one release in advance.
