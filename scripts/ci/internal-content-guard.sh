#!/usr/bin/env bash
# scripts/ci/internal-content-guard.sh: this is a public repo. Fail the build
# if internal-only planning/agent artifacts get re-tracked, or if any public
# doc or source names an internal product codename.
#
# Two halves, and each one reports its own verdict, because a guard that says
# "clean" when it inspected nothing is worse than no guard: it is a wrong
# answer that a reader trusts.
#
#   (1) paths    no tracked file may live under an internal-only path
#   (2) denylist no tracked text file may match PUBLICATION_DENYLIST_REGEX
#
# The half that made this a script: the denylist ran as
#
#     names="$(git grep -niP "$REGEX" -- '*.md' ... || true)"
#
# and `|| true` conflates git-grep exit 1 (no match, the good case) with exit
# 2+ (invalid PCRE, `-P` unsupported in the runner's git build, an unreadable
# pathspec). Both leave `names` empty, and the step then printed
# "paths clean, denylist checked." Edit the denylist secret to something with an
# unbalanced group and the guard stops inspecting anything while the log claims
# it ran, so an internal codename can be merged into a public repo
# indefinitely. Verified: `PUBLICATION_DENYLIST_REGEX='('` printed
# "fatal: unmatched ( for expression group" and then "denylist checked", exit 0.
#
# `git ls-files | grep ... || true` had the same shape: run it outside a git
# repository and it printed "paths clean". Both halves now read the producing
# command's status directly, never through a pipe, and treat anything that is
# not "ran and found nothing" as a failure.
#
# Usage:
#   internal-content-guard.sh             run both halves against $PWD's repo
#   internal-content-guard.sh --selftest  prove both halves can fail
#
# Env:
#   PUBLICATION_DENYLIST_REGEX  the private denylist. Unset on fork pull
#                               requests (secrets are withheld), in which case
#                               the denylist half is reported NOT CHECKED and
#                               gets its verdict on the push run after merge.

set -uo pipefail

# Tracked files may not live here. Kept in the script rather than the workflow
# so the selftest exercises the same list CI does.
INTERNAL_PATH_REGEX='^(docs/strategy/|docs/superpowers/|docs/audits/|docs/gtm-strategy\.md|\.omx/)'

# The file types the denylist inspects.
DENYLIST_PATHSPEC=('*.md' '*.rs' '*.toml' '*.yml' '*.yaml')

guard() {
  local fail=0 tmp listing status paths denylist_verdict tracked=0

  tmp="$(mktemp -d "${TMPDIR:-/tmp}/powdb-internal-guard.XXXXXX")" || {
    echo "::error::internal-content-guard: mktemp -d failed; the guard did not run" >&2
    return 1
  }
  listing="${tmp}/tracked.txt"

  # (1) No tracked file may live under an internal-only path.
  git ls-files > "${listing}" 2>"${tmp}/ls-files.err"
  status=$?
  if [ "${status}" -ne 0 ]; then
    echo "::error::internal-content-guard: git ls-files failed (exit ${status}); the path half did not run" >&2
    sed 's/^/  /' "${tmp}/ls-files.err" >&2
    rm -rf "${tmp}"
    return 1
  fi
  if [ ! -s "${listing}" ]; then
    echo "::error::internal-content-guard: git ls-files listed no tracked files at all; the path half inspected nothing" >&2
    rm -rf "${tmp}"
    return 1
  fi
  tracked="$(wc -l < "${listing}" | tr -d ' ')"
  paths="$(grep -E "${INTERNAL_PATH_REGEX}" "${listing}")"
  if [ -n "${paths}" ]; then
    echo "::error::Internal-only paths are tracked in this public repo (see .gitignore):"
    echo "${paths}"
    fail=1
  fi

  # (2) Apply the private publication denylist without storing sensitive names
  # in the public workflow.
  if [ -n "${PUBLICATION_DENYLIST_REGEX:-}" ]; then
    git grep -niP "${PUBLICATION_DENYLIST_REGEX}" -- "${DENYLIST_PATHSPEC[@]}" \
      > "${tmp}/hits.txt" 2>"${tmp}/grep.err"
    # Read directly off git grep. Never through a pipe: 0 = matched,
    # 1 = ran and found nothing, 2+ = did not run.
    status=$?
    case "${status}" in
      0)
        echo "::error::Private name found in a public file. Use neutral wording:"
        cat "${tmp}/hits.txt"
        fail=1
        denylist_verdict="checked"
        ;;
      1)
        denylist_verdict="checked"
        ;;
      *)
        echo "::error::internal-content-guard: git grep exited ${status}, so the denylist half inspected nothing." >&2
        echo "         An invalid PCRE in the denylist secret, or a git build without -P, used to" >&2
        echo "         read as 'checked'. It is a failure." >&2
        sed 's/^/  /' "${tmp}/grep.err" >&2
        fail=1
        denylist_verdict="ERRORED"
        ;;
    esac
  else
    denylist_verdict="NOT CHECKED"
    echo "::warning::internal-content-guard: PUBLICATION_DENYLIST_REGEX is unavailable on this event (${GITHUB_EVENT_NAME:-unknown}), so only the path check ran. Secrets are withheld from fork pull requests; the denylist half gets its verdict on the push run after merge."
  fi

  rm -rf "${tmp}"

  if [ "${fail}" -ne 0 ]; then
    return 1
  fi
  echo "internal-content-guard: paths clean across ${tracked} tracked files, denylist ${denylist_verdict}."
  return 0
}

# --- selftest -------------------------------------------------------------

fail_selftest() {
  echo "::error::internal-content-guard selftest: $*" >&2
  exit 1
}

make_repo() {
  local dir="$1"
  mkdir -p "${dir}"
  git -C "${dir}" init -q
  git -C "${dir}" config user.email selftest@example.invalid
  git -C "${dir}" config user.name selftest
  printf '# fixture\n\nordinary public prose.\n' > "${dir}/README.md"
  printf 'pub fn f() {}\n' > "${dir}/lib.rs"
  git -C "${dir}" add -A
}

run_case() {
  # run_case <label> <pass|fail> <repo dir> [regex]
  local label="$1" expect="$2" dir="$3" regex="${4-}"
  local log status
  log="${SELFTEST_WORK}/$(tr -c 'a-zA-Z0-9' '-' <<<"${label}").log"
  (
    # GIT_CEILING_DIRECTORIES keeps the "not a repo" case honest: without it,
    # git would walk up out of the fixture and find whatever repo happens to
    # contain TMPDIR, and the case would silently stop testing anything.
    cd "${dir}" || exit 97
    unset GIT_DIR GIT_WORK_TREE
    export GIT_CEILING_DIRECTORIES="${SELFTEST_WORK}"
    if [ -n "${regex}" ]; then
      export PUBLICATION_DENYLIST_REGEX="${regex}"
    else
      unset PUBLICATION_DENYLIST_REGEX
    fi
    bash "${SELFTEST_SELF}"
  ) > "${log}" 2>&1
  status=$?
  if [ "${expect}" = pass ] && [ "${status}" -ne 0 ]; then
    sed 's/^/    /' "${log}" >&2
    fail_selftest "${label}: expected exit 0, got ${status}"
  fi
  if [ "${expect}" = fail ] && [ "${status}" -eq 0 ]; then
    sed 's/^/    /' "${log}" >&2
    fail_selftest "${label}: expected a non-zero exit, got 0; this half cannot fail"
  fi
  SELFTEST_LOG="${log}"
  echo "  ok: ${label} (exit ${status})"
}

selftest() {
  # Not `local`: the EXIT trap fires after this function returns.
  SELFTEST_WORK="$(mktemp -d "${TMPDIR:-/tmp}/powdb-internal-guard-selftest.XXXXXX")" || fail_selftest "mktemp -d failed"
  trap 'rm -rf "${SELFTEST_WORK}"' EXIT
  SELFTEST_SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
  local w="${SELFTEST_WORK}"

  make_repo "${w}/clean"
  run_case "a clean repo passes" pass "${w}/clean" 'codename-that-appears-nowhere'

  # The denylist half really matches.
  make_repo "${w}/named"
  printf 'we shipped Projectzebra last quarter.\n' > "${w}/named/notes.md"
  git -C "${w}/named" add -A
  run_case "a denylisted name is found" fail "${w}/named" 'projectzebra'
  grep -qi 'Private name found' "${SELFTEST_LOG}" || {
    sed 's/^/    /' "${SELFTEST_LOG}" >&2
    fail_selftest "a real hit was not reported as a denylist hit"
  }

  # The finding: a regex git cannot compile must not read as "checked".
  run_case "an invalid denylist regex is a failure, not a pass" fail "${w}/clean" '('
  grep -q 'inspected nothing' "${SELFTEST_LOG}" || {
    sed 's/^/    /' "${SELFTEST_LOG}" >&2
    fail_selftest "an unrunnable git grep did not say the half inspected nothing"
  }
  grep -q 'denylist checked' "${SELFTEST_LOG}" && {
    sed 's/^/    /' "${SELFTEST_LOG}" >&2
    fail_selftest "an unrunnable git grep still claimed the denylist was checked"
  }

  # The path half.
  make_repo "${w}/internal"
  mkdir -p "${w}/internal/docs/strategy"
  printf 'internal only\n' > "${w}/internal/docs/strategy/plan.md"
  git -C "${w}/internal" add -A
  run_case "a tracked internal path is refused" fail "${w}/internal" 'codename-that-appears-nowhere'
  grep -q 'Internal-only paths are tracked' "${SELFTEST_LOG}" || {
    sed 's/^/    /' "${SELFTEST_LOG}" >&2
    fail_selftest "a tracked internal path was not reported"
  }

  # git ls-files that cannot run must not read as "paths clean".
  mkdir -p "${w}/notarepo"
  run_case "a repo git cannot list is a failure, not 'paths clean'" fail "${w}/notarepo" 'codename-that-appears-nowhere'
  grep -q 'path half did not run\|inspected nothing' "${SELFTEST_LOG}" || {
    sed 's/^/    /' "${SELFTEST_LOG}" >&2
    fail_selftest "an unrunnable git ls-files did not say the path half did not run"
  }

  # No secret: the path half still runs, and the log says the other half did not.
  run_case "a missing denylist secret says NOT CHECKED and still passes" pass "${w}/clean"
  grep -q 'denylist NOT CHECKED' "${SELFTEST_LOG}" || {
    sed 's/^/    /' "${SELFTEST_LOG}" >&2
    fail_selftest "a withheld secret did not produce a NOT CHECKED verdict"
  }

  echo "internal-content-guard: selftest ok"
}

case "${1:-}" in
  --selftest) selftest ;;
  '') guard ;;
  *)
    echo "::error::internal-content-guard: usage: internal-content-guard.sh [--selftest]" >&2
    exit 1
    ;;
esac
