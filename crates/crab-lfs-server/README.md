# crab-lfs-server

crab-lfs-server is the standalone HTTP composition boundary for standard Git
LFS clients. It owns URL discovery, Batch negotiation, basic upload/download,
the optional verify action, File Locking endpoints, authentication,
repository policy, request limits, upload spooling, and TLS listener setup.

The server never gives a client object-store credentials. It builds one
provider-neutral origin through crab-storage and creates a crab-lfs handle
under the configured origin prefix and repository name. Every object is
verified against its SHA-256 OID before it is served or accepted.

The supported route shape is:

    /<repository>.git/info/lfs
    /<repository>.git/info/lfs/objects/batch
    /<repository>.git/info/lfs/objects/<sha256>
    /<repository>.git/info/lfs/objects/<sha256>/verify
    /<repository>.git/info/lfs/locks

Basic downloads support one RFC 7233 byte range, including open-ended ranges;
range requests always return `206 Partial Content` with `Content-Range`, even
when the selected range covers the complete object.

Batch object checks run with bounded ordered concurrency, so large Git LFS
batches retain the client-request order without issuing an unbounded burst of
object-store requests.

For deployments mounted below a reverse-proxy prefix, the equivalent
`/lfs/<repository>/info/lfs` form is also accepted. `/healthz` is an
unauthenticated process-liveness endpoint. `/readyz` checks that the upload
spool directory remains available, and `/metrics` exposes process-wide
Prometheus counters without repository or path labels; both are unauthenticated
operational endpoints.

Set `server.public_url` explicitly for internet-facing deployments. When it is
omitted, native TLS listeners generate HTTPS links; reverse-proxy deployments
must strip and recreate `x-forwarded-proto` before forwarding requests.

Repository names are normalized by removing a final .git suffix, matching
Git LFS server discovery. The origin URL prefix and normalized repository
name form the object-store namespace; requests cannot select an arbitrary
bucket path.

The configuration file is TOML. Authentication supports none for trusted
development, Basic users with bounded scrypt PHC password hashes, bearer
tokens with BLAKE3 token hashes, and mTLS identity supplied by the native TLS acceptor or
an explicitly trusted reverse proxy (`auth.trust_proxy_mtls = true`). The
gateway rejects proxy identity headers unless that trust is configured.
Production deployments should use HTTPS, a policy file that grants
read/write/admin actions per repository, and `server.action_secret` (or
`CRAB_LFS_ACTION_SECRET`) so Batch actions are short-lived and bound to their
repository, operation, OID, and size. Set `server.max_spool_bytes` to the
capacity reserved for in-flight upload files; the gateway returns `507` when
that process-local budget is exhausted. Set `server.max_requests_per_second`
and `server.request_burst` for process-local `429` admission protection;
responses include `Retry-After` and Git LFS may retry them. The dedicated
spool directory is scanned at startup and stale `.crab-lfs-upload-*` files
older than twice `server.request_timeout` are removed. Recent gateway-owned
files are charged against the startup spool budget; startup fails closed when
their size already exceeds `server.max_spool_bytes`. Keep unrelated files out
of that directory.

Signed Batch actions also include Git LFS `expires_in` metadata, matching the
capability lifetime advertised by the URL.

Lock creation is exclusive, including repeated requests from the same owner,
and lock listing is ID-ordered with bounded page retention. Lock record reads
for listing, verification, and pre-push conflict checks use bounded ordered
concurrency; pagination still scans the repository lock prefix. Unlock
operations bind the compare-and-swap release to the requested lock ID so a
stale force unlock cannot release a replacement lock for the same path. Batch
and locking requests accept the standard optional Git ref/refspec context; the
current lock namespace is repository-wide, so that context is validated but
does not yet partition locks by branch.

When no action secret is configured, the gateway permits unauthenticated
object actions only with `auth.mechanism = "none"`, or with an end-to-end mTLS
deployment whose client identity is present on every action request. Basic and
bearer configurations are rejected without an action secret because Git LFS
clients do not reliably forward repository credentials to Batch action URLs.

The binary is intentionally separate from crab-cache-server. The cache
server remains responsible for cache HTTP routes, while this package is the
Git LFS protocol gateway.
