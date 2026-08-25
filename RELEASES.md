# PowDB Release Targets

Every PowDB release ships to the following registries and platforms.
When cutting a release, follow the checklist at the bottom.

> **Current release: v0.26.0.** The audit round: the findings from the 2026-08-22 gold-standard audit. A duplicate key on a unique expression index reported wire error class `0` (`internal`) instead of `8` (`constraint_violation`), telling drivers the server had faulted when a caller had simply inserted a duplicate. The cause was structural: storage errors crossed the crate boundary as plain strings, so the server recovered their type by searching the rendered message for a column-level phrase an expression-index violation never contains. Classification now runs off a typed `StorageErrorKind` through an exhaustive match, so a new storage error variant is a compile error rather than a silent `internal`. Nine `.expect()` calls on the wire decode path became typed errors: each was an invariant assertion, but under `panic = "abort"` any one firing would abort the process and every connected client. The PowQL "did you mean" suggestion was computed from the first token of the statement rather than the token that failed, so every query beginning with `User` suggested `upsert`; suggestions now come from the offending token, cover pipeline keywords, and draw on 169 of 87,332 dictionary words instead of 2,329. `@zvndev/powdb-embedded` errors now carry a stable `code`, so an embedded host can distinguish `poisoned` and `open_panicked` from an ordinary `query_failed` and recycle the handle instead of retrying; this is breaking for anyone reading `err.code`, which previously held napi's `GenericFailure` on every error. `crates/server/src/handler.rs` (8,217 lines, holding seven of the codebase's eight lint suppressions) became a seven-file module directory with those suppressions retired rather than relocated. Cross-crate plumbing modules in `powdb-storage` and `powdb-query` are now `#[doc(hidden)]`, which takes the on-disk row encoding out of the published API surface without restricting access.
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
| **crates.io** | `powdb` (embedded facade: in-process Rust API) | https://crates.io/crates/powdb |
| **crates.io** | `powdb-cli` | https://crates.io/crates/powdb-cli |
| **crates.io** | `powdb-sync` (experimental, the embedded-sync substrate) | https://crates.io/crates/powdb-sync |
| **npm** | `@zvndev/powdb-client` | https://www.npmjs.com/package/@zvndev/powdb-client |
| **npm** | `@zvndev/powdb-sync` (experimental sync orchestration; bootstrapped at 0.24.0, and published on every `v*` tag by `release.yml` since) | https://www.npmjs.com/package/@zvndev/powdb-sync |
| **npm** | `@zvndev/powdb-embedded` (in-process Node addon; prebuilt binaries for macOS arm64, Linux x64-gnu, Linux arm64-gnu; no source fallback, other targets are unsupported) | https://www.npmjs.com/package/@zvndev/powdb-embedded |
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
4. `powdb-sync` (experimental; depends on storage)
5. `powdb-backup` (depends on storage + sync; query is dev-only)
6. `powdb-server` (depends on storage + query + auth + sync)
7. `powdb` (embedded facade; depends on storage + query + sync)
8. `powdb-cli` (depends on storage + query + server + backup + auth + sync)

Non-publishable workspace crates (`publish = false`): `powdb-bench`, `powdb-compare`, `powdb-oracle`.
Those three plus the eight above are the whole workspace: `cargo metadata --no-deps` lists
eleven packages. The fuzz crate `powdb-query-fuzz` is **not** among them; it lives under
`crates/query/fuzz` with its own `[workspace]` table, so `crates/*` never picks it up and it is
built only by `cargo fuzz`.

## Publishing is token-less (Trusted Publishing / OIDC)

Both registries publish from CI with **no stored token**: neither
`CARGO_REGISTRY_TOKEN` nor an npm token exists anymore. The workflows mint
short-lived credentials from their GitHub OIDC identity. This is configured once
per package/crate on the registry websites; see
[`docs/ci/trusted-publishing.md`](docs/ci/trusted-publishing.md) for the
one-time setup and the reusable standard.

- **crates.io**: `publish.yml` (manual `workflow_dispatch`, `dry_run=false`),
  authenticated via `rust-lang/crates-io-auth-action`. Kept manual because
  publishing to crates.io is irreversible.
- **npm (`@zvndev/powdb-client`)**: published automatically by `release.yml`
  on a `v*` tag push, with provenance. No manual `npm publish`, no token to make.
- **npm (`@zvndev/powdb-embedded`)**: published by `publish-node-addon.yml`
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
[ ] Update clients/sync/package.json version and its two exact peer pins
[ ] Update bindings/node/Cargo.toml version, then regenerate its Cargo.lock
[ ] Regenerate crates/query/fuzz/Cargo.lock
[ ] Move CHANGELOG.md notes from Unreleased to the dated version entry
[ ] Update both the Next release and Current release lines in RELEASES.md
[ ] Update doc version strings: --version pins and CLI banner transcripts in
    README.md, docs/getting-started.md, docs/powdb-vs-sqlite.md
[ ] Run bash scripts/check-version-consistency.sh
[ ] Run bash scripts/smoke-package.sh (npm pack/import smoke + cargo package list)

Note on the three lockfiles: bindings/node and crates/query/fuzz are detached
workspaces, so `cargo build --workspace` never regenerates them. All three are
gated against the workspace version, so bumping only the root Cargo.toml turns
the release PR red. The failure names the exact file and expected value.
[ ] Commit: "chore: release vX.Y.Z", open a PR, merge it

TAG BEFORE PUBLISHING. publish.yml refuses to publish unless the tag vX.Y.Z
already exists AND points at the exact commit being published, and it must be
dispatched on the tag. That guard is what stops an arbitrary branch shipping
under a released version number, so the crates cannot go first.

[ ] git tag -a vX.Y.Z -m "..." && git push origin vX.Y.Z
    Pushing the tag triggers release.yml, which builds the binaries, publishes
    the multi-arch Docker image, and publishes @zvndev/powdb-client to npm
    token-less via OIDC. No manual npm publish for the client.
[ ] Publish the crates, dispatched ON THE TAG, in dependency order (the
    workflow already orders them: storage, auth, query, sync, backup, server,
    powdb, cli):

      gh workflow run publish.yml --ref vX.Y.Z -f version=X.Y.Z -f dry_run=false

    `dry_run` defaults to TRUE on purpose, so it must be spelled out or nothing
    publishes. A dry run is NOT a useful rehearsal here: it fails by design for
    every crate that depends on a workspace version not yet on crates.io.
    Either way the workflow first runs cargo-semver-checks against the
    published crates.io baselines and refuses to publish an API change bigger
    than the version bump allows (the point-release-over-a-break hazard). If
    it fires on a real release, the bump is wrong: raise the version, do not
    bypass the check.
[ ] Publish the embedded Node addon: run publish-node-addon.yml with
    dry_run=true to validate the full platform matrix, then re-run with
    dry_run=false to publish @zvndev/powdb-embedded (token-less, provenance).
    Unlike publish.yml, this dry run IS meaningful: it packs every platform and
    needs no OIDC setup. Do this BEFORE the smoke, which installs the addon.
[ ] Smoke-test the LIVE registries: run post-publish-smoke.yml with the
    released version (`gh workflow run post-publish-smoke.yml -f version=X.Y.Z`).
    It cargo-installs powdb-cli + powdb-server from crates.io and reruns the
    durability smoke (README PowQL flow + kill -9/restart WAL replay; the gate
    v0.4.1-v0.4.3 lacked), then npm-installs @zvndev/powdb-client and
    @zvndev/powdb-embedded and exercises both
[ ] Verify each registry directly rather than trusting workflow exit codes:
    crates.io versions, `gh release view vX.Y.Z`, the ghcr tag list, and
    `npm view <pkg> version` for each npm package
```

A brand-new package or crate name cannot use Trusted Publishing for its FIRST
publish, because the registry only lets you configure a trusted publisher on a
name that already exists. Bootstrap it once by hand, then configure. See
docs/ci/trusted-publishing.md.

## Yank / Rollback Runbook

For when a shipped release turns out to carry a data-loss, corruption, or
security bug. Precedent: v0.4.1–v0.4.3 (crash-recovery data loss), yanked and
noted at the top of this file.

**Decide first: yank, or fix forward?** Yank only when installing the version
actively harms users (data loss, corruption, unopenable data dirs, security).
For ordinary bugs, ship the fix as a patch release and skip this section — a
yank breaks every lockfile that pins the version, which is its own harm.

The steps, in order (stop the bleeding at the installs first, then annotate):

```
[ ] crates.io — yank ALL EIGHT crates at the bad version, not just the buggy
    one (inter-crate deps pin the workspace version, so a partial yank
    strands the rest):

      for c in powdb-storage powdb-auth powdb-query powdb-sync powdb-backup \
               powdb-server powdb powdb-cli; do
        cargo yank --version X.Y.Z "$c"
      done

    Yanking is NOT covered by OIDC trusted publishing: it needs `cargo login`
    with a real crates.io token scoped to yank. Mint one for the operation and
    revoke it immediately after (standing policy: no stored registry tokens).
    Yanked versions stay downloadable for existing lockfiles; new resolution
    skips them. `cargo yank --undo` reverses a mistaken yank.

[ ] npm — you cannot yank, and unpublish is restricted after 72 hours.
    Deprecate instead, per package:

      npm deprecate @zvndev/powdb-client@X.Y.Z  "DATA-LOSS BUG — use X.Y.Z+1; see CHANGELOG"
      npm deprecate @zvndev/powdb-embedded@X.Y.Z "..."
      npm deprecate @zvndev/powdb-sync@X.Y.Z     "..."

    Like yank, deprecation needs an authenticated npm session (OIDC covers
    publish only). Mint, use, revoke.

[ ] Docker — ghcr tags cannot be yanked. Repoint `latest` at the previous
    good release so new pulls stop getting the bad build:

      docker pull ghcr.io/zvn-dev/powdb:vPREV
      docker tag  ghcr.io/zvn-dev/powdb:vPREV ghcr.io/zvn-dev/powdb:latest
      docker push ghcr.io/zvn-dev/powdb:latest

    Leave the bad `vX.Y.Z` tag in place (deleting it breaks reproducibility
    for anyone diagnosing the incident) — the release-notes warning below is
    what marks it.

[ ] GitHub Release — edit the vX.Y.Z release notes to LEAD with a warning
    block naming the bug, the affected surface, and the fixed version. Mark
    the release as a pre-release (`gh release edit vX.Y.Z --prerelease`) so it
    loses the "Latest" badge. NEVER delete the tag: publish.yml's tag-match
    guard and the cross-version compat CI leg both depend on released tags
    being immutable history.

[ ] Fly example (if it was deployed): redeploy the previous good version.

[ ] Annotate: add the version to the yanked-versions note at the top of this
    file (the v0.4.x block is the template), and give CHANGELOG.md's entry
    for the bad version a **YANKED** header line stating why.

[ ] Ship the fixed release. The fix release's smoke run
    (post-publish-smoke.yml) is what closes the incident — verify each
    registry directly, same as a normal release.

[ ] Verify the rollback took: `cargo info <crate>` / the crates.io page shows
    the version yanked, `npm view <pkg>@X.Y.Z deprecated` prints the message,
    and `docker pull ghcr.io/zvn-dev/powdb:latest` resolves to the previous
    good digest.
```

What NOT to do: never `npm unpublish` a version something depends on, never
delete git tags or GitHub Releases, never force-push over a release commit,
and never reuse a version number — the fix is always a NEW version, even if
the bad one was live for five minutes.
