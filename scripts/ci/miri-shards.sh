#!/usr/bin/env bash
# scripts/ci/miri-shards.sh: the canonical miri filter set for powdb-storage,
# split into balanced shards.
#
# Why this file exists rather than a literal array in ci.yml: miri interprets
# rather than executes, so one job running all seven filters took 18 minutes and
# owned the entire CI critical path while every other job finished inside 7.
# Splitting it across a matrix fixes the wall clock and introduces a new way to
# be wrong: a filter can now be dropped from one shard and nobody notices,
# because the remaining shards still pass. The map lives here so that
#
#   1. no shard filter falls outside the modules miri is meant to run,
#   2. every module in scope is carried by at least one shard,
#   3. no filter is a prefix of another, which would run tests twice,
#   4. the shard names in ci.yml equal the shards defined here, and
#   5. `--check-listing` proves against the REAL test list that every test in
#      scope is selected by exactly one filter,
#
# which together mean neither a filter nor a test can leave the matrix by being
# forgotten. Point 5 is what makes prefix sharding safe: without it, a new
# `btree::tests::verify_something` would match no shard and never run, and
# every shard would still be green. The per-filter "this filter selects at
# least one test" guard still runs inside each shard in ci.yml; that guard
# exists because a libtest filter matching nothing exits 0, and a `tx::tests`
# filter once emptied the job silently.
#
# Usage:
#   miri-shards.sh <shard-name>        validate, then print that shard's filters
#   miri-shards.sh --check             validate only
#   miri-shards.sh --list-shards       print the shard names, one per line
#   miri-shards.sh --check-listing F   assert every test in F that is in scope
#                                      is selected by exactly one shard filter
#                                      (F is `cargo miri test -- --list` output)
#   miri-shards.sh --selftest          prove the guards above can fail
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
#
# This is the scope, not the shard map: every test whose name begins with one
# of these prefixes MUST be selected by exactly one shard filter, which is what
# `--check-listing` asserts against the real test list.
MIRI_MODULES=(
  btree::tests
  page::tests
  row::tests
  types::tests
  view::tests
  pj1::tests
  stored_json_path::tests
)

# Shard assignment. Sharding by module put all of btree in one job, which then
# ran 11 to 18 minutes against a 20 minute timeout and owned the CI critical
# path on its own while the other two shards finished in under two. btree is
# 44 of the tests in scope, so it is split three ways BY TEST-NAME PREFIX
# (13/16/15), spreading the heaviest cases (test_large_tree_splits,
# test_many_inserts_and_lookups, test_non_unique_many_duplicates,
# stats_duplicate_raw_distinct_survives_multi_leaf_delete) across all three.
#
# Prefix sharding introduces a failure mode module sharding did not have: a new
# btree test whose name matches no prefix would be silently unfuzzed by miri
# while every shard still passed. `--check-listing` exists for exactly that and
# runs in every shard job, so a new test outside these prefixes fails the build
# instead of quietly leaving the run.
SHARD_NAMES=(
  btree-core
  btree-nonunique
  btree-stats
  row-page
  json-types-view
)
SHARD_FILTERS=(
  "btree::tests::test_insert btree::tests::test_lookup btree::tests::test_many btree::tests::test_large btree::tests::test_duplicate btree::tests::test_range btree::tests::test_reverse btree::tests::test_string btree::tests::test_save_load"
  "btree::tests::test_non_unique btree::tests::test_delete btree::tests::nonunique_twin btree::tests::lookup_prefix btree::tests::escape_"
  "btree::tests::stats_ btree::tests::bounded_ btree::tests::count_"
  "row::tests page::tests"
  "pj1::tests stored_json_path::tests types::tests view::tests"
)

die() { echo "::error::miri-shards: $*" >&2; exit 1; }

sorted() { printf '%s\n' "$@" | sort; }

# --- validation ------------------------------------------------------------

# Every filter across every shard, one per line.
all_filters() {
  local entry filter
  for entry in "${SHARD_FILTERS[@]}"; do
    for filter in ${entry}; do
      printf '%s\n' "${filter}"
    done
  done
}

validate() {
  (( ${#SHARD_NAMES[@]} == ${#SHARD_FILTERS[@]} )) \
    || die "SHARD_NAMES and SHARD_FILTERS have different lengths"
  (( ${#MIRI_MODULES[@]} > 0 )) \
    || die "MIRI_MODULES is empty; this script would authorise a miri job that runs nothing"

  local union=()
  local entry filter
  for entry in "${SHARD_FILTERS[@]}"; do
    for filter in ${entry}; do
      union+=("${filter}")
    done
  done
  (( ${#union[@]} > 0 )) \
    || die "no shard defines a filter; this script would authorise a miri job that runs nothing"

  local dupes
  dupes="$(sorted "${union[@]}" | uniq -d)"
  [[ -z "${dupes}" ]] \
    || die "these filters appear in more than one shard, so miri would run them twice: $(tr '\n' ' ' <<<"${dupes}")"

  # 1. No filter may be a prefix of another. libtest filters are substring
  #    matches anchored here at the start of the name, so overlapping filters
  #    would run the same tests in two shards and double the wall clock the
  #    sharding is meant to cut.
  local a b
  for a in "${union[@]}"; do
    for b in "${union[@]}"; do
      [[ "${a}" == "${b}" ]] && continue
      if [[ "${b}" == "${a}"* ]]; then
        die "filter '${a}' is a prefix of '${b}', so those tests would run in two shards"
      fi
    done
  done

  # 2. Every filter must live inside a module miri is meant to run, and every
  #    module must be carried by at least one filter. The first stops a shard
  #    smuggling in heap/wal tests miri cannot interpret; the second stops a
  #    whole module falling out of the matrix.
  local module covered
  for filter in "${union[@]}"; do
    local inside=0
    for module in "${MIRI_MODULES[@]}"; do
      if [[ "${filter}" == "${module}" || "${filter}" == "${module}::"* ]]; then
        inside=1
        break
      fi
    done
    (( inside )) \
      || die "filter '${filter}' is not inside any module in MIRI_MODULES; miri cannot interpret the mmap-backed modules"
  done
  for module in "${MIRI_MODULES[@]}"; do
    covered=0
    for filter in "${union[@]}"; do
      if [[ "${filter}" == "${module}" || "${filter}" == "${module}::"* ]]; then
        covered=1
        break
      fi
    done
    (( covered )) \
      || die "module '${module}' is in scope but no shard filter selects any of it"
  done

  # 3. The shard names in ci.yml must be exactly the shards defined here. A
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

# --- coverage against the real test list -----------------------------------

# Read a `cargo miri test -- --list` listing and assert every test inside
# MIRI_MODULES is selected by exactly one shard filter. This is the guard that
# makes prefix sharding safe: without it a new btree test whose name matches no
# prefix would silently never run under miri while all shards stayed green.
check_listing() {
  local listing="$1"
  [[ -f "${listing}" ]] || die "no such listing file: ${listing}"

  local filters=()
  local f
  while IFS= read -r f; do
    filters+=("${f}")
  done < <(all_filters)

  local in_scope=0 unselected=0 doubled=0
  local line name module filter matches

  while IFS= read -r line; do
    # `--list` prints "<name>: test" (and "<name>: benchmark"); everything else
    # is a summary line.
    case "${line}" in
      *": test" | *": benchmark") name="${line%:*}" ;;
      *) continue ;;
    esac

    local scoped=0
    for module in "${MIRI_MODULES[@]}"; do
      if [[ "${name}" == "${module}::"* ]]; then
        scoped=1
        break
      fi
    done
    (( scoped )) || continue
    in_scope=$((in_scope + 1))

    matches=0
    for filter in "${filters[@]}"; do
      if [[ "${name}" == "${filter}"* ]]; then
        matches=$((matches + 1))
      fi
    done

    if (( matches == 0 )); then
      echo "::error::miri-shards: '${name}' is in scope for miri but no shard filter selects it." >&2
      echo "         Add a prefix to SHARD_FILTERS or rename the test to match one." >&2
      unselected=$((unselected + 1))
    elif (( matches > 1 )); then
      echo "::error::miri-shards: '${name}' is selected by ${matches} shard filters, so it runs more than once." >&2
      doubled=$((doubled + 1))
    fi
  done < "${listing}"

  if (( in_scope == 0 )); then
    die "the listing contains no test inside MIRI_MODULES; this guard is inspecting the wrong output"
  fi
  if (( unselected > 0 || doubled > 0 )); then
    die "${unselected} test(s) unselected, ${doubled} test(s) selected twice, out of ${in_scope} in scope"
  fi
  echo "miri-shards: all ${in_scope} in-scope tests are selected by exactly one shard"
}

selftest() {
  SELFTEST_WORK="$(mktemp -d "${TMPDIR:-/tmp}/powdb-miri-shards.XXXXXX")" || die "mktemp failed"
  trap 'rm -rf "${SELFTEST_WORK}"' EXIT
  local work="${SELFTEST_WORK}" out status

  # A listing every filter covers, plus one out-of-scope module that must be
  # ignored rather than demanded.
  {
    all_filters | sed 's/$/_selftest_case: test/'
    echo "heap::tests::mmap_scan: test"
    echo "3 tests, 0 benchmarks"
  } > "${work}/covered.txt"

  out="$(check_listing "${work}/covered.txt" 2>&1)"
  status=$?
  if [ "${status}" -ne 0 ]; then
    printf '%s\n' "${out}" >&2
    die "selftest: a fully covered listing was rejected"
  fi
  case "${out}" in
    *"heap::tests"*)
      printf '%s\n' "${out}" >&2
      die "selftest: an out-of-scope module was counted as in scope"
      ;;
  esac
  echo "miri-shards: selftest covered-listing case OK (${out})"

  # The case prefix sharding introduces: a new in-scope test no prefix selects.
  cp "${work}/covered.txt" "${work}/orphan.txt"
  echo "btree::tests::verify_a_new_thing_nobody_sharded: test" >> "${work}/orphan.txt"
  out="$(check_listing "${work}/orphan.txt" 2>&1)"
  status=$?
  if [ "${status}" -eq 0 ]; then
    printf '%s\n' "${out}" >&2
    die "selftest: an unsharded in-scope test was accepted; prefix sharding is unsafe"
  fi
  case "${out}" in
    *"verify_a_new_thing_nobody_sharded"*) ;;
    *)
      printf '%s\n' "${out}" >&2
      die "selftest: the orphan test was not named in the failure"
      ;;
  esac
  echo "miri-shards: selftest orphan-test case OK (refused, exit ${status})"

  # A listing with nothing in scope must fail rather than pass vacuously.
  printf 'heap::tests::only_this: test\n' > "${work}/empty.txt"
  out="$(check_listing "${work}/empty.txt" 2>&1)"
  status=$?
  if [ "${status}" -eq 0 ]; then
    printf '%s\n' "${out}" >&2
    die "selftest: a listing with no in-scope test passed; the guard is vacuous"
  fi
  echo "miri-shards: selftest empty-listing case OK (refused, exit ${status})"
  echo "miri-shards: selftest ok"
}

# --- entry point -----------------------------------------------------------

mode="${1:-}"
[[ -n "${mode}" ]] || die "usage: miri-shards.sh <shard-name>|--check|--check-listing FILE|--list-shards|--selftest"

if [[ "${mode}" == "--list-shards" ]]; then
  printf '%s\n' "${SHARD_NAMES[@]}"
  exit 0
fi

if [[ "${mode}" == "--selftest" ]]; then
  selftest
  exit 0
fi

if [[ "${mode}" == "--check-listing" ]]; then
  check_listing "${2:-}"
  exit 0
fi

validate

if [[ "${mode}" == "--check" ]]; then
  echo "miri-shards: ${#SHARD_NAMES[@]} shards, $(all_filters | wc -l | tr -d ' ') filters over ${#MIRI_MODULES[@]} modules, matching ${CI_WORKFLOW}." >&2
  exit 0
fi

for i in "${!SHARD_NAMES[@]}"; do
  if [[ "${SHARD_NAMES[$i]}" == "${mode}" ]]; then
    echo "${SHARD_FILTERS[$i]}"
    exit 0
  fi
done
die "unknown shard '${mode}'; known shards: ${SHARD_NAMES[*]}"
