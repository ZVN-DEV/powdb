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

# Every tracked Cargo.lock must pin the workspace version for the local powdb
# crates. This is the check that stops the stale-lockfile pattern recurring: it
# hit bindings/node/Cargo.lock last audit (fixed at 0.25.0) and immediately
# reappeared in crates/query/fuzz/Cargo.lock, which sat at 0.21.0 for four
# minors because the fuzz build never passes --locked and simply regenerated it
# on every run. A lockfile nothing validates is a lockfile that drifts.
#
# Local crates that deliberately carry their own version, and why:
#   powdb-query-fuzz  a fuzz harness, publish = false, never released, 0.0.0
lock_exempt_crates=" powdb-query-fuzz "

tracked_locks="$(git ls-files '*Cargo.lock' 2>/dev/null || true)"
[[ -n "$tracked_locks" ]] \
  || fail "found no tracked Cargo.lock files; this check cannot be doing anything"
lock_count=0
while IFS= read -r lock; do
  [[ -n "$lock" ]] || continue
  lock_count=$((lock_count + 1))
  # A [[package]] block with no `source =` is a local (path) crate. Blocks are
  # blank-line separated, and `source` always follows `version`, so the verdict
  # is known by the time the block ends.
  local_pkgs="$(awk '
    /^\[\[package\]\]/ { name=""; ver=""; src=0; next }
    /^name = "/    { name = $0; sub(/^name = "/, "", name); sub(/"$/, "", name); next }
    /^version = "/ { ver  = $0; sub(/^version = "/, "", ver);  sub(/"$/, "", ver);  next }
    /^source = /   { src = 1; next }
    /^$/ { if (name != "" && src == 0) print name " " ver; name=""; ver=""; src=0 }
    END  { if (name != "" && src == 0) print name " " ver }
  ' "$lock")"
  while read -r pkg ver; do
    [[ -n "${pkg:-}" ]] || continue
    [[ "$pkg" == powdb || "$pkg" == powdb-* ]] || continue
    [[ "$lock_exempt_crates" == *" $pkg "* ]] && continue
    [[ "$ver" == "$workspace_version" ]] \
      || fail "$lock pins $pkg at $ver, expected the workspace version $workspace_version (regenerate it: cd $(dirname "$lock") && cargo metadata --offline --format-version 1 >/dev/null)"
  done <<< "$local_pkgs"
done <<< "$tracked_locks"
log "checked $lock_count tracked Cargo.lock file(s) against workspace $workspace_version"

# The Node addon crate is publish = false and detached from the root workspace
# (it needs panic = "unwind"), so nothing made its version track anything. It
# drifted 17 minors to 0.8.1 while the npm package it produces shipped in
# lockstep. Keep the Rust crate honest about which engine it wraps.
addon_crate_version="$(sed -n '/^\[package\]/,/^\[/{ s/^version = "\([^"]*\)"/\1/p; }' bindings/node/Cargo.toml | head -1)"
[[ -n "$addon_crate_version" ]] || fail "could not parse [package] version from bindings/node/Cargo.toml"
[[ "$addon_crate_version" == "$workspace_version" ]] \
  || fail "bindings/node/Cargo.toml version $addon_crate_version != workspace $workspace_version"

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
# This used to cover docs/STABILITY.md alone. README.md and docs/powdb-vs-sqlite.md
# carry the same "pin an exact version" advice and were checked by nothing, which
# is how both sat at 0.23.0 two releases after 0.23.0 stopped being current while
# site/getting-started.html stayed right.
pin_docs=(README.md docs/*.md)
doc_pins="$(grep -ohE -- '--version [0-9]+\.[0-9]+\.[0-9]+' "${pin_docs[@]}" | sort -u || true)"
# A pattern that stops matching is a check that stops checking. These pins exist
# today, so finding none means the scan broke, not that the docs got cleaner.
[[ -n "$doc_pins" ]] \
  || fail "found no '--version X.Y.Z' install pin in README.md or docs/*.md; this check has gone vacuous"
while IFS= read -r pin; do
  [[ -n "$pin" ]] || continue
  [[ "$pin" == "--version $current_release" ]] \
    || fail "a doc pins '$pin', expected --version $current_release (see: grep -rn -- '$pin' README.md docs/*.md)"
done <<< "$doc_pins"

# Documented CLI transcripts print the binary's own version, so a reader
# comparing their terminal against the doc must see the release they installed.
# Scoped to the files whose blocks are meant to be reproducible: docs/FORMAT.md
# opens with "PowDB v0.5.0 makes persisted formats explicit", which is a
# historical statement about when a format landed and must not be swept.
banner_docs=(README.md docs/getting-started.md)
banner_refs="$(grep -ohE '(PowDB|server) v[0-9]+\.[0-9]+\.[0-9]+' "${banner_docs[@]}" \
  | grep -oE 'v[0-9]+\.[0-9]+\.[0-9]+' | sort -u || true)"
[[ -n "$banner_refs" ]] \
  || fail "found no CLI version banner in ${banner_docs[*]}; this check has gone vacuous"
while IFS= read -r ref; do
  [[ -n "$ref" ]] || continue
  [[ "$ref" == "v$current_release" ]] \
    || fail "${banner_docs[*]} shows a CLI banner for $ref, expected v$current_release"
done <<< "$banner_refs"

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

# That entry is also the GitHub Release body: release.yml extracts it with
# scripts/ci/changelog-section.sh at tag time. release.yml only ever runs on a
# tag, so exercise the extractor here instead, where every PR runs it. A release
# should not be the first time anyone discovers the body would have been empty.
release_body="$(bash scripts/ci/changelog-section.sh "$current_release" 2>&1)" \
  || fail "scripts/ci/changelog-section.sh cannot build a GitHub Release body for $current_release: $release_body"
[[ -n "${release_body//[[:space:]]/}" ]] \
  || fail "the release body extracted for $current_release is empty"

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

# The release workflow's crate list must equal the set of publishable crates. A
# crate added to crates/ without a publish step is simply never released, and
# the failure mode is silent: the other seven publish fine and the missing one
# is only noticed by a user whose `cargo install` cannot resolve it.
publishable_crates="$(for manifest in crates/*/Cargo.toml; do
  sed -n '/^\[package\]/,/^\[/p' "$manifest" | grep -q '^publish = false' && continue
  sed -n '/^\[package\]/,/^\[/{ s/^name = "\([^"]*\)"/\1/p; }' "$manifest" | head -1
done | sort)"
workflow_crates="$(grep -oE 'publish-crate\.sh [A-Za-z0-9_-]+' .github/workflows/publish.yml \
  | awk '{ print $2 }' | sort -u)"
[[ -n "$publishable_crates" ]] || fail "parsed no publishable crates out of crates/*/Cargo.toml"
[[ -n "$workflow_crates" ]] || fail "parsed no publish steps out of .github/workflows/publish.yml"
if [[ "$publishable_crates" != "$workflow_crates" ]]; then
  fail "publish.yml does not publish exactly the publishable crates.
  publishable (crates/*/Cargo.toml without publish = false): $(tr '\n' ' ' <<< "$publishable_crates")
  published   (publish-crate.sh steps in publish.yml):       $(tr '\n' ' ' <<< "$workflow_crates")"
fi

# The run summary that step prints must name the same crates. It listed 7 while
# the job published 8, so the one channel a human actually reads after a release
# under-reported what had shipped.
summary_crates="$(sed -n 's/.*echo "- \(powdb[a-z-]*\) .*GITHUB_STEP_SUMMARY.*/\1/p' \
  .github/workflows/publish.yml | sort -u)"
[[ -n "$summary_crates" ]] || fail "parsed no crate list out of the publish.yml run summary"
if [[ "$publishable_crates" != "$summary_crates" ]]; then
  fail "the publish.yml run summary does not list the crates it publishes.
  publishes: $(tr '\n' ' ' <<< "$publishable_crates")
  summary  : $(tr '\n' ' ' <<< "$summary_crates")"
fi

log "development version $workspace_version and published release $current_release are consistent across manifests, deploy examples, site output, changelog, release docs, format/stability policies, and SECURITY.md."
