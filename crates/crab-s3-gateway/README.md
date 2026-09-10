# Crab S3 gateway

`crab-s3-gateway` presents configured Crab repositories as S3 buckets. Existing
S3 clients use their normal endpoint, region, access-key, and secret-key
configuration. Object keys use `REF/path`, for example
`s3://my-repository/main/data/model.bin`.

The gateway accepts S3 SigV4 and SigV2 authentication through `s3s`. It maps
each access key to a Crab principal and authorizes that principal against the
logical repository catalog. Gateway credentials are independent of the cloud
credentials used for the backing object store.

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
`multipart_staging_bytes_per_upload` caps their registered temporary bytes, and
`multipart_upload_ttl_seconds` persists the Open-session expiry chosen when the
upload starts. The defaults are 1,024 sessions, S3's 50 TB object ceiling, and
seven days. Every instance serving the same repository must use the same three
values. A once-per-minute reconciler aborts expired Open sessions, retries terminal
cleanup, and reclaims expired slots whose process died before writing the session
record. Completing sessions remain fenced and are never expired.
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
stream the selected bytes through bounded temporary storage. Low-coverage cold
reads fetch bounded xorb ranges; the cache may fetch a complete verified xorb
for high-coverage reads. Complete GETs retain whole-file verification.
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
seconds before returning S3 `SlowDown`; request bodies are not consumed while
waiting. The default budget is 32 and should be tuned from measured CPU, memory,
file-descriptor, and scratch usage rather than client fanout alone. Each
PutObject, UploadPart, and copied source range uses a request-local temporary
file. Large multipart completion rereads durable parts instead of creating an
additional full-object spool, so its local scratch does not scale with the
assembled object size. Deployments must still place `TMPDIR` on
capacity-managed scratch storage sized for concurrent request bodies and reserve
it independently from the request count budget.
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
parallel. A new write cancels in-flight derived maintenance so foreground traffic
does not wait behind catalog or commit-graph work. Successful journal publication
is immediately readable by the gateway; catalog compaction and commit-graph
maintenance continue after the write burst becomes idle. Maintenance is
coalesced so a later journal wave never advances from a generation whose
visibility proof is still being finalized.

## Build and run

```sh
cargo build --release -p crab-s3-gateway --locked
crab-s3-gateway --config /etc/crab/s3-gateway.toml --initialize
crab-s3-gateway --config /etc/crab/s3-gateway.toml
```

`--initialize` creates missing canonical Crab metadata only for empty configured
prefixes, then exits. It is safe to run repeatedly. Normal serving never
initializes or converts repository storage.

The backing provider uses Crab's existing environment credential chain. Set
the usual AWS, GCP, or Azure credentials for the selected provider. For an
S3-compatible endpoint, `AWS_ENDPOINT_URL_S3`, `AWS_ALLOW_HTTP`, and
`AWS_VIRTUAL_HOSTED_STYLE_REQUEST` are supported by the shared storage layer.

`endpoint_domain` is the gateway's public host name, without a scheme. When it
is set, both `https://endpoint.example/repository/key` and
`https://repository.endpoint.example/key` address the same logical bucket.
The deployment's DNS and TLS certificate must cover the wildcard host.

See `s3-gateway.example.toml` for configuration and
`crab/docs/architecture/s3-gateway-contract.md` for the protocol contract.
Terminate with SIGTERM or SIGINT for graceful connection draining.

Production deployments should bind to a private listener and terminate TLS at
an ingress, load balancer, or service mesh. Do not expose the plain HTTP
listener beyond a trusted network boundary.
