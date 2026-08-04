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
| `check-ci-success-needs.sh` | every `ci.yml` job is in `ci-success`'s `needs:` | delete a job from the `needs:` list |
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
```

`cross-version-compat.sh` caches the downloaded release binaries under
`target/compat-bins/`, so only the first run needs the network.
