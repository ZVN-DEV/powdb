#!/usr/bin/env bash
# scripts/ci/cross-version-compat.sh: on-disk format compatibility against the
# REAL released binaries, in both directions.
#
# Why this exists: every compatibility claim in the test suite is asserted
# against bytes this repo writes itself. `crates/storage/tests/
# catalog_v7_migration.rs` builds a "v6 catalog" by calling today's writer and
# stopping short of the v7 trigger; that proves today's reader can read today's
# writer, which is not the same thing as reading what v0.19.1 actually shipped.
# If the v6 encoder had ever been wrong, the fixture would be wrong in exactly
# the same way and the test would still pass. This script removes the shared
# author: it downloads binaries that were built, released and installed by
# users, and makes them write the bytes.
#
# Three legs, each of which must hold:
#
#   FORWARD    an old released binary creates and populates a data dir;
#              HEAD opens it and must read back the exact rows, the index, the
#              declared link, and must be able to keep writing to it.
#
#   DOWNGRADE-COMPATIBLE
#              HEAD creates a data dir that never touches a lazily-activated
#              feature; every old binary must open it and read the exact rows.
#              This is the promise that lets an operator roll back a deploy.
#
#   DOWNGRADE-REFUSED
#              HEAD creates a data dir that DID activate the newest lazy
#              feature (an entity link, which lifts the catalog to v7); a
#              binary released before that feature must refuse it with a clean
#              version error and a normal non-zero exit, not a panic, not a
#              signal, and above all not a partial read that silently drops the
#              rows it did not understand.
#
# The version list is DERIVED, not written down. It used to be a literal
# ("v0.19.1 v0.20.0 v0.21.0") that nothing bumped, so v0.22.0 through v0.24.0
# shipped with no on-disk coverage at all, including the v0.25.0 release whose
# headline fix was a data directory that could be left permanently unopenable.
# The default now asks the release list for the newest patch of each of the last
# POWDB_COMPAT_MINORS published minor series, and the pinned fallback used on a
# machine without `gh` is itself checked against the workspace version, so a
# stale pin fails the run instead of quietly shrinking the matrix.
#
# Why 5 minors: docs/FORMAT.md documents a 4-minor deprecation floor (a legacy
# read branch may only be removed once 4 minors have shipped since the release
# that superseded it), so 4 back plus the current release is exactly the window
# in which compatibility is still promised.
#
# Env:
#   POWDB_CLI                  HEAD powdb-cli (default target/release/powdb-cli)
#   POWDB_COMPAT_VERSIONS      space-separated release tags to test forward and
#                              downgrade-compatible against. Default: derived
#                              from `gh release list`, falling back to a pinned
#                              list that is validated, not trusted.
#   POWDB_COMPAT_MINORS        how many published minor series the derived list
#                              covers (default 5, see above)
#   POWDB_COMPAT_FLOOR         release tag predating the newest lazily-activated
#                              on-disk feature, used for DOWNGRADE-REFUSED
#                              (default "v0.18.2", the last catalog-v6 release)
#   POWDB_COMPAT_CACHE         where to keep downloaded binaries
#   POWDB_COMPAT_REPO          owner/name to download releases from
#
# Flags:
#   --print-plan               print `versions=<list>` and `floor=<tag>` and
#                              exit 0. A caller keys its binary cache on this so
#                              the key is derived from the same list the run
#                              uses, instead of a second hand-maintained copy.
#
# Exits 0 only when every leg of every version passed.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

POWDB_CLI="${POWDB_CLI:-${REPO_ROOT}/target/release/powdb-cli}"
COMPAT_FLOOR="${POWDB_COMPAT_FLOOR:-v0.18.2}"
COMPAT_MINORS="${POWDB_COMPAT_MINORS:-5}"
COMPAT_REPO="${POWDB_COMPAT_REPO:-ZVN-DEV/powdb}"
CACHE_DIR="${POWDB_COMPAT_CACHE:-${REPO_ROOT}/target/compat-bins}"

# Used only when `gh` cannot answer (no CLI, no network, no auth). Validated
# against the workspace version below, so it cannot silently fall behind.
COMPAT_VERSIONS_FALLBACK="v0.21.0 v0.22.0 v0.23.0 v0.24.0 v0.25.0"

# Every tested version must understand the newest lazily-activated on-disk
# feature, because the lazy-accept leg requires it. Entity links (catalog v7)
# landed in v0.19.0, so that is the oldest tag this matrix may contain; the
# DOWNGRADE-REFUSED leg deliberately uses an older binary via COMPAT_FLOOR.
COMPAT_FEATURE_FLOOR="v0.19.0"

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/powdb-compat-XXXXXX")"
FAILURES=0
CHECKS=0

cleanup() {
  if [[ -d "${WORK_DIR}" ]]; then rm -rf "${WORK_DIR}"; fi
}
trap cleanup EXIT

log()  { echo "compat: $*"; }
fail() {
  echo "compat: FAIL: $*" >&2
  FAILURES=$((FAILURES + 1))
}
die()  { echo "compat: FATAL: $*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Version list resolution
#
# The list this job runs against must track the releases that actually exist.
# When it was a literal in this file, nothing bumped it and three consecutive
# minors shipped with zero coverage. Derive it instead, then validate whatever
# was derived: a `gh` call that returns nothing, or a fallback pin that has
# aged out, would otherwise leave the loops below iterating over an empty or
# truncated list and still printing ALL-PASS.
# ---------------------------------------------------------------------------

# "v0.25.0" -> 25 (major-scaled so it stays correct past 1.0)
minor_key() {
  local v="${1#v}" maj rest min
  maj="${v%%.*}"
  rest="${v#*.}"
  min="${rest%%.*}"
  echo $(( maj * 1000 + min ))
}

# Newest patch of each of the last COMPAT_MINORS published minor series, oldest
# first. Drafts and prereleases are excluded by field, not by tag shape, so a
# `v0.27.0` draft cut before its release cannot enter the matrix.
derive_versions() {
  command -v gh >/dev/null 2>&1 || return 1
  local tags
  tags="$(gh release list --repo "${COMPAT_REPO}" --limit 200 \
            --json tagName,isDraft,isPrerelease \
            --jq '.[] | select(.isDraft == false and .isPrerelease == false) | .tagName' \
          2>/dev/null)" || return 1
  [[ -n "${tags}" ]] || return 1
  printf '%s\n' "${tags}" \
    | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' \
    | sed 's/^v//' \
    | sort -t. -k1,1n -k2,2n -k3,3n \
    | awk -F. '
        { key = $1 "." $2
          if (key != prev) { if (prev != "") print last; prev = key }
          last = $0 }
        END { if (prev != "") print last }' \
    | tail -n "${COMPAT_MINORS}" \
    | sed 's/^/v/' \
    | tr '\n' ' ' \
    | sed 's/ $//'
}

# Refuse a list that cannot do the job, whatever produced it.
validate_versions() {
  local list="$1" origin="$2"
  [[ -n "${list//[[:space:]]/}" ]] \
    || die "${origin} produced an empty version list; this job would test nothing"

  local ws ws_key floor_key newest_key tag key
  ws="$(sed -n '/^\[workspace\.package\]/,/^\[/{ s/^version = "\([^"]*\)"/\1/p; }' \
          "${REPO_ROOT}/Cargo.toml" | head -1)"
  [[ -n "${ws}" ]] || die "could not parse [workspace.package] version from Cargo.toml"
  ws_key="$(minor_key "v${ws}")"
  floor_key="$(minor_key "${COMPAT_FEATURE_FLOOR}")"
  newest_key=0

  for tag in ${list}; do
    if [[ ! "${tag}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
      die "${origin} produced '${tag}', which is not a vX.Y.Z release tag"
    fi
    key="$(minor_key "${tag}")"
    if (( key < floor_key )); then
      die "${origin} produced ${tag}, older than ${COMPAT_FEATURE_FLOOR}; the lazy-accept leg requires a release that understands entity links"
    fi
    (( key > newest_key )) && newest_key="${key}"
  done

  # The newest tested release must be the one immediately below the version
  # under development. Anything further back means a release shipped without
  # entering this matrix, which is the exact drift that let v0.22.0 through
  # v0.24.0 go untested.
  if (( newest_key < ws_key - 1 )); then
    die "${origin} newest entry is a released minor below v${ws%.*}: the matrix has fallen behind the workspace version ${ws}. List: ${list}"
  fi
}

if [[ -n "${POWDB_COMPAT_VERSIONS:-}" ]]; then
  COMPAT_VERSIONS="${POWDB_COMPAT_VERSIONS}"
  VERSIONS_ORIGIN="POWDB_COMPAT_VERSIONS"
elif COMPAT_VERSIONS="$(derive_versions)" && [[ -n "${COMPAT_VERSIONS}" ]]; then
  VERSIONS_ORIGIN="gh release list"
else
  COMPAT_VERSIONS="${COMPAT_VERSIONS_FALLBACK}"
  VERSIONS_ORIGIN="pinned fallback (gh unavailable)"
  echo "compat: WARNING: could not reach ${COMPAT_REPO} releases; using the pinned fallback list" >&2
fi
validate_versions "${COMPAT_VERSIONS}" "${VERSIONS_ORIGIN}"

if [[ "${1:-}" == "--print-plan" ]]; then
  echo "versions=${COMPAT_VERSIONS}"
  echo "floor=${COMPAT_FLOOR}"
  exit 0
fi

# ---------------------------------------------------------------------------
# Assertion helpers. Every check is value-level: an exit code alone would pass
# on a binary that opened the file and returned nothing.
# ---------------------------------------------------------------------------

# assert_exec <label> <cli> <data-dir> <powql> <expected-substring>
assert_exec() {
  local label="$1" cli="$2" dir="$3" q="$4" want="$5"
  CHECKS=$((CHECKS + 1))
  local out rc
  out="$("${cli}" --data-dir "${dir}" --exec "${q}" 2>&1)"
  rc=$?
  if (( rc != 0 )); then
    fail "${label}: query exited ${rc}: ${q}
       output: ${out}"
    return 1
  fi
  if ! grep -qF -- "${want}" <<<"${out}"; then
    fail "${label}: expected to find '${want}' in output of: ${q}
       output: ${out}"
    return 1
  fi
  return 0
}

# assert_refused <label> <cli> <data-dir> <powql> <expected-error-substring>
# The binary must exit non-zero, below the 128 signal band (a SIGABRT from
# panic=abort surfaces as 134, and "it crashed" is not "it refused"), and say
# the expected thing.
assert_refused() {
  local label="$1" cli="$2" dir="$3" q="$4" want="$5"
  CHECKS=$((CHECKS + 1))
  local out rc
  out="$("${cli}" --data-dir "${dir}" --exec "${q}" 2>&1)"
  rc=$?
  if (( rc == 0 )); then
    fail "${label}: expected refusal, but the query SUCCEEDED: ${q}
       output: ${out}"
    return 1
  fi
  if (( rc >= 128 )); then
    fail "${label}: expected a clean error exit, got signal-band status ${rc}: ${q}
       output: ${out}"
    return 1
  fi
  if ! grep -qF -- "${want}" <<<"${out}"; then
    fail "${label}: refusal message did not mention '${want}': ${q}
       output: ${out}"
    return 1
  fi
  return 0
}

# ---------------------------------------------------------------------------
# Binary acquisition
# ---------------------------------------------------------------------------

platform_suffix() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "${os}/${arch}" in
    Linux/x86_64)  echo "linux-x86_64" ;;
    Darwin/arm64)  echo "macos-aarch64" ;;
    *)
      echo "compat: no released asset for ${os}/${arch}" >&2
      return 1
      ;;
  esac
}

SUFFIX="$(platform_suffix)" || exit 1

fetch_cli() {
  local tag="$1"
  local dest="${CACHE_DIR}/powdb-cli-${tag}-${SUFFIX}"
  if [[ -x "${dest}" ]]; then
    echo "${dest}"
    return 0
  fi
  mkdir -p "${CACHE_DIR}"
  local url="https://github.com/${COMPAT_REPO}/releases/download/${tag}/powdb-cli-${SUFFIX}"
  if ! curl -fsSL "${url}" -o "${dest}.part"; then
    echo "compat: could not download ${url}" >&2
    return 1
  fi
  chmod +x "${dest}.part"
  # macOS quarantines curl-downloaded executables; strip it or exec fails.
  xattr -d com.apple.quarantine "${dest}.part" 2>/dev/null || true
  mv "${dest}.part" "${dest}"
  echo "${dest}"
}

# ---------------------------------------------------------------------------
# Fixture builders. Both binaries speak the same PowQL here on purpose: the
# point is that the BYTES differ, not the language.
# ---------------------------------------------------------------------------

# Baseline schema + rows: nothing that lifts the on-disk catalog past the
# floor release's ceiling.
seed_baseline() {
  local cli="$1" dir="$2" label="$3"
  "${cli}" --data-dir "${dir}" --exec \
    'type User { required unique id: int, required name: str, age: int }' >/dev/null 2>&1 \
    || { fail "${label}: could not create type User"; return 1; }
  "${cli}" --data-dir "${dir}" --exec \
    'type Note { required unique id: int, user_id: int, body: str }' >/dev/null 2>&1 \
    || { fail "${label}: could not create type Note"; return 1; }
  local i
  for i in 1 2 3; do
    "${cli}" --data-dir "${dir}" --exec \
      "insert User { id := ${i}, name := \"user-${i}\", age := $((20 + i)) }" >/dev/null 2>&1 \
      || { fail "${label}: could not insert User ${i}"; return 1; }
    "${cli}" --data-dir "${dir}" --exec \
      "insert Note { id := ${i}, user_id := ${i}, body := \"note-${i}\" }" >/dev/null 2>&1 \
      || { fail "${label}: could not insert Note ${i}"; return 1; }
  done
  return 0
}

# Lift the data dir onto the newest lazily-activated on-disk feature. Declaring
# an entity link is what promotes the catalog from v6 to v7.
activate_link() {
  local cli="$1" dir="$2" label="$3"
  "${cli}" --data-dir "${dir}" --exec 'link Note.user -> User on user_id = id' >/dev/null 2>&1 \
    || { fail "${label}: could not declare entity link"; return 1; }
  return 0
}

# What every reader must be able to see in a baseline data dir.
assert_baseline_readable() {
  local label="$1" cli="$2" dir="$3"
  assert_exec "${label}" "${cli}" "${dir}" 'count(User)' '3'
  assert_exec "${label}" "${cli}" "${dir}" 'count(Note)' '3'
  # Indexed point read: exercises the persisted B-tree, not just the heap.
  assert_exec "${label}" "${cli}" "${dir}" 'User filter .id = 2 { .name }' 'user-2'
  # Non-indexed filter: exercises the heap row decoder for a nullable int.
  assert_exec "${label}" "${cli}" "${dir}" 'User filter .age > 22 { .name }' 'user-3'
  assert_exec "${label}" "${cli}" "${dir}" 'Note filter .id = 3 { .body }' 'note-3'
}

# ---------------------------------------------------------------------------
main() {
  if [[ ! -x "${POWDB_CLI}" ]]; then
    echo "compat: HEAD powdb-cli not found at ${POWDB_CLI}" >&2
    echo "        build it with: cargo build --release -p powdb-cli" >&2
    exit 1
  fi

  log "HEAD cli    : ${POWDB_CLI}"
  log "platform    : ${SUFFIX}"
  log "versions    : ${COMPAT_VERSIONS}"
  log "version src : ${VERSIONS_ORIGIN}"
  log "floor       : ${COMPAT_FLOOR}"
  echo

  # ── LEG 1: FORWARD (old writes, HEAD reads and keeps writing) ────────────
  local tag old_cli dir
  for tag in ${COMPAT_VERSIONS}; do
    old_cli="$(fetch_cli "${tag}")" || { fail "forward ${tag}: download failed"; continue; }
    dir="${WORK_DIR}/forward-${tag}"
    mkdir -p "${dir}"

    log "FORWARD ${tag}: ${tag} writes, HEAD reads"
    seed_baseline "${old_cli}" "${dir}" "forward ${tag}" || continue
    activate_link "${old_cli}" "${dir}" "forward ${tag}" || continue

    assert_baseline_readable "forward ${tag}" "${POWDB_CLI}" "${dir}"
    # The link the OLD binary persisted must be visible to HEAD's introspection
    # and usable as a traversal, not merely present as bytes.
    assert_exec "forward ${tag}" "${POWDB_CLI}" "${dir}" 'schema links' 'user'
    assert_exec "forward ${tag}" "${POWDB_CLI}" "${dir}" \
      'Note as n filter n.id = 1 { n.body, owner: n.user.name }' 'user-1'
    # HEAD must be able to keep writing into a data dir it did not create, and
    # the old binary's unique index must still reject a duplicate.
    assert_exec "forward ${tag}" "${POWDB_CLI}" "${dir}" \
      'insert User { id := 4, name := "user-4", age := 40 }' '1 row'
    assert_exec "forward ${tag}" "${POWDB_CLI}" "${dir}" 'count(User)' '4'
    assert_refused "forward ${tag}" "${POWDB_CLI}" "${dir}" \
      'insert User { id := 4, name := "dup", age := 1 }' 'unique'
  done

  # ── LEG 2: DOWNGRADE-COMPATIBLE (HEAD writes baseline, old reads) ────────
  local plain_dir="${WORK_DIR}/head-plain"
  mkdir -p "${plain_dir}"
  seed_baseline "${POWDB_CLI}" "${plain_dir}" "downgrade-compatible HEAD seed"
  for tag in ${COMPAT_VERSIONS} "${COMPAT_FLOOR}"; do
    old_cli="$(fetch_cli "${tag}")" || { fail "downgrade ${tag}: download failed"; continue; }
    log "DOWNGRADE-COMPATIBLE ${tag}: HEAD wrote it, ${tag} must read it"
    assert_baseline_readable "downgrade ${tag}" "${old_cli}" "${plain_dir}"
  done

  # ── LEG 3: DOWNGRADE-REFUSED (HEAD activates the lazy feature) ───────────
  local lazy_dir="${WORK_DIR}/head-lazy"
  mkdir -p "${lazy_dir}"
  seed_baseline "${POWDB_CLI}" "${lazy_dir}" "downgrade-refused HEAD seed"
  activate_link "${POWDB_CLI}" "${lazy_dir}" "downgrade-refused HEAD seed"

  # Sanity: HEAD itself must still read what it just wrote. Without this, a
  # HEAD that produced an unreadable data dir would make leg 3 pass for the
  # wrong reason (everything refuses an unreadable file).
  assert_baseline_readable "downgrade-refused self-check" "${POWDB_CLI}" "${lazy_dir}"

  old_cli="$(fetch_cli "${COMPAT_FLOOR}")" \
    || fail "downgrade-refused ${COMPAT_FLOOR}: download failed"
  if [[ -n "${old_cli:-}" && -x "${old_cli}" ]]; then
    log "DOWNGRADE-REFUSED ${COMPAT_FLOOR}: must refuse the activated catalog cleanly"
    assert_refused "downgrade-refused ${COMPAT_FLOOR}" "${old_cli}" "${lazy_dir}" \
      'count(User)' 'unsupported catalog version'
    # A refusal that still leaks rows is not a refusal.
    local out
    out="$("${old_cli}" --data-dir "${lazy_dir}" --exec 'User { .name }' 2>&1)"
    CHECKS=$((CHECKS + 1))
    if grep -qF 'user-1' <<<"${out}"; then
      fail "downgrade-refused ${COMPAT_FLOOR}: refused reader still emitted row data:
       ${out}"
    fi
  fi

  # Releases at or above the feature must accept it rather than refuse it,
  # which is what keeps leg 3 from degenerating into "old binaries fail".
  for tag in ${COMPAT_VERSIONS}; do
    old_cli="$(fetch_cli "${tag}")" || { fail "lazy-accept ${tag}: download failed"; continue; }
    log "DOWNGRADE-COMPATIBLE(lazy) ${tag}: must ACCEPT the activated catalog"
    assert_baseline_readable "lazy-accept ${tag}" "${old_cli}" "${lazy_dir}"
    assert_exec "lazy-accept ${tag}" "${old_cli}" "${lazy_dir}" 'schema links' 'user'
  done

  echo
  if (( FAILURES > 0 )); then
    echo "compat: ${FAILURES} failure(s) across ${CHECKS} checks." >&2
    return 1
  fi
  echo "compat: ALL-PASS (${CHECKS} checks)."
  return 0
}

main "$@"
