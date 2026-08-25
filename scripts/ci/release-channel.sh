#!/usr/bin/env bash
# scripts/ci/release-channel.sh: classify a release version into its channel
# and print the per-channel publishing decisions release.yml consumes.
#
#   X.Y.Z        final        -> GitHub Release, npm dist-tag `latest`,
#                                 ghcr floating tag `latest`
#   X.Y.Z-rc.N   candidate    -> GitHub pre-release, npm dist-tag `next`,
#                                 ghcr floating tag `rc`
#
# crates.io needs no decision: cargo never resolves a pre-release version
# unless a dependent asks for it explicitly, so an rc on crates.io is
# invisible to `cargo add powdb`.
#
# Any other shape (a bare `-beta`, a build suffix, a missing patch number)
# is refused, so a typo in a tag fails the release instead of quietly
# publishing under a channel nobody intended.
#
# Usage: release-channel.sh <version>    print `key=value` lines
#        release-channel.sh --selftest   prove the classifier can refuse

set -uo pipefail

die() { echo "::error::release-channel: $*" >&2; exit 1; }

classify() {
  local version="$1"
  if [[ "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    printf 'channel=final\nprerelease=false\nnpm_tag=latest\ndocker_floating_tag=latest\n'
  elif [[ "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+-rc\.[0-9]+$ ]]; then
    printf 'channel=candidate\nprerelease=true\nnpm_tag=next\ndocker_floating_tag=rc\n'
  else
    return 1
  fi
}

if [[ "${1:-}" == "--selftest" ]]; then
  [[ "$(classify 0.27.0)" == $'channel=final\nprerelease=false\nnpm_tag=latest\ndocker_floating_tag=latest' ]] \
    || die "selftest: 0.27.0 must classify as final"
  [[ "$(classify 0.27.0-rc.1)" == $'channel=candidate\nprerelease=true\nnpm_tag=next\ndocker_floating_tag=rc' ]] \
    || die "selftest: 0.27.0-rc.1 must classify as candidate"
  for bad in 0.27.0-beta.1 0.27.0-rc1 0.27.0-rc.1+build 0.27 v0.27.0 ""; do
    if classify "${bad}" >/dev/null 2>&1; then
      die "selftest: '${bad}' must be refused, the classifier is not fail-closed"
    fi
  done
  echo "release-channel: selftest ok (final and rc classify, five malformed shapes refused)"
  exit 0
fi

[[ $# -eq 1 && -n "$1" ]] || die "usage: release-channel.sh <version>|--selftest"
classify "$1" || die "unsupported release version '$1': expected X.Y.Z or X.Y.Z-rc.N"
