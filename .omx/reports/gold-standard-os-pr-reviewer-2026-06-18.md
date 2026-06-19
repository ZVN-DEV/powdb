# Kirby — Gold Standard OS PR Reviewer Report — 2026-06-18

## Review summary

Kirby, this PR hardens PowDB's release and CI infrastructure while keeping the change infrastructure-first. The only runtime/client code change is a minimal TypeScript client compile fix found by the new strict TypeScript build gate: socket `data` chunks are normalized to `Buffer` before protocol decoding.

## Changed files to review

- `.github/workflows/ci.yml` — Adds one version consistency job, one TypeScript client build/test job, and one SHA-pinned gitleaks secret-scan job; duplicate integrated job blocks were removed during review.
- `.github/workflows/publish.yml` — Runs shared version checks and package smoke before credentialed publishing; publish order matches `RELEASES.md` (`storage`, `auth`, `query`, `backup`, `server`, `cli`).
- `.github/workflows/release.yml` — Runs package smoke in the tag release test path and keeps release-binary durability smoke.
- `.gitleaks.toml` — Adds a narrow regex-only placeholder allowlist for documented dummy secrets.
- `scripts/check-version-consistency.sh` — Fails on Rust workspace/inter-crate, TS package/`CLIENT_VERSION`, changelog, or release banner drift.
- `scripts/quality`, `scripts/quality.sh`, `scripts/smoke-package.sh` — Adds local quality/package gates that invoke pnpm via `npm exec --package=pnpm@10.29.3` instead of depending on local Corepack keys.
- `scripts/README.md`, `CONTRIBUTING.md`, `README.md`, `RELEASES.md`, `clients/ts/CHANGELOG.md` — Documents local gates, release checklist changes, and TS/workspace lockstep version policy.
- `crates/{backup,cli,query,server}/Cargo.toml` — Aligns publishable inter-crate dependency pins to workspace `0.5.1`.
- `clients/ts/src/index.ts` — Normalizes `string | Buffer` socket chunks to `Buffer` to satisfy current Node typings and preserve decoder input.

## Required check matrix

| Requirement | Command / evidence | Status |
| --- | --- | --- |
| Script syntax | `bash -n scripts/check-version-consistency.sh scripts/quality.sh scripts/quality scripts/smoke-package.sh scripts/smoke-release.sh` | PASS |
| Workflow sanity | Ruby `YAML.load_file` for `ci.yml`, `publish.yml`, `release.yml`; duplicate job assertion for `version-consistency`, `ts-client`, `secret-scan` | PASS |
| Version consistency | `bash scripts/check-version-consistency.sh` | PASS |
| Local quality help | `scripts/quality help` | PASS |
| Fast local quality / TS typecheck | `bash scripts/quality.sh --fast` | PASS |
| Package smoke | `bash scripts/smoke-package.sh` | PASS |
| Rust lint | `cargo clippy --workspace --all-targets -- -D warnings` | PASS |
| Rust tests | `cargo test --workspace`; `cargo test --workspace --doc` | PASS |
| TS server-backed tests | `pnpm test` (57), `pnpm test:auth` (10), `pnpm test:pool` (13) via `npm exec pnpm@10.29.3` | PASS |
| Security/supply chain | `cargo audit`; `gitleaks detect --no-git --redact --config .gitleaks.toml`; `pnpm audit --audit-level high` if available | PASS / documented warnings only |
| Release durability smoke | `cargo build --release -p powdb-cli -p powdb-server && bash scripts/smoke-release.sh` | PASS (`SMOKE-RELEASE: ALL-PASS`) |

## Reviewer risks / notes

- CI check names changed; confirm branch protection expects the new `version consistency`, `TypeScript client (build + tests)`, and `gitleaks secret scan` checks.
- `cargo audit` still reports allowed warnings under the repo's existing audit posture. They remain visible but non-failing locally/CI by policy.
- The TypeScript client still has no ESLint config; this PR uses strict `tsc`, pure tests, and server-backed tests as the TS gate.
- Keep `.gitleaks.toml` narrow. Future false-positive exceptions should prefer exact placeholder regexes over broad path allowlists.

## Subagent evidence

Subagent spawn evidence: 3 probes spawned for Task 3 — Erdos/code-reviewer (`019ed8c2-92a6-7f70-8bda-a427f2f26e7d`), Newton/test-engineer (`019ed8c2-930e-7510-b548-d0b04bd3516a`), and Laplace/explore (`019ed8c2-93a6-7833-a1f4-efb4d4163fee`). Findings integrated: duplicate CI job risk, stale Corepack/pnpm documentation, report evidence drift, need for a reproducible pass/fail matrix, and docs for release/package/supply-chain checks.
