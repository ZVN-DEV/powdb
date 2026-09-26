#!/usr/bin/env bash
# scripts/ci/testing-feature-guard.sh: refuse any shipped artifact whose
# non-dev dependency resolution turns on powdb-query's or powdb-storage's
# `testing` feature.
#
# `testing` enables Engine::set_force_generic_path, instrumentation that
# exists so tests can prove the optimized and generic executor paths agree.
# It also enables a storage-layer WAL fsync failpoint used only by crash/fault
# tests. Neither must ever reach a shipped build. Tests get the query feature
# through powdb-query's path-only dev-dependency on itself, and that feature
# propagates to powdb-storage/testing; cargo strips that dev edge from the
# published manifest. The one way either feature leaks is someone adding
# `features = ["testing"]` to a normal dependency edge (or making it a default
# feature). This guard resolves each shipped artifact's normal+build graph and
# fails on either.
#
# Usage: testing-feature-guard.sh            check every shipped artifact
#        testing-feature-guard.sh --selftest prove the detector can fire

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${REPO_ROOT}"

# The eight published workspace crates, plus the node addon (its own
# manifest outside the workspace, riding the `powdb` facade).
WORKSPACE_PACKAGES=(
  powdb-storage
  powdb-query
  powdb-sync
  powdb-auth
  powdb-backup
  powdb-server
  powdb-cli
  powdb
)

die() { echo "::error::testing-feature-guard: $*" >&2; exit 1; }

# Fail if this cargo-tree output enables one crate's `testing` feature.
# Two shapes betray it: the feature node itself, and that package's line whose
# resolved feature list contains the word.
tree_enables_testing_for() {
  local crate="$1"
  local escaped="${crate//-/\\-}"
  grep -qE "${escaped} feature \"testing\"" <<<"$2" \
    || grep -E "${escaped} v[0-9][^)]*\\)" <<<"$2" | grep -qE '\)[^(]*\btesting\b'
}

tree_enables_testing() {
  tree_enables_testing_for powdb-query "$1" \
    || tree_enables_testing_for powdb-storage "$1"
}

check() {
  local label="$1"; shift
  local out
  if ! out="$(cargo tree "$@" -e normal,build,features -f '{p} {f}' 2>&1)"; then
    die "cargo tree failed for ${label}: ${out}"
  fi
  [[ -n "${out}" ]] || die "cargo tree printed nothing for ${label}; this guard is not inspecting anything"
  if tree_enables_testing "${out}"; then
    echo "::error::shipped artifact '${label}' resolves a test-only 'testing' feature enabled" >&2
    grep -nE 'testing' <<<"${out}" >&2
    return 1
  fi
  echo "testing-feature-guard: ${label} clean"
}

if [[ "${1:-}" == "--selftest" ]]; then
  # The detector must fire on a resolution that HAS both features on. The
  # dev-unified test build of powdb-query is exactly that shape: query/testing
  # propagates to storage/testing for fault-injection integration tests.
  out="$(cargo tree -p powdb-query -e features -f '{p} {f}' 2>&1)" \
    || die "selftest: cargo tree failed: ${out}"
  tree_enables_testing_for powdb-query "${out}" \
    || die "selftest: detector did not fire on powdb-query/testing; the guard is vacuous"
  tree_enables_testing_for powdb-storage "${out}" \
    || die "selftest: detector did not fire on powdb-storage/testing; the guard is vacuous"
  echo "testing-feature-guard: selftest ok (detector fires on query/storage testing features)"
  exit 0
fi

failed=0
for pkg in "${WORKSPACE_PACKAGES[@]}"; do
  check "${pkg}" -p "${pkg}" || failed=1
done
check "bindings/node (powdb-node)" --manifest-path bindings/node/Cargo.toml || failed=1

if (( failed )); then
  die "at least one shipped artifact enables the test-only feature"
fi
echo "testing-feature-guard: all shipped artifacts clean"
