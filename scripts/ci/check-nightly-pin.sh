#!/usr/bin/env bash
# scripts/ci/check-nightly-pin.sh: fail when a pinned nightly toolchain has
# been pinned too long.
#
# `fuzz.yml` pins a dated nightly because a floating one cannot be compared
# run to run, and because one night's nightly once ICEd while compiling tokio.
# That is the right call and it is also a trap: a pin is invisible once it
# works. The fuzz job keeps passing, the corpus keeps growing, and the
# sanitizer, the codegen and the borrow checker all quietly stop moving.
# Months later the fuzzer is exercising a compiler nobody runs.
#
# This check makes the pin expire loudly. It parses every pinned
# `nightly-YYYY-MM-DD` out of the workflow files and fails once one is older
# than MAX_PIN_AGE_DAYS. The fix is either to re-float the toolchain to plain
# `nightly` (once the upstream breakage is gone) or to move the pin forward and
# restate why it is still needed. Raising MAX_PIN_AGE_DAYS is not the fix.
#
# It scans CODE, not comments: every `#` comment is stripped before matching,
# so a date in prose never counts, and every pin does. The narrower earlier
# version matched only `toolchain:` values, which silently missed the
# `cargo +nightly-YYYY-MM-DD metadata` step and the `FUZZ_TOOLCHAIN` env var
# that also pin a compiler; both were left a release behind.
#
# Usage:
#   check-nightly-pin.sh              check the workflow directory
#   check-nightly-pin.sh --selftest   prove the check can fail, on a fixture
#
# Env:
#   MAX_PIN_AGE_DAYS  how old a pin may get before this fails (default 45)
#   WORKFLOW_DIR      directory to scan (default .github/workflows)

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORKFLOW_DIR="${WORKFLOW_DIR:-${REPO_ROOT}/.github/workflows}"
MAX_PIN_AGE_DAYS="${MAX_PIN_AGE_DAYS:-45}"

# Seconds since epoch for a YYYY-MM-DD date. BSD `date` and GNU `date` disagree
# on every flag involved, so try both.
epoch_of() {
  local d="$1"
  date -u -j -f "%Y-%m-%d" "${d}" "+%s" 2>/dev/null && return 0
  date -u -d "${d}" "+%s" 2>/dev/null && return 0
  return 1
}

# Print "<file>:<pin>" for every pinned nightly in the scanned tree, with
# comments removed first so only real configuration counts.
find_pins() {
  local dir="$1" f stripped
  for f in "${dir}"/*.yml "${dir}"/*.yaml; do
    [ -f "${f}" ] || continue
    # Strip a whole-line comment and any trailing " # ..." comment, then list
    # each remaining nightly-YYYY-MM-DD occurrence with its file.
    stripped="$(sed -e 's/^[[:space:]]*#.*$//' -e 's/[[:space:]]#.*$//' "${f}")"
    printf '%s\n' "${stripped}" \
      | grep -oE 'nightly-[0-9]{4}-[0-9]{2}-[0-9]{2}' \
      | while IFS= read -r pin; do printf '%s:%s\n' "${f}" "${pin}"; done
  done
}

check_dir() {
  local dir="$1"
  local now failures found line file pin pin_date pin_epoch age_days
  now="$(date -u "+%s")"
  failures=0
  found=0

  while IFS= read -r line; do
    [ -n "${line}" ] || continue
    file="${line%:*}"
    pin="${line##*:}"
    pin_date="${pin#nightly-}"
    found=$((found + 1))

    if ! pin_epoch="$(epoch_of "${pin_date}")"; then
      echo "::error::could not parse pinned toolchain date '${pin_date}' in ${file}" >&2
      failures=$((failures + 1))
      continue
    fi

    age_days=$(( (now - pin_epoch) / 86400 ))
    if (( age_days > MAX_PIN_AGE_DAYS )); then
      echo "::error::${file} pins ${pin}, which is ${age_days} days old (limit ${MAX_PIN_AGE_DAYS})." >&2
      echo "         Re-float it to plain 'nightly' if the upstream breakage that" >&2
      echo "         justified the pin is fixed, or move the pin forward and update" >&2
      echo "         the comment explaining why it is still needed." >&2
      failures=$((failures + 1))
    else
      echo "nightly-pin: ${file} pins ${pin} (${age_days}d old, limit ${MAX_PIN_AGE_DAYS}d) OK"
    fi
  done < <(find_pins "${dir}" | sort -u || true)

  if (( found == 0 )); then
    echo "nightly-pin: no pinned nightly toolchains in ${dir} (all floating) OK"
  fi

  return $(( failures > 0 ))
}

selftest() {
  local stale fresh out status
  # Not `local`: the EXIT trap below runs after this function has returned, so
  # a local would already be out of scope by the time the trap expands it.
  SELFTEST_WORK="$(mktemp -d "${TMPDIR:-/tmp}/powdb-nightly-pin.XXXXXX")" || {
    echo "::error::nightly-pin: mktemp failed" >&2
    exit 1
  }
  trap 'rm -rf "${SELFTEST_WORK}"' EXIT
  local work="${SELFTEST_WORK}"

  # A date far enough back that it is stale under any sane limit, and one that
  # is today, so neither verdict depends on the wall clock beyond the limit.
  stale="nightly-2000-01-01"
  fresh="nightly-$(date -u '+%Y-%m-%d')"

  # Case 1: a stale pin in each of the three shapes this repo actually uses.
  # If any shape stops being detected, this fails.
  {
    echo "jobs:"
    echo "  a:"
    echo "    steps:"
    echo "      - uses: dtolnay/rust-toolchain@deadbeef"
    echo "        with:"
    echo "          toolchain: ${stale}"
    echo "      - env:"
    echo "          FUZZ_TOOLCHAIN: ${stale}"
    echo "        run: cargo +${stale} metadata --locked"
  } > "${work}/stale.yml"
  out="$(WORKFLOW_DIR="${work}" MAX_PIN_AGE_DAYS="${MAX_PIN_AGE_DAYS}" check_dir "${work}" 2>&1)"
  status=$?
  if [ "${status}" -eq 0 ]; then
    printf '%s\n' "${out}" >&2
    echo "::error::nightly-pin: selftest: a ${stale} pin was accepted; the check cannot fail" >&2
    exit 1
  fi
  echo "nightly-pin: selftest stale-pin case OK (refused, exit ${status})"

  # Case 2: a date that appears only in a comment must NOT be treated as a pin,
  # or every explanation of a past pin would fail the build forever.
  rm -f "${work}/stale.yml"
  {
    echo "jobs:"
    echo "  a:"
    echo "    steps:"
    echo "        # Pinned once to ${stale} because of an upstream ICE."
    echo "      - uses: dtolnay/rust-toolchain@deadbeef"
    echo "        with:"
    echo "          toolchain: ${fresh} # was ${stale}"
  } > "${work}/comment.yml"
  out="$(WORKFLOW_DIR="${work}" MAX_PIN_AGE_DAYS="${MAX_PIN_AGE_DAYS}" check_dir "${work}" 2>&1)"
  status=$?
  if [ "${status}" -ne 0 ]; then
    printf '%s\n' "${out}" >&2
    echo "::error::nightly-pin: selftest: a stale date inside a comment was counted as a pin" >&2
    exit 1
  fi
  case "${out}" in
    *"${fresh}"*) ;;
    *)
      printf '%s\n' "${out}" >&2
      echo "::error::nightly-pin: selftest: the fresh pin ${fresh} was not detected at all" >&2
      exit 1
      ;;
  esac
  echo "nightly-pin: selftest comment/fresh case OK (comment ignored, ${fresh} seen)"
  echo "nightly-pin: selftest ok"
}

case "${1:-}" in
  --selftest) selftest ;;
  '') check_dir "${WORKFLOW_DIR}" || exit 1 ;;
  *) echo "::error::usage: check-nightly-pin.sh [--selftest]" >&2; exit 1 ;;
esac
