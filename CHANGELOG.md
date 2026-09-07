# Changelog

All notable changes to PowDB will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Breaking

An index, not a second copy: every item below is tagged `**BREAKING (...)**` on
the entry itself, which is where the reasoning lives. Nothing here is a new
fact. It exists because 35 breaking items are spread across four sections of a
long release, and twelve of them change what a query answers with no error
raised at all, which is the group an upgrading user gets no signal about.

`cargo-semver-checks` runs zero lints on a 0.x MINOR bump, so the Rust API
changes below get no automated verdict on this release. This list is the record.

**Answers or written data change, with no error raised.** Re-check anything that
compares floats, sums floats, ranges over a `bytes` column, measures string
length, negates a filter on an optional column, reads result columns by name, or
keys an `update` / `delete` on a `datetime`, `uuid` or `bytes` column:

- `Changed` A comparison against NaN follows IEEE.
- `Changed` Float `sum` and `avg` are accumulated with compensation.
- `Changed` An ordered comparison on a `bytes` column no longer uses the index.
- `Changed` `length()` counts characters, not bytes.
- `Changed` `not` over a missing value is the plain complement (rows that were
  dropped are now kept).
- `Changed` A projection column is named after what it computes (result column
  names move: `?` and `__agg_0` become real names).
- `Changed` A number binds as `int` when it is integral and inside the signed
  64-bit range (`@zvndev/powdb-client`).
- `Fixed` A repeated query no longer answers a different question (a cached
  query's second and later executions were computing something else).
- `Fixed` A comparison against a `datetime`, `uuid` or `bytes` column coerces its
  literal. **An `update` or `delete` keyed on such a column was a silent no-op
  and now writes rows.**
- `Fixed` `union` de-duplicates both branches (row counts fall).
- `Fixed` An ordered comparison across two types is false (rows of another type
  are no longer returned).
- `Fixed` `x in (...)` answers what `x = ...` answers.
- `Fixed` `length()` on a `bytes` column answers its byte count (it was null
  on every row, with no error).

**Statements, directories or deployments that used to be accepted are now
refused.** These announce themselves, but they fail work that previously ran:

- `Fixed` `length()` on an `int`, `float`, `bool`, `datetime`, `uuid` or
  `json` operand is a typed refusal naming the column and its type.
- `Changed` An arithmetic result with no int64 answer is an error, not the
  missing value.
- `Changed` An `int` column refuses a float that is not exactly that integer.
- `Changed` An unknown backslash escape in a string is a parse error (catches
  Windows paths and regexes written in literals).
- `Changed` A cast literal against a mistyped column is an error.
- `Changed` `having` and `like` refuse a mistyped operand.
- `Changed` A pipeline clause that carries a value may be written once.
- `Changed` A supplied parameter the query never references is an error.
- `Changed` A type no row of which could ever be stored is refused at `type` and
  at `alter ... add column`.
- `Changed` `drop <name>` on a materialized view is refused; use `drop view`.
- `Changed` A damaged data directory refuses to open instead of opening smaller.
- `Changed` WAL damage inside the log refuses the open.
- `Changed` A malformed `POWDB_*` value refuses startup with exit 2. A
  deployment carrying a typo (`POWDB_READONLY=ture`) used to start and serve the
  directory writable.
- `Changed` `powdb-cli --exec` exits 1, not 0, when a script ends inside an open
  transaction.
- `Fixed` `@zvndev/powdb-embedded` rejects arguments it used to coerce: an
  `undefined` parameter no longer binds a PowQL null, and a `NaN` memory limit
  no longer opens with a zero-byte budget.
- `Security` A data directory the owner cannot open is refused rather than
  widened; 0000 is refused outright.

**API, wire and operational contracts:**

- `Changed` A corrupt page read fails closed. Rust: `HeapFile::get`,
  `Table::get` and `Table::index_lookup` return `io::Result<Option<..>>`, so
  every caller of the three must handle the error arm. A point lookup on a
  rotted page raises `PageCorrupt` where it answered zero rows.
- `Changed` Engine error text reaches remote clients verbatim. Message text
  changes for everything that used to arrive as the bare `query execution
  error`, `InvalidIdentifier` moves from wire class 0 to 2 and `RowTooLarge` /
  `ValueTooLarge` from 0 to 4, and Rust callers of
  `powdb_server::protocol::Message::decode` get `Result<Message, DecodeError>`
  where they got `Result<Message, String>`.
- `Changed` A served sync pull chunk stays inside the wire envelope every
  released peer decodes. This makes the 4096-unit cap a hard product ceiling of
  about 4094 row changes per transaction; a larger transaction is answered with
  a typed rebootstrap rather than served. See
  [docs/embedded-sync.md](docs/embedded-sync.md).
- `Changed` `@zvndev/powdb-sync`: `LocalApplyRequest.primaryGeneration` is
  `bigint`, a compile break for an adapter implementer, and
  `DEFAULT_MAX_PULL_UNITS` moves from 512 to 4096.
- `Fixed` `@zvndev/powdb-embedded`: engine failures carry the wire error class,
  so `err.code` values change. Anything matching the flat `query_failed` must be
  updated, as it was for the same package in 0.26.0.
- `Added` SIGHUP reloads the user store. The server installed no SIGHUP handler
  before, so `kill -HUP` terminated it; it now reloads `auth.json` and keeps
  serving.
- `Fixed` Backups now carry `auth.json`, so a server started on a restored
  directory enforces authentication where it previously accepted
  unauthenticated connections.
- `Security` A shared-password deployment counts auth failures against the peer,
  not the username. Unix-socket peers are rate limited for the first time and
  share one bucket, a shared-password server drops from 50 to 5 guesses a minute
  per peer, and `powdb_server::handler::AuthRateLimiter`'s underlying type
  changed.

### Added

- **`drop link <Owner>.<name>`**, also spelled `alter <Owner> drop link
  <name>`, both with `if exists`. A declared entity link could be created but
  never removed short of dropping the table.

- **`powdb-cli --readonly`** opens an embedded data directory through
  `Engine::open_read_only`, so a shell or a script can read a directory it must
  not write. `--readonly` together with `--remote` is a usage error naming the
  server-side setting, and a read-only open never creates the directory. A
  missing `--data-dir`, or one that names a file, is reported as such instead
  of surfacing a bare OS error.

- **`powdb-cli --remote` accepts a Unix-domain socket path.** A path (rather
  than `host:port`) connects over the socket `powdb-server --socket` publishes;
  before this it failed with `connection failed: invalid socket address`, so
  the CLI could not reach a socket-only server at all.

- **`powdb-cli --password-stdin`**, accepted wherever `--password` is, and
  refused alongside it. Only the line terminator is stripped, so a password
  ending in spaces still works. `--password` now warns that the value is
  visible in `ps`, without printing it.

- **Per-subcommand `--help` in `powdb-cli`.** `backup --help` used to run the
  backup and fail on a missing data directory. Every subcommand answers its own
  help from the same strings that back the top-level `--help`.

- **`--wal-checkpoint-bytes <BYTES>`** on `powdb-server` (env
  `POWDB_WAL_CHECKPOINT_BYTES`) and on `powdb-cli`'s embedded path, with
  `Catalog::set_wal_checkpoint_bytes`, `Catalog::wal_checkpoint_bytes` and
  `powdb_storage::catalog::DEFAULT_WAL_CHECKPOINT_BYTES` (64 MiB) behind them.
  The threshold is a plain byte count with no unit suffix; `0` disables the
  automatic checkpoint and restores the grow-until-close behaviour. **It has no
  effect in either shipped binary yet:** the automatic checkpoint is skipped
  whenever a WAL archive hook is installed, and both `powdb-server` and
  `powdb-cli` install one unconditionally, so the log still grows until an
  explicit checkpoint or the close. Embedded callers that open without an
  archive hook get the bound.

- **A TLS certificate's expiry is read at startup.** `powdb-server` warns when
  the certificate it is about to serve has expired or is close to expiring, and
  a certificate or key it cannot use is refused with a message naming what is
  wrong (an EC key with explicit curve parameters names the command that
  re-encodes it, a duplicate `basicConstraints` extension is named).

- **BREAKING (operational):** **SIGHUP reloads the user store** without restarting the server, so a
  password rotated with `powdb-cli passwd` takes effect on the next
  authentication and a deleted user stops being able to log in. `powdb-cli`'s
  user-admin subcommands now say so when a server holds the data directory.

- **`powdb_sync::archived_through_lsn`**: the highest LSN the retained archive
  carries, answered from segment file names alone, with no lock taken.

- **`powdb-sync` and `powdb-backup` emit `tracing` spans and events**: what a
  backup copied, which increments a restore chain applied, where a blake3
  mismatch was found, and a warning when the sync-aware checkpoint in
  `SyncCatalog::drop` fails. That last one previously swallowed the error
  entirely, so retained history stopping short left no trace anywhere.

- **`docker run <image> --version` works.** The image's entrypoint routes a
  leading `-` argument to `powdb-server`, so the documented flags can be passed
  straight to `docker run` without repeating the binary path. A first argument
  that is not a flag is still executed as its own command, so
  `docker run <image> sh` and the compose overrides in `examples/deploy/`
  behave exactly as before.

- **The published ghcr image is tagged with the bare version.** A `vX.Y.Z` tag
  now also produces `ghcr.io/zvn-dev/powdb:X.Y.Z` alongside `vX.Y.Z`, `X.Y`,
  `X` and `latest`. `examples/deploy/` and the docs have always pinned the bare
  form; until now that tag did not exist for a fresh release until the floating
  minor tag happened to line up.

- **The publish workflow builds and smoke-tests a release binary before it
  publishes anything.** `publish.yml` runs the same `scripts/smoke-release.sh`
  the release workflow runs, on the same commit, before the first crate goes
  out. crates.io publishes are irreversible, so the check that a release binary
  actually starts, serves a query and replays its WAL now happens on the near
  side of the door rather than minutes later in a parallel workflow.

- **`post-publish-smoke.yml` checks all four channels.** It verified the CLI and
  the two npm packages; it now also installs the `powdb` embedded facade crate
  from crates.io and runs a query through it, asserts the npm packages'
  dist-tags and peer pins, pulls the ghcr image for both architectures and runs
  `--version` under emulation, and verifies the GitHub Release's assets exist,
  are non-empty, match their published checksums and carry a valid build
  provenance attestation.

- **New CI gates**, each with a `--selftest` that CI runs immediately before the
  real check: `scripts/ci/semver-gate.sh` (cargo-semver-checks really linted
  something), `scripts/ci/check-dependabot-coverage.sh` (every tracked
  dependency manifest is watched), `scripts/ci/strip-empty-unreleased.sh`, and
  `scripts/ci/check-tool-pins.sh` (every `cargo install` in a workflow carries
  `--version` and `--locked`, a tool installed by two workflows is pinned to the
  same version in both, and a scan that matched no install at all fails). Four
  `--selftest` entry points run as their own workflow steps:
  `semver-advisory.sh` (4 cases), `check-ci-success-needs.sh` (14),
  `internal-content-guard.sh` (6) and `check-tool-pins.sh` (8).

- **The MSRV job runs the tests.** It compiled the workspace on the MSRV
  toolchain and stopped there, so an MSRV-incompatible construct anywhere in a
  `#[cfg(test)]` block or a dev-dependency was invisible. The clippy, ASan,
  fuzz-replay and examples-smoke jobs also gained a second operating system.

- **`clients/ts/scripts/check-readme-samples.mjs`** extracts every fenced
  `ts`/`js` block from the three package READMEs, compiles each against the
  package's real types under `strict`, and reports diagnostics at the README
  line they came from. Wired up as `pnpm test:readme` in `clients/ts` and
  `clients/sync`.

- **`bindings/node/CHANGELOG.md`**, shipped in the npm tarball.

### Changed

- **BREAKING (API/wire):** **A corrupt page read fails closed instead of answering "no such row".**
  `HeapFile::get` and `Table::get` mapped an I/O failure and a CRC refusal to
  `None`, and ten executor sites read `None` as "the row is deleted", so an
  indexed point lookup on a page that rotted after the directory was opened
  answered zero rows with a success status. Both now return
  `io::Result<Option<..>>`, with `Ok(None)` reserved for a slot that really is
  deleted or out of range, and every executor site propagates. The refusal
  reaches an engine caller as a `PageCorrupt` error and a wire client as error
  class 0 (`internal`). `Table::index_lookup` moved with them
  (`io::Result<Option<(RowId, Row)>>`). See
  [docs/STABILITY.md](docs/STABILITY.md).

- **BREAKING (now refused):** **An arithmetic result with no int64 answer is an error, not the missing
  value.** `+`, `-` and `*` that overflow, and `/` by a divisor that is zero for
  some row, used to evaluate to the missing value: indistinguishable from a
  missing column, and an `update` wrote it into a required column. All four now
  refuse the statement, and an `update` that hits one writes no row at all.

- **BREAKING (silent):** **A comparison against NaN follows IEEE.** `=`, `<`, `>`, `<=` and `>=`
  against NaN are false and `!=` is true, on every access path. Ordering,
  `group`, `distinct` and index key order still place NaN in one definite
  position, and a join on a NaN key still matches.

- **BREAKING (silent):** **Float `sum` and `avg` are accumulated with compensation.** The same rows
  summed through a sequential scan and through an index gave two different
  numbers on a column that mixes very large and very small values, and neither
  was the exactly rounded total. Every float accumulator in the engine now
  carries the lost low-order bits, so a float aggregate is the same number on
  every access path. **Reported float aggregate results may move**, always
  towards the exact answer. Integer sums are unchanged.

- **BREAKING (now refused):** **An `int` column refuses a float that is not exactly that integer.** `30.7`
  was stored as `30`, `-0.5` as `0` and `1e300` as `i64::MAX`, on insert and
  update alike.

- **BREAKING (silent):** **An ordered comparison on a `bytes` column no longer uses the index.** The
  B+tree orders bytes keys by length before content, so an index-backed
  `.y >= "\\x0102"` silently dropped every stored value of a different length,
  including the row holding `\xff`. `<`, `<=`, `>` and `>=` on a bytes column
  are answered by the scan, which orders bytes bytewise as every other path
  does. Equality still uses the index, so a point lookup keeps its plan; a range
  over an indexed bytes column now plans a filtered scan and costs what a scan
  costs.

- **BREAKING (silent):** **`length()` counts characters, not bytes.** `length("café")` was `5`, and
  disagreed with `substring`, which has always been character-indexed.

- **BREAKING (now refused):** **An unknown backslash escape in a string is a parse error**, in PowQL and in
  the SQL frontend alike, in string literals and in quoted identifiers. The
  lexer used to drop the backslash and keep the rest, so `"a\Ab"` was stored as
  `aAb` and a quoted SQL identifier `"i\d"` silently resolved to `id`. The
  escape set is `\'`, `\"`, `\\`, `\n`, `\t`, `\r`, `\0` and `\uXXXX`. One
  consequence: a `bytes` literal must be written with the doubled-backslash
  form (`"\\x0a"`), which was always the only form that worked.

- **BREAKING (now refused):** **A cast literal against a mistyped column is an error** where it was silently
  false. `.u = bytes("...")` on a column that is not `bytes` used to match no
  row and say nothing.

- **BREAKING (now refused):** **`having` and `like` refuse a mistyped operand.** `.n like "abc"` on an int
  column, `.s like 5`, and `having count(.id) = "x"` used to return no rows;
  they now raise the same typed error `filter` raises.

- **BREAKING (now refused):** **A pipeline clause that carries a value may be written once.**
  `User filter .a = 1 filter .b = 2` used to keep only the second predicate and
  answer as if the first had never been written; the same held for `order`,
  `limit`, `offset`, `group` and the projection block. Writing one twice is now
  a parse error. Repeated `having` still chains with `and`.

- **BREAKING (now refused):** **A supplied parameter the query never references is an error.** Binding three
  parameters to a query that mentions `$1` and `$2` used to run with the third
  silently ignored, which hides a caller's off-by-one instead of reporting it.

- **BREAKING (silent):** **`not` over a missing value is the plain complement.** `not (.x = 1)` on a
  row where `.x` is missing was itself missing, so the row was dropped. PowQL's
  filter logic is two-valued everywhere else, and now here too: the row is kept.

- **BREAKING (silent):** **A projection column is named after what it computes.** `D { .j->a, .j->b }`
  came back as two columns both called `?`, and a grouped aggregate written
  without an alias was headed by the planner's internal `__agg_0`. SQL names an
  unaliased aggregate the way SQL does: `count(n)`.

- **BREAKING (now refused):** **A type no row of which could ever be stored is refused at `type` and at
  `alter ... add column`.** Declaring several hundred columns used to succeed
  and then fail on every insert, including inserts that set no value at all,
  because the empty row already exceeded the page budget. The refusal says how
  many bytes the columns need and how many a row may use.

- **BREAKING (now refused):** **`drop <name>` on a materialized view is refused; use `drop view <name>`.**
  It removed the backing table and left the definition behind, so the view
  stayed registered, still went dirty on writes to its source, and reported a
  missing table on every read, across restarts.

- **BREAKING (now refused):** **A damaged data directory refuses to open instead of opening smaller.** A
  heap whose length is not a whole number of pages was rounded down, so
  `truncate -s 100` on a 500-row table gave a table with no rows and no error. A
  catalogued table whose heap is zero bytes was the same defect with a length
  that divides evenly. And a `catalog.bin` missing beside surviving heap and
  index files reported `NotFound`, which every caller reads as "fresh
  directory", so a new catalog was created next to the data and every table
  vanished; that is now `CatalogCorrupt` naming `catalog.bin`, while a genuinely
  empty directory still reports `NotFound`.

- **BREAKING (now refused):** **WAL damage inside the log refuses the open; only a torn tail is
  truncated.** A CRC failure mid-log returned the prefix before it and the log
  was then truncated with nothing logged, which is data loss dressed as
  recovery. Replay now looks for a valid record after the bad one, first where
  the length field says it ends and then with a bounded byte scan. If one is
  there the damage is inside the log and the open refuses; if nothing is there
  it is the torn tail of the write that was in flight, which warns with the
  offset and the discarded byte count and truncates as before. A WAL whose file
  header is not `PWAL` was read as a pre-0.5.0 headerless log, silently dropping
  every un-checkpointed row; a directory whose catalog or heap format is past
  that era now refuses.

- **BREAKING (now refused):** **A malformed `POWDB_*` value refuses startup.** A typo in a deployment's
  environment used to run the server on a default nobody chose. Every setting
  now goes through the same validator as its flag and exits 2 naming the
  variable and the value. `0` is refused for `POWDB_IDLE_TIMEOUT` and
  `POWDB_QUERY_TIMEOUT`; `POWDB_TX_MAX_LIFETIME_MS=0` keeps its documented
  meaning of disabling the bound. A refusal also names the unit the setting is
  actually in (connections, bytes, candidate pairs, pages) rather than calling
  every one of them a byte count.

- **The user store is re-read while the server runs.** `auth.json` was read once
  at startup, so a password rotated with `powdb-cli passwd` did not take effect
  and a deleted user could keep logging in until a restart. The server stamps
  the file by mtime and length and re-reads it when it changes, and SIGHUP
  forces a reload. The check is a stat on a path that is already rate limited.

- **A begin-only explicit transaction no longer blocks readers.** `begin` took
  the write-admission gate outright, so a connection that had opened a
  transaction and not yet written made every other connection's read wait for
  it, up to `POWDB_TX_WAIT_TIMEOUT_MS`. Reads are now admitted from a second
  pool that only readers take from, tied to the transaction's own hold so it
  cannot leave the door open for the next holder of the gate. The transaction's
  first write closes that pool and drains it, which waits for the reads already
  running and for nothing else. A read during a transaction that has written
  still waits, and a second `begin` and an autocommit write still wait as
  before.

- **BREAKING (API/wire):** **Engine error text reaches remote clients verbatim, classified by type.**
  Egress rendering was a prefix allowlist over the message text, so any
  diagnostic whose wording did not start with one of about 35 recognized
  phrases arrived as the bare string `query execution error`: a mistyped column,
  `commit` outside a transaction, a malformed `uuid` or `bytes` literal, a
  missing required column, an unknown insert field, a negative limit, an upsert
  on a non-unique key, a non-scalar path-index key, a refresh of an unknown
  view. The same statement was diagnosable embedded and undiagnosable over the
  wire. The decision now runs exhaustively on the `QueryError` variant, so
  rewording a diagnostic can never silently mask it. Only messages that can
  carry internal state are still redacted: a plain I/O failure, corruption
  detail, and the read-only retry sentinel. Two classes moved with it:
  `InvalidIdentifier` reaches clients as class 2 and `RowTooLarge` /
  `ValueTooLarge` as class 4, where all three were class 0 (a server fault with
  nothing to fix on your side) for what is the caller's own data. A frame-level
  refusal now answers with an `Error` frame carrying its class rather than only
  being logged before the close.

- **The in-flight read-ahead byte budget defers a frame rather than buffering
  it.** The read-ahead arm passed the 64 MiB wire limit as its frame bound, so
  the 1 MiB budget only decided whether a read may start: a peer that opened a
  statement and then announced a 64 MiB frame buffered all 64 MiB of it on that
  connection, per connection, and `Vec::drain` never returns the capacity. A
  frame that does not fit the remaining budget is now neither refused nor lost:
  its header stays buffered, read-ahead pauses for the rest of the statement
  exactly as it does when the queue is full, and the main loop reads the whole
  frame at the full wire limit as soon as the statement finishes.

- **BREAKING (API/wire):** **A served sync pull chunk stays inside the wire envelope every released peer
  decodes.** Chunking was changed this cycle so a chunk runs on to the commit or
  rollback that closes the transaction the cut would otherwise land inside,
  because a chunk cut inside a transaction is not applyable and wedged the
  replica for good. The chunk is capped at 4096 units: every decoder through
  v0.27.0 refuses a `SYNC_PULL_RESULT` declaring more than that, and a v0.27.0
  replica pulling a larger chunk got a frame-level decode failure that drops the
  socket with no diagnostic and repeats on every retry. A transaction that does
  not fit is answered with the rebootstrap status the byte budget already
  produces, naming it, rather than a bare error the replica retries forever. A
  chunk still runs past the replica's `maxUnits` hint up to that cap, so the
  1000-row transaction the hint-not-a-limit rule was written for is still served
  whole. The server's own decoder ceiling is raised from 4096 to 262144 units to
  match `clients/ts/src/protocol.ts`, so what a v0.28 peer accepts and what a
  v0.28 primary sends are now two separate numbers: raising what is sent needs a
  negotiated wire feature.

- **BREAKING (now refused):** **`powdb-cli --exec` fails a script that ends inside an open transaction.** It
  exited 0 while every write the script made was discarded. It now exits 1 with
  `transaction still open at end of script; rolled back`. A script that commits
  or rolls back explicitly is unaffected.

- **`sync-status` reports `awaitArchive` as a momentary state, not an error.** A
  replica waiting for the primary to archive has not failed, and printing it
  under `lastSyncError` read as one. A `rebootstrap` still reports its error.

- **BREAKING (silent):** **A number binds as `int` when it is integral and inside the signed 64-bit
  range**, in `@zvndev/powdb-client` as it already did in the embedded addon.
  The wire client tagged a number `int` only when `Number.isSafeInteger` held.
  Every double in `[2^53, 2^63)` is an exact integer, so the `int` tag is
  lossless there, and the float tag cost the index: measured against a running
  server, `filter .id = $1` with `2 ** 60` on a `required unique id: int`
  planned `Filter(SeqScan)` over the wire and `IndexScan` in process.
  Snowflake-shaped ids sit squarely in that range. `2 ** 63` and above still
  bind float.

- **`@zvndev/powdb-client` gains a `maxInFlight` connect option** (default 64)
  and the `invalid_argument` error code. `queryTyped`'s generic is no longer
  constrained to `TypedRow`: the bound rejected the `interface` most callers
  declare a row type with, while claiming a check the method never performed.

- **BREAKING (API/wire):** **`@zvndev/powdb-sync` raises `DEFAULT_MAX_PULL_UNITS` from 512 to 4096**, and
  `LocalApplyRequest` extends the new `NormalizedSyncIdentity` so
  `primaryGeneration` is `bigint`, matching what a replica already passes.

- **`@zvndev/powdb-embedded` is entered through `loader.js`** rather than the
  generated `index.js`, and exports `SUPPORTED_PLATFORMS` and
  `ERROR_CLASS_BY_CODE`. Anything that bypasses the loader loses the error
  classification.

- **`@zvndev/powdb-sync` builds on the same TypeScript and `@types/node` majors
  as `@zvndev/powdb-client`**; both declare `sideEffects: false`.

- **The published `powdb-query` crate no longer ships the executor's unit
  tests**, about 300 KB of `#[cfg(test)]` source in every download.

- Internal, CI and release engineering. **Dependabot watches every dependency
  manifest**: it watched 3 of the 7, and `clients/sync`, `bindings/node` and
  `crates/query/fuzz` were unwatched while the Dockerfile's base images were
  pinned with nothing to move them; a new check fails if that drifts again in
  either direction. **Third-party tooling in CI is pinned and
  checksum-verified**: gitleaks and cargo-semver-checks are verified with
  `sha256sum -c` (the semver-checks fetch no longer pipes `curl` into `tar`,
  which cannot verify anything), cargo-fuzz is pinned, and `cargo audit` runs
  from a pinned, cached binary rather than a source build of a floating
  version, with the audit suppression living only in `.cargo/audit.toml`.
  **The miri job is sharded five ways and its coverage is asserted**: it was one
  job filtering on a module prefix, which is how it once spent months filtering
  on a module that did not exist, and a new check runs the real test list
  through the shard map and fails if any in-scope test is claimed by no shard or
  by more than one. **The Docker builder image is digest-pinned**, like the
  runtime image already was, and **the Docker healthcheck no longer logs an
  error every 30 seconds** (the probe read the PONG frame's header and closed,
  leaving bytes in the receive queue, which makes the kernel send RST instead of
  FIN; it now drains the frame and closes cleanly). **Publish workflows read
  their dispatch inputs through environment variables** rather than
  interpolating them into shell text. **Concurrency groups only cancel in-flight
  runs on pull requests**, so a push to `main` or a tag build can no longer be
  cancelled by the next push. **The published npm packages no longer carry an
  empty `## Unreleased` heading** in their bundled CHANGELOGs; a section with
  content in it is left alone.

### Fixed

- **BREAKING (silent):** **A repeated query no longer answers a different question.** The plan cache
  collected a query's literals in source order and re-bound them in plan-walk
  order, and the two disagree wherever the planner reorders clauses. The
  everyday case was a projection above a slice: `T { x: .n + 1 } limit 5`
  computed `.n + 1` on its first execution and `.n + 5` on every one after, and
  `T filter ... { x: .n + 1 } limit 5 offset 0` silently started skipping five
  rows. A projection whose literal cannot be ordered against a sort key's is no
  longer cached at all.

- **BREAKING (silent):** **A comparison against a `datetime`, `uuid` or `bytes` column coerces its
  literal.** `.u = "<the exact uuid>"` was false on every row, `!=` true on
  every row, `update` and `delete` by such a key touched nothing, and the index
  was never probed. The literal is now coerced once, where the plan enters
  execution, so the scan, the compiled predicate, the index probe and the
  mutation's discovery scan all read the same value; a literal that cannot be
  coerced is the typed error `insert` already gives.

- **BREAKING (silent):** **`union` de-duplicates both branches.** `A union B` kept duplicate rows that
  came from the left branch.

- **BREAKING (silent):** **An ordered comparison across two types is false.** `.j->v > 99.5` returned
  the rows whose value was the string `"deep"` and the bool `true`, because the
  fallback ordering ranked the two values by type.

- **BREAKING (silent):** **`x in (...)` answers what `x = ...` answers.** The list compared with strict
  variant equality while `=` compares numerically, so `.n in (28.0)` and
  `.n = 28.0` disagreed.

- **A float literal can be written with an exponent.** `1e9`, `1.5E-3` and
  `2e+10` were lexed as an integer followed by an identifier, so a query using
  scientific notation failed with a confusing parse error. A literal whose value
  is not finite is refused rather than becoming `inf`.

- **A materialized view over an empty source is typed from its source.** Every
  column came back `str`, so the first refresh that produced a real row failed
  with a type error the user could not act on.

- **Refreshing a materialized view reclaims the space the last one used.** A
  view's backing table grew on every refresh, because deleted rows never
  returned their bytes.

- **A materialized view tracks its source through nested projections and link
  traversals.** Those dependencies were not recorded, so writes to the source
  left the view clean and it served stale rows.

- **A `views.bin` that cannot be decoded refuses the open.** It was read as "no
  materialized views", so every `materialize` ever run vanished and the next one
  overwrote the file that still held the definitions.

- **A query that reads a table that does not exist says which table.**
  `count(Missing)` reported the storage layer's generic error and a join against
  a missing table reported a missing *column*; every read shape now reports the
  same typed "table not found" naming the table. It carries
  `StorageErrorKind::TableNotFound`, so the same mistake no longer changes wire
  error class depending on which layer noticed it (class 2 either way).

- **Calling a function PowQL does not have says so.** `T { x: foo(.id) }`
  reported `column 'foo' not found` and `T filter foo(.id) = 1` reported an
  unexpected `(`. Both now name the function, with a suggestion when the name is
  close to a real one.

- **A link traversal outside a projection names the limitation.**
  `Post as p filter p.author.name = "ann"` reported an unexpected trailing
  token; it now says a link path is only usable in a projection.

- **A prepared write inside a transaction no longer fsyncs at every statement.**
  The four prepared-statement commit points called the autocommit commit
  unconditionally, so a statement inside `begin` / `commit` flushed and fsynced
  the WAL at every statement boundary where the identical rows written as PowQL
  text did not. 5000 single-row inserts through a prepared statement inside one
  transaction cost 5000 fsyncs where the text path cost 78. The two paths now
  cost the same.

- **`PRIMARY KEY` on a column works in SQL `CREATE TABLE`**, mapping to
  `required unique`; a table-level `PRIMARY KEY (col)` now names the spelling
  that works instead of a message about table constraints.

- **SQL `CREATE TABLE` accepts the `JSON` (and `JSONB`) column type.** The JSON
  examples in `docs/SQL.md` were unreachable through SQL DDL.

- **Two processes could open one data directory for writing.** The writer
  directory lock published its PID file first and took the kernel `flock`
  afterwards, over whatever inode the `LOCK` path had come to name, and then
  discarded the answer. Two servers started against the same `--data-dir` (a
  systemd restart overlapping the old process, two containers on one bind mount)
  could both be admitted, both opening the same heap and the same WAL. The lock
  is taken first and is what admits a writer: the PID is written in place
  through the locked handle, so it never names a process that does not hold the
  directory, and a lock that is already held is a refusal rather than a shrug. A
  lock on an inode the path no longer names is not accepted, so a previous
  holder's cleanup cannot hand the directory to two acquirers at once. A `LOCK`
  naming a dead or ghost PID whose lock is free is still reclaimed, so the
  container and PID-namespace case the lock file was extended for still works.
  On a filesystem with no advisory locking at all (some network mounts) PowDB
  logs a warning and falls back to the PID heuristic rather than refusing to
  open.

- **`insert` could spin forever on a corrupt heap page, wedging the server.**
  `Page::compact` refuses a page whose slot directory points outside it, and the
  free-space search re-offered the page on the strength of the dead space it
  could never reclaim. One insert into such a table looped inside the engine
  write lock and blocked every other reader and writer until the process was
  restarted, whereupon it happened again. A page that cannot be compacted now
  leaves the free list: the rows it holds stay readable, and it is never offered
  for new ones.

- **A statement could report a durable write as failed.** The automatic
  checkpoint that runs after a commit is housekeeping, but its failure was
  returned through the commit's own result. An I/O failure there (a filesystem
  that has just filled is the obvious one) told the client the write had not
  happened when it had, and a retry on a table with no unique constraint stored
  the row twice. The checkpoint failure is now logged, not returned; the commit
  is durable before it runs.

- **The automatic checkpoint could truncate a WAL somebody was archiving.** It
  skipped only when PowDB's own sync was enabled, but `Engine::new_with_wal_archive`
  installs an arbitrary archive hook, and such a caller has no `.powdb-sync`
  directory. After enough writes the next commit truncated `wal.log` without
  running the hook, losing every record since the last explicit checkpoint,
  silently. A catalog opened with an archive hook no longer runs the automatic
  checkpoint at all.

- **`insert` cost no longer grows with the size of the heap.** The free-space
  search kept one flat list of every page with 64 bytes free and walked it from
  the front, installing each candidate until one happened to fit. For a 1 KB row
  a page holds three rows and keeps about 990 bytes free, which is over the
  threshold and under the row size, so every page in the heap stayed on that
  list and every candidate was a guaranteed miss: placing one row cost one page
  install per page in the heap. A size-segregated in-memory summary (free bytes
  per page, 64-byte buckets, lazy deletion of stale entries) now looks only in a
  bucket guaranteed to fit. Loading 40,000 rows of 1 KB went from 45.2 s to
  68.7 ms, and 100 inserts into that heap from 233 ms to 147 us; at 200,000 rows
  the same 100 inserts cost 138 us. No on-disk format change. The summary is
  kept exact on every insert, which on a bulk load into a small heap costs about
  7 ns per row (Depot, same instance as 0.27.0: `insert_10k` +5% to +8% across
  four runs, `insert_batch_1k` -53%); deferring the hot page's update to the
  moment it is unpinned would remove it and is the follow-up.

- **Deleted space is allocatable again.** `Page::delete` only wrote a tombstone,
  so a table that was emptied and refilled grew by the full amount a second
  time. `Page::compact` slides live rows to the front of the data area,
  preserving slot indices so every `RowId` stays valid, and the free-space
  summary counts dead bytes as reclaimable so the insert path compacts a page
  just before reusing it.

- **A page written before checksums shipped is stamped on open.** Such a page
  carried no CRC and was never rewritten, so bytes that rotted after the open
  scan reached a decoder unchecked, and `row_body`'s `expect` on the row format
  version aborted the process under `panic = "abort"`. `row_body` is now
  infallible (the `PROW` prefix is fixed width whatever the version claims),
  `HeapFile::get` validates the row format of what it reads off disk, and the
  open scan stamps any page it finds without a CRC on a writable handle, so a
  live database never keeps a page outside the checksum.

- **The WAL is bounded during a run.** It was truncated only by an explicit
  checkpoint, which in practice meant only on `Drop`, so a process that stayed
  up through a long write workload carried every record it had ever written and
  replayed all of it after a crash. A finished statement now checkpoints once
  the durable log passes `--wal-checkpoint-bytes` (64 MiB by default). An open
  explicit transaction, and a directory publishing retained sync history, are
  skipped rather than turned into errors, since an insert must not fail because
  the log crossed a size.

- **WAL replay no longer drops records in silence.** A `DdlCreateTable` replay
  that failed was swallowed and every later `Insert` for that table was dropped;
  it now propagates, naming the table. Records that do not decode, or that name
  a table this catalog does not have, are counted, the first one is warned with
  its index and its reason, and the count joins the replay summary line.
  `rebuild_indexes_from_heap` skipped a row whose overflow chain would not
  reassemble, so crash recovery rebuilt a column index that was silently missing
  it. DDL replay discarded the result of the row rewrite and of the page
  stamping, so a failed migration came back as a half-migrated table that read
  as success and panicked on the next projection. All of them propagate now.

- **WAL replay refuses a page id the log could not have produced.** Replay
  handed a record's `page_id` straight to the insert and overflow-write paths,
  which grow the heap one page at a time up to it, so a crafted or corrupt page
  id meant a hang and a full disk. Replay now refuses any page id past what the
  log could have grown the heap to. The nightly fuzz artifact of 2026-08-30 is
  checked in as a seed and refuses in 9.5 ms instead of running forever.

- **BREAKING (operational):** **Backups no longer lose materialized views or the user store.** A full or
  incremental backup enumerated `catalog.bin`, the catalog LSN sidecar, every
  heap and every index, and stopped there. Two durable files live beside them
  and were never copied: `views.bin`, the materialized-view registry, and
  `auth.json`, the user store. A restored directory therefore lost every view
  definition (`refresh V` answered "not found", and reads of `V` served whatever
  rows the backing heap happened to hold) and lost every user, **so a server
  started on the restore accepted unauthenticated connections**. Both are now in
  the manifest set, in the restore name allowlist, and on the incremental path,
  and both are optional (copied when present). The durable file names a data
  directory holds are now one list in `powdb_storage::data_dir`, which the
  catalog, the view registry and the directory lock read from, so `powdb-backup`
  no longer maintains a second copy by hand. A new test walks a data directory
  that exercises every durable-file kind and fails on anything the manifest set
  does not claim.

- **A committed transaction larger than the sync pull window can be pulled.**
  The chunk was cut at `maxUnits`, the cut landed inside the transaction, the
  primary refused its own chunk, and every retry cut in the same place: the
  replica was wedged for good while `status` reported `repairAction: "pull"`
  with `lastSyncError: null`. Both sides capped `maxUnits` at 4096 and the JS
  client defaulted to 512, so a 1000-row transaction already wedged it.
  `maxUnits` is a hint now: the chunk runs on to the commit or rollback that
  closes the transaction it is standing in, bounded by the byte budget and the
  4096-unit chunk cap. The one case that genuinely cannot be served, a
  transaction too large for either bound, answers with a rebootstrap status
  naming it rather than a bare error the replica would retry forever. Ack ranges
  are bounded separately.

- **A live primary archives committed history on demand.** Retained segments
  were written only by a checkpoint, and the only checkpoint a running primary
  performed was on graceful shutdown, so every write on a live primary sat
  unarchived and a replica polling it was told `awaitArchive` forever.
  `sync-status` and `sync-pull` archive pending history when a replica asks, and
  only when something is unarchived: the archive's own high-water mark, read
  from segment file names before any lock is taken, gates it. Before that gate
  the demand archive ran a full checkpoint on every frame, under the engine
  write lock and with every transaction-gate permit held, so a replica polling
  once a second stalled all client traffic once a second against a primary with
  nothing new to archive. Measured on a 2000-row database over 20 idle status
  polls, the worst wait seen by a thread contending for the engine write lock
  fell from 5986 us to 98 us. Archiving is best effort: a checkpoint refuses
  while a transaction is open, so the archive stops at the last commit.

- **`sync-status` reports `rebootstrap` when the tail holds unapplyable DDL**,
  with a reason naming it, instead of leaving the replica to discover it one
  failed pull at a time.

- **`@zvndev/powdb-client`: a bad parameter no longer desynchronizes the
  connection.** `2 ** 63`, `1e19`, `1e300` and `Number.MAX_VALUE` were tagged as
  ints and threw a raw Node `RangeError` out of `writeBigInt64LE` after the
  pending entry had been queued, leaving a FIFO slot waiting for a reply the
  server was never asked for: the next query hung and later ones took
  `ECONNRESET`. Encoding now happens before anything is queued, an integral
  double outside the safe-integer range binds as described under Changed, and an
  out-of-range `bigint` or a non-finite number is refused synchronously as
  `invalid_argument`.

- **`@zvndev/powdb-client`: a server-initiated `Error` frame keeps its text and
  class.** An idle timeout, a reaped transaction or "server shutting down"
  arrived as `protocol_error: received unexpected frame from server`, throwing
  away what the server said. Raw socket errors are wrapped as `PowDBError`
  `closed` with the Node error as `cause`.

- **`@zvndev/powdb-client`: an over-cap result no longer poisons the
  connection.** A result past `MAX_RESULT_CELLS` (2,000,000) closed the client
  for every later call. It is the one decode failure that leaves the byte stream
  intact, so the frame is skipped, only the query that asked for it fails (with
  `size_exceeded`), and the connection stays up.

- **`@zvndev/powdb-client`: a burst no longer dies mid-flight.** With no
  in-flight cap, 200 concurrent queries answered 22 and then took `ECONNRESET`
  against the server's frame read-ahead budget. Requests past the window wait in
  a local FIFO and go out as replies come back.

- **`@zvndev/powdb-client`: the in-flight window bounds bytes as well as
  frames.** A server's read-ahead budget is 128 frames *and* 1 MiB, and the byte
  cap is the tighter one for anything but small statements: 64 frames of 20 KiB
  of query text is 1.28 MiB of read-ahead while the frame count never approaches
  128. Against a pre-0.28.0 server, which cancelled the running query and closed
  the connection with no `Error` frame, that was the exact `ECONNRESET` the
  window was added to prevent. The pump now also stops at `MAX_IN_FLIGHT_BYTES`
  (1 MiB, the server's own cap), and always writes the head frame on an empty
  window so one oversized statement cannot deadlock.

- **`@zvndev/powdb-client`: `execScript` dispatch is bounded by the in-flight
  window.** `send` stopped writing directly when the window landed: it queues the
  encoded frame and the pump writes up to `maxInFlight` of them. The dispatch
  loop still yielded only on socket backpressure, which the window makes
  unreachable, so it never yielded at all: its fail-fast and abort guards could
  not fire (a 400-statement script dispatched all 400 after statement 6 failed),
  and every frame past the window accumulated in the client's heap (roughly
  50 MB for a 10,000-statement script of 5 KB inserts). Dispatch now waits for
  the reply that frees each statement's slot. The window still slides, so the
  pipeline stays full and throughput is unchanged.

- **`@zvndev/powdb-client`: `Pool` evicts dead connections.** It handed out idle
  clients whose socket had already closed, costing one failed `withClient` per
  idle client after a server restart. The acquire timeout is a `PowDBError`
  coded `timeout` now, not a bare `Error`.

- **`@zvndev/powdb-client`: a reserved word can be used as an identifier.**
  `escapeIdent("select")` returned a bare keyword the engine refused, and it
  refused the backticked form too, so no spelling of a reserved table name
  worked; it now backtick-quotes a reserved word. `escapeSqlIdent`, twelve lines
  below it, kept returning the bare name, so there was no injection-safe
  spelling of a reserved-word table left in SQL either: it now returns the
  validated name double-quoted. The SQL frontend has read `"..."` as an
  identifier since 0.23.0 and does no case folding, so this is safe for every
  name and needs no second keyword list. A column *type* is a keyword, not an
  identifier, and must not be passed through it. `escapeLiteral` and
  `escapeSqlLiteral` range-check bigints instead of emitting an integer literal
  above `i64::MAX`.

- **`@zvndev/powdb-client`: every abort is a `PowDBError` coded `aborted`,**
  with the abort reason on `.cause`. `ctrl.abort(new Error(...))` used to reject
  with the bare `Error`, so `err.code === "aborted"` did not hold for a custom
  reason.

- **BREAKING (API/wire):** **`@zvndev/powdb-embedded`: engine failures carry the wire error class.**
  Every one reached JavaScript as a flat `query_failed` with no class, so an
  embedded caller could not tell a unique-constraint violation from a parse
  error without matching on message text, while a networked caller could read
  the class for the identical failure. Six codes were added (`parse_error`,
  `timeout`, `size_exceeded`, `readonly_refused`, `constraint_violation`,
  `cancelled`) and every error now carries `errorClass`. `Database.open` and its
  three sibling factories carry it too: the loader wrapped only writable
  function properties, and napi defines `#[napi(factory)]` statics as
  non-writable, so the four ways to obtain a handle were the one surface with no
  classification while the shipped `.d.ts` promised it everywhere. The exported
  class is now a subclass owning classifying statics, with `Symbol.hasInstance`
  kept pointing at the native class so `instanceof` is unchanged.

- **BREAKING (now refused):** **`@zvndev/powdb-embedded`: rejected arguments say what is wrong.**
  `Database.open("")` and `Database.open` on a regular file surfaced the raw OS
  error; `openWithMemoryLimit(dir, NaN)` opened with a silently coerced
  zero-byte budget; an `undefined` parameter bound a PowQL `null`. All are
  `invalid_argument` now. Errors from generated argument coercion, which carried
  napi's own status names, are reported the same way, on every entry point.

- **`@zvndev/powdb-embedded`: the unsupported-platform message is honest.** It
  blamed a known npm bug with optional dependencies, which this package does not
  ship, sending readers to reinstall forever. It names the platforms that have
  prebuilt binaries and carries the code `unsupported_platform`.

- **`@zvndev/powdb-sync`: the default pull byte budget matches the primary's
  ceiling.** `maxPullUnits` was raised to the server's 4096 while `maxPullBytes`
  stayed at 4 MiB against a `MAX_SYNC_PULL_BYTES` of 16 MiB. The byte budget is
  the one that cuts a chunk, and a chunk cut inside a transaction is not
  applyable, so any transaction between 4 and 16 MiB (about 1,000 rows of 5 KB)
  made the primary answer `rebootstrap` on every pull, for good, and the replica
  re-wedged on the next transaction of that size after each bootstrap. Both
  budgets are also range-checked against the primary's ceilings at construction,
  which previously rejected only zero and left an over-large `maxPullBytes` to
  fail server-side on every pull.

- **`@zvndev/powdb-sync`: every entry point reports a transport failure as
  `remote_unavailable`.** `status()` and `syncNow()` let the underlying client's
  own code escape, so a caller branching on `PowDBSyncErrorCode` saw codes
  outside that union.

- **The README samples in all three JS packages type-check.** Each shipped one a
  reader could not paste into a strict TypeScript project: `queryTyped<User>`
  with an `interface` (TS2344), `.rows` off an un-narrowed `NativeQueryResult`
  (TS2339), and a sync adapter body whose `primaryGeneration` was
  `bigint | number` against the addon's `bigint` (TS2345). Two of the three were
  the types being wrong.

- **`powdb-cli` says which statement failed.** A multi-statement `--exec` or
  script reported the error with nothing identifying the statement that raised
  it; it now names the position and quotes the text. The REPL accepts one
  trailing `;`, `.schema`'s usage line matches `.help`, and the REPL banner says
  "Type SQL queries" when `--sql` put it in SQL mode.

- **`powdb-server` and `powdb-cli` report a TLS certificate problem instead of a
  bare handshake failure.** The CLI gives `CaUsedAsEndEntity` its own hint, and
  the generic hint no longer suggests a flag that was already passed.

- **A health probe's disconnect no longer logs as a warning.** A peer that
  closes before `CONNECT` logs `peer disconnected before CONNECT` at DEBUG and a
  mid-session peer that vanishes logs `peer disconnected` at DEBUG; anything
  else is still WARN or ERROR.

- **The nightly toolchain pin is current again**, and the freshness check that is
  supposed to catch it now sees every shape a pin takes. It scanned only one of
  them, so the `FUZZ_TOOLCHAIN` env value and the `cargo +nightly-...`
  invocations aged silently past the limit and turned the nightly fuzz run red.

- **`publish.yml`'s semver gate could not fail.** `cargo-semver-checks`
  classifies a 0.x MINOR bump as a major change and runs zero lints, so on every
  ordinary release the gate passed having checked nothing. It now refuses a run
  that examined no crate or ran no check on a patch bump, and reports how many
  crates were linted. Separately, a 0.x minor bump could be published with no
  semver verdict at all: `cargo-semver-checks` skips every lint on a 0.x minor
  (correctly, a 0.x minor is allowed to break), so the advisory pass is that
  release shape's whole verdict, and it now refuses `cargo-semver-checks`
  failing to run at all. Findings still never block.

- **The "advisory, never blocks" semver step in `publish.yml` blocked**, and
  would have failed the next crates.io publish. An inline `run:` block runs under
  GitHub's default `bash -e {0}`, and `set -uo pipefail` does not clear the
  inherited `-e`, so the first non-zero exit from `cargo semver-checks
  --release-type minor` killed the shell before the log, the job summary and the
  trailing `exit 0`. The `semver` job went red and `publish` (`needs: semver`)
  never started. Forcing `--release-type minor` turns the major lints on, and
  this release removes public items in published crates, so it would have fired.
  The body now lives in `scripts/ci/semver-advisory.sh` with an explicit
  `set +e` and a `--selftest`.

- **`ci-success` could pass with an empty `needs:` list.** It is the single
  required status check on `main`, and its verdict was `for result in $RESULTS`
  over `join(needs.*.result, ' ')`, which iterates zero times on an empty list
  and prints success. The job that would have caught it blocked nothing except
  through `ci-success` itself. `ci-success` now runs the completeness check
  directly, against `ci.yml` on disk rather than against the list it is
  auditing. `check-ci-success-needs.sh` could also not see a job whose key line
  carried a trailing comment (`  my-new-gate: # TODO` is valid YAML and was
  invisible to the parser, so it was compared against neither `ci-success.needs`
  nor CONTRIBUTING.md while the script reported every job covered); any
  unrecognised job-key shape is now an error rather than a silent drop.

- **`internal-content-guard` reported "denylist checked" when `git grep` had
  errored and inspected nothing.** `|| true` conflated git-grep exit 1 (no
  match) with exit 2 and above (an uncompilable PCRE in the denylist secret, a
  git build without `-P`). The `git ls-files` half printed "paths clean" the same
  way. Both halves now read the producing command's status directly, and the
  guard also skipped silently when its pattern file was missing.

- **Three of the five pattern scans in `check-version-consistency.sh` had no
  "found nothing" floor.** Pinning the deploy compose file to a digest (which the
  deploy README recommends), regenerating `site/` without the leading `v`, or
  documenting the npm dependency as `^0.27.0` each silently emptied a scan and
  the job still printed its ALL-PASS line. The script also now checks the
  AGENTS.md release stamp: the "Available in released PowDB (vX.Y.Z)" line
  drifted three minors behind and nothing noticed, and it is the line agents read
  to decide which features exist.

- **`release.yml`'s npm publish skipped itself when a registry probe failed for
  any reason**, including a 500 or a network error, so an outage looked like a
  successful release.

- **`publish-node-addon.yml` verified a tag that it had not necessarily built.**
  It resolved the version from the tag but built from whatever the checkout gave
  it; it now asserts the tag resolves to the commit being built. It also asserts
  the addon's lockfile is up to date and runs the package's tests on each
  platform runner it publishes from.

- **`release-channel.sh` accepted leading zeros** (`0.27.00`, `01.2.3`) as valid
  SemVer, which would have published a release under a version string crates.io
  and npm reject.

- **`fuzz.yml` installed an unpinned `cargo-fuzz`** after the commit that pinned
  the third-party tools the gates run on. It now matches ci.yml's
  `--version 0.13.2 --locked`.

- **The ghcr smoke leg checked two of the three tags its own comment demanded.**
  The floating channel pointer (`latest` for a final, `rc` for a candidate) was
  never pulled, so a release that moved it to the wrong digest, or never moved
  it, passed the smoke. All three tags are now pulled and all three have their
  `org.opencontainers.image.version` label asserted.

- **Two gates that could not fail now can.** The format-version gate reads
  `docs/FORMAT.md` instead of comparing the constants to a second copy of
  themselves in the same file, so a bumped version with a stale table fails. The
  data-directory mode gate asserts the warning it is named for instead of the
  mode the code already produced without it.

- **Six CLI and server tests stopped skipping themselves.** They printed one line
  to stderr and returned when `powdb-server` was not next to the CLI binary, so
  the only coverage the remote client path has could vanish without changing a
  test result. A missing binary is now a hard failure naming the command that
  builds it.

- **A crash after an ordinary insert could leave the data directory
  unopenable.** Two entry points place a row in a heap page and only one of them
  could compact. The runtime insert compacts when a row does not fit the
  untouched tail but does fit the space deleted rows left behind, so a row can be
  placed purely because compaction reclaimed that space, and the identifier it
  chose is what the log records. The replay entry point went straight to the slot
  insert, which refuses on free space alone however much dead space the page
  holds. Replay therefore could not reproduce the identifier, and opening the
  catalog failed. The everyday route in is a relocating update, recorded as a
  delete plus an insert: the delete makes the dead space and the insert then
  needs it. Replay now compacts and retries, which preserves slot indices so
  later records in the same replay stay correctly targeted.

- **The automatic WAL checkpoint was off in both shipped binaries.** A catalog
  opened with an archive hook stopped checkpointing altogether, because the hook
  was borrowed for the open and could not be run later. The server and the CLI
  both always pass a hook, so `--wal-checkpoint-bytes` did nothing in either of
  them and the log grew without bound, which is the thing the threshold exists to
  prevent. The catalog can now own a hook, and the threshold checkpoint publishes
  the log through it before truncating. Nothing truncates without running the
  hook, so a hook passed to the open but never registered is still a refusal to
  truncate. The separate skip when PowDB's own replica sync is enabled is kept,
  because there the sync machinery owns checkpointing and that is a different
  reason to skip.

- **BREAKING (silent, and now refused):** **`length()` on a `bytes` column answered null on every row, with no error.** A
  silent wrong answer. A bytes value has exactly one length, its byte count,
  which is what SQLite, Postgres and MySQL all return for a blob, so it now
  answers with that. An operand with no length at all (`int`, `float`, `bool`,
  `datetime`, `uuid`, `json`) is now a typed refusal naming the column and its
  type. A `json` document is refused rather than answered because it has no one
  obvious length, and the `->` paths already spell each candidate. Both frontends
  share the one code path, so PowQL and SQL agree by construction.

- **A mistyped `having` named the planner's invented column instead of the
  query's.** `having count(.id) = "x"` reported a type mismatch on `__agg_0`, a
  name that appears in no query, no schema and no result set. The refusal was
  right; the name was not. The same internal name escaped into two neighbouring
  messages, and all three now read back the aggregate the query actually wrote.

- **Only one of the two spellings of an unsupported link path explained
  itself.** The aliased form named the limitation, which was this sprint's fix,
  while the identical mistake written without an alias still reported an
  unexpected trailing token. The two lex differently and landed in two different
  parser arms. The wording now lives in one function both arms call.

- **The node addon's own documented first line failed to load.** `import
  { Database } from "@zvndev/powdb-embedded"` raised a named-export error,
  because the loader published its exports through a dynamic assignment that the
  CommonJS module lexer cannot see through. The dynamic assignment stays as the
  source of truth, with explicit re-publication beside it so the lexer sees the
  names, and a test that reads the import line out of the README rather than
  restating it.

- **Requiring the addon's generated binding directly bypassed error
  classification.** The package had no `exports` map, so a deep import reached
  the unwrapped binding and threw errors without the class the type declarations
  promise. The wrapped loader is now the only reachable entry point.

- **Three of the addon's runtime exports were declared in neither type file**, so
  they were absent from every published tarball, and the unsupported-platform
  check compared a platform key against entries carrying a libc suffix, so its
  membership test could never succeed on any Linux target.

- **A corruption refusal at startup names the table and the remedy.** A rotted
  page reported only `page 1 CRC32 mismatch`, and the documented remedy is to
  restore from a backup, which is a per-table decision. With forty tables in a
  directory that message did not say which file to restore, and the database
  would not start until one was. It now reads
  `page corrupt: table 'Orders' (/path/Orders.heap): page 1 CRC32 mismatch: ...;
  restore this table from a backup`. A catalog file that will not parse names
  itself and its remedy the same way. The typed error class is preserved, so
  what reaches the wire is unchanged.

- **A Unix-domain socket path close to the platform limit binds again.** The
  socket is bound under a private staging name and renamed into place, so it is
  never briefly world-connectable. That staging name was built by decorating the
  published one and cost 15 bytes, and `sun_path` is a hard 104 bytes on macOS
  and 108 on Linux, so any socket path within 15 bytes of the limit stopped
  binding: on macOS the per-user temp directory is 49 bytes by itself. The
  staging name no longer grows with the published one, and a bind that still
  fails on length now names both paths and the difference instead of reporting a
  system error about a path the operator never wrote.

- **A conjunction with a hot indexed literal no longer pays for counting it.**
  The index chooser ranks a conjunction's indexed equalities by the exact count
  of each literal, and that count is bounded at half the index. The bound was
  walked in full on every execution when the literal was hot: `filter
  .is_published = true and .data->slug = "s2"` over 20K rows, with both an index
  on the 50/50 boolean and one on the per-row-unique path, walked ~10K index
  entries per query to learn what the path's count of 1 had already decided,
  and the query took twenty times longer than the same query without the
  boolean index. The plan it chose was right all along; only the choosing was
  slow. Counting now starts at a small cap and doubles only until the ranking is
  settled, which picks the same driver the full count picks in every case (a
  saturated tie goes another round; an all-hot tie still falls to conjunct
  order), and a unique equality or a lone one is never counted at all. The
  regression dated from 0.19.1 and went unmeasured because the workload sat in
  the benchmark baseline but not in the comparator's gated list until now. On
  the same Depot instance, head against 0.27.0: 65,612 ns to 4,808 ns (-93%).

- **A query no longer rebuilds its validation scaffolding on every execution.**
  Since 0.20.0 every execution, plan-cache hits included, checks the plan
  against the catalog as it is right now: unknown tables and columns, mistyped
  comparisons, mistyped JSON path bases, negative slice counts and stray
  aggregates. Those checks stay where they are, because a column dropped between
  two executions of a cached plan must still be refused on the second one (a
  test now holds that on both the mutable and the read-only path). What changes
  is their cost. Each check used to gather the scan columns for itself, three
  walks plus a fourth for the join ambiguity check, cloning every column name
  into a fresh string and building hash sets of them per query. They now share
  one walk and borrow every name they resolve, the projection names that can
  shadow a JSON path base are collected only when the plan carries a `->` path,
  and the join ambiguity walk is skipped for a single-table plan. On the same
  Depot instance, head against 0.27.0: a point lookup 1,977 ns to 1,439 ns
  (-27%), an update by primary key 2,018 ns to 1,616 ns (-20%).

### Security

- **The auth rate limiter is bounded in key size and entry count.** A failed
  `CONNECT` used to retain the whole username the peer sent as a map key for a
  60-second window, and nothing under the 4 KB pre-auth frame limit bounded that
  name. A key now retains at most 64 bytes of username, the table holds at most
  4096 buckets, and it evicts deterministically at capacity (lowest failure
  count, then oldest window, then key order), so a spray of throwaway source
  addresses cannot clear a peer that is close to its bound. The expiry sweep runs
  at most once a second instead of on every handshake, so a legitimate `CONNECT`
  no longer pays an O(n) scan of a table an attacker inflated.

- **BREAKING (operational):** **A shared-password deployment counts auth failures against the peer, not the
  username.** A server with no user store never reads the username while
  authenticating, so counting failures against it let one address make 50
  password guesses a minute instead of 5. It also made an unauthenticated peer
  the author of the limiter's keys for exactly the deployment shape that has no
  user store. Relatedly, a Unix-socket peer has no `SocketAddr` and was exempt
  from rate limiting entirely; all socket peers now share one bucket.

- **A wrong username no longer locks another user out.** Failures are counted per
  (peer, username) at 5 per 60 seconds and per peer at 50 per 60 seconds, so a
  username sweep from one address is still stopped without letting it lock a real
  user out of their own account from behind a shared NAT.

- **The Unix-domain socket is never connectable at the process umask.** `bind`
  created the node at 0755 or 0775 and the `chmod` to 0660 ran afterwards, and a
  `chmod` on a socket does not close connections already accepted on it. The
  socket is now bound at a private staging name in the same directory
  (`.<name>.staging-<pid>`), restricted there, and renamed into place, so the
  published path only ever names an already-restricted socket and is never
  briefly absent either.

- **BREAKING (now refused):** **A data directory the owner cannot open is refused rather than widened.** A
  0555, 0500 or 0000 data directory was silently `chmod`ded to 0700 and written
  into. The change is now logged with both the old and the new mode, and 0000 is
  refused.

- **`drop link` is classified as schema definition under RBAC**, alongside
  `create link`, so a `readonly` principal is refused with the message the rest
  of DDL gives. No outcome changed for any builtin role.

## [0.27.0] - 2026-08-26

### Added

- **Release candidates.** A `vX.Y.Z-rc.N` tag now ships a full release on
  every channel without moving what a default install resolves to: GitHub
  pre-release, ghcr `:rc` floating tag (`:latest` untouched), npm dist-tag
  `next` for the client and sync packages (and `publish-node-addon.yml`,
  dispatched with the rc version, publishes the addon under `next` too),
  and a crates.io pre-release version cargo will not pick up unless pinned.
  See RELEASES.md.
- **`powdb-server --port 0` reports the bound port.** A new `--port-file
  <path>` flag (env `POWDB_PORT_FILE`) writes `port=N` (plus `metrics=N` when
  the metrics endpoint is on) after startup, and the "powdb server
  listening" log line now reports the address actually bound rather than
  the one requested.
- **`docs/metrics.md`**: every metric family the Prometheus endpoint
  exposes, held equal to the code by CI.
- **`powdb-sync` refusals carry a typed `SyncError`.** Every error the crate
  raises still arrives as an `io::Error` with the same message text and the
  same `io::ErrorKind` as before, but its source is now a
  `powdb_sync::SyncError`: `IdentityMismatch`, `ApplyInProgress`,
  `ApplyStateRequiresRepair`, `UntrustedApplyBoundary`, plus the two
  catch-alls `InvalidRequest` (`ErrorKind::InvalidInput`) and `CorruptState`
  (`ErrorKind::InvalidData`). A replica host deciding between resume,
  repair, and rebootstrap can branch on
  `err.get_ref().and_then(|e| e.downcast_ref::<SyncError>())` instead of
  matching rendered text. Public signatures are unchanged.
- **A yank/rollback runbook in RELEASES.md** covering every channel: the
  full-set crates.io yank, npm deprecation, repointing the ghcr `latest`
  tag, demoting the GitHub Release, and the rule for yank versus
  fix-forward.
- **Publishing is gated by `cargo-semver-checks`.** `publish.yml` now runs
  it (v0.50.0) against the crates.io baselines before any crate publishes,
  so a point release cannot carry an API break; a `dry_run=true` dispatch
  doubles as a release preflight. It sits at the publish door rather than
  in the merge gate on purpose: between releases `main` legitimately
  carries breaking changes against the last published version.

### Changed

- **Parse errors now say where.** PowQL and SQL parser failures
  (`expected X, got Y` and syntax refusals) lead with the same
  `at position N:` prefix the lexer's diagnostics always had, where `N` is
  the char offset of the token the parser stopped on. Error text that
  previously had no location is otherwise unchanged, and errors raised from
  synthesized token streams (no source text) stay position-free.

- **A json column now compares against a string literal as a document.** The
  literal is parsed and canonicalized exactly as on insert, so
  `filter .j = "{ \"b\": 2, \"a\": 1 }"` matches `{"a":1,"b":2}`
  regardless of key order or whitespace. Before, `Value` equality's strict
  typing made every such filter silently return nothing. A literal that is
  not valid JSON is now a typed error before any row is read, and ordered
  comparisons (`<`, `>`, ...) between a json column and text are refused
  rather than silently false. The corresponding oracle ledger entry
  (`json-column-never-equals-a-string-literal`) is retired: the differential
  oracle now holds PowDB to SQLite's answer on canonical-text equality.

- **`sum` over zero non-null values now returns null, matching SQL and
  PowDB's own `avg`.** Before, "no rows" and "a total of zero" were the same
  answer: the generic and compiled-int paths said `0`, the compiled float
  path said `0.0`, and every one of them disagreed with `avg` (already null)
  and with SQL's `SUM`. This applies to PowQL `sum(...)`, SQL `SUM(...)`,
  grouped and windowed sums, and sums over outer-join groups whose inputs are
  entirely null-extended. `count` still answers `0` for no rows. If you
  relied on the old default, wrap the aggregate:
  `coalesce(sum(x), 0)` in SQL. The corresponding entry has left the oracle
  divergence ledger: the differential oracle now holds PowDB to SQLite's
  answer here.

- **A failed WAL fsync now poisons the WAL instead of being retried.** Once
  `fsync` fails, the OS may already have dropped the dirty pages and marked
  them clean, so a retried `fsync` reports success over bytes that never
  reached stable storage (the "fsyncgate" hazard), and both durability
  paths used to retry. A failed fsync now sets a sticky poisoned flag on
  the WAL. From then on every commit that needs durability fails fast with
  `WAL poisoned by an earlier fsync failure; commits can no longer be made
  durable (the OS may have dropped the unsynced pages). Restart the process
  to recover from the on-disk log`, the background flusher stops, the WAL
  takes no new bytes in `full` or `normal` mode, and
  `powdb_wal_fsync_failures_total` increments (alert on it; see
  `docs/metrics.md`). Commits an earlier fsync already covered still report
  success truthfully. Recovery is a process restart, which replays the log
  that actually reached disk; the engine poisons rather than aborts because
  it also runs embedded, where killing the host is not its decision.
  `Wal::is_sync_poisoned()` exposes the state.

- **`From<StorageError> for io::Error` keeps the typed error as the source.**
  It used to render the refusal to text, so any storage error that crossed a
  plain `?` or `.into()` lost its variant and the server fell back to
  substring-matching the message to pick a wire error class. The conversion
  now wraps the `StorageError` itself (`io::Error::other(err)`): the rendered
  text is byte-identical, and `StorageError::kind_of_io_error(&err)` recovers
  the `StorageErrorKind` on the far side of any `io::Result` boundary. With
  no producer left that can strip a kind, the substring fallback is deleted
  (see Removed), and the binary-level `wire_error_class_from_type` suite pins
  every class end to end.

- Internal: `crates/storage/src/catalog.rs` is now the
  `crates/storage/src/catalog/` module directory and the CLI's `main.rs` is
  split into `args`, `admin`, `embedded`, `remote`, `repl`, and `output`
  modules, both pure moves with every public path preserved
  (`read_active_catalog_version` through a `pub use`); the executor's
  unit tests are sharded into 14 themed files; and process tests spawn
  `powdb-server --port 0` with `--port-file` instead of racing for a free
  port.
- Internal: new CI gates. `testing-feature-guard` refuses `powdb-query`'s
  test-only `testing` feature in any shipped artifact's normal and build
  feature graph (eight crates plus the Node addon); `missing-docs-ratchet`
  holds each published library crate's count of undocumented public items
  equal to `scripts/ci/missing-docs-baseline.txt`, so public-API docs only
  ever tighten (`powdb` and `powdb-auth` are at zero and now carry
  `#![warn(missing_docs)]`); `miri-query` runs the compiled predicate module
  (`executor::compiled`, byte-offset filter evaluation) under miri; and two
  fuzz targets, `fuzz_sync_segment` (`RetainedSegment::from_bytes`) and
  `fuzz_btree_open` (`BTree::load` plus the read surface), join `fuzz.yml`
  and the corpus-replay gate; and a `rustdoc` job builds the public docs
  with `RUSTDOCFLAGS=-D warnings` (default features only), which nothing
  did before, so the six intra-doc links in the public API docs that
  pointed at private or nonexistent items and rendered as plain text on
  docs.rs are fixed.

### Fixed

- **Scans fail closed on unreadable or unverifiable pages.** `HeapFile::scan`
  mapped a failed page read or an unparseable page to zero rows from that
  page, and the closure scans' `pread` fallback skipped read errors, so bit
  rot or an I/O error hit mid-scan produced a silently shorter result from
  a full-table query (and from the index-rebuild, vacuum, and DDL paths
  built on the same scans), while the same page hit through a point read
  errored. Scans now hold exactly the point-read standard
  (`Page::from_bytes_verified`: CRC when the page is stamped, format
  version always) and every consumer propagates. Engine users get a
  `QueryError::Storage` whose kind is `PageCorrupt` (for example `page
  corrupt: page 2 CRC32 mismatch: stored 0x..., computed 0x...`), direct
  `powdb-storage` users a `StorageError::PageCorrupt`; over the wire it is
  error class 0 (`internal`), a server-side fault, never a partial answer.
  The mmap fast paths are unchanged and now document their contract. For
  direct users of `powdb-storage` the signatures moved with it:
  `HeapFile::scan`, `Table::scan`, and `Catalog::scan` yield
  `io::Result` items; `for_each_row`, `try_for_each_row`,
  `for_each_row_raw`, and `try_for_each_row_raw` return `io::Result<()>`;
  `HeapFile::has_rows` returns `io::Result<bool>`; and
  `Catalog::assign_auto_columns` returns `io::Result<()>`. New suite:
  `crates/storage/tests/scan_corruption.rs`.

- **`LIKE '%'` now matches text that contains a literal `%`.** The matcher
  tested the literal branch before the `%` branch, so a pattern `%` landing
  on a `%` in the text was consumed as an ordinary character with no
  backtrack point: `LIKE '%'` failed to match `"%a"` and `"%%"`, and
  `LIKE '%_'` failed to match `"%"`. The corresponding oracle ledger entry
  (`like-pattern-percent-consumed-as-a-literal`) is retired; the
  differential oracle now holds PowDB to SQLite's answer here.

- **SQL-subset diagnostics now reach remote clients verbatim.** Every
  documented "unsupported SQL" wall in `docs/SQL.md` (CASE, COALESCE,
  COUNT(DISTINCT), CAST, OVER, IN, EXISTS, scalar subqueries, BETWEEN, table
  constraints, `RETURNING <columns>`) is a static message naming the
  construct and the working alternative, but none had a prefix in the
  server's safe-to-forward list, so an embedded caller saw the real message
  while a remote client saw `query execution error`. The `sql ` and
  `returning ` prefixes are now forwarded, with error class 1 (`parse`), and
  an enumerate-by-executing test holds every documented wall to the wire.
  `docs/SQL.md` no longer tells you to prototype embedded to read the
  reason.

- **A plan-cache hash collision can no longer execute the wrong plan.** The
  cache was keyed by a bare 64-bit FNV-1a hash of the canonical query, which
  is not collision-resistant, so two different query shapes with the same
  hash served each other's cached plan: a silent wrong answer. The cache now
  stores the canonical byte stream beside each plan and re-compares it on
  every hit; a mismatch is a counted miss (the query is replanned from
  source) and increments the plan cache's public `hash_collisions` counter
  (`PlanCache::hash_collisions`; it is not on `/metrics`). Cost on the point
  lookup microbenchmark: `powql_point` 1.68 us to 1.76 us (+4.5%), inside
  the regression gate.

- **A crashed replica can resume.** A SIGKILL between writing the
  `InProgress` apply intent and marking it complete left an embedded replica
  permanently wedged: the whole-tail resume refused with `another
  retained-tail apply is in progress for this replica`, the exact-chunk
  retry with `retained chunk start LSN N is not a trusted completed apply
  boundary`, and no public API could clear it. Found by a new process-level
  kill -9 test (`crates/backup/tests/replica_kill9.rs`). A resume that
  starts exactly at the catalog LSN WAL replay recovered, when that LSN
  lies inside the stranded intent's `[applied, through]` window, now voids
  the intent and proceeds; anything else stays fail-closed. Affects
  `powdb_sync::apply_retained_tail` and
  `apply_retained_units_chunk`, and therefore the retained-unit apply the
  `powdb` crate and `@zvndev/powdb-embedded` expose (`applyRetainedUnits`).

- **`link` statements are classified as schema definition under RBAC.** The
  entity-link DDL was missing from the server's schema-definition check, so
  a `readonly` principal refused a `link` statement was told `permission
  denied: role 'readonly' cannot execute write statements` rather than
  `... schema-definition statements`. No outcome changed for any builtin
  role: `admin` and `readwrite` hold both Write and Ddl and `readonly` holds
  neither, so only the message moved. The full role-by-statement matrix
  (every builtin role, an unknown role, and the legacy no-principal path
  against one sample of every statement kind) is now pinned by a test whose
  exhaustive match makes a new statement kind a compile error until it is
  classified.

- **The Docker image's dependency-cache stage was silently dead.** The
  manifest copy list never gained `crates/sync/Cargo.toml` when
  `powdb-server` took its `powdb-sync` dependency (v0.8.0), so workspace
  resolution failed in the stub build and a `2>/dev/null || true` swallowed
  the error: the cache layer warmed nothing and every image build recompiled
  all dependencies. The sync manifest is stubbed with the others, the error
  swallowing is gone (a future miss fails the build loudly), and the real
  build now runs `--locked` like CI and the release workflows.

- **`@zvndev/powdb-sync` accepts catalog v7 servers.** Its
  `SUPPORTED_CATALOG_VERSION` was still 6 after the engine moved to v7
  (persisted entity links, 0.19.0), so a primary whose database had
  activated v7 refused a replica stating this ceiling, and
  `assertServerCatalogVersionSupported` rejected such a primary with
  `server catalog format v7 is newer than this client supports (max v6)`.
  Raised to 7, matching `@zvndev/powdb-client`; `test/sync.test.ts` now
  reads `CATALOG_VERSION` out of `crates/storage/src/catalog/mod.rs` and
  fails when the two disagree. The package treats catalog payloads as
  opaque bytes, so nothing else changes.

- **Documentation.**
  - `json_text` was shipped but documented nowhere; it is now next to
    `json_type` in `docs/POWQL.md`, the "complete keyword list" regained
    `json_text`, `json_type`, and `raw`, and a new test
    (`crates/query/tests/powql_doc_sync.rs`) holds that list equal to the
    lexer's `POWQL_KEYWORDS`.
  - The C toolchain and `cmake` prerequisite for `cargo install powdb-cli`
    / `powdb-server` is stated in `docs/getting-started.md`, the site, and
    `CONTRIBUTING.md`.
  - The README gained the embedded-Rust front door (`cargo add powdb`, a
    runnable snippet, the docs.rs link) and no longer claims the embedded
    npm addon "builds from source" on other platforms (it ships no source
    and `require()` throws; use `@zvndev/powdb-client` there).
  - `site/powql.html` gained the Nested Projections and Entity Links
    sections, and `docs/POWQL.md`'s table of contents the Comments and
    Entity Links sections it lacked.

### Removed

- **`StorageError::is_ddl_in_transaction_message`,
  `is_transaction_too_large_message`, and `is_unique_violation_message`
  (`powdb-storage`).** They were the legacy fallback for classifying a
  storage refusal after it had been rendered to text. The last path that
  did that rendering was `From<StorageError> for io::Error`, which now
  carries the typed error as the `io::Error`'s source (see Changed), so
  nothing is left for a substring predicate to classify. Migration: recover
  the variant with `StorageError::kind_of_io_error(&err)`, which returns
  `Option<StorageErrorKind>`, or downcast the source and match it:
  `err.get_ref().and_then(|e| e.downcast_ref::<StorageError>())`. Message
  text is unchanged. This, together with the scan signatures under Fixed,
  is a breaking change to a published crate's API, which is why the next
  release is 0.27.0 rather than 0.26.1.

### Security

- **An unauthenticated peer can no longer hold a connection slot forever.**
  The wait for CONNECT ran under the general idle timeout, and every
  pre-auth Ping reset it, so a client that pinged and never authenticated
  squatted on a slot indefinitely, while the TLS handshake has had a 10 s
  ceiling since 0.20.0. The whole pre-auth phase of every connection, pings
  included, now runs under one 10 s deadline (`DEFAULT_PREAUTH_DEADLINE`;
  not configurable on the binary, library embedders set
  `ConnOpts::preauth_deadline`). Pings are answered inside the window but
  never extend it; expiry closes the connection and logs `pre-auth deadline
  expired waiting for CONNECT`. Load-balancer health checks (connect, ping,
  close) fit comfortably; a checker that parks one pre-auth connection and
  pings it forever must reconnect per check.

- **Username enumeration by timing is closed.** `UserStore::authenticate`
  returned in nanoseconds for an unknown user and after one argon2id
  verification (~100 ms) for a known user with a wrong password, a remote
  timing oracle that enumerated usernames (measured at 84 ns against
  199 ms). An unknown user now costs one argon2id verify against a
  process-wide dummy hash, so both refusals do the same work.

## [0.26.0] - 2026-08-23

**The audit round: the findings from the 2026-08-22 gold-standard audit.**

### Fixed

- **A duplicate key on a unique expression index reported the wrong error class.**
  It arrived as wire class `0` (`internal`), telling drivers the server had
  faulted, when the caller had simply inserted a duplicate. The cause was
  structural: storage errors crossed the crate boundary as plain strings, so the
  server recovered their type by searching the rendered message for a
  column-level phrase that an expression-index violation never contains. Wire
  classification now runs off a typed `StorageErrorKind` through an exhaustive
  match with no wildcard, so a new storage error variant is a compile error
  rather than a silent `internal`. The message text is unchanged, and no other
  class moved.

- **The PowQL "did you mean" suggestion was computed from the wrong token.**
  It read the first token of the statement instead of the token that actually
  failed, so every query beginning with `User` suggested `` `upsert` `` no matter
  what went wrong, including `User xyzzy`, which resembles no keyword at all.
  Since `User` is the table name in nearly every example in the documentation,
  a newcomer's first typo produced a confidently wrong suggestion. Suggestions
  now come from the offending token and cover pipeline keywords (`filter`,
  `order`, `limit`, `group`, `desc` and siblings), not just statement keywords,
  and a token resembling nothing now suggests nothing.

- **Nine `.expect()` calls on the wire decode path are now typed errors.**
  Each was an invariant assertion guarded by an upstream length check, but under
  the crate's deliberate `panic = "abort"` profile any one of them firing would
  abort the process and every connected client. They now return protocol errors.

### Added

- `powdb_storage::error::StorageErrorKind`, with `StorageError::kind()` and
  `kind_of_io_error()`, and `powdb_query::result::QueryError::Storage { kind,
  message }` with `QueryError::from_storage_io()` and `impl From<StorageError>
  for QueryError`. `QueryError` keeps `Clone` and `PartialEq`. Note these are
  new enum variants on types that are not `#[non_exhaustive]`, so an external
  crate matching `StorageError` or `QueryError` exhaustively will need a new
  arm.

- **`@zvndev/powdb-embedded`: every error now carries a stable, machine-readable
  `code`.** `poisoned` and `open_panicked` are distinguishable from an ordinary
  `query_failed`, so an embedded host can recycle the handle or restore the data
  directory instead of retrying. The full set is `query_failed`, `closed`,
  `open_failed`, `open_panicked`, `poisoned`, `invalid_argument`, `sync_failed`,
  `already_open`, and `internal`. `query_failed` and `closed` are the same
  strings `@zvndev/powdb-client` uses, so one `switch` reads correctly against
  an embedded database or a server. `PowDBErrorCode` and `PowDBError` are
  exported from the addon's TypeScript declarations.

### Changed

- **Breaking for anyone reading `err.code` from `@zvndev/powdb-embedded`.** The
  property previously held napi's `"GenericFailure"` on every error the addon
  raised; it now holds one of the nine codes listed above. Error messages are
  unchanged byte for byte, so code matching on message text is unaffected.
  Errors from argument coercion (passing a number where a string is required)
  still report napi's own status strings and are deliberately not part of the
  documented union.

- **A duplicate key on a unique expression index now reaches the client with its
  own message.** The prefix `unique expression index violation` was added to the
  wire egress allowlist, which already carried its column-level twin. Without
  it, the corrected class 8 arrived over the generic `query execution error`
  text, telling a caller a constraint had rejected the write while naming no
  constraint.

- **Cross-crate plumbing modules are now `#[doc(hidden)]`.** In `powdb-storage`:
  `btree`, `disk`, `format`, `heap`, `page`, `row`, `wal`. In `powdb-query`:
  `canonicalize`, `plan`, `plan_cache`, `token`. These were public only so a
  sibling crate could reach them, which placed the on-disk row encoding inside
  the published semver contract. They remain accessible, so nothing breaks, but
  they no longer appear on docs.rs and carry no compatibility promise. See the
  new "Public API boundary" section in `docs/STABILITY.md`. The documented
  surface drops from 117 items to 43 in `powdb-storage` and 94 to 79 in
  `powdb-query`.

- **GitHub Release bodies now carry the curated changelog.** Releases previously
  published only auto-generated pull-request titles, so the release notes for a
  version that fixed a data-directory-bricking defect were a single link. The
  changelog section for the tagged version is now the release body, with the
  generated pull-request list appended beneath it.

### Documentation

- `AGENTS.md` listed entity links as unimplemented and omitted nested
  projections, both of which shipped, and described the workspace as ten crates
  and 100K lines when it is eleven and 145K.
- `README.md` and `docs/POWQL.md` taught an entity-link block spelling
  (`u.orders { ... }`) that does not parse. The working form labels the field.
- Stale `0.23.0` version strings in `README.md`, `docs/getting-started.md`, and
  `docs/powdb-vs-sqlite.md`, including a REPL banner transcript that no longer
  matched the binary.
- `CLAUDE.md` claimed `powdb-bench` depends only on storage and query, which was
  the stated justification for the benchmark workflow not gating merges. It also
  depends on `powdb-server` and `powdb-auth`.
- The production checklist in `README.md` never mentioned running under a
  process supervisor with auto-restart, despite `Cargo.toml` citing that exact
  checklist item as required.

### Internal

- The cross-version on-disk compatibility matrix tested v0.19.1 through v0.21.0
  only, leaving three releases untested, two of which shipped on-disk fixes. It
  is now derived from the published release list rather than a hardcoded literal.
- `crates/query/fuzz/Cargo.lock` was four minors stale and failed `--locked`,
  while a CI cache key hashed that same ignored file and therefore never
  invalidated.
- The `miri` job was an 18-minute single critical path; it is now sharded three
  ways, with a guard asserting the shards cover the canonical filter set exactly.
- `scripts/check-version-consistency.sh` now also gates documentation install
  pins, CLI banner transcripts, every tracked `Cargo.lock`, `bindings/node/Cargo.toml`,
  and the publish workflow's crate list, each with an anti-vacuity assertion.

## [0.25.0] - 2026-08-16

### Fixed

**The follow-up round: the seven defects the v0.24.0 audit found and deferred.
One of them destroyed a database.**

- **`alter <T> drop <column>` could permanently brick the data directory.**
  Dropping a column that carried a `unique` index, or dropping the last column
  a table had, aborted the process with `index out of bounds` and left a
  directory no later process could open. The rewrite that follows a drop
  rebuilds every secondary index from the rewritten heap, and it used the
  `col_idx` each index cached *before* the drop. That number is wrong in two
  ways at once afterwards: the dropped column's own index entry names a column
  that is gone, and every index sitting after it has shifted one slot left. The
  crate is built `panic = "abort"`, so this was a SIGABRT rather than a
  recoverable error, and it fired *before* the catalog was persisted while the
  `DdlDropColumn` WAL record was already durable — so every subsequent open
  replayed the record and aborted in the same place. A supervised server
  restart-looped forever on data that was otherwise intact. Index positions are
  now resolved from the post-drop schema by name, which is what the
  expression-index arm of the same function already did, and the dropped
  column's `.idx` file is removed instead of being orphaned under the exact
  name a re-added column would claim.
- **A materialized view built over another view was stale forever.** A view is a
  legal source for another view, so `materialize V2 as V1` records `V1` as its
  dependency, but dirty propagation only walked one level. Mutating the base
  table marked `V1` dirty and left `V2` clean over `V1`'s pre-mutation rows, and
  nothing ever revisits a clean view, so `V2` stayed wrong permanently, in both
  directions, with no error. Propagation is now transitive; only a clean-to-dirty
  transition is enqueued, so an already-dirty layer stops the walk and the
  steady-state write cost is unchanged.
- **Dropping a table left its views serving rows from a table that no longer
  existed.** `drop <T>` never touched the view registry, so a materialized view
  over the dropped table kept answering from its orphaned copy — while `refresh`
  on that same view already failed with "table not found". The read and the
  refresh disagreed, and the read was the one that lied. Dropping a table now
  marks its dependent views (including views over those views) dirty, so a read
  takes the refresh path and reports the missing source instead of serving it.
  The `drop` message also names the views it just broke rather than leaving them
  to be discovered by hitting the error later.

### Changed

- **`powdb-server` and `powdb-cli` no longer depend on `rustls-pemfile`.** The
  rustls project marked it unmaintained (RUSTSEC-2025-0134) after folding the
  API into `rustls-pki-types`, which was already in the tree. TLS certificate
  and key parsing now uses `PemObject` there. Same maintainers, same parser, no
  behaviour change; the "no private key found in TLS key file" message is
  preserved explicitly, because the new API reports that case as an error where
  the old one returned `Ok(None)`.
- **The sync cursor lock waits longer and backs off.** `upsert_replica_cursor`
  and friends gave up after 5 seconds, which a holder doing two fsyncs under
  ordinary contention on a busy machine could exhaust — the caller then got a
  `WouldBlock` error for a lock that was in use rather than stuck. The timeout is
  now 30 seconds, equal to the staleness window, because giving up sooner means
  failing before the reclaim path that detects an abandoned lock can even run.
  Retries use jittered exponential backoff instead of a fixed 5 ms, so waiters
  stop waking in lockstep and re-colliding on every round.
- **The embedded Node addon's `QueryResultJs` is a discriminated union.**
  `query`, `querySql` and `queryReadonly` were typed as a flat interface with
  `kind: string` and every field optional, so `r.rows` stayed `string[][] |
  undefined` even after checking `r.kind === "rows"`, and reading `r.rows` off a
  scalar result type-checked fine. It is now the same four-variant union
  `@zvndev/powdb-client` exports, matching the addon's own `NativeQueryResult`.

  **The runtime shape is unchanged** — it is still the same flat object, now
  exported as `QueryResultShape` — but this is a **breaking change for
  TypeScript source**: reading `r.rows` without first narrowing on `r.kind` no
  longer compiles, because `rows` is not a member of every variant. That is the
  point of the change; the code it now rejects was reading a field the result
  did not have. Add the `kind` check, and the non-null assertions the old type
  forced (`r.rows!`) can come out:

  ```ts
  // before: r.rows is string[][] | undefined, even here
  if (r.kind === "rows") console.log(r.rows!.length)
  // after: r.rows is string[][]
  if (r.kind === "rows") console.log(r.rows.length)
  ```

  JavaScript consumers are unaffected.

### Documentation

- **`docs/backup-and-restore.md` no longer documents a restore that cannot be
  performed.** The coarse-PITR section showed `restore ... --apply inc-1 --apply
  inc-2` as the recipe for recovering to a point in time. Every increment
  `powdb-cli backup --base` produces is diffed against the same full base, so no
  two of them can ever be chained: applying `inc-1` moves the restored database
  to `inc-1`'s LSN while `inc-2` still records the base LSN, and the continuity
  check correctly rejects it. The doc now shows the single-increment restore
  that actually reaches each point in time, quotes the error the impossible form
  produces, and explains why it is the check working rather than a bug.

## [0.24.0] - 2026-08-15

### Changed

- **Read-only snapshot serving now refuses a stale materialized view instead of
  serving it.** This follows from persisting the view dirty flag (below): a
  read-only open cannot refresh, so a snapshot taken while a view was dirty now
  fails every query that touches that view, with a message naming the fix
  (`refresh materialized views before snapshotting a read-only directory`).
  Previously the flag did not survive the snapshot, so the same directory
  answered with stale rows and looked healthy. Operators upgrading may see new
  errors on snapshots that appeared to work before; the remedy is documented in
  `docs/read-only-serving.md`. Refresh before snapshotting.

### Fixed

**Two of these are silent wrong answers: one survives a restart and never
repairs itself, the other writes NULL into a column the schema declares as a
unique key. The rest are first-hour papercuts found by auditing the project the
way a new user meets it: install, follow the README, run the CLI.**

- **A materialized view served pre-mutation rows forever after a restart.**
  Mutate a base table that has a view over it, then let the process exit before
  anything reads that view, and the view came back marked CLEAN over its
  pre-mutation contents and served them from then on. It was wrong in both
  directions: a row deleted from the base table kept being returned, and a row
  inserted into it was never returned. The dirty marker lived in memory only,
  and it is the only record that a refresh is owed, so nothing after the
  restart could detect the staleness or repair it; `refresh` had to be issued
  by hand, by someone who already suspected the answer was wrong. A graceful
  shutdown reached this just as reliably as a crash, which is why normal
  operation could produce it. `mark_dirty` now writes the flag through to
  `views.bin` on the `false` to `true` transition, so the cost is at most one
  small write per refresh cycle rather than one per write statement, and a
  failure to record it is returned to the caller instead of being swallowed as
  success.
- **`upsert` wrote NULL into `unique auto` columns.** The insert branch of
  `upsert` skipped auto-assignment, so a table declared
  `{ unique auto id: int, unique k: str }` came back as
  `[[1,"a"],[null,"b"],[null,"c"]]` after one `insert` and two `upsert`s: only
  the plain `insert` got an id. Auto columns are now assigned on the insert
  branch of `upsert` exactly as on `insert` (the same sequence, so the example
  above now yields `[[1,"a"],[2,"b"],[3,"c"]]`), while an `upsert` that matches
  an existing row updates it and leaves that row's id alone.
- **`--format json` returned different types embedded than it did remote.** The
  legacy `Query` frame stringifies every cell server-side, so an integer came
  back as `"1"` over the wire and `1` in embedded mode: a script written against
  an embedded run silently stopped matching the moment it was pointed at a
  server. The CLI now requests the typed wire frames, which carry storage
  values, and renders them the same way in both modes.
- **`--exec` and `--exec-file` exited 1 on input that merely ended with a
  comment.** A segment holding only comments and whitespace was still handed to
  the parser, which answered "expected statement, got end of input" *after*
  every write in the file had already committed. A dump that happened to end
  with a comment line aborted a `set -e` script that had in fact succeeded.
  Comment-only segments now never reach the engine in either mode. (0.23.0
  announced this for the interactive REPL only, which is why the non-interactive
  paths kept the bug.)
- **A mistyped subcommand created a stray database.** An unrecognized argument
  was taken as a data directory, so a typo provisioned a new directory and
  reported success while the operator's real database went untouched. Unknown
  subcommands and unexpected arguments now exit 2 with a "did you mean" hint.
- **`useradd`, `passwd`, and `userdel` failed on a data directory that did not
  exist yet.** They wrote the user store without first creating the directory
  and surfaced a raw errno, which contradicted the documented workflow of
  provisioning the first admin *before* the server's first start. All three now
  create the directory with the same `0700` mode the engine uses, so `auth.json`
  (itself `0600`) never lands in a world-readable directory.
- **The SQL frontend mis-parsed constructs it does not support instead of
  refusing them.** `CASE`/`WHEN` lowered to a bare `.CASE` column reference,
  `COALESCE`, `CAST`, and `COUNT(DISTINCT ...)` lowered to nonsense PowQL calls,
  and a window function's `OVER` surfaced as "expected from, got OVER" at the
  clause boundary. Each now returns a terminal unsupported-feature error naming
  the construct and pointing at the PowQL spelling that does work.

### Changed

- The differential oracle adjudicates **mutations**, not just reads, by
  comparing resulting table state against SQLite. It ships with a self-test
  proving the adjudicator can fail, after the first version turned out to be
  vacuous.
- The dual-path equivalence tests (roughly 5,900 lines that had never run in
  CI) are now enabled in CI, and the compiling `cargo` invocations that gate
  merges pass `--locked`, which exposed the node addon lockfile as two minors
  stale. (The nightly miri and ASan jobs do not, because `-Zbuild-std` resolves
  its own std dependencies.)
- GitHub Release binaries and the ghcr image are now attested.
- Dependency refresh verified against the upstream registries rather than by
  merging bot branches: 14 Cargo checksums confirmed against the crates.io API,
  `powdb-sync` moved to getrandom 0.4, and the TypeScript client moved to
  TypeScript 7 (which removes `moduleResolution: "node10"`; the published
  artifact shape is unchanged and was verified rather than assumed).

### Documentation

- **Every relative link in `README.md` is now absolute.** All 8 published crates
  pointed `readme` at the workspace `README.md`, so crates.io resolved each
  relative target against the crate's own subdirectory rather than the repo
  root. That is 16 link occurrences across 12 distinct targets, including the
  LICENSE link, 404ing on all 8 crate pages: roughly 128 dead links on our
  primary Rust discovery surface. (`powdb-cli` now renders its own CLI
  reference instead, so the absolute-link fix covers the remaining 7 pages.) Each replacement target was checked for a 200, and every
  `#anchor` was checked against a real heading in the file it points at.
- **Removed a documented `tls` feature that does not exist.** The README told
  readers to disable a default `tls` feature for a fully-Rust build.
  `powdb-server` declares no Cargo features at all, so `--no-default-features`
  was a silent no-op and a C toolchain plus `cmake` was mandatory the whole
  time. The requirement is now stated plainly in the README and in
  `docs/powdb-vs-sqlite.md`.
- **Re-measured the PowDB vs SQLite table.** The published numbers were four
  releases old. `point_lookup_indexed` was understated by roughly 2x, and
  `delete_by_filter` and `insert_batch_1k` have crossed from "roughly tied" to
  marginally slower. Several aggregate workloads measured *better* than
  published. The headline no longer claims 3-7x on scan workloads, which the
  table never supported.
- Corrected the fuzzing count in `docs/powdb-vs-sqlite.md` from 4 targets to the
  9 that actually exist and run.
- Doc examples that used `--` as a comment now use `#`. In PowQL `--` is
  subtraction, so those lines errored, and inside a `begin`/`commit` block the
  error left the transaction open.
- Refreshed stale `0.19.1` version pins and CLI banners to 0.23.0.
- `docs/POWQL.md` no longer documents the `upsert` auto-column bug as if it were
  the design. It said "Auto-assignment applies on `insert` (not on `upsert`)";
  it now describes the fixed behavior.
- `docs/getting-started.md` states plainly that `useradd`, `passwd`, and
  `userdel` work on a fresh install and create the data directory themselves,
  which is what the surrounding "before first start" workflow always assumed.
- Corrected the `cargo bench` runtime estimate in the README, which said ~60s
  for a suite that measures for about four minutes.
- Added Keep a Changelog compare links for every released version, and a
  `[0.2.1]` entry for a release that had been live on crates.io with no
  changelog record.

## [0.23.0] - 2026-08-09

### Fixed

**Two of these are silent data corruption. All four were reproduced before the
fix and verified not to reproduce after.**

- **WAL replay duplicated a relocating UPDATE.** `set_page_lsn` was called on
  the live INSERT paths and throughout replay, but never on the live UPDATE
  path, so the page LSN redo guard could never fire for an Update record and
  every one re-applied on every recovery. That is safe only while UPDATE is
  idempotent, and it is not: a row grown past the free space on its page
  relocates via delete plus insert with a fresh RowId, so replay left a second
  live copy behind. 200 rows, a checkpoint, one growing update and a crash
  recovered **261 rows with one id present twice**. A plain checkpoint is
  enough to reach it. Every pre-existing crash test used a shrinking update,
  which is why this survived.
- **A missing heap file silently reinitialized the database.** Any `NotFound`
  during open was read as "no database here", which renamed a fresh empty
  catalog over the real one and truncated the WAL. Deleting a single table's
  heap file took `catalog.bin` from 92 bytes to 14 and orphaned a second,
  fully intact table. The create fallback is now gated on `catalog.bin` itself
  being absent, and `drop_table` persists the catalog before unlinking the
  heap.
- **`order ... desc limit N` returned the wrong set of tied rows.** The bounded
  top-N heap evicted the smallest `(key, seq)`, but the required order is key
  descending then sequence ascending, so the correct victim is the largest
  sequence among ties. `order .k desc limit 3` could return a different row
  *set* than the first three rows of `order .k desc`, and adding a no-op
  `offset 0` changed the answer.
- **`distinct` was applied after `limit`/`offset`.** The planner built the
  Distinct node outermost, so the limit cut source rows before de-duplication:
  `distinct limit 3` returned 2 rows where `distinct` alone returned 3.
- **A non-literal `limit` silently became unbounded on the fast paths.** It
  degraded to `usize::MAX` while the generic path errored, so adding a
  projection flipped the same query between "error" and "returns the whole
  table", bypassing `MAX_SORT_ROWS` and the per-query row budget.

### Changed

**Behavior changes. Read these before upgrading if you send SQL from an ORM,
or set TLS through environment variables.**

- **In SQL, double quotes now delimit an identifier, not a string.** The SQL
  lexer treated `'` and `"` identically, so `SELECT "name" FROM Author`
  returned the literal text `name` once per row under a header of `?`, and
  `SELECT name FROM "Author"` failed with `expected table name`. Every ORM
  quotes identifiers as a matter of course (Prisma, Django, SQLAlchemy,
  ActiveRecord), so ported SQL was silently wrong in one direction and
  rejected in the other. A quoted identifier is also never a keyword, which is
  the entire reason delimited identifiers exist: `"limit"` is a column named
  limit. PowQL reserves roughly 93 lowercase words, and there was previously
  no way at all to name a column after one of them from SQL. Two cases are now
  refused rather than mangled: an empty quoted identifier, and one containing a
  backtick, which PowQL uses as its own quote character and cannot escape.
  Single quotes are unchanged and still delimit a string.
- **In SQL, `= NULL` and `<> NULL` now match nothing.** PowQL deliberately
  desugars `x = null` to `x is null`, and the SQL frontend inherited that by
  lowering to PowQL text, so ported SQL silently received the `IS NULL` rows:
  the opposite row set from every other engine, on the single most commonly
  written incorrect SQL idiom. PowDB returned 56 rows where SQLite returned 0.
  SQL now lowers a NULL comparison to a constant-false predicate, while PowQL
  keeps its documented convenience. This is the first and only accepted
  divergence between PowQL and PowDB-SQL.
- **`POWDB_TLS_CA` and `POWDB_TLS_SERVER_NAME` now imply TLS in the CLI.**
  They previously set only their own values and never enabled TLS, so
  `POWDB_TLS_CA=/ca.pem powdb-cli -r host:5433` connected in **cleartext**,
  silently, while the operator had every reason to believe the session was
  encrypted. The matching `--tls-ca` and `--tls-server-name` flags have always
  implied `--tls`, and the CLI README documented the environment variables as
  implying it too. A variable that says how to verify a certificate must never
  leave the connection unencrypted.
- **The CLI no longer sends comment-only input to the engine.** A line
  carrying no statement lexes to zero tokens, and the REPL forwarded it
  anyway, which reported `expected statement, got end of input`. It now asks
  the lexer what counts as a comment rather than scanning for `#` itself, so
  the CLI cannot drift from the language. A lex *error* is deliberately still
  forwarded: that is a real statement with a real problem.

### Added

- **Differential oracle shapes covering this round.** The oracle caught none of
  the four defects above, and that was structural rather than bad luck:
  `order_limit_offset` unconditionally emitted `offset 0`, and an Offset node
  between Limit and Sort blocks the executor's top-N fast path, so the shape
  that should have covered the descending heap could never reach it. Adds
  `order_desc_limit` over a low-cardinality key (so boundary ties are common
  rather than incidental), an ascending sibling to pin the branch that was
  already correct, `distinct_limit`, and `cmp_against_null_literal`, which
  failed on its first run and found the `= NULL` divergence above.
- Regression suites for each defect: `wal_relocating_update`,
  `engine_open_create_fallback`, `topn_tie_eviction`,
  `distinct_pipeline_order`, `non_literal_slice_counts`, and
  `sql_quoted_identifiers`.

### Security

- **`SECURITY.md` pointed reporters at an address that cannot receive mail.**
  It named a `@users.noreply.github.com` address; GitHub's noreply addresses
  exist for commit attribution and reject inbound mail, so a researcher
  following the documented process ("do not open a public issue") had no
  working way to reach us, against a promised 48-hour acknowledgment. It now
  points at GitHub private vulnerability reporting, with a detail-free public
  fallback if that form is unavailable to the reporter.

### Documentation

- **The README's own PowQL examples did not parse.** PowQL's only comment
  syntax is `#`, but the docs used SQL's `--` in 83 places, and `-` is the
  subtraction operator, so every comment line was a parse error. Piping the
  README's PowQL section into the CLI produced 20 errors before and 0 after.
  The comment syntax itself appeared nowhere in `README.md`, `POWQL.md`,
  `SQL.md` or `getting-started.md` and is now documented.
- `docs/SQL.md`'s "No SQL mode in powdb-cli" callout was false: `--sql`,
  `.sql <STMT>`, and `.sql`/`.powql` mode switching all exist and are in
  `--help`.

## [0.22.0] - 2026-08-03

### Changed

**Behavior changes that can affect running servers. Read these before upgrading
if you hold long transactions, write a driver, or read `catalog.bin` directly.**

- **Explicit transactions now have a maximum lifetime, and it is default-on.**
  `POWDB_TX_MAX_LIFETIME_MS` defaults to `300000` (five minutes). A `begin`
  still open after that is rolled back, the write-admission gate is released,
  and the connection is closed with a class-3 `timeout` error. If the budget
  expires while a reply is being written and that write cannot finish, the
  server closes the connection without sending the error frame (anything
  written after a partly-sent frame would be read as its payload); a client
  should treat an unexpected close during an open transaction exactly like a
  timeout. Every uncommitted
  write is gone: no partial commit, no resume. The clock starts at `begin` and
  nothing extends it. It applies to legitimate long transactions exactly as it
  applies to abandoned ones, migrations and bulk loads included. Rationale: an
  explicit transaction holds the single write-admission gate, so one connection
  could previously block every other connection indefinitely. Raise the budget
  for long migrations (for example `3600000`), or set it to `0` to disable the
  bound and restore the pre-0.22 behavior. Reaps are counted at
  `powdb_tx_reaped_total`, and each one logs a warning naming the peer. The
  embedded API is unaffected.
- **Integer arithmetic that overflows now returns Empty instead of
  saturating.** `i64::MAX + 1` previously clamped to `i64::MAX`; a query that
  relied on the saturating result now reads a missing value.
- **Int and Float now compare numerically in filters, on every access path.**
  The six comparison operators compare by exact numeric value, with no
  precision loss at any magnitude, so `.score = 3` matches a stored `3.0`
  whether the filter runs compiled, interpreted, or through an index, and
  `=`, `<`, and `>=` agree as one total order. Previously the answer could
  depend on the access path: an indexed filter could match a row the same
  filter missed without the index. `group by`, `distinct`, and join keys are
  unchanged and keep int and float distinct. JSON path comparisons follow the
  same rule: `.data->views = 10.0` now matches an integer node `10`.
- **Link cardinality is re-derived on every read** instead of read from the
  catalog byte: a link is to-one exactly when the target key currently carries
  a unique index. `alter <Target> add unique .<key>` now promotes a link with
  no re-declaration, DDL order no longer changes behavior, and `schema links`,
  `describe`, `explain`, and traversal all report the same derived answer.
- **The to-many refusal message now leads with the schema fix**
  (`alter <Target> add unique .<key>`) and says explicitly when a plain index
  on the target key is what blocks that statement.

### Added

- **Wire protocol version negotiation.** `Connect` and `ConnectOk` carry
  optional trailing hello blocks (protocol version range, catalog format
  ceiling, named features). A mismatch is now a typed `Error` during the
  handshake instead of an unknown tag mid-session.
- **Error class 10 (`ProtocolVersion`)**, sent in place of `ConnectOk` when the
  two sides cannot agree, after which the server closes the connection.
- **Handshake conformance vectors** at
  `crates/server/tests/wire_vectors/handshake.txt`, decoded and re-encoded by
  the Rust server and the TypeScript client on every CI run, and usable by any
  driver.
- **TypeScript client:** `CLIENT_CAPABILITIES`, `PROTOCOL_VERSION_NEGOTIATED`,
  `WIRE_FEATURE`, `Client.protocolVersion`, `Client.hasFeature`,
  `Client.serverHello`, and opt-in `requireProtocolVersion` / `requireFeatures`.
  `SUPPORTED_CATALOG_VERSION` is now derived from `CLIENT_CAPABILITIES`.

### Fixed (correctness)

- **A materialized view over a projection that yields different types per row
  could abort the server or silently corrupt values.** The backing table's
  column types were derived from the first row only, while an expression like
  `.tags ?? 0` is typed per row: a json-then-int column crashed the encoder
  (an abort under the release profile, from one query), and an int/float mix
  went further and stored one type's bits behind the other's column, so `7`
  read back as `3.5e-323`. Column types are now unified across every row
  (null never constrains them), a projection that genuinely mixes types in
  one column is a typed error, and a `refresh` whose fresh rows no longer fit
  the backing schema fails cleanly before the old contents are touched.
  Found by the new `fuzz_execute` target on its first CI run.
- **A prepared `update` filtered on an indexed column could silently lose the
  write.** The prepared fast path probed every index as if it were a unique
  int index, so on float, datetime, str, or bool columns, or under a
  non-unique int index, it reported `Modified(0)` and wrote nothing while the
  same statement as text modified the row. The fast path now applies only
  where its assumption is provably true (a unique int index); everything else
  takes the general path. The wire path was unaffected; embedded prepared
  statements were not.
- **Reading a projection or aggregate of a stale materialized view returned
  pre-write data.** The fast paths scanned the view's backing heap without the
  dirty check, so `V { .col }` could disagree with `V` on the same engine.
  Every read now refreshes the dirty views the plan names before executing.
- **A materialized view whose stored source no longer parses returns a typed
  error** naming the view and the repair (`drop view`, then re-`materialize`)
  instead of silently serving stale or wrong rows, and `union` views are now
  tracked for refresh like every other view.
- **Embedded: `Engine::execute_plan` now lowers the plan it is given.** Raw
  `planner::plan` output executed through the public API could return wrong
  rows for range predicates on unindexed columns; the entry point now answers
  exactly like the same query as text.

### On-disk compatibility

No format change. Catalog v7 is unchanged and databases move in both
directions between 0.19.0-0.21.x and this release. The per-link cardinality
byte is still written at its existing offset but is no longer the source of
truth; nothing resyncs or repairs it. Tools that read `catalog.bin` directly
must derive cardinality from target-key uniqueness. See `docs/FORMAT.md`.

### Compatibility

Both directions, with no forced upgrade order. Protocol version `1` means "sent
no hello block", which is every release through 0.21.0. A 0.21.0 client gets a
byte-identical `ConnectOk`; a new client against a 0.21.0 server has its hello
ignored and treats the server as protocol `1`. A newer client refuses an older
server only when it explicitly asks, via `requireProtocolVersion` /
`requireFeatures`.

### Docs

- `docs/errors.md`: the four budgets that produce a class-3 `timeout` and why a
  driver must not fold them into one retry path; error class 10.
- `docs/integrations/powql-for-drivers.md`: hello byte layouts, the negotiation
  rule, named features, and how to use the conformance vectors.
- `docs/STABILITY.md` and `docs/FORMAT.md`: the cardinality-byte caveat and the
  negotiation guarantee; wire negotiation is struck from "What 1.0 requires".

## [0.21.0] - 2026-07-27

### Fixed (correctness)

- **Unqualified columns in a join returned a row of NULLs.** The 0.20.0
  "silent wrong answers become errors" round did not reach join scope.
  Validation accepted a bare field name inside a join on the strength of a
  runtime suffix match that evaluation did not actually implement, so
  `User join Order on User.id = Order.user_id { .name, .amount }` returned NULL
  for every column while the qualified form returned the right answer. The
  runtime now performs the suffix match, and one resolver serves the
  projection, filter, join-key, and sort paths instead of three that disagreed.
  A bare name that two joined tables both expose is a typed ambiguity error
  telling you to qualify it, never a silent pick.
- **`order` on an unqualified join column claimed the column did not exist,**
  one clause after the same query projected it successfully. The read-write and
  read-only sort paths now use the same resolver as everything else.
- **Window `sum` reported a false overflow** on a partition whose emitted total
  fits in `int64`, because a transient running total was converted per row.
  `sum(.v) over ()` over `[5e18, 5e18, -5e18]` now agrees with the scalar
  `sum`, which is what "which path fired cannot change the answer" requires.
- **Aggregates over non-numeric values silently answered 0.** `sum` over a
  `str`, `datetime`, `bool`, `uuid`, `bytes`, or `json` column returned
  `Int(0)` and `avg` returned NULL. All aggregate paths now raise a typed
  error and agree with each other.
- **Integer aggregate overflow silently clamped to `i64::MAX`** instead of
  reporting overflow.
- **Arithmetic on a non-numeric operand silently produced NULL.** `.ts + 1` on
  a `datetime` column and `.id + "x"` both evaluated to a missing value.
  Datetime arithmetic is deliberately an error rather than microsecond
  arithmetic: `datetime + int` has no single meaning and `date_add` /
  `date_diff` already spell both operations unambiguously. Widening an error
  into support later is backward compatible; the reverse is not.
- **`date_add`, `date_diff`, and `abs` used unchecked arithmetic**, which
  panicked under overflow checks and wrapped to a bogus timestamp in release.

### Security

- **DDL inside an explicit transaction destroyed data while reporting
  success.** `begin; drop T; rollback` reported "transaction rolled back" and
  the table and its rows were permanently gone, across a full restart. DDL is
  now refused inside a transaction with a typed error, at every entry point
  including `drop view`, which previously left the view destroyed, its backing
  table orphaned, and the name permanently unusable. DDL is not transactional
  in PowDB; see `docs/POWQL.md`.
- **An unbounded per-transaction dirty page set could OOM the process.** Under
  `panic = "abort"` that is a client-reachable denial of service. Unflushed
  pages are now charged against a budget (default 256 MiB, configurable via
  `POWDB_DIRTY_PAGE_BUDGET`) and an oversized transaction is refused with a
  typed error instead of growing until the host kills the server.
- **Parse failures were admitted as writers.** A query that failed to parse
  took a writer admission permit, so any authenticated principal, including a
  read-only one, could hold all 1024 permits in a loop and block every real
  client. A statement that executes nothing now acquires nothing. The same
  applies to a parseable write from a principal who is not allowed to write:
  permission is now checked before any permit is taken.

### Changed

- **Errors that were mislabeled now tell the truth.** Numeric overflow and
  division by zero no longer render as "type mismatch". `DdlInTransaction` and
  `TransactionTooLarge` reach drivers as a client-error class and
  `LimitExceeded` respectively, rather than `Internal`, the class that means
  "server bug, not your fault".

### Known limitations

- **An `update` can durably store NULL in a `required` column,** while `insert`
  refuses the same value. PowDB has no statement-level atomicity: an `update`
  writes rows as it walks them, so refusing a per-row value mid-statement
  produces a torn write, which is a worse failure than the one it fixes. The
  refusal was implemented, measured to tear, and deliberately reverted. The
  real fix is statement-level atomicity. Documented with a reproduction in
  `docs/STABILITY.md`.

### Fixed (CI)

- **The miri job had been passing without running.** It filtered on
  `tx::tests`, a module that does not exist in `powdb-storage`, and a cargo
  filter matching nothing exits 0. The filter is corrected and a guard now
  fails the job when a filter matches no tests.
- `docs/FORMAT.md` and `docs/STABILITY.md` had drifted a full minor behind the
  published release, turning a compatibility promise into a guess. Both are
  current and now covered by the version-consistency gate.

## [0.20.0] - 2026-07-25

### Security

- **Remote denial of service via long operator chains (both frontends).** The
  parser bounded recursive descent but not the shape of the tree it produced,
  so a flat chain such as `T filter .a = 1 and .a = 1 ...` repeated a few
  thousand times built an AST deep enough to overflow the stack during the
  recursive planner walk. Under `panic = "abort"` that killed the whole server
  process, dropping every connection, and it was reachable by any client that
  could send a query (including a read-only user). Chain-building loops in both
  the PowQL parser and the SQL frontend now count against the nesting budget.
- **Backups and restored directories are no longer world readable.** Backup and
  restore created directories and files with default permissions (0755 / 0644)
  rather than the 0700 the live engine uses. A backup is a byte-for-byte copy of
  all table data and the catalog, so a backup written on a shared host exposed
  the entire database to other local users. Backups are now 0700 directories and
  0600 files.
- **TLS handshakes are now bounded (10 seconds).** A connection permit was taken
  before the TLS handshake, and the handshake itself had no timeout, so idle
  half-open sockets could hold every connection slot before authenticating.
- **Corrupt pages return errors instead of aborting the process.** A corrupt slot
  count could underflow an index computation and abort the process inside
  `HeapFile::open`, before WAL replay, which meant a supervised server would
  restart into the same abort indefinitely. See the breaking-change note below
  for the new failure mode.

### Fixed (correctness)

- **Comparisons against a `datetime` column returned wrong rows.** A timestamp
  literal is written as a plain integer, so `filter .created_at > 1752000000`
  compared a `DateTime` value against an `Int`. That pairing was not handled, so
  it fell back to comparing type tags: every `DateTime` sorted above every `Int`
  whatever the timestamps were. The effect was a filter that matched every
  non-null row, an equality that matched none, and a reversed comparison
  (`1752000000 < .created_at`) that matched none. Timestamps now compare as
  microseconds on every path.
  - The answer also depended on whether an index existed, because the index path
    accepted an integer literal as a `datetime` index key while the scan path did
    not. Index keys are stored behind a type tag, so an integer probe cannot
    match a stored timestamp: equality found nothing and a range scan matched
    every entry. A predicate on an indexed `datetime` column now runs as a
    compiled sequential scan, which is correct; using the index needs a real
    timestamp literal and will come with the temporal type work.
  - `datetime` columns also now compile into the predicate fast path and the
    top-N sort fast path, so `filter .created_at > <ts>` and
    `order .created_at desc limit N` no longer fall back to full row decoding.

### Changed

**Behavior changes that can affect existing queries. Read these before upgrading
if you depend on exact result sets.**

- **Unknown columns in `filter` and in projections are now errors.** They
  previously evaluated to NULL, so a typo returned a plausible wrong answer
  instead of failing: `T filter .agee > 25` returned an empty set, and
  `count(T filter .agee = null)` matched every row, which meant
  `T filter .agee = null delete` would silently delete an entire table. `group`,
  `order`, and `insert` already rejected unknown columns; `filter` and
  projections now match them. Queries that relied on the old NULL behavior, or
  that generate column names dynamically, will now raise
  `column '<name>' not found`.
- **Type-mismatched comparisons are now errors.** Comparing a `str` column
  against an integer literal previously evaluated true for every row.
- **A negative `limit` is now an error** instead of being ignored and returning
  every row. Separately, `limit 0` in a projection fast path returned one row and
  now correctly returns none.
- **`count(T { .col })` counts non-null values, not rows.** Both frontends were
  wrong here in the same direction: SQL `COUNT(col)` and ungrouped PowQL
  `count(T { .col })` both returned the row count. They now agree with each other
  and with the grouped path. `count(T)` and `COUNT(*)` are unchanged.
- **Operator chains longer than the nesting budget are now rejected.** This is
  the DoS fix above. Machine-generated predicates with very many `and` / `or`
  terms may need to be split.
- **A corrupt page now prevents the table from opening at all.** Previously the
  open scan skipped an unreadable page and the failure surfaced later, on the
  read that touched it. Opening now verifies page checksums and fails closed, so
  a single corrupt page makes the database refuse to open rather than serving
  partial data. There is currently no skip-corrupt-pages or salvage mode: recover
  by restoring from a backup. This trades a partial-data incident for a loud
  total one, which is the safer default under `panic = "abort"`, but it is a real
  availability change.
- **The Docker image now runs as a non-root user (uid 10001)** and pins its base
  image by digest. An existing root-owned bind mount or volume will fail with a
  permission error until it is chowned to `10001:10001` (or the container is run
  with `--user 0:0`). This affects upgrades of existing deployments.
- **The TypeScript client rejects result frames over 2,000,000 cells.** A server
  declaring a large row count could previously force a disproportionate
  allocation in the client (a ~40 MB frame decoded into ~1.9 GB of heap).

### Added

- `/health` and `/healthz` on the metrics listener, for liveness probes.
- Storage metrics: database size, WAL size, and WAL fsync counters and latency.
- `docs/STABILITY.md`: what a minor release may break, data directory
  compatibility, downgrade policy, and what 1.0 requires.
- CLI: SQL is now reachable (`--sql`, and `.sql` / `.powql` in the REPL),
  machine-readable output (`--format table|json|csv` and `.mode`), a `.cancel`
  meta-command, and `crates/cli/README.md`.
- TypeScript client: generics and parameter support on typed queries,
  `queryObjects` / `querySqlObjects` returning object rows on the lossless native
  path, and SQL escaping helpers.

### Fixed

- Query text is no longer written to debug logs with its literals intact; logs
  now carry a literal-free query shape plus a hash.
- Published benchmark numbers were measured against a code path users cannot
  invoke (a raw B-tree probe and a hand-built aggregate plan, compared against
  SQLite paying full statement-preparation cost). The harness now runs everything
  through the normal PowQL path, and all published tables were re-measured. The
  indexed point lookup, previously published as 3.0x faster, is 7.9x slower.
  See `docs/benchmarks/2026-07-24-wide-bench-snapshot.md`.
- Corrected platform support: PowDB does not build on Windows.

## [0.19.1] - 2026-07-24

### Added

- **Link introspection.** `schema links` lists every declared entity link as
  ordinary result rows (`owner`, `name`, `target`, `local_key`, `target_key`,
  `cardinality`), sorted by owner then name; an empty catalog returns zero rows.
  `describe T` (and its `schema T` alias) keeps its existing four columns
  byte-identical and appends link rows: outgoing (`-> Target (to-one,
  local_key -> target_key)`) and incoming (`Owner.name` / `<- Owner ...`).
  Note: `links` is contextual, so only the exact spelling `schema links` is the
  listing; a table literally named `links` is still reachable via
  `describe links`.
- **Entity-links driver contract.** `docs/integrations/powql-for-drivers.md`
  gains a full entity-links section for client/ORM authors: DDL grammar,
  derived-cardinality rules, scalar vs block traversal, required aliasing,
  missing-value semantics, the link-misuse error table, and the plan-cache
  exclusion for link-bearing queries.
- **Per-operator missing-value table.** `docs/POWQL.md` now documents the
  behavior of every filter operator against a missing value in one normative
  table (`=`, `!=`, ranges, `in`/`not in`, `between`, `like`/`not like`,
  `not ( ... )`, `is null`/`is not null`, `??`).

### Fixed

- **`not in` never matches missing values.** `not in (list)` and
  `not in (subquery)` are operator forms and now exclude rows whose tested
  value is missing, consistent with `!=` under the two-valued semantics
  established in 0.18.2. Explicit `not ( ... )` remains the documented plain
  complement. (`not like` remains complement sugar; guard with
  `is not null`, see the new operator table.)
- **Skew-aware driver selection.** The cardinality estimator previously
  modeled a non-unique equality as the uniform average
  (`total_entries / distinct_keys`), so a hot value was treated as
  average-rare: a lone hot equality index-scanned most of the table (~7x
  slower than a scan) and a conjunction drove from the hot column instead of
  the selective one (~5-12x, widening with table size). Non-unique equality
  probes on concrete literals now count the actual literal via a bounded,
  allocation-free capped B-tree walk; a lone equality matching more than half
  the table lowers to a compiled sequential scan; conjunction ranking is
  tier-first, then the exact count. Plan choice only, never results; EXPLAIN
  estimates route through the same function.
- **Parent-selectivity for nested blocks and scalar links.** Nested-block
  assembly (`u.orders { ... }`) only materializes children for surviving
  parents (index-probe vs scan chosen by measured stats), and scalar link
  hops (`o.user.name`) point-probe the target under a unique index instead of
  building maps over the whole target table when the outer query is
  selective. Up to ~200x on selective parents over large child tables.
- **Bare dotted link paths are a clear parse error.** An un-aliased
  `.user.name` projection field previously mis-parsed into two separate
  fields silently (it is token-identical to two comma-less bare fields). It
  is now a hard error directing you to alias the table
  (`Order as o { o.user.name }`) or comma-separate the fields.
- **Aggregates over or inside nested/link projections are rejected.**
  `count(Order { .user.name })` previously returned the parent-row count and
  `sum(Order as o { o.user.name })` returned 0, both silently; aggregates in
  a nested block yielded per-row Empty. All now fail with clear typed errors.
- **EXPLAIN names link paths.** Plans containing link fields print the
  declared hop path (scalar hops and to-many blocks) instead of the opaque
  `(unresolved)` marker.

### Client

- **TypeScript client catalog ceiling raised to v7.**
  `SUPPORTED_CATALOG_VERSION` is now 7, so sync/replica clients are accepted
  by servers whose catalog activated v7 (entity links). The client treats
  catalog payloads as opaque bytes, so no decoding change is involved.

### Docs

- Embedded facade concurrency documented precisely: single-writer is enforced
  at compile time by ownership (`&mut`), not a runtime lock; write-gate FIFO
  fairness is observed behavior, not a contract (only the bounded-wait
  timeout is contractual); stored-column indexes cannot be dropped
  (`drop index` is JSON-path/expression indexes only), fixed in three places.

## [0.19.0] - 2026-07-22

### Added

- **Entity links (PowQL relationship traversal).** Declare a relationship once
  on the schema and traverse it by name, instead of writing the same JOIN
  repeatedly.
  - DDL: `link Order.user -> User on user_id = id` (bare, owner-qualified) and
    `alter Order add link user -> User on user_id = id`. Declaring validates
    that both types and both columns exist and that the name does not collide
    with a column or another link on the owner.
  - Cardinality is **derived**, not declared: a unique target key makes the link
    to-one (traversed as a scalar path `o.user.name`, multi-hop supported);
    otherwise it is to-many (traversed as a block `u.orders { ... }` that
    desugars onto nested projections, with per-parent filter/order/limit).
  - **Correct by default:** a scalar hop through a non-unique key is a hard
    error, never a silent row fan-out. A block through a to-one link and a
    scalar path through a to-many link are clean, pinned errors. A missing or
    NULL key at any hop yields Empty and never drops the parent row; a childless
    to-many parent yields `[]`.
  - Links are read-only naming metadata over existing columns: no new storage,
    no write-time enforcement. The planner stays pure; links resolve at
    execution time. Link-bearing plans are never cached, so no stale-plan or
    collision hazard.
  - This is a native PowQL capability with no SQL-frontend equivalent.

### Changed

- **Catalog on-disk format v6 -> v7** to persist entity-link metadata. The bump
  is **lazily activated**: a database that never declares a link stays at its
  current catalog version and opens unchanged on an older binary. The first
  `link` declaration activates v7 atomically (temp-file + rename, with rollback
  on failure). A pre-v7 catalog reads as a valid v7 catalog with zero links; an
  old binary opening a v7 catalog fails loudly with `unsupported catalog
  version: 7`. Full and incremental backup/restore carry links, and links
  survive a crash via WAL replay. See `docs/FORMAT.md` and `docs/POWQL.md`.

## [0.18.2] - 2026-07-21

### Fixed

- **PowQL (P0):** a qualified `alias.column` (or `Table.column`) reference in a
  single-table query resolved against the join-output column set only, so an
  aliased single-table query such as `Widget as w filter w.id > 2` failed with
  an unknown-qualifier error and any `w.col` projection/filter/update/delete
  target could not be reached. Single-table qualifiers are now rewritten to the
  bare field when the qualifier matches the table's name or alias, and an
  unknown qualifier is a hard error. Joins are unaffected.
- **Query semantics (P0):** a filter comparison against a missing/NULL value
  used a total order in which `Empty` sorted first, so `filter .f != x` and the
  ordering comparisons could wrongly match rows whose `.f` is absent. A missing
  value now never satisfies any filter comparison (`=`, `!=`, `<`, `<=`, `>`,
  `>=`); presence is required for a row to match, matching the documented and
  SQL-frontend two-valued behavior. JOIN `ON` equality still treats two absent
  keys as equal.

## [0.18.1] - 2026-07-21

### Fixed

- **SQL frontend (P0):** single-table statements with qualified `table.column`
  references silently resolved the reference to Empty, producing wrong SELECT
  results, wrong WHERE matches, and UPDATE/DELETE affecting the wrong rows
  (`DELETE FROM t WHERE t.v < 10` could delete every row). Qualifiers are now
  resolved against the statement's table (alias-aware); unknown qualifiers are
  a hard error, matching SQLite.
- **Planner (P0):** an AND of two same-side range bounds on one column dropped
  the second bound (`filter .v > 1 and .v >= 9` behaved as `.v > 1`). Only the
  canonical lower-then-upper conjunct pair merges into an index RangeScan; all
  other spellings keep the full predicate and still regain index execution at
  runtime lowering.
- **Plan cache (P0):** a range conjunction written upper-bound-first cached a
  plan whose bounds were swapped on every warm execution, silently corrupting
  results for that query shape for the life of the process.
- **Backup (data loss):** `powdb-cli backup` and `sweep` against a data dir
  owned by a live server checkpointed and truncated the live WAL, destroying
  acknowledged writes while reporting success. Both commands now take the
  writer lock and refuse when a live owner holds it.
- **Wire protocol:** unique constraint violations now carry error class 8
  (constraint_violation) as documented, instead of class 0 (internal).
- **PowQL parser:** prefix `not` now binds at its documented precedence level,
  so `filter not .v > 0` means `not (.v > 0)` and agrees with the SQL frontend.
- **Nested projections:** integral float values (e.g. `3.0`) now stay JSON
  floats instead of becoming JSON ints, per the documented canonicalization.

## [0.18.0] - 2026-07-20

### Added

- **Nested projections (shaped results), PowQL-only.** A projection field can
  now be a whole correlated child query:
  `User as u { u.name, orders: Order as o filter o.user_id = u.id order o.total desc limit 3 { o.total } }`
  returns one row per parent with the matching children assembled into a
  native JSON array of objects; childless parents get `[]` (never NULL, never
  a dropped row) and there is no join fan-out to regroup client-side. Nested
  blocks take one equi-correlation predicate plus optional `and` conditions
  on child columns, per-parent `order`/`limit`/`offset` (top-N per parent),
  and nest recursively for multi-level shapes. Execution is hash-based,
  O(parent + child); plans are cached and `EXPLAIN` shows the nested
  structure. The SQL frontend deliberately has no equivalent. Documented in
  `docs/POWQL.md` (Nested Projections) with a runnable, CI-smoked demo in
  `examples/nested-results/`.

## [0.17.0] - 2026-07-19

### Added

- **Typed wire error codes.** Error frames now carry a stable one-byte error
  class (parse, execution, timeout, limit exceeded, readonly refused, auth
  failed, rate limited, constraint violation, cancelled, internal) appended
  after the message, fully backward compatible in both directions: old
  clients ignore the byte, and new clients treat classless frames from old
  servers as before. The TS client maps the class onto its existing typed
  error codes (`timeout`, `size_exceeded`, `auth_failed`, ...) instead of
  collapsing everything to `query_failed`, and exposes the raw class as
  `wireErrorClass`. Codes are documented in `docs/errors.md` and pinned by a
  regression test (append-only, never renumbered).
- **CLI TLS.** `powdb-cli` remote mode can now speak TLS: `--tls`,
  `--tls-ca <PATH>` for self-signed roots, and `--tls-server-name <NAME>`
  for connecting by IP, with `POWDB_TLS`/`POWDB_TLS_CA`/`POWDB_TLS_SERVER_NAME`
  env fallbacks. Uses the same rustls stack as the server; a server started
  with `POWDB_REQUIRE_TLS=1` is now reachable with the shipped CLI.
- **Wire-protocol and WAL-replay fuzzing.** Two new fuzz targets: `fuzz_wire`
  drives the frame decoder (including the pre-auth CONNECT cap) with
  arbitrary bytes, and `fuzz_wal_replay` mutates real WAL files inside a
  staged crashed data directory and requires recovery to succeed or fail
  cleanly, never panic. Both run in CI with seeded corpora.
- **SIGKILL durability test.** A process-level test inserts over the wire,
  lands `kill -9` with a statement in flight, restarts on the same data
  directory, and asserts every acknowledged write survived WAL replay.
- **Post-publish registry smoke workflow.** A dispatchable workflow installs
  the released binaries from live crates.io and the npm packages from the
  live registry and reruns the durability smoke against them, replacing the
  manual post-release checklist step.
- **Format version support policy** (`docs/FORMAT.md`): which on-disk
  versions each release reads and writes, and the deprecation rules a legacy
  read path must follow before removal; legacy branches in catalog, B-tree,
  and heap code are annotated accordingly.
- **cargo-deny in CI** (license allowlist, duplicate-version warnings,
  crates.io-only sources) alongside the existing cargo-audit gate, plus an
  opt-in pre-commit hook (`.githooks/`) and a pinned `rustfmt.toml`.

### Changed

- **Executor internals reorganized.** The monolithic `plan_exec.rs` (7.5K
  lines) is now a directory of operator-family modules (dispatch, scan,
  fast paths, mutation, aggregate, join, lowering, validate); pure code
  motion with no behavior or performance change.
- `QueryError` and `ParseError` now derive their error implementations via
  `thiserror` with byte-identical messages, matching the storage and auth
  crates' convention.
- Server integration tests share one canonical wire-helper module instead of
  nine per-file copies.
- CI Rust jobs use `Swatinem/rust-cache`; the shared cargo config no longer
  pins `target-cpu=native` (opt in locally via `CARGO_BUILD_RUSTFLAGS`).

### Fixed

- README env-var table now documents `POWDB_SOCKET`, `POWDB_SYNC_MODE`,
  `POWDB_DB_NAME`, `POWDB_MAX_NESTED_LOOP_PAIRS`, and
  `POWDB_TX_WAIT_TIMEOUT_MS`; stale version references in README and
  AGENTS.md corrected; `docs/FORMAT.md` version table matches the code
  again; `scripts/smoke-package.sh` now validates the `powdb` facade crate.

## [0.16.0] - 2026-07-18

### Fixed

- **Wrong rows from non-unique string indexes on values with embedded NUL
  bytes.** Composite index keys previously terminated string values with a
  bare `0x00`, so `"A"` and `"A\0"` could interleave in key order: indexed
  equality, prefix lookups, and index-driven mutations could return or touch
  rows belonging to a neighboring value, and distinct statistics miscounted.
  String values inside composite keys are now escape-encoded (prefix-free
  and order-preserving). This is an on-disk format change for non-unique
  column indexes (v3): an old index is rebuilt from the heap automatically
  on first open and saved in the new format; a read-only open rebuilds in
  memory on every open until a writable open persists the upgrade. Unique
  and expression indexes are unchanged.

### Added

- **"What PowDB is for" positioning.** The README now leads with the
  engine's fit boundary (single-writer embedded state, local agent memory,
  read-only snapshot serving, per-tenant isolation, CI databases, bulk
  ingest) and says plainly when to use Postgres, Turso, or DuckDB instead.
  Benchmark tables are framed as single-request cost.
- **Concurrency decomposition** (`docs/benchmarks/concurrency-decomposition.md`
  plus `scripts/bench-concurrency.sh`): reproducible c1-vs-c10 measurements
  of read/write amplification through the admission gate, including the
  read-only serving tier.
- **Edge snapshot serving example** (`examples/edge-snapshot-serving/`): a
  runnable backup, restore, N read-only servers, incremental refresh, and
  atomic swap walkthrough with a pass/fail smoke script.
- **Transaction concurrency guidance** in the PowQL reference: explicit
  transactions hold the write-admission gate for their lifetime; keep them
  short and prefer autocommit on read-mostly paths.

## [0.15.0] - 2026-07-16

### Added

- **Per-index statistics.** Every B-tree index now maintains entry and
  distinct-key counts in memory (computed during load, kept exact through the
  write path, healed on reload and rollback). No on-disk format change.
- **Cardinality-aware conjunction index choice.** The conjunction index
  chooser ranks candidate indexes by estimated rows per key
  (entries/distinct) instead of fixed tier order alone, so a selective index
  drives the scan even when an unselective indexed conjunct appears first in
  the filter. On a 20K-row json-equality conjunction with a 50/50 boolean
  index and a selective path index, single-request latency drops about 120x.
- **Explain selectivity annotations.** Chosen index-scan lines in `explain`
  now carry `est_rows=... entries=... distinct=...` tokens showing why the
  index was picked.
- **Driver spec additions** (shipped to main ahead of this release): the
  read-only snapshot-mode error family and the equality/range comparison
  coercion regimes are now documented in
  `docs/integrations/powql-for-drivers.md`.

## [0.14.0] - 2026-07-16

### Added

- **Conjunction index selection with residual recheck.** A filter whose
  predicate is an `and` chain now drives its scan from the most selective
  resolvable indexed conjunct (unique equality, then equality, then range)
  and rechecks the remaining conjuncts on the fetched rows, instead of
  sequentially scanning the table. Applies to SELECT-shaped queries and to
  UPDATE/DELETE row discovery. A residual fast path decodes only the
  columns the residual references, so non-matching candidates never pay a
  full-row decode.
- **`explain` shows the executed plan.** Explain output now reflects
  runtime lowering, so a conjunction over an available index renders as the
  index scan plus residual `Filter` that will actually run, not the
  speculative pre-lowering plan.
- **Embedded typed results and parameter binding.** The Node addon gains
  `queryNative`, `querySqlNative`, `queryReadonlyNative`, and
  `queryWithParams`, returning tagged typed cells at parity with the
  networked client's native API: 64-bit ints and datetimes as `bigint`,
  raw bytes as `Buffer`, JSON as the parsed value plus exact PJ1 bytes,
  and empty distinct from JSON `null`. The `powdb` facade gains
  `query_with_params` / `query_readonly_with_params`.
- **Read-only snapshot serving.** A quiescent data directory (restored
  backup or checkpointed replica) can be opened genuinely read-only:
  `Engine::open_read_only`, `powdb-server --readonly` /
  `POWDB_READONLY=1`, and embedded `Database.openReadOnly`. Multiple
  read-only processes serve the same directory concurrently; readers and
  writers exclude each other; a non-empty WAL is refused with a
  recovery-pointing error; mutations return a terminal readonly error.
  See docs/read-only-serving.md for the supported flow.
- **Driver-implementer spec.** docs/integrations/powql-for-drivers.md
  documents the wire protocol, native typed frames, PowQL mapping
  guidance, null semantics, the explain contract, admission semantics,
  and quasi-stable error families for driver and ORM authors.

### Fixed

- The npm client's packaged CHANGELOG had been frozen at 0.8.0; it is
  backfilled through 0.13 and now gated by the version-consistency check.
- docs/POWQL.md now states datetime units explicitly (epoch microseconds),
  matching the storage layer and both clients.

## [0.13.0] - 2026-07-15

### Added

- **Persistent JSON-path indexes.** Native PowQL can add, add unique, and drop
  indexes over scalar paths with `alter T add index (.data->key)`. Equality,
  range, and bounded ordered reads use the path index automatically, while
  missing indexes retain a sequential fallback. SQL `CREATE INDEX` and
  `CREATE UNIQUE INDEX` lower direct JSON `->` paths to the same index form.
- **Expression-valued JSON operations.** JSON paths now work as group keys,
  aggregate arguments, and order keys, including path-based `HAVING`, limit,
  and offset shapes.
- **Lossless native result wire.** Additive typed PowQL, parameterized PowQL,
  and SQL request/result frames preserve actual cell types. The TypeScript
  client exposes `queryNative()` and `querySqlNative()` for exact bytes,
  recursive JSON, and integers outside JavaScript's safe range. Legacy query
  methods and string result frames remain unchanged.
- **SQL JSON arrows.** The SQL frontend supports postfix `->` extraction and
  `->>` canonical text extraction for object keys and array indexes.

### Changed

- **PowQL aggregates over joins are symmetric by default.** `sum`, `count`,
  and `avg` deduplicate fan-out by source row identity. `raw` restores joined-row
  evaluation explicitly; SQL aggregates always retain raw SQL semantics.
- **Concurrent autocommit reads share server admission.** Independent
  read-only queries can overlap, while writers and explicit transactions remain
  exclusive and preserve complete before-or-after visibility.
- **Compound join predicates use hash plus residual evaluation.** An equi-key
  inside a compound `ON` clause selects a hash join, and oversized pure
  non-equi nested loops fail before entering an unbounded pair loop.

### Fixed

- **Query deadlines and disconnects cooperatively stop work.** Supported scan,
  join, group, and mutation-discovery loops release server locks/admission when
  cancelled. Mutation writes retain statement-atomic boundaries.
- **Remote result types no longer require lossy string rendering.** Native
  typed results preserve Empty, strings, raw Bytes, and PJ1 JSON on the wire;
  `json_type()` can distinguish missing paths from explicit JSON null remotely.
- **Sync survives the lazy v5-to-v6 catalog format bump.** Because a database
  stays on catalog format v5 on disk until the first expression index activates
  v6, segment identity now treats catalog format as a compatibility annotation
  rather than a strict-equality field. Producers stamp the database's active
  catalog version (a database that never activated v6 keeps stamping v5, so
  v0.12 replicas still match), consumers accept an older format and reject a
  newer one, a catalog-format increase across a segment chain is accepted while
  a decrease is rejected, and the primary's pull gate accepts any replica whose
  maximum supported format is at least the database's active format. Database
  id, primary generation, WAL format, and segment format remain strict.

## [0.12.0] - 2026-07-14

Native JSON. A new `json` column type stores documents in **PJ1**, a canonical
binary encoding designed for PowDB's compiled-predicate scans, and the new
**`->` path operator** extracts and filters on nested fields directly in
PowQL. Filters on JSON paths compile to a zero-parse, zero-allocation
directory walk over raw mapped bytes: on the reference workload (100K rows,
~1KB documents, 10% selectivity) a JSON-path filter runs in **11ms**, 2.3x
faster than the generic decode path.

### Added

- **`json` column type.** Documents are validated on insert (typed errors for
  invalid JSON/UTF-8, a 128-level nesting cap, and the 64MB value limit) and
  stored canonically: object keys sorted bytewise, duplicate keys last-wins,
  int/float distinction preserved. Equal documents have equal bytes, and
  ordering, grouping, and equality agree everywhere. Large documents spill
  transparently through the 0.11 overflow-page machinery.
- **`->` path extraction.** `.data->author->name`, `.data->tags->0`, and
  `.data->"weird key"` walk into documents and scalarize the result (string
  to `str`, integral number to `int`, bool to `bool`, object/array to a
  `json` sub-document, null and missing to empty). Works in filters,
  projections, and anywhere expressions do, with plan-cache-safe structural
  hashing (same path shares a plan across different literals).
- **`json_type()` scalar function** distinguishing an explicit JSON `null`
  from a missing path, and reporting a node's type.
- **Compiled JSON-path filter leaf.** `filter .data->status = "live"` on an
  inline-document table compiles to a direct binary walk over the row's
  mapped bytes, with semantics differentially tested byte-for-byte identical
  to the interpreted path.
- **Clients.** JSON cells render as canonical JSON text over the existing
  wire format (no protocol change); the TypeScript client's typed API gains
  the `"json"` column type (parsed to objects), and the embedded Node addon
  passes canonical text through.

### Known limitations (documented in docs/POWQL.md)

- Aggregating over a path (`sum(.data->price)`) and grouping/ordering BY a
  path are unsupported. Correction: earlier notes claimed these fail with "a
  clear error"; the actual 0.12 parser result was the opaque
  `unexpected trailing token near token N: '->'` message, not a targeted one.
  Both the support and the targeted errors arrive with the expression-index
  work in a following release (v0.13).
- Over the network, `json_type()` cannot distinguish JSON `null` from a
  missing path (both render as NULL); embedded use distinguishes them. A
  typed wire surface fixes this in a following release.
- Path filters over spilled (>4KB) documents use the decode path rather than
  the compiled walk; correct, but slower until path indexes land.

### On-disk compatibility

Databases that never use `json` are byte-identical to 0.11 and open on 0.11
binaries. The `json` type id is new; files containing json columns require
0.12 binaries. 0.11.x databases and backups open as-is.

## [0.11.0] - 2026-07-13

Document-store foundations. **Overflow pages** remove the roughly 4KB
row-size cap: a single row can now hold values of arbitrary size (up to the
64MB `MAX_VALUE_SIZE` engine limit), spilling the oversized value to a chain
of overflow pages behind a fixed-size stub. This release also fixes
**grouped aggregates over joins** and adds a **bounded wait for bare
autocommit writes** queued behind a held transaction, completing the
transaction-gate story started in 0.10.

On-disk compatibility: a database that never stores an oversized value stays
byte-identical to 0.10 and opens on 0.10 binaries. The row format bumps to v2
and the heap to v3 lazily, only on the first spill; once a value has spilled,
older binaries correctly refuse the file rather than misread it. 0.10.x
databases and backups open as-is.

### Added

- **Overflow pages (P-2).** Values larger than the inline page budget spill
  to a chain of 4KB overflow pages, pointed to by a 24-byte stub carrying the
  logical length, first page, and a CRC32. Fixed-size columns never spill;
  indexed columns are kept inline where possible so index keys stay fast.
  Spills, updates, and deletes are fully WAL-logged (`OverflowWrite` /
  `OverflowFree` records) and replay on crash recovery. A mark-and-sweep
  reclaimer (`powdb sweep`, and an automatic pass after recovery) returns
  orphaned chains to the free list so churn does not leak disk.
- **Bounded wait for bare autocommit writes.** 0.10's `--tx-wait-timeout-ms`
  bounded explicit `begin` only; a bare autocommit write queued behind a held
  transaction still waited unbounded. It now respects the same bound and fails
  with the typed transaction-gate timeout error, and increments the
  `powdb_tx_gate_timeouts_total` metric. A wedged transaction holder can no
  longer wedge autocommit writers indefinitely.

### Fixed

- **Grouped aggregates over joins (P-5).** Aggregating a value field grouped
  by another field across a joined/latest-version shape now resolves qualified
  keys and arguments correctly instead of silently returning nulls.
- **Row-format-v2 correctness (defect class closed).** A class of latent bugs
  where v1 layout math or v1-only code paths were applied to v2 (spilled) rows
  is closed systematically: row decode is v2-aware, every raw byte-patch
  primitive refuses a v2 row, the executor's raw fast paths gate on whether a
  table can hold spilled rows, rid relocation repoints every index, overflow
  chains are freed on update and delete, and `ALTER` plus index rebuilds
  reassemble spilled values. Round trips are byte-exact from 4KB through 256KB
  and beyond, and survive crash recovery.
- **Update fast path never silently drops a spilled row.** The all-fixed-column
  update fast path now routes any row it cannot patch in place through the
  reassembling update path instead of skipping it, so an update can never be
  silently lost or the modified count under-reported.

### Changed

- **Row format v2 and heap format v3** are introduced lazily on first spill
  (see the compatibility note above). Databases that never spill are
  unaffected and remain readable by 0.10 binaries.

## [0.10.0] - 2026-07-13

Dogfood-hardening release from an internal dogfood workload: a **bounded
transaction-gate wait** that turns concurrent `begin` hangs into a clear,
metered error — fixing a ~54% concurrent-create failure mode — plus PowQL
developer-experience work (reserved-word errors + backtick quoting, `if
(not) exists` DDL idempotency, and `schema`/`describe` introspection),
opt-in `--db-name` connect enforcement, a dual ESM+CJS TS client, a
multi-arch (`linux/amd64` + `linux/arm64`) Docker image, and a fix for a
pre-existing rollback bug that could poison a unique index on disk. On-disk
formats unchanged — 0.9.x databases/backups open as-is.

**Breaking change:** `schema` and `describe` are now PowQL keywords. Existing
schemas that use either word as a column, type, or index name must backtick-
quote it (e.g. `` `schema` ``); the same backtick quoting now works for every
reserved word in identifier position.

### Added

- **Bounded transaction-gate wait.** An explicit `begin` now waits on the
  transaction gate with a configurable bound (`--tx-wait-timeout-ms` /
  `POWDB_TX_WAIT_TIMEOUT_MS`, default 5000ms) instead of hanging up to the
  300s idle timeout. On elapse the client gets a clear `transaction gate
  timeout` error and the new `powdb_tx_gate_timeouts_total` metric is
  incremented. Fixes the ~54% concurrent-create failure mode from an
  internal dogfood run, where pooled clients saw `begin` hang past their own
  timeouts. New wire-level e2e tests including the 10-writer repro (0
  failures) and disconnect-mid-transaction gate release.
- **Opt-in `--db-name` / `POWDB_DB_NAME` connect enforcement.** When set,
  `Connect` frames naming a different database are rejected after auth with a
  clear error. Unset keeps the 0.9.x accept-anything behavior (warns once per
  process).
- **PowQL reserved-word quoting.** Reserved words in identifier positions now
  error with an actionable message and can be used as column names via
  backtick quoting, which round-trips lexer → parser → planner → executor
  across DDL, insert, filter, project, order, and index DDL. New "Reserved
  Words and Quoting" section in `docs/POWQL.md` with the complete keyword
  list derived from `POWQL_KEYWORDS`.
- **DDL idempotency.** `if not exists` on create type / add index / add
  unique, and `if exists` on drop / drop view / drop column. No-clause
  semantics are unchanged; a duplicate `create` now names the type in its
  error.
- **Schema introspection.** `schema` lists types; `describe <Type>` (alias
  `schema <Type>`) returns columns, types, nullability, and indexes as normal
  result rows. Plan-cache-safe — reflects live catalog state.
- **TS client: dual ESM + CJS build.** `@zvndev/powdb-client` now ships a CJS
  build with a proper `require` export condition, fixing
  `ERR_PACKAGE_PATH_NOT_EXPORTED` for CommonJS consumers.
- **Multi-arch Docker image.** `ghcr.io/zvn-dev/powdb` now builds
  `linux/amd64` + `linux/arm64` via buildx. The Rust compile stays native on
  `$BUILDPLATFORM` and cross-compiles (no QEMU-emulated cargo build), and the
  portable `target-cpu` is preserved (no SIGILL).
- New `powdb_tx_gate_timeouts_total` metric on the server metrics endpoint.

### Changed

- **DX error messages now survive wire sanitization.** The new reserved-word
  and duplicate-type errors — and pre-existing `table 'X' not found`,
  `type 'X' …`, and backtick lexer diagnostics — were masked to the generic
  `query execution error` by the server's `SAFE_ERROR_PREFIXES` allowlist.
  They are now allowlisted (all derived from the client's own query text,
  leaking no internal state) and covered by a wire-level regression test.

### Fixed

- **Rollback no longer flushes rolled-back index writes to disk.** A
  pre-existing bug (affects ≤0.9.0): rolling back a transaction replaced the
  catalog with a fresh one, but the discarded catalog's Drop-time checkpoint
  still flushed its dirty (rolled-back) btree state to the on-disk `.idx`
  files. Rolling back the same reused auto-id twice reloaded the poisoned
  file: a permanent phantom unique-index entry, wrong-row lookups, and
  spurious unique-constraint violations that survived restart. Fixed by
  discarding dirty index state (`BTree::discard_dirty`, mirroring
  `Heap::discard_dirty`) on the outgoing catalog before it drops. No WAL
  format, on-disk format, or group-commit change.
- **`powdb-cli` rejects non-UTF-8 argv** with a clean exit-2 error instead of
  panicking in `std::env::args()`.

## [0.9.0] - 2026-07-12

Throughput release: WAL **group commit** + server **read-ahead batching** +
TS-client **pipelined scripts and eager connect**. Durable (Full-mode) write
throughput goes from one-fsync-per-statement to fsync-sharing across
in-flight work — measured locally at ~250/s → ~16,000/s for a pipelined
500-insert burst and ~250/s → ~1,000/s aggregate for 8 concurrent
connections — with Full durability semantics unchanged.

### Added

- **WAL group commit (Full sync mode).** Overlapping committers now share a
  single covering fsync (leader/follower over the WAL's shared sync fd)
  instead of paying one each. Full mode's promise is untouched: no statement
  is acknowledged before an fsync covering its WAL records has returned; if
  that fsync fails the statement is not acknowledged. A lone sequential
  committer still fsyncs exactly once per commit — no timers, no added
  latency. New engine-level contract suite (`crates/query/tests/group_commit.rs`)
  pins lone-committer no-regression, fsync sharing, and crash recovery of a
  coalesced batch; the standing durability suite is unchanged and green.
- **Server read-ahead batching for pipelining clients.** When a client's
  next query frames are already buffered, the connection loop executes them
  back-to-back (cap 128) and settles ONE durability ticket for the burst
  (durability generations are cumulative). Replies keep frame order; a
  non-query frame flushes the batch and is handled normally; batching is
  disabled while an explicit transaction is open; if the covering fsync
  fails, every success reply in the batch is downgraded to a durability
  error. New wire-level suite (`crates/server/tests/pipelined_batching.rs`).
- **TS client: `client.execScript(script, opts?)` / `pool.execScript(...)`** —
  statement-aware multi-statement script execution (same `;`-in-strings and
  `#`-comment splitting as the CLI), pipelined down one connection: N
  statements cost ~1 round trip instead of N. Fail-fast by default
  (rejects with the new `PowDBScriptError` carrying `statementIndex`,
  `statement`, and prior `results`); `continueOnError: true` returns a dense
  per-statement outcome array.
- **TS client: `execScript(..., { transactional: true })`** — all-or-nothing
  scripts done safely: the client opens the transaction, waits for every
  statement's reply, and only then sends `commit` (or `rollback` on any
  failure). Embedding your own `begin`/`commit` in a pipelined script is
  rejected — the trailing `commit` would already be on the wire when an
  error reply arrives, silently committing partial work.
- **TS client: eager connect** — `Client.connect({ eager: true })` returns
  after the socket opens and the Connect frame is written; queries issued
  immediately are pipelined right behind the handshake (−1 RTT on every
  fresh connection). `client.ready()` exposes the handshake outcome.
- **TS client: `splitStatements()`** exported for callers that want the
  script splitter without execution.

### Changed

- **Durability-failure errors are now explicit on the wire.** A statement
  whose covering fsync failed replies `WAL durability sync failed: …`
  (previously masked to the generic `query execution error`). Clients must
  be able to distinguish "your write was rejected" from "your write executed
  in memory but was never made durable".
- Wire-protocol server: back-to-back `Connect`+`Query` (frames written
  before the handshake reply is read) is now pinned by an integration test
  as a supported client behavior.

## [0.8.1] - 2026-07-11

Production-readiness patch from a full-product audit: a critical TS-client fix,
statement-aware loading + uuid/bytes fidelity for bulk imports (#150, #151),
embedded-lifecycle hardening, a CI fix, and a truth pass over every doc and the
marketing site.

### Fixed

- **TS client: aborting an in-flight query no longer corrupts the connection.**
  `onData` consumed settled (aborted) pending entries incorrectly, delivering
  the aborted query's reply to the *next* query — silently wrong results for
  the rest of the connection — or tearing the connection down with
  `protocol_error` and hanging `close()`. Replies are now matched one frame to
  one pending entry and discarded when that entry is settled; `close()` after
  an errored teardown releases the socket.
- **TS client: plain `AbortController.abort()` now rejects with
  `PowDBError` code `"aborted"`** instead of a raw `DOMException`; custom
  abort reasons still pass through as documented.
- **`powdb-cli --exec` splits statements with string-literal and `#`-comment
  awareness** (previously a naive `';'` split), so text values containing `;`
  or newlines load intact — embedded and remote (#150).
- **CLI: the interactive REPL no longer panics** with a raw Rust panic on
  engine-open failure; it prints the same clean error as the one-shot path.
  All CLI stderr failures now use a uniform `Error:` prefix.
- **CI: `examples-smoke` SIGILL fixed** (red `ci-success` on main since #148):
  the job was the only builder missing the portable
  `RUSTFLAGS: -C target-cpu=x86-64-v2` override, so `target-cpu=native` from
  `.cargo/config.toml` crashed on heterogeneous shared runners.
- Embedded (Rust facade): query errors render their human `Display` message
  (was `Debug`); `query_readonly` rejects mutations with an actionable
  message instead of leaking an internal sentinel.
- `clients/ts/pnpm-workspace.yaml` contained committed placeholder text that
  broke `pnpm install`.

### Added

- **`powdb-cli --exec-file <PATH>`** (or `-` for stdin) loads a whole PowQL
  file, removing the shell ARG_MAX ceiling on large loads.
- **`uuid` and `bytes` columns accept validated string literals directly**,
  plus `uuid("…")` / `bytes("…")` cast sugar in insert and filter positions;
  plan-cache and prepared-insert paths store real typed values (#151). SQL
  `CREATE TABLE` now accepts `bytea` (maps to `bytes`; spell hex with a
  doubled backslash in SQL string literals, e.g. `'\\xdeadbeef'`).
- **Embedded Node addon: `Database.close()`** for deterministic
  flush/checkpoint and data-dir lock release; opening the same directory twice
  in one process now throws instead of silently creating two live engines.
  The README documents the lifecycle and the 3 supported prebuild platforms.

### Changed

- `scripts/check-version-consistency.sh` now also gates
  `clients/sync/package.json` (version + exact peer pins), every
  `ghcr.io/zvn-dev/powdb` image tag under `examples/`, and version strings in
  `site/` — the marketing site can no longer freeze on an old release.
- Deploy examples bumped from the stale `v0.6.1` image pin; root
  `docker-compose.yml` dropped the obsolete `version` key.
- rustdoc warnings 23 → 0 across 5 crates; `powdb-server`'s crates.io
  description no longer claims "no SQL parsing layer"; `bindings/node`
  package metadata corrected (repo URL, `napi.targets`); internal sprint
  jargon stripped from public code comments.

### Docs & site

- Marketing site unfroze from v0.4.8: corrected the false "No SQL
  compatibility" positioning (PowQL-native + SQL frontend, one engine, two
  languages), added the embedded Node package, SQL frontend, and experimental
  sync to the story, fixed two broken PowQL snippets, and made the benchmark
  copy accurate. Every snippet on the site now executes against the real CLI.
- Docs truth pass: `powdb-vs-sqlite.md` SQL reality, README v0.7.x features
  (`returning` / `default` / `auto`) + embedded install, `FORMAT.md` catalog
  v5 + `catalog.lsn` sidecar + retained-segment formats, `POWQL.md`
  `now()` / `alter … required` / upsert caveats, `SQL.md` wire-error notes,
  `RELEASES.md` platform matrices and `powdb-sync` publish order,
  `CONTRIBUTING.md` project structure, TS client `querySql()` docs.

## [0.8.0] - 2026-07-02

Embedded Sync **Milestone 0** — the retained replication-unit log substrate for
primary-authoritative embedded replicas. **The entire sync feature is
experimental.** It ships so the substrate can bake against the core engine; the
public `@zvndev/powdb-sync` package is **beta-gated and is NOT published to npm
until the Milestone-1 gates pass** (crash matrix, concurrent-read-during-apply,
version-compat rejection, handshake, perf, and fuzz — see below and
`docs/embedded-sync.md`). Existing PowDB users are unaffected by the sync
feature itself, but note the additive core-engine changes called out under
*Changed*.

### Added

- **`powdb-sync` crate (experimental).** Retained replication-unit segment
  format (magic / version / WAL-format / catalog-format / database-identity /
  primary-generation / unit-count / LSN-range / footer CRC), atomic no-clobber
  segment publish (temp write → fsync → hard-link publish → directory fsync),
  identity-aware range reads by LSN with gap/overlap/corruption rejection,
  durable sync identity, primary-side replica-cursor metadata with stale-lock
  recovery, and cursor-based retention GC that fails closed on a corrupt,
  gapped, or wrong-identity retained tail.
- **Private authenticated server sync frames** (`0x20`–`0x25`): status / pull /
  ack, sharing the server transaction gate, with pull output capped by both the
  authoritative remote LSN and the currently servable retained LSN, and V1
  transaction-boundary validation so a chunk limit or buggy client cannot strand
  retained history at a mid-transaction cursor.
- **Backup-based replica bootstrap** — full/incremental backup manifests carry
  optional sync fork-safety metadata; default restore strips sync identity for
  plain-engine safety, with explicit preserve/fork restore modes.
- **Two new JS packages / bindings (experimental):** `@zvndev/powdb-sync`
  (embedded-replica orchestration: local readonly reads, primary-authoritative
  write-forward, retained-unit pull/apply/ack, background sync scheduler) and
  low-level sync helpers in `@zvndev/powdb-client` (`syncStatus`, `syncPull`,
  `syncAck`); the `@zvndev/powdb-embedded` addon exposes
  `applyRetainedUnits(...)`.

### Changed

- **Durable `catalog.lsn` sidecar.** The catalog now persists its durability
  high-water mark in a `catalog.lsn` file next to `catalog.bin`. **Backward
  compatible:** a v0.7.2 database predates the sidecar, so on first open under
  0.8.0 the durable LSN reads as `0` and is recovered from page LSNs exactly as
  before; the sidecar is (re)written on the next durable mutation. On-disk
  catalog format is **unchanged — still v5**; v1–v4 catalogs still load.
- **Sync-aware checkpoint / recovery guards.** Checkpoint, recovery, engine
  open/drop, and rollback archive WAL records into retained segments *before*
  truncation when a sync identity exists. Plain checkpoint/recovery **fails
  closed** for sync-enabled WAL history when no archive hook is provided rather
  than silently discarding retained history; a failed archive leaves the
  transaction retryable.
- **Additive backup manifest fields.** `BackupManifest` gains an optional
  `sync` metadata block (`#[serde(default)]`); backup `format_version` is
  unchanged, so a v0.7.2 backup (no `sync` field, no `catalog.lsn` entry)
  restores unchanged.

### Not in this release (Milestone-1 gates, still experimental)

`@zvndev/powdb-sync` will not be published until these land: crash-injection
matrix (RF-04 / RF-11 / PH-02), concurrent-read-during-apply (RA-01 / RA-09),
version-compat rejection (RA-03), handshake (SP-01 / SP-05), perf gates
(PH-05), and fuzz (PH-07). Also deferred: offline local writes, partial /
row-level sync, and DDL write-forward.

## [0.7.2] - 2026-06-29

A correctness and documentation patch from the post-v0.7.1 gold-standard audit
and ORM integration issue triage.

### Fixed

- **`in (<subquery>)` no longer returns stale rows across same-shape calls.**
  The plan cache keys queries by canonical shape and re-binds literals on a hit,
  but a subquery's inner literal lives in an un-walked `QueryExpr` AST that the
  substitution pass could not reach. Two same-shape `… in (<subquery>)` queries
  differing only in the inner literal returned the **first** call's rows in
  release builds (silent wrong answer) and tripped a substitution-count
  assertion in debug builds. The cache now refuses to store any plan whose
  substitutable literal slots don't match the literals collected from the source
  (the only such case today is a subquery), so those queries plan from source on
  every call and always return correct rows. The hot cache-hit path is
  unaffected — the check runs only when populating the cache. Affected the shared
  executor, so the TCP server path inherited it too. (#137)

### Documentation

- README's PowQL set-operation example used a parenthesised `(A) union (B)` form
  the parser rejects; corrected to the unparenthesised `A union B` form that the
  parser and `docs/POWQL.md` use. (#71)
- `RELEASES.md` described the `@zvndev/powdb-embedded` build matrix as five
  platforms including Windows; it ships four (macOS arm64/x64, Linux x64/arm64),
  with Windows deferred until the storage engine is ported off Unix-only APIs.
- `docs/getting-started.md` example output showed a stale `v0.6.2` REPL banner.

## [0.7.1] - 2026-06-28

A correctness, safety, and embedded-write-performance patch driven by an ORM
integration's v0.7.0 adoption review plus a PowDB-side audit.

### Fixed

- **SQL `count(*)` (and ungrouped `sum`/`avg`/`min`/`max`) now aggregate.** The
  SQL frontend lowered an ungrouped aggregate `SELECT` to a row projection
  (`T { count(*) }`) and returned one null row per source row instead of a
  scalar — a silent wrong answer on both the server `QuerySql` path and the
  embedded addon, present since the SQL frontend shipped (v0.5.0). It now lowers
  to PowQL's aggregate form (`count(T filter ...)`). Multiple ungrouped
  aggregates, or an aggregate over a join / with `DISTINCT`, return a clear
  unsupported-feature error instead of garbage. Grouped aggregates were already
  correct.
- **Embedded `open()` is panic-safe.** A corrupt heap/index header panicked deep
  in the open path; under `panic = "unwind"` (the Node addon) that could abort
  the host process. `Database::open` / `open_with_memory_limit` are now wrapped
  in `catch_unwind` and return `Error::OpenPanicked`.
- Version drift: `bindings/node/Cargo.toml` was stuck at 0.6.2.

### Added

- **Data-directory lock.** A PID-based lock file prevents two separate live
  processes from opening the same data directory (concurrent writers corrupt the
  heap/WAL). Same-process reopen (after a crash) and dead-owner takeover are
  allowed, so crash recovery is unaffected.
- **Embedded durability control.** `@zvndev/powdb-embedded` gains
  `db.setSyncMode("full" | "normal" | "off")` and a
  `Database.openWithMemoryLimit(dir, limitBytes)` factory. `"normal"` moves the
  fsync off the commit path (background flusher), closing the embedded write gap
  versus SQLite. `"full"` remains the default.

## [0.7.0] - 2026-06-27

The write-performance + ORM-ergonomics + embedded-mode release. Highlights:
opt-in `Normal` WAL durability (15–40× faster single-row writes), `RETURNING`
on PowQL **and** SQL, column `default` values, `auto`-increment columns, a Unix
domain socket transport, and **embedded mode** — run the engine in-process via
the `powdb` Rust crate or the `@zvndev/powdb-embedded` Node addon. `Full`
durability remains the default; the on-disk catalog format moved to v5 and older
catalogs (v1–v4) still load.

### Added
- **Embedded mode — Node addon `@zvndev/powdb-embedded`** (`bindings/node/`).
  A napi-rs native addon exposing the in-process engine to JavaScript with no
  server/socket: `Database.open(dir)` → `.query(powql)` / `.querySql(sql)` /
  `.queryReadonly(...)`. Results match the `@zvndev/powdb-client` `QueryResult`
  shape exactly (rows as `string[][]`, `affected` as `bigint`) so embedded and
  networked code paths are interchangeable — the foundation for local-first
  apps (e.g. `powdbEmbedded({ embedded })`). Built as a standalone
  `panic = "unwind"` workspace so a query panic is caught and surfaced as a JS
  error rather than aborting the host process.
- **Embedded mode — `powdb` crate** (the SQLite-shaped front door). Run the
  engine **in-process**, no server/socket: `Database::open(dir)` →
  `.query(powql)` / `.query_sql(sql)` / `.query_readonly(...)`. Single-op
  latency becomes the engine's own cost (no wire round-trip) and the database
  works offline — the foundation for local-first apps. Reuses the same storage
  engine, indexes, WAL durability, and PowQL/SQL frontends as the server.
  Panic-safe for embedding: every query is wrapped in `catch_unwind`; a caught
  panic poisons the handle (further calls error) and skips the clean checkpoint
  so torn in-memory state is never persisted — committed data is recovered from
  the WAL on reopen (crash-only, scoped to the handle). See
  `docs/design/2026-06-27-embedded-mode-design.md`. The Node native addon
  (`@zvndev/powdb-embedded`) wrapping this lands next.
- **Unix domain socket listener** (write-performance / transport) — start the
  server with `--socket <path>` (or `POWDB_SOCKET=<path>`) to listen on a Unix
  domain socket in addition to TCP. Same-host clients avoid the TCP/IP stack
  (~2× lower round-trip latency), which is the dominant cost per op for
  co-located clients. Additive: the TCP listener always runs; the socket is
  local-only (no TLS, no IP rate-limiting). The TypeScript client gains a
  `{ path }` connection option (`Client.connect({ path: "/run/powdb.sock" })`).
  See `docs/design/2026-06-27-beating-sqlite-latency-design.md`.
- **`insert`/`update`/`delete ... returning`** (write-performance Phase 3) — a
  write statement ending with `returning` now returns the affected rows (all
  columns) as a result set instead of a modified-count, so a client no longer
  needs a follow-up `SELECT` to read the rows back (removes the ORM reselect
  round-trip). `insert` returns the inserted rows, `update` the **post-update**
  image, `delete` the **pre-delete** image. Works for single- and multi-row
  writes and over the wire (the rows come back on the existing rows path).
  `returning` is opt-in: without it, every existing fast path (byte-patch,
  fused single-pass scan/update/delete) is unchanged.
- **Auto-increment columns** (write-performance Phase 3, ORM ergonomics) — an
  integer column may be declared `auto` (PowQL `unique auto id: int`, SQL
  `id INTEGER AUTOINCREMENT`). When an insert omits it, the engine assigns the
  next value from a per-table sequence and returns it via `RETURNING *` — the
  canonical *insert-without-the-id, read-it-back* flow. The sequence resumes
  above the highest existing id after a restart (recomputed from the recovered
  data, so a crash never reuses a committed id); an explicit value pushes the
  sequence past it. `auto` requires an `int` column, can't be combined with a
  `default`, and applies on `insert` (not `upsert`). Persisted in the catalog
  (on-disk format bumped to v5 — older catalogs still load). This reverses the
  former "PowDB does not generate implicit IDs" stance. See `docs/POWQL.md`.
- **Column `default` values** (write-performance Phase 3, ORM ergonomics) — a
  column may declare a literal default: PowQL `status: str default "active"`,
  SQL `status TEXT DEFAULT 'active'`. When an insert (or upsert-insert) omits the
  column, the default is filled in — applied *before* the required-column check,
  so a `required` column with a default may be omitted. Defaults are scalar
  literals only (`int`/`float`/`str`/`bool`); a type-mismatched default is
  rejected at table-creation time. Defaults are persisted in the catalog
  (on-disk format bumped to v4 — older v1–v3 catalogs still load, with no
  defaults) and the create-table WAL record. Pairs with `RETURNING` for the
  insert-without-the-value-then-read-it-back flow. See `docs/POWQL.md`.
- **SQL `RETURNING *`** (write-performance Phase 3) — the SQL frontend now
  accepts an optional trailing `RETURNING *` on `INSERT`/`UPDATE`/`DELETE`,
  lowering to PowQL's `returning` clause. ORMs hitting the SQL surface (the
  standard `INSERT INTO t (...) VALUES (...), (...) RETURNING *` createMany
  shape) get the inserted/updated/deleted rows back in one round-trip instead of
  a write followed by a reselect. Column-projected `RETURNING a, b` returns an
  explicit unsupported-feature error (PowQL `returning` is all-columns). See
  `docs/SQL.md`.
- **`Normal` WAL durability mode** (write-performance Phase 1) — a third
  `WalSyncMode` between `Full` and `Off`. Commits are acknowledged once their
  WAL record reaches the OS page cache (no per-commit fsync); a background
  flusher fsyncs on a ~10 ms interval. A process crash loses nothing; an OS
  crash / power loss can lose only the unsynced tail (≤ one interval). This is
  SQLite `synchronous=NORMAL` / Postgres `synchronous_commit=off` semantics and
  removes the per-write fsync from the latency path (~15–40× faster single-row
  writes). Select it with `POWDB_SYNC_MODE=full|normal|off` (default `full` —
  no durability change unless opted in). Addresses an ORM integration's
  write-latency finding; see `docs/design/2026-06-27-write-performance-*`.

### Security
- **Fixed a remotely-triggerable denial-of-service in the in-place `UPDATE`
  fast path** (#117). Assigning a value whose type does not match a fixed-size
  column (e.g. a `str` into a `float` column) reached
  `unreachable!("all_fixed_nonnull guard lied")` and, under the deliberate
  `panic = "abort"` profile, took the whole server process down for every
  connection. The fast path now coerces each assignment to the column's
  declared type before writing bytes and returns a typed `TypeError` on a
  genuine mismatch. Reported by an ORM integration test.

### Fixed
- **The on-disk catalog now loads every format version from 1 up to the
  current one.** The format-version gate accepted only `{1, 2, CATALOG_VERSION}`,
  so when `CATALOG_VERSION` moved to 5 this release it silently *rejected*
  catalogs written at version 3 (v0.6.x) or 4 (an intermediate build) with
  `unsupported catalog version` — a database created by a v0.6.x server would
  fail to open after upgrading, i.e. **data loss on upgrade**. The field-reading
  path already handled v3/v4; only the gate was stale. It is now a range check
  (`1..=CATALOG_VERSION`) so older catalogs always load and the next bump can't
  reintroduce the gap. Caught by the v0.7.0 pre-release audit before publish.
- **Catalog open rejects an implausible table count instead of over-allocating.**
  A corrupt or hostile `catalog.bin` could claim billions of tables and make the
  reader attempt a multi-gigabyte allocation (host abort — fatal in embedded
  mode). The count is now bounded by the file length before any allocation, the
  same guard the b-tree loader already had.
- **The Normal-mode WAL background flusher now reports fsync failures.** It
  previously swallowed them (`&& sync_data().is_ok()`); in `WalSyncMode::Normal`
  that background fsync is the *only* durability point, so a silent `ENOSPC`/`EIO`
  let the server keep acking non-durable commits. Failures are now logged (and
  the sync is retried on the next tick).
- **`UPDATE` now coerces an integer assigned to a `float` column to `f64`**
  (#118) instead of writing the raw i64 bit pattern (which read back as a
  denormal such as `5e-323`) — silent numeric corruption on the in-place
  update fast path. `INSERT` already coerced correctly; the byte-patch
  `UPDATE` path bypassed it. Fix covers both the indexed and the fused
  `Filter(SeqScan)` update paths. Reported by an ORM integration test.
- **Completed that coercion fix across the remaining write paths** (#117/#118).
  The previous fix only covered literal assignments; a *computed* assignment
  (any non-literal RHS, e.g. `balance := .tag + 9`) took the per-row
  expression path, which wrote the evaluated value into the row with no
  coercion — and so did the `UPSERT` on-conflict path. Consequences: an
  int-valued expression into a `float` column silently corrupted the row
  (#118), and a str-valued expression into a fixed-size column reached the
  row encoder's `unreachable!` and **aborted the process** (`panic = "abort"`)
  — a remotely-triggerable DoS (#117). All write paths (`INSERT`, literal and
  expression `UPDATE` including `RETURNING`, and `UPSERT`) now coerce each
  assignment to the target column's declared type and return a typed error on
  a genuine mismatch. Reported by an ORM integration test.

## [0.6.2] - 2026-06-26

### Security
- **Fixed three remotely-triggerable denial-of-service vectors** found in the
  2026-06-26 code review (all are process aborts/exhaustion under the deliberate
  `panic = "abort"` profile, so a connected — and in the first case
  unauthenticated — client could disrupt the server):
  - **Pre-auth memory amplification** (`crates/server/src/protocol.rs`): a
    ~12-byte `RESULT_ROWS` frame declaring up to 10M zero-column rows forced a
    ~240 MB allocation during the pre-auth read. The decoder now bounds the
    row/column/param preallocation by the actual remaining payload and rejects
    counts that cannot fit.
  - **Parser stack overflow** (`crates/query/src/parser.rs`): chained unary
    prefixes (`not not … .x`, `exists …`) recursed through `parse_primary`
    without the nesting-depth guard. The depth limit now covers that recursion.
  - **`LIKE` stack overflow / exponential backtracking**
    (`crates/query/src/executor/eval.rs`): `col like "…"` used a recursive
    matcher that overflowed on long inputs and backtracked exponentially on
    patterns like `%a%a%`. Replaced with an iterative two-pointer matcher
    (O(n·m) time, O(1) stack).

### Fixed
- **`avg()` over a column containing NULLs** returned the wrong value on the
  generic aggregate path: it divided by the total row count instead of the count
  of non-null values, and disagreed with the compiled fast path. Both paths now
  divide by the contributing count and return the empty value when there are
  none. (`crates/query/src/executor/{plan_exec,mod}.rs`)
- **Correlated and `IN`-subquery comparisons over `datetime`, `uuid`, `bytes`,
  and NULL values** silently coerced those values to `0`, producing wrong
  results. Such values are now carried verbatim through subquery substitution
  via a runtime-only AST node. (`crates/query/src/executor/eval.rs`)

### Documentation
- Reconciled the SQL story across `README.md`, `AGENTS.md`, and `CLAUDE.md`:
  these onboarding docs previously asserted "no SQL / no translation layer"
  even though the SQL frontend shipped in v0.5.0. They now describe SQL as a
  supported-subset frontend that lowers to the PowQL AST (PowQL remains the
  native path) and link `docs/SQL.md`. `CLAUDE.md` no longer instructs agents
  to refuse SQL work.
- Refreshed the `SECURITY.md` supported-versions table to the 0.6.x line.

### Internal
- CI now feeds every job (clippy/fmt/test, miri, asan, MSRV, examples-smoke,
  ts-client, secret-scan, audit) into a single `ci-success` aggregator job so
  branch protection can require one check and still gate on the whole matrix —
  closing the gap where safety jobs could fail without blocking a merge.
- `scripts/check-version-consistency.sh` now also asserts `SECURITY.md` lists
  the current minor series as supported, so the supported-versions table can't
  drift behind the shipping release.
- Regression tests for all five code-review fixes (parser depth, LIKE
  adversarial input, wire-decode amplification, avg-over-NULL generic path,
  correlated subquery over datetime/NULL).

## [0.6.1] - 2026-06-19

### Security
- `powdb-server` now applies `POWDB_REQUIRE_TLS=1` to every credentialed auth
  mode: shared-password auth, named-user auth, and first-run admin bootstrap
  auth. Operators can no longer accidentally enable named users or bootstrap
  admin credentials over plaintext TCP when TLS is required.
- Backup restore now rejects unsafe manifest file names before writing files:
  absolute paths, `..`, path separators, Windows drive/ADS-style colons,
  unexpected file names, and mismatched incremental delta targets are all
  refused. This closes the path-traversal restore class for full and
  incremental backup archives.
- Remote row responses are now capped before wire serialization. Oversized
  materialized result sets return an actionable `result too large` error
  instead of letting one query allocate unbounded response memory.

### Fixed
- Query timeout handling no longer claims a blocking query was aborted while
  the underlying work can still continue in the blocking executor. The timeout
  counter now represents threshold breaches, and the handler waits for the work
  to finish before replying so timed-out writes cannot keep mutating detached
  from the connection lifecycle.
- Deployment examples and release docs no longer pin stale container/image
  versions in active examples; they now point at the current `v0.6.1` release
  line.

### Known limitations
- Full cooperative query cancellation is still planned work rather than part of
  this hardening patch. A scoped implementation plan is tracked separately.

## [0.6.0] - 2026-06-19

### Added
- `powdb-server` can serve a Prometheus `/metrics` endpoint on a separate, opt-in HTTP listener (`--metrics-addr` / `POWDB_METRICS_ADDR`, e.g. `127.0.0.1:9090`). Exposes the four golden signals plus security and build identity: `powdb_connections_active`, `powdb_connections_accepted_total`, `powdb_queries_total{result}`, `powdb_queries_in_flight`, `powdb_query_duration_seconds` (histogram), `powdb_query_timeouts_total`, `powdb_query_memory_limit_exceeded_total`, `powdb_auth_failures_total`, `powdb_tls_handshake_failures_total`, `powdb_build_info`, and `powdb_uptime_seconds`. Zero new dependencies; scrapes are lock-free (never touch the engine lock). The endpoint is unauthenticated — bind it to localhost or a private network.

### Fixed
- `powdb-server` now drains gracefully on **SIGTERM**, not just SIGINT (Ctrl-C). `docker stop`, Kubernetes pod termination, and systemd all send SIGTERM — previously the process was killed by the signal, skipping the connection drain and `catalog.checkpoint()`. Committed data was still recovered via WAL replay on restart, but in-flight queries were cut and the WAL was left un-checkpointed (slower recovery).

## [0.5.1] - 2026-06-17

### Changed
- Bumped `zeroize` 1.8.2 → 1.9.0, the dependency used to scrub secrets from memory in `powdb-auth` and `powdb-server`.

### Internal
- Pinned the fuzz workflow to an explicit `nightly` toolchain so cargo-fuzz builds survive `dtolnay/rust-toolchain` SHA bumps.
- Routine dev-dependency bumps (`postgres` test harness, `@types/node`) and a remaining-work backlog docs refresh. No runtime or wire-protocol changes.

## [0.5.0] - 2026-06-16

### Added
- **Connection-scoped explicit transactions over TCP.** `BEGIN` now owns the transaction on the connection that opened it; other connections wait behind the transaction gate until `COMMIT`, `ROLLBACK`, disconnect, or timeout, preventing cross-connection visibility into uncommitted state.
- **Self-identifying storage formats.** Heap files, pages, rows, and WAL files now carry magic/version metadata, reject unknown future versions, preserve legacy-read compatibility, and expose format introspection docs in `docs/FORMAT.md`.
- **SQL frontend (explicit dialect).** A production SQL subset lowers into the existing PowQL AST/plan path with `Engine::execute_sql`, read-only SQL execution, wire protocol `QuerySql` (`0x05`), TypeScript `querySql`, SQL/PowQL plan-cache parity, unsupported-feature errors, and `docs/SQL.md`.

### Changed
- **Bench baselines refreshed for the v0.5.0 foundation.** The release gate now compares against post-format-versioning/post-SQL measurements; the `scan_filter_count_over_btree_lookup` thesis ratio was relaxed only because `btree_lookup` improved materially while aggregate absolute performance remained inside its gate.
- Remote CLI default database naming now matches the TypeScript client default (`default`).

### Fixed
- WAL replay now honors transaction commit boundaries, so uncommitted records do not replay after crash recovery.
- Rollback now discards uncommitted WAL writer spillover before reopening/replay, including multi-page dirty inserts.
- Raw aggregate/order fast paths now account for the `PROW` row-format prefix before reading null bitmaps and fixed-width values. This restores integer/float aggregate and sort correctness after row versioning.
- Parser diagnostics now include clearer trailing-token positions and typo suggestions for statement-like keywords.
- The concurrent-read corruption regression remains a real indexed `heap.get`/`disk.read_page` race check but is bounded enough to complete in full workspace/release runs.
- The unused storage `BufferPool` module and tests were removed to eliminate dead API surface before 0.5.0.
- Unix/Windows disk-positioned I/O paths now use platform `FileExt` helpers instead of a Windows TODO.

### Verification
- Release gate passed locally before publishing: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo check --workspace`, full `cargo test --workspace -- --nocapture`, TypeScript client runtime tests, release-binary durability smoke, nightly fuzz target builds/short runs, and the bench comparator after rebaseline.

## [0.4.9] - 2026-06-15

### Security
- **Fixed two remotely-craftable denial-of-service crashes** surfaced by the
  2026-06-14 product review. Both are process aborts under the deliberate
  `panic = "abort"` release profile, so an unauthenticated-but-connected client
  could take the server down with a single query:
  - **Integer-division overflow** (`crates/query/src/executor/eval.rs`): `i64::MIN / -1`
    panics even in release. Division now uses `checked_div` and returns the
    empty set on overflow or divide-by-zero, matching the other arithmetic arms.
  - **Unbounded `LIMIT` pre-allocation** (`crates/query/src/executor/plan_exec.rs`):
    the sort+limit fast path reserved a top-N heap of the raw user `LIMIT` up
    front (e.g. `order .x limit 99999999999` → multi-terabyte reservation →
    allocator abort). The pre-allocation is now capped; the heap still grows on
    demand to the true limit.
- **Data directory is now created `0700` on Unix** (`powdb_storage::create_data_dir_secure`).
  Previously the directory — and the heap/WAL/index files holding all row data —
  were created under the default umask and were world/group readable. `auth.json`
  was already `0600`; this extends the same posture to the table data, matching
  PostgreSQL's owner-only data-directory model.

### Internal
- Regression tests for all three fixes in `crates/query/tests/safety_limits.rs`
  (the malicious division and huge-`LIMIT` queries, plus a `0700` permission
  assertion) so they cannot silently regress.

## [0.4.8] - 2026-06-10

### Added
- **RBAC now enforces the full permission lattice.** The server maps each
  statement to the capability it needs — reads → `Read`, row mutations
  (insert/update/delete/upsert) → `Write`, schema changes (create/alter/drop
  type or view) → `Ddl` — and checks it against the user's role. The
  `readwrite` role now explicitly carries `Ddl` (application users create and
  evolve their own tables), so **this is behavior-preserving**: readwrite and
  admin keep full access, readonly is still read-only, and any authenticated
  role may still run read-only queries. `Admin` remains reserved for user/role
  management (CLI-only).
- **Automated post-publish durability smoke** (`scripts/smoke-release.sh`),
  wired as a required gate in `release.yml`: installs the built binaries, runs
  the README PowQL flow over the wire, then `kill -9`s the server and restarts
  it to assert WAL replay recovers every row and the unique constraint still
  holds. This is the exact gate whose absence caused the v0.4.1–0.4.3 data-loss
  yanks; it now runs on every tagged release.
- **MSRV build job** in CI that compiles the workspace with the pinned 1.93
  toolchain (the previous job only checked that the version string matched the
  docs).

### Changed
- **Resource-limit errors now reach remote clients verbatim.** Sort, join, and
  per-query memory-budget errors (e.g. "sort input exceeds row limit — add a
  LIMIT clause") were being masked to the generic "query execution error" by
  the wire sanitizer. They carry actionable guidance and leak no internal
  state, so they are now on the safe-error allowlist.

### Fixed
- CLI `--help` showed a remote one-shot example using a `|` pipe operator that
  PowQL does not have; corrected to the whitespace-pipeline syntax so the
  example runs as written.
- CI `cargo audit` no longer fails on three `postgres`-only RUSTSEC advisories
  whose entire dependency path is confined to the `publish = false`
  `powdb-compare` benchmark crate (scoped ignore in `.cargo/audit.toml` + the
  audit action, with provenance comments). No shipping crate is affected.
- Dockerfile dependency-cache stage now copies the `powdb-auth` and
  `powdb-backup` manifests it was silently missing, so the cached layer covers
  the full server/CLI dependency closure.
- TypeScript client version drift: the `CLIENT_VERSION` handshake constant,
  the built `dist`, and the README now all agree with `package.json` (0.5.0),
  and a CI job asserts they can't diverge again.

### Internal
- Documented `panic = "abort"` as a deliberate **crash-only** design: on a
  panic the server exits fast and a supervisor restarts it, with WAL replay
  recovering to a consistent state — safer for a stateful engine than
  unwinding into a poisoned lock. Every deploy example is confirmed to run
  under an auto-restart policy, and the requirement is now documented in
  `examples/deploy/README.md`.
- Promoted the CI lint policy into `[workspace.lints]` (`clippy::all = deny`)
  so `cargo clippy` fails locally with the same rules CI enforces.
- Removed ~190 LOC of dead, never-wired snapshot-isolation scaffolding
  (`storage::mvcc`, `storage::tx`) that was shipping in the `powdb-storage`
  crate; the live engine uses `RwLock` concurrency.
- Refreshed stale `powdb-auth` doc-comments that claimed the crate was "not
  yet wired into the server or CLI" — it has enforced auth/RBAC since 0.4.6.

## [0.4.7] - 2026-06-10

### Added
- **Parameter binding over the wire (`$1`..`$N`).** Clients can send a query
  template plus positional values instead of interpolating untrusted input
  into the query string. Placeholders are 1-based `$N` (not `?` — `??` is the
  COALESCE operator). Binding happens at the **token level** on the server:
  each `$N` is replaced with the literal token for its value before parsing,
  so an injection-shaped string is inert data and can never change the query's
  shape. New wire message `QueryWithParams` (`0x04`) — a pure protocol
  addition; existing messages and pre-0.4.7 clients are unaffected. The
  TypeScript client gains `client.query(powql, params?)`. Engine API:
  `Engine::execute_powql_with_params` / `execute_powql_readonly_with_params`.
- **Unique constraints.** Declare a column unique with the `unique` field
  modifier (`type User { required unique email: str }`) or add one to an
  existing table with `alter User add unique .email` (which scans for existing
  duplicates first and fails if any exist). Declaring `unique` auto-creates a
  unique B+tree index; enforcement is a storage-layer pre-check shared by the
  plain, prepared, and upsert write paths, so duplicates are rejected with
  `unique constraint violation on <table>.<column>` before anything is written
  or WAL-logged. The constraint survives restart (persisted in the catalog +
  rebuilt on WAL replay).
- **Range scans use B+tree indexes.** `>`, `>=`, `<`, `<=`, and `between` on an
  indexed column now traverse the index (unique: raw keys; non-unique:
  composite `(value, rid)` keys) instead of always falling back to a full
  scan — roughly 7× faster on a selective range over 100K rows. NULLs are
  correctly excluded and exclusive bounds are honored.
- **`EXPLAIN` shows the executed plan.** Because the planner is pure (no
  catalog access), it emits speculative `IndexScan`/`RangeScan` nodes; the
  executor lowers them at runtime when no index exists. `EXPLAIN` now applies
  the same lowering before printing, so it shows `Filter(SeqScan)` for an
  unindexed column instead of a misleading `IndexScan`.
- **Multi-line REPL input.** The `powdb-cli` REPL buffers lines until braces
  and parentheses balance (outside string literals), so multi-line `type` and
  `insert` statements can be pasted or typed across lines.
- **Agent-DX evaluation harness** (`scripts/agent-eval/`). A model-agnostic,
  offline harness that scores how well an LLM writes PowQL given only
  `AGENTS.md` and a 10-table schema, with a parallel SQLite baseline for
  comparison. Not wired into CI.

### Changed
- **BREAKING:** `upsert <T> on .col` now requires `.col` to be a `unique`
  column. Declare it with the `unique` modifier or `alter <T> add unique .col`.
  This fixes a bug where `upsert ... on .id` followed by a plain
  `insert` of the same id silently produced duplicate rows.

### Fixed
- Lowering an unindexed equality update to `Filter(SeqScan)` exposed a fused
  scan-update path that swallowed `update_hinted` errors and still counted the
  row as modified — which bypassed the v0.4.6 oversized-row guard for that
  path. Errors now propagate as `StorageError`; all three `oversized_rows`
  tests pass.

## [0.4.6] - 2026-06-09

### Fixed
- **Oversized rows no longer kill the server (remote DoS).** Inserting or
  updating a row whose encoded size exceeds one 4 KB page (`MAX_ROW_DATA_SIZE`,
  4070 bytes) previously hit a `panic!` in the heap layer, which — combined with
  `panic = "abort"` — terminated the entire server process. Any connected client
  could take down the database with a single ~5 KB string insert. The heap now
  rejects oversized rows with a graceful `row too large: N bytes exceeds max M
  bytes` query error before anything is written or WAL-logged (an oversized
  update can no longer poison WAL replay), and the connection and server keep
  running. There is still no large-object/overflow-page support — values over
  ~4 KB are rejected, not stored.
- **`readonly` role is now enforced at the query layer.** Previously the role
  was authenticated and stored but never checked: a `readonly` user could
  insert, update, delete, and drop tables. The server now classifies each
  parsed statement and rejects writes (DML, DDL, view DDL, and transaction
  control) from `readonly` principals with `permission denied: role 'readonly'
  cannot execute write statements`. Unknown roles fail closed. Shared-password
  mode, open mode, and embedded use are unaffected.
- **NULL values now arrive as `null` on the wire instead of `{}`.** The server
  serialized SQL NULLs as `{}`, which the remote CLI displayed verbatim and
  which broke the TS client's documented `"null"` sentinel for typed-row
  decoding. The wire serialization is now the bareword `null`, and the remote
  REPL renders it as `NULL`, matching embedded mode.
- **Window aggregates without `order` now compute the whole-partition value.**
  `avg(.x) over (partition .d)` previously returned a running aggregate per
  row (frame = partition start → current row) even with no `order` clause; per
  standard semantics the frame is now the entire partition. Ordered windows
  keep the running-frame behavior; ranking functions are unchanged.

### Documentation
- Ecosystem-wide accuracy sweep: site pages synced to v0.4.5 (banners were
  v0.2.0, MSRV corrected to 1.93), crates.io homepage URL fixed (was a 404),
  README/CONTRIBUTING/AGENTS no longer claim the bench suite is a CI merge
  gate, SECURITY.md documents both auth modes and the ≤0.4.5 readonly caveat,
  RELEASES.md covers all six crates + the Docker image, deploy examples fixed
  (`fly.toml` was missing `POWDB_BIND=0.0.0.0`), TS client docs document the
  multi-user-server incompatibility, and AGENTS.md gained small-model-tested
  gotchas (reserved aggregate keywords as aliases; line-oriented REPL).

## [0.4.5] - 2026-06-09

### Added
- **Multi-user authentication (operator surface).** The server now authenticates
  per-connection `(username, password)` against a persisted user store
  (`auth.json`, argon2id hashes only) when any users are defined. New CLI flag
  `powdb-cli --user <NAME>` carries the username on remote connect, and new
  offline user-admin subcommands edit the data dir's user store without a running
  server: `useradd <NAME> [--role <ROLE>] [--password <PW>]` (role defaults to
  `readwrite`; password may come from `POWDB_NEW_PASSWORD`), `userdel <NAME>`,
  `passwd <NAME> [--password <PW>]`, and `users` (lists name + role, never
  hashes). The server can bootstrap an initial admin from `POWDB_ADMIN_USER` /
  `POWDB_ADMIN_PASSWORD` on startup when that user does not yet exist. Added
  `UserStore::set_password`. **Backward compatible:** with no users defined, the
  single shared-password model (`POWDB_PASSWORD` / `--password`) still applies.
  New `powdb-auth` crate.
- **Full snapshot backup & restore.** `powdb-cli backup <dest>` takes a
  crash-consistent, blake3-verified full snapshot (checkpoint-then-copy of
  `catalog.bin` + every heap + every index, plus an integrity `manifest.json`
  recording each file's hash/size and the page-LSN the snapshot is consistent
  at); `powdb-cli restore <backup> <dest>` re-verifies every file against the
  manifest before writing and rebuilds a fresh data dir, validating by
  reopening (which preserves the post-restore LSN invariant so writes made
  after a restore survive a later crash). Offline / single-writer in this
  release — do not back up a directory a live server has open. Incremental
  backup, point-in-time restore, and cloud sync are planned (see
  `docs/design/2026-06-05-backup-pitr-sync-migrations-plan.md`). New
  `powdb-backup` crate; guide at `docs/backup-and-restore.md`.
- **Multi-row INSERT.** `insert T { a := 1 }, { a := 2 }, { a := 3 }` inserts
  many rows in a single statement. This is the fastest durable way to bulk-load:
  one statement = N WAL appends + **one fsync** (vs one fsync per single-row
  autocommit statement), and over a network connection it's **one round trip**
  instead of N — the right tool for remote bulk writes. Semantics:
  - **All-or-nothing on validation** — if any row is missing a required field,
    names an unknown column, or has an uncoercible value, the whole statement
    fails and no rows are written (rows are validated before any insert).
  - The whole batch is charged against `POWDB_QUERY_MEMORY_LIMIT`, so an
    over-large batch errors cleanly instead of exhausting memory.
  - Single-row `insert T { … }` is unchanged (still hits the prepared
    byte-patch fast path). Covered by a new `crates/query/tests/multi_row_insert.rs`
    suite (correctness, atomicity, transaction batching, plan-cache literal
    substitution across rows, 1000-row batches, memory budget, crash recovery).
- **Incremental backup & chain / point-in-time restore.** `powdb-cli backup
  <dest> --base <full_dir>` writes a **differential** backup against a full
  base — only the 4 KB heap/index pages whose `page_lsn` is newer than the
  base's high-water mark are stored (in `<name>.delta` sidecars), with the
  catalog copied whole when it changed. `powdb-cli restore <full> <dest>
  --apply <inc>...` chain-restores a full base plus one or more increments in
  the order given, enabling **coarse point-in-time restore** (recover to the
  state captured by the increment you stop at). The chain is verified before it
  writes a usable database: **page-LSN continuity** (each increment's recorded
  base LSN must match the running LSN) plus **blake3** on every delta /
  whole-file copy, then a reopen-to-validate. Fine-grained (sub-increment) PITR
  via WAL archiving and cloud sync remain future work. See
  `docs/backup-and-restore.md`.

## [0.4.4] - 2026-06-04

Critical durability release. Three distinct data-loss bugs found by a full
production test pass (server mode, ~40K rows, driven as a new user) — all
reproduced by tests that fail on 0.4.3 and pass here. **v0.4.1, v0.4.2, and
v0.4.3 are all yanked. Upgrade to 0.4.4.**

### Fixed — data loss (P0)

- **Writes issued after a restart/crash-recovery were silently lost on the next crash.** After a clean shutdown (or recovery) the WAL is truncated, but `Wal::open` reset the LSN counter to `1` while heap pages kept the high LSNs stamped during the previous session. New writes reused already-persisted LSNs, so the next crash's replay skipped them as "already applied."

  **Fix:** `Catalog::open` now restores `next_lsn` to `max(page LSN across all tables) + 1` on every open (new `Wal::set_next_lsn_at_least`). LSNs are monotonic across restarts. Guard: `test_writes_after_crash_recovery_survive_second_crash`, `test_repeated_restart_write_crash_cycles`.

- **Crash after a partial page flush duplicated rows and orphaned deletes/updates.** Two compounding causes: (1) `Insert` WAL records carried a placeholder RowId, so replay re-`insert`ed at a fresh slot, duplicating already-persisted rows; (2) the idempotency check used a single per-table max LSN, which could wrongly skip a low-LSN record stranded on an unflushed page because a *different* flushed page advertised a higher LSN. Compounding both, `DiskManager::allocate_page` zero-extends the file, and a zero page reads with a malformed header (`free_start = 0`), so replay packed re-inserted rows differently and later Update/Delete records (carrying the original RowIds) missed.

  **Fix:** ARIES-style per-page redo. `Insert` now logs its real RowId (heap insert first, then log) and replay places the row at that exact RowId via the new `HeapFile::insert_at` / `Page::insert_at_slot`, so every later record stays correctly targeted regardless of how pages were flushed. Replay skips a record iff its *target page's* LSN ≥ the record's LSN. Newly grown pages are initialised as valid empty pages, never zero pages. Guard: `test_mixed_mutations_survive_crash`, `test_cross_table_mutations_with_ddl_survive_crash`.

- **Prepared-statement inserts bypassed the WAL entirely.** The `execute_prepared` insert fast path called the raw `Table::insert` (heap only) and never logged a WAL record, so every prepared insert was lost on a crash.

  **Fix:** the fast path now routes through the new WAL-logging `Catalog::insert_by_slot` (keeps the O(1) prepared-time slot resolution). Guard: `test_prepared_insert_survives_crash`.

### Fixed — correctness

- **`count(T filter .x in (<subquery>))` silently returned 0.** The `count` fast path evaluated the predicate against raw row bytes without materialising the subquery. Both the read (`execute_plan_readonly`) and write (`execute_plan`) count fast paths now fall through to the generic path when the predicate contains a subquery. Guard: `test_count_with_in_subquery`.
- **Materialized-view auto-refresh did not fire for `count(View)` or `View filter …`,** so those access shapes returned stale data after an underlying mutation — contradicting the documented "no stale reads." Both shapes now refresh a dirty view first. Guard: `test_materialized_view_autorefresh`.
- **`cast(expr as type)` documentation didn't match the parser.** The reference showed `cast(.age as str)`, but the parser accepts `cast(.age, "str")`. Docs corrected to the implemented syntax.

### Added
- `crates/query/tests/durability.rs` — a permanent durability contract suite covering restart-then-crash, repeated crash-loops, mixed mutations, cross-table DDL, and prepared inserts. Every test fails on 0.4.3 and passes here. This is the gate the prior three releases lacked.

### Process note
v0.4.1–v0.4.3 all shipped data-loss bugs despite green unit/integration/clippy/miri/ASan/audit/criterion gates, because none of those gates exercised crash recovery with real mutation mixes and restarts. The new durability suite runs in CI and is the standing guard against this class of regression.

## [0.4.3] - 2026-06-02

P0 data-loss fix. Both v0.4.1 and v0.4.2 are affected; upgrade immediately and
consider yanking those versions if you published artifacts that depend on them.

### Fixed
- **`alter add column` + `update` + `alter add index` corrupted the heap on crash recovery.** WAL replay re-applied the pre-DDL `Insert` records to a heap whose rows had already been rewritten into the post-DDL layout, producing a mixed-layout heap that panicked on the next projection with `range start index N out of range for slice of length M` in `powdb-storage/src/row.rs`. Reproducible by following only the README's documented PowQL flow.

  **Root cause:** the alter paths rewrote every row through `rewrite_rows_for_schema_change` but never bumped the heap pages' LSNs, so the LSN-based idempotency check in `Catalog::replay_wal` saw `max_page_lsn == 0` and re-injected every pre-DDL row.

  **Fix:** `alter_table_add_column` and `alter_table_drop_column` now stamp every heap page with the DDL record's LSN after the rewrite (via the new `HeapFile::stamp_all_pages_min_lsn` + `Wal::last_appended_lsn`), and the WAL replay handlers for `DdlAddColumn` / `DdlDropColumn` do the same. Replay now correctly recognises every pre-DDL row record as already-applied and skips it. Verified via the new crash-recovery integration test `test_alter_add_column_then_index_survives_crash` in `crates/query/tests/wal_recovery_executor.rs`, which fails on 0.4.2 with the exact panic from the bug report and passes after the fix.

### Process note
This bug shipped because the v0.4.1 / v0.4.2 release gates were self-referential — full unit/integration/clippy/miri/ASan/cargo-audit/criterion/examples-smoke were green, but no release gate ran the README's own documented usage flow against the published binary with a real server restart in between. The post-release smoke test caught it on the first restart. The release process will add a "README-flow smoke against the installed crates.io binary, including a kill -9 + restart" step before the next publish.

## [0.4.2] - 2026-06-01

Documentation + example pinning patch surfaced by the v0.4.1 post-release audit. No engine changes.

### Fixed
- **Install instructions pinned to 0.4.0 instead of the current release.** README's production checklist and the PowDB-vs-SQLite guide both contained `cargo install … --version 0.4.0 --locked`, sending new users to the old version. Now points at 0.4.2.
- **Deployment examples defaulted to `ghcr.io/zvndev/powdb:latest`.** The AWS ECS Fargate Terraform module (`aws-ecs/variables.tf`) and the Cloudflare Tunnel docker-compose now pin to `v0.4.2` for example reproducibility.

### Known issues
- `@zvndev/powdb-client` npm package is stuck at 0.3.3 — the publish workflow has no npm step. Tracking in #68.

## [0.4.1] - 2026-06-01

Phase 1 (perf + security hardening) + Phase 2 (deployment + DX), shipped together.

### Added
- **CRC32 heap page checksums** with backward-compatibility — pages stamped on flush, verified on cold reads. New `HeapFile::verify_integrity()` for on-demand full-file scrub. Zero per-read overhead on the mmap fast path (documented trade-off — the on-demand scrub is the honest guarantee against silent disk bit-rot).
- **Per-query memory budget** — `POWDB_QUERY_MEMORY_LIMIT` (default 256 MiB) bounds materialization for sort, join, GROUP BY, and IN-list. New `QueryError::MemoryLimitExceeded` returned cleanly (no panic). RAII reentrancy guard so nested `execute_powql` (view refresh) cannot reset the outer budget.
- **`POWDB_REQUIRE_TLS`** — startup gate that refuses to bind when a password is set without TLS configured. Off by default for backward compat.
- **Postgres comparison benchmark** — `powdb-compare` now includes Postgres with a graceful skip when unreachable, plus `crates/compare/docker-compose.yml` for a one-command local PG.
- **`scripts/dev.sh`** — one-command local boot (`up | repl | bench | down`) with free-port discovery, tmp data dir, and `rm -rf` guard against non-tmp paths.
- **Deployment examples** — AWS ECS Fargate + EFS Terraform module, Cloudflare Tunnel docker-compose, Railway `railway.toml`, plus a refreshed Fly.io example with the new env vars and properly sized memory budget vs concurrency.
- **CI `examples-smoke` job** — Terraform validate + docker compose config + `dev.sh up/down` lifecycle on every PR.
- **`docs/powdb-vs-sqlite.md`** — honest when-to-use-which guide with side-by-side feature and benchmark tables.
- **Phase 3 risky-research dossier** with per-subsystem go/no-go verdicts (Windows file I/O port, disk-spill external sort, cost-based optimizer plumbing; multi-writer MVCC explicitly no-go).

### Fixed
- **`BufferPool::ensure_loaded` panicked on a corrupt page** and skipped CRC verification — now uses `from_bytes_verified` and returns `PageCorrupt`. The four `BufferPool` write paths now stamp CRCs.
- **Postgres `SUM(BIGINT)` cast bug** — `SUM` returns `NUMERIC` in PostgreSQL; the comparison engine now casts to `::bigint` to match the deserialization target.
- **Insert benchmark stability** — the `insert_single`/`insert_batch_1k` workloads now use the prepared/`InsertFast` path with literals pre-built outside the timed loop. Eliminates a ~13× run-to-run swing that previously made the insert workloads unmeasurable.
- **Cloudflared example placeholder** — added an inline `# REPLACE THIS` comment so deployers cannot silently ship a config that 404s on the wrong hostname.
- **Fly.io memory contradiction** — the example previously set `POWDB_QUERY_MEMORY_LIMIT=256 MiB` on a 256 MiB VM with `hard_limit=200` connections, the exact OOM scenario the README warns against. Limit dropped to 64 MiB, hard_limit to 32, and the comment now shows the per-query × concurrency math.

### Security
- **Password zeroization** — both the configured `POWDB_PASSWORD` and the client-supplied candidate from the wire are wrapped in `zeroize::Zeroizing<String>` so they are wiped on drop.
- **Production-path `unwrap()` driven to zero** on the wire-protocol decode path, DB-open path (WAL/catalog/page), and view-file path. Garbage inputs now return errors, never panic. Fuzz: 3.2 M iterations clean.
- **Reduced server panic surface** by converting 23 production-code `unwrap()`s on fallible paths to typed `Result`/`PageCorrupt` errors. Provably-infallible call sites converted to `.expect("invariant: …")` with documented invariants.

### Changed
- **README reframed** to lead with the compiled-predicate engine architecture (pure-Rust embedded DB, compiled byte-level predicates + zero-copy mmap + plan cache). PowQL is now positioned as the DX front door rather than the headline thesis. Production checklist now documents per-query (not global) semantics of `POWDB_QUERY_MEMORY_LIMIT`.
- **Test count** grew from **582 to 612** (+30 tests across all changes). All gates remained green throughout: `cargo test --workspace`, clippy, fmt, miri, AddressSanitizer, cargo audit, criterion regression gate, and the new examples-smoke job.

## [0.4.0] - 2026-05-26

### Added
- **Explicit transactions** — `BEGIN`, `COMMIT`, and `ROLLBACK` statements for grouping multiple operations into atomic units. Uncommitted changes are discarded on `ROLLBACK` or connection close
- **Benchmark tuning** — promoted `powql_filter_only` and `projection` workloads to NOISY (10%) threshold to reduce false-positive regressions

### Fixed
- Critical bugs and review findings from the 0.4.0 audit (executor, storage edge cases)
- Upgraded `rcgen` 0.13 to 0.14 (drops transitive `ring` dependency)

### Changed
- Bumped all crate versions to 0.4.0
- Rebaselined benchmarks after hardening; relaxed `insert_single` ratio threshold (16 to 300)
- Documentation and packaging improvements from smoke-audit findings

## [0.3.1] - 2026-05-18

### Added
- **LSN-tagged WAL replay** — pages carry monotonic LSNs; replay skips already-applied records, eliminating data duplication on crash recovery
- **CRC32 checksums on catalog.bin** — the catalog file now has integrity checking, matching WAL and btree index files
- **StorageError enum** — typed error handling for the storage crate (replaces raw `io::Result`)
- **Bounds validation on page slots** — `iter_page_slots` returns `None` for corrupt entries instead of panicking
- **Bounds checks on unsafe executor macros** — guards against UB from corrupt row data in `agg_int_loop!`/`agg_float_loop!`
- **Plaintext password warning** — server logs a loud warning when password auth is enabled without TLS
- **Pre-auth payload limit** — CONNECT messages capped at 4KB (was 64MB), blocking pre-auth memory exhaustion
- **67 new tests** — B+tree edge cases (27), buffer pool (12), catalog corruption (10), WAL CRC rejection (14), TLS connections (4)
- **Landing page** — static docs site in `site/` for GitHub Pages with benchmarks, getting started, PowQL reference
- **Docker image CI** — release workflow now pushes to ghcr.io/zvndev/powdb on tag
- **Crates.io publish workflow** — `workflow_dispatch` for automated cargo publish in dependency order
- **Dependabot** — weekly checks for cargo, npm, and GitHub Actions dependencies
- **MSRV declared** — `rust-version = "1.75"` in workspace Cargo.toml
- **Crate-level `//!` docs** on query and server crates

### Fixed
- Flaky `connection_management` tests — replaced hand-rolled temp dirs with `tempfile::tempdir()`
- `// SAFETY:` comments on all unsafe blocks in storage and query crates

### Changed
- Updated SECURITY.md with TLS documentation, supported versions, auth mechanisms
- Updated benchmark numbers to latest run
- Added CHANGELOG entry for v0.3.0
- GitHub repo topics added for discoverability

## [0.3.0] - 2026-05-18

### Added

- **DDL WAL replay**: schema mutations (CREATE TABLE, DROP TABLE, ADD COLUMN,
  DROP COLUMN) are now logged to the WAL with record types 6-9. Crash recovery
  replays DDL operations idempotently — if the table/column already exists or
  was already dropped, replay skips gracefully. WAL records are flushed
  immediately before filesystem mutations for durability.
- **CRC32 checksums on B+ tree nodes**: every serialized B+ tree node now
  includes a CRC32 checksum (last 4 bytes), verified on load. Returns
  `io::Error(InvalidData)` on mismatch, protecting against silent corruption of
  index data on disk.
- `TypeId::from_u8()` convenience method on storage types.
- Doc-tests added across lexer, parser, executor, and storage modules.

### Fixed

- **Compiled predicates**: bounds-checked `CompiledLeaf::eval()` — no panics on
  corrupt row data.
- Bounds checks in sort+limit fast path and mmap heap scan slot directory reads.
- UTF-8 slicing bug found by fuzzer: `&s[..20]` replaced with
  `&s[..s.floor_char_boundary(20)]` in token display.
- Clippy `collapsible_if` lint in connection management tests.

### CI

- Added Miri job (scoped to non-mmap modules: btree, page, row, types, tx,
  view).
- Added AddressSanitizer job (hard gate, leak detection disabled for mmap
  regions).
- Fixed fuzz workflow `cargo-fuzz` install (removed `--locked` to avoid stale
  transitive deps).
- Updated required status check names in branch protection.

## [0.2.1] - 2026-05-10

Patch release carrying the security and QA fixes found after 0.2.0 was
published. crates.io versions are immutable, so these could not be folded into
0.2.0 and shipped as 0.2.1 instead.

### Security

- **The shared-password comparison leaked the password's length.** The
  constant-time comparison returned early when the two inputs differed in
  length, which is a length oracle regardless of how the remaining bytes are
  compared. Both inputs are now hashed to SHA-256 before comparison, so the
  compared buffers are always the same size.
- **Added a 1 MB query-length cap (`MAX_QUERY_LENGTH`).** The handler dispatched
  arbitrarily large query strings to the parser, so a single oversized query
  could consume unbounded memory during parsing. The length is now checked
  before dispatch.
- Pinned all GitHub Actions to commit SHAs and added a top-level deny-all
  `permissions: {}` to the ci, bench, fuzz, and release workflows.
- Removed a hardcoded production IP address from the TypeScript client's JSDoc
  and demo script, and added `.env*`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, and
  `credentials.json` to `.gitignore`.
- Bumped rustls-webpki 0.103.12 to 0.103.13 (RUSTSEC-2026-0104).

### Added

- `--version` / `-V` flag on the `powdb-server` binary.

### Fixed

- Documentation and test-harness fixes found by real user-flow QA, including
  self-contained TypeScript live tests.
- Refreshed the benchmark baseline from CI runner medians, without changing the
  thesis ratio ceilings.

## [TS client 0.3.3] - 2026-05-10

### Fixed

- Report intentional protocol truncation errors for malformed short
  `ResultRows` and `ResultOk` frames instead of surfacing raw Node buffer
  range errors.

### Changed

- Pin local package-manager metadata to `pnpm@10.29.3` so audit and install
  tooling match the v9 lockfile used by the TypeScript client workspace.

## [TS client 0.3.1] - 2026-05-09

### Fixed

- Added Ping/Pong protocol support — client auto-replies Pong on server
  health check Ping frames instead of throwing "unknown message type"

## [0.2.0] - 2026-05-09

### Security

- Fixed path traversal vulnerability via table names
- Added TLS support for encrypted connections
- Added authentication rate limiting
- Added row size validation to prevent u16 overflow corruption
- Replaced unsafe UTF-8 decoding with safe alternative

### Added

- CLI meta-commands: `.tables`, `.schema`, `.timing`, `.help`, `.quit`
- Tab completion for PowQL keywords
- Persistent command history
- Query timing display
- Health check (Ping/Pong) protocol message
- Cross join and sort row count safety limits
- String literal size limit (16 MB)
- `--version` flag for CLI and server binaries
- `cargo install powdb-cli` / `cargo install powdb-server` install path

### Fixed

- Version strings now use Cargo.toml version instead of hardcoded "0.1.0"
- NULL values display as "NULL" instead of "{}"
- UUID values display in full standard format
- B-tree load validates file size to prevent OOM
- mmap slot access validates bounds

### Changed

- README repositioned around compiled predicates and PowQL ergonomics
- Server `--password` flag removed (use `POWDB_PASSWORD` env var)
- Human-readable error messages in parser
- PowQL docs updated with math, date/time, window functions, CAST, EXPLAIN, UPSERT

## [TS client 0.3.0] - 2026-04-16

TypeScript client (`@zvndev/powdb-client`) production-readiness release.
Server is unaffected — this is a client-only version bump. No breaking
changes to the 0.2.x API surface; all additions are additive.

### Added

- **Structured errors**: every error thrown by the client is now a
  `PowDBError` with a stable `.code` (`connect_failed`, `auth_failed`,
  `query_failed`, `aborted`, `size_exceeded`, `protocol_error`, `closed`,
  `timeout`, `type_coercion_failed`). Callers can branch on `.code` without
  string-matching messages. `isPowDBError(err)` narrows `unknown` for use
  in catch blocks.
- **Typed rows**: `client.queryTyped(query, schema, opts?)` coerces the
  server's string wire format into JS values using a caller-supplied
  schema (`int`, `float`, `bool`, `str`, `datetime`, `uuid`). Int values
  that exceed `Number.MAX_SAFE_INTEGER` are promoted to `bigint`;
  datetime microseconds are converted via `BigInt` division to avoid
  float precision loss at year-2262 boundaries. Bytes columns are
  intentionally unsupported until the wire protocol grows a binary type.
- **Polling watch**: `client.watch(query, { intervalMs, onRows, onError?,
  stopOnError? })` re-runs a query on an interval and delivers rows to a
  callback. Guards against pile-up when a query is slower than the
  interval; returns `{ stop() }` to cancel. Uses `handle.unref()` so the
  watcher doesn't keep the event loop alive on its own.
- **Observability hooks**: `Client` now extends `EventEmitter`. Emits
  `"query"` (with `{ query, durationMs, ok, kind?, error? }`) after every
  query and `"close"` (with `{ error }`) exactly once per socket. Designed
  to be dropped into OpenTelemetry, pino, or a Prometheus exporter.
- **Pool connect retries**: `Pool` now retries transient connect-phase
  failures (`connect_failed`, `timeout`) with exponential backoff. New
  options: `connectRetries` (default 3), `connectBackoffMs` (default
  100), `connectMaxBackoffMs` (default 2_000). Auth failures, size
  violations, and other non-transient errors are never retried — they'd
  just fail the same way.

### Changed

- All raw `Error` throws in the client have been replaced with
  `PowDBError`. Existing `instanceof Error` checks still work; new code
  should prefer `instanceof PowDBError` + `.code` branching.

## [TS client 0.2.0] - 2026-04-16

TypeScript client (`@zvndev/powdb-client`) hardening release. Server is
unaffected — this is a client-only version bump.

### Added

- **Safe query composition**: new `powql` tagged template plus `escapeLiteral`,
  `escapeIdent`, and `ident()` helpers. The tagged template escapes literals
  and validates identifiers, neutralising PowQL-injection attacks (the same
  class of issue as SQL injection).
- **Connection pool**: new `Pool` class with `acquire`/`release`/`withClient`,
  FIFO waiter queue, and `acquireTimeoutMs`.
- **TLS support**: `tls` option on `Client.connect` accepts `true` or a
  `tls.ConnectionOptions` object.
- **Cancellation**: `client.query(q, { signal })` accepts an `AbortSignal`;
  cancelling does not tear down the socket — the reply is discarded so other
  in-flight queries keep working.
- **Frame size limits**: decoder enforces `MAX_PAYLOAD_SIZE` (64 MiB),
  `MAX_ROWS` (10M), and `MAX_COLUMNS` (4096) matching the server.
- **Version check**: on connect, warns once per `host:port` if the server's
  major version differs from the client's.
- **TCP keepalive**: `setKeepAlive(true, 30_000)` enabled on every connection
  so dead peers are detected on idle sockets.

### Fixed

- **O(n²) receive buffer**: the previous `Buffer.concat` on every chunk caused
  quadratic CPU on large result sets. Replaced with a lazy chunk queue that
  only coalesces when needed to decode a frame.

### Changed

- Minimum Node.js version is now explicitly `>=18` (was previously implicit).

## [0.1.2] - 2026-04-16

Hardening release: all known fuzz-reachable panics in the query layer are now
errors, and the CI gate has been tightened with cargo-audit and blocking fuzz
smoke runs.

### Fixed

- **Lexer**: integer literals wider than `i64::MAX` now return `LexError`
  instead of panicking (#25, closes #24)
- **Parser**: unterminated projection/assignment/argument/type-decl bodies at
  EOF (`nn{`, `z{`, etc.) now return `ParseError` instead of panicking via
  out-of-bounds indexing (#25, closes #26)
- **Executor**: `ORDER BY` on an unknown column now returns an error instead
  of panicking (#22)

### Security

- Bumped `rustls-webpki` 0.103.10 → 0.103.12 to pick up fixes for
  RUSTSEC-2026-0098 / RUSTSEC-2026-0099 (name-constraint bypass) (#25)

### CI

- New `cargo audit` job on every PR — blocks merges on known advisories (#23)
- New fuzz smoke workflow: `fuzz_lexer`, `fuzz_parser`, `fuzz_roundtrip` each
  run 60s on PRs that touch the query front-end, and nightly at 07:00 UTC.
  Blocking on failure (#23, #25)

## [0.1.1] - 2026-04-14

Post-launch polish: TS client test coverage, engine bug fixes surfaced by
end-to-end testing, and documentation sync.

### Added

- **TS client**: 53 end-to-end tests covering DDL, insert, filter, projection,
  aggregates, joins, GROUP BY/HAVING, subqueries, updates, deletes, and error
  paths (#18)
- **AGENTS.md**: user-facing primer with PowQL-vs-SQL cheat sheet, footgun
  table, and performance notes for AI assistants and new users (#20)

### Fixed

- **Parser**: `= null` and `!= null` now desugar to `IS NULL` / `IS NOT NULL`
  instead of being rejected (#19)
- **Executor**: `HAVING` on post-projection group queries now filters groups
  correctly (#19)
- **Parser**: statements with trailing tokens (e.g. `User match T on ...`,
  `User create_index .col`) now error cleanly instead of silently parsing as a
  bare-source query and dropping the rest (#19)
- **Executor**: DDL statements (`alter ... add index`, `alter ... add column`,
  `alter ... drop column`) now return an affected-count result instead of an
  empty row set (#19)

### Changed

- **Docs**: `README.md`, `docs/getting-started.md`, and `docs/POWQL.md` updated
  to use current syntax everywhere — `alter T add index .col` (not
  `create_index`), `alter T add column` (not `add_column`), `sum(T { .x })`
  (not `sum(T | .x)`), and `T1 as a inner join T2 as b on ...` (not `match`) (#20)

## [0.1.0] - 2026-04-12

Initial release of PowDB — a from-scratch database engine with PowQL query language.

### Added

- **Storage engine**: slotted 4KB pages, heap files, B+ tree indexes (disk-persisted), WAL with group commit, buffer pool with clock-sweep eviction, mmap-based scanning
- **Row format**: compact binary encoding with 1-byte type tags, variable-length strings, support for Int, Float, Str, Bool, DateTime, UUID, Bytes types
- **Query language (PowQL)**: lexer, recursive-descent parser, pure-function planner, executor with compiled predicates
  - Schema: `type T { required field: type, ... }`
  - Insert: `insert T { field := value, ... }`
  - Query pipeline: `T filter .field > val order .field desc limit N { .f1, .f2 }`
  - Aggregates: `count()`, `sum()`, `avg()`, `min()`, `max()` with GROUP BY / HAVING
  - Joins: INNER, LEFT OUTER, RIGHT OUTER (rewritten to LEFT), CROSS
  - Subqueries: IN, EXISTS, NOT IN, NOT EXISTS (correlated and uncorrelated)
  - Window functions: ROW_NUMBER, RANK, DENSE_RANK, SUM/AVG/MIN/MAX OVER
  - Set operations: UNION, UNION ALL
  - EXPLAIN for query plan inspection
  - CAST, CASE/WHEN, BETWEEN, LIKE, IS NULL/IS NOT NULL
  - Scalar functions: UPPER, LOWER, LENGTH, TRIM, SUBSTRING, CONCAT, ABS, ROUND, CEIL, FLOOR, SQRT, POW, NOW, EXTRACT, DATE_ADD, DATE_DIFF
  - UPSERT with ON CONFLICT
  - ALTER TABLE ADD/DROP COLUMN
  - Materialized views (CREATE VIEW, REFRESH VIEW, DROP VIEW)
- **Plan cache**: FNV-1a canonical hashing, literal substitution at lookup time
- **Executor fast paths**: compiled predicates for zero-decode filtering, fused scan+update, fused scan+delete, sort+limit, project+limit, aggregation fast paths
- **Plan lowering**: `RangeScan` → `Filter(SeqScan)` for unindexed columns at execution time
- **TCP server**: Tokio-based, binary wire protocol, password authentication, connection limits, graceful shutdown
- **CLI**: rustyline REPL with embedded and remote modes
- **TypeScript client**: `@zvndev/powdb-client` with full wire protocol support
- **Benchmarks**: criterion regression gate (20 workloads, per-workload thresholds), wide comparison suite vs SQLite/Postgres/MySQL
- **CI**: clippy + fmt + test workflow, criterion regression gate workflow
- **Performance**: 1.3x–10.8x faster than SQLite across all 15 comparison workloads at 100K rows

### Performance (100K rows, vs SQLite)

| Workload | Ratio |
|---|---|
| point_lookup_indexed | 3.8x faster |
| scan_filter_count | 6.7x faster |
| agg_min | 10.8x faster |
| agg_sum | 9.2x faster |
| update_by_filter | 3.2x faster |
| delete_by_filter | 1.3x faster |

<!-- Release comparison links. Generated from `git tag -l`; the four
     `TS client x.y.z` headings are npm releases with no git tag, so they
     have no compare link. -->

[Unreleased]: https://github.com/ZVN-DEV/powdb/compare/v0.27.0...HEAD
[0.27.0]: https://github.com/ZVN-DEV/powdb/compare/v0.26.0...v0.27.0
[0.26.0]: https://github.com/ZVN-DEV/powdb/compare/v0.25.0...v0.26.0
[0.25.0]: https://github.com/ZVN-DEV/powdb/compare/v0.24.0...v0.25.0
[0.24.0]: https://github.com/ZVN-DEV/powdb/compare/v0.23.0...v0.24.0
[0.23.0]: https://github.com/ZVN-DEV/powdb/compare/v0.22.0...v0.23.0
[0.22.0]: https://github.com/ZVN-DEV/powdb/compare/v0.21.0...v0.22.0
[0.21.0]: https://github.com/ZVN-DEV/powdb/compare/v0.20.0...v0.21.0
[0.20.0]: https://github.com/ZVN-DEV/powdb/compare/v0.19.1...v0.20.0
[0.19.1]: https://github.com/ZVN-DEV/powdb/compare/v0.19.0...v0.19.1
[0.19.0]: https://github.com/ZVN-DEV/powdb/compare/v0.18.2...v0.19.0
[0.18.2]: https://github.com/ZVN-DEV/powdb/compare/v0.18.1...v0.18.2
[0.18.1]: https://github.com/ZVN-DEV/powdb/compare/v0.18.0...v0.18.1
[0.18.0]: https://github.com/ZVN-DEV/powdb/compare/v0.17.0...v0.18.0
[0.17.0]: https://github.com/ZVN-DEV/powdb/compare/v0.16.0...v0.17.0
[0.16.0]: https://github.com/ZVN-DEV/powdb/compare/v0.15.0...v0.16.0
[0.15.0]: https://github.com/ZVN-DEV/powdb/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/ZVN-DEV/powdb/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/ZVN-DEV/powdb/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/ZVN-DEV/powdb/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/ZVN-DEV/powdb/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/ZVN-DEV/powdb/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/ZVN-DEV/powdb/compare/v0.8.1...v0.9.0
[0.8.1]: https://github.com/ZVN-DEV/powdb/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/ZVN-DEV/powdb/compare/v0.7.2...v0.8.0
[0.7.2]: https://github.com/ZVN-DEV/powdb/compare/v0.7.1...v0.7.2
[0.7.1]: https://github.com/ZVN-DEV/powdb/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/ZVN-DEV/powdb/compare/v0.6.2...v0.7.0
[0.6.2]: https://github.com/ZVN-DEV/powdb/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/ZVN-DEV/powdb/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/ZVN-DEV/powdb/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/ZVN-DEV/powdb/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/ZVN-DEV/powdb/compare/v0.4.9...v0.5.0
[0.4.9]: https://github.com/ZVN-DEV/powdb/compare/v0.4.8...v0.4.9
[0.4.8]: https://github.com/ZVN-DEV/powdb/compare/v0.4.7...v0.4.8
[0.4.7]: https://github.com/ZVN-DEV/powdb/compare/v0.4.6...v0.4.7
[0.4.6]: https://github.com/ZVN-DEV/powdb/compare/v0.4.5...v0.4.6
[0.4.5]: https://github.com/ZVN-DEV/powdb/compare/v0.4.4...v0.4.5
[0.4.4]: https://github.com/ZVN-DEV/powdb/compare/v0.4.3...v0.4.4
[0.4.3]: https://github.com/ZVN-DEV/powdb/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/ZVN-DEV/powdb/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/ZVN-DEV/powdb/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/ZVN-DEV/powdb/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/ZVN-DEV/powdb/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/ZVN-DEV/powdb/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/ZVN-DEV/powdb/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/ZVN-DEV/powdb/compare/v0.1.3...v0.2.0
[0.1.2]: https://github.com/ZVN-DEV/powdb/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/ZVN-DEV/powdb/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/ZVN-DEV/powdb/releases/tag/v0.1.0
