# Crab S3 gateway

`crab-s3-gateway` exposes configured [Crab](https://crab.build)
repositories through the S3 REST API. Applications keep using their existing
S3 operations and configure the gateway endpoint, region, and credentials.

```text
S3 client  ->  crab-s3-gateway  ->  Crab repository  ->  S3, GCS, or Azure
                  |                       |
             S3 auth and API        Git history and
             compatibility          Xet deduplication
```

Each logical S3 bucket maps to one configured Crab repository. An object key
selects a Git ref and a repository path:

```text
s3://<logical-repository>/<ref>/<path>
s3://analytics/main/tables/events.parquet
```

Reads can address branches, tags, or complete commit IDs. Writes publish a Git
commit immediately and are accepted only on writable branches.

## When to use it

Use the gateway when an application already speaks S3 but the data should live
in a versioned, deduplicated Crab repository. The gateway is designed for data
tools, SDKs, and services that need ordinary object operations, listings, byte
ranges, checksums, conditional writes, or multipart uploads.

The gateway is not an AWS control-plane emulator. It intentionally does not
create buckets, manage IAM or bucket policies, issue STS credentials, or expose
AWS bucket version IDs. Git history remains the version model. See the
[protocol contract](../../crab/docs/architecture/s3-gateway-contract.md) for
the exact supported and excluded surface.

## Quick start with Docker Compose

The checked Compose stack runs two gateway instances against an isolated
RustFS backend. It is a development and qualification environment, not a
production configuration.

From the repository root:

```sh
mkdir -p /path/to/crab-s3-gateway-smoke
umask 077
printf '%s' 'gateway-qualification-secret' \
  > /path/to/crab-s3-gateway-smoke/gateway-secret
sudo chown 10001:10001 /path/to/crab-s3-gateway-smoke/gateway-secret
chmod 0600 /path/to/crab-s3-gateway-smoke/gateway-secret

export RUSTFS_ACCESS_KEY=crab
export RUSTFS_SECRET_KEY=crab
export CRAB_S3_GATEWAY_IMAGE=crab-s3-gateway:local
export CRAB_S3_GATEWAY_CONFIG="$PWD/crates/crab-s3-gateway/deploy/compose.gateway.toml"
export CRAB_S3_GATEWAY_SECRET_FILE=/path/to/crab-s3-gateway-smoke/gateway-secret

docker build \
  -f crates/crab-s3-gateway/deploy/Dockerfile \
  -t "$CRAB_S3_GATEWAY_IMAGE" .
docker compose -f crates/crab-s3-gateway/deploy/compose.yaml up -d rustfs
```

Create the isolated physical bucket, initialize the Crab repository, and start
the gateway:

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

The S3 endpoint is now `http://127.0.0.1:18080`. Use logical bucket
`gateway-repository`, region `us-east-1`, access key `gateway-qualification`,
secret `gateway-qualification-secret`, and path-style addressing.

```sh
AWS_ACCESS_KEY_ID=gateway-qualification \
AWS_SECRET_ACCESS_KEY=gateway-qualification-secret \
AWS_DEFAULT_REGION=us-east-1 \
aws --endpoint-url http://127.0.0.1:18080 \
  s3 cp ./example.parquet s3://gateway-repository/main/data/example.parquet
```

The [Compose qualification guide](deploy/README.md) covers the second gateway,
multipart recovery, metrics, retained evidence, workload tests, and safe
teardown.

## Configure a client

Configure clients with four values:

- endpoint: the gateway's S3 listener
- region: the gateway's configured `region`
- access key and secret: a gateway credential, not backend cloud credentials
- addressing style: path-style, unless `endpoint_domain` and wildcard DNS/TLS
  are configured for virtual-hosted requests

For example, Boto3 needs no Crab-specific adapter:

```python
import boto3

s3 = boto3.client(
    "s3",
    endpoint_url="http://127.0.0.1:18080",
    region_name="us-east-1",
    aws_access_key_id="gateway-qualification",
    aws_secret_access_key="gateway-qualification-secret",
)

s3.put_object(
    Bucket="gateway-repository",
    Key="main/data/example.json",
    Body=b'{"ready":true}\n',
    ContentType="application/json",
)

response = s3.get_object(
    Bucket="gateway-repository",
    Key="main/data/example.json",
    Range="bytes=0-14",
)
print(response["Body"].read())
```

Temporary SigV4 credentials also require the configured session token. The
gateway accepts SigV4 headers, presigned SigV4 queries, signed streaming
uploads, and legacy SigV2 headers and queries. It does not support SigV4a or
issue/refresh STS credentials.

## Namespace and Git behavior

One configured repository is one logical bucket. Its backing provider bucket
and prefix are never returned to clients.

The first key component selects a ref:

| Key | Meaning | Writable |
| --- | --- | --- |
| `main/path/file` | Branch `refs/heads/main` | Yes, unless protected |
| `feature%2Fdata/path/file` | Branch `refs/heads/feature/data` | Yes, unless protected |
| `refs%2Ftags%2Fv1/path/file` | Tag `refs/tags/v1` | No |
| `<40-hex-commit>/path/file` | Exact Git commit | No |

An empty listing prefix returns the authorized branch prefixes. Reads pin one
commit for a consistent view. Each successful state-changing PUT, COPY, single
DELETE, or completed multipart upload owns one commit. Compatible same-branch
requests wait up to 10 ms for a bounded batch of at most 32 requests or 32 MiB
of Git payload. Their conditions are evaluated in FIFO order, their commits form
one parent chain, and their Git objects share one pack and ref-journal
publication. Trusted generated objects enter one bounded decoded spool before
canonical pack and sidecar preparation; they are not compressed and reinflated
as synthetic wire input. A single larger request runs alone. Multipart completion also runs
alone so its durable exactly-once receipt cannot be coupled to another request.
Once drained, a gateway-owned worker retains the batch even if an originating
HTTP connection closes; one disconnected client cannot cancel peer mutations.
Multi-delete retains one result and commit per successful entry; grouping never
makes the whole request atomic. A process-local, 128 MiB warm-state budget may
retain the exact-tip Git directories and S3 attribute manifest between batches.
The cached parent is revalidated while holding the object-store ref lease before
publishing a commit or returning a prepared no-op or precondition result. It is
discarded on a tip mismatch, restart, or memory-pressure
eviction; idle branch state remains reusable below the shared watermark. Object
storage remains the authority and a cold request reconstructs the same state from
immutable repository objects. Unborn branches skip catalog access and recheck
absence under their ref lease before publication. The first cold
publication and every 64 subsequent publication batches write a commit-identified
attribute checkpoint into one bounded object-store slot per branch while holding
the same ref lease. The final commit delta records the slot; a missing or
superseded slot falls back to the immutable delta chain. Checkpoint markers remain
optional in the existing version-2 delta format. If the full manifest exceeds the
existing 32 MiB manifest bound, publication keeps the delta chain authoritative
and skips the checkpoint, so the optimization cannot reject an otherwise valid
mutation. When the delta ancestry proves that the branch began empty and every
commit came from the gateway, the checkpoint is also a complete sorted S3
namespace index. `ListObjects` binds it to the current durable ref and pages it
with only the newer per-commit deltas since the last successful checkpoint,
without opening SlateDB or walking Git trees. A missing delta, legacy checkpoint,
or Git-authored ancestor keeps canonical Git tree listing, so the accelerator
cannot hide Git-written objects. SlateDB is not required by this write path; its
object catalog remains
a rebuildable derived index. During sustained writes, one background worker every
64 local publication epochs folds the active ref journal into the object-store
manifest and advances catalog coverage under the generation-owner and GC-writer
fences. Read views use the newest catalog whose immutable pack inventory is a
proven subset of their snapshot, then inspect only the remaining pack tail. This
also covers the interval between manifest compaction and matching catalog
publication without scanning every historical pack. Full commit-graph
maintenance still waits for a five-second quiet window.

Keys must be valid UTF-8 paths that Git trees can represent without loss. The
gateway rejects ambiguous or unsafe components such as empty segments, `.`,
`..`, and `.git`. Empty trailing-slash PUT and DELETE requests are accepted as
virtual directory hints but are not stored as marker objects.

## S3 compatibility

The supported data-plane surface includes:

| Area | Operations and behavior |
| --- | --- |
| Bucket discovery | `ListBuckets`, `HeadBucket`, `GetBucketLocation`, `GetBucketVersioning` |
| Objects | `GetObject`, `HeadObject`, `PutObject`, `DeleteObject`, `CopyObject`, `GetObjectAttributes` |
| Listings | `ListObjects`, `ListObjectsV2`, delimiter and continuation semantics |
| Metadata | user metadata, standard content headers, tagging, ETags, modeled checksums |
| Conditions | read conditions; strong `If-Match`; `If-None-Match: *` where documented |
| Ranges | one open, closed, or suffix byte range; multipart part-number reads |
| Batch delete | `DeleteObjects`, including quiet mode and per-key results |
| Multipart | create, upload, upload-copy, list, abort, and complete with durable recovery |

Single PUTs and multipart parts may be as large as 5 GiB. Completed multipart
objects may be as large as 50 TB, subject to the backing provider's own object
and multipart limits. Checksums are validated before publication. Unsupported
modeled fields fail explicitly rather than being silently ignored.

Notable exclusions include bucket creation/deletion, ACLs, bucket policies,
AWS version IDs, browser POST policies, Select, object lock, retention,
replication, notifications, and server-side-encryption request headers. The
[protocol contract](../../crab/docs/architecture/s3-gateway-contract.md) is the
canonical compatibility reference; this README is only an overview.

## Large objects, range reads, and deduplication

Large uploads use bounded temporary spools and Crab's verified LFS path.
`git_blob_max_bytes` is configured per repository, defaults to 1 MiB, and
accepts values through 64 MiB. Objects at or below that limit remain ordinary
Git blobs; larger PUTs and copies use LFS. Multipart completion does not
assemble objects above the configured limit into a second full-size local file;
it validates and replays durable parts directly into LFS. Configure the same
limit on every gateway instance serving a repository so writers use one
representation policy.

Objects already represented by Crab/Xet keep their deduplication. A byte-range
GET reconstructs only overlapping Xet chunks and streams the selected bytes
through bounded backpressure. It does not hydrate the complete logical file or
create response-sized scratch. Complete GETs retain whole-file verification.

The process uses a required local read cache shared by its configured
repositories. Put the cache on a private, capacity-limited volume separate from
request scratch. Cache contents are disposable; Crab repository state remains
authoritative.

## Configuration

Start from [`deploy/gateway.example.toml`](deploy/gateway.example.toml). A
minimal configuration defines:

- separate S3 and private management listeners
- a region and per-process request budget
- an absolute cache directory and retention limit
- access keys mapped to Crab principals, with secrets read from protected files
- logical repositories, backing placements, member access, protected branches,
  the Git blob limit, and durable multipart limits

```toml
listen = "0.0.0.0:8080"
management_listen = "0.0.0.0:8081"
region = "us-east-1"
max_in_flight_requests = 32

[cache]
directory = "/var/lib/crab/cache-volume/cache"
max_bytes = 2147483648

[[credentials]]
access_key = "issued-access-key"
secret_key_file = "/run/secrets/crab-s3-secret"
principal = "service-account:analytics"

[[repositories]]
name = "analytics"
provider = "s3"
bucket = "physical-storage-bucket"
prefix = "repositories/analytics"
default_branch = "main"
protected_branches = ["release"]
git_blob_max_bytes = 1048576

[[repositories.members]]
principal = "service-account:analytics"
access = "write"
```

Backend access uses Crab's provider credential chain. For S3-compatible
backends, the shared storage layer recognizes `AWS_ENDPOINT_URL_S3`,
`AWS_ALLOW_HTTP`, and `AWS_VIRTUAL_HOSTED_STYLE_REQUEST`. These backend
credentials are independent of client-facing gateway credentials.

Secret files must be private to the process owner or effective group. Never put
secrets on the command line or in the TOML file. Set `endpoint_domain` only
when deployment DNS and TLS cover its wildcard hosts.

Every gateway instance serving the same repository must use an identical Git
blob limit and identical multipart session, byte-budget, and expiry settings.
Run one continuously supervised `crab metadb owner` worker per backing
repository for catalog, visibility, commit-graph, and geometric repack
maintenance.

## Initialize and run

Build from the repository root. Keep Cargo artifacts on the mounted workspace
volume:

```sh
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-s3-gateway \
  cargo build --release -p crab-s3-gateway --locked

/Volumes/Workspace/crabbuild-target/crab-s3-gateway/release/crab-s3-gateway \
  --config /etc/crab/s3-gateway.toml --initialize
/Volumes/Workspace/crabbuild-target/crab-s3-gateway/release/crab-s3-gateway \
  --config /etc/crab/s3-gateway.toml
```

`--initialize` creates missing canonical Crab metadata only for empty configured
prefixes, then exits. It is idempotent. Normal serving never initializes,
migrates, or converts repository storage.

SIGTERM and SIGINT trigger graceful connection draining.

## Health and observability

The management listener is intentionally separate and unauthenticated. Keep it
private.

| Endpoint or command | Purpose |
| --- | --- |
| `GET /livez` or `--healthcheck` | Process can accept requests |
| `GET /readyz` or `--readiness-check` | Every configured repository has a fresh, safe read view |
| `GET /metrics` | Prometheus request, admission, mutation-batch, multipart, backend, cache, and scratch metrics |

Readiness returns `503 Service Unavailable` with `Retry-After: 5` when any
repository cannot be served safely. Metric labels never contain repository
names, refs, keys, upload IDs, principals, access keys, or secrets.

Requests use bounded control, read, and transfer admission pools. A saturated
pool waits for bounded capacity, then returns S3 `SlowDown` with
`Retry-After: 1`. Standard S3 SDK retry policies handle this response; custom
clients should use exponential backoff with jitter. Scratch capacity is also
reserved before work begins so overload fails predictably instead of filling
the filesystem.

For write-efficiency diagnosis, compare
`crab_s3_gateway_mutation_batch_requests_total` and
`crab_s3_gateway_mutation_batch_commits_total` with
`crab_s3_gateway_mutation_batches_total`. The first ratio is requests drained
per local execution and the second is commits amortized over each durable pack.
`crab_s3_gateway_mutation_queue_wait_seconds` and
`crab_s3_gateway_mutation_batch_duration_seconds` separate collection/admission
delay from object-store publication latency. These metrics use no repository,
ref, key, principal, or credential labels.

Operational alerts, capacity guidance, credential rotation, maintenance,
backup/restore, upgrades, and rollback are documented in the
[operations runbook](deploy/operations.md).

## Deployment

Build the checked container from the repository root:

```sh
docker build -f crates/crab-s3-gateway/deploy/Dockerfile -t crab-s3-gateway .
```

Production deployments should terminate TLS at a trusted ingress, load
balancer, or service mesh. Expose the S3 listener only through that ingress and
the management listener only to the workload's probe network. Run the container
as its non-root user with a read-only root filesystem, a bounded scratch mount,
a separate bounded cache mount, and read-only credential files.

Deployment assets are available for:

- [Docker Compose](deploy/README.md)
- [Kubernetes and EKS](deploy/helm/crab-s3-gateway/README.md)
- [ECS Fargate](deploy/ecs/README.md)

Static validation of an artifact is not evidence of a live cloud deployment.
The deployment guides state their current qualification boundary.

## Development and qualification

Run focused crate checks from the repository root, with a target directory
dedicated to this checkout:

```sh
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-s3-gateway \
  cargo test -p crab-s3-gateway --locked
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-s3-gateway \
  cargo clippy -p crab-s3-gateway --all-targets --locked -- -D warnings
cargo fmt --check -p crab-s3-gateway
```

The packaged-image qualification exercises signed AWS CLI and Boto3 requests,
checksums, streaming uploads, multipart restart and multi-instance recovery,
large-object full and range reads, Xet deduplication, bounded listing, and
cleanup. It also runs write/read fixtures through DuckDB, LanceDB, Spark S3A,
Arrow and dataframe libraries, table formats, alternative S3 clients, and the
Java and Go AWS SDKs. The on-demand workflow can also run the time-window
degradation gate for fifteen, thirty, or sixty minutes. The accepted release
matrix and the distinction between checked-in, local, and live-cloud evidence
are defined in the protocol contract.

## Further reading

- [Protocol contract](../../crab/docs/architecture/s3-gateway-contract.md) —
  canonical S3 behavior and exclusions
- [Architecture and implementation record](../../crab/docs/architecture/crab-s3-gateway.md) —
  ownership, data flow, and phased qualification
- [Compose qualification](deploy/README.md) — local packaged-image smoke and
  retained evidence
- [Operations runbook](deploy/operations.md) — production alerts and procedures
- [Example configuration](deploy/gateway.example.toml) — complete annotated
  configuration
- [Boto3 qualification client](../../crab/scripts/e2e/s3_gateway_boto3.py) —
  executable official-SDK coverage
- [Ecosystem qualification client](../../crab/scripts/e2e/s3_gateway_ecosystem.py) —
  executable data-tool, table-format, CLI, and SDK interoperability coverage
- [Sustained-write qualification](../../crab/scripts/e2e/s3_gateway_workload.py) —
  signed concurrent writes with integrity and time-window degradation gates
