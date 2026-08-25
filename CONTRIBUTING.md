# Contributing to PowDB

## Prerequisites

- Rust 1.93 or newer (MSRV is `1.93`, edition 2021; enforced by the `msrv-consistency` CI check)
- A C toolchain and `cmake`. `cargo build --workspace` compiles `powdb-server` and `powdb-cli`, which reach TLS through `tokio-rustls` -> `aws-lc-sys`, and there is currently no feature to opt out. Without cmake the very first build dies inside `aws-lc-sys`. (The engine libraries alone need no C: `cargo build -p powdb`.)
- Docker + Docker Compose (optional, for running wide benchmarks against Postgres/MySQL)

## Quick Start

```bash
git clone https://github.com/ZVN-DEV/powdb.git
cd powdb
git config core.hooksPath .githooks   # enable the fast pre-commit gate (fmt + version consistency)
cargo build --workspace
cargo test --workspace
```

### Optional: host-CPU-tuned local builds

The checked-in `.cargo/config.toml` sets no rustflags, so default builds are
portable. To opt in to host-specific instructions (NEON/AVX, faster WAL CRC)
for local benchmarking, set `CARGO_BUILD_RUSTFLAGS="-C target-cpu=native"` in
your shell, or add the flag to an untracked personal config such as
`~/.cargo/config.toml`. Never commit `target-cpu=native`: it SIGILLs on CI's
shared runners.

### Watch mode

For an edit-compile loop, `cargo install bacon && bacon` (or `cargo watch -x "check --workspace"`) rechecks the workspace on every save.

## Build Commands

```bash
cargo build --workspace           # debug build
cargo build --release --workspace # release build
cargo test --workspace            # run all tests (roughly 35-45 minutes on an M-series laptop; iterate with `cargo test -p <crate>`)
cargo bench -p powdb-bench        # criterion benchmarks (24 benches, 22 gated workloads; ~5 min of measurement)
cargo run --release -p powdb-compare  # wide bench vs SQLite + Postgres (add --features mysql for MySQL)
```

## Project Structure

```
crates/storage   # slotted pages, B+ tree, WAL, buffer pool, catalog
crates/query     # lexer, parser, planner, executor (Engine)
crates/auth      # user store, roles, argon2id password hashing
crates/backup    # offline backup/restore (full, incremental, PITR)
crates/sync      # embedded-sync substrate (retained replication-unit log; experimental)
crates/server    # Tokio TCP server + binary wire protocol
crates/powdb     # embedded facade: in-process Rust API over the engine
crates/cli       # rustyline REPL (embedded + remote mode)
crates/bench     # criterion benchmarks + regression gate (publish = false)
crates/compare   # wide benchmark comparisons vs other databases (publish = false)
crates/oracle    # differential correctness oracle: same fixture and query through
                 # PowQL, the SQL frontend, and SQLite, full result sets compared
                 # (publish = false)
bindings/node    # @zvndev/powdb-embedded: in-process Node addon (napi-rs)
clients/ts       # TypeScript client + demo
clients/sync     # sync client helpers (experimental)
```

## What belongs in this repo

This is a public repository. Internal planning, strategy, roadmaps, sprint and
mission plans, audits, session notes, and agent-orchestration output are kept
private and are git-ignored (see the "Internal-only docs and agent tooling"
block in `.gitignore`); do not commit them here.

Public materials (README, CHANGELOG, docs, commit messages, PR descriptions)
describe changes on their own technical merit. They must never name or imply an
internal product, customer, or dogfood project as the source of a change.
Instead of "product X asked for this" or "reported by the X integration", write
it neutrally: "an ORM integration", "a cross-engine benchmark", "reported by an
integration test". A CI job (`internal-content-guard`) enforces both rules and
fails the build if an internal-only path gets re-tracked or an internal codename
appears in a public file.

## Development Workflow

**Never push directly to `main`.** Every change (docs, CI tweaks, version bumps, "trivial" fixes, all of it) goes through a pull request.

1. Create a branch from `main` (kebab-case)
2. Make changes, then run `scripts/quality fast` for the local fast gate
3. Before review, run `bash scripts/quality.sh --full` or document any unavailable optional security tools
4. Run `cargo run --release -p powdb-compare` to check for performance regressions when the change can affect query/storage performance
5. Push the branch and open a PR against `main` using the template in `.github/pull_request_template.md`

### Branch protection on `main`

- PRs are required (no direct pushes)
- There is exactly **one** required status check: `ci-success`. It is an aggregator that
  `needs:` every other job in `ci.yml` and fails if any of them did not succeed, so the
  whole matrix below gates merges rather than a hand-picked subset. `scripts/ci/check-ci-success-needs.sh`
  (the `ci-needs-completeness` job) fails the build if a job is ever defined without being
  required, or required without being defined.
- Force-push is rejected by branch protection

Admin bypass exists for break-glass scenarios (security patches, recovering from a broken state). **Do not use it for routine work**: routine work goes through PRs even when bypass is technically available.

### If you push to main by accident

1. Revert the commit on `main` with a forward `git revert`, not a force-push (force-push to `main` is blocked anyway).
2. Push the revert directly. The revert restores the invariant; that's why bypass exists.
3. Re-introduce the work on a branch and open a PR.

### For AI assistants working in this repo

- The branch + PR rule is not a default you can override. The user does not need to repeat it per session.
- "Implement X" never includes "push to main." Implementation always lands on a branch with a PR.
- Treat the branch-protection bypass capability as if it doesn't exist for your account.

## CI Checks

PRs must pass every job below (see `.github/workflows/ci.yml`). All of them are reachable
through the single `ci-success` required check, and this list is asserted against `ci.yml`
by `scripts/ci/check-ci-success-needs.sh`, so it cannot fall behind the workflow:

- **`lint-test`**: clippy, `cargo fmt --check`, the full workspace test suite, and the `testing`-feature dual-path equivalence suite, on a two-OS matrix (`ubuntu-24.04`, `macos-latest`)
- **`miri`**: undefined-behavior check on the non-mmap `powdb-storage` modules, sharded three ways (`btree`, `row-page`, `json-types-view`). The shard map lives in `scripts/ci/miri-shards.sh`, which fails if the shards stop covering the canonical filter set or stop matching `ci.yml`
- **`miri-query`**: undefined-behavior check on `powdb-query`'s compiled predicate module (`executor::compiled`), the byte-offset evaluation where an out-of-bounds read would hide; fails if the test filter matches nothing
- **`asan`**: AddressSanitizer over the workspace with `-Zbuild-std`
- **`msrv-consistency`**: the declared MSRV agrees across `Cargo.toml`, `README.md`, and `Dockerfile`
- **`msrv-build`**: the declared MSRV toolchain actually builds the workspace (`cargo +<msrv> check --workspace --locked`)
- **`examples-smoke`**: terraform validate, compose config, a `scripts/dev.sh` up/down cycle, and the runnable examples
- **`version-consistency`**: `scripts/check-version-consistency.sh` prevents Rust/TypeScript/lockfile/docs/changelog/release-doc drift
- **`ts-client`**: TypeScript client installs with `pnpm --frozen-lockfile`, builds, and runs pure plus server-backed tests
- **`node-addon`**: the embedded Node addon (`bindings/node`) builds and its tests pass
- **`embedded-sync-js`**: `@zvndev/powdb-sync` builds and passes its unit, native and end-to-end tests
- **`cross-version-compat`**: on-disk format compatibility in both directions against the REAL released binaries, not fixtures this repo wrote (`scripts/ci/cross-version-compat.sh`)
- **`fuzz-corpus-replay`**: deterministic single-pass replay of the checked-in fuzz corpus, so a reintroduced crash fails on the first PR rather than on some future night
- **`release-profile-suites`**: the corruption and wire-corpus suites under the shipped `panic = "abort"` profile
- **`bench-gate-selftest`**: proves the benchmark regression gate can still fail
- **`testing-feature-guard`**: resolves every shipped artifact's normal and build feature graph and refuses `powdb-query/testing` (test-only executor instrumentation); `scripts/ci/testing-feature-guard.sh --selftest` proves the detector fires
- **`missing-docs-ratchet`**: each published library crate's count of undocumented public items must equal `scripts/ci/missing-docs-baseline.txt`, so public-API docs only ever tighten; `scripts/ci/missing-docs-ratchet.sh --selftest` proves the counter can see gaps
- **`ci-needs-completeness`**: every job defined in `ci.yml` is required by `ci-success`, and every `needs:` entry names a job that exists
- **`secret-scan`**: gitleaks, low-noise, with a documented placeholder allowlist
- **`audit`**: `cargo audit` against the advisory database
- **`deny`**: `cargo deny` for licenses, banned crates and source registries
- **`internal-content-guard`**: keeps internal planning and dogfood content out of the public repo
- **`ci-success`**: the aggregator described above, and the only check branch protection requires

The criterion benchmark suite (`.github/workflows/bench.yml`) is **manual-only** (`workflow_dispatch`) and is *not* a required PR gate, because shared-runner noise makes it unreliable as a blocking check. Run the regression gate locally instead (below).

## Benchmark Regression Gate

The criterion gate compares each workload's median against baselines in `crates/bench/baseline/main.json`. Thresholds vary by workload (7-20%). Run it locally with `cargo bench -p powdb-bench && cargo run -p powdb-bench --bin compare`, or on demand in CI with `gh workflow run bench.yml`.

If you intentionally change performance characteristics:
```bash
./scripts/update-bench-baseline.sh
git add crates/bench/baseline/main.json
git commit -m "bench: rebaseline after <change> (<workload>: <delta>)"
```

## Code Style

- Standard `rustfmt` formatting (enforced by CI)
- All clippy warnings are errors in CI
- Prefer `?` for error propagation over manual matching
- No `unwrap()` in new code: use proper error handling

## Architecture Notes

PowDB uses PowQL, a custom query language (not SQL). The query pipeline is:

```
Input → Lexer → Parser → Planner → Executor → Result
                                      ↓
                              Storage Engine (B+ tree, heap files, WAL)
```

The planner has no catalog access (it's a pure function). Plan lowering (e.g., `RangeScan` → `Filter(SeqScan)` for unindexed columns) happens at execution time in the executor.

## License

MIT

### Local quality gate

TypeScript checks use `npm exec --package=pnpm@10.29.3` so contributors do not need a globally activated pnpm/Corepack setup.


Before submitting infrastructure, release, or client changes, run the shared
local gate that mirrors CI where practical:

```bash
scripts/quality help
scripts/quality --fast   # quick local smoke
scripts/quality          # default fmt/check/clippy/test gate
```

For release prep, also run `bash scripts/check-version-consistency.sh` so the
Rust workspace version, publishable inter-crate dependency pins, TypeScript
client metadata, changelog, and release notes stay in lockstep.

