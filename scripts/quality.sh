#!/usr/bin/env bash
# Local quality gate that mirrors the high-signal CI checks without making
# optional tooling a prerequisite for the common path.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

mode="default"
if [[ $# -gt 0 ]]; then
  case "$1" in
    --help|-h|help) mode="help" ;;
    --fast|fast) mode="fast" ;;
    --full|full) mode="full" ;;
    *) echo "quality: unknown mode '$1'" >&2; echo "Try: scripts/quality.sh --help" >&2; exit 2 ;;
  esac
fi

usage() {
  cat <<'USAGE'
Usage: scripts/quality.sh [--fast|--full|--help]
       scripts/quality [fast|full|help]

Modes:
  --fast   Version consistency + Rust fmt + TS build/pure tests via pnpm 10.
  default  Fast mode plus cargo check and optional cargo audit/gitleaks if installed.
  --full   CI-parity local sweep: fmt, clippy, cargo tests/doc tests, TS build/tests,
           release package smoke, and optional supply-chain scans.

TypeScript checks use `npm exec --package=pnpm@10.29.3` to avoid stale Corepack keys.
Optional security tools are skipped with install hints instead of failing the default path:
  cargo-audit: cargo install cargo-audit
  gitleaks:    brew install gitleaks (or use the GitHub Action in CI)
USAGE
}

run() { echo "+ $*"; "$@"; }
skip() { echo "quality: skip — $*"; }

pnpm10() { npm exec --yes --package=pnpm@10.29.3 -- pnpm "$@"; }

run_ts_pure() {
  (cd clients/ts && run pnpm10 install --frozen-lockfile && run pnpm10 build     && run pnpm10 test:escape && run pnpm10 test:protocol && run pnpm10 test:typed && run pnpm10 test:errors)
}
run_optional_scans() {
  if command -v cargo-audit >/dev/null 2>&1; then
    run cargo audit
  else
    skip "cargo-audit not installed (cargo install cargo-audit)"
  fi
  if command -v gitleaks >/dev/null 2>&1; then
    run gitleaks detect --no-git --redact --config .gitleaks.toml
  else
    skip "gitleaks not installed (CI runs gitleaks/gitleaks-action)"
  fi
}

case "$mode" in
  help)
    usage
    ;;
  fast)
    run bash scripts/check-version-consistency.sh
    run cargo fmt --all -- --check
    run_ts_pure
    ;;
  default)
    run bash scripts/check-version-consistency.sh
    run cargo fmt --all -- --check
    run cargo check --workspace --locked
    run_ts_pure
    run_optional_scans
    ;;
  full)
    run bash scripts/check-version-consistency.sh
    run cargo fmt --all -- --check
    run cargo clippy --workspace --all-targets -- -D warnings
    run cargo test --workspace
    run cargo test --workspace --doc
    run_ts_pure
    (cd clients/ts && run pnpm10 test && run pnpm10 test:auth && run pnpm10 test:pool)
    run bash scripts/smoke-package.sh
    run_optional_scans
    ;;
esac
