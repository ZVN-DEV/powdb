#!/usr/bin/env bash
# scripts/ci/changelog-section.sh: print one release's CHANGELOG section.
#
# release.yml used `generate_release_notes: true` alone, so the whole body of a
# GitHub Release was a single auto-generated bullet linking the release PR. The
# v0.25.0 entry is 95 curated lines describing a defect that left data
# directories permanently unopenable, and none of it reached the page most
# users actually read. This extracts that section so it can be passed as the
# release body, with the generated PR list still appended underneath.
#
# It fails loudly rather than printing nothing. An empty release body is the
# failure mode this exists to prevent, so "no section found" must stop the
# release, not quietly publish a blank page. The section must also carry real
# content: a heading followed immediately by the next heading is empty, and an
# empty section is the same wrong answer as a missing one.
#
# Usage:
#   changelog-section.sh <version>        e.g. changelog-section.sh 0.26.0
#
# Env:
#   CHANGELOG  path to the changelog (default CHANGELOG.md at the repo root)

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CHANGELOG="${CHANGELOG:-${REPO_ROOT}/CHANGELOG.md}"

die() { echo "::error::changelog-section: $*" >&2; exit 1; }

version="${1:-}"
[[ -n "${version}" ]] || die "usage: changelog-section.sh <version>"
version="${version#v}"

[[ "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || die "'${version}' is not an X.Y.Z version"

[[ -f "${CHANGELOG}" ]] || die "no changelog at ${CHANGELOG}"

# Everything after `## [X.Y.Z]` up to the next `## ` heading, heading excluded.
section="$(awk -v want="## [${version}]" '
  index($0, want) == 1 { in_section = 1; next }
  in_section && /^## / { exit }
  in_section           { print }
' "${CHANGELOG}")"

# Trim leading and trailing blank lines so the release body does not open on
# whitespace, and so the emptiness test below sees the real content.
section="$(printf '%s\n' "${section}" | awk '
  { lines[NR] = $0 }
  END {
    start = 1; while (start <= NR && lines[start] ~ /^[[:space:]]*$/) start++
    end = NR;  while (end >= start && lines[end] ~ /^[[:space:]]*$/) end--
    for (i = start; i <= end; i++) print lines[i]
  }')"

# grep, not `${section//[[:space:]]/}`: bash 3.2 (macOS) implements that
# replacement quadratically, and on the ~11 KB v0.27.0 section it pinned a
# CPU for minutes. Found the hard way cutting v0.27.0; CI's bash 5 masked it.
if ! printf '%s' "${section}" | grep -q '[^[:space:]]'; then
  die "CHANGELOG.md has no content under '## [${version}]'.
       The GitHub Release body is built from that section, and publishing an
       empty body is the exact failure this check exists to prevent. Add the
       entry (or move it out of Unreleased) before tagging."
fi

printf '%s\n' "${section}"
