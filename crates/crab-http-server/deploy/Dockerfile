# syntax=docker/dockerfile:1

FROM node:22.12.0-bookworm-slim@sha256:35531c52ce27b6575d69755c73e65d4468dba93a25644eed56dc12879cae9213 AS frontend

WORKDIR /build
COPY packages/repository/package.json packages/repository/package-lock.json packages/repository/
RUN npm ci --prefix packages/repository
COPY packages/repository/index.html packages/repository/tsconfig.json packages/repository/vite.config.ts packages/repository/
COPY packages/repository/src/ packages/repository/src/
RUN npm run build --prefix packages/repository

FROM rust:1.93-bookworm@sha256:7c4ae649a84014c467d79319bbf17ce2632ae8b8be123ac2fb2ea5be46823f31 AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ .cargo/
COPY crab/ crab/
COPY crates/ crates/
COPY --from=frontend /build/packages/repository/dist/ packages/repository/dist/
RUN cargo build --release --locked -p crab-http-server

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171

RUN printf 'crab:x:10001:\n' >> /etc/group \
    && printf 'crab:x:10001:10001:Crab server:/var/lib/crab:/bin/false\n' >> /etc/passwd \
    && install -d -o 10001 -g 10001 /etc/crab /var/lib/crab/tmp

COPY --from=builder /build/target/release/crab-http-server /usr/local/bin/crab-http-server

ENV TMPDIR=/var/lib/crab/tmp
USER crab:crab
EXPOSE 8788
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=12s --start-period=15s --retries=3 \
    CMD ["/usr/local/bin/crab-http-server", "--config", "/etc/crab/server.toml", "--healthcheck"]

ENTRYPOINT ["/usr/local/bin/crab-http-server"]
CMD ["--config", "/etc/crab/server.toml"]
