# Multi-stage Dockerfile for crab-lfs-server.
#
# Build from the repository root:
#   docker build -f crab/deploy/lfs-service/Dockerfile -t crab-lfs-server .
#
# The configuration and upload spool are mounted at runtime. Object-store
# credentials should come from workload identity or the container secret
# mechanism, never from this image.

FROM rust:1-slim-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        cmake \
        libssl-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY crab/ crab/

RUN cargo build --release --locked -p crab-lfs-server --bin crab-lfs-server

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home-dir /var/lib/crab-lfs --create-home --shell /usr/sbin/nologin crab-lfs \
    && mkdir -p /etc/crab-lfs /var/lib/crab-lfs/spool \
    && chown -R crab-lfs:crab-lfs /var/lib/crab-lfs

COPY --from=builder /build/target/release/crab-lfs-server /usr/local/bin/crab-lfs-server

USER crab-lfs
EXPOSE 8444

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:8444/healthz >/dev/null

ENTRYPOINT ["/usr/local/bin/crab-lfs-server", "--config", "/etc/crab-lfs/config.toml"]
