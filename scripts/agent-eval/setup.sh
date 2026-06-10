#!/usr/bin/env bash
# setup.sh — build powdb-cli and create a pristine, seeded golden data dir.
#
# The golden dir (.golden-data/) is the read-only source of truth: run.py
# copies it per candidate so every scored statement runs against identical
# state. Re-running this script rebuilds the golden dir from scratch.
#
# No model calls, no network. Pure local scaffolding.
set -euo pipefail

# Resolve paths relative to this script so it works from any cwd.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
CLI="$REPO_ROOT/target/release/powdb-cli"
GOLDEN="$HERE/.golden-data"

echo "==> building powdb-cli (release)"
( cd "$REPO_ROOT" && cargo build --release -p powdb-cli )

if [ ! -x "$CLI" ]; then
  echo "error: expected CLI at $CLI after build" >&2
  exit 1
fi

echo "==> resetting golden data dir: $GOLDEN"
rm -rf "$GOLDEN"
mkdir -p "$GOLDEN"

# Stream schema then seed, one statement per line, through --exec.
# Skip blank lines and `--` comments. One process per statement keeps the
# error surface obvious at seed scale (~60 statements).
seed_file() {
  local file="$1"
  local n=0
  while IFS= read -r line || [ -n "$line" ]; do
    # strip leading/trailing whitespace
    local trimmed="${line#"${line%%[![:space:]]*}"}"
    trimmed="${trimmed%"${trimmed##*[![:space:]]}"}"
    [ -z "$trimmed" ] && continue
    case "$trimmed" in
      --*) continue ;;
    esac
    if ! "$CLI" --data-dir "$GOLDEN" --exec "$trimmed" >/dev/null; then
      echo "error: statement failed while seeding $file:" >&2
      echo "  $trimmed" >&2
      exit 1
    fi
    n=$((n + 1))
  done < "$file"
  echo "    applied $n statements from $(basename "$file")"
}

echo "==> applying schema.powql"
seed_file "$HERE/schema.powql"
echo "==> applying seed.powql"
seed_file "$HERE/seed.powql"

echo "==> golden data dir ready: $GOLDEN"
echo "    next: python3 $HERE/run.py $HERE/examples/golden-candidates.jsonl"
