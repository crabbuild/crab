#!/bin/sh
set -eu

# Explicit configuration remains the owner of managed, multi-repository setups.
if [ "$#" -gt 0 ]; then
    exec /usr/local/bin/crab-s3-gateway "$@"
fi

AWS_DEFAULT_REGION=$(/usr/local/bin/crab-s3-gateway --setup)
export AWS_DEFAULT_REGION
cp /var/lib/crab/gateway/gateway.toml /etc/crab/s3-gateway.toml
/usr/local/bin/crab-s3-gateway --config /etc/crab/s3-gateway.toml --initialize
exec /usr/local/bin/crab-s3-gateway --config /etc/crab/s3-gateway.toml
