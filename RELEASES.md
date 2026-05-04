# PowDB Release Targets

Every PowDB release ships to the following registries and platforms.
When cutting a release, follow the checklist at the bottom.

## Registries

| Target | Package | Registry URL |
|--------|---------|-------------|
| **crates.io** | `powdb-storage` | https://crates.io/crates/powdb-storage |
| **crates.io** | `powdb-query` | https://crates.io/crates/powdb-query |
| **crates.io** | `powdb-server` | https://crates.io/crates/powdb-server |
| **crates.io** | `powdb-cli` | https://crates.io/crates/powdb-cli |
| **npm** | `@zvndev/powdb-client` | https://www.npmjs.com/package/@zvndev/powdb-client |

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
2. `powdb-query` (depends on storage)
3. `powdb-server` (depends on storage + query)
4. `powdb-cli` (depends on storage + query + server)

Non-publishable crates (`publish = false`): `powdb-compare`, `powdb-bench`, `powdb-query-fuzz`.

## Release Checklist

```
[ ] Update workspace version in root Cargo.toml
[ ] Update inter-crate dep versions in query/server/cli Cargo.toml
[ ] Update clients/ts/package.json version
[ ] Update CHANGELOG.md
[ ] Commit: "chore: release vX.Y.Z"
[ ] cargo publish -p powdb-storage
[ ] cargo publish -p powdb-query
[ ] cargo publish -p powdb-server
[ ] cargo publish -p powdb-cli
[ ] cd clients/ts && npm publish --access public
[ ] git tag vX.Y.Z && git push origin vX.Y.Z
[ ] Verify GitHub Release workflow creates binaries
```
