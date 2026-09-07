#!/usr/bin/env bash
# update-bench-baseline.sh — refresh crates/bench/baseline/main.json from a
# clean criterion run.
#
# Run this AFTER an intentional code change that legitimately moves the
# numbers. The script:
#
#   1. Runs `cargo bench -p powdb-bench` (release, full suite).
#   2. Extracts each workload's median.point_estimate from
#      target/criterion/<workload>/new/estimates.json.
#   3. Writes a new main.json with the current rustc version, git sha, and date.
#   4. Stages it. (Does NOT commit — you commit, with a message explaining why.)
#
# This script does NOT touch thesis-ratios.json. That file is hand-edited.
# Raising a ratio ceiling is a separate, deliberate commit.
#
# POLICY: baseline/main.json may only be rebaselined from a Depot run of
# bench.yml (see CLAUDE.md). This script records the fingerprint of the host
# it actually runs on (runner from POWDB_BENCH_RUNNER, RUSTFLAGS as set, arch
# measured from the compare binary). It never fabricates the Depot values, so
# a baseline produced on a laptop will be refused by the comparator. That is
# the gate working, not a bug in this script.
#
# Convention for the rebaseline commit:
#   bench: rebaseline after <change> (<workload>: <delta>)
#
# Requires: cargo, jq, git.

set -euo pipefail

# Resolve repo root from this script's location.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
BASELINE_FILE="${REPO_ROOT}/crates/bench/baseline/main.json"
CRITERION_DIR="${REPO_ROOT}/target/criterion"

# The gated workload list comes from the comparator, which is the only thing
# that actually enforces it. A hand-copied list here can only go wrong in one
# direction: a short copy drops workloads from main.json, and the comparator
# treats a workload with no baseline entry as a first-run CAPTURE, so it prints
# a number and passes instead of gating. The list is populated below, after
# the build, because asking the binary requires compiling it.
WORKLOADS=()


if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is required but not installed." >&2
  exit 1
fi

cd "${REPO_ROOT}"

echo "===> asking the comparator which workloads it gates"
while IFS= read -r w; do
  [[ -n "${w}" ]] && WORKLOADS+=("${w}")
done < <(cargo run -q -p powdb-bench --bin compare -- --list-workloads)
if [[ ${#WORKLOADS[@]} -eq 0 ]]; then
  echo "error: the comparator reported no gated workloads; refusing to write an" >&2
  echo "       empty baseline, which would disable the gate entirely." >&2
  exit 1
fi
echo "     ${#WORKLOADS[@]} workloads"

echo "===> running cargo bench -p powdb-bench (this takes ~60s)"
cargo bench -p powdb-bench --quiet

echo
echo "===> extracting median values from criterion output"

# Build the workloads object.
workloads_json="{}"
for w in "${WORKLOADS[@]}"; do
  est_file="${CRITERION_DIR}/${w}/new/estimates.json"
  if [[ ! -f "${est_file}" ]]; then
    echo "error: missing ${est_file}" >&2
    echo "       did the bench targets run? check 'cargo bench -p powdb-bench' output." >&2
    exit 1
  fi
  ns=$(jq '.median.point_estimate' "${est_file}")
  if [[ -z "${ns}" || "${ns}" == "null" ]]; then
    echo "error: no median.point_estimate in ${est_file}" >&2
    exit 1
  fi
  ops=$(awk -v n="${ns}" 'BEGIN { printf "%.0f", 1e9 / n }')
  workloads_json=$(jq \
    --arg name "${w}" \
    --argjson ns "${ns}" \
    --argjson ops "${ops}" \
    '. + {($name): {ns_per_iter: $ns, ops_per_sec: $ops}}' \
    <<< "${workloads_json}")
done

# Build the full baseline document. The fingerprint is recorded truthfully
# from this host: the comparator (schema 3) checks runner, rustflags, and a
# measured arch, and will refuse a baseline whose fingerprint does not match
# the machine the comparison later runs on.
if [[ -z "${POWDB_BENCH_RUNNER:-}" ]]; then
  echo "error: POWDB_BENCH_RUNNER is not set." >&2
  echo "       The baseline records the runner it was produced on. The" >&2
  echo "       authoritative rebaseline path is the Depot bench.yml run," >&2
  echo "       which sets this. Set it explicitly to proceed locally," >&2
  echo "       knowing the comparator will refuse a non-Depot baseline." >&2
  exit 1
fi

RUSTC_VERSION=$(rustc --version | awk '{print $2}')
GIT_SHA=$(git rev-parse --short HEAD)
TODAY=$(date -u +%Y-%m-%d)
ARCH=$(cargo run --quiet -p powdb-bench --bin compare -- --print-arch)
if [[ -z "${ARCH}" ]]; then
  echo "error: could not measure arch via 'compare --print-arch'." >&2
  exit 1
fi

new_baseline=$(jq -n \
  --argjson workloads "${workloads_json}" \
  --arg rustc "${RUSTC_VERSION}" \
  --arg commit "${GIT_SHA}" \
  --arg today "${TODAY}" \
  --arg runner "${POWDB_BENCH_RUNNER}" \
  --arg rustflags "${RUSTFLAGS:-}" \
  --arg arch "${ARCH}" \
  '{
    schema: 3,
    runner: $runner,
    rustflags: $rustflags,
    arch: $arch,
    rustc: $rustc,
    updated: $today,
    commit: $commit,
    workloads: $workloads
  }')

# Show a diff summary before writing.
echo
echo "===> diff (old → new):"
if [[ -f "${BASELINE_FILE}" ]]; then
  for w in "${WORKLOADS[@]}"; do
    old=$(jq -r ".workloads.${w}.ns_per_iter // \"null\"" "${BASELINE_FILE}")
    new=$(jq -r ".workloads.${w}.ns_per_iter" <<< "${new_baseline}")
    printf "  %-28s %15s -> %15s\n" "${w}" "${old}" "${new}"
  done
fi

echo "${new_baseline}" | jq '.' > "${BASELINE_FILE}"
git add "${BASELINE_FILE}"

echo
echo "===> wrote ${BASELINE_FILE} and staged it."
echo "     review the diff with 'git diff --cached crates/bench/baseline/main.json'"
echo "     commit convention: 'bench: rebaseline after <change> (<workload>: <delta>)'"
