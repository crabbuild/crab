# Docker Compose qualification

This stack is an isolated local smoke environment, not a production credential
or storage configuration. It runs the packaged gateway with a non-root user,
read-only root filesystem, dropped capabilities, bounded scratch, private
management port, and a persistent RustFS data volume.

From the repository root, choose an untracked working directory and create the
synthetic gateway secret with mode `0600`:

```sh
mkdir -p /path/to/crab-s3-gateway-smoke
umask 077
printf '%s' 'gateway-qualification-secret' > /path/to/crab-s3-gateway-smoke/gateway-secret
sudo chown 10001:10001 /path/to/crab-s3-gateway-smoke/gateway-secret
chmod 0600 /path/to/crab-s3-gateway-smoke/gateway-secret
```

Set the smoke environment. All credentials below are synthetic and valid only
for the isolated stack:

```sh
export RUSTFS_ACCESS_KEY=crab
export RUSTFS_SECRET_KEY=crab
export CRAB_S3_GATEWAY_IMAGE=crab-s3-gateway:local
export CRAB_S3_GATEWAY_CONFIG="$PWD/crates/crab-s3-gateway/deploy/compose.gateway.toml"
export CRAB_S3_GATEWAY_SECRET_FILE=/path/to/crab-s3-gateway-smoke/gateway-secret
```

Build and validate the exact assets, then start RustFS:

```sh
docker build -f crates/crab-s3-gateway/deploy/Dockerfile -t "$CRAB_S3_GATEWAY_IMAGE" .
docker compose -f crates/crab-s3-gateway/deploy/compose.yaml config --quiet
docker compose -f crates/crab-s3-gateway/deploy/compose.yaml up -d rustfs
```

Create only the isolated physical bucket, initialize the empty Crab prefix, and
start the gateway:

```sh
AWS_ACCESS_KEY_ID="$RUSTFS_ACCESS_KEY" \
AWS_SECRET_ACCESS_KEY="$RUSTFS_SECRET_KEY" \
AWS_DEFAULT_REGION=us-east-1 \
aws --endpoint-url http://127.0.0.1:19000 s3api create-bucket \
  --bucket crab-s3-gateway-qualification
docker compose -f crates/crab-s3-gateway/deploy/compose.yaml \
  --profile initialize run --rm gateway-init
docker compose -f crates/crab-s3-gateway/deploy/compose.yaml up -d gateway
docker compose -f crates/crab-s3-gateway/deploy/compose.yaml exec gateway \
  crab-s3-gateway --config /etc/crab/s3-gateway.toml --readiness-check
```

Use any unchanged S3 client at `http://127.0.0.1:18080` with access key
`gateway-qualification`, secret `gateway-qualification-secret`, region
`us-east-1`, and path-style addressing. The management listener is bound only
to `127.0.0.1:18081`.

The checked CI qualification also creates a multipart session, uploads a valid
non-final part, force-recreates the gateway container, verifies the replacement
process can list the durable part, uploads the final part, completes the object,
and compares every assembled byte. RustFS retains the shared upload catalog and
repository data; the gateway's scratch filesystem remains disposable.

Stop containers without deleting repository data:

```sh
docker compose -f crates/crab-s3-gateway/deploy/compose.yaml down
```

Only the explicit isolated-smoke teardown removes the RustFS volume:

```sh
docker compose -f crates/crab-s3-gateway/deploy/compose.yaml down --volumes
```

Never use `down --volumes` for a production deployment or a stack whose RustFS
volume contains data that must be retained.
