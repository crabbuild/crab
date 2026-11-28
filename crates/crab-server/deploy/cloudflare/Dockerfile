FROM crab-server:cloudflare-base
COPY Caddyfile /etc/crab/Caddyfile
COPY --chmod=0755 entrypoint.sh /usr/local/bin/cloudflare-entrypoint
ENV XDG_CONFIG_HOME=/var/lib/crab/caddy/config XDG_DATA_HOME=/var/lib/crab/caddy/data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/cloudflare-entrypoint"]
CMD []
