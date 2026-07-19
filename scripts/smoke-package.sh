#!/usr/bin/env bash
# Credential-free package smoke for release/publish paths.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

run() { echo "+ $*"; "$@"; }
skip() { echo "smoke-package: skip — $*"; }
pnpm10() { npm exec --yes --package=pnpm@10.29.3 -- pnpm "$@"; }

run bash scripts/check-version-consistency.sh

(
  cd clients/ts
  run pnpm10 install --frozen-lockfile
  run pnpm10 build
  tarball="$(npm pack --json | node -e "const chunks=[]; process.stdin.on('data', c=>chunks.push(c)); process.stdin.on('end', ()=>process.stdout.write(JSON.parse(Buffer.concat(chunks).toString())[0].filename));")"
  tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/powdb-npm-smoke-XXXXXX")"
  trap 'rm -rf "$tmpdir"' EXIT
  (cd "$tmpdir" && npm init -y >/dev/null && npm install "${ROOT}/clients/ts/${tarball}" >/dev/null && node --input-type=module -e "import { Client, CLIENT_VERSION } from '@zvndev/powdb-client'; if (!Client || !CLIENT_VERSION) process.exit(1); console.log('npm import smoke', CLIENT_VERSION)")
  rm -f "$tarball"
)

# Validate crates can be packaged without credentials. This is lighter and less
# network-sensitive than publish --dry-run, while still catching missing files
# and manifest/package metadata issues before the credentialed publish step.
for crate in powdb-storage powdb-auth powdb-query powdb-sync powdb-backup powdb-server powdb powdb-cli; do
  run cargo package -p "$crate" --allow-dirty --list >/dev/null
done

echo "SMOKE-PACKAGE: ALL-PASS"
