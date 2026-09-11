# syntax=docker/dockerfile:1

FROM rust:1.93-bookworm@sha256:7c4ae649a84014c467d79319bbf17ce2632ae8b8be123ac2fb2ea5be46823f31 AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ .cargo/
COPY crab/ crab/
COPY crates/ crates/
RUN cargo build --release --locked -p crab-s3-gateway

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && printf 'crab:x:10001:\n' >> /etc/group \
    && printf 'crab:x:10001:10001:Crab S3 gateway:/var/lib/crab:/bin/false\n' >> /etc/passwd \
    && install -d -o 10001 -g 10001 /etc/crab /var/lib/crab/tmp /var/lib/crab/cache-volume

COPY --from=builder /build/target/release/crab-s3-gateway /usr/local/bin/crab-s3-gateway

ENV TMPDIR=/var/lib/crab/tmp
USER crab:crab
EXPOSE 8080 8081
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=15s --start-period=15s --retries=3 \
    CMD ["/usr/local/bin/crab-s3-gateway", "--config", "/etc/crab/s3-gateway.toml", "--healthcheck"]

ENTRYPOINT ["/usr/local/bin/crab-s3-gateway"]
CMD ["--config", "/etc/crab/s3-gateway.toml"]
