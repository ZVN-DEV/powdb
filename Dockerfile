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
# powdb-server depends on storage + query + auth; powdb-cli additionally pulls
# backup. The dep-cache stage must include the FULL dependency closure of the
# crates we build, or the cache layer is silently incomplete (the `|| true`
# below would hide the miss and re-resolve every build).
COPY Cargo.toml Cargo.lock ./
COPY crates/storage/Cargo.toml crates/storage/Cargo.toml
COPY crates/query/Cargo.toml   crates/query/Cargo.toml
COPY crates/server/Cargo.toml  crates/server/Cargo.toml
COPY crates/cli/Cargo.toml     crates/cli/Cargo.toml
COPY crates/auth/Cargo.toml    crates/auth/Cargo.toml
COPY crates/backup/Cargo.toml  crates/backup/Cargo.toml

# Create empty src trees so cargo can resolve+download deps without source
RUN mkdir -p crates/storage/src crates/query/src crates/server/src crates/cli/src \
              crates/auth/src crates/backup/src \
 && echo 'pub fn _stub() {}' > crates/storage/src/lib.rs \
 && echo 'pub fn _stub() {}' > crates/query/src/lib.rs \
 && echo 'pub fn _stub() {}' > crates/server/src/lib.rs \
 && echo 'pub fn _stub() {}' > crates/auth/src/lib.rs \
 && echo 'pub fn _stub() {}' > crates/backup/src/lib.rs \
 && echo 'fn main() {}'      > crates/server/src/main.rs \
 && echo 'fn main() {}'      > crates/cli/src/main.rs \
 && . /cross-env \
 && cargo build --release --target "$(cat /rust-target)" -p powdb-server 2>/dev/null || true

# Now copy real source and build for real
COPY crates ./crates
RUN . /cross-env \
 && touch crates/storage/src/lib.rs crates/query/src/lib.rs crates/server/src/lib.rs \
          crates/auth/src/lib.rs crates/backup/src/lib.rs \
          crates/server/src/main.rs crates/cli/src/main.rs \
 && cargo build --release --target "$(cat /rust-target)" -p powdb-server \
 && cp "target/$(cat /rust-target)/release/powdb-server" /powdb-server

# ─── Runtime ────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# tini reaps zombies and forwards signals so SIGTERM from fly cleanly stops the server
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini \
 && rm -rf /var/lib/apt/lists/*

# Persistent data dir; fly volume will be mounted here
RUN mkdir -p /data
VOLUME ["/data"]

COPY --from=builder /powdb-server /usr/local/bin/powdb-server

ENV RUST_LOG=info \
    POWDB_DATA=/data \
    POWDB_PORT=5433

EXPOSE 5433

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/usr/local/bin/powdb-server"]
