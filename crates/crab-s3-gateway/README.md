# Crab S3 gateway

`crab-s3-gateway` presents configured Crab repositories as S3 buckets. Existing
S3 clients use their normal endpoint, region, access-key, and secret-key
configuration. Object keys use `REF/path`, for example
`s3://my-repository/main/data/model.bin`.

The gateway accepts S3 SigV4 header signing and presigned-query URLs, plus
legacy SigV2 header and presigned-query authentication, through `s3s`. It maps
each access key to a Crab principal and authorizes that principal against the
logical repository catalog. Gateway credentials may be long-lived or configured
temporary SigV4 credential triples. Temporary credentials require a protected
session-token file and an RFC 3339 expiry; missing, wrong, unsigned, duplicate,
or expired tokens fail before repository authorization. The gateway does not
issue or refresh STS credentials and does not support SigV4a. Client credentials
remain independent of the cloud credentials used for the backing object store.
HMAC-signed SigV4 streaming requests verify every chained chunk before
publication. Their `aws-chunked` transport encoding is removed from stored
object metadata, matching S3; any accompanying application encoding remains.

The client-compatible surface includes bucket listing/head/location/versioning
discovery, object GET/HEAD/PUT/DELETE/COPY/attributes/tagging, V1/V2 object
listing, multi-delete, and durable multipart create/upload/copy/list/abort/complete.
GET/HEAD support conditions, checksum mode, and a single byte range. PUT and
multipart completion support atomic `If-Match` and `If-None-Match: *` writes.
Content-MD5 and modeled S3 checksum headers are validated and persisted;
multipart full-object and composite checksum profiles are returned by object
and part inspection APIs. Path-style addressing is always available. Set
`endpoint_domain` to also accept virtual-hosted requests. Single PUTs and
multipart parts support up to 5 GiB, and multipart completion supports S3's
50 TB object limit. Explicit `STANDARD` storage-class hints and the
`RestoreStatus` listing hint are accepted. Empty trailing-slash PUT and DELETE
requests are treated as validated virtual directory hints for filesystem
clients; they do not create marker blobs because Git trees represent
directories.
Each UploadPart transfer uses a unique immutable backend object. A successful
replacement reclaims the prior unreferenced payload, while a registration that
loses to Abort or completion reclaims only its own payload. This prevents
same-part concurrency from deleting the winning bytes or accumulating every
replaced version.
Each repository has a bounded durable slot catalog shared by every gateway
instance. `max_active_multipart_uploads` caps non-terminal sessions,
`multipart_staging_bytes_per_upload` independently caps registered part bytes
and the combined bytes of reserved in-flight transfers plus retired replacement
payloads awaiting deletion, and
`multipart_upload_ttl_seconds` persists the Open-session expiry chosen when the
upload starts. The defaults are 1,024 sessions, S3's 50 TB object ceiling, and
seven days. Every instance serving the same repository must use the same three
values. During a same-number replacement burst, physical temporary storage can
therefore reach twice the configured per-upload budget, but a process crash
cannot create unaccounted payloads. Transfers stop at the persisted session
expiry. A once-per-minute reconciler aborts expired Open sessions, retries terminal
cleanup, reclaims stalled transfer reservations after a ten-minute provider-drain
grace period, and reclaims expired slots whose process died before writing the
session record. A terminal session retains its distributed slot until every
reserved transfer is cleaned, preventing a late backend write from escaping
quota accounting. For a frozen Completing session, the reconciler reads only
the current object's Git-bound attributes or resolves its deterministic
ref-journal publication plan,
then closes the session only when durable evidence proves that publication
succeeded. Plan evidence remains valid after a later write replaces the object.
A session without committed evidence remains fenced for an identical client
retry.

Repositories written by older Crab builds may not have the verified Git
visibility evidence required by current background readability maintenance. If
the gateway reports that visibility repair is required, run `crab fsck
--repair` against the same repository to backfill historical generations, then
run `crab metadb owner --once` to fully verify and publish the current
catalog-bound proof. A repaired self-contained proof remains usable for
integrity checks after an older catalog checkpoint retires; current accelerated
Git reads still require the owner-published catalog-bound proof. The gateway
deliberately does not infer this proof from unverified objects inside a request
or background sweep. New gateway writes carry their own immutable visibility
evidence and continue the repaired proof.

Large payloads use bounded-memory request spools and Crab's verified LFS content
path. Multipart completion keeps objects through 64 MiB in a local spool. Above
that threshold it hashes and validates the durable selected parts, then replays
them directly into LFS without assembling the logical object on local disk.
The pointer commit atomically adds an exact, same-directory LFS tracking rule,
so Git clones materialize gateway-authored large objects without local attribute
workarounds. LFS publication chooses an aligned backend part size from the final
object length, keeping every supported multipart object within its provider's
part-count and part-size limits. S3 uses at most 10,000 5 GiB parts, GCS uses at
most 10,000 5 GiB parts and caps objects at 5 TiB, and Azure uses at most 50,000
4,000 MiB blocks. Parts above the normal retained-payload budget upload
synchronously rather than retaining two multi-gigabyte payloads.
Successful publication records the provider's completion validator, so later
range requests bind to the verified object version without rehashing the whole
object for every slice.
Objects already stored as Crab/Xet pointers retain Xet deduplication: partial
GET and copy-source ranges limit reconstruction to overlapping Xet chunks and
partial GET streams selected bytes through one bounded backpressure slot and
cancels reconstruction on disconnect without response-sized scratch. Copy-source
ranges still use bounded temporary storage because publication requires a fully
verified source before mutation. Low-coverage cold reads fetch bounded xorb
ranges; the cache may fetch a complete verified xorb for high-coverage reads.
Complete GETs retain whole-file verification before response headers. Legacy
Crab and LFS pointers project their content digest as an opaque ETag, so HEAD,
listings, conditions, and range admission do not hydrate object payloads.
Every process uses one cache instance shared by all configured repositories.
The required `[cache]` section supplies an absolute writable directory and a
positive `max_bytes` retention ceiling; startup proves descriptor-relative
publish and removal and initializes the cache catalog before opening the
listener. Keep this cache on a private volume separate from request scratch so
eviction and upload admission do not compete for the same free-space signal.
The complete frozen surface and deliberate exclusions are in the protocol
contract linked below.

## Read and write performance model

Requests share immutable repository read views keyed by the compacted generation
and committed journal state. Ref snapshots, parsed Git trees, and S3 attributes
are singleflight-cached inside that view. HEAD and attributed LIST requests use
the committed size and ETag without opening blob payloads.

The per-process `max_in_flight_requests` budget is split into reserved control,
read, and transfer pools so large uploads cannot starve bucket discovery,
metadata, or range reads. Each pool admits a bounded FIFO burst for up to 60
seconds before returning S3 `SlowDown` with `Retry-After: 1`; request bodies are
not consumed while waiting. Standard S3 SDK retry policies handle this response;
custom clients should retry with exponential backoff and jitter. The default
budget is 32 and should be tuned from measured CPU, memory, file-descriptor, and
scratch usage rather than client fanout alone. Each
PutObject, UploadPart, and copied source range uses a request-local temporary
file. Large multipart completion rereads durable parts instead of creating an
additional full-object spool, so its local scratch does not scale with the
assembled object size. Deployments must still place `TMPDIR` on
capacity-managed scratch storage sized for concurrent request bodies. The
gateway atomically reserves declared bodies before reading them and reserves
unknown streams in bounded increments. Xet reconstructions that materialize
local files and generated Git packs share the same process-wide capacity gate;
partial Xet GETs use bounded in-memory backpressure instead. The gate retains
10% of the filesystem outside reservations, with a 64 MiB minimum and 1 GiB
maximum, and returns retryable S3 `SlowDown` before admitted work can consume
that headroom.
Give each replica its own scratch mount; reservations are process-local while
filesystem probes account for already materialized bytes from every writer.
Registered multipart staging is bounded by the configured active-slot count
times the per-upload byte budget. Transfers that have not registered yet add at
most one 5 GiB payload per admitted transfer request; the transfer admission
pool bounds that crash window. An interrupted replacement can retain its
superseded immutable payload until the upload completes, aborts, or expires.

Each mutation rewrites only the target path's ancestor trees and persists one
path-local attribute delta. Immutable pack, index, visibility, and attribute
artifacts are prepared and uploaded before the destination ref lease; the lease
contains only branch revalidation and journal publication. Same-ref requests are
admitted through a bounded FIFO queue, while different refs may prepare in
parallel. A new write cancels maintenance that is still in the idle debounce.
Once a pass starts canonical publication, it drains across the manifest,
catalog, and visibility boundary; a write that arrives during that pass is
coalesced into a follow-up pass. Successful journal publication is immediately
readable by the gateway; catalog compaction and commit-graph maintenance continue
after the write burst becomes idle. This prevents cancellation from leaving an
intermediate manifest generation without its verified visibility proof.

Run `crab metadb owner` as one continuously supervised worker for each backing
repository. The owner performs bounded geometric repack outside request
acknowledgement, along with catalog, visibility, and commit-graph maintenance.
Monitor its `geometric_repack_packs`, `action`, and maintenance byte fields; a
persistently nonzero candidate count means the worker is absent, repeatedly
deferred by its maintenance budget, or failing. `crab repack` remains the
explicit catch-up command for an already fragmented repository.

## Build and run

```sh
cargo build --release -p crab-s3-gateway --locked
crab-s3-gateway --config /etc/crab/s3-gateway.toml --initialize
crab-s3-gateway --config /etc/crab/s3-gateway.toml
crab-s3-gateway --config /etc/crab/s3-gateway.toml --healthcheck
crab-s3-gateway --config /etc/crab/s3-gateway.toml --readiness-check
```

`--initialize` creates missing canonical Crab metadata only for empty configured
prefixes, then exits. It is safe to run repeatedly. Normal serving never
initializes or converts repository storage.

The S3 and management listeners are deliberately separate. `GET /livez` on
`management_listen` reports only that the process can serve requests. `GET
/readyz` freshly reads and constructs every configured repository's current
immutable view; it returns `503 Service Unavailable` with `Retry-After: 5` when
any repository is unsafe to serve. `GET /metrics` returns Prometheus 0.0.4 text
for bounded HTTP method/outcome counts, full response-stream duration and
in-flight requests, response-body errors/aborts, and control/read/transfer
admission capacity, queue pressure, and outcomes. It also reports aggregate
multipart-maintenance cycles, completed lifecycle actions, failure reasons,
cycle duration, and the last cycle in which every configured repository was
healthy. Per-slot failures make that cycle degraded instead of disappearing
into a successful sweep. Content-spool, Xet-reconstruction, and generated-pack
series expose currently owned temporary files and logical reserved bytes,
cumulative bytes written, and bounded create/write/flush/read failures.
Ownership remains charged until the spool, response stream, or pack upload
drops, including cancellation and disconnect.
Backend series cover the complete logical object-store call and response-stream
lifetime for GET, HEAD, range, PUT, delete, list, copy, and multipart lifecycle
operations. They expose fixed success/failure classes, active calls, duration,
body bytes delivered, and payload bytes accepted by successful writes. These
are logical Crab transport operations; provider-internal HTTP retries may make
more wire requests than the counters report. Scratch-filesystem gauges report
the total, free, and process-available bytes seen at the configured process
temporary directory on every scrape. A separate probe-success gauge and failure
counter make mount loss distinguishable from genuine zero capacity; a failed
probe clears all three capacity gauges rather than retaining stale values.
Separate headroom, pending-reservation, and bounded rejection series expose
capacity admission before the filesystem reports an I/O failure.
Cache series report fixed memory/local/service read attempts by hit, miss, or
failure; verified bytes returned by each cache layer; and best-effort local
persistence failures. Per-scrape gauges read aggregate entry, retained,
reserved, and temporary-byte totals from the existing SQLite catalog without
walking payload files. Probe health, failures, and last-success time distinguish
an empty cache from an unavailable or malformed catalog. Concurrent scrapes
coalesce rather than queue catalog probes.
Metric labels are fixed enums; they never contain
repository names, refs, keys, upload IDs, principals, access keys, or secrets.
The gateway also suppresses protocol-library debug/trace events that contain
complete signed requests and malformed-body events that contain raw payloads.
This credential boundary cannot be disabled through `RUST_LOG`; other gateway
debug logging remains operator-configurable.
The corresponding CLI checks are suitable for container and orchestration
probes. The metrics endpoint is unauthenticated by design; scrape it only over
the private management network and never publish that listener through the S3
ingress.

The backing provider uses Crab's existing environment credential chain. Set
the usual AWS, GCP, or Azure credentials for the selected provider. For an
S3-compatible endpoint, `AWS_ENDPOINT_URL_S3`, `AWS_ALLOW_HTTP`, and
`AWS_VIRTUAL_HOSTED_STYLE_REQUEST` are supported by the shared storage layer.

`endpoint_domain` is the gateway's public host name, without a scheme. When it
is set, both `https://endpoint.example/repository/key` and
`https://repository.endpoint.example/key` address the same logical bucket.
The deployment's DNS and TLS certificate must cover the wildcard host.

See `deploy/gateway.example.toml` for configuration and
`crab/docs/architecture/s3-gateway-contract.md` for the protocol contract.
For a temporary client credential, add `session_token_file` and `expires_at` to
the same `[[credentials]]` entry. Both are required together, the token file is
subject to the same private-permission check as the secret-key file, and the
process must be rolled before the configured credential expires.
Terminate with SIGTERM or SIGINT for graceful connection draining.

Production deployments should bind to a private listener and terminate TLS at
an ingress, load balancer, or service mesh. Do not expose the plain HTTP
listener beyond a trusted network boundary.

Build the checked image from the repository root with:

```sh
docker build -f crates/crab-s3-gateway/deploy/Dockerfile -t crab-s3-gateway .
```

The isolated Docker Compose qualification procedure and retained evidence
contract are in `deploy/README.md`. Successful packaged-image runs retain a
machine-verified report for 90 days. The report includes a real duplicated
64 MiB Crab/Xet fixture, projected ETags, an exact throttled 16 MiB range, and
proof that metadata and range delivery used no Xet reconstruction scratch.
Stale, dirty, incomplete, skipped, unmeasured, or identity-bearing reports fail
the evidence gate.
The statically validated Kubernetes workload and EKS values are in
`deploy/helm/crab-s3-gateway/`. They are deployment assets, not evidence of a
live EKS qualification. Operational alert response, scaling, credential
rotation, repository maintenance, backup/restore, and upgrade/rollback are in
the [operations runbook](deploy/operations.md).

Run it with a read-only root filesystem, a capacity-limited writable scratch
mount at `/var/lib/crab/tmp`, a separate bounded cache mount whose child path
matches `[cache].directory`, the configuration mounted at
`/etc/crab/s3-gateway.toml`, and credential files mounted read-only for UID/GID
10001. Credential files may be owner-only or readable only by the process's
effective group; group write/execute and all other-user access are rejected.
Expose port 8080 only through the S3 ingress and port 8081 only to the workload's
probe network.
