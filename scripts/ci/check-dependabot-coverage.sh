#!/usr/bin/env bash
# scripts/ci/check-dependabot-coverage.sh: every dependency manifest in this
# repository must be watched by .github/dependabot.yml.
#
# The config is a hand-maintained list, which means it is a list of what
# somebody remembered. It covered three of six manifests for months: the sync
# and node-addon npm packages (both published), the node addon's own detached
# Cargo workspace, the fuzz workspace's lock, and the Dockerfile's base images
# all aged unwatched, and nothing said so, because a dependabot config cannot
# fail. This makes it fail.
#
# It walks the tracked manifests rather than a second hand-written list, so a
# new crate, package or Dockerfile is covered the day it lands or the build
# turns red naming it.
#
# Usage:
#   check-dependabot-coverage.sh              check this repository
#   check-dependabot-coverage.sh --selftest   prove the check can fail
#
# Env:
#   DEPENDABOT_CONFIG  path to the config (default .github/dependabot.yml)

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CONFIG="${DEPENDABOT_CONFIG:-${REPO_ROOT}/.github/dependabot.yml}"

die() { echo "::error::dependabot-coverage: $*" >&2; exit 1; }

# Print "<ecosystem> <directory>" for every entry in the config. Parsed with
# awk rather than a YAML library so this runs anywhere bash does; the config is
# a flat list of two-key entries, which is well within what that can read.
config_entries() {
  awk '
    /^[[:space:]]*-[[:space:]]*package-ecosystem:/ {
      eco = $0
      sub(/.*package-ecosystem:[[:space:]]*/, "", eco)
      gsub(/["\047[:space:]]/, "", eco)
      next
    }
    /^[[:space:]]*directory:/ {
      dir = $0
      sub(/.*directory:[[:space:]]*/, "", dir)
      gsub(/["\047[:space:]]/, "", dir)
      if (eco != "") { print eco " " dir; eco = "" }
    }
  ' "$1"
}

# Print "<ecosystem> <directory>" for every manifest that needs watching.
# `git ls-files` so untracked scratch files and vendored trees never count.
required_entries() {
  local root="$1" f dir
  (
    cd "${root}" || exit 1
    git ls-files 'Cargo.toml' '*/Cargo.toml' '**/Cargo.toml' \
                 'package.json' '*/package.json' '**/package.json' \
                 'Dockerfile' '*/Dockerfile' '**/Dockerfile' 2>/dev/null
  ) | while IFS= read -r f; do
    dir="$(dirname "${f}")"
    [ "${dir}" = "." ] && dir=""
    case "${f}" in
      # A crate inside the root workspace is covered by the root cargo entry;
      # only a manifest that declares its own [workspace] is invisible to it.
      */Cargo.toml)
        if grep -qE '^\[workspace\]' "${root}/${f}"; then
          printf 'cargo /%s\n' "${dir#/}"
        fi
        ;;
      Cargo.toml) printf 'cargo /\n' ;;
      # A package.json with no dependencies of its own (a bare scratch or
      # fixture manifest) has nothing to update.
      */package.json | package.json)
        if grep -qE '"(dependencies|devDependencies|peerDependencies)"' "${root}/${f}"; then
          if [ -z "${dir}" ]; then printf 'npm /\n'; else printf 'npm /%s\n' "${dir#/}"; fi
        fi
        ;;
      Dockerfile) printf 'docker /\n' ;;
      */Dockerfile) printf 'docker /%s\n' "${dir#/}" ;;
    esac
  done | sort -u
}

check() {
  local root="$1" config="$2"
  [ -f "${config}" ] || die "no dependabot config at ${config}"

  local have want missing extra
  have="$(config_entries "${config}" | sort -u)"
  want="$(required_entries "${root}")"

  [ -n "${have}" ] || die "parsed no entries out of ${config}; this check is not working"
  [ -n "${want}" ] || die "found no dependency manifests under ${root}; this check is not working"

  missing="$(comm -23 <(printf '%s\n' "${want}") <(printf '%s\n' "${have}"))"
  if [ -n "${missing}" ]; then
    echo "::error::dependabot-coverage: these manifests are not watched by ${config}:" >&2
    sed 's/^/         /' <<<"${missing}" >&2
    echo "         Add a matching package-ecosystem + directory entry, or delete the manifest." >&2
    exit 1
  fi

  # An entry pointing at a directory that no longer has a manifest is dead
  # config, and dead config is how a real gap hides in a long list.
  extra="$(comm -13 <(printf '%s\n' "${want}") <(printf '%s\n' "${have}") \
            | grep -v '^github-actions ' || true)"
  if [ -n "${extra}" ]; then
    echo "::error::dependabot-coverage: these entries watch nothing:" >&2
    sed 's/^/         /' <<<"${extra}" >&2
    exit 1
  fi

  echo "dependabot-coverage: all $(printf '%s\n' "${want}" | wc -l | tr -d ' ') manifest(s) watched"
}

selftest() {
  SELFTEST_WORK="$(mktemp -d "${TMPDIR:-/tmp}/powdb-dependabot.XXXXXX")" || die "mktemp failed"
  trap 'rm -rf "${SELFTEST_WORK}"' EXIT
  local work="${SELFTEST_WORK}" out status

  mkdir -p "${work}/repo/sub" "${work}/repo/.github"
  (
    cd "${work}/repo" || exit 1
    git init -q .
    printf '[workspace]\n' > Cargo.toml
    printf '[workspace]\n' > sub/Cargo.toml
    git -c user.email=t@t -c user.name=t add Cargo.toml sub/Cargo.toml >/dev/null
    git -c user.email=t@t -c user.name=t commit -qm fixture >/dev/null
  ) || die "selftest: could not build the fixture repository"

  # Config that watches only the root manifest: the sub workspace is missed,
  # which is precisely the shape of the real gap.
  cat > "${work}/repo/.github/dependabot.yml" <<'EOF'
version: 2
updates:
  - package-ecosystem: cargo
    directory: /
EOF
  out="$(check "${work}/repo" "${work}/repo/.github/dependabot.yml" 2>&1)"
  status=$?
  if [ "${status}" -eq 0 ]; then
    printf '%s\n' "${out}" >&2
    die "selftest: an unwatched manifest was accepted; this check cannot fail"
  fi
  case "${out}" in
    *"cargo /sub"*) ;;
    *)
      printf '%s\n' "${out}" >&2
      die "selftest: the unwatched manifest was not named in the failure"
      ;;
  esac
  echo "dependabot-coverage: selftest missing-entry case OK (refused, exit ${status})"

  # Same fixture, both manifests watched: must pass, which is what proves the
  # case above was a verdict and not a check that always fails.
  cat > "${work}/repo/.github/dependabot.yml" <<'EOF'
version: 2
updates:
  - package-ecosystem: cargo
    directory: /
  - package-ecosystem: cargo
    directory: /sub
EOF
  out="$(check "${work}/repo" "${work}/repo/.github/dependabot.yml" 2>&1)"
  status=$?
  if [ "${status}" -ne 0 ]; then
    printf '%s\n' "${out}" >&2
    die "selftest: a fully watched fixture was rejected"
  fi
  echo "dependabot-coverage: selftest complete-config case OK (${out})"

  # And an entry watching a directory with no manifest must be refused too.
  cat > "${work}/repo/.github/dependabot.yml" <<'EOF'
version: 2
updates:
  - package-ecosystem: cargo
    directory: /
  - package-ecosystem: cargo
    directory: /sub
  - package-ecosystem: npm
    directory: /nothing-here
EOF
  out="$(check "${work}/repo" "${work}/repo/.github/dependabot.yml" 2>&1)"
  status=$?
  if [ "${status}" -eq 0 ]; then
    printf '%s\n' "${out}" >&2
    die "selftest: an entry watching nothing was accepted"
  fi
  echo "dependabot-coverage: selftest dead-entry case OK (refused, exit ${status})"
  echo "dependabot-coverage: selftest ok"
}

case "${1:-}" in
  --selftest) selftest ;;
  '') check "${REPO_ROOT}" "${CONFIG}" ;;
  *) die "usage: check-dependabot-coverage.sh [--selftest]" ;;
esac
