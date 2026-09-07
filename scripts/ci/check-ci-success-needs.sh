#!/usr/bin/env bash
# scripts/ci/check-ci-success-needs.sh: every ci.yml job must be required.
#
# `ci-success` is the single required status check on `main`, and it only fails
# if a job listed in its `needs:` fails. A job that is not in that list runs
# beside the gate and blocks nothing: it can be red on every PR for months and
# the merge button stays green. The instruction "add new jobs to the needs
# list" is a comment, and comments do not fail builds.
#
# This makes the omission a build failure. Every job defined in ci.yml (except
# `ci-success` itself, and anything explicitly listed as intentionally
# advisory) must appear in `ci-success.needs`.
#
# It also checks the same list against CONTRIBUTING.md's "CI Checks" section.
# That section had drifted to nine of nineteen jobs, and it is what a
# contributor reads to find out what must pass, so a stale list there is a
# wrong answer rather than a missing one.
#
# `--results` is what `ci-success` itself runs, and it exists because the
# guard used to be circular. The aggregator's whole verdict was
#
#     for result in $RESULTS; do [ "$result" = success ] || exit 1; done
#     echo "All required CI jobs succeeded."
#
# with `RESULTS: ${{ join(needs.*.result, ' ') }}`. Delete the `needs:` list and
# that expands to the empty string, the loop runs zero times, and the one
# required check on `main` goes green with nothing behind it. The job that would
# have noticed, `ci-needs-completeness`, blocks nothing except *through*
# `ci-success` — i.e. through the list that was just deleted. So the completeness
# check now runs inside `ci-success` too, against ci.yml on disk rather than
# against the `needs:` list, and `--results` additionally refuses a results
# vector whose length is not the number of jobs ci.yml defines. Emptying
# `needs:` now fails from inside the required check itself.
#
# Usage:
#   check-ci-success-needs.sh                 completeness check only
#   check-ci-success-needs.sh --results "..."  the same, plus verify the
#                                              aggregator's results vector
#   check-ci-success-needs.sh --selftest       prove all of it can fail
#
# Env:
#   CI_WORKFLOW       path to the workflow (default .github/workflows/ci.yml)
#   CONTRIBUTING_DOC  path to CONTRIBUTING.md (default CONTRIBUTING.md)

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CI_WORKFLOW="${CI_WORKFLOW:-${REPO_ROOT}/.github/workflows/ci.yml}"
CONTRIBUTING_DOC="${CONTRIBUTING_DOC:-${REPO_ROOT}/CONTRIBUTING.md}"

# Jobs deliberately excluded from the required set. Keep this empty unless
# there is a written reason; an entry here is a job that cannot block a merge.
ADVISORY_JOBS=()

# Job keys are the only two-space-indented keys under `jobs:`. YAML allows a
# trailing comment after the colon, and the original pattern anchored on `$`
# right after the optional spaces, so `  my-new-gate: # TODO wire this up` was
# not a job as far as this script was concerned: never compared against
# `ci-success.needs`, never compared against CONTRIBUTING.md, and the script
# still reported "all N jobs are required". That is exactly the omission it
# exists to make impossible, so the comment is now stripped rather than
# disqualifying the line.
#
# The `UNPARSED ` arm is the anti-vacuity half: any other two-space key shape
# (a quoted key, say) is reported instead of being silently dropped, so this
# parser can no longer lose a job by not recognising it.
parse_job_keys() {
  awk '
    /^jobs:/         { in_jobs = 1; next }
    /^[^[:space:]]/  { in_jobs = 0 }
    !in_jobs         { next }
    /^[[:space:]]*$/ { next }
    /^  #/           { next }
    /^   /           { next }
    /^  [a-zA-Z0-9_-]+:[[:space:]]*(#.*)?$/ {
      line = $0
      sub(/[[:space:]]*#.*$/, "", line)
      gsub(/[: ]/, "", line)
      print line
      next
    }
    /^  [^[:space:]]/ { print "UNPARSED " $0 }
  ' "$1"
}

# The `needs:` list of the ci-success job. Same trailing-comment tolerance on
# the "next job starts here" boundary, and on the list entries themselves.
parse_ci_success_needs() {
  awk '
    /^  ci-success:[[:space:]]*(#.*)?$/                { in_job = 1; next }
    in_job && /^  [a-zA-Z0-9_-]+:[[:space:]]*(#.*)?$/  { in_job = 0 }
    in_job && /^    needs:/                            { in_needs = 1; next }
    in_needs && /^      - / {
      line = $0
      sub(/^      - /, "", line)
      sub(/[[:space:]]*#.*$/, "", line)
      sub(/[[:space:]]+$/, "", line)
      print line
      next
    }
    in_needs && !/^      / { in_needs = 0 }
  ' "$1"
}

check() {
  local results_given="$1" results="$2"

  if [[ ! -f "${CI_WORKFLOW}" ]]; then
    echo "::error::no workflow at ${CI_WORKFLOW}" >&2
    return 1
  fi

  local raw defined required
  raw="$(parse_job_keys "${CI_WORKFLOW}")"

  local unparsed
  unparsed="$(grep '^UNPARSED ' <<<"${raw}")"
  if [[ -n "${unparsed}" ]]; then
    echo "::error::${CI_WORKFLOW} has job-indent lines this parser does not recognise as job keys." >&2
    echo "         They would be invisible to every check below, so they are a failure, not a skip:" >&2
    sed 's/^UNPARSED /  /' <<<"${unparsed}" >&2
    return 1
  fi

  defined="$(sort <<<"${raw}")"
  required="$(parse_ci_success_needs "${CI_WORKFLOW}" | sort)"

  if [[ -z "${defined}" ]]; then
    echo "::error::parsed zero jobs out of ${CI_WORKFLOW}; this guard is not working" >&2
    return 1
  fi
  if [[ -z "${required}" ]]; then
    echo "::error::parsed zero entries out of ci-success.needs in ${CI_WORKFLOW}." >&2
    echo "         An empty needs: list makes ci-success (the one required check on main)" >&2
    echo "         pass with nothing behind it." >&2
    return 1
  fi

  local missing=() must_require=0
  while IFS= read -r job; do
    [[ -z "${job}" || "${job}" == "ci-success" ]] && continue
    local skip=0 advisory
    for advisory in ${ADVISORY_JOBS[@]+"${ADVISORY_JOBS[@]}"}; do
      [[ "${job}" == "${advisory}" ]] && skip=1
    done
    (( skip )) && continue
    must_require=$((must_require + 1))
    if ! grep -qx -- "${job}" <<<"${required}"; then
      missing+=("${job}")
    fi
  done <<<"${defined}"

  # The reverse direction matters too: a `needs:` entry naming a job that no
  # longer exists makes the whole workflow invalid, and is easy to leave behind
  # when a job is renamed.
  local stale=()
  while IFS= read -r dep; do
    [[ -z "${dep}" ]] && continue
    if ! grep -qx -- "${dep}" <<<"${defined}"; then
      stale+=("${dep}")
    fi
  done <<<"${required}"

  # The contributor-facing list must name the same jobs. Bullets in the
  # "## CI Checks" section look like: - **`job-key`**: description
  if [[ ! -f "${CONTRIBUTING_DOC}" ]]; then
    echo "::error::no CONTRIBUTING.md at ${CONTRIBUTING_DOC}" >&2
    return 1
  fi
  local documented
  documented="$(awk '
    /^## CI Checks/           { in_section = 1; next }
    in_section && /^## /      { in_section = 0 }
    in_section && /^- \*\*`[a-zA-Z0-9_-]+`\*\*/ {
      line = $0
      sub(/^- \*\*`/, "", line)
      sub(/`\*\*.*$/, "", line)
      print line
    }
  ' "${CONTRIBUTING_DOC}" | sort -u)"

  local undocumented=() overdocumented=()
  if [[ -z "${documented}" ]]; then
    echo "::error::parsed zero job names out of the CI Checks section of ${CONTRIBUTING_DOC}; this guard is not working" >&2
    return 1
  fi
  while IFS= read -r job; do
    [[ -z "${job}" ]] && continue
    grep -qx -- "${job}" <<<"${documented}" || undocumented+=("${job}")
  done <<<"${defined}"
  while IFS= read -r job; do
    [[ -z "${job}" ]] && continue
    grep -qx -- "${job}" <<<"${defined}" || overdocumented+=("${job}")
  done <<<"${documented}"

  local status=0
  if (( ${#undocumented[@]} > 0 )); then
    echo "::error::these ci.yml jobs are missing from the CI Checks list in ${CONTRIBUTING_DOC}:" >&2
    printf '  %s\n' "${undocumented[@]}" >&2
    status=1
  fi
  if (( ${#overdocumented[@]} > 0 )); then
    echo "::error::${CONTRIBUTING_DOC} documents CI jobs that do not exist in ci.yml:" >&2
    printf '  %s\n' "${overdocumented[@]}" >&2
    status=1
  fi
  if (( ${#missing[@]} > 0 )); then
    echo "::error::these ci.yml jobs are NOT in ci-success.needs, so they gate nothing:" >&2
    printf '  %s\n' "${missing[@]}" >&2
    status=1
  fi
  if (( ${#stale[@]} > 0 )); then
    echo "::error::ci-success.needs names jobs that do not exist in ci.yml:" >&2
    printf '  %s\n' "${stale[@]}" >&2
    status=1
  fi

  # --- the aggregator's own verdict ---------------------------------------
  #
  # `must_require` comes from the job list in ci.yml, NOT from `needs:`, which
  # is the whole point: an emptied `needs:` gives a zero-length results vector
  # and a non-zero expectation, so the mismatch is visible from inside the
  # required check.
  if (( results_given )); then
    local -a seen=()
    # shellcheck disable=SC2206
    seen=(${results})
    echo "ci-success: upstream job results: ${results:-<none>}"
    if (( ${#seen[@]} != must_require )); then
      echo "::error::ci-success aggregated ${#seen[@]} job result(s) but ci.yml defines ${must_require} job(s) that must be required." >&2
      echo "         An aggregator that iterates fewer jobs than exist is not a gate: with an empty" >&2
      echo "         needs: list this loop ran zero times and reported success." >&2
      status=1
    fi
    local result
    for result in ${results}; do
      if [[ "${result}" != "success" ]]; then
        echo "::error::A required CI job did not succeed (results: ${results})" >&2
        status=1
      fi
    done
  fi

  if (( status == 0 )); then
    echo "ci-needs: all $(wc -l <<<"${defined}" | tr -d ' ') jobs are required by ci-success and documented in ${CONTRIBUTING_DOC}."
    if (( results_given )); then
      echo "ci-success: all ${must_require} required CI jobs succeeded."
    fi
  fi
  return "${status}"
}

# --- selftest -------------------------------------------------------------
#
# Every case below is a mutation of a clean fixture pair, and each one asserts
# the script FAILS on it. Case 1 asserts it passes on the clean pair, so the
# failures are verdicts rather than a harness that always says no.

fail_selftest() {
  echo "::error::check-ci-success-needs selftest: $*" >&2
  exit 1
}

write_fixture() {
  local dir="$1" jobs_extra="$2" needs_extra="$3" doc_extra="$4"
  mkdir -p "${dir}"
  {
    printf 'name: fixture\non:\n  push:\n\njobs:\n'
    printf '  # a comment at job indent, which is not a job\n'
    printf '  alpha:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: "true"\n\n'
    printf '  beta:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: "true"\n\n'
    printf '%s' "${jobs_extra}"
    printf '  ci-success:\n    name: ci-success\n    runs-on: ubuntu-24.04\n    if: always()\n    needs:\n      - alpha\n      - beta\n%s' "${needs_extra}"
    printf '    steps:\n      - run: "true"\n'
  } > "${dir}/ci.yml"
  {
    printf '# Contributing\n\n## CI Checks\n\n'
    # shellcheck disable=SC2016  # markdown backticks, not command substitution
    printf -- '- **`alpha`**: a\n- **`beta`**: b\n- **`ci-success`**: aggregator\n'
    printf '%s' "${doc_extra}"
    printf '\n## Something else\n'
  } > "${dir}/CONTRIBUTING.md"
}

run_case() {
  # run_case <label> <expect: pass|fail> <dir> [extra args...]
  local label="$1" expect="$2" dir="$3"
  shift 3
  local log status
  log="${SELFTEST_WORK}/$(tr -c 'a-zA-Z0-9' '-' <<<"${label}").log"
  CI_WORKFLOW="${dir}/ci.yml" CONTRIBUTING_DOC="${dir}/CONTRIBUTING.md" \
    bash "${SELFTEST_SELF}" "$@" > "${log}" 2>&1
  status=$?
  if [[ "${expect}" == "pass" && "${status}" -ne 0 ]]; then
    sed 's/^/    /' "${log}" >&2
    fail_selftest "${label}: expected exit 0, got ${status}"
  fi
  if [[ "${expect}" == "fail" && "${status}" -eq 0 ]]; then
    sed 's/^/    /' "${log}" >&2
    fail_selftest "${label}: expected a non-zero exit, got 0; this check cannot fail"
  fi
  SELFTEST_LOG="${log}"
  echo "  ok: ${label} (exit ${status})"
}

selftest() {
  # Not `local`: the EXIT trap fires after this function returns.
  SELFTEST_WORK="$(mktemp -d "${TMPDIR:-/tmp}/powdb-ci-needs-selftest.XXXXXX")" || fail_selftest "mktemp -d failed"
  trap 'rm -rf "${SELFTEST_WORK}"' EXIT
  SELFTEST_SELF="${BASH_SOURCE[0]}"
  local w="${SELFTEST_WORK}"

  write_fixture "${w}/clean" '' '' ''
  run_case "clean fixture passes" pass "${w}/clean"

  write_fixture "${w}/clean" '' '' ''
  run_case "clean fixture with a full results vector passes" pass "${w}/clean" --results "success success"

  # The finding this rewrite is for: a job whose key line carries a trailing
  # comment used to be invisible to the parser.
  write_fixture "${w}/comment" \
    '  gamma: # TODO wire into ci-success
    runs-on: ubuntu-24.04
    steps:
      - run: "true"

' '' ''
  run_case "a job key with a trailing comment is seen" fail "${w}/comment"
  grep -q 'gamma' "${SELFTEST_LOG}" || {
    sed 's/^/    /' "${SELFTEST_LOG}" >&2
    fail_selftest "the trailing-comment job was not named in the failure"
  }

  # ... and once it IS in needs: and documented, it passes, so the case above
  # is a verdict about the needs list and not just about the comment.
  # shellcheck disable=SC2016  # markdown backticks in the fixture doc
  write_fixture "${w}/comment-ok" \
    '  gamma: # TODO wire into ci-success
    runs-on: ubuntu-24.04
    steps:
      - run: "true"

' '      - gamma
' '- **`gamma`**: g
'
  run_case "a trailing-comment job that IS required passes" pass "${w}/comment-ok"

  # shellcheck disable=SC2016  # markdown backticks in the fixture doc
  write_fixture "${w}/missing" \
    '  gamma:
    runs-on: ubuntu-24.04
    steps:
      - run: "true"

' '' '- **`gamma`**: g
'
  run_case "a job absent from needs fails" fail "${w}/missing"

  write_fixture "${w}/stale" '' '      - delta
' ''
  run_case "a needs entry naming no job fails" fail "${w}/stale"

  write_fixture "${w}/undocumented" \
    '  gamma:
    runs-on: ubuntu-24.04
    steps:
      - run: "true"

' '      - gamma
' ''
  run_case "a job absent from CONTRIBUTING fails" fail "${w}/undocumented"

  # An emptied needs: list. This is the shape that turned the one required
  # check on main green with nothing behind it.
  mkdir -p "${w}/empty-needs"
  write_fixture "${w}/empty-needs" '' '' ''
  python3 - "${w}/empty-needs/ci.yml" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
s = s.replace("    needs:\n      - alpha\n      - beta\n", "")
open(p, "w").write(s)
PY
  run_case "an emptied needs: list fails" fail "${w}/empty-needs"
  run_case "an emptied needs: list fails the aggregator too" fail "${w}/empty-needs" --results ""

  # A results vector shorter than the job list: the same vacuity, arrived at
  # from the aggregator's side.
  write_fixture "${w}/clean" '' '' ''
  run_case "an empty results vector fails" fail "${w}/clean" --results ""
  grep -q 'aggregated 0 job result' "${SELFTEST_LOG}" || {
    sed 's/^/    /' "${SELFTEST_LOG}" >&2
    fail_selftest "an empty results vector was not reported as an under-count"
  }
  run_case "a short results vector fails" fail "${w}/clean" --results "success"
  run_case "a failed upstream job fails" fail "${w}/clean" --results "success failure"
  run_case "a skipped upstream job fails" fail "${w}/clean" --results "success skipped"

  # A two-space key shape the parser does not recognise must be reported, not
  # dropped: a dropped job is invisible to every check above.
  write_fixture "${w}/odd" \
    '  "gamma":
    runs-on: ubuntu-24.04
    steps:
      - run: "true"

' '' ''
  run_case "an unrecognised job-key shape fails" fail "${w}/odd"
  grep -q 'does not recognise as job keys' "${SELFTEST_LOG}" || {
    sed 's/^/    /' "${SELFTEST_LOG}" >&2
    fail_selftest "an unparseable job key was not reported as such"
  }

  echo "check-ci-success-needs: selftest ok"
}

case "${1:-}" in
  --selftest)
    selftest
    ;;
  --results)
    if (( $# < 2 )); then
      echo "::error::check-ci-success-needs: --results needs a value (pass \"\" if there is none)" >&2
      exit 1
    fi
    check 1 "$2"
    ;;
  '')
    check 0 ''
    ;;
  *)
    echo "::error::check-ci-success-needs: usage: check-ci-success-needs.sh [--results \"<join(needs.*.result)>\" | --selftest]" >&2
    exit 1
    ;;
esac
