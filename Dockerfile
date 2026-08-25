# syntax=docker/dockerfile:1.7
# ─── Builder ────────────────────────────────────────────────────────────────
# Pinned to $BUILDPLATFORM so the Rust compile always runs natively on the
# host arch and cross-compiles to $TARGETARCH. Under multi-arch buildx this
# keeps the (slow) DB-engine build off QEMU emulation — only the tiny runtime
# stage below runs emulated for the non-native arch.
FROM --platform=$BUILDPLATFORM rust:1.95-slim-bookworm AS builder

WORKDIR /src

# Resolve the Rust target triple and, when the target arch differs from the
# build (host) arch, install the GNU cross toolchain for it. This is
# direction-generic: it cross-compiles amd64→arm64 (the CI release path) and
# arm64→amd64 symmetrically, and installs nothing when target == host (native).
#
# The cross gcc package alone is NOT enough: aws-lc-sys (pulled by powdb-server's
# `tls` feature) compiles C with cc-rs, which needs the TARGET libc headers +
# sysroot. Those live in `libc6-dev-<arch>-cross`, a *recommended* (not required)
# dep of the gcc-cross package — so under `--no-install-recommends` it is skipped
# and the cross gcc falls back to the host /usr/include (wrong arch), failing its
# feature tests with "bits/libc-header-start.h / asm/types.h: No such file". We
# install it explicitly. cc-rs already auto-derives the `<prefix>-gcc` compiler
# from the target triple; once the sysroot headers exist the build resolves them.
#
# Toolchain env (cargo linker + cc-rs CC/CXX/AR) is written to /cross-env and
# sourced by the build steps below, ONLY when cross-compiling. Exporting it
# globally would point a native build at a cross gcc that isn't installed.
ARG TARGETARCH
ARG BUILDARCH
RUN set -eux; \
    : > /cross-env; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-gnu;  cross_prefix=x86_64-linux-gnu;  gcc_pkg=gcc-x86-64-linux-gnu ;; \
      arm64) triple=aarch64-unknown-linux-gnu; cross_prefix=aarch64-linux-gnu; gcc_pkg=gcc-aarch64-linux-gnu ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    echo "$triple" > /rust-target; \
    rustup target add "$triple"; \
    if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
      apt-get update; \
      apt-get install -y --no-install-recommends "$gcc_pkg" "libc6-dev-${TARGETARCH}-cross"; \
      rm -rf /var/lib/apt/lists/*; \
      triple_us="$(echo "$triple" | tr '-' '_')"; \
      triple_env="$(echo "$triple_us" | tr 'a-z' 'A-Z')"; \
      { \
        echo "export CARGO_TARGET_${triple_env}_LINKER=${cross_prefix}-gcc"; \
        echo "export CC_${triple_us}=${cross_prefix}-gcc"; \
        echo "export CXX_${triple_us}=${cross_prefix}-g++"; \
        echo "export AR_${triple_us}=${cross_prefix}-ar"; \
      } > /cross-env; \
    fi

# No RUSTFLAGS/target-cpu override is set here: .cargo/config.toml (which pins
# target-cpu=native for local dev) is never copied into the build context, so
# cargo uses the portable baseline target-cpu for each triple — the binaries
# stay runnable across the whole arch, no SIGILL on older silicon.

# Cache deps separately from source by copying manifests first.
# powdb-server depends on storage + query + auth + sync; powdb-cli additionally
# pulls backup. The dep-cache stage must include the FULL dependency closure of
# the crates we build, or workspace resolution fails and the cache layer warms
# nothing. That exact miss shipped: sync was added to powdb-server at v0.8.0,
# this copy list was never updated, and a `2>/dev/null || true` on the build
# swallowed the resolution error, so every image build silently re-compiled
# all deps. The stub build now fails the image build instead of hiding.
# (No --locked here: with the glob workspace's other members absent, cargo must
# trim the lock copy for this throwaway layer; the real build below is locked.)
COPY Cargo.toml Cargo.lock ./
COPY crates/storage/Cargo.toml crates/storage/Cargo.toml
COPY crates/query/Cargo.toml   crates/query/Cargo.toml
COPY crates/server/Cargo.toml  crates/server/Cargo.toml
COPY crates/cli/Cargo.toml     crates/cli/Cargo.toml
COPY crates/auth/Cargo.toml    crates/auth/Cargo.toml
COPY crates/backup/Cargo.toml  crates/backup/Cargo.toml
COPY crates/sync/Cargo.toml    crates/sync/Cargo.toml

# Create empty src trees so cargo can resolve+download deps without source
RUN mkdir -p crates/storage/src crates/query/src crates/server/src crates/cli/src \
              crates/auth/src crates/backup/src crates/sync/src \
 && echo 'pub fn _stub() {}' > crates/storage/src/lib.rs \
 && echo 'pub fn _stub() {}' > crates/query/src/lib.rs \
 && echo 'pub fn _stub() {}' > crates/server/src/lib.rs \
 && echo 'pub fn _stub() {}' > crates/auth/src/lib.rs \
 && echo 'pub fn _stub() {}' > crates/backup/src/lib.rs \
 && echo 'pub fn _stub() {}' > crates/sync/src/lib.rs \
 && echo 'fn main() {}'      > crates/server/src/main.rs \
 && echo 'fn main() {}'      > crates/cli/src/main.rs \
 && . /cross-env \
 && cargo build --release --target "$(cat /rust-target)" -p powdb-server

# Now copy real source and build for real. The stub build above trimmed its
# throwaway copy of the lock (absent workspace members), so restore the
# pristine one: the real build is --locked and must see the committed pins.
COPY crates ./crates
COPY Cargo.lock ./Cargo.lock
RUN . /cross-env \
 && touch crates/storage/src/lib.rs crates/query/src/lib.rs crates/server/src/lib.rs \
          crates/auth/src/lib.rs crates/backup/src/lib.rs crates/sync/src/lib.rs \
          crates/server/src/main.rs crates/cli/src/main.rs \
 && cargo build --release --locked --target "$(cat /rust-target)" -p powdb-server \
 && cp "target/$(cat /rust-target)/release/powdb-server" /powdb-server

# ─── Runtime ────────────────────────────────────────────────────────────────
# Pinned by digest (multi-arch index digest for debian:bookworm-slim as of
# 2026-07-24) so the runtime layer is reproducible and cannot be swapped under
# us by a tag repoint. Refresh deliberately with:
#   docker buildx imagetools inspect debian:bookworm-slim
FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818 AS runtime

# tini reaps zombies and forwards signals so SIGTERM from fly cleanly stops the server
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini \
 && rm -rf /var/lib/apt/lists/*

# Run as an unprivileged user: the server needs no root capability (it binds
# 5433, well above the privileged range) and a database process should not be
# able to write outside its data dir. Fixed uid/gid 10001 so a host bind mount
# can be chowned to a stable, known id.
RUN groupadd --system --gid 10001 powdb \
 && useradd --system --uid 10001 --gid 10001 --home-dir /data --shell /usr/sbin/nologin powdb

# Persistent data dir; fly volume will be mounted here.
# NOTE: an EMPTY named/anonymous volume inherits this ownership at creation, so
# `docker run -v powdb_data:/data` works unprivileged. A HOST BIND MOUNT or a
# pre-existing root-owned volume does NOT: chown it to 10001:10001 on the host,
# or run the container with `--user 0:0` to restore the previous behaviour.
RUN mkdir -p /data && chown powdb:powdb /data
VOLUME ["/data"]

COPY --from=builder /powdb-server /usr/local/bin/powdb-server

# Container healthcheck. The runtime image ships no curl and no wget on
# purpose: adding an HTTP client (and its TLS stack) to a production database
# image just to answer a probe is more attack surface than the probe is worth.
# Everything below is bash's built-in /dev/tcp redirection plus coreutils/grep,
# all already present in debian-slim.
#
# No single probe is correct in every configuration, so the script picks the
# strongest one the running config actually supports. See the block comment
# inside it for the three modes and why each degrades the way it does.
COPY --chmod=0755 <<'HEALTHCHECK_SH' /usr/local/bin/powdb-healthcheck
#!/bin/bash
# Health probe for powdb-server, in three modes:
#
#   1. POWDB_METRICS_ADDR set: GET /health on the metrics listener and require
#      the documented "ok powdb <version>" body. This is the best answer
#      available and the only mode that logs nothing on the server side. The
#      handler never takes the engine lock, so a slow query cannot make a
#      healthy process look dead and get it restarted mid-write.
#   2. No metrics listener and no TLS: send a pre-auth PING frame on the wire
#      port and require a PONG. The server accepts PING before CONNECT exactly
#      so load balancers can probe it. Costs one "accepted connection" INFO
#      line per interval in the server log.
#   3. No metrics listener and TLS enabled: bash cannot speak TLS, and a
#      plaintext connect would log a handshake failure and bump the
#      tls_failure counter every interval. Fall back to proving the
#      powdb-server process exists, and say so on stderr (docker inspect keeps
#      the last probe outputs).
#
# Modes 2 and 3 are weaker than mode 1. Set POWDB_METRICS_ADDR to get mode 1.
set -u

note() { printf 'powdb-healthcheck: %s\n' "$*" >&2; }

# A wildcard bind is reached over loopback from inside the container.
loopback_if_wildcard() {
  case "$1" in
    '' | '0.0.0.0' | '::' | '[::]' | '*') printf '127.0.0.1' ;;
    *) printf '%s' "$1" ;;
  esac
}

if [ -n "${POWDB_METRICS_ADDR:-}" ]; then
  host="$(loopback_if_wildcard "${POWDB_METRICS_ADDR%:*}")"
  port="${POWDB_METRICS_ADDR##*:}"
  exec 3<>"/dev/tcp/${host}/${port}" || {
    note "cannot connect to metrics listener ${host}:${port}"
    exit 1
  }
  printf 'GET /health HTTP/1.0\r\nHost: %s\r\nConnection: close\r\n\r\n' "$host" >&3 || exit 1
  if grep -q '^ok powdb ' <&3; then
    exit 0
  fi
  note "metrics listener did not answer /health with 'ok powdb'"
  exit 1
fi

if [ -z "${POWDB_TLS_CERT:-}" ] && [ -z "${POWDB_TLS_KEY:-}" ]; then
  host="$(loopback_if_wildcard "${POWDB_BIND:-}")"
  port="${POWDB_PORT:-5433}"
  exec 3<>"/dev/tcp/${host}/${port}" || {
    note "cannot connect to wire port ${host}:${port}"
    exit 1
  }
  # Frame layout: [type][flags][payload length u32 LE]. 0x11 = PING, no payload.
  printf '\x11\x00\x00\x00\x00\x00' >&3 || exit 1
  reply=''
  IFS= read -r -N 1 -t 5 reply <&3 || true
  # 0x12 = PONG
  if [ "$reply" = $'\x12' ]; then
    exit 0
  fi
  note "wire port did not answer PING with PONG"
  exit 1
fi

note "TLS is on and POWDB_METRICS_ADDR is unset, so only process liveness is checkable; set POWDB_METRICS_ADDR for a real probe"
for proc in /proc/[0-9]*; do
  [ -r "$proc/comm" ] || continue
  read -r comm < "$proc/comm" || continue
  if [ "$comm" = "powdb-server" ]; then
    exit 0
  fi
done
note "no powdb-server process found"
exit 1
HEALTHCHECK_SH

ENV RUST_LOG=info \
    POWDB_DATA=/data \
    POWDB_PORT=5433

EXPOSE 5433

USER 10001:10001

# Declared last so a changed revision/version/timestamp only invalidates these
# two trailing layers, not the apt and build layers above.
ARG VERSION=0.0.0-dev
ARG REVISION=unknown
ARG CREATED=1970-01-01T00:00:00Z

# OCI image annotations. Without these, `docker inspect` reports Labels=null,
# scanners and SBOM tools get nothing, and there is no way to map a running
# container back to the commit that built it. The three dynamic values come
# from build args wired up in .github/workflows/release.yml; a local
# `docker build` with no args gets the honest placeholder defaults above
# rather than a fabricated version.
LABEL org.opencontainers.image.title="PowDB" \
      org.opencontainers.image.description="PowDB database server: a from-scratch storage and query engine with the PowQL pipeline language and a SQL frontend" \
      org.opencontainers.image.url="https://zvn-dev.github.io/powdb/" \
      org.opencontainers.image.documentation="https://github.com/ZVN-DEV/powdb#readme" \
      org.opencontainers.image.source="https://github.com/ZVN-DEV/powdb" \
      org.opencontainers.image.vendor="ZVN DEV" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.created="${CREATED}" \
      org.opencontainers.image.base.name="debian:bookworm-slim" \
      org.opencontainers.image.base.digest="sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818"

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD ["/usr/local/bin/powdb-healthcheck"]

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/usr/local/bin/powdb-server"]
