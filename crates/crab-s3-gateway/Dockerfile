FROM rust:1.90-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --locked --release -p crab-s3-gateway

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 crab
COPY --from=builder /src/target/release/crab-s3-gateway /usr/local/bin/crab-s3-gateway
USER crab
EXPOSE 8080
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/crab-s3-gateway"]
CMD ["--config", "/etc/crab/s3-gateway.toml"]
