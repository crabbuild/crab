# syntax=docker/dockerfile:1

FROM node:22.23.2-bookworm-slim@sha256:83f487e0a63425e5b4d146fb5e5be574bcbe1b7b843d3ebafdd95eaf7767a7e5 AS frontend

WORKDIR /build
COPY packages/ui/.npmrc packages/ui/package.json packages/ui/package-lock.json packages/ui/
RUN npm ci --prefix packages/ui
COPY packages/ui/index.html packages/ui/tsconfig.json packages/ui/vite.config.ts packages/ui/
COPY packages/web/crab.optimized.svg packages/web/crab.optimized.svg
COPY packages/ui/src/ packages/ui/src/
RUN npm run build --prefix packages/ui

FROM rust:1.97-bookworm@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97 AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY vendor/nfs3_server/ vendor/nfs3_server/
COPY crab/ crab/
COPY crates/ crates/
COPY packages/desktop/agent/ packages/desktop/agent/
COPY --from=frontend /build/packages/ui/dist/ packages/ui/dist/
ARG GIT_SHA=unknown
RUN GIT_SHA="$GIT_SHA" cargo build --release --locked -p crab-server --features provider-cdn

FROM caddy:2.11.9-builder-alpine@sha256:646fee578e680989ca5c7fd1ce6f1de574d19fd71ef2221810cc0510a9f40c86 AS proxy-builder

# Remove the dependency override when a stable Caddy includes the GO-2026-6860 fix.
RUN xcaddy build v2.11.9 --replace golang.org/x/net=golang.org/x/net@v0.62.0 --output /usr/bin/caddy

FROM caddy:2.11.9-alpine@sha256:003a97808e781aa9f7e75cc2f43d34d461099031faf5e20faefb9f61126836f0 AS proxy
COPY --from=proxy-builder /usr/bin/caddy /usr/bin/caddy

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171

ARG GIT_SHA=unknown
LABEL org.opencontainers.image.revision="$GIT_SHA"

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        apt=2.6.2 \
        libapt-pkg6.0=2.6.2 \
        git \
        openssl \
        libpcre2-8-0=10.42-1+deb12u2 \
    && rm -rf /var/lib/apt/lists/* \
    && printf 'crab:x:10001:\n' >> /etc/group \
    && printf 'crab:x:10001:10001:Crab server:/var/lib/crab:/bin/false\n' >> /etc/passwd \
    && install -d -o 10001 -g 10001 /etc/crab /var/lib/crab /var/lib/crab/tmp /var/lib/crab/cells /var/lib/crab/server

COPY --from=builder /build/target/release/crab-server /usr/local/bin/crab-server
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=proxy /usr/bin/caddy /usr/local/bin/caddy
COPY crates/crab-server/deploy/compose.Caddyfile /etc/crab/container.Caddyfile
COPY --chmod=0755 crates/crab-server/deploy/entrypoint.sh /usr/local/bin/crab-server-entrypoint

ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
    TMPDIR=/var/lib/crab/tmp \
    XDG_CONFIG_HOME=/var/lib/crab/tmp/caddy/config \
    XDG_DATA_HOME=/var/lib/crab/tmp/caddy/data
USER crab:crab
EXPOSE 8080 8788 8789
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=12s --start-period=15s --retries=3 \
    CMD ["/usr/local/bin/crab-server", "--config", "/etc/crab/server.toml", "healthcheck"]

ENTRYPOINT ["/usr/local/bin/crab-server-entrypoint"]
CMD []
