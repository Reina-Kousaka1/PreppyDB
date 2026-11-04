# syntax=docker/dockerfile:1.7
#
# Multi-stage build for PreppyDB. Produces one image that works on linux/amd64 and
# linux/arm64 (Raspberry Pi 4 and later):
#
#   docker buildx build --platform linux/amd64,linux/arm64 -f docker/Dockerfile -t preppydb:local .
#
# The build stage runs on the native platform ($BUILDPLATFORM), cross-compiles for the
# requested $TARGETARCH and copies only the two static-ish binaries into the runtime
# image. Nothing is pushed anywhere by this file.

ARG RUST_VERSION=1.99
FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-bookworm AS build

ARG TARGETARCH
ARG CARGO_PROFILE=release

# Cross toolchain for arm64. The amd64 build uses the image's own gcc.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Copy manifests first so dependency resolution is cached when only source changes.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY clients ./clients

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    set -eu; \
    case "${TARGETARCH:-amd64}" in \
      amd64) RUST_TARGET=x86_64-unknown-linux-gnu ;; \
      arm64) RUST_TARGET=aarch64-unknown-linux-gnu; \
             rustup target add "$RUST_TARGET" >/dev/null; \
             export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc ;; \
      *) echo "unsupported TARGETARCH '$TARGETARCH' (want amd64 or arm64)" >&2; exit 1 ;; \
    esac; \
    cargo build --locked --profile "$CARGO_PROFILE" --target "$RUST_TARGET" \
        --bin preppydb-server --bin preppydb; \
    mkdir -p /out; \
    cp "target/$RUST_TARGET/$CARGO_PROFILE/preppydb-server" /out/; \
    cp "target/$RUST_TARGET/$CARGO_PROFILE/preppydb" /out/; \
    strip /out/preppydb-server /out/preppydb 2>/dev/null || true

# --------------------------------------------------------------------------- runtime

FROM debian:bookworm-slim AS runtime

# gosu lets the entrypoint fix the ownership of a fresh (root-owned) volume and then
# drop to the unprivileged user without any shell or setuid binary of our own.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates gosu tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 preppydb \
    && useradd --uid 10001 --gid preppydb --shell /usr/sbin/nologin --no-create-home preppydb \
    && install -d -o preppydb -g preppydb -m 0700 /var/lib/preppydb

COPY --from=build /out/preppydb-server /usr/local/bin/preppydb-server
COPY --from=build /out/preppydb /usr/local/bin/preppydb
COPY docker/entrypoint.sh /usr/local/bin/preppydb-entrypoint
RUN chmod 0755 /usr/local/bin/preppydb-server /usr/local/bin/preppydb /usr/local/bin/preppydb-entrypoint

ENV PREPPYDB_DATA_DIR=/var/lib/preppydb \
    PREPPYDB_LISTEN=0.0.0.0:7077 \
    PREPPYDB_APP_UID=10001 \
    PREPPYDB_APP_GID=10001 \
    RUST_BACKTRACE=1

# Data lives on a volume so a container restart/recreate keeps every document.
VOLUME ["/var/lib/preppydb"]
EXPOSE 7077

# The container starts as root *only* so the entrypoint can take ownership of a freshly
# created (root-owned) volume and then `exec gosu 10001:10001` — the database process
# itself never runs as root. `docker run --user 10001:10001` also works and skips that
# step, as long as the mounted directory is writable by that uid.
USER root
WORKDIR /var/lib/preppydb

# tini reaps children and forwards SIGTERM, so `docker stop` triggers the server's
# graceful shutdown (checkpoint + WAL close) instead of killing it abruptly.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/preppydb-entrypoint"]
CMD []

# No token needed: /healthz and /readyz are deliberately public.
# Shell form so the port can follow PREPPYDB_LISTEN; no token is needed.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD preppydb health --url "http://127.0.0.1:${PREPPYDB_LISTEN##*:}"
