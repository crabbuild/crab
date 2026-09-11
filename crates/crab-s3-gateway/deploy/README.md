# Docker Compose qualification

This stack is an isolated local smoke environment, not a production credential
or storage configuration. It runs the packaged gateway with a non-root user,
read-only root filesystem, dropped capabilities, bounded scratch, private
management port, a process-bounded persistent gateway cache, and a persistent
RustFS data volume. The cache is disposable and survives gateway-container
replacement; it is never authoritative repository state.

The checked ECS Fargate deployment profile is in [`ecs/`](ecs/), with the
CloudFormation template, parameter example, static lint contract, and live-
qualification boundary documented in [`ecs/README.md`](ecs/README.md).

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

Verify the private Prometheus endpoint after generating traffic:

```sh
curl --fail --silent --show-error http://127.0.0.1:18081/metrics
```

The endpoint contains only fixed method, outcome, and admission-class labels.
Treat a repository name, ref, key, upload ID, principal, access key, or secret in
that response as a security defect.

`crab_s3_gateway_cache_limit_bytes` must match `[cache].max_bytes`. The image
fails startup if the configured cache root cannot privately create, publish,
sync, and remove a probe file. Keep the cache volume separate from
`/var/lib/crab/tmp`; the former is app-evicted reusable data while the latter is
capacity-reserved live request state.

The canonical alert rules and their trigger, diagnosis, safe-action, and
recovery-proof procedures live in the [operations runbook](operations.md).
Keep that runbook and the PrometheusRule from the same source revision.
Provider-internal retries still require provider or load-balancer telemetry
because gateway metrics count complete logical object-store operations.

The checked CI qualification also creates a multipart session, uploads a valid
non-final part, force-recreates the gateway container, verifies the replacement
process can list the durable part, uploads the final part, completes the object,
and compares every assembled byte. RustFS retains the shared upload catalog and
repository data; the gateway's scratch filesystem remains disposable. The
named `gateway-cache` volume may warm the replacement process but is not needed
for correctness or recovery.
It also sends a multi-chunk HMAC-signed SigV4 `PutObject`, verifies the decoded
bytes and S3-compatible `Content-Encoding` metadata, then corrupts a chunk
signature and proves no object was published.
The packaged-image qualification additionally uses an unchanged AWS CLI with a
configured temporary access-key/secret/session-token triple. Header-signed and
presigned-query requests succeed, while a missing or wrong token returns
`InvalidToken`; log and metric scans include the session credential material.
It then validates and retains a 90-day `crab.s3-gateway-evidence` report
bound to the exact source and image digest. The report includes the fixed check
inventory, backend and client versions, fixture digest, request/byte/latency
measurements, container resident working-set memory, disk usage, and terminal
state, but no endpoint, repository, object, or credential identity. A 10,032-key
deduplicated Xet namespace must traverse in eleven 1,000-key pages within two
minutes and 125% of an equivalent direct RustFS traversal, remain byte-ordered
and duplicate-free, resume a late prefix exactly, collapse delimiter subtrees,
and write no Xet reconstruction scratch.
It also registers one 8 MiB and one 64 MiB multipart part, requires each durable
state record to stay within 64 KiB and their sizes to differ by at most 64 bytes,
then aborts both uploads and proves their staged payloads are gone.
Finally, it kills the gateway while RustFS holds one live part, one frozen part,
and one eligible synthetic missing-session orphan. The replacement must remove
the orphan and release its capacity within two completed maintenance scans while
leaving both protected parts intact; explicit teardown must then reclaim those
fixture payloads too.

Stop containers without deleting repository data:

```sh
docker compose -f crates/crab-s3-gateway/deploy/compose.yaml down
```

Only the explicit isolated-smoke teardown removes the RustFS and disposable
gateway-cache volumes:

```sh
docker compose -f crates/crab-s3-gateway/deploy/compose.yaml down --volumes
```

Never use `down --volumes` for a production deployment or a stack whose RustFS
volume contains data that must be retained.

## Sustained-write qualification runner

The repository includes a dependency-free signed-request workload runner for a
dedicated gateway and direct S3-compatible baseline. It sends deterministic
4 KiB `PutObject` requests from 16 concurrent writers, paginates the resulting
keys, and records p50/p95 latency, throughput, failed requests, duplicate
listing entries, missing acknowledged keys, and unexpected keys. Credentials
are read only from the environment; they are never command-line arguments or
written to the report:

```sh
export S3_GATEWAY_WORKLOAD_ACCESS_KEY=...
export S3_GATEWAY_WORKLOAD_SECRET_KEY=...
export S3_BASELINE_WORKLOAD_ACCESS_KEY=...
export S3_BASELINE_WORKLOAD_SECRET_KEY=...
python3 -B crab/scripts/e2e/s3_gateway_workload.py \
  --gateway-endpoint https://gateway.example.invalid \
  --gateway-bucket logical-repository \
  --baseline-endpoint https://s3.example.invalid \
  --baseline-bucket isolated-baseline \
  --duration-seconds 300 \
  --report <external-workspace>/s3-gateway-workload.json
```

Use an isolated prefix and bucket for every run. The direct endpoint is a
transport/SDK baseline; it is not a substitute for a direct Crab SDK
committed-write baseline, so this runner does not by itself close the Phase-8
committed-write performance gate. The report is identity-free and the command
returns nonzero on any failed request, lost acknowledged key, unexpected key,
duplicate listing entry, throughput regression below 90% of baseline, or p95
latency above 125% of baseline.
