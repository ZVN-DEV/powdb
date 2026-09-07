#!/usr/bin/env bash
# scripts/ci/semver-advisory.sh: list the breaking changes a 0.x MINOR bump is
# allowed to have, for the CHANGELOG and the release notes. Advisory: it never
# blocks the release, and it says so in its own body rather than in a
# `continue-on-error:` YAML key a reader of the log has to go look up.
#
# Why this is a script and not an inline `run:` block. It WAS an inline block,
# and it blocked. A `run:` with no `shell:` key runs under GitHub's default
# `bash -e {0}`; the block opened with `set -uo pipefail`, which turns on `u`
# and `pipefail` and does NOT clear the inherited `-e`. So the first non-zero
# exit from cargo-semver-checks killed the shell on that line: no log, no
# `$GITHUB_STEP_SUMMARY` block, and the trailing `exit 0` unreachable. The
# `semver` job went red and `publish` (needs: semver) never started, which took
# the whole crates.io channel down for exactly the releases this step is for.
# `--release-type minor` forces the major lints on, so ANY removed public item
# in the workspace made the tool exit 1 and the release stop.
#
# Two things follow from that history and are load-bearing here:
#
#   1. `set +e` is explicit, after `set -uo pipefail`, and the exit status of
#      the tool is captured into a variable rather than being allowed to end
#      the script. The script's own exit is an unconditional `exit 0`.
#   2. The behaviour is testable outside GitHub. `--selftest` drives this same
#      script as a subprocess against stub tools that fail, pass, and crash,
#      and asserts each time that the script exited 0 AND that the summary it
#      wrote contains what that case should contain. Reintroducing the `-e`
#      bug makes case 1 fail, because the summary comes back empty.
#
# The step exists to produce the breaking-change list, and on the release shape
# this repo actually cuts it is the ONLY thing that produces one. A 0.x MINOR
# bump (0.27.0 -> 0.28.0, every release since 0.19) is classified by
# cargo-semver-checks as a major release, so it skips every lint and
# semver-gate.sh exits 0 having compared nothing. That is correct: a 0.x minor
# is allowed to break. It also means that if this step produces nothing, the
# release got no semver verdict at all from anywhere.
#
# So the one thing that does block here is the tool failing to RUN. Findings
# never block, because findings are legal on a 0.x minor and blocking on them
# is the bug above:
#
#   exit 0                     clean; summarised as "no breaking changes"
#   exit 1 with a report       the normal 0.x minor; the list goes to the summary
#   exit 1 with no report      the report format probably moved; warn, do not block
#   exit >1                    the tool did not run; fail, because nothing else
#                              in this job compared anything either
#
# Usage:
#   semver-advisory.sh              run the forced-minor pass over the workspace
#   semver-advisory.sh --selftest   prove the wrapper survives a failing tool
#
# Env:
#   SEMVER_CHECKS         command that invokes cargo-semver-checks (default
#                         "cargo semver-checks")
#   GITHUB_STEP_SUMMARY   file the summary block is appended to; when unset the
#                         block goes to stdout, so local runs show it too.

set -uo pipefail
# Deliberate, and the whole point of this file: see the header.
set +e

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# Deliberately unquoted at the call site: this may be a two-word command
# ("cargo semver-checks") or a single binary path.
SEMVER_CHECKS="${SEMVER_CHECKS:-cargo semver-checks}"

# Matches cargo-semver-checks' own report lines: one "--- failure <lint>: ..."
# per break, and one "     Summary ..." verdict line.
BREAK_PATTERN='^--- failure |^[[:space:]]*Summary '

run_advisory() {
  local log status found

  log="$(mktemp "${TMPDIR:-/tmp}/powdb-semver-advisory.XXXXXX")" || {
    echo "::warning::semver-advisory: mktemp failed; the advisory pass did not run" >&2
    return 0
  }

  # shellcheck disable=SC2086
  ( cd "${REPO_ROOT}" && ${SEMVER_CHECKS} check-release --workspace --release-type minor --color never ) \
    > "${log}" 2>&1
  # Read directly off the command, never through a pipe.
  status=$?
  echo "EXIT: ${status}" >> "${log}"
  cat "${log}"

  found="$(grep -E "${BREAK_PATTERN}" "${log}")"

  {
    echo "## Semver advisory for a 0.x minor bump (not a gate)"
    echo
    echo '```'
    if [ -n "${found}" ]; then
      printf '%s\n' "${found}"
    elif [ "${status}" -eq 0 ]; then
      echo "no breaking changes reported"
    else
      echo "the advisory pass exited ${status} without reporting any breaking change;"
      echo "cargo-semver-checks did not run to completion, so this release has no list."
    fi
    echo '```'
  } >> "${GITHUB_STEP_SUMMARY:-/dev/stdout}"

  rm -f "${log}"

  if [ -n "${found}" ] || [ "${status}" -eq 0 ]; then
    return 0
  fi
  if [ "${status}" -eq 1 ]; then
    # 1 is cargo-semver-checks' "there are findings" status. Reaching here with
    # nothing parsed most likely means its report format moved, and turning a
    # release red over a grep that stopped matching is the bug this file exists
    # to have fixed. Say so loudly and let the release through.
    echo "::warning::semver-advisory: the forced-minor pass exited 1 and no '--- failure' or 'Summary' line was parsed out of it." \
         "The report format has probably changed; BREAK_PATTERN needs updating. Not blocking the release." >&2
    return 0
  fi
  # Anything above 1 is the tool not running: a panic, a manifest it could not
  # parse, a missing binary. On a 0.x minor bump semver-gate.sh lints nothing by
  # design, so this step is the whole semver verdict, and a job that compared
  # nothing anywhere must not report a release as checked.
  echo "::error::semver-advisory: cargo-semver-checks exited ${status} without running." >&2
  echo "         On a 0.x minor bump the gate skips every lint by design, so this pass is the" >&2
  echo "         only semver verdict the release gets. There is now none." >&2
  return 1
}

# --- selftest -------------------------------------------------------------
#
# Each case runs THIS script as a subprocess, so the assertions are about the
# real thing the workflow invokes and not about a re-implementation of it.

fail_selftest() {
  echo "::error::semver-advisory selftest: $*" >&2
  exit 1
}

selftest() {
  # Not `local`: the EXIT trap below fires after this function has returned, so
  # a local would already be out of scope when the trap expands it.
  SELFTEST_WORK="$(mktemp -d "${TMPDIR:-/tmp}/powdb-semver-advisory-selftest.XXXXXX")" || fail_selftest "mktemp -d failed"
  trap 'rm -rf "${SELFTEST_WORK}"' EXIT
  local work self summary status out
  work="${SELFTEST_WORK}"
  self="${BASH_SOURCE[0]}"

  # A tool that reports breaks and exits 1: the shape of every 0.x minor bump
  # this repo cuts, and the shape that used to kill the step.
  cat > "${work}/breaking.sh" <<'STUB'
#!/bin/sh
echo "--- failure function_missing: pub fn removed_in_current, previously in file lib.rs:2 ---"
echo "     Summary semver requires new major version: 1 major and 0 minor checks failed"
exit 1
STUB

  # A clean run.
  cat > "${work}/clean.sh" <<'STUB'
#!/bin/sh
echo "     Checking powdb-storage v0.27.0 -> v0.28.0 (major change)"
echo "      Checked [0.000s] 0 checks: 0 pass, 254 skip"
echo "     Summary no semver update required"
exit 0
STUB

  # A tool that could not run at all: no report, non-zero exit.
  cat > "${work}/crashed.sh" <<'STUB'
#!/bin/sh
echo "error: failed to parse manifest" >&2
exit 101
STUB
  chmod +x "${work}/breaking.sh" "${work}/clean.sh" "${work}/crashed.sh"

  # Case 1: a failing tool must NOT fail the step, and its list must reach the
  # summary. This is the regression that took the crates.io channel down.
  summary="${work}/summary-breaking.txt"
  : > "${summary}"
  SEMVER_CHECKS="${work}/breaking.sh" GITHUB_STEP_SUMMARY="${summary}" bash "${self}" > "${work}/out1.log" 2>&1
  status=$?
  [ "${status}" -eq 0 ] || {
    sed 's/^/    /' "${work}/out1.log" >&2
    fail_selftest "a tool that exited 1 made the advisory step exit ${status}; it must never block the release"
  }
  grep -q 'function_missing' "${summary}" || {
    sed 's/^/    /' "${summary}" >&2
    fail_selftest "the breaking-change list did not reach the job summary, which is the only reason this step exists"
  }
  grep -q 'semver requires new major version' "${summary}" || {
    sed 's/^/    /' "${summary}" >&2
    fail_selftest "the tool's Summary verdict did not reach the job summary"
  }
  out="$(cat "${work}/out1.log")"
  case "${out}" in
    *"EXIT: 1"*) : ;;
    *) fail_selftest "the tool's own log and exit status were not printed to the step log" ;;
  esac
  echo "semver-advisory: selftest case 1 OK (tool exit 1 -> step exit 0, list in summary)"

  # Case 2: a clean run says so, rather than leaving an empty block.
  summary="${work}/summary-clean.txt"
  : > "${summary}"
  SEMVER_CHECKS="${work}/clean.sh" GITHUB_STEP_SUMMARY="${summary}" bash "${self}" > "${work}/out2.log" 2>&1
  status=$?
  [ "${status}" -eq 0 ] || fail_selftest "a clean tool run made the advisory step exit ${status}"
  grep -q 'no semver update required' "${summary}" || {
    sed 's/^/    /' "${summary}" >&2
    fail_selftest "a clean run's Summary line did not reach the job summary"
  }
  echo "semver-advisory: selftest case 2 OK (clean run summarised)"

  # Case 3: a tool that never ran is a failure. On a 0.x minor bump
  # semver-gate.sh lints nothing by design, so a crashed advisory pass leaves
  # the release with no semver verdict from anywhere, and a green job would be
  # a claim that it was checked.
  summary="${work}/summary-crashed.txt"
  : > "${summary}"
  SEMVER_CHECKS="${work}/crashed.sh" GITHUB_STEP_SUMMARY="${summary}" bash "${self}" > "${work}/out3.log" 2>&1
  status=$?
  [ "${status}" -ne 0 ] || {
    sed 's/^/    /' "${work}/out3.log" >&2
    fail_selftest "a tool that exited 101 without running left the advisory step green; the release would report as semver-checked having compared nothing"
  }
  grep -q 'without reporting any breaking change' "${summary}" || {
    sed 's/^/    /' "${summary}" >&2
    fail_selftest "a run that produced no list did not say so in the summary; it would read as 'nothing broke'"
  }
  grep -q '::error::semver-advisory' "${work}/out3.log" || {
    sed 's/^/    /' "${work}/out3.log" >&2
    fail_selftest "a tool that did not run emitted no error annotation"
  }
  echo "semver-advisory: selftest case 3 OK (a tool that did not run fails the step)"

  # Case 4: findings the grep could not parse. Exit 1 is the tool's "there are
  # findings" status, so a pattern that stopped matching must warn, never block:
  # turning a release red over a grep is the bug this file was extracted from.
  cat > "${work}/reformatted.sh" <<'STUB'
#!/bin/sh
echo "!!! BREAKING function_missing (some future report format)"
exit 1
STUB
  chmod +x "${work}/reformatted.sh"
  summary="${work}/summary-reformatted.txt"
  : > "${summary}"
  SEMVER_CHECKS="${work}/reformatted.sh" GITHUB_STEP_SUMMARY="${summary}" bash "${self}" > "${work}/out4.log" 2>&1
  status=$?
  [ "${status}" -eq 0 ] || {
    sed 's/^/    /' "${work}/out4.log" >&2
    fail_selftest "an unparseable findings report exited ${status}; a grep that stopped matching must not block a release"
  }
  grep -q '::warning::semver-advisory' "${work}/out4.log" || {
    sed 's/^/    /' "${work}/out4.log" >&2
    fail_selftest "an unparseable findings report emitted no warning"
  }
  echo "semver-advisory: selftest case 4 OK (unparseable findings warn, do not block)"

  echo "semver-advisory: selftest ok"
}

case "${1:-}" in
  --selftest) selftest ;;
  '') run_advisory ;;
  *)
    echo "::error::semver-advisory: usage: semver-advisory.sh [--selftest]" >&2
    exit 1
    ;;
esac
