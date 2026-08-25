#!/usr/bin/env bash
# scripts/ci/fuzz-corpus-replay.sh: deterministic fuzz-corpus replay.
#
# `fuzz.yml` runs a TIME-BUDGETED fuzz campaign: it explores new inputs, which
# makes it valuable and also makes it unfit to be a required merge gate (the
# same commit can pass and fail on consecutive runs). This script is the other
# half: it replays a FIXED, checked-in set of inputs exactly once each with
# `-runs=0`, so the result is a function of the commit alone. That is what can
# be required.
#
# What it guards, concretely: every fuzz target must stay total (Ok/Err, never
# a panic or an abort) over the inputs we have already decided are interesting,
# including the ones that used to crash. A regression that reintroduces a fixed
# crash fails here on the first PR, not on whatever night the campaign happens
# to rediscover it.
#
# It also refuses to run a target with an empty input set. That is the exact
# way the miri job stayed green for months: a filter that selected nothing
# still exits 0, so "the job passed" meant nothing. A target whose seed
# directory is empty is a hole, and this script reports it as a failure.
#
# Env:
#   FUZZ_TARGETS    space-separated target list (default: all checked-in ones)
#   FUZZ_TOOLCHAIN  rust toolchain to use (default: nightly)
#   FUZZ_EXTRA_DIRS extra corpus directories appended to every replay, e.g. a
#                   restored cache; missing directories are ignored

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FUZZ_CRATE_DIR="${REPO_ROOT}/crates/query"
SEEDS_DIR="${FUZZ_CRATE_DIR}/fuzz/seeds"

FUZZ_TOOLCHAIN="${FUZZ_TOOLCHAIN:-nightly}"
DEFAULT_TARGETS="fuzz_lexer fuzz_parser fuzz_roundtrip fuzz_sql fuzz_pj1 fuzz_wire fuzz_wal_replay fuzz_execute fuzz_catalog_open fuzz_sync_segment fuzz_btree_open"
TARGETS="${FUZZ_TARGETS:-${DEFAULT_TARGETS}}"

FAILURES=0
TOTAL_INPUTS=0

log()  { echo "fuzz-replay: $*"; }
fail() { echo "fuzz-replay: FAIL: $*" >&2; FAILURES=$((FAILURES + 1)); }

cd "${FUZZ_CRATE_DIR}" || exit 1

# The checked-in fuzz lockfile must actually describe this build.
#
# `cargo fuzz` has no --locked flag (cargo-fuzz 0.13 rejects it outright), so
# nothing forced the lock to be current and the fuzz build simply regenerated
# it on every run. It sat four minors stale, pinning the powdb crates at 0.21.0
# and still naming a dependency the workspace had dropped, while ci.yml keyed
# its build cache on hashFiles() of that same file: a cache key derived from a
# file the build ignored and that therefore never changed. Asserting it here is
# what makes both the lockfile and that cache key mean something.
if ! lock_check="$(cd fuzz && cargo "+${FUZZ_TOOLCHAIN}" metadata --locked --format-version 1 2>&1 >/dev/null)"; then
  echo "fuzz-replay: FAIL: crates/query/fuzz/Cargo.lock is stale, or the check itself could not run." >&2
  echo "       cargo said: ${lock_check}" >&2
  echo "       If the lock is stale, regenerate it and commit the result:" >&2
  echo "         (cd crates/query/fuzz && cargo metadata --offline --format-version 1 >/dev/null)" >&2
  exit 1
fi

# Every target declared in the fuzz crate must appear in the replay list.
# Without this, adding a `[[bin]]` and forgetting the list here would silently
# leave the new target out of the required gate.
declared="$(grep -oE '^name = "fuzz_[a-z0-9_]+"' fuzz/Cargo.toml | sed -E 's/name = "(.*)"/\1/' | sort)"
listed="$(tr ' ' '\n' <<<"${TARGETS}" | grep -v '^$' | sort)"
if [[ -z "${FUZZ_TARGETS:-}" ]] && [[ "${declared}" != "${listed}" ]]; then
  fail "fuzz/Cargo.toml declares targets that this script does not replay:
       declared: $(tr '\n' ' ' <<<"${declared}")
       replayed: $(tr '\n' ' ' <<<"${listed}")"
fi

log "toolchain : ${FUZZ_TOOLCHAIN}"
log "targets   : ${TARGETS}"
echo

for target in ${TARGETS}; do
  seed_dir="${SEEDS_DIR}/${target}"
  if [[ ! -d "${seed_dir}" ]]; then
    fail "${target}: no checked-in seed directory at fuzz/seeds/${target}"
    continue
  fi
  count="$(find "${seed_dir}" -type f | wc -l | tr -d ' ')"
  if (( count == 0 )); then
    fail "${target}: seed directory fuzz/seeds/${target} is EMPTY; a replay over zero inputs proves nothing"
    continue
  fi
  TOTAL_INPUTS=$((TOTAL_INPUTS + count))

  dirs=("fuzz/seeds/${target}")
  for extra in ${FUZZ_EXTRA_DIRS:-}; do
    if [[ -d "${extra}/${target}" ]] && [[ -n "$(find "${extra}/${target}" -type f -print -quit)" ]]; then
      dirs+=("${extra}/${target}")
    fi
  done

  # fuzz_wal_replay builds its crashed-database template once via
  # std::mem::forget (the same crash-simulation trick the durability suite
  # uses), which LeakSanitizer reports as a one-time leak at exit. Memory-error
  # detection stays fully on; only leak accounting is disabled, and only here.
  asan_opts=""
  if [[ "${target}" == "fuzz_wal_replay" ]]; then
    asan_opts="detect_leaks=0"
  fi

  log "replaying ${target} (${count} checked-in input(s), ${#dirs[@]} dir(s))"
  # `-runs=0` means "load the corpus, execute each input once, stop". No
  # mutation, no time budget, no randomness that changes the verdict.
  if ! ASAN_OPTIONS="${asan_opts}" cargo "+${FUZZ_TOOLCHAIN}" fuzz run "${target}" "${dirs[@]}" -- \
        -runs=0 -timeout=25 -rss_limit_mb=4096; then
    fail "${target}: replay crashed (artifact under crates/query/fuzz/artifacts/${target}/)"
  fi
done

echo
if (( FAILURES > 0 )); then
  echo "fuzz-replay: ${FAILURES} target(s) failed." >&2
  exit 1
fi
echo "fuzz-replay: ALL-PASS (${TOTAL_INPUTS} checked-in inputs across $(wc -w <<<"${TARGETS}" | tr -d ' ') targets)."
