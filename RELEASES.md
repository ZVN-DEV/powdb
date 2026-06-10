# PowDB Release Targets

Every PowDB release ships to the following registries and platforms.
When cutting a release, follow the checklist at the bottom.

> **Current release: v0.4.7** (all six crates live on crates.io).
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
| **crates.io** | `powdb-cli` | https://crates.io/crates/powdb-cli |
| **npm** | `@zvndev/powdb-client` | https://www.npmjs.com/package/@zvndev/powdb-client |
| **ghcr.io** | `ghcr.io/zvn-dev/powdb` (Docker image, `latest` + `vX.Y.Z` tags) | https://github.com/orgs/ZVN-DEV/packages |

## GitHub Releases

| Artifact | Platforms |
|----------|-----------|
| `powdb-cli-linux-x86_64` | Linux x86_64 |
| `powdb-server-linux-x86_64` | Linux x86_64 |
| `powdb-cli-macos-aarch64` | macOS ARM64 |
| `powdb-server-macos-aarch64` | macOS ARM64 |

Binary artifacts are built automatically by `.github/workflows/release.yml`
when a `v*` tag is pushed.

## Crate Publish Order

Inter-crate dependencies require publishing in this order:

1. `powdb-storage` (no inter-crate deps)
2. `powdb-auth` (no inter-crate deps)
3. `powdb-query` (depends on storage)
4. `powdb-backup` (depends on storage + query)
5. `powdb-server` (depends on storage + query + auth)
6. `powdb-cli` (depends on storage + query + server + backup + auth)

Non-publishable crates (`publish = false`): `powdb-compare`, `powdb-bench`, `powdb-query-fuzz`.

## Release Checklist

```
[ ] Update workspace version in root Cargo.toml
[ ] Update inter-crate dep versions in query/backup/server/cli Cargo.toml
[ ] Update clients/ts/package.json version
[ ] Update CHANGELOG.md
[ ] Commit: "chore: release vX.Y.Z"
[ ] cargo publish -p powdb-storage
[ ] cargo publish -p powdb-auth
[ ] cargo publish -p powdb-query
[ ] cargo publish -p powdb-backup
[ ] cargo publish -p powdb-server
[ ] cargo publish -p powdb-cli
[ ] cd clients/ts && npm publish --access public
[ ] git tag vX.Y.Z && git push origin vX.Y.Z
[ ] Verify GitHub Release workflow creates binaries
[ ] Smoke-test the installed crates.io binary: run the README's documented
    PowQL flow, then kill -9 the server and restart to confirm WAL replay
    recovers the data (v0.4.1–v0.4.3 shipped data-loss P0s that the
    pre-publish gates missed because none exercised a real crash + restart)
```
