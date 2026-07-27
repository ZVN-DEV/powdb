#!/usr/bin/env bash
# Fail fast when release/version metadata drifts across Rust crates, TS client,
# changelog, and release checklist. Intentionally dependency-light so it runs in
# CI and on contributor machines with only bash + common Unix tools.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

fail() { echo "version-consistency: FAIL — $*" >&2; exit 1; }
log() { echo "version-consistency: $*"; }

workspace_version="$(sed -n '/^\[workspace\.package\]/,/^\[/{ s/^version = "\([^"]*\)"/\1/p; }' Cargo.toml | head -1)"
[[ -n "$workspace_version" ]] || fail "could not parse [workspace.package] version from Cargo.toml"
log "workspace version: $workspace_version"

# Publishable crates inherit the workspace version. Non-publishable benchmarking
# crates may also inherit it, but they are not part of the release metadata gate.
for manifest in crates/{auth,backup,cli,powdb,query,server,storage,sync}/Cargo.toml; do
  grep -q '^version\.workspace = true$' "$manifest" \
    || fail "$manifest must use version.workspace = true"
done

# Path dependencies between publishable PowDB crates must match the workspace
# version because crates.io cannot resolve path-only specs. Path-only references
# in publish=false helper crates are deliberately ignored.
version_refs="$(grep -RInE '^powdb-[a-z-]+ = \{[^}]*version = "[^"]+"[^}]*path = '   crates/{auth,backup,cli,powdb,query,server,storage,sync}/Cargo.toml || true)"
while IFS=: read -r manifest line text; do
  [[ -n "${manifest:-}" ]] || continue
  dep_version="$(printf '%s\n' "$text" | sed -n 's/.*version = "\([^"]*\)".*/\1/p')"
  dep_name="$(printf '%s\n' "$text" | sed -n 's/^\([^ =]*\).*/\1/p')"
  if [[ "$dep_version" != "$workspace_version" ]]; then
    fail "$manifest:$line $dep_name uses version $dep_version, expected $workspace_version"
  fi
done <<< "$version_refs"

node_bin="${NODE:-node}"
command -v "$node_bin" >/dev/null 2>&1 || fail "node is required to check clients/ts/package.json"
ts_package_version="$($node_bin -p "require('./clients/ts/package.json').version")"
[[ "$ts_package_version" == "$workspace_version" ]] \
  || fail "clients/ts/package.json version $ts_package_version != workspace $workspace_version"

client_version="$(sed -n 's/^export const CLIENT_VERSION = "\([^"]*\)";.*/\1/p' clients/ts/src/index.ts | head -1)"
[[ -n "$client_version" ]] || fail "could not parse CLIENT_VERSION from clients/ts/src/index.ts"
[[ "$client_version" == "$ts_package_version" ]] \
  || fail "CLIENT_VERSION $client_version != TS package version $ts_package_version"

# Embedded Node addon (@zvndev/powdb-embedded) ships in lockstep with the
# workspace version, same as the TS client.
addon_version="$($node_bin -p "require('./bindings/node/package.json').version")"
[[ "$addon_version" == "$workspace_version" ]] \
  || fail "bindings/node/package.json version $addon_version != workspace $workspace_version"

# @zvndev/powdb-sync ships in lockstep with the workspace, and its
# peerDependencies pin the client/embedded versions exactly, so both pins must
# be bumped every release or published sync will demand stale peers.
sync_version="$($node_bin -p "require('./clients/sync/package.json').version")"
[[ "$sync_version" == "$workspace_version" ]] \
  || fail "clients/sync/package.json version $sync_version != workspace $workspace_version"
for peer in @zvndev/powdb-client @zvndev/powdb-embedded; do
  peer_pin="$($node_bin -p "require('./clients/sync/package.json').peerDependencies['$peer']")"
  [[ "$peer_pin" == "$workspace_version" ]] \
    || fail "clients/sync/package.json peerDependency $peer pins $peer_pin, expected $workspace_version"
done

# Release metadata distinguishes the next development version from the latest
# version that is actually published. This prevents a release-prep branch from
# advertising packages or container tags that do not exist yet. Two states are
# valid: released (workspace version == Current release in RELEASES.md) and
# development (workspace version is ahead, and RELEASES.md must announce it as
# the unreleased Next release).
current_release="$(sed -nE 's/.*Current release: v([0-9]+\.[0-9]+\.[0-9]+).*/\1/p' RELEASES.md | head -1)"
[[ -n "$current_release" ]] || fail "could not parse current published release from RELEASES.md"
if [[ "$workspace_version" != "$current_release" ]]; then
  grep -q "Next release: v$workspace_version (unreleased)" RELEASES.md \
    || fail "RELEASES.md next release does not reference unreleased v$workspace_version"
fi

# Deploy examples pin a published ghcr image tag; every such pin must track the
# current published release, never the unreleased workspace version.
example_tags="$(grep -RhoE 'ghcr\.io/zvn-dev/powdb:v[0-9]+\.[0-9]+\.[0-9]+' examples/ | sort -u || true)"
while IFS= read -r ref; do
  [[ -n "$ref" ]] || continue
  ref_version="${ref##*:v}"
  [[ "$ref_version" == "$current_release" ]] \
    || fail "examples pin $ref, expected ghcr.io/zvn-dev/powdb:v$current_release"
done <<< "$example_tags"

# The marketing site quotes versioned banners/output; any vX.Y.Z it mentions
# must be the published release, not the in-progress development version.
site_versions="$(grep -RhoE 'v[0-9]+\.[0-9]+\.[0-9]+' site/*.html | sort -u || true)"
while IFS= read -r ref; do
  [[ -n "$ref" ]] || continue
  [[ "$ref" == "v$current_release" ]] \
    || fail "site/ mentions $ref, expected v$current_release"
done <<< "$site_versions"

# The format and stability policies state what the *published* release
# supports. Both drifted a whole minor behind (they still said v0.19.1 after
# 0.20.0 shipped), which turns a compatibility promise into a guess, so pin the
# version-bearing anchors in each. Historical version references elsewhere in
# these documents are deliberately untouched.
grep -qF "What the current release (v$current_release) supports:" docs/FORMAT.md \
  || fail "docs/FORMAT.md does not state v$current_release as the current release"

grep -qF "Across a patch (\`$current_release\` to " docs/STABILITY.md \
  || fail "docs/STABILITY.md summary table patch column does not start at $current_release"
grep -qF "Across a minor (\`${current_release%.*}\` to " docs/STABILITY.md \
  || fail "docs/STABILITY.md summary table minor column does not start at ${current_release%.*}"

# Install/pin examples must name the published release, never a stale one.
stability_pins="$(grep -ohE -- '--version [0-9]+\.[0-9]+\.[0-9]+' docs/STABILITY.md | sort -u || true)"
while IFS= read -r pin; do
  [[ -n "$pin" ]] || continue
  [[ "$pin" == "--version $current_release" ]] \
    || fail "docs/STABILITY.md pins '$pin', expected --version $current_release"
done <<< "$stability_pins"

npm_pins="$(grep -ohE '"@zvndev/powdb-[a-z]+": "[0-9]+\.[0-9]+\.[0-9]+"' docs/STABILITY.md | sort -u || true)"
while IFS= read -r pin; do
  [[ -n "$pin" ]] || continue
  pin_version="$(printf '%s\n' "$pin" | sed -n 's/.*: "\([^"]*\)"/\1/p')"
  [[ "$pin_version" == "$current_release" ]] \
    || fail "docs/STABILITY.md pins $pin, expected $current_release"
done <<< "$npm_pins"

grep -qE '^## \[Unreleased\]' CHANGELOG.md \
  || fail "CHANGELOG.md is missing the Unreleased section for v$workspace_version work"

# The root CHANGELOG must carry an entry for the release being published. A
# scripted release edit whose anchor drifts can silently no-op (v0.18.1 shipped
# with no entry this way); this guard catches that before publish.
grep -qE "^## \[$current_release\]" CHANGELOG.md \
  || fail "CHANGELOG.md has no [$current_release] entry for the published release"

# The npm client ships its CHANGELOG.md in the tarball; it must at least cover
# the latest published release so package consumers see current release notes.
grep -qE "^## $current_release" clients/ts/CHANGELOG.md \
  || fail "clients/ts/CHANGELOG.md has no entry for published release $current_release"

# SECURITY.md must list the published minor series (e.g. 0.12.x) as supported.
# During development an unreleased workspace series must additionally remain
# explicitly unsupported until it ships.
minor_series="${current_release%.*}.x"
grep -F ':white_check_mark:' SECURITY.md | grep -qF "$minor_series" \
  || fail "SECURITY.md does not list published series $minor_series as supported"
next_minor_series="${workspace_version%.*}.x"
if [[ "$next_minor_series" != "$minor_series" ]]; then
  grep -F ':x: (unreleased)' SECURITY.md | grep -qF "$next_minor_series" \
    || fail "SECURITY.md does not mark development series $next_minor_series as unreleased"
fi

log "development version $workspace_version and published release $current_release are consistent across manifests, deploy examples, site output, changelog, release docs, format/stability policies, and SECURITY.md."
