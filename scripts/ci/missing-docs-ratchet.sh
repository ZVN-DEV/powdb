#!/usr/bin/env bash
# scripts/ci/missing-docs-ratchet.sh: the public-API documentation ratchet.
#
# Every published library crate is compiled with `-W missing_docs` and the
# number of undocumented public items must EQUAL the number recorded in
# scripts/ci/missing-docs-baseline.txt. Higher fails: a new public item
# shipped without docs. Lower fails too: the baseline must be lowered in the
# same change, so the ratchet only ever tightens and slack can never build
# up (ten items documented in one PR must not license ten undocumented ones
# in the next). Crates at zero get `#![warn(missing_docs)]` in their lib.rs
# as well, so the gap shows up in a local `cargo build` before CI.
#
# Usage: missing-docs-ratchet.sh            enforce the baseline
#        missing-docs-ratchet.sh --update   rewrite the baseline from reality
#        missing-docs-ratchet.sh --selftest prove the counter can see gaps

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${REPO_ROOT}"
BASELINE="scripts/ci/missing-docs-baseline.txt"

# Published crates with a library target (powdb-cli is binary-only).
CRATES=(
  powdb-storage
  powdb-query
  powdb-sync
  powdb-auth
  powdb-backup
  powdb-server
  powdb
)

die() { echo "::error::missing-docs-ratchet: $*" >&2; exit 1; }

# Count `missing documentation` diagnostics for one crate. `cargo rustc -p`
# applies the flag to that crate alone, and cargo replays cached diagnostics
# for a fresh unit, so a warm cache still reports the real count.
count_for() {
  local out
  out="$(cargo rustc -p "$1" --lib -- -W missing_docs 2>&1)" \
    || die "cargo rustc failed for $1: ${out}"
  grep -c '^warning: missing documentation' <<<"${out}" || true
}

if [[ "${1:-}" == "--selftest" ]]; then
  # The counter must be able to see a gap. While any crate in the list still
  # carries undocumented items, at least one count is non-zero; once every
  # crate reaches zero, retire this selftest for one that plants a gap.
  total=0
  for crate in "${CRATES[@]}"; do
    n="$(count_for "${crate}")"
    total=$((total + n))
  done
  (( total > 0 )) \
    || die "selftest: every crate counted zero; either the repo is fully documented (retire this selftest) or the counter is blind"
  echo "missing-docs-ratchet: selftest ok (the counter sees ${total} undocumented items repo-wide)"
  exit 0
fi

if [[ "${1:-}" == "--update" ]]; then
  : > "${BASELINE}"
  for crate in "${CRATES[@]}"; do
    n="$(count_for "${crate}")"
    echo "${crate}=${n}" >> "${BASELINE}"
    echo "${crate}=${n}"
  done
  echo "missing-docs-ratchet: baseline rewritten at ${BASELINE}"
  exit 0
fi

[[ -f "${BASELINE}" ]] || die "no baseline at ${BASELINE}; run with --update to create it"

failed=0
for crate in "${CRATES[@]}"; do
  want="$(grep -E "^${crate}=[0-9]+$" "${BASELINE}" | cut -d= -f2)"
  [[ -n "${want}" ]] || die "no baseline entry for ${crate} in ${BASELINE}"
  got="$(count_for "${crate}")"
  if (( got > want )); then
    echo "::error::${crate}: ${got} undocumented public items, baseline is ${want}. Document the new items (cargo rustc -p ${crate} --lib -- -W missing_docs lists them)." >&2
    failed=1
  elif (( got < want )); then
    echo "::error::${crate}: ${got} undocumented public items, baseline is ${want}. Nice — now lower the baseline in ${BASELINE} to ${got} so the ratchet stays tight." >&2
    failed=1
  else
    echo "missing-docs-ratchet: ${crate} ${got} (matches baseline)"
  fi
done

(( failed == 0 )) || die "baseline mismatch"
echo "missing-docs-ratchet: every crate matches its baseline"
