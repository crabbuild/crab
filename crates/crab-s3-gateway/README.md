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
50 TB object limit.
Large payloads use bounded-memory spooling and Crab's verified LFS content path.
The complete frozen surface and deliberate exclusions are in the protocol
contract linked below.

## Read and write performance model

Requests share immutable repository read views keyed by the compacted generation
and committed journal state. Ref snapshots, parsed Git trees, and S3 attributes
are singleflight-cached inside that view. HEAD and attributed LIST requests use
the committed size and ETag without opening blob payloads.

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
