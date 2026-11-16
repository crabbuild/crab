#!/bin/bash
set -euo pipefail

if [ "$#" -gt 0 ]; then
    exec /usr/local/bin/crab-server "$@"
fi

/usr/local/bin/crab-server --container-setup
cp /var/lib/crab/server/server.toml /etc/crab/server.toml
mkdir -p /var/lib/crab/cells /var/lib/crab/tmp

children=()
cleanup() {
    trap - EXIT TERM INT
    if [ "${#children[@]}" -gt 0 ]; then
        kill -TERM "${children[@]}" 2>/dev/null || true
        wait "${children[@]}" 2>/dev/null || true
    fi
}
trap cleanup EXIT
trap 'cleanup; exit 0' TERM INT

/usr/local/bin/crab-server --config /etc/crab/server.toml serve &
children+=("$!")
ready=false
for ((attempt = 0; attempt < 120; attempt++)); do
    if /usr/local/bin/crab-server --config /etc/crab/server.toml healthcheck >/dev/null 2>&1; then
        ready=true
        break
    fi
    if ! kill -0 "${children[0]}" 2>/dev/null; then
        wait "${children[0]}"
        exit 1
    fi
    sleep 1
done
if [ "$ready" != true ]; then
    echo 'Server startup did not become ready.' >&2
    exit 1
fi

# Directory preparation is an explicit private command before browser admission.
# A missing or outdated schema must fail setup rather than yield a partial UI.
/usr/local/bin/crab-server --config /etc/crab/server.toml cells initialize-directory >/dev/null
/usr/local/bin/caddy run --config /etc/crab/container.Caddyfile --adapter caddyfile &
children+=("$!")
wait -n "${children[@]}"
