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

For deployments mounted below a reverse-proxy prefix, the equivalent
`/lfs/<repository>/info/lfs` form is also accepted. `/healthz` is an
unauthenticated process-liveness endpoint.

Repository names are normalized by removing a final .git suffix, matching
Git LFS server discovery. The origin URL prefix and normalized repository
name form the object-store namespace; requests cannot select an arbitrary
bucket path.

The configuration file is TOML. Authentication supports none for trusted
development, Basic users with BLAKE3 password hashes, bearer tokens with
BLAKE3 token hashes, and mTLS identity supplied by the native TLS acceptor or
an explicitly trusted reverse proxy (`auth.trust_proxy_mtls = true`). The
gateway rejects proxy identity headers unless that trust is configured.
Production deployments should use HTTPS, a policy file that grants
read/write/admin actions per repository, and `server.action_secret` (or
`CRAB_LFS_ACTION_SECRET`) so Batch actions are short-lived and bound to their
repository, operation, OID, and size.

Lock creation is exclusive, including repeated requests from the same owner,
and lock listing is ID-ordered with bounded page retention. Unlock operations
bind the compare-and-swap release to the requested lock ID so a stale force
unlock cannot release a replacement lock for the same path.

When no action secret is configured, the gateway permits unauthenticated
object actions only with `auth.mechanism = "none"`, or with an end-to-end mTLS
deployment whose client identity is present on every action request. Basic and
bearer configurations are rejected without an action secret because Git LFS
clients do not reliably forward repository credentials to Batch action URLs.

The binary is intentionally separate from crab-cache-server. The cache
server remains responsible for cache HTTP routes, while this package is the
Git LFS protocol gateway.
