#!/usr/bin/env bash
# scripts/ci/check-nightly-pin.sh: fail when a pinned nightly toolchain has
# been pinned too long.
#
# `fuzz.yml` pins `nightly-2026-07-22` because the following night's nightly
# ICEd while compiling tokio. That was the right call and it is also a trap: a
# pin is invisible once it works. The fuzz job keeps passing, the corpus keeps
# growing, and the sanitizer, the codegen and the borrow checker all quietly
# stop moving. Months later the fuzzer is exercising a compiler nobody runs.
#
# This check makes the pin expire loudly. It parses every pinned
# `nightly-YYYY-MM-DD` out of the workflow files and fails once one is older
# than MAX_PIN_AGE_DAYS. The fix is either to re-float the toolchain to plain
# `nightly` (once the upstream ICE is gone) or to move the pin forward and
# restate why it is still needed.
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

now="$(date -u "+%s")"
failures=0
found=0

# Match the toolchain value, not any date that happens to appear in a comment:
# only `nightly-YYYY-MM-DD` occurrences that are the value of a `toolchain:`
# key count as a pin.
while IFS= read -r line; do
  file="${line%%:*}"
  pin="$(sed -E 's/.*(nightly-[0-9]{4}-[0-9]{2}-[0-9]{2}).*/\1/' <<<"${line}")"
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
done < <(grep -rnE '^[[:space:]]*toolchain:[[:space:]]*nightly-[0-9]{4}-[0-9]{2}-[0-9]{2}' "${WORKFLOW_DIR}" || true)

if (( found == 0 )); then
  echo "nightly-pin: no pinned nightly toolchains in ${WORKFLOW_DIR} (all floating) OK"
fi

if (( failures > 0 )); then
  exit 1
fi
