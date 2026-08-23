# scripts/ci/

The gates that CI runs, as scripts rather than inline YAML.

Every one of these was written because the corresponding check either did not
exist or could not fail. Keeping the logic in a script (and the workflow step
down to one `bash scripts/ci/<name>.sh` line) means the gate can be run and,
more importantly, *broken on purpose* locally. A gate nobody has watched fail
is a gate nobody knows works: the `miri` job filtered on a module that does not
exist and passed for months.

| script | what it gates | how to make it fail |
|---|---|---|
| `cross-version-compat.sh` | on-disk format compatibility against the real released binaries, forward and both downgrade directions | `POWDB_COMPAT_FLOOR=v0.19.1` (a release that *does* support the activated catalog, so the refusal leg has nothing to refuse) |
| `fuzz-corpus-replay.sh` | deterministic replay of the checked-in fuzz corpus; refuses to "pass" a target with no inputs | empty a `crates/query/fuzz/seeds/<target>/` directory, or add a `[[bin]]` to `fuzz/Cargo.toml` without adding it to the replay list |
| `release-profile-suites.sh` | the corruption and wire-corpus suites against the SHIPPED `panic = "abort"` binary | remove `panic = "abort"` from `[profile.release]`, name a nonexistent test target, or `kill -9` the server mid-corpus |
| `bench-gate-selftest.sh` | that every verdict of the bench comparator is reachable | make `env_mismatches` return `vec![]`, or `control_threshold_for` return infinity |
| `check-ci-success-needs.sh` | every `ci.yml` job is in `ci-success`'s `needs:`, and every job is named in CONTRIBUTING.md's CI Checks list | delete a job from the `needs:` list, or delete a bullet from that list |
| `miri-shards.sh` | the sharded miri matrix still covers every canonical filter, and its shard names match `ci.yml` | drop a filter from a shard, or delete a shard from the `ci.yml` matrix |
| `changelog-section.sh` | the GitHub Release body is the curated CHANGELOG entry, and is never empty | ask for a version with no entry (`changelog-section.sh 9.9.9`), or empty the section under `## [X.Y.Z]` |
| `check-nightly-pin.sh` | pinned nightly toolchains do not silently age out | `MAX_PIN_AGE_DAYS=1` |

## Running them locally

All of them work from a normal checkout, with no CI-only environment:

```bash
cargo build --release -p powdb-cli
bash scripts/ci/cross-version-compat.sh      # downloads released binaries (network)
bash scripts/ci/fuzz-corpus-replay.sh        # needs nightly + cargo-fuzz
bash scripts/ci/release-profile-suites.sh    # ~10 min, builds release
bash scripts/ci/bench-gate-selftest.sh       # seconds, no benchmarking
bash scripts/ci/check-ci-success-needs.sh
bash scripts/ci/check-nightly-pin.sh
bash scripts/ci/miri-shards.sh --check
bash scripts/ci/changelog-section.sh 0.25.0   # prints the release body
```

`cross-version-compat.sh` caches the downloaded release binaries under
`target/compat-bins/`, so only the first run needs the network. It also derives
its version list from `gh release list` rather than a literal (the literal is
what went three minors stale), and validates whatever list it ends up with, so
`--print-plan` shows exactly what a run would test:

```bash
bash scripts/ci/cross-version-compat.sh --print-plan
```
