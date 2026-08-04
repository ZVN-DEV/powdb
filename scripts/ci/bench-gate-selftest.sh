#!/usr/bin/env bash
# scripts/ci/bench-gate-selftest.sh: prove the bench regression gate can fail.
#
# The bench comparator is a gate about gates: nothing else checks it, it only
# runs in a manual workflow, and a comparator that always exits 0 would look
# exactly like a suite that never regresses. This script drives the real
# `compare` binary over synthetic criterion fixtures and asserts that each of
# its verdicts is reachable:
#
#   - a clean run passes
#   - a workload over its absolute threshold FAILS
#   - a workload over its same-instance control threshold FAILS
#   - a control run missing workloads FAILS (a partial comparison is not a gate)
#   - a runner label that differs from the baseline's FAILS
#   - RUSTFLAGS that differ from the baseline's FAIL
#   - a baseline arch that differs from the comparator's own FAILS, even when
#     both self-attested fields are spoofed to the baseline's exact values
#   - a baseline that records no arch at all FAILS (fail closed)
#   - a baseline schema this comparator does not implement FAILS
#   - the documented override downgrades the mismatch to a warning
#
# It runs in about a second: no benchmarking happens, the criterion estimate
# files are written by hand.
#
# The fixture `arch` has to match whatever this script is running on (laptops
# and CI both), so it is read from the comparator itself via `--print-arch`
# rather than from `rustc --print cfg`. The comparator compares against the
# arch compiled into its own binary; asking rustc would introduce a second
# source of truth that can legitimately disagree (a cross-compile, or
# CARGO_BUILD_TARGET set in the environment), and this self-test would then go
# red for a reason that has nothing to do with the gate.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/powdb-benchgate-XXXXXX")"
FAILURES=0
COMPARE=""

cleanup() { [[ -d "${WORK}" ]] && rm -rf "${WORK}"; }
trap cleanup EXIT

log()  { echo "bench-gate: $*"; }
fail() { echo "bench-gate: FAIL: $*" >&2; FAILURES=$((FAILURES + 1)); }

# Same list the comparator gates on. Kept here deliberately rather than parsed
# out of the source: if the two drift, the "missing workload" case below stops
# being synthetic and the self-test says so.
WORKLOADS=(
  insert_10k btree_lookup seq_scan_filter
  powql_point powql_filter_only powql_filter_projection powql_aggregation
  point_lookup_nonindexed scan_filter_project_top100 scan_filter_sort_limit10
  agg_sum agg_avg agg_min agg_max multi_col_and_filter conjunction_index_residual
  insert_single insert_batch_1k update_by_pk update_by_filter delete_by_filter
)

# write_criterion <dir> <default-ns> [workload=ns ...]
# No associative arrays: macOS still ships bash 3.2 and this has to run in
# both places.
write_criterion() {
  local dir="$1" default_ns="$2"; shift 2
  local overrides=("$@")
  local w ns kv
  for w in "${WORKLOADS[@]}"; do
    ns="${default_ns}"
    for kv in "${overrides[@]:-}"; do
      [[ "${kv%%=*}" == "${w}" ]] && ns="${kv#*=}"
    done
    mkdir -p "${dir}/${w}/new"
    printf '{"median":{"point_estimate":%s}}\n' "${ns}" > "${dir}/${w}/new/estimates.json"
  done
}

# write_baseline <path> <ns> <runner> <rustflags> <arch> [schema]
# <arch> may be the literal string "omit" to write a baseline that records no
# arch at all, which the comparator must treat as a mismatch, not a pass.
write_baseline() {
  local path="$1" ns="$2" runner="$3" flags="$4" arch="$5" schema="${6:-${BASELINE_SCHEMA}}"
  local body="" w arch_field=""
  for w in "${WORKLOADS[@]}"; do
    [[ -n "${body}" ]] && body+=","
    body+="\"${w}\":{\"ns_per_iter\":${ns},\"ops_per_sec\":1}"
  done
  [[ "${arch}" != "omit" ]] && arch_field="\"arch\":\"${arch}\","
  printf '{"schema":%s,"runner":"%s","rustflags":"%s",%s"workloads":{%s}}\n' \
    "${schema}" "${runner}" "${flags}" "${arch_field}" "${body}" > "${path}"
}

# expect <expected-status> <label> -- <env assignments...> -- <extra args...>
run_compare() {
  local expected="$1" label="$2"; shift 2
  local -a envs=() args=()
  local seen_sep=0
  for a in "$@"; do
    if [[ "${a}" == "--" ]]; then seen_sep=1; continue; fi
    if (( seen_sep )); then args+=("${a}"); else envs+=("${a}"); fi
  done
  local out rc
  out="$(env "${envs[@]}" "${COMPARE}" ${args[@]+"${args[@]}"} 2>&1)"
  rc=$?
  if [[ "${expected}" == "pass" && ${rc} -ne 0 ]]; then
    fail "${label}: expected exit 0, got ${rc}
$(sed 's/^/       /' <<<"${out}" | tail -20)"
  elif [[ "${expected}" == "fail" && ${rc} -eq 0 ]]; then
    fail "${label}: expected a non-zero exit, got 0
$(sed 's/^/       /' <<<"${out}" | tail -20)"
  else
    log "  ${label}: expected ${expected}, got exit ${rc} OK"
  fi
  printf '%s' "${out}" > "${WORK}/last-output.txt"
}

log "building the comparator"
if ! cargo build -p powdb-bench --bin compare --quiet; then
  echo "bench-gate: could not build the compare binary" >&2
  exit 1
fi
COMPARE="${REPO_ROOT}/target/debug/compare"
[[ -x "${COMPARE}" ]] || { echo "bench-gate: no compare binary at ${COMPARE}" >&2; exit 1; }

# Schema the comparator implements. Bump in lockstep with
# EXPECTED_BASELINE_SCHEMA in compare.rs; case 12 proves a stale value fails.
BASELINE_SCHEMA=3
# Ask the binary under test what arch it was compiled for. Anything else is a
# guess about the binary rather than a reading of it.
HOST_ARCH="$("${COMPARE}" --print-arch)"
[[ -n "${HOST_ARCH}" ]] || { echo "bench-gate: compare --print-arch printed nothing" >&2; exit 1; }
log "comparator arch: ${HOST_ARCH}"

RUNNER="selftest-runner"
FLAGS="-C target-cpu=selftest"
write_baseline "${WORK}/baseline.json" 1000 "${RUNNER}" "${FLAGS}" "${HOST_ARCH}"
# The ratio file is a real input; an empty ratio set keeps this test focused on
# the absolute/control/environment gates.
printf '{"schema":1,"ratios":{}}\n' > "${WORK}/ratios.json"

BASE_ENV=(
  "POWDB_BENCH_BASELINE=${WORK}/baseline.json"
  "POWDB_BENCH_RATIOS=${WORK}/ratios.json"
  "POWDB_BENCH_RUNNER=${RUNNER}"
  "RUSTFLAGS=${FLAGS}"
)

log "case 1: clean run passes"
write_criterion "${WORK}/head-clean" 1000
run_compare pass "clean" "${BASE_ENV[@]}" "POWDB_BENCH_CRITERION_DIR=${WORK}/head-clean" --

log "case 2: absolute regression fails"
# btree_lookup is a DEFAULT-tier (7%) workload; 1000 -> 1500 is +50%.
write_criterion "${WORK}/head-slow" 1000 "btree_lookup=1500"
run_compare fail "absolute regression" "${BASE_ENV[@]}" "POWDB_BENCH_CRITERION_DIR=${WORK}/head-slow" --
if ! grep -q "btree_lookup" "${WORK}/last-output.txt"; then
  fail "absolute regression: output never mentioned the offending workload"
fi

log "case 3: control regression fails where the absolute gate would not"
# agg_sum is a VERY_NOISY (20%) workload, so +15% passes the absolute gate.
# Against a same-instance control it is gated at 10% and must fail.
write_criterion "${WORK}/control-ok" 1000
write_criterion "${WORK}/head-ctl" 1000 "agg_sum=1150"
run_compare pass "control-sensitive delta, absolute gate only" \
  "${BASE_ENV[@]}" "POWDB_BENCH_CRITERION_DIR=${WORK}/head-ctl" --
run_compare fail "control-sensitive delta, with control run" \
  "${BASE_ENV[@]}" "POWDB_BENCH_CRITERION_DIR=${WORK}/head-ctl" -- --control "${WORK}/control-ok"

log "case 4: an incomplete control run fails instead of passing quietly"
mkdir -p "${WORK}/control-partial/btree_lookup/new"
printf '{"median":{"point_estimate":1000}}\n' \
  > "${WORK}/control-partial/btree_lookup/new/estimates.json"
run_compare fail "partial control run" \
  "${BASE_ENV[@]}" "POWDB_BENCH_CRITERION_DIR=${WORK}/head-clean" -- --control "${WORK}/control-partial"

log "case 5: runner mismatch fails"
run_compare fail "runner mismatch" \
  "POWDB_BENCH_BASELINE=${WORK}/baseline.json" "POWDB_BENCH_RATIOS=${WORK}/ratios.json" \
  "POWDB_BENCH_CRITERION_DIR=${WORK}/head-clean" \
  "POWDB_BENCH_RUNNER=some-other-runner" "RUSTFLAGS=${FLAGS}" --

log "case 6: RUSTFLAGS mismatch fails"
run_compare fail "rustflags mismatch" \
  "POWDB_BENCH_BASELINE=${WORK}/baseline.json" "POWDB_BENCH_RATIOS=${WORK}/ratios.json" \
  "POWDB_BENCH_CRITERION_DIR=${WORK}/head-clean" \
  "POWDB_BENCH_RUNNER=${RUNNER}" "RUSTFLAGS=-C target-cpu=native" --

log "case 7: an absent runner label is a mismatch, not a free pass"
run_compare fail "runner unset" \
  "POWDB_BENCH_BASELINE=${WORK}/baseline.json" "POWDB_BENCH_RATIOS=${WORK}/ratios.json" \
  "POWDB_BENCH_CRITERION_DIR=${WORK}/head-clean" "RUSTFLAGS=${FLAGS}" --

log "case 8: the documented override downgrades the mismatch to a warning"
run_compare pass "override" \
  "POWDB_BENCH_BASELINE=${WORK}/baseline.json" "POWDB_BENCH_RATIOS=${WORK}/ratios.json" \
  "POWDB_BENCH_CRITERION_DIR=${WORK}/head-clean" \
  "POWDB_BENCH_RUNNER=some-other-runner" "RUSTFLAGS=${FLAGS}" \
  "POWDB_BENCH_ALLOW_ENV_MISMATCH=1" --
if ! grep -q "NOT AUTHORITATIVE" "${WORK}/last-output.txt"; then
  fail "override: a non-authoritative run must say so in its output"
fi

# ── The measured half of the fingerprint ──────────────────────────────────
# Cases 1-8 all set POWDB_BENCH_RUNNER and RUSTFLAGS to whatever the fixture
# baseline recorded, which is the only thing those two fields can ever prove:
# that someone exported the right strings. The cases below spoof them
# perfectly and still have to be refused.
DEPOT_RUNNER="depot-ubuntu-24.04-4"
DEPOT_FLAGS="-C target-cpu=x86-64-v2"
# An arch no host can be, so this case cannot accidentally match the machine
# running the self-test.
FOREIGN_ARCH="selftest-not-this-arch"
write_baseline "${WORK}/baseline-foreign-arch.json" 1000 \
  "${DEPOT_RUNNER}" "${DEPOT_FLAGS}" "${FOREIGN_ARCH}"

log "case 9: a spoofed Depot environment cannot beat the measured arch check"
run_compare fail "arch mismatch under a spoofed environment" \
  "POWDB_BENCH_BASELINE=${WORK}/baseline-foreign-arch.json" \
  "POWDB_BENCH_RATIOS=${WORK}/ratios.json" \
  "POWDB_BENCH_CRITERION_DIR=${WORK}/head-clean" \
  "POWDB_BENCH_RUNNER=${DEPOT_RUNNER}" "RUSTFLAGS=${DEPOT_FLAGS}" --
if ! grep -q "ENVIRONMENT MISMATCH" "${WORK}/last-output.txt"; then
  fail "arch mismatch: refusal must be reported as an ENVIRONMENT MISMATCH"
fi
if ! grep -q "arch" "${WORK}/last-output.txt"; then
  fail "arch mismatch: the refusal must name the arch field"
fi
# Nothing about the runner or the flags disagreed: the refusal has to rest on
# arch alone, otherwise this case is passing for the wrong reason.
if grep -qE "^  (runner|rustflags) " "${WORK}/last-output.txt"; then
  fail "arch mismatch: expected arch to be the ONLY mismatched field"
fi

log "case 10: the override still downgrades an arch mismatch, and labels it"
run_compare pass "arch mismatch with override" \
  "POWDB_BENCH_BASELINE=${WORK}/baseline-foreign-arch.json" \
  "POWDB_BENCH_RATIOS=${WORK}/ratios.json" \
  "POWDB_BENCH_CRITERION_DIR=${WORK}/head-clean" \
  "POWDB_BENCH_RUNNER=${DEPOT_RUNNER}" "RUSTFLAGS=${DEPOT_FLAGS}" \
  "POWDB_BENCH_ALLOW_ENV_MISMATCH=1" --
if ! grep -q "NOT AUTHORITATIVE" "${WORK}/last-output.txt"; then
  fail "arch override: an overridden arch mismatch must still be labelled"
fi

log "case 11: a baseline that records no arch fails closed"
write_baseline "${WORK}/baseline-no-arch.json" 1000 "${RUNNER}" "${FLAGS}" omit
run_compare fail "baseline missing arch" \
  "POWDB_BENCH_BASELINE=${WORK}/baseline-no-arch.json" \
  "POWDB_BENCH_RATIOS=${WORK}/ratios.json" \
  "POWDB_BENCH_CRITERION_DIR=${WORK}/head-clean" \
  "POWDB_BENCH_RUNNER=${RUNNER}" "RUSTFLAGS=${FLAGS}" --
if ! grep -q "arch" "${WORK}/last-output.txt"; then
  fail "baseline missing arch: the refusal must name the arch field"
fi

log "case 12: a baseline schema the comparator does not implement fails"
write_baseline "${WORK}/baseline-old-schema.json" 1000 \
  "${RUNNER}" "${FLAGS}" "${HOST_ARCH}" 2
run_compare fail "stale baseline schema" \
  "POWDB_BENCH_BASELINE=${WORK}/baseline-old-schema.json" \
  "POWDB_BENCH_RATIOS=${WORK}/ratios.json" \
  "POWDB_BENCH_CRITERION_DIR=${WORK}/head-clean" \
  "POWDB_BENCH_RUNNER=${RUNNER}" "RUSTFLAGS=${FLAGS}" --
if ! grep -q "schema" "${WORK}/last-output.txt"; then
  fail "stale baseline schema: the refusal must say the schema is the problem"
fi

log "case 13: the checked-in baseline is the schema this comparator implements"
# Cases 1-12 all run against synthetic fixtures. If crates/bench/baseline/main.json
# ever drifts from EXPECTED_BASELINE_SCHEMA, every real bench run fails at the
# first step and no fixture-based case would have noticed.
REAL_BASELINE="${REPO_ROOT}/crates/bench/baseline/main.json"
REAL_SCHEMA="$(sed -n 's/.*"schema"[[:space:]]*:[[:space:]]*\([0-9]*\).*/\1/p' "${REAL_BASELINE}" | head -1)"
if [[ "${REAL_SCHEMA}" != "${BASELINE_SCHEMA}" ]]; then
  fail "checked-in baseline declares schema ${REAL_SCHEMA}, comparator implements ${BASELINE_SCHEMA}"
else
  log "  checked-in baseline schema ${REAL_SCHEMA}: expected pass, got exit 0 OK"
fi

echo
if (( FAILURES > 0 )); then
  echo "bench-gate: ${FAILURES} self-test failure(s)." >&2
  exit 1
fi
echo "bench-gate: ALL-PASS (every comparator verdict is reachable)."
