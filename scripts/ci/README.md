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
| `miri-shards.sh` | the sharded miri matrix still covers every module in scope, its filters do not overlap, its shard names match `ci.yml`, and (via `--check-listing`, against the real test list) every in-scope test is selected by exactly one shard | drop a filter from a shard, delete a shard from the `ci.yml` matrix, or add a `btree::tests::` test whose name matches no prefix; `--selftest` does the last one on a fixture |
| `changelog-section.sh` | the GitHub Release body is the curated CHANGELOG entry, and is never empty | ask for a version with no entry (`changelog-section.sh 9.9.9`), or empty the section under `## [X.Y.Z]` |
| `check-nightly-pin.sh` | pinned nightly toolchains do not silently age out, in every shape a pin takes (`toolchain:` value, `FUZZ_TOOLCHAIN` env, `cargo +nightly-...`) | `MAX_PIN_AGE_DAYS=1`; `--selftest` proves it on a fixture and that a date in a comment is not counted |
| `semver-gate.sh` | cargo-semver-checks actually ran lints, and can still fail; refuses "0 checks" on a patch bump and a run that examined no crate | `SEMVER_CHECKS=true bash scripts/ci/semver-gate.sh --selftest` (a checker that only ever passes) |
| `semver-advisory.sh` | nothing by design: it is the advisory forced-minor pass. Its `--selftest` is the gate, and what it gates is that the step can never block a release while still writing the breaking-change list to the job summary | swap the script's `set +e` for `set -e` (that is verbatim the bug it was extracted from) and run `bash scripts/ci/semver-advisory.sh --selftest`: case 1 fails because the step exited 1 with an empty summary. Deleting the line that writes `${found}` into the summary fails it too |
| `check-dependabot-coverage.sh` | every tracked dependency manifest is watched by `.github/dependabot.yml`, and no entry watches a directory with no manifest | delete an entry from `dependabot.yml`, or add one for a directory that has none |
| `strip-empty-unreleased.sh` | the published npm CHANGELOGs lose an EMPTY `## Unreleased` heading and keep a non-empty one | break the `nonblank` branch so a real section is dropped; `--selftest` catches it |
| `testing-feature-guard.sh` | no shipped artifact resolves `powdb-query/testing` | resolve the feature from a published crate's manifest |
| `missing-docs-ratchet.sh` | the `missing_docs` count never grows (`--color never` is load-bearing: this greps cargo's own output) | add an undocumented public item |
| `release-channel.sh` | a release tag classifies into exactly one channel, or the release fails | `release-channel.sh 0.27` or any non-SemVer shape, leading zeros included |
| `check-tool-pins.sh` | every `cargo install` in a workflow carries `--version` and `--locked`, and a tool installed by two workflows is pinned to the same version in both | drop `--version` from any install line, or set `CARGO_FUZZ_VERSION` to a different value in ci.yml than in fuzz.yml; `--selftest` does both on fixtures |
| `internal-content-guard.sh` | no tracked file lives under an internal-only path, and no public doc or source matches the private publication denylist; refuses a run in which either half did not actually inspect anything | `PUBLICATION_DENYLIST_REGEX='(' bash scripts/ci/internal-content-guard.sh` (a regex git cannot compile: it used to print "denylist checked"), or run it from a directory that is not a git repo |

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
bash scripts/ci/check-nightly-pin.sh --selftest
bash scripts/ci/check-dependabot-coverage.sh
bash scripts/ci/miri-shards.sh --check
bash scripts/ci/miri-shards.sh --selftest
bash scripts/ci/strip-empty-unreleased.sh --selftest
bash scripts/ci/release-channel.sh --selftest
bash scripts/ci/internal-content-guard.sh --selftest
bash scripts/ci/check-tool-pins.sh --selftest
bash scripts/ci/check-ci-success-needs.sh --selftest
bash scripts/ci/semver-advisory.sh --selftest
bash scripts/ci/changelog-section.sh 0.25.0   # prints the release body
```

`semver-gate.sh` is the one that needs a tool CI installs for it. Point it at a
local cargo-semver-checks and the selftest runs anywhere:

```bash
SEMVER_CHECKS="cargo semver-checks" bash scripts/ci/semver-gate.sh --selftest
```

`semver-advisory.sh --selftest` needs no tool at all: it drives the script
against stub checkers that fail, pass and crash, and asserts each time that the
step exited 0 and that the summary says what that case should say.

```bash
bash scripts/ci/semver-advisory.sh --selftest
```

`--selftest` is not decoration. Several of these scripts are the only thing
standing between a silent regression and a green release, so each one that has
a selftest runs it as its own workflow step BEFORE the real check, and the
selftest is written to fail if the detector stops detecting.

`cross-version-compat.sh` caches the downloaded release binaries under
`target/compat-bins/`, so only the first run needs the network. It also derives
its version list from `gh release list` rather than a literal (the literal is
what went three minors stale), and validates whatever list it ends up with, so
`--print-plan` shows exactly what a run would test:

```bash
bash scripts/ci/cross-version-compat.sh --print-plan
```
