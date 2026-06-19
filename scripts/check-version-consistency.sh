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
for manifest in crates/{auth,backup,cli,query,server,storage}/Cargo.toml; do
  grep -q '^version\.workspace = true$' "$manifest" \
    || fail "$manifest must use version.workspace = true"
done

# Path dependencies between publishable PowDB crates must match the workspace
# version because crates.io cannot resolve path-only specs. Path-only references
# in publish=false helper crates are deliberately ignored.
version_refs="$(grep -RInE '^powdb-[a-z-]+ = \{[^}]*version = "[^"]+"[^}]*path = '   crates/{auth,backup,cli,query,server,storage}/Cargo.toml || true)"
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

grep -qE "^## \[$workspace_version\]" CHANGELOG.md \
  || fail "CHANGELOG.md is missing a top-level entry for [$workspace_version]"
grep -q "Current release: v$workspace_version" RELEASES.md \
  || fail "RELEASES.md current release does not reference v$workspace_version"

log "Rust crate versions, TS client version, changelog, and release docs agree."
