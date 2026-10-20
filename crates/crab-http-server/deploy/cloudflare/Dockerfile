FROM caddy:2.10.2-alpine@sha256:4c6e91c6ed0e2fa03efd5b44747b625fec79bc9cd06ac5235a779726618e530d AS proxy
FROM crab-http-server:cloudflare-base
COPY --from=proxy /usr/bin/caddy /usr/local/bin/caddy
COPY Caddyfile /etc/crab/Caddyfile
COPY --chmod=0755 entrypoint.sh /usr/local/bin/cloudflare-entrypoint
ENV XDG_CONFIG_HOME=/var/lib/crab/caddy/config XDG_DATA_HOME=/var/lib/crab/caddy/data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/cloudflare-entrypoint"]
CMD []
