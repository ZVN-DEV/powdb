#!/usr/bin/env bash
# scripts/ci/strip-empty-unreleased.sh: remove an EMPTY "## Unreleased" section
# from a changelog, in place.
#
# Every published npm tarball ships its package CHANGELOG.md, and every one of
# them opened with
#
#     # Changelog
#
#     ## Unreleased
#
#     ## 0.27.0 - 2026-08-26
#
# so the first thing a reader saw on npm was an empty promise of unreleased
# work. The heading is useful in the repository (it is where the next entry
# goes, and check-version-consistency.sh asserts the root one is empty on main)
# and useless in a published artifact, where "unreleased" cannot mean anything.
#
# This runs at publish time on the working copy only. Nothing is committed: the
# repository keeps its heading, the tarball does not get it.
#
# A section counts as empty only when every line between the heading and the
# next `## ` heading is blank. A section with content is left alone and said
# so, because a release that genuinely shipped unreleased notes should publish
# them rather than have them silently deleted.
#
# Usage:
#   strip-empty-unreleased.sh <changelog path>
#   strip-empty-unreleased.sh --selftest

set -uo pipefail

die() { echo "::error::strip-empty-unreleased: $*" >&2; exit 1; }

# Emits the file with an empty Unreleased section removed. A section with
# content is reproduced verbatim.
strip() {
  awk '
    state == 0 {
      if ($0 ~ /^##[[:space:]]+\[?[Uu]nreleased\]?[[:space:]]*$/) {
        state = 1; heading = $0; buf = ""; nonblank = 0; next
      }
      print; next
    }
    state == 1 {
      if ($0 ~ /^##[[:space:]]/) {
        if (nonblank) { print heading; printf "%s", buf }
        print; state = 2; next
      }
      buf = buf $0 "\n"
      if ($0 ~ /[^[:space:]]/) nonblank = 1
      next
    }
    { print }
    END {
      if (state == 1 && nonblank) { print heading; printf "%s", buf }
    }
  ' "$1"
}

has_unreleased() {
  grep -qE '^##[[:space:]]+\[?[Uu]nreleased\]?[[:space:]]*$' "$1"
}

run() {
  local file="$1" tmp
  [ -f "${file}" ] || die "no such file: ${file}"

  if ! has_unreleased "${file}"; then
    echo "strip-empty-unreleased: ${file} has no Unreleased heading, nothing to do"
    return 0
  fi

  tmp="$(mktemp "${TMPDIR:-/tmp}/powdb-changelog.XXXXXX")" || die "mktemp failed"
  strip "${file}" > "${tmp}" || { rm -f "${tmp}"; die "could not rewrite ${file}"; }

  if cmp -s "${file}" "${tmp}"; then
    rm -f "${tmp}"
    echo "strip-empty-unreleased: ${file} has a NON-EMPTY Unreleased section; left as it is"
    return 0
  fi

  # Refuse to publish a changelog the rewrite emptied out or mangled: the only
  # thing this is allowed to remove is the heading and its blank lines.
  if ! grep -qE '^##[[:space:]]' "${tmp}"; then
    rm -f "${tmp}"
    die "the rewrite of ${file} left no release heading at all; refusing to use it"
  fi

  mv "${tmp}" "${file}" || die "could not replace ${file}"
  echo "strip-empty-unreleased: removed the empty Unreleased section from ${file}"
}

selftest() {
  SELFTEST_WORK="$(mktemp -d "${TMPDIR:-/tmp}/powdb-strip-unreleased.XXXXXX")" || die "mktemp failed"
  trap 'rm -rf "${SELFTEST_WORK}"' EXIT
  local work="${SELFTEST_WORK}"

  # 1. An empty section is removed, and nothing else moves.
  printf '# Changelog\n\n## Unreleased\n\n## 0.27.0 - 2026-08-26\n\nNotes.\n' > "${work}/empty.md"
  run "${work}/empty.md" >/dev/null
  printf '# Changelog\n\n## 0.27.0 - 2026-08-26\n\nNotes.\n' > "${work}/empty.expected"
  cmp -s "${work}/empty.md" "${work}/empty.expected" \
    || { diff "${work}/empty.expected" "${work}/empty.md" >&2; die "selftest: an empty section was not removed cleanly"; }
  grep -q '^## 0.27.0 - 2026-08-26$' "${work}/empty.md" \
    || die "selftest: the release heading did not survive"
  echo "strip-empty-unreleased: selftest empty-section case OK"

  # 2. A section with content is untouched. This is the case that must NOT
  #    fire: silently deleting real release notes would be worse than the wart.
  printf '# Changelog\n\n## Unreleased\n\n- a real note\n\n## 0.27.0 - 2026-08-26\n' > "${work}/full.md"
  cp "${work}/full.md" "${work}/full.expected"
  run "${work}/full.md" >/dev/null
  cmp -s "${work}/full.md" "${work}/full.expected" \
    || { diff "${work}/full.expected" "${work}/full.md" >&2; die "selftest: a non-empty section was modified"; }
  echo "strip-empty-unreleased: selftest non-empty-section case OK"

  # 3. No heading at all is a no-op, not an error.
  printf '# Changelog\n\n## 0.27.0 - 2026-08-26\n' > "${work}/none.md"
  cp "${work}/none.md" "${work}/none.expected"
  run "${work}/none.md" >/dev/null
  cmp -s "${work}/none.md" "${work}/none.expected" \
    || die "selftest: a changelog without an Unreleased heading was modified"
  echo "strip-empty-unreleased: selftest no-heading case OK"

  # 4. Running twice must not keep changing the file.
  run "${work}/empty.md" >/dev/null
  cmp -s "${work}/empty.md" "${work}/empty.expected" \
    || die "selftest: the rewrite is not idempotent"
  echo "strip-empty-unreleased: selftest idempotence case OK"

  # 5. An empty section at end of file is removed without eating the file.
  printf '# Changelog\n\n## 0.27.0 - 2026-08-26\n\nNotes.\n\n## Unreleased\n\n' > "${work}/tail.md"
  run "${work}/tail.md" >/dev/null
  grep -q 'Unreleased' "${work}/tail.md" && die "selftest: a trailing empty section survived"
  grep -q '^## 0.27.0 - 2026-08-26$' "${work}/tail.md" \
    || die "selftest: the trailing case ate the release heading"
  echo "strip-empty-unreleased: selftest trailing-section case OK"
  echo "strip-empty-unreleased: selftest ok"
}

case "${1:-}" in
  --selftest) selftest ;;
  '') die "usage: strip-empty-unreleased.sh <changelog path>|--selftest" ;;
  *) run "$1" ;;
esac
