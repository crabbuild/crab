# syntax=docker/dockerfile:1

FROM node:22.23.2-bookworm-slim@sha256:83f487e0a63425e5b4d146fb5e5be574bcbe1b7b843d3ebafdd95eaf7767a7e5 AS frontend

WORKDIR /build
COPY packages/repository/package.json packages/repository/package-lock.json packages/repository/
RUN npm ci --prefix packages/repository
COPY packages/repository/index.html packages/repository/tsconfig.json packages/repository/vite.config.ts packages/repository/
COPY packages/repository/src/ packages/repository/src/
RUN npm run build --prefix packages/repository

FROM rust:1.97-bookworm@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97 AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crab/ crab/
COPY crates/ crates/
COPY --from=frontend /build/packages/repository/dist/ packages/repository/dist/
RUN cargo build --release --locked -p crab-http-server

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171

RUN printf 'crab:x:10001:\n' >> /etc/group \
    && printf 'crab:x:10001:10001:Crab server:/var/lib/crab:/bin/false\n' >> /etc/passwd \
    && install -d -o 10001 -g 10001 /etc/crab /var/lib/crab/tmp

COPY --from=builder /build/target/release/crab-http-server /usr/local/bin/crab-http-server
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
    TMPDIR=/var/lib/crab/tmp
USER crab:crab
EXPOSE 8788 8789
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=12s --start-period=15s --retries=3 \
    CMD ["/usr/local/bin/crab-http-server", "--config", "/etc/crab/server.toml", "healthcheck"]

ENTRYPOINT ["/usr/local/bin/crab-http-server"]
CMD ["--config", "/etc/crab/server.toml"]
