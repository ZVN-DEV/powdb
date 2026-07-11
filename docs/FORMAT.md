# PowDB on-disk format policy

PowDB v0.5.0 makes persisted formats explicit: every durable structure has a
magic/version boundary and unknown versions fail loudly before decode. Legacy
0.4.x data without a new marker is treated as version `0` only where a
compatibility reader exists.

## Current format versions

| Structure | Magic | Current writer | Legacy accepted | Unknown behavior |
| --- | --- | ---: | ---: | --- |
| Catalog (`catalog.bin`) | `BCAT` | 5 | 1, 2, 3, 4 | reject `unsupported catalog version` |
| Catalog LSN sidecar (`catalog.lsn`) | none (raw `u64` LE) | — | absent (durable LSN reads as 0) | n/a — value-only file |
| B+tree index (`*.idx`) | `BIDX` | 1 | none | reject `unsupported btree version` |
| Heap file (`*.heap`) | `PHEAP` in page-0 superblock | 2 | 1 (no superblock) | reject `unsupported heap format version` |
| Heap page | flags high nibble | 1 | 0 | reject `unsupported page format version` |
| Row payload | `PROW` prefix | 1 | 0 (no prefix) | reject `unsupported row format version` |
| WAL (`wal.log`) | `PWAL` file header | 1 | 0 (records at byte 0) | reject `unsupported WAL format version` |
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
