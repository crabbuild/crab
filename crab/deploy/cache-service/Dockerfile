# Multi-stage Dockerfile for crab-cache-server.
#
# Build:
#   docker build -f crab/deploy/cache-service/Dockerfile -t crab-cache-server .
#
# Run:
#   docker run -p 8443:8443 \
#     -v /data/crab-cache:/data/crab-cache \
#     -v /etc/crab-cache-server:/etc/crab-cache-server:ro \
#     crab-cache-server

# ---------------------------------------------------------------------------
# Build stage
# ---------------------------------------------------------------------------
FROM rust:1.87-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy workspace root files first for layer caching.
COPY Cargo.toml Cargo.lock ./

# Copy the crates the workspace needs.
COPY crates/ crates/
COPY crab/ crab/
COPY beyond-desktop/agent/ beyond-desktop/agent/

RUN cargo build --release --manifest-path Cargo.toml -p crab-cache-server --bin crab-cache-server

# ---------------------------------------------------------------------------
# Runtime stage — minimal image with just the binary.
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/crab-cache-server /usr/local/bin/crab-cache-server

# Default config location; mount your own at runtime.
RUN mkdir -p /etc/crab-cache-server /data/crab-cache

EXPOSE 8443

ENTRYPOINT ["/usr/local/bin/crab-cache-server", "--config", "/etc/crab-cache-server/config.toml"]
