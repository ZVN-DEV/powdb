#!/usr/bin/env bash
# scripts/dev.sh — one-command local PowDB dev loop.
#
# Subcommands:
#   up     boot powdb-server in a tmp data dir on a free port (dev defaults:
#          no password, no TLS, RUST_LOG=info). Writes a pidfile, prints
#          connection + REPL one-liner.
#   repl   launch powdb-cli --remote against the running dev server.
#   bench  run cargo run --release -p powdb-compare. Optionally brings up
#          the WS6 Postgres docker-compose if POWDB_BENCH_PG_URL is unset.
#   down   stop the dev server, clean the tmp data dir.
#
# Honesty: this script promises a single-tenant local dev loop. It is NOT
# the production launcher.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PIDFILE="${REPO_ROOT}/target/dev-server.pid"
PORTFILE="${REPO_ROOT}/target/dev-server.port"
DATADIRFILE="${REPO_ROOT}/target/dev-server.datadir"
LOGFILE="${REPO_ROOT}/target/dev-server.log"

usage() {
  cat <<'EOF'
scripts/dev.sh — one-command local PowDB dev loop

USAGE:
    scripts/dev.sh <command>

COMMANDS:
    up        Boot powdb-server in a tmp data dir on a free port.
    repl      Connect powdb-cli --remote to the running dev server.
    bench     Run the SQLite (and optional Postgres) comparison benchmark.
    down      Stop the dev server and clean up tmp state.
    --help    Show this message.

ENV:
    POWDB_BENCH_PG_URL     If set, `bench` uses this Postgres URL.
                           If unset and crates/compare/docker-compose.yml
                           exists, `bench` offers to start it.
EOF
}

# Find a free TCP port. Python is in every CI image we use; fall back to a
# bash /dev/tcp probe if it isn't.
find_free_port() {
  if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
    return
  fi
  # Fallback: probe in the ephemeral range until one is free.
  local port
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    port=$(( (RANDOM % 16384) + 49152 ))
    if ! (echo >"/dev/tcp/127.0.0.1/${port}") >/dev/null 2>&1; then
      echo "${port}"
      return
    fi
  done
  echo "dev.sh: could not find a free port" >&2
  return 1
}

cmd_up() {
  mkdir -p "${REPO_ROOT}/target"

  if [[ -f "${PIDFILE}" ]] && kill -0 "$(cat "${PIDFILE}")" 2>/dev/null; then
    echo "dev.sh: server already running (pid $(cat "${PIDFILE}"), port $(cat "${PORTFILE}" 2>/dev/null || echo '?'))"
    return 0
  fi

  local port datadir
  port="$(find_free_port)"
  datadir="$(mktemp -d "${TMPDIR:-/tmp}/powdb-dev-XXXXXX")"

  echo "dev.sh: building powdb-server (release) — first run takes a few minutes…"
  cargo build --release -p powdb-server >&2

  echo "dev.sh: starting powdb-server on port ${port}, data dir ${datadir}"
  # Dev defaults: no password, no TLS, info logs. We do NOT inherit the
  # caller's POWDB_* env to keep the dev loop deterministic.
  env -i \
      PATH="${PATH}" \
      HOME="${HOME:-}" \
      RUST_LOG="info" \
      RUST_BACKTRACE="1" \
      "${REPO_ROOT}/target/release/powdb-server" \
      --port "${port}" \
      --bind "127.0.0.1" \
      --data-dir "${datadir}" \
      >"${LOGFILE}" 2>&1 &
  local pid=$!

  echo "${pid}" >"${PIDFILE}"
  echo "${port}" >"${PORTFILE}"
  echo "${datadir}" >"${DATADIRFILE}"

  # Wait briefly for the server to bind. If it dies, surface the log.
  local waited=0
  while (( waited < 50 )); do
    if ! kill -0 "${pid}" 2>/dev/null; then
      echo "dev.sh: powdb-server exited during startup — log follows:" >&2
      cat "${LOGFILE}" >&2 || true
      rm -f "${PIDFILE}" "${PORTFILE}" "${DATADIRFILE}"
      return 1
    fi
    if (echo >"/dev/tcp/127.0.0.1/${port}") >/dev/null 2>&1; then
      break
    fi
    sleep 0.1
    waited=$(( waited + 1 ))
  done

  cat <<EOF
dev.sh: powdb-server up
    pid:        ${pid}
    port:       ${port}
    data dir:   ${datadir}
    log:        ${LOGFILE}

Connect:
    cargo run -p powdb-cli --release -- --remote 127.0.0.1:${port}

Stop:
    scripts/dev.sh down
EOF
}

cmd_repl() {
  if [[ ! -f "${PORTFILE}" ]]; then
    echo "dev.sh: no dev server running — run 'scripts/dev.sh up' first." >&2
    return 1
  fi
  local port
  port="$(cat "${PORTFILE}")"
  echo "dev.sh: connecting powdb-cli to 127.0.0.1:${port}"
  cargo run -p powdb-cli --release -- --remote "127.0.0.1:${port}" "$@"
}

cmd_bench() {
  local compose_file="${REPO_ROOT}/crates/compare/docker-compose.yml"

  if [[ -z "${POWDB_BENCH_PG_URL:-}" ]] && [[ -f "${compose_file}" ]]; then
    if command -v docker >/dev/null 2>&1; then
      echo "dev.sh: POWDB_BENCH_PG_URL unset; bringing up local Postgres via docker compose"
      docker compose -f "${compose_file}" up -d
      export POWDB_BENCH_PG_URL="postgresql://postgres:powdb@localhost:5432/powdb_bench"
      echo "dev.sh: using POWDB_BENCH_PG_URL=${POWDB_BENCH_PG_URL}"
    else
      echo "dev.sh: docker not available; benchmark will run SQLite-only" >&2
    fi
  else
    if [[ -n "${POWDB_BENCH_PG_URL:-}" ]]; then
      echo "dev.sh: using preset POWDB_BENCH_PG_URL"
    else
      echo "dev.sh: no docker-compose for Postgres found; benchmark will run SQLite-only"
    fi
  fi

  cargo run --release -p powdb-compare
}

cmd_down() {
  local stopped=0
  if [[ -f "${PIDFILE}" ]]; then
    local pid
    pid="$(cat "${PIDFILE}")"
    if kill -0 "${pid}" 2>/dev/null; then
      echo "dev.sh: stopping powdb-server (pid ${pid})"
      kill "${pid}" 2>/dev/null || true
      # Give it 5s to exit cleanly, then SIGKILL.
      local waited=0
      while kill -0 "${pid}" 2>/dev/null && (( waited < 50 )); do
        sleep 0.1
        waited=$(( waited + 1 ))
      done
      if kill -0 "${pid}" 2>/dev/null; then
        echo "dev.sh: SIGTERM ignored; sending SIGKILL"
        kill -9 "${pid}" 2>/dev/null || true
      fi
      stopped=1
    else
      echo "dev.sh: stale pidfile (${pid} not running) — cleaning up"
    fi
    rm -f "${PIDFILE}"
  fi
  if [[ -f "${PORTFILE}" ]]; then
    rm -f "${PORTFILE}"
  fi
  if [[ -f "${DATADIRFILE}" ]]; then
    local datadir
    datadir="$(cat "${DATADIRFILE}")"
    if [[ -d "${datadir}" ]] && [[ "${datadir}" == "${TMPDIR:-/tmp}"/powdb-dev-* || "${datadir}" == /tmp/powdb-dev-* || "${datadir}" == /var/folders/*/powdb-dev-* ]]; then
      echo "dev.sh: removing tmp data dir ${datadir}"
      rm -rf "${datadir}"
    else
      echo "dev.sh: refusing to remove ${datadir} (not a recognized tmp path)" >&2
    fi
    rm -f "${DATADIRFILE}"
  fi
  if (( stopped == 0 )); then
    echo "dev.sh: no dev server was running"
  fi
}

main() {
  if (( $# == 0 )); then
    usage
    exit 2
  fi
  case "$1" in
    up)    shift; cmd_up "$@" ;;
    repl)  shift; cmd_repl "$@" ;;
    bench) shift; cmd_bench "$@" ;;
    down)  shift; cmd_down "$@" ;;
    --help|-h|help) usage ;;
    *)
      echo "dev.sh: unknown command '$1'" >&2
      usage
      exit 2
      ;;
  esac
}

main "$@"
