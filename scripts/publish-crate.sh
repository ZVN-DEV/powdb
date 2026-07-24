#!/usr/bin/env bash
# Publish one workspace crate to crates.io, idempotently.
#
#   publish-crate.sh <crate> <version> <dry_run: true|false>
#
# - Skips the publish when <version> is already in the crates.io sparse index,
#   so a failed release run can be re-dispatched without erroring on the
#   crates that already made it out.
# - After publishing, polls the sparse index until the new version is actually
#   visible (up to ~5 minutes) instead of sleeping a fixed 30s: dependent
#   crates fail to resolve the freshly published version when the index lags,
#   which is exactly how the v0.19.1 run failed at powdb-query.
set -euo pipefail

crate="$1"
version="$2"
dry_run="${3:-false}"

# Sparse-index path: crates with >=4 chars live at {first2}/{next2}/{name}.
prefix="${crate:0:2}/${crate:2:2}"
index_url="https://index.crates.io/${prefix}/${crate}"

indexed() {
  # Cache-bust: the index is CDN-fronted and a stale 200 is worse than a miss.
  curl -sf -H 'Cache-Control: no-cache' "${index_url}?cachebust=$(date +%s)" \
    | jq -r '.vers' | grep -qx "$version"
}

if [ "$dry_run" = "true" ]; then
  cargo publish -p "$crate" --dry-run
  exit 0
fi

if indexed; then
  echo "${crate} ${version} is already on crates.io; skipping."
  exit 0
fi

cargo publish -p "$crate"

echo "Waiting for crates.io to index ${crate} ${version}..."
deadline=$((SECONDS + 300))
until indexed; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    echo "ERROR: ${crate} ${version} not visible in the sparse index after 5m." >&2
    exit 1
  fi
  sleep 10
done
echo "${crate} ${version} is indexed."
