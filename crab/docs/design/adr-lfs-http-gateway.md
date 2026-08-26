# ADR: Standard Git LFS HTTP gateway ownership

Status: accepted for implementation

## Decision

Standard Git LFS HTTP compatibility is owned by the top-level
crab-lfs-server package. The reusable crab-lfs package owns verified
SHA-256 object bytes and the shared LFS lock-record format; crab-storage owns
provider construction and object-store transport. crab-cache-server is not
the LFS gateway because its server contract is cache-specific.

The gateway exposes the standard Git LFS endpoint shape under
/<repository>.git/info/lfs. It also accepts the mounted
/lfs/<repository>/info/lfs form for reverse-proxy deployments. It implements
discovery, Batch, the basic transfer adapter, upload verification, byte-range
downloads, and File Locking. A final .git on the repository URL is removed for
storage routing, and the origin URL prefix plus repository name is the only
object namespace the request can address. `/healthz` is an unauthenticated
process-liveness endpoint.

## Authentication and authorization

The first shipped deployment contract supports HTTP Basic, bearer tokens,
and mTLS. Credentials are verified by the gateway; clients never receive
bucket or provider credentials. Basic and bearer configuration stores only
BLAKE3 hashes. Repository policy rules grant read, write, or admin actions
to an authenticated principal. Force unlock requires admin authorization
when a policy is configured.

Native mTLS validates the client certificate against the configured CA and
derives a certificate digest principal. Reverse-proxy mTLS may provide the
same identity through x-client-cn, but the proxy must strip untrusted
incoming copies and establish TLS before forwarding.

## Transfer and integrity

Batch responses select basic and return actions against the gateway. Object
presence and integrity checks run with bounded ordered concurrency so response
order remains stable without an unbounded object-store fan-out. Uploads are
streamed to a local spool file under a concurrency semaphore, then crab-lfs
verifies the declared size and SHA-256 digest before the object-store commit.
Successful verification best-effort records a provider-validator receipt, so
later checks can skip re-reading immutable bytes when the ETag or version still
matches. Downloads verify the complete immutable object before opening the
requested full or byte-range response. Object actions do not expose provider
URLs or long-lived cloud credentials. When `server.action_secret` is set,
Batch action URLs carry a short-lived signature bound to repository,
operation, OID, and size; the server rejects missing, expired, or tampered
grants. Git LFS does not forward the repository credential to these action
URLs, so a valid action is the authorization boundary for the object request.
Basic and bearer deployments therefore require the action secret; mTLS may
use the client certificate as the action-request credential instead.
Retries may reuse an unexpired action, as required for idempotent Git LFS
transfers.

## Operational boundary

The gateway applies a bounded Batch body, per-object size limit, request
timeout, concurrent request limit, upload concurrency limit, streamed-download
concurrency bound, temporary disk spooling, and bounded lock-list page
retention. Lock creation is exclusive, and unlock CAS operations are bound to
the requested lock ID. The origin URL is built only through crab-storage.
Native mTLS or an explicitly configured trusted proxy is required for mTLS
identity; an untrusted `x-client-cn` header is never accepted. Metrics,
rate-limit integration, and managed protected-push binding remain follow-up
controls before broad internet exposure.

## Compatibility contract

The supported HTTP profile is unmodified Git LFS using the standard basic
adapter. The standalone Crab transfer-agent and native Crab profiles remain
separate and continue to use their existing configuration and storage
paths. Compatibility claims require an end-to-end test with a real Git LFS
client and a real object-store provider.
