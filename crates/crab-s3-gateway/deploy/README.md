# Docker Compose qualification

This stack is an isolated local smoke environment, not a production credential
or storage configuration. It defines two independently runnable packaged
gateway instances with non-root users, read-only root filesystems, dropped
capabilities, bounded scratch, private management ports, process-bounded
persistent gateway caches, and a persistent RustFS data volume. The caches are
disposable and survive gateway-container replacement; they are never
authoritative repository state.

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

To exercise the two-instance path locally, start `gateway-standby` alongside
`gateway`, use `http://127.0.0.1:18082` and its management listener at
`127.0.0.1:18083`, and continue an upload through either endpoint. Each
instance has its own disposable cache; RustFS-backed Crab metadata and staged
multipart parts are the shared recovery boundary.

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

The checked CI qualification starts both gateway instances, creates a multipart
session through the primary, uploads a valid non-final part, force-recreates the
primary while the standby remains live, verifies the standby can list the
durable part, uploads the final part and completes the object through the
standby, then reads it through both instances and compares every assembled byte.
RustFS retains the shared upload catalog and repository data; each gateway's
scratch filesystem remains disposable. The `gateway-cache` and
`gateway-cache-standby` volumes may warm their respective replacement
processes but are not needed for correctness or recovery.
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
The pinned Boto3 client additionally uploads a deterministic 512 MiB object as
eight sequential 64 MiB parts, reads the complete object back, and reads a
range crossing a persisted part boundary. Both reads are compared with the
local SHA-256 fixture, and upload/full-read/range-read timings are retained
without credentials or object names.
The same packaged-image job runs an identity-free ecosystem matrix against the
gateway: AWS CLI and Boto3 (including SigV4 presigning), DuckDB CLI and Python,
LanceDB, Spark S3A, PyArrow, Pandas, Polars, fsspec/s3fs, Dask, MinIO,
smart_open, awswrangler, PyIceberg, Delta Lake, Java AWS SDK v2, Go AWS SDK v2,
and s3cmd. Each fixture performs the client's normal object, dataframe, or
table-format write/read path and checks exact bytes, rows, aggregates, ranges,
listings, snapshots, or transaction versions as appropriate. Dependency
versions and per-client timings are retained in
`crab.s3-gateway-ecosystem` evidence.
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
`gateway-cache` and `gateway-cache-standby` volumes:

```sh
docker compose -f crates/crab-s3-gateway/deploy/compose.yaml down --volumes
```

Never use `down --volumes` for a production deployment or a stack whose RustFS
volume contains data that must be retained.

## Sustained-write qualification runner

The repository includes a dependency-free signed-request workload runner for a
dedicated gateway, with an optional direct S3-compatible baseline. It sends
deterministic 4 KiB `PutObject` requests from 16 concurrent writers, paginates
the resulting keys, and records p50/p95 latency, throughput, failed requests,
duplicate listing entries, missing acknowledged keys, and unexpected keys.
It also retains complete time windows and compares the first and terminal
cohorts so an acceptable aggregate cannot hide progressive degradation.
Credentials are read only from the environment; they are never command-line
arguments or written to the report:

```sh
export S3_GATEWAY_WORKLOAD_ACCESS_KEY=...
export S3_GATEWAY_WORKLOAD_SECRET_KEY=...
python3 -B crab/scripts/e2e/s3_gateway_workload.py \
  --gateway-endpoint https://gateway.example.invalid \
  --gateway-bucket logical-repository \
  --duration-seconds 900 \
  --window-seconds 60 \
  --report <external-workspace>/s3-gateway-workload.json
```

Use an isolated prefix and bucket for every run. Add `--baseline-endpoint` and
`--baseline-bucket` plus the `S3_BASELINE_WORKLOAD_*` credentials when a direct
transport comparison is useful. That baseline is not a substitute for direct
Crab SDK committed-write evidence. The report is identity-free and the command
returns nonzero on any failed request, lost acknowledged key, unexpected key,
duplicate listing entry, terminal throughput below 80% of the startup cohort,
or terminal p95 latency above 150% of startup. With a baseline it additionally
requires at least 90% of baseline throughput and no more than 125% of its p95.

## Ecosystem qualification runner

Install the pinned Python dependencies into an isolated environment, then run
the full client matrix against a dedicated gateway:

```sh
python3 -m venv <external-workspace>/s3-gateway-ecosystem-venv
<external-workspace>/s3-gateway-ecosystem-venv/bin/pip install \
  --requirement crab/scripts/e2e/s3_gateway_ecosystem_requirements.txt
export PATH="<duckdb-cli-directory>:<external-workspace>/s3-gateway-ecosystem-venv/bin:$PATH"
export S3_GATEWAY_ECOSYSTEM_ACCESS_KEY=...
export S3_GATEWAY_ECOSYSTEM_SECRET_KEY=...
python3 -B crab/scripts/e2e/s3_gateway_ecosystem.py \
  --endpoint https://gateway.example.invalid \
  --bucket logical-repository \
  --work-dir <external-workspace>/s3-gateway-ecosystem-work \
  --report <external-workspace>/s3-gateway-ecosystem.json
```

The full matrix also requires the AWS CLI, DuckDB CLI, s3cmd, Maven with Java
17 or newer, and Go. The runner validates required commands before traffic,
uses a random per-run prefix, suppresses child-process output on failure, and
stores no endpoint, repository, prefix, or credential identity in its report.
