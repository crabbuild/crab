FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl libpcre2-8-0=10.42-1+deb12u1 \
    && rm -rf /var/lib/apt/lists/* \
    && printf 'crab:x:10001:\n' >> /etc/group \
    && printf 'crab:x:10001:10001:Crab Cell example:/var/lib/crab:/bin/false\n' >> /etc/passwd \
    && install -d -o 10001 -g 10001 /var/lib/crab/cells

COPY compose_kv_service /usr/local/bin/compose_kv_service
RUN chmod 0755 /usr/local/bin/compose_kv_service

USER crab:crab
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/compose_kv_service"]
