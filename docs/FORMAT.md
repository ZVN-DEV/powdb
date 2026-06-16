# PowDB on-disk format policy

PowDB v0.5.0 makes persisted formats explicit: every durable structure has a
magic/version boundary and unknown versions fail loudly before decode. Legacy
0.4.x data without a new marker is treated as version `0` only where a
compatibility reader exists.

## Current format versions

| Structure | Magic | Current writer | Legacy accepted | Unknown behavior |
| --- | --- | ---: | ---: | --- |
| Catalog (`catalog.bin`) | `BCAT` | 3 | 1, 2 | reject `unsupported catalog version` |
| B+tree index (`*.idx`) | `BIDX` | 1 | none | reject `unsupported btree version` |
| Heap file (`*.heap`) | `PHEAP` in page-0 superblock | 2 | 1 (no superblock) | reject `unsupported heap format version` |
| Heap page | flags high nibble | 1 | 0 | reject `unsupported page format version` |
| Row payload | `PROW` prefix | 1 | 0 (no prefix) | reject `unsupported row format version` |
| WAL (`wal.log`) | `PWAL` file header | 1 | 0 (records at byte 0) | reject `unsupported WAL format version` |

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

## Compatibility rule

Readers may accept older versions only through an explicit compatibility branch.
They must never silently reinterpret a recognized magic with an unknown version.
Any new durable structure must add its magic/version constants and tests before
it is used by production code.
