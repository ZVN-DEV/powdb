#!/usr/bin/env bash
#
# edge-snapshot-serving: an end-to-end demo of PowDB's blessed read-scaling
# pattern. A single writer primary is snapshotted with `powdb-cli backup`,
# restored into a serving directory, and served by N read-only server
# processes. A refresh publishes a newer snapshot beside the current one and
# swaps it in atomically with a directory rename.
#
# The script is self-contained: it builds the binaries it needs, runs the whole
# cycle against loopback ports in the 56xx range, prints PASS/FAIL for every
# checked step, and exits nonzero if any check fails.
#
# Usage:
#   ./run.sh                 # build (if needed) and run the demo
#   POWDB_BIN=/path ./run.sh # use prebuilt binaries from that directory
#
set -euo pipefail

# ── Paths ────────────────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
WORK="$SCRIPT_DIR/.work"          # gitignored scratch; wiped on each run

# Ports (56xx range, per the demo convention). The writer primary here is the
# embedded CLI (no listening port); the two read-only servers use 5602/5603.
PORT_RO_A=5602
PORT_RO_B=5603

# ── Pretty output + pass/fail bookkeeping ────────────────────────────────────
FAILURES=0
pass()  { printf 'PASS: %s\n' "$1"; }
fail()  { printf 'FAIL: %s\n' "$1"; FAILURES=$((FAILURES + 1)); }
step()  { printf '\n=== %s ===\n' "$1"; }

# ── Binaries ─────────────────────────────────────────────────────────────────
# There is no separate powdb-backup binary: backup and restore are subcommands
# of powdb-cli (see `powdb-cli --help`). We need powdb-cli and powdb-server.
if [ -n "${POWDB_BIN:-}" ]; then
  BIN="$POWDB_BIN"
else
  step "Building powdb-cli and powdb-server (release)"
  cargo build --release -p powdb-cli -p powdb-server --manifest-path "$REPO_ROOT/Cargo.toml"
  BIN="$REPO_ROOT/target/release"
fi
PC="$BIN/powdb-cli"
PS="$BIN/powdb-server"
for b in "$PC" "$PS"; do
  if [ ! -x "$b" ]; then
    echo "error: missing binary: $b" >&2
    exit 1
  fi
done

# ── Background process + cleanup management ──────────────────────────────────
SERVER_PIDS=()
start_server() {  # start_server <label> <port> <data-dir> [extra args...]
  local label="$1" port="$2" dir="$3"; shift 3
  "$PS" --readonly --data-dir "$dir" --port "$port" \
      >"$WORK/$label.log" 2>&1 &
  SERVER_PIDS+=("$!")
}
stop_all_servers() {
  local pid
  for pid in "${SERVER_PIDS[@]:-}"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  for pid in "${SERVER_PIDS[@]:-}"; do
    [ -n "$pid" ] && wait "$pid" 2>/dev/null || true
  done
  SERVER_PIDS=()
}
# shellcheck disable=SC2329  # invoked indirectly via the trap below
cleanup() { stop_all_servers; }
trap cleanup EXIT INT TERM

wait_for_port() {  # wait_for_port <port>
  local port="$1"
  for _ in $(seq 1 100); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
      exec 3<&- 3>&- || true
      return 0
    fi
    sleep 0.1
  done
  return 1
}

# Run a one-shot PowQL query against a remote server; print its scalar/table.
q() { "$PC" --remote "127.0.0.1:$1" --exec "$2"; }

# ── Fresh workspace ──────────────────────────────────────────────────────────
rm -rf "$WORK"
mkdir -p "$WORK"
PRIMARY="$WORK/primary"
SNAP1="$WORK/snapshots/1-full"
SNAP2="$WORK/snapshots/2-incremental"
SERVE_CURRENT="$WORK/serve/current"
SERVE_NEXT="$WORK/serve/next"

# ── 1. Writer primary: create schema and seed data ───────────────────────────
step "1. Seed the writer primary"
# The writer is the single read-write owner of ./primary. Here we use the
# embedded CLI as the writer; in production this is your app or powdb-server.
"$PC" --data-dir "$PRIMARY" --exec \
'type Article { unique auto id: int, required title: str, required category: str, required views: int };
insert Article { title := "Welcome to the edge", category := "news",    views := 120 };
insert Article { title := "Snapshot serving 101", category := "guides",  views := 340 };
insert Article { title := "How the gate works",   category := "guides",  views := 210 };
insert Article { title := "Release notes",         category := "news",    views := 90 }'
primary_count="$("$PC" --data-dir "$PRIMARY" --exec 'count(Article)')"
if [ "$primary_count" = "4" ]; then
  pass "primary seeded with 4 articles"
else
  fail "primary seed count expected 4, got '$primary_count'"
fi

# ── 2. Back up the (quiescent) primary ───────────────────────────────────────
step "2. Back up the primary"
# Backup is offline: it opens ./primary with its own handle and checkpoints it.
# Nothing else may hold ./primary open while this runs. The embedded writer
# above has already exited, so the directory is quiescent.
"$PC" --data-dir "$PRIMARY" backup "$SNAP1"
if [ -f "$SNAP1/manifest.json" ]; then
  pass "full backup written to snapshots/1-full"
else
  fail "backup did not produce a manifest"
fi

# ── 3. Restore into a serving directory ──────────────────────────────────────
step "3. Restore into the serving directory"
# restore opens and validates the snapshot, guaranteeing it is WAL-clean
# (quiescent) and therefore safe to serve read-only.
"$PC" restore "$SNAP1" "$SERVE_CURRENT"
if [ -f "$SERVE_CURRENT/catalog.bin" ]; then
  pass "restored snapshot into serve/current"
else
  fail "restore did not produce a catalog"
fi

# ── 4. Serve read-only with two server processes ─────────────────────────────
step "4. Start two read-only servers on serve/current"
start_server "ro-a" "$PORT_RO_A" "$SERVE_CURRENT"
start_server "ro-b" "$PORT_RO_B" "$SERVE_CURRENT"
if wait_for_port "$PORT_RO_A" && wait_for_port "$PORT_RO_B"; then
  pass "two read-only servers are listening ($PORT_RO_A, $PORT_RO_B)"
else
  fail "read-only servers did not come up"
fi

# ── 5. Verify reads on both, and write refusal on both ───────────────────────
step "5. Verify reads and write refusal"
count_a="$(q "$PORT_RO_A" 'count(Article)')"
count_b="$(q "$PORT_RO_B" 'count(Article)')"
if [ "$count_a" = "4" ] && [ "$count_b" = "4" ]; then
  pass "both read-only servers return count(Article) = 4"
else
  fail "read-only counts wrong: A='$count_a' B='$count_b' (expected 4/4)"
fi

# A point read by unique id through the index.
read_a="$(q "$PORT_RO_A" 'Article filter .id = 2 { .title }' | sed -n '3p' | tr -d ' ')"
if [ "$read_a" = "Snapshotserving101" ]; then
  pass "point read by id on server A returns the expected row"
else
  fail "point read on server A wrong: '$read_a'"
fi

# A write must be refused on a read-only server (terminal error, exit nonzero).
if q "$PORT_RO_A" 'insert Article { title := "nope", category := "x", views := 1 }' \
     >/dev/null 2>&1; then
  fail "a write was ACCEPTED on a read-only server (should be refused)"
else
  pass "write correctly refused on read-only server A"
fi
if q "$PORT_RO_B" 'delete Article filter .id = 1' >/dev/null 2>&1; then
  fail "a delete was ACCEPTED on a read-only server (should be refused)"
else
  pass "delete correctly refused on read-only server B"
fi

# ── 6. Writer advances: more data on the primary ─────────────────────────────
step "6. Writer primary advances (readers still serve the old snapshot)"
"$PC" --data-dir "$PRIMARY" --exec \
'insert Article { title := "Fresh off the primary", category := "news",   views := 500 };
insert Article { title := "Another new one",        category := "guides", views := 275 }'
primary_count2="$("$PC" --data-dir "$PRIMARY" --exec 'count(Article)')"
# Readers must still see the OLD snapshot (stale-by-design) until we swap.
stale_a="$(q "$PORT_RO_A" 'count(Article)')"
if [ "$primary_count2" = "6" ] && [ "$stale_a" = "4" ]; then
  pass "primary now has 6 rows; readers still serve the frozen snapshot (4)"
else
  fail "expected primary=6 readers=4, got primary='$primary_count2' readers='$stale_a'"
fi

# ── 7. Incremental backup of just what changed ───────────────────────────────
step "7. Take an incremental backup and restore it beside current"
"$PC" --data-dir "$PRIMARY" backup "$SNAP2" --base "$SNAP1"
if [ -f "$SNAP2/increment.json" ]; then
  pass "incremental backup written to snapshots/2-incremental"
else
  fail "incremental backup did not produce an increment manifest"
fi
# Chain-restore the full base plus the increment into serve/next.
"$PC" restore "$SNAP1" "$SERVE_NEXT" --apply "$SNAP2"
next_count="$("$PC" --data-dir "$SERVE_NEXT" --exec 'count(Article)')"
if [ "$next_count" = "6" ]; then
  pass "serve/next restored with the fresh data (6 rows)"
else
  fail "serve/next expected 6 rows, got '$next_count'"
fi

# ── 8. Atomic swap: rename next -> current, restart readers ──────────────────
step "8. Swap serve/next into place and re-point readers"
# Drain and stop the readers on the old directory first, then flip directories
# by rename (atomic at the process level), then start readers on the new one.
stop_all_servers
mv "$SERVE_CURRENT" "$WORK/serve/old"
mv "$SERVE_NEXT" "$SERVE_CURRENT"
start_server "ro-a2" "$PORT_RO_A" "$SERVE_CURRENT"
start_server "ro-b2" "$PORT_RO_B" "$SERVE_CURRENT"
if wait_for_port "$PORT_RO_A" && wait_for_port "$PORT_RO_B"; then
  pass "read-only servers restarted on the swapped directory"
else
  fail "read-only servers did not come back up after swap"
fi

fresh_a="$(q "$PORT_RO_A" 'count(Article)')"
fresh_b="$(q "$PORT_RO_B" 'count(Article)')"
if [ "$fresh_a" = "6" ] && [ "$fresh_b" = "6" ]; then
  pass "readers now serve the fresh snapshot (count = 6 on both)"
else
  fail "post-swap counts wrong: A='$fresh_a' B='$fresh_b' (expected 6/6)"
fi

# The new row is visible through the index on the refreshed servers.
fresh_read="$(q "$PORT_RO_A" 'Article filter .id = 5 { .title }' | sed -n '3p' | tr -d ' ')"
if [ "$fresh_read" = "Freshofftheprimary" ]; then
  pass "the newly-published row is readable after the swap"
else
  fail "expected the fresh row after swap, got '$fresh_read'"
fi

# ── Summary ──────────────────────────────────────────────────────────────────
step "Summary"
if [ "$FAILURES" -eq 0 ]; then
  echo "ALL CHECKS PASSED"
  exit 0
else
  echo "$FAILURES CHECK(S) FAILED"
  exit 1
fi
