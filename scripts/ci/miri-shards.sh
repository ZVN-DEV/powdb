#!/usr/bin/env bash
# scripts/ci/miri-shards.sh: the canonical miri filter set for powdb-storage,
# split into balanced shards.
#
# Why this file exists rather than a literal array in ci.yml: miri interprets
# rather than executes, so one job running all seven filters took 18 minutes and
# owned the entire CI critical path while every other job finished inside 7.
# Splitting it across a matrix fixes the wall clock and introduces a new way to
# be wrong: a filter can now be dropped from one shard and nobody notices,
# because the remaining shards still pass. The list lives here so that
#
#   1. the union of the shards is asserted to equal the canonical set, and
#   2. the shard names in ci.yml are asserted to equal the shards defined here,
#
# which together mean a filter cannot leave the matrix by being forgotten. The
# per-filter "this filter selects at least one test" guard still runs inside
# each shard in ci.yml; that guard exists because a libtest filter matching
# nothing exits 0, and a `tx::tests` filter once emptied the job silently.
#
# Usage:
#   miri-shards.sh <shard-name>   validate, then print that shard's filters
#   miri-shards.sh --check        validate only
#   miri-shards.sh --list-shards  print the shard names, one per line
#
# Env:
#   CI_WORKFLOW  path to the workflow (default .github/workflows/ci.yml)

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CI_WORKFLOW="${CI_WORKFLOW:-${REPO_ROOT}/.github/workflows/ci.yml}"

# The complete set of powdb-storage test modules miri runs. miri cannot
# interpret libc::mmap, so the modules that touch the mmap-based heap (heap,
# catalog, table, disk, wal) and the integration tests are deliberately absent.
# The remainder still gives strong UB coverage of the core data structures and
# the hand-rolled binary formats.
ALL_FILTERS=(
  btree::tests
  page::tests
  row::tests
  types::tests
  view::tests
  pj1::tests
  stored_json_path::tests
)

# Shard assignment, balanced by test count and module size. btree is roughly a
# third of the tests and by far the largest module, so it runs alone; the other
# two shards carry comparable weight.
SHARD_NAMES=(
  btree
  row-page
  json-types-view
)
SHARD_FILTERS=(
  "btree::tests"
  "row::tests page::tests"
  "pj1::tests stored_json_path::tests types::tests view::tests"
)

die() { echo "::error::miri-shards: $*" >&2; exit 1; }

sorted() { printf '%s\n' "$@" | sort; }

# --- validation ------------------------------------------------------------

validate() {
  (( ${#SHARD_NAMES[@]} == ${#SHARD_FILTERS[@]} )) \
    || die "SHARD_NAMES and SHARD_FILTERS have different lengths"
  (( ${#ALL_FILTERS[@]} > 0 )) \
    || die "ALL_FILTERS is empty; this script would authorise a miri job that runs nothing"

  # 1. Every canonical filter must be carried by exactly one shard, and no
  #    shard may invent one. Set equality both ways, so neither a dropped
  #    filter nor a typo survives.
  local union=()
  local entry filter
  for entry in "${SHARD_FILTERS[@]}"; do
    for filter in ${entry}; do
      union+=("${filter}")
    done
  done

  local dupes
  dupes="$(sorted "${union[@]}" | uniq -d)"
  [[ -z "${dupes}" ]] \
    || die "these filters appear in more than one shard, so miri would run them twice: $(tr '\n' ' ' <<<"${dupes}")"

  local want got
  want="$(sorted "${ALL_FILTERS[@]}")"
  got="$(sorted "${union[@]}")"
  if [[ "${want}" != "${got}" ]]; then
    die "the shards do not cover the canonical filter set.
       canonical: $(tr '\n' ' ' <<<"${want}")
       sharded  : $(tr '\n' ' ' <<<"${got}")"
  fi

  # 2. The shard names in ci.yml must be exactly the shards defined here. A
  #    shard added here but not to the workflow would never run, and its
  #    filters would still count as covered by the check above.
  [[ -f "${CI_WORKFLOW}" ]] || die "no workflow at ${CI_WORKFLOW}"
  local yaml_shards
  yaml_shards="$(awk '
    /^  miri:/                        { in_job = 1; next }
    in_job && /^  [a-zA-Z0-9_-]+:[[:space:]]*$/ { in_job = 0 }
    in_job && /^ *shard: \[/ {
      line = $0
      sub(/^ *shard: \[/, "", line)
      sub(/\].*$/, "", line)
      gsub(/[ \t]/, "", line)
      n = split(line, parts, ",")
      for (i = 1; i <= n; i++) if (parts[i] != "") print parts[i]
    }
  ' "${CI_WORKFLOW}" | sort)"

  [[ -n "${yaml_shards}" ]] \
    || die "parsed zero shard names out of the miri job in ${CI_WORKFLOW}; this guard is not working"

  local want_shards
  want_shards="$(sorted "${SHARD_NAMES[@]}")"
  if [[ "${want_shards}" != "${yaml_shards}" ]]; then
    die "ci.yml miri shards do not match this script.
       script: $(tr '\n' ' ' <<<"${want_shards}")
       ci.yml: $(tr '\n' ' ' <<<"${yaml_shards}")"
  fi
}

# --- entry point -----------------------------------------------------------

mode="${1:-}"
[[ -n "${mode}" ]] || die "usage: miri-shards.sh <shard-name>|--check|--list-shards"

if [[ "${mode}" == "--list-shards" ]]; then
  printf '%s\n' "${SHARD_NAMES[@]}"
  exit 0
fi

validate

if [[ "${mode}" == "--check" ]]; then
  echo "miri-shards: ${#SHARD_NAMES[@]} shards cover all ${#ALL_FILTERS[@]} filters, and match ${CI_WORKFLOW}." >&2
  exit 0
fi

for i in "${!SHARD_NAMES[@]}"; do
  if [[ "${SHARD_NAMES[$i]}" == "${mode}" ]]; then
    echo "${SHARD_FILTERS[$i]}"
    exit 0
  fi
done
die "unknown shard '${mode}'; known shards: ${SHARD_NAMES[*]}"
