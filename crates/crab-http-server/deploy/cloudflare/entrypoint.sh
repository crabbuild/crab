#!/bin/sh
set -eu
umask 077
printf '%s' "$CRAB_CONFIG_B64" | base64 -d > /etc/crab/server.toml
printf '%s' "$CRAB_CA_B64" | base64 -d > /etc/crab/ca.crt
printf '%s' "$CRAB_CERT_B64" | base64 -d > /etc/crab/peer.crt
printf '%s' "$CRAB_KEY_B64" | base64 -d > /etc/crab/peer.key
unset CRAB_CONFIG_B64 CRAB_CA_B64 CRAB_CERT_B64 CRAB_KEY_B64
crab-http-server --config /etc/crab/server.toml cells capacity --json
caddy start --config /etc/crab/Caddyfile --adapter caddyfile
exec crab-http-server --config /etc/crab/server.toml serve
