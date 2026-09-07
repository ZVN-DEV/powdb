#!/usr/bin/env bash
# update-bench-baseline-from-depot.sh: refresh crates/bench/baseline/main.json
# from the criterion estimates a Depot run of bench.yml uploaded.
#
# This is the rebaseline path the policy allows (CLAUDE.md, and the header of
# update-bench-baseline.sh): every number, the runner label, the RUSTFLAGS and
# the toolchain are read from that run's artifact and job log. Nothing is
# measured on this machine, which is why this works from an arm64 laptop where
# update-bench-baseline.sh cannot: that script measures the arch of the host
# it runs on, and the comparator then refuses the document on Depot.
#
#   scripts/update-bench-baseline-from-depot.sh <bench.yml run id> [output path]
#
# The run must be a completed bench.yml run on a Depot runner. Its head
# checkout's estimates are used (bench.yml also uploads the control run's, for
# the same-instance comparison; those are not a baseline). The document records
# the run in `source_run` so a later reader can audit where every number came
# from. The script stages the result; you commit, with the convention
#
#   bench: rebaseline after <change> (<workload>: <delta>, ...)
#
# It does NOT touch thesis-ratios.json. That file is hand-edited.
#
# Requires: gh (logged in), jq, cargo, git.

set -euo pipefail

RUN_ID="${1:?usage: $0 <bench.yml run id> [output path]}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
OUT="${2:-${REPO_ROOT}/crates/bench/baseline/main.json}"
REPO="ZVN-DEV/powdb"

die() { echo "error: $*" >&2; exit 1; }
for tool in gh jq cargo git; do
  command -v "${tool}" >/dev/null 2>&1 || die "${tool} is required but not installed."
done
cd "${REPO_ROOT}"

echo "===> reading bench.yml run ${RUN_ID}"
RUN_JSON="$(gh run view "${RUN_ID}" --repo "${REPO}" --json workflowName,headSha,createdAt,status,url)"
WORKFLOW="$(jq -r .workflowName <<< "${RUN_JSON}")"
[[ "${WORKFLOW}" == "bench" ]] || die "run ${RUN_ID} is the '${WORKFLOW}' workflow, not bench.yml."
[[ "$(jq -r .status <<< "${RUN_JSON}")" == "completed" ]] || die "run ${RUN_ID} has not completed."
HEAD_SHA="$(jq -r .headSha <<< "${RUN_JSON}")"
RUN_DATE="$(jq -r '.createdAt[:10]' <<< "${RUN_JSON}")"
RUN_URL="$(jq -r .url <<< "${RUN_JSON}")"

JOBS_JSON="$(gh api "repos/${REPO}/actions/runs/${RUN_ID}/jobs")"
RUNNER="$(jq -r '[.jobs[].labels[]] | unique | join(",")' <<< "${JOBS_JSON}")"
case "${RUNNER}" in
  depot-*) ;;
  *) die "run ${RUN_ID} did not execute on a Depot runner (labels: ${RUNNER}); refusing to rebaseline." ;;
esac
case "${RUNNER}" in
  *arm*) ARCH="aarch64" ;;
  *) ARCH="x86_64" ;;
esac
JOB_ID="$(jq -r '.jobs[0].id' <<< "${JOBS_JSON}")"

# The job log is what actually ran: the RUSTFLAGS and runner name the workflow
# exported, and the toolchain it installed.
JOB_LOG="$(mktemp "${TMPDIR:-/tmp}/powdb-bench-job-XXXXXX")"
gh run view --job "${JOB_ID}" --repo "${REPO}" --log > "${JOB_LOG}"
RUSTFLAGS_RECORDED="$(grep -m1 -oE 'RUSTFLAGS: -C [^[:cntrl:]]*' "${JOB_LOG}" | sed 's/^RUSTFLAGS: //' | sed 's/[[:space:]]*$//')"
RUNNER_RECORDED="$(grep -m1 -oE 'POWDB_BENCH_RUNNER: [^[:space:]]+' "${JOB_LOG}" | sed 's/^POWDB_BENCH_RUNNER: //')"
RUSTC_VERSION="$(grep -m1 -oE 'rustc [0-9]+\.[0-9]+\.[0-9]+' "${JOB_LOG}" | head -1 | awk '{print $2}')"
rm -f "${JOB_LOG}"
[[ -n "${RUSTFLAGS_RECORDED}" ]] || die "could not read RUSTFLAGS from the job log."
[[ -n "${RUNNER_RECORDED}" ]] || die "could not read POWDB_BENCH_RUNNER from the job log."
[[ "${RUNNER_RECORDED}" == "${RUNNER}" ]] || die "job log says POWDB_BENCH_RUNNER=${RUNNER_RECORDED} but the job ran on ${RUNNER}."
[[ -n "${RUSTC_VERSION}" ]] || RUSTC_VERSION="unknown"
echo "     head ${HEAD_SHA:0:7}, ${RUN_DATE}, runner ${RUNNER}, arch ${ARCH}, rustc ${RUSTC_VERSION}, RUSTFLAGS '${RUSTFLAGS_RECORDED}'"

echo "===> downloading the criterion estimates"
ART_DIR="$(mktemp -d "${TMPDIR:-/tmp}/powdb-bench-artifact-XXXXXX")"
trap 'rm -rf "${ART_DIR}"' EXIT
gh run download "${RUN_ID}" --repo "${REPO}" -n criterion-estimates -D "${ART_DIR}"
CRITERION_DIR="${ART_DIR}/head/target/criterion"
[[ -d "${CRITERION_DIR}" ]] || die "artifact has no head/target/criterion directory."

echo "===> asking the comparator which workloads it gates"
# Capture to a file first: reading the exit code through a pipe reads the
# wrong process (see update-bench-baseline.sh for the full reasoning).
WORKLOAD_LIST="$(mktemp "${TMPDIR:-/tmp}/powdb-workloads-XXXXXX")"
set +e
cargo run -q -p powdb-bench --bin compare -- --list-workloads > "${WORKLOAD_LIST}"
LIST_STATUS=$?
set -e
[[ ${LIST_STATUS} -eq 0 ]] || { rm -f "${WORKLOAD_LIST}"; die "'compare --list-workloads' exited ${LIST_STATUS}."; }
WORKLOADS=()
while IFS= read -r w; do
  [[ -z "${w}" ]] && continue
  [[ "${w}" =~ ^[a-z][a-z0-9_]*$ ]] || { rm -f "${WORKLOAD_LIST}"; die "'compare --list-workloads' printed something that is not a workload name: ${w}"; }
  WORKLOADS+=("${w}")
done < "${WORKLOAD_LIST}"
rm -f "${WORKLOAD_LIST}"
[[ ${#WORKLOADS[@]} -gt 0 ]] || die "the comparator reported no gated workloads; refusing to write an empty baseline."
echo "     ${#WORKLOADS[@]} workloads"

echo "===> extracting medians (head checkout of the run)"
OLD_FILE="${REPO_ROOT}/crates/bench/baseline/main.json"
workloads_json="{}"
printf '     %-32s %14s %14s %9s\n' workload old_ns new_ns delta
for w in "${WORKLOADS[@]}"; do
  est_file="${CRITERION_DIR}/${w}/new/estimates.json"
  [[ -f "${est_file}" ]] || die "the artifact has no estimates for gated workload '${w}' (${est_file})."
  ns="$(jq '.median.point_estimate' "${est_file}")"
  [[ -n "${ns}" && "${ns}" != "null" ]] || die "no median.point_estimate in ${est_file}"
  ops="$(awk -v n="${ns}" 'BEGIN { printf "%.0f", 1e9 / n }')"
  old_ns="$(jq -r --arg w "${w}" '.workloads[$w].ns_per_iter // empty' "${OLD_FILE}" 2>/dev/null || true)"
  if [[ -n "${old_ns}" ]]; then
    delta="$(awk -v o="${old_ns}" -v n="${ns}" 'BEGIN { printf "%+.1f%%", (n - o) / o * 100 }')"
  else
    delta="(new)"
  fi
  printf '     %-32s %14.0f %14.0f %9s\n' "${w}" "${old_ns:-0}" "${ns}" "${delta}"
  workloads_json="$(jq --arg name "${w}" --argjson ns "${ns}" --argjson ops "${ops}" \
    '. + {($name): {ns_per_iter: $ns, ops_per_sec: $ops}}' <<< "${workloads_json}")"
done

jq -n \
  --arg runner "${RUNNER}" \
  --arg rustflags "${RUSTFLAGS_RECORDED}" \
  --arg arch "${ARCH}" \
  --arg rustc "${RUSTC_VERSION}" \
  --arg updated "${RUN_DATE}" \
  --arg sha "${HEAD_SHA:0:7}" \
  --arg source "bench.yml run ${RUN_ID} (Depot, ${RUN_URL})" \
  --argjson workloads "${workloads_json}" \
  '{schema: 3, runner: $runner, rustflags: $rustflags, arch: $arch, rustc: $rustc, updated: $updated,
    commit: $sha, git_sha: $sha, source_run: $source, workloads: $workloads}' > "${OUT}"

echo "===> wrote ${OUT}"
if [[ "${OUT}" == "${OLD_FILE}" ]]; then
  git add "${OUT}"
  echo "     staged. Commit it as: bench: rebaseline after <change> (<workload>: <delta>, ...)"
fi
