#!/usr/bin/env bash
# scripts/smoke-release.sh — post-build durability smoke for a RELEASE binary.
#
# This is the gate whose ABSENCE caused the v0.4.1–0.4.3 data-loss yanks: a
# server that accepted writes, exited, and came back EMPTY because WAL replay
# wasn't exercised end-to-end against the actual shipped binary. This script
# proves, over the wire, that:
#
#   1. The README's documented PowQL flow works (create type, insert, filter,
#      count) against the built `powdb-server` via `powdb-cli -r host:port`.
#   2. A `required unique` constraint REJECTS a duplicate insert.
#   3. After `kill -9` + restart on the SAME data dir, the previously-inserted
#      rows are STILL there (WAL replay recovered them) AND the unique
#      constraint is STILL enforced.
#
# It drives the binaries via env vars so it works both in CI (against freshly
# built release artifacts) and locally:
#
#   POWDB_CLI      path to powdb-cli      (default ./target/release/powdb-cli)
#   POWDB_SERVER   path to powdb-server   (default ./target/release/powdb-server)
#   POWDB_SMOKE_PORT  TCP port to bind    (default 7950)
#
# Prints 'SMOKE-RELEASE: ALL-PASS' and exits 0 on success; exits nonzero on the
# first failed assertion.

set -euo pipefail
# Quiet bash job-control "Killed: 9" notices when we kill -9 the server.
set +m 2>/dev/null || true

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

POWDB_CLI="${POWDB_CLI:-${REPO_ROOT}/target/release/powdb-cli}"
POWDB_SERVER="${POWDB_SERVER:-${REPO_ROOT}/target/release/powdb-server}"
PORT="${POWDB_SMOKE_PORT:-7950}"
HOST="127.0.0.1"

DATADIR="$(mktemp -d "${TMPDIR:-/tmp}/powdb-smoke-XXXXXX")"
SERVER_PID=""

log()  { echo "smoke-release: $*"; }
fail() { echo "smoke-release: FAIL — $*" >&2; exit 1; }

cleanup() {
  if [[ -n "${SERVER_PID}" ]] && kill -0 "${SERVER_PID}" 2>/dev/null; then
    kill -9 "${SERVER_PID}" 2>/dev/null || true
  fi
  # Belt-and-suspenders: reap any stray server bound to our temp data dir.
  pkill -9 -f "powdb-server.*${DATADIR}" 2>/dev/null || true
  if [[ -d "${DATADIR}" && ( "${DATADIR}" == "${TMPDIR:-/tmp}"/powdb-smoke-* \
        || "${DATADIR}" == /tmp/powdb-smoke-* \
        || "${DATADIR}" == /var/folders/*/powdb-smoke-* ) ]]; then
    rm -rf "${DATADIR}"
  fi
}
trap cleanup EXIT

[[ -x "${POWDB_CLI}" ]]    || fail "powdb-cli not found/executable at ${POWDB_CLI} (build with: cargo build --release -p powdb-cli)"
[[ -x "${POWDB_SERVER}" ]] || fail "powdb-server not found/executable at ${POWDB_SERVER} (build with: cargo build --release -p powdb-server)"

# Start the server in the background on $DATADIR/$PORT, wait for it to bind.
start_server() {
  env -i \
      PATH="${PATH}" \
      HOME="${HOME:-}" \
      RUST_LOG="warn" \
      RUST_BACKTRACE="1" \
      "${POWDB_SERVER}" \
      --port "${PORT}" \
      --bind "${HOST}" \
      --data-dir "${DATADIR}" \
      >"${DATADIR}/server.log" 2>&1 &
  SERVER_PID=$!
  # Drop the server from job control so a later `kill -9` doesn't print a
  # cosmetic "Killed: 9" job notice. We still track/kill it via $SERVER_PID.
  disown "${SERVER_PID}" 2>/dev/null || true

  local waited=0
  while (( waited < 100 )); do
    if ! kill -0 "${SERVER_PID}" 2>/dev/null; then
      echo "smoke-release: server exited during startup — log:" >&2
      cat "${DATADIR}/server.log" >&2 || true
      fail "server did not stay up"
    fi
    if (echo >"/dev/tcp/${HOST}/${PORT}") >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
    waited=$(( waited + 1 ))
  done
  echo "smoke-release: server never bound ${HOST}:${PORT} — log:" >&2
  cat "${DATADIR}/server.log" >&2 || true
  fail "server bind timeout"
}

# Run one PowQL statement over the wire. Echoes the CLI's stdout.
q() {
  "${POWDB_CLI}" -r "${HOST}:${PORT}" -c "$1"
}

# Run a PowQL statement that is EXPECTED to fail; capture combined output and
# return success only if it ran (we inspect the text in the caller).
q_expect_fail() {
  "${POWDB_CLI}" -r "${HOST}:${PORT}" -c "$1" 2>&1 || true
}

# ── Phase 1: fresh server, documented PowQL flow ───────────────────────────
log "starting server (pid-to-be) on ${HOST}:${PORT}, data dir ${DATADIR}"
start_server
log "server up (pid ${SERVER_PID})"

# README/getting-started PowQL (NOT SQL): a type with a `required unique` field.
log "creating type Acct (required unique email)"
q 'type Acct { required unique email: str, id: int }' >/dev/null \
  || fail "could not create type Acct"

log "inserting two rows"
q 'insert Acct { email := "a@x.com", id := 1 }' >/dev/null || fail "insert #1 failed"
q 'insert Acct { email := "b@x.com", id := 2 }' >/dev/null || fail "insert #2 failed"

log "filter query (Acct filter .id = 1 { .email })"
FILT="$(q 'Acct filter .id = 1 { .email }')"
echo "${FILT}" | grep -q 'a@x.com' || fail "filter did not return the expected row: ${FILT}"

log "count(Acct) should be 2"
CNT="$(q 'count(Acct)' | tr -dc '0-9')"
[[ "${CNT}" == "2" ]] || fail "expected count 2, got '${CNT}'"

# ── Phase 2: unique constraint rejects a duplicate ─────────────────────────
log "asserting unique constraint rejects duplicate email"
DUP_OUT="$(q_expect_fail 'insert Acct { email := "a@x.com", id := 3 }')"
echo "${DUP_OUT}" | grep -qi 'unique constraint violation' \
  || fail "duplicate insert was NOT rejected with a unique constraint violation. Output: ${DUP_OUT}"

log "re-counting after rejected duplicate (must still be 2)"
CNT2="$(q 'count(Acct)' | tr -dc '0-9')"
[[ "${CNT2}" == "2" ]] || fail "count changed after a rejected insert: expected 2, got '${CNT2}'"

# ── Phase 3: kill -9 + restart on the SAME data dir → WAL replay ───────────
log "kill -9 the server (simulating a crash) — pid ${SERVER_PID}"
kill -9 "${SERVER_PID}" 2>/dev/null || true
# Wait for the process to actually die and release the port.
waited=0
while kill -0 "${SERVER_PID}" 2>/dev/null && (( waited < 50 )); do
  sleep 0.1; waited=$(( waited + 1 ))
done
SERVER_PID=""

log "restarting server on the SAME data dir ${DATADIR}"
start_server
log "server back up (pid ${SERVER_PID}) — verifying recovery"

log "post-restart count(Acct) must still be 2 (WAL replay recovered rows)"
RCNT="$(q 'count(Acct)' | tr -dc '0-9')"
[[ "${RCNT}" == "2" ]] || fail "DATA LOSS: post-restart count is '${RCNT}', expected 2 — WAL replay did not recover the rows"

log "post-restart filter must still find a@x.com"
RFILT="$(q 'Acct filter .id = 1 { .email }')"
echo "${RFILT}" | grep -q 'a@x.com' || fail "post-restart filter lost the row: ${RFILT}"

log "post-restart unique constraint must STILL be enforced"
RDUP_OUT="$(q_expect_fail 'insert Acct { email := "a@x.com", id := 4 }')"
echo "${RDUP_OUT}" | grep -qi 'unique constraint violation' \
  || fail "unique constraint NOT enforced after restart — index/WAL recovery incomplete. Output: ${RDUP_OUT}"

log "final count must remain 2 (rejected dup did not slip in)"
FCNT="$(q 'count(Acct)' | tr -dc '0-9')"
[[ "${FCNT}" == "2" ]] || fail "final count is '${FCNT}', expected 2"

echo "SMOKE-RELEASE: ALL-PASS"
