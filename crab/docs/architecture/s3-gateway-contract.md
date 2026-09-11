# Crab S3 gateway protocol contract

Status: accepted initial-release contract. This record resolves the phase-0
choices in `crab-s3-gateway.md`. Implementation and qualification status is
tracked by the gateway crate and its test reports, not by this document.

## Release model

The gateway exposes existing Crab repositories through the S3 REST protocol.
Each successful `PutObject`, `CopyObject`, `DeleteObject`, or completed multipart
upload publishes one commit immediately to the addressed branch. `DeleteObjects`
performs ordered, individually reported mutations and is not atomic across keys.
A delete of a missing key succeeds without advancing the branch.

Repository history is the only version model. The gateway does not
implement AWS bucket versioning, delete markers, or version-ID parameters. A
branch names a mutable view; a tag or full commit ID names an immutable read-only
view. Every request resolves and pins one commit before reading. Writes recheck
authorization, branch protection, and the branch tip under the canonical per-ref
publication lock. A conflicting branch update returns `OperationAborted` and is
safe for the S3 client to retry. `PutObject` and `CompleteMultipartUpload`
support `If-None-Match: *` and strong `If-Match` atomically. The ETag is checked
before receiving or assembling content, and the observed blob identity is
checked again under the publication lock. Conditional DELETE headers are not
part of the surface and return `NotImplemented` before mutation.

## Bucket and key namespace

One configured logical repository is one S3 bucket. Bucket names are configured
explicitly and are unique ignoring ASCII case. They contain 3–63 lowercase
ASCII letters, digits, dots, and hyphens, start and end with a letter or digit,
and are never derived from a backing provider bucket. Backing bucket and prefix
values never appear in S3 responses.

An object key is `REF/KEY`. The first slash separates the encoded ref segment
from the repository path. A short ref selects `refs/heads/REF`;
`refs%2Fheads%2F...` and `refs%2Ftags%2F...` select a fully qualified ref; a
40-digit hexadecimal segment selects a commit. A branch/tag collision is
impossible for a short ref because short refs select branches only. Abbreviated
object IDs and Git revision expressions such as `~`, `^`, and `@{}` are rejected.
Writes require a branch ref.

The gateway verifies the signature against the original HTTP path and query,
then applies HTTP percent-decoding once. It splits the resulting S3 key at the
first literal slash, percent-decodes the ref segment once more, and does not
decode the remaining repository path again. A literal percent in a ref is
therefore represented in the logical key as `%25` and on the HTTP wire as
`%2525`. Copy source parsing follows the same rule independently of the
destination URI.

Examples:

| Logical URI | S3 bucket/key | Raw path | Ref | Repository path |
| --- | --- | --- | --- | --- |
| `crabfs://demo/main/a.txt` | `demo`, `main/a.txt` | `/demo/main/a.txt` | `refs/heads/main` | `a.txt` |
| `crabfs://demo/feature%2Fdata/a.txt` | `demo`, `feature%2Fdata/a.txt` | `/demo/feature%252Fdata/a.txt` | `refs/heads/feature/data` | `a.txt` |
| `crabfs://demo/refs%2Ftags%2Fv1/a%2Fb` | `demo`, `refs%2Ftags%2Fv1/a%2Fb` | `/demo/refs%252Ftags%252Fv1/a%252Fb` | `refs/tags/v1` | literal `a%2Fb` |
| `crabfs://demo/0123456789012345678901234567890123456789/a` | same bucket/key | `/demo/0123456789012345678901234567890123456789/a` | that commit | `a` |

`HeadBucket` addresses the repository. `ListObjects` and `ListObjectsV2` require
a ref in `prefix`; listing with an empty prefix returns authorized branch
prefixes only. Tag and commit namespaces are not synthesized at repository root.
Listing beneath an unknown or unborn ref returns an empty page.
The empty key, a ref without a trailing slash, and a ref root are prefixes, not
objects. `GetObject`, `HeadObject`, and mutations require a non-empty repository
path.

## Supported key profile

S3 keys are restricted to paths representable without loss in a Git tree. The
complete logical key is at most 1024 UTF-8 bytes. Its repository path is
non-empty, uses `/` separators, and has components of 1–255 bytes. Empty, `.`,
`..`, and case-insensitive `.git` components are rejected. NUL, ASCII control
characters, repeated separators, leading or trailing separators, and trailing
folder-marker objects are rejected. Empty `REF/path/` PUT and DELETE requests
are accepted as virtual directory hints for filesystem clients. They are
validated and authorized against `REF/path`, but are not persisted, listed, or
returned as objects because Git trees already represent non-empty directories.
Non-empty marker PUTs are rejected. The gateway never normalizes Unicode or path
separators.

Writes create ordinary non-executable Git blobs. They reject a path whose
ancestor is a blob or whose existing entry is a tree. Overwriting a symlink,
executable, or submodule replaces that entry with an ordinary blob; deleting it
removes the entry. Reads expose pre-existing ordinary and executable blobs as
objects; symlinks and submodules return `InvalidObjectState`. A Git tree cannot
contain both `a` and `a/b`, and the gateway reports the conflict instead of
inventing a lossless side namespace. These restrictions are intentional
compatibility limits, never silent transformations.

## Authentication and authorization

The gateway requires Crab-issued access-key credentials and accepts S3 SigV4
header signing, SigV4 presigned queries, SigV4 streaming payloads supported by
the selected protocol library, SigV2 headers, and SigV2 presigned queries.
Unsigned requests return `AccessDenied`. Header-signed requests allow 15 minutes
of clock skew. Presigned request expiry is verified by the protocol layer;
operators should issue SigV4 URLs for no more than seven days.
Configured temporary SigV4 credentials require the exact session token in
either the signed header or signed query, never both, and an RFC 3339 expiry.
Missing, wrong, duplicate, or expired tokens fail as `InvalidToken` or
`ExpiredToken`; a header token omitted from `SignedHeaders` fails signature
verification. The gateway consumes temporary credential triples but does not
provide an STS issuance or refresh API. SigV4a and browser POST-policy uploads
remain outside this frozen profile.

Access-key lookup yields current HMAC verification material and one Crab
principal. Unknown keys fail before repository authorization. Static key
rotation or revocation takes effect when the process reloads its configuration.
Gateway keys are never backend cloud credentials. Secret values must come from
protected files, never command-line arguments, logs, error bodies, persisted
multipart records, or reports.
Protocol dependency events containing complete signed requests, signature
material, or raw malformed request bodies are suppressed even when application
debug logging is enabled. Operators cannot weaken this credential boundary
through `RUST_LOG`.

Every request authorizes its logical repository, ref, path, and action after
signature verification. Historical commit and tag reads require current
repository read permission. Copy independently authorizes the pinned source and
destination. Direct writes to protected branches return `AccessDenied`.

## Object representation

Gateway-authored object ETags are the quoted lowercase hexadecimal MD5 of
logical object bytes. They are stable across metadata-only changes but are not
Git OIDs. Multipart ETags use the S3-compatible quoted MD5 of concatenated
binary part MD5 values followed by `-PART_COUNT`. A pre-existing Crab or LFS
pointer without gateway attributes projects its BLAKE3 or SHA-256 content digest
as an opaque ETag, so HEAD, listings, conditions, and range admission do not
hydrate the logical payload. ETags are validators, not integrity claims beyond
the exact response contract.

Object attributes are stored in an immutable versioned manifest named by the
new commit and uploaded before that commit becomes reachable. Readers select the
manifest by their pinned commit, so tree bytes and attributes cannot be mixed
across versions. Attributes contain user metadata, `Content-Type`,
`Content-Encoding`, `Content-Disposition`, `Content-Language`, `Cache-Control`,
`Expires`, ETag, logical size, and modification time. Ordinary Git commits that
lack an attribute entry project an empty metadata map, inferred
`application/octet-stream`, a content-bound ETag according to the preceding
rule, and the selected commit's committer time. A content write replaces prior
attributes;
`CopyObject` with `COPY` copies them and `REPLACE` uses request attributes.
Deletes remove the current attribute entry. Renames and merges performed outside
the gateway follow normal Git projection until a later gateway write commits an
explicit entry.

`Last-Modified` is the committed attribute modification time, rounded to whole
seconds. A metadata-only write changes `Last-Modified` while retaining the ETag.
Content and attributes are never read from different commits.

## Protocol surface

Path-style addressing is always supported. Setting `endpoint_domain` enables
virtual-hosted addressing under that base domain; DNS and TLS wildcard coverage
remain deployment responsibilities. HTTPS is mandatory beyond loopback and is
terminated by the deployment ingress. Browser POST, bucket create/delete, ACLs, policies, IAM,
version mutation/listing, Select, object lock, retention, torrent, website, inventory,
replication, acceleration, notification, non-standard storage classes, and all SSE
request headers return `NotImplemented` or the operation-specific documented S3
error before mutation.

Supported operations:

| Operation | Supported contract |
| --- | --- |
| `ListBuckets`, `HeadBucket`, `GetBucketLocation`, `GetBucketVersioning` | Authorized logical repositories only; deterministic order; configured region; honest unversioned response |
| `GetObject`, `HeadObject`, `GetObjectAttributes` | metadata, response overrides, RFC dates, ETag/date conditions, checksum mode, object size, ETag, part-number reads, and paginated multipart-part attributes; one byte range including open and suffix forms |
| `ListObjects`, `ListObjectsV2` | prefix, delimiter `/`, marker/start-after, max keys, reusable keys, common prefixes, and the `RestoreStatus` optional-object hint |
| `PutObject` | body up to 5 GiB, atomic `If-Match` and `If-None-Match: *`, `Content-MD5`, SigV4 payload and chained streaming signatures, CRC32/CRC32C/CRC64NVME/SHA1/SHA256 checksums, tags, metadata, standard content headers with the `aws-chunked` transport token removed, explicit `STANDARD` storage class, and virtual empty directory-marker hints |
| `GetObjectTagging`, `PutObjectTagging`, `DeleteObjectTagging` | Up to ten current-object tags; tag changes publish metadata-only commits without changing object bytes or ETag |
| `DeleteObject`, `DeleteObjects` | S3 missing-key success, virtual directory-marker deletion, per-key authorization/results, quiet mode, and at most 1000 XML entries |
| `CopyObject` | pinned source, source conditions/range where defined, metadata/tag `COPY`/`REPLACE`, checksum selection, explicit `STANDARD` storage class, separately authorized destination |
| Multipart create/upload/copy/list/abort/complete | durable opaque sessions, conditional completion, validated full-object CRC and composite CRC/SHA checksums, part replacement, explicit `STANDARD` storage class, ordered selection, 10,000 parts, 5 GiB per part, 50 TB completed objects, restart and multi-instance retry |

Modeled unsupported request headers and query parameters are rejected rather
than ignored. Multi-range GET returns `InvalidRange`. `versionId` returns
`NotImplemented`; `GetBucketVersioning` returns the S3 empty/unversioned state.
Checksums are validated before publication and stored in the commit attribute
manifest. XML mutation bodies for `PutObjectTagging` and `DeleteObjects` require
and validate `Content-MD5` before metadata or deletion mutation; supplied
`x-amz-checksum-*` values are validated as well. Objects uploaded without a
checksum receive S3's default full-object CRC64NVME checksum. A response never
attaches a full-object checksum to a
partial range. A part-number request or a byte range exactly aligned to one
persisted multipart part returns that part's checksum.
Composite object checksum responses carry S3's `-PART_COUNT` suffix; individual
part checksums and a precomputed completion checksum use the raw Base64 digest.

Conditional reads use S3 precedence: match conditions are evaluated before
unmodified conditions, then modified conditions; a failed read condition returns
`NotModified` or `PreconditionFailed` as defined by that header. Conditional
DELETE and destination COPY conditions are outside this surface. `PutObject`
and `CompleteMultipartUpload` support atomic strong `If-Match` and
`If-None-Match: *`; weak or multi-value write validators are rejected.

## Listings and continuation

Keys are ordered by their complete UTF-8 byte representation, including the
encoded ref prefix. `delimiter=/` groups each subtree once and counts a common
prefix against `MaxKeys`. Each page seeks from its prefix and exclusive marker
inside the raw Git trees, stops after `MaxKeys` plus one lookahead, and reads
blob metadata only for emitted objects. It does not hydrate logical payloads or
walk every preceding/following object. V1 markers are visible keys and each page
resolves the current branch according to S3's non-snapshot behavior.

V2 continuation tokens are the last emitted raw key or common prefix. They are
portable across gateway nodes and are reauthorized when the next request is
handled. Like V1 markers, they resume against the branch state current for that
request; listings are not snapshot-pinned. `encoding-type=url` percent-encodes
the S3-defined response fields but does not transform the continuation token.

## Multipart durability

Gateway multipart state is provider-neutral and separate from native provider
multipart uploads. The durable catalog uses versioned records and conditional
updates. An upload records its repository placement, branch/key, initiator,
attributes, timestamp, state, revision, registered immutable parts, the frozen
completion request, terminal completion ETag, capacity-slot generation, expiry,
and per-upload staging-byte budget. The committed attribute manifest carries the
upload identity
so an identical retry can recognize a publication that completed before its
multipart record reached the terminal state.

The state machine is `Open -> Completing -> Completed` or `Open -> Aborted`.
Registration, freeze, and abort use conditional revisions. A network transfer
does not hold a branch lock. A part is acknowledged only after its immutable
bytes and catalog registration are durable. Replacing a part number swaps the
record to a new immutable object. Every transfer has a unique payload identity,
so the winning replacement can reclaim the prior payload and a losing
registration can reclaim its own bytes without deleting a concurrent winner.
Frozen and active objects remain GC roots.

Completion requires 1–10,000 strictly ascending selected parts, matching quoted
ETags, and at least 5 MiB for every selected part except the last. It freezes the
exact ordered identities before execution and persists the committed response.
An identical retry returns the recorded result; a different selection cannot
cause another mutation. An uncertain result remains `Completing` and an
identical retry resumes from the frozen part set.

List APIs are ordered and bounded, reauthorize every page, exclude terminal and
replaced transfers, and work without process-local iterator state. Abort and
completion first persist their terminal state, then attempt immediate part
cleanup. A cleanup failure does not erase the terminal outcome or release its
capacity slot; background reconciliation retries it.

Every configured repository owns a fixed durable capacity-slot catalog. Slot
create/reuse and release use conditional generations, so concurrent gateway
instances cannot exceed `max_active_multipart_uploads` and a delayed worker
cannot release a newer occupant. Session creation persists its slot generation,
absolute Open-state expiry, and staging-byte budget before acknowledging the
upload. All instances serving a repository must use the same capacity, staging,
and expiry values. Registered part replacement is rejected with `SlowDown` when
the persisted byte budget would be exceeded, leaving the former part
authoritative.

The bounded background reconciler scans only capacity slots. It conditionally
transitions expired Open sessions to Aborted, retries cleanup for terminal
sessions, and reclaims an expired slot and payload prefix when a process died
between capacity acquisition and session publication. For Completing sessions,
the gateway persists the planned ETag and checksums before mutation and derives a
deterministic ref-journal publication-plan ID from the upload ID. The reconciler
transitions to Completed when that plan's immutable commit evidence proves
publication, including after a later write replaces the object. Current
Git-bound upload ID, ETag, logical size, selected parts, and user attributes are
the compatibility proof for a record created before plan binding. Missing or
mismatched evidence leaves the frozen parts fenced for an identical client retry;
Completing sessions are never expired merely because their original Open
deadline passed.

Limits follow the S3 general-purpose bucket contract: 5 GiB per single PUT or
multipart part, 10,000 parts per upload, 50 TB per completed multipart object,
and 1000 results per multipart listing page. A GCS-backed repository caps the
completed object at GCS's lower 5 TiB physical limit. Unknown-length streams are
counted as they arrive. Requests beyond the applicable limit return
`EntityTooLarge` before multipart assembly begins.
LFS publication derives an aligned backend part size from the completed object
length so the content-addressed upload stays within the configured provider's
part-count and part-size limits: 10,000 5 GiB parts for S3 and GCS, or 50,000
4,000 MiB blocks for Azure. When an adaptive part exceeds the normal
retained-payload budget, it is submitted synchronously to avoid retaining a
second large part concurrently.
Successful LFS multipart completion records the provider's returned validator.
Subsequent range requests compare that validator with the current object and
read only the requested bytes instead of rehashing the complete object for each
range.

## Error and response contract

Errors use S3 XML with a stable `Code` and safe `Message`. HEAD errors have a
status and headers but no body. Internal source errors are logged at the service
boundary without credentials or request bodies.

| Condition | S3 code |
| --- | --- |
| Unknown/revoked credential, invalid signature, or invalid/expired session token | `InvalidAccessKeyId` / `SignatureDoesNotMatch` / `InvalidToken` / `ExpiredToken` |
| Missing authentication or denied repository/path | `AccessDenied` |
| Unknown logical repository | `NoSuchBucket` |
| Missing object | `NoSuchKey` |
| Invalid ref/key/encoding, body, XML, header, checksum | `InvalidArgument`, `MalformedXML`, or `BadDigest` |
| Unsupported operation/feature/version | `NotImplemented` |
| Failed condition | `PreconditionFailed` or `NotModified` |
| Invalid range | `InvalidRange` |
| Branch contention or transient shared-state conflict | `OperationAborted` |
| Missing/terminal multipart session | `NoSuchUpload` |
| Bad completion selection/order/size | `InvalidPart`, `InvalidPartOrder`, `EntityTooSmall` |
| Bounded admission queue exhaustion or wait timeout | `SlowDown` |
| Multipart active-session or persisted staging-byte capacity exhausted | `SlowDown` |
| Scratch capacity exhausted or filesystem probe unavailable | `SlowDown` |
| Corrupt/unavailable committed data | `InternalError` |

Every gateway-generated `SlowDown` includes `Retry-After: 1`. Clients should
still apply their normal S3 exponential-backoff policy with jitter; the header
is a retry floor, not a promise that distributed capacity will be free after
one second.

Request bodies are streamed through bounded memory to temporary storage while
checksums are computed. Declared request lengths reserve scratch before the body
is consumed; unknown streams reserve bounded increments. Content spools, Xet
range reconstruction, and generated Git packs share one atomic process-local
gate. It retains 10% of visible capacity outside reservations, bounded to a
64 MiB minimum and 1 GiB maximum. A failed capacity probe fails closed, and
reservation pressure returns `SlowDown` without publishing partial state.
Multipart completion keeps objects through 64 MiB in a local spool; larger
objects validate and hash the frozen durable parts, then replay them through
size-and-SHA-verified LFS publication without assembling the logical object on
local disk. Objects above the inline Git threshold are stored through Crab's
verified LFS content path; the committed Git blob is the canonical LFS pointer
and the S3 attribute record retains the logical size and ETag. That same Git
commit appends an exact tracking rule to the nearest `.gitattributes`,
preserving any existing rules, so ordinary Git/LFS checkout interprets the
pointer consistently. GET streams LFS content directly. A partial Crab/Xet GET
streams verified selected chunks through a single bounded backpressure slot and
cancels reconstruction when the response is dropped; a complete GET reconstructs
to temporary storage before opening the response so its whole-file hash is
verified. Successful writes are returned only after their committed outcome is
durable and read-ready.

The temporary-storage requirement is proportional to each in-progress request
body, copied range, or complete Crab/Xet GET, not to a partial Xet GET or a
completed multipart object's aggregate size.
Production deployments must place the process temporary directory on
capacity-managed scratch storage; atomic reservations bind admitted work to
currently visible free space before it writes. Newly published large multipart
content costs a second read of the frozen durable parts; an already verified
LFS object can skip that replay.

The process-local immutable read cache is also explicit: configuration requires
an absolute directory and positive retention ceiling. All repositories in one
gateway process share that bounded cache. Startup fails before listening unless
the cache owner can safely create, publish, sync, and remove a
descriptor-relative probe. Deploy the cache on a private volume separate from
scratch; cache loss may reduce performance but never removes acknowledged
repository state.

The private metrics listener reports cache attempts by the fixed
memory/local/service and hit/miss/failure dimensions, verified hit bytes, local
persistence failures, and aggregate catalog entry/byte accounting. Catalog
gauges come from a coalesced read-only SQLite probe on each scrape; probe health
and last-success time distinguish genuine zero usage from an unreadable
catalog. No series includes a repository, ref, path, endpoint, principal, or
credential label. Origin transport metrics remain the authority for fallback
cost and provider failures.

The canonical Helm chart can optionally install a release-scoped PodMonitor and
PrometheusRule. Both are opt-in because their CRDs belong to the Prometheus
Operator, not Crab. The bundled rules pass syntax validation plus healthy and
faulting semantic tests with a pinned `promtool`. Production qualification must
still prove that the deployed Prometheus selects both resources and that
Alertmanager delivers every routed severity to a real receiver.

## Repository extension API

S3 methods are not repurposed for Git concepts. A separate authenticated
`/crab/v1/repositories/{bucket}` API may expose ref listing, bounded history,
diff, and expected-OID branch/tag create/update/delete. Merge and explicit
staged commits remain excluded until their shared SDK contracts are specified.
`crabfs://` consumers need a filesystem/scheme adapter; configuring an S3
endpoint alone does not teach a library that URI scheme.

## Qualification matrix

Release qualification covers the AWS CLI v2, current AWS SDKs for Rust, Python
(boto3), JavaScript v3, Java v2, Go v2, and `s3cmd`. It runs against S3, GCS, and
Azure-backed Crab repositories through the provider-neutral storage layer.
Path-style HTTP is permitted only for loopback tests; deployment suites use HTTPS.

Every supported operation requires positive, protocol-error, authorization,
restart/two-instance, and independent Git/SDK visibility evidence. Multipart,
publication, and listing state is qualified under process termination and
concurrency. Unsupported/skipped cells make a report incomplete rather than
passing. Backend/client versions, source SHA and dirty digest, fixture identity,
assertion and byte counts, latency, RSS, scratch usage, and terminal state are
recorded without credentials.

The checked-in implementation currently has unit coverage for namespace,
multipart persistence/retry, checksum validation, and branch-preserving Git
mutation. Local release qualification additionally runs the AWS CLI against a
RustFS-backed Crab repository. It publishes a real duplicated 64 MiB Xet object
through Crab, then proves signed metadata and throttled range reads are
byte-exact without Xet reconstruction scratch. The broader client/backend matrix
remains a release gate, not an inferred claim from that local smoke test.
