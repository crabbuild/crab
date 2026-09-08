# Crab S3 gateway

`crab-s3-gateway` presents configured Crab repositories as S3 buckets. Existing
S3 clients use their normal endpoint, region, access-key, and secret-key
configuration. Object keys use `REF/path`, for example
`s3://my-repository/main/data/model.bin`.

The gateway accepts S3 SigV4 and SigV2 authentication through `s3s`. It maps
each access key to a Crab principal and authorizes that principal against the
logical repository catalog. Gateway credentials are independent of the cloud
credentials used for the backing object store.

The initial client-compatible surface includes bucket listing/head, object
GET/HEAD/PUT/DELETE/COPY, V1/V2 object listing, multi-delete, and durable
multipart create/upload/copy/list/abort/complete. GET/HEAD support conditions
and a single byte range; PUT validates Content-MD5 and the standard S3 checksum
headers, and supports atomic create with `If-None-Match: *`. Path-style
addressing is required. Single PUTs and multipart parts support up to 5 GiB,
and multipart completion supports S3's 50 TB object limit.
Large payloads use bounded-memory spooling and Crab's verified LFS content path.
The complete frozen surface and deliberate exclusions are in the protocol
contract linked below.

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

See `s3-gateway.example.toml` for configuration and
`crab/docs/architecture/s3-gateway-contract.md` for the protocol contract.
Terminate with SIGTERM or SIGINT for graceful connection draining.

Production deployments should bind to a private listener and terminate TLS at
an ingress, load balancer, or service mesh. Do not expose the plain HTTP
listener beyond a trusted network boundary.
