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

if [[ ! -f "${CI_WORKFLOW}" ]]; then
  echo "::error::no workflow at ${CI_WORKFLOW}" >&2
  exit 1
fi

# Job keys are the only two-space-indented `name:`-style keys under `jobs:`.
defined="$(awk '
  /^jobs:/        { in_jobs = 1; next }
  /^[^[:space:]]/ { in_jobs = 0 }
  in_jobs && /^  [a-zA-Z0-9_-]+:[[:space:]]*$/ {
    gsub(/[: ]/, "", $0); print
  }
' "${CI_WORKFLOW}" | sort)"

# The `needs:` list of the ci-success job.
required="$(awk '
  /^  ci-success:/ { in_job = 1; next }
  in_job && /^  [a-zA-Z0-9_-]+:[[:space:]]*$/ { in_job = 0 }
  in_job && /^    needs:/ { in_needs = 1; next }
  in_needs && /^      - / { sub(/^      - /, "", $0); print; next }
  in_needs && !/^      / { in_needs = 0 }
' "${CI_WORKFLOW}" | sort)"

if [[ -z "${defined}" ]]; then
  echo "::error::parsed zero jobs out of ${CI_WORKFLOW}; this guard is not working" >&2
  exit 1
fi
if [[ -z "${required}" ]]; then
  echo "::error::parsed zero entries out of ci-success.needs in ${CI_WORKFLOW}" >&2
  exit 1
fi

missing=()
while IFS= read -r job; do
  [[ -z "${job}" || "${job}" == "ci-success" ]] && continue
  skip=0
  for advisory in ${ADVISORY_JOBS[@]+"${ADVISORY_JOBS[@]}"}; do
    [[ "${job}" == "${advisory}" ]] && skip=1
  done
  (( skip )) && continue
  if ! grep -qx -- "${job}" <<<"${required}"; then
    missing+=("${job}")
  fi
done <<<"${defined}"

# The reverse direction matters too: a `needs:` entry naming a job that no
# longer exists makes the whole workflow invalid, and is easy to leave behind
# when a job is renamed.
stale=()
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
  exit 1
fi
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

undocumented=()
overdocumented=()
if [[ -z "${documented}" ]]; then
  echo "::error::parsed zero job names out of the CI Checks section of ${CONTRIBUTING_DOC}; this guard is not working" >&2
  exit 1
fi
while IFS= read -r job; do
  [[ -z "${job}" ]] && continue
  grep -qx -- "${job}" <<<"${documented}" || undocumented+=("${job}")
done <<<"${defined}"
while IFS= read -r job; do
  [[ -z "${job}" ]] && continue
  grep -qx -- "${job}" <<<"${defined}" || overdocumented+=("${job}")
done <<<"${documented}"

status=0
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

if (( status == 0 )); then
  echo "ci-needs: all $(wc -l <<<"${defined}" | tr -d ' ') jobs are required by ci-success and documented in ${CONTRIBUTING_DOC}."
fi
exit "${status}"
