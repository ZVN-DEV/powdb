#!/usr/bin/env bash
# scripts/ci/release-profile-suites.sh: run the corruption and fuzz-corpus
# suites against the SHIPPED panic configuration.
#
# `panic = "abort"` is set on `[profile.release]` only. `cargo test` builds the
# dev/test profile, and the process-level server tests spawn
# `env!("CARGO_BIN_EXE_powdb-server")`, which resolves to whatever profile the
# test was built with. So every "the server did not abort" assertion in the
# suite has, until now, been "the server did not panic under UNWIND", a
# different runtime with a different failure mode. A panic that unwinds through
# a poisoned `RwLock<Engine>` and a panic that aborts the process are not the
# same event, and the abort one is the one users get.
#
# This script closes that gap in three steps:
#
#   1. Assert the shipped profile still declares `panic = "abort"`. The whole
#      chain below is worthless if that line is quietly deleted.
#   2. Assert that a `--release` test build really does hand the process tests
#      a `target/release/` server binary, by reading cargo's own artifact
#      metadata rather than believing the flag.
#   3. Run the corruption suites and replay the checked-in wire fuzz corpus at
#      a spawned RELEASE server. An abort kills the process outright, so
#      "is the server still answering" is a direct, honest abort detector.
#
# Honest scope note: cargo refuses to build a TEST HARNESS with panic=abort, so
# the in-process unit assertions still unwind. What this script adds is the
# out-of-process half, which is the half that runs the shipped configuration:
# the server binary under test is the abort-on-panic binary.
#
# Env:
#   POWDB_RELEASE_SUITE_PORT   base TCP port for the spawned server (default 7960)

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${REPO_ROOT}" || exit 1

PORT="${POWDB_RELEASE_SUITE_PORT:-7960}"
FAILURES=0
SERVER_PID=""
DATADIR=""

log()  { echo "release-suite: $*"; }
fail() { echo "release-suite: FAIL: $*" >&2; FAILURES=$((FAILURES + 1)); }

# Is the spawned server still a LIVE process?
#
# `kill -0` alone is not enough and this is not a nitpick: the server is a
# child of this script, so when it aborts it becomes a zombie until reaped, and
# `kill -0` succeeds on a zombie. A liveness check built on `kill -0` therefore
# reports "alive" for exactly the crash this job exists to catch. Found by
# killing the server mid-corpus and watching the gate stay green.
server_alive() {
  [[ -n "${SERVER_PID}" ]] || return 1
  kill -0 "${SERVER_PID}" 2>/dev/null || return 1
  local state
  state="$(ps -o stat= -p "${SERVER_PID}" 2>/dev/null | tr -d ' ')"
  [[ -n "${state}" && "${state}" != Z* ]]
}

# Portable TCP helpers.
#
# The obvious spelling for both of these is bash's `/dev/tcp/host/port`, and it
# is wrong here: Apple ships bash 3.2 built WITHOUT net redirections, so on
# macOS `/dev/tcp` is "no such file or directory" even while the port is
# plainly listening. The readiness probe below would then never succeed, and
# this job would report "the release server did not start" on every developer
# laptop while passing in CI. A gate that only its CI runner can execute is a
# gate nobody can debug, and the wire-frame loop would silently send zero bytes
# and still count them. python3 is already a hard requirement of step 2, so the
# socket work goes through it instead.

# tcp_probe <port>: exit 0 when something accepts a connection on the port.
tcp_probe() {
  python3 -c '
import socket, sys
s = socket.socket()
s.settimeout(1)
rc = s.connect_ex(("127.0.0.1", int(sys.argv[1])))
s.close()
sys.exit(0 if rc == 0 else 1)
' "$1" 2>/dev/null
}

# tcp_send_file <port> <path>: open one connection, write the file, half-close,
# and briefly drain the reply so the server gets to answer or close on its own
# terms rather than being reset mid-response.
tcp_send_file() {
  python3 -c '
import socket, sys
with open(sys.argv[2], "rb") as fh:
    payload = fh.read()
try:
    conn = socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=5)
except OSError:
    sys.exit(1)
try:
    conn.sendall(payload)
    conn.shutdown(socket.SHUT_WR)
    conn.settimeout(2)
    try:
        conn.recv(65536)
    except OSError:
        pass
except OSError:
    sys.exit(1)
finally:
    conn.close()
' "$1" "$2" 2>/dev/null
}

cleanup() {
  if [[ -n "${SERVER_PID}" ]] && kill -0 "${SERVER_PID}" 2>/dev/null; then
    kill -9 "${SERVER_PID}" 2>/dev/null || true
  fi
  if [[ -n "${DATADIR}" && -d "${DATADIR}" && "${DATADIR}" == *powdb-relsuite-* ]]; then
    rm -rf "${DATADIR}"
  fi
}
trap cleanup EXIT

# ── 1. The shipped profile must still declare panic = "abort" ──────────────
log "step 1: shipped panic strategy"
profile_block="$(awk '/^\[profile\.release\]/{f=1;next} /^\[/{f=0} f' Cargo.toml)"
if ! grep -qE '^panic[[:space:]]*=[[:space:]]*"abort"' <<<"${profile_block}"; then
  fail 'Cargo.toml [profile.release] does not declare panic = "abort".
       PowDB is crash-only by design (single RwLock<Engine>, WAL replay on
       restart). If this was removed deliberately, delete this check and this
       whole job with it, because nothing below means anything without it.'
fi

# ── 2. A --release test build must hand the process tests a release binary ─
log "step 2: process tests must spawn the release-profile server binary"
artifact_json="$(cargo test --release -p powdb-server --no-run --message-format=json 2>/dev/null)"
read -r -d '' FIND_SERVER_EXE <<'PY'
import sys, json
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("reason") != "compiler-artifact":
        continue
    target = m.get("target", {})
    # The BIN artifact, not the test harness built from the same crate.
    if target.get("name") == "powdb-server" and "bin" in target.get("kind", []) \
            and not m.get("profile", {}).get("test", False):
        print(m.get("executable") or "")
        break
PY
server_exe="$(printf '%s' "${artifact_json}" | python3 -c "${FIND_SERVER_EXE}")"
if [[ -z "${server_exe}" ]]; then
  fail "cargo reported no powdb-server bin artifact for the release test build"
elif [[ "${server_exe}" != *"/target/release/powdb-server" ]]; then
  fail "release test build produced the server at '${server_exe}', not under target/release/.
       CARGO_BIN_EXE_powdb-server would hand the process tests the wrong profile."
else
  log "  server binary: ${server_exe}"
fi

# ── 3a. Corruption suites, release profile ─────────────────────────────────
# Each entry is "crate:test-target". The test target must EXIST: a `--test`
# name that matches no file is the miri failure mode (a filter that selects
# nothing still exits 0), so the existence of every file is asserted first.
SUITES=(
  "powdb-storage:catalog_corruption"
  "powdb-storage:page_corruption"
  "powdb-storage:wal_crc"
  "powdb-storage:pj1_adversarial"
  "powdb-storage:format_versioning"
  "powdb-storage:heap_page_checksum"
  "powdb-storage:overflow_index_and_recovery"
  "powdb-storage:btree_edge_cases"
  "powdb-query:durability"
  "powdb-query:wal_recovery_executor"
  "powdb-query:safety_limits"
  "powdb-server:kill9_durability"
  "powdb-server:graceful_shutdown_sigterm"
  "powdb-server:wire_error_codes"
  "powdb-server:connection_management"
)

log "step 3a: corruption suites under --release"
crate_dir_for() {
  case "$1" in
    powdb-storage) echo "crates/storage" ;;
    powdb-query)   echo "crates/query" ;;
    powdb-server)  echo "crates/server" ;;
    *)             echo "" ;;
  esac
}

missing=0
for suite in "${SUITES[@]}"; do
  crate="${suite%%:*}"
  test_name="${suite##*:}"
  dir="$(crate_dir_for "${crate}")"
  if [[ -z "${dir}" || ! -f "${REPO_ROOT}/${dir}/tests/${test_name}.rs" ]]; then
    fail "suite list names ${crate}:${test_name}, but ${dir}/tests/${test_name}.rs does not exist"
    missing=1
  fi
done

if (( missing == 0 )); then
  for crate in powdb-storage powdb-query powdb-server; do
    args=()
    for suite in "${SUITES[@]}"; do
      if [[ "${suite%%:*}" == "${crate}" ]]; then
        args+=(--test "${suite##*:}")
      fi
    done
    (( ${#args[@]} == 0 )) && continue
    log "  cargo test --release -p ${crate} ${args[*]}"
    if ! cargo test --release -p "${crate}" "${args[@]}"; then
      fail "${crate}: release-profile corruption suite failed"
    fi
  done
fi

# ── 3b. Wire fuzz corpus at a live RELEASE server ──────────────────────────
# An abort takes the process down. Feeding every checked-in wire frame at the
# release binary and then asking it to answer a query is a direct test of the
# shipped panic configuration, which no in-process test can perform.
log "step 3b: wire fuzz corpus against a live release-profile server"
if ! cargo build --release -p powdb-server -p powdb-cli; then
  fail "could not build the release server/cli"
else
  DATADIR="$(mktemp -d "${TMPDIR:-/tmp}/powdb-relsuite-XXXXXX")"
  "${REPO_ROOT}/target/release/powdb-server" \
    --port "${PORT}" --bind 127.0.0.1 --data-dir "${DATADIR}" \
    >"${DATADIR}/server.log" 2>&1 &
  SERVER_PID=$!

  # ~30s: a fresh data dir has to be created, mmapped and WAL-initialised
  # before the listener binds, which is measurably slower than the 10s the
  # first version allowed on a loaded machine.
  waited=0
  until tcp_probe "${PORT}"; do
    waited=$((waited + 1))
    if (( waited > 120 )) || ! kill -0 "${SERVER_PID}" 2>/dev/null; then
      fail "release server did not start on port ${PORT}; log:
$(cat "${DATADIR}/server.log" 2>/dev/null)"
      SERVER_PID=""
      break
    fi
    sleep 0.25
  done

  if [[ -n "${SERVER_PID}" ]]; then
    sent=0
    for frame in "${REPO_ROOT}"/crates/query/fuzz/seeds/fuzz_wire/*; do
      [[ -f "${frame}" ]] || continue
      # One connection per frame: a malformed frame is expected to get the
      # connection closed, and reusing it would only prove the first result.
      # A refused connection is not counted, so `sent` below is a real count
      # of frames the server actually received rather than of loop iterations.
      if tcp_send_file "${PORT}" "${frame}"; then
        sent=$((sent + 1))
      fi
      if ! server_alive; then
        fail "release server DIED after wire frame $(basename "${frame}"): this is the abort the test-profile suite cannot see. Log:
$(cat "${DATADIR}/server.log" 2>/dev/null)"
        SERVER_PID=""
        break
      fi
    done
    log "  replayed ${sent} wire frame(s)"
    # Zero delivered frames is the miri failure mode again: the loop completes,
    # the server is trivially still alive because nothing was sent to it, and
    # the job reports a pass having tested nothing. Require that the corpus
    # both exists and actually reached the server.
    available="$(find "${REPO_ROOT}/crates/query/fuzz/seeds/fuzz_wire" -type f | wc -l | tr -d ' ')"
    if (( available == 0 )); then
      fail "no wire corpus at crates/query/fuzz/seeds/fuzz_wire; step 3b tested nothing"
    elif (( sent == 0 )); then
      fail "delivered 0 of ${available} wire frame(s) to the release server; step 3b tested nothing"
    fi
  fi

  # The server must still be alive AND still serving: a process that is up but
  # wedged is not a pass.
  if [[ -n "${SERVER_PID}" ]] && server_alive; then
    out="$("${REPO_ROOT}/target/release/powdb-cli" \
      --remote "127.0.0.1:${PORT}" --exec 'type RelSuite { required unique id: int }' 2>&1)"
    if ! grep -qF 'RelSuite' <<<"${out}"; then
      fail "release server survived the corpus but stopped serving queries: ${out}"
    else
      log "  server still serving after the corpus"
    fi
  elif (( FAILURES == 0 )); then
    fail "release server was not running at the end of the wire replay"
  fi
fi

echo
if (( FAILURES > 0 )); then
  echo "release-suite: ${FAILURES} failure(s)." >&2
  exit 1
fi
echo "release-suite: ALL-PASS."
