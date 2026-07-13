# syntax=docker/dockerfile:1.7
# ─── Builder ────────────────────────────────────────────────────────────────
# Pinned to $BUILDPLATFORM so the Rust compile always runs natively on the
# host arch and cross-compiles to $TARGETARCH. Under multi-arch buildx this
# keeps the (slow) DB-engine build off QEMU emulation — only the tiny runtime
# stage below runs emulated for the non-native arch.
FROM --platform=$BUILDPLATFORM rust:1.95-slim-bookworm AS builder

WORKDIR /src

# Resolve the Rust target triple + cross toolchain for the requested arch.
ARG TARGETARCH
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-gnu ;; \
      arm64) triple=aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    echo "$triple" > /rust-target; \
    rustup target add "$triple"; \
    if [ "$TARGETARCH" = arm64 ]; then \
      apt-get update; \
      apt-get install -y --no-install-recommends gcc-aarch64-linux-gnu; \
      rm -rf /var/lib/apt/lists/*; \
    fi

# Cross linker for the aarch64 target (unused when building amd64 natively).
# No RUSTFLAGS/target-cpu override is set here: .cargo/config.toml (which pins
# target-cpu=native for local dev) is never copied into the build context, so
# cargo uses the portable baseline target-cpu for each triple — the binaries
# stay runnable across the whole arch, no SIGILL on older silicon.
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc

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
 && cargo build --release --target "$(cat /rust-target)" -p powdb-server 2>/dev/null || true

# Now copy real source and build for real
COPY crates ./crates
RUN touch crates/storage/src/lib.rs crates/query/src/lib.rs crates/server/src/lib.rs \
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
