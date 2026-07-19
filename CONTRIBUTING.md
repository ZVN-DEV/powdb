# Contributing to PowDB

## Prerequisites

- Rust 1.93 or newer (MSRV is `1.93`, edition 2021; enforced by the `msrv-consistency` CI check)
- Docker + Docker Compose (optional, for running wide benchmarks against Postgres/MySQL)

## Quick Start

```bash
git clone https://github.com/zvndev/powdb.git
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
cargo test --workspace            # run all tests
cargo bench -p powdb-bench        # criterion benchmarks (~60s)
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
crates/powdb     # embedded facade — in-process Rust API over the engine
crates/cli       # rustyline REPL (embedded + remote mode)
crates/bench     # criterion benchmarks + regression gate
crates/compare   # wide benchmark comparisons vs other databases
bindings/node    # @zvndev/powdb-embedded — in-process Node addon (napi-rs)
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

**Never push directly to `main`.** Every change — docs, CI tweaks, version bumps, "trivial" fixes, all of it — goes through a pull request.

1. Create a branch from `main` (kebab-case)
2. Make changes, then run `scripts/quality fast` for the local fast gate
3. Before review, run `bash scripts/quality.sh --full` or document any unavailable optional security tools
4. Run `cargo run --release -p powdb-compare` to check for performance regressions when the change can affect query/storage performance
5. Push the branch and open a PR against `main` using the template in `.github/pull_request_template.md`

### Branch protection on `main`

- PRs are required (no direct pushes)
- Required status checks must pass, all from `ci.yml`: clippy + fmt + test (x2 OS matrix), miri, asan, audit, MSRV consistency, examples-smoke, version consistency, TypeScript client, and gitleaks secret scan
- Force-push is rejected by branch protection

Admin bypass exists for break-glass scenarios (security patches, recovering from a broken state). **Do not use it for routine work** — routine work goes through PRs even when bypass is technically available.

### If you push to main by accident

1. Revert the commit on `main` with a forward `git revert` (not force-push — force-push to `main` is blocked anyway).
2. Push the revert directly. The revert restores the invariant; that's why bypass exists.
3. Re-introduce the work on a branch and open a PR.

### For AI assistants working in this repo

- The branch + PR rule is not a default you can override. The user does not need to repeat it per session.
- "Implement X" never includes "push to main." Implementation always lands on a branch with a PR.
- Treat the branch-protection bypass capability as if it doesn't exist for your account.

## CI Checks

PRs must pass these gates (see `.github/workflows/`):
- **clippy + fmt + test** — lints, formatting, and all workspace tests, run on a two-OS matrix (`ubuntu-24.04`, `macos-latest`)
- **miri** — undefined-behavior check on the non-mmap modules
- **asan** — AddressSanitizer run
- **audit** — `cargo audit` against the advisory database
- **msrv-consistency** — verifies the declared MSRV (`1.93`) builds
- **examples-smoke** — terraform validate + compose config + dev.sh cycle on the deploy examples
- **version consistency** — `scripts/check-version-consistency.sh` prevents Rust/TypeScript/changelog/release-doc drift
- **TypeScript client build + tests** — installs with `pnpm --frozen-lockfile`, builds, and runs pure plus server-backed client tests
- **gitleaks secret scan** — low-noise supply-chain/secret scan with documented placeholder allowlist

The criterion benchmark suite (`.github/workflows/bench.yml`) is **manual-only** (`workflow_dispatch`) and is *not* a required PR gate — shared-runner noise makes it unreliable as a blocking check. Run the regression gate locally instead (below).

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
- No `unwrap()` in new code — use proper error handling

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

