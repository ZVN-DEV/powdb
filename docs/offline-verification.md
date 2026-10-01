# Offline verification

PowDB includes read-only verification for data directories and full backups. It is meant for operators who want evidence that a snapshot can be served or restored before they trust it.

Verification never repairs data. A failed report names the failing check and gives a remedy, usually “recover pending WAL with a normal engine” or “restore from a known-good backup.”

## Commands

Verify a quiescent data directory:

```bash
powdb-cli verify --data-dir ./powdb_data
powdb-cli --format json verify --data-dir ./powdb_data
```

Verify a full backup without writing a restore destination:

```bash
powdb-cli verify-backup ./backups/full
powdb-cli --format json verify-backup ./backups/full
```

Run a restore drill into a fresh empty directory:

```bash
powdb-cli verify-backup ./backups/full \
  --restore-drill-dir ./drill-restored \
  --compare-source ./powdb_data
```

`--compare-source` is optional, but it requires `--restore-drill-dir` because there must be a restored copy to compare. The drill destination must be empty; the verifier refuses to overwrite existing files.

## Exit codes

- `0`: every required check completed and no errors were reported.
- `1`: verification ran but found corruption, unsafe live state, malformed backup input, or a failed restore drill.
- `2`: command-line usage error, such as a missing path or `--compare-source` without `--restore-drill-dir`.

## JSON report shape

`--format json` prints one stable JSON object:

```json
{
  "schema_version": 1,
  "target": "database:./powdb_data",
  "ok": true,
  "checks": [
    {
      "name": "catalog_open_read_only",
      "status": "ok",
      "detail": "catalog opened read-only and WAL was clean"
    }
  ],
  "errors": [],
  "warnings": []
}
```

Each error and warning has:

- `code`: stable short category, such as `heap_crc_failed`, `index_mismatch`, `manifest_duplicate`, or `restore_destination_not_empty`.
- `message`: human-readable evidence.
- `remedy`: recommended operator action.

## What `verify` checks

`verify` takes the shared administrative reader lock for the lifetime of the check. This publishes a normal reader lock file under `readers/` and removes it on normal exit. The lock is metadata only; it is not a data repair.

The verifier then opens the catalog with `Catalog::open_read_only`, which refuses pending committed WAL instead of replaying and truncating it. A directory with uncheckpointed writes must be opened once by a read-write engine to recover before it can pass offline verification.

For every table, `verify`:

- checks every heap page CRC with `heap.verify_integrity`;
- enumerates row IDs from the heap;
- validates raw row format;
- decodes each row through strict `Table::get(RowId)`, so corrupt v2 overflow chains fail closed instead of being hidden by scan fallback behavior;
- reconstructs column-index contents from rows and compares indexed row IDs and entry counts;
- reconstructs JSON-path expression indexes and compares the loaded `.eidx` B+tree contents;
- enforces unique index constraints from the reconstructed logical rows.

It also validates metadata:

- link owner/target tables and columns exist;
- link cardinality can be derived from current index metadata;
- materialized-view backing tables exist.

Links do not enforce row-level referential integrity in PowDB, so `verify` does not reject unmatched link values. A materialized view marked dirty is reported as a warning because it needs refresh, but it is not storage corruption.

## What `verify-backup` checks

`verify-backup` reads `manifest.json` and fails before any restore write if it sees:

- unsupported manifest version;
- invalid file name, path traversal, or nested path;
- duplicate file entry;
- missing `catalog.bin`;
- missing referenced file;
- symlink or non-regular file;
- file length mismatch;
- blake3 hash mismatch.

With `--restore-drill-dir`, it restores the full backup through the existing restore path into a fresh directory, verifies that restored directory, and optionally compares logical rows plus index metadata with `--compare-source`.

## Limits

Verification is offline. It is not an online backup protocol, does not replay WAL, does not rebuild indexes, and does not salvage corrupt pages. If a required check cannot complete, the report fails closed rather than silently returning partial coverage.
