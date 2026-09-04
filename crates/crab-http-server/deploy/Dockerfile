# syntax=docker/dockerfile:1

FROM node:22.12.0-bookworm-slim AS frontend

WORKDIR /build
COPY packages/repository/package.json packages/repository/package-lock.json packages/repository/
RUN npm ci --prefix packages/repository
COPY packages/repository/index.html packages/repository/tsconfig.json packages/repository/vite.config.ts packages/repository/
COPY packages/repository/src/ packages/repository/src/
RUN npm run build --prefix packages/repository

FROM rust:1.93-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ .cargo/
COPY crab/ crab/
COPY crates/ crates/
COPY --from=frontend /build/packages/repository/dist/ packages/repository/dist/
RUN cargo build --release --locked -p crab-http-server

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 crab \
    && useradd --uid 10001 --gid crab --no-create-home --home-dir /var/lib/crab crab \
    && install -d -o crab -g crab /etc/crab /var/lib/crab/tmp

COPY --from=builder /build/target/release/crab-http-server /usr/local/bin/crab-http-server

ENV TMPDIR=/var/lib/crab/tmp
USER crab:crab
EXPOSE 8788
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=12s --start-period=15s --retries=3 \
    CMD curl --fail --silent --show-error --max-time 10 http://127.0.0.1:8788/readyz >/dev/null

ENTRYPOINT ["/usr/local/bin/crab-http-server"]
CMD ["--config", "/etc/crab/server.toml"]
