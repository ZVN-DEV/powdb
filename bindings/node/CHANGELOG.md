# Changelog

Changes to `@zvndev/powdb-embedded`, the embedded PowDB native addon for Node.

Releases before this file existed are recorded in the [repository
CHANGELOG](https://github.com/ZVN-DEV/powdb/blob/main/CHANGELOG.md), which
covers every PowDB crate and package in one place. The package version tracks
the engine, so `@zvndev/powdb-embedded` 0.27.0 is PowDB 0.27.0.

## Unreleased

### Fixed

- Engine failures now carry the classification the server puts on the wire.
  Every one of them reached JavaScript as a flat `query_failed` with no class,
  so an embedded caller could not tell a unique-constraint violation from a
  parse error without matching on message text, while a caller of the networked
  `@zvndev/powdb-client` could read the error class for the identical failure.
  Six codes were added (`parse_error`, `timeout`, `size_exceeded`,
  `readonly_refused`, `constraint_violation`, `cancelled`), and every error now
  also carries `errorClass`, numbered exactly as `docs/errors.md` numbers it.
- `Database.open("")` and `Database.open("/some/regular/file")` surfaced the raw
  OS error ("No such file or directory", "Not a directory"), which says nothing
  about what the argument was for. Both are `invalid_argument` now, with a
  message naming the problem.
- `Database.openWithMemoryLimit(dir, NaN)` opened the database with a silently
  coerced zero-byte budget. A non-integer, negative or non-finite budget is
  refused as `invalid_argument`.
- An `undefined` parameter bound a PowQL `null`, so a caller who read a missing
  property off an object silently ran a different query. It is refused with a
  message saying to pass `null` for a null.
- Errors raised by generated argument coercion carried napi's own status names
  (`StringExpected`, `InvalidArg`) rather than any of this package's codes. They
  are reported as `invalid_argument`.
- The "cannot find native binding" message on an unsupported platform blamed a
  known npm bug with optional dependencies. This package ships no optional
  dependencies, so that advice sent readers to reinstall forever. The message
  now names the platforms that have prebuilt binaries and says to build from
  source elsewhere, under the code `unsupported_platform`.
- The README's typed-results sample read `.rows` off `NativeQueryResult` without
  narrowing on `kind`, so it could not be pasted into a TypeScript project.

### Changed

- The package entry point is `loader.js` rather than the generated `index.js`.
  It is what attaches `errorClass`, rewrites napi's coercion status names, and
  replaces the load-failure message; the generated file cannot be hand-edited.
  It re-exports everything `index.js` exports, plus `SUPPORTED_PLATFORMS` and
  `ERROR_CLASS_BY_CODE`.
