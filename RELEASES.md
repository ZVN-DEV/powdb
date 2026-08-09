# PowDB Release Targets

Every PowDB release ships to the following registries and platforms.
When cutting a release, follow the checklist at the bottom.

> **Current release: v0.23.0.** The correctness round. Four defects were reproduced before being fixed and verified not to reproduce after, two of them silent data corruption. WAL replay duplicated a relocating UPDATE: the page LSN was never stamped on the live UPDATE path, so the redo guard could not fire and every Update record re-applied on every recovery, which is safe only while UPDATE is idempotent, and a row grown past its page relocates with a fresh RowId. 200 rows plus a checkpoint plus one growing update recovered 261 rows with an id present twice. A missing heap file silently reinitialized the database: any `NotFound` during open was read as "no database here" and renamed a fresh empty catalog over the real one, taking `catalog.bin` from 92 bytes to 14 and orphaning a second intact table. `order ... desc limit N` evicted the wrong tied row and could return a different row *set* than the first N rows of the same unlimited query, and `distinct` was applied after `limit` instead of before it. Behavior changes: in SQL, double quotes now delimit an identifier rather than a string, so `SELECT "name" FROM Author` returns the column instead of the literal text once per row, and a quoted identifier is never a keyword; `= NULL` and `<> NULL` now match nothing in SQL as they do in every other engine, while PowQL keeps its documented `x = null` convenience; and `POWDB_TLS_CA` and `POWDB_TLS_SERVER_NAME` now imply TLS in the CLI, which previously connected in cleartext while the operator had every reason to believe otherwise. The differential oracle gained the shapes that would have caught this round, including one that found the `= NULL` divergence on its first run.
> **v0.4.1, v0.4.2, and v0.4.3 are yanked** for crash-recovery data-loss bugs;
> 0.4.4 fixed them and added a standing durability regression suite. See
> `CHANGELOG.md`.

## Registries

| Target | Package | Registry URL |
|--------|---------|-------------|
| **crates.io** | `powdb-storage` | https://crates.io/crates/powdb-storage |
| **crates.io** | `powdb-auth` | https://crates.io/crates/powdb-auth |
| **crates.io** | `powdb-query` | https://crates.io/crates/powdb-query |
| **crates.io** | `powdb-backup` | https://crates.io/crates/powdb-backup |
| **crates.io** | `powdb-server` | https://crates.io/crates/powdb-server |
| **crates.io** | `powdb` (embedded facade — in-process Rust API) | https://crates.io/crates/powdb |
| **crates.io** | `powdb-cli` | https://crates.io/crates/powdb-cli |
| **crates.io** | `powdb-sync` (experimental — embedded-sync substrate; the companion npm package `@zvndev/powdb-sync` is **not yet published**) | https://crates.io/crates/powdb-sync |
| **npm** | `@zvndev/powdb-client` | https://www.npmjs.com/package/@zvndev/powdb-client |
| **npm** | `@zvndev/powdb-embedded` (in-process Node addon; prebuilt binaries for macOS arm64, Linux x64-gnu, Linux arm64-gnu — no source fallback, other targets are unsupported) | https://www.npmjs.com/package/@zvndev/powdb-embedded |
| **ghcr.io** | `ghcr.io/zvn-dev/powdb` (Docker image, `latest` + `vX.Y.Z` tags) | https://github.com/orgs/ZVN-DEV/packages |

## GitHub Releases

| Artifact | Platforms |
|----------|-----------|
| `powdb-cli-linux-x86_64` | Linux x86_64 |
| `powdb-server-linux-x86_64` | Linux x86_64 |
| `powdb-cli-macos-aarch64` | macOS ARM64 |
| `powdb-server-macos-aarch64` | macOS ARM64 |

These two platforms (Linux x86_64, macOS ARM64) are the **only** prebuilt
`powdb-cli` / `powdb-server` binaries. Intel macOS and Linux ARM64 have no
prebuilt binary but do build from source (`cargo install` / `cargo build
--release`).

**Windows is not supported and does not build from source.** The heap's
memory-mapped scan path (`crates/storage/src/heap.rs`) uses `libc::mmap` /
`libc::munmap` and `std::os::unix::io::AsRawFd` with no platform gate, so
`cargo check -p powdb-storage --target x86_64-pc-windows-msvc` fails to
compile. This is why `publish-node-addon.yml` also omits the
`x86_64-pc-windows-msvc` addon target. Do not tell Windows users to build from
source; there is nothing for them to build until the mmap path gains a Windows
backend.
Binary artifacts are built automatically by `.github/workflows/release.yml`
when a `v*` tag is pushed.

## Crate Publish Order

Inter-crate dependencies require publishing in this order:

1. `powdb-storage` (no inter-crate deps)
2. `powdb-auth` (no inter-crate deps)
3. `powdb-query` (depends on storage)
4. `powdb-sync` (experimental — depends on storage)
5. `powdb-backup` (depends on storage + sync; query is dev-only)
6. `powdb-server` (depends on storage + query + auth + sync)
7. `powdb` (embedded facade — depends on storage + query + sync)
8. `powdb-cli` (depends on storage + query + server + backup + auth + sync)

Non-publishable crates (`publish = false`): `powdb-compare`, `powdb-bench`, `powdb-query-fuzz`.

## Publishing is token-less (Trusted Publishing / OIDC)

Both registries publish from CI with **no stored token** — neither
`CARGO_REGISTRY_TOKEN` nor an npm token exists anymore. The workflows mint
short-lived credentials from their GitHub OIDC identity. This is configured once
per package/crate on the registry websites; see
[`docs/ci/trusted-publishing.md`](docs/ci/trusted-publishing.md) for the
one-time setup and the reusable standard.

- **crates.io** — `publish.yml` (manual `workflow_dispatch`, `dry_run=false`),
  authenticated via `rust-lang/crates-io-auth-action`. Kept manual because
  publishing to crates.io is irreversible.
- **npm (`@zvndev/powdb-client`)** — published automatically by `release.yml`
  on a `v*` tag push, with provenance. No manual `npm publish`, no token to make.
- **npm (`@zvndev/powdb-embedded`)** — published by `publish-node-addon.yml`
  (manual `workflow_dispatch`). It first builds the native addon on a per-platform
  runner matrix (macOS arm64, Linux x64/arm64; Intel macOS builds from source and
  Windows is deferred, both since the macos-13 runner retired in #149), then
  publishes one fat package bundling all three prebuilt `.node` binaries,
  token-less with provenance. `dry_run=true` (the default) packs every platform
  without publishing. Kept manual because the binary matrix is slow and the
  package is released on demand, not on every `v*` tag.

## Release Checklist

```
[ ] Update workspace version in root Cargo.toml
[ ] Update inter-crate dep versions in query/sync/backup/server/powdb/cli Cargo.toml
[ ] Update clients/ts/package.json version and clients/ts/src/index.ts CLIENT_VERSION
[ ] Update bindings/node/package.json version (@zvndev/powdb-embedded, lockstep)
[ ] Move CHANGELOG.md notes from Unreleased to the dated version entry
[ ] Update both the Next release and Current release lines in RELEASES.md
[ ] Run bash scripts/check-version-consistency.sh
[ ] Run bash scripts/smoke-package.sh (npm pack/import smoke + cargo package list)
[ ] Commit: "chore: release vX.Y.Z"
[ ] cargo publish -p powdb-storage
[ ] cargo publish -p powdb-auth
[ ] cargo publish -p powdb-query
[ ] cargo publish -p powdb-sync   # experimental (depends on storage)
[ ] cargo publish -p powdb-backup
[ ] cargo publish -p powdb-server
[ ] cargo publish -p powdb       # embedded facade (depends on storage + query + sync)
[ ] cargo publish -p powdb-cli
[ ] git tag vX.Y.Z && git push origin vX.Y.Z
[ ] Verify GitHub Release workflow creates binaries AND auto-publishes npm
    (@zvndev/powdb-client) token-less via release.yml — no manual npm publish
[ ] Smoke-test the LIVE registries: run post-publish-smoke.yml with the
    released version (`gh workflow run post-publish-smoke.yml -f version=X.Y.Z`).
    It cargo-installs powdb-cli + powdb-server from crates.io and reruns the
    durability smoke (README PowQL flow + kill -9/restart WAL replay; the gate
    v0.4.1-v0.4.3 lacked), then npm-installs @zvndev/powdb-client and
    @zvndev/powdb-embedded and exercises both
[ ] Publish the embedded Node addon: run publish-node-addon.yml with
    dry_run=true to validate the full platform matrix, then re-run with
    dry_run=false to publish @zvndev/powdb-embedded (token-less, provenance).
    First-ever release of this name needs a one-time bootstrap `npm publish`
    + trusted-publisher config — see docs/ci/trusted-publishing.md
```
