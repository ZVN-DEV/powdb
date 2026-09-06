#!/usr/bin/env bash
# scripts/ci/semver-gate.sh: run cargo-semver-checks over the workspace and
# refuse the vacuous outcome.
#
# The hazard the semver check exists for is a point release cut over an
# unnoticed API break. The hazard THIS wrapper exists for is subtler:
# cargo-semver-checks treats a 0.x MINOR bump as a major release and skips
# every lint, so on the path this repo actually takes (seven consecutive minors
# since 0.19) the gate printed, for all eight crates,
#
#     Checking powdb-storage v0.26.0 -> v0.27.0 (major change)
#      Checked [0.000s] 0 checks: 0 pass, 254 skip
#      Summary no semver update required
#
# and exited 0 having compared nothing. A gate that has never executed a single
# check is not a gate. This adds the two halves that were missing:
#
#   1. `--selftest` builds a two-crate fixture whose current version deletes a
#      public function on a patch bump and asserts the tool FAILS on it, then
#      asserts it PASSES on the same pair with the function kept. That proves
#      the binary on this runner really runs lints and really can say no,
#      before it is ever pointed at the workspace.
#   2. The real run refuses "0 checks" for any crate whose baseline -> current
#      is a PATCH bump, which is where lints are supposed to run: on a 0.x
#      patch bump cargo-semver-checks reports "(minor change)" and runs ~196
#      lints. Zero there means the tool degraded (missing baseline, wrong
#      manifest, silent rustdoc failure) and the release would sail through
#      unchecked. It also refuses a run in which no crate was examined at all.
#
# A 0.x minor bump legitimately reports 0 checks, so that is not failed here.
# publish.yml runs an advisory `--release-type minor` pass for those instead,
# which lists the breaks for the CHANGELOG without blocking the release.
#
# `--color never` everywhere: this parses the tool's own stdout and the
# publishing job sets CARGO_TERM_COLOR=always. Grepping coloured tool output is
# how this project has produced vacuous gates before.
#
# Usage:
#   semver-gate.sh              check the workspace against its crates.io baselines
#   semver-gate.sh --selftest   prove the checker can fail, using a local fixture
#
# Env:
#   SEMVER_CHECKS  command that invokes cargo-semver-checks (default
#                  "cargo semver-checks"); set it to a standalone binary path
#                  when testing without a cargo subcommand install.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# Deliberately unquoted at the call sites: this may be a two-word command
# ("cargo semver-checks") or a single binary path.
SEMVER_CHECKS="${SEMVER_CHECKS:-cargo semver-checks}"

die() {
  echo "::error::semver-gate: $*" >&2
  exit 1
}

# "0.27.1" -> major=0 minor=27 patch=1, printed space separated. Anything that
# is not three numeric components prints nothing, and callers treat that as
# "cannot classify" rather than guessing.
split_version() {
  local v="$1"
  if [[ "${v}" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)([-+].*)?$ ]]; then
    printf '%s %s %s' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" "${BASH_REMATCH[3]}"
  fi
}

# True when base -> curr differs only in the patch component, which is the bump
# shape cargo-semver-checks actually lints on a 0.x crate.
is_patch_bump() {
  local base curr
  base="$(split_version "$1")" || return 1
  curr="$(split_version "$2")" || return 1
  [ -n "${base}" ] && [ -n "${curr}" ] || return 1
  # shellcheck disable=SC2086
  set -- ${base} ${curr}
  [ "$1" = "$4" ] && [ "$2" = "$5" ] && [ "$3" != "$6" ]
}

write_fixture_manifest() {
  cat > "$1" <<EOF
[package]
name = "powdb-semver-gate-fixture"
version = "$2"
edition = "2021"
publish = false

[lib]
path = "src/lib.rs"

# Detached from any surrounding workspace so the fixture resolves standalone.
[workspace]
EOF
}

# Pull "196" out of "     Checked [   0.005s] 196 checks: 195 pass, 1 fail...".
checks_run_in() {
  sed -nE 's/.*Checked \[[^]]*\] ([0-9]+) checks:.*/\1/p' "$1" | head -1
}

selftest() {
  # Not `local`: the EXIT trap below fires after this function has returned, so
  # a local would already be out of scope when the trap expands it.
  SELFTEST_WORK="$(mktemp -d "${TMPDIR:-/tmp}/powdb-semver-gate.XXXXXX")" || die "mktemp failed"
  trap 'rm -rf "${SELFTEST_WORK}"' EXIT
  local work="${SELFTEST_WORK}"

  mkdir -p "${work}/baseline/src" "${work}/current/src"
  write_fixture_manifest "${work}/baseline/Cargo.toml" "0.1.0"
  write_fixture_manifest "${work}/current/Cargo.toml" "0.1.1"
  printf 'pub fn kept() {}\npub fn removed_in_current() {}\n' > "${work}/baseline/src/lib.rs"

  local log status checks

  # Negative case. The patch release deletes a public function, which is
  # exactly the "point release over an unnoticed break" this gate is for.
  printf 'pub fn kept() {}\n' > "${work}/current/src/lib.rs"
  log="${work}/breaking.log"
  # shellcheck disable=SC2086
  ${SEMVER_CHECKS} check-release \
    --manifest-path "${work}/current/Cargo.toml" \
    --baseline-root "${work}/baseline" \
    --color never > "${log}" 2>&1
  status=$?
  echo "EXIT: ${status}" >> "${log}"

  if [ "${status}" -eq 0 ]; then
    sed 's/^/    /' "${log}" >&2
    die "selftest: deleting a public function on a patch bump exited 0; the checker cannot fail"
  fi
  if ! grep -q 'function_missing' "${log}"; then
    sed 's/^/    /' "${log}" >&2
    die "selftest: the failure was not the expected function_missing lint; the checker is failing for the wrong reason"
  fi
  checks="$(checks_run_in "${log}")"
  if [ -z "${checks}" ] || [ "${checks}" -eq 0 ]; then
    sed 's/^/    /' "${log}" >&2
    die "selftest: the failing run reported no executed checks"
  fi
  echo "semver-gate: selftest negative case OK (exit ${status}, ${checks} checks, function_missing reported)"

  # Positive case. Same version pair, nothing removed: the checker must pass,
  # which is what proves the negative case above was a real verdict and not a
  # harness that always fails.
  cp "${work}/baseline/src/lib.rs" "${work}/current/src/lib.rs"
  log="${work}/clean.log"
  # shellcheck disable=SC2086
  ${SEMVER_CHECKS} check-release \
    --manifest-path "${work}/current/Cargo.toml" \
    --baseline-root "${work}/baseline" \
    --color never > "${log}" 2>&1
  status=$?
  echo "EXIT: ${status}" >> "${log}"

  if [ "${status}" -ne 0 ]; then
    sed 's/^/    /' "${log}" >&2
    die "selftest: an API-compatible patch bump was rejected (exit ${status}); the checker is not usable as a gate"
  fi
  checks="$(checks_run_in "${log}")"
  if [ -z "${checks}" ] || [ "${checks}" -eq 0 ]; then
    sed 's/^/    /' "${log}" >&2
    die "selftest: the passing run executed no checks, so its pass means nothing"
  fi
  echo "semver-gate: selftest positive case OK (exit 0, ${checks} checks)"
  echo "semver-gate: selftest ok"
}

check_workspace() {
  local log status crates_seen vacuous crate base curr checks
  log="$(mktemp "${TMPDIR:-/tmp}/powdb-semver-gate-run.XXXXXX")" || die "mktemp failed"

  # shellcheck disable=SC2086
  ( cd "${REPO_ROOT}" && ${SEMVER_CHECKS} check-release --workspace --color never ) > "${log}" 2>&1
  status=$?
  echo "EXIT: ${status}" >> "${log}"
  cat "${log}"

  crates_seen=0
  vacuous=0
  unlinted=0
  crate=''
  base=''
  curr=''
  while IFS= read -r line; do
    case "${line}" in
      *"Checking "*" v"*" -> v"*)
        crate="$(sed -E 's/.*Checking ([^ ]+) v.*/\1/' <<<"${line}")"
        base="$(sed -E 's/.*Checking [^ ]+ v([^ ]+) -> v.*/\1/' <<<"${line}")"
        curr="$(sed -E 's/.*-> v([^ ]+).*/\1/' <<<"${line}")"
        ;;
      *"Checked ["*" checks:"*)
        checks="$(sed -E 's/.*Checked \[[^]]*\] ([0-9]+) checks:.*/\1/' <<<"${line}")"
        crates_seen=$((crates_seen + 1))
        if [ "${checks}" -eq 0 ]; then
          unlinted=$((unlinted + 1))
        fi
        if is_patch_bump "${base}" "${curr}" && [ "${checks}" -eq 0 ]; then
          echo "::error::semver-gate: ${crate} ${base} -> ${curr} is a patch bump but ran 0 checks." >&2
          echo "         A patch bump is the one shape cargo-semver-checks always lints, so zero" >&2
          echo "         means the checker degraded (missing baseline, wrong manifest, rustdoc" >&2
          echo "         failure) and this release was compared against nothing." >&2
          vacuous=$((vacuous + 1))
        fi
        ;;
    esac
  done < "${log}"

  rm -f "${log}"

  if [ "${crates_seen}" -eq 0 ]; then
    die "the run examined no crate at all; --workspace matched nothing or the output format changed"
  fi
  if [ "${vacuous}" -gt 0 ]; then
    exit 1
  fi

  # Say out loud how much of the run was real. On a 0.x minor bump this reads
  # "8 examined, 8 linted nothing", which is the accepted behaviour but should
  # never again be mistaken for a gate that ran.
  if [ "${unlinted}" -gt 0 ]; then
    echo "::notice::semver-gate: ${unlinted} of ${crates_seen} crate(s) ran zero lints." \
         "cargo-semver-checks skips every lint when it classifies the bump as major," \
         "which is what a 0.x MINOR bump is. The advisory --release-type minor pass in" \
         "publish.yml is what lists breaking changes for those releases."
  fi
  echo "semver-gate: ${crates_seen} crate(s) examined, ${unlinted} ran zero lints, semver-checks exit ${status}"
  exit "${status}"
}

case "${1:-}" in
  --selftest) selftest ;;
  '') check_workspace ;;
  *) die "usage: semver-gate.sh [--selftest]" ;;
esac
