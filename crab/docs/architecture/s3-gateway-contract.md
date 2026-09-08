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

Repository history is the only version model. The initial release does not
implement AWS bucket versioning, delete markers, or version-ID parameters. A
branch names a mutable view; a tag or full commit ID names an immutable read-only
view. Every request resolves and pins one commit before reading. Writes recheck
authorization, branch protection, and the branch tip under the canonical per-ref
publication lock. A conflicting branch update returns `OperationAborted` and is
safe for the S3 client to retry. Conditional PUT and DELETE headers are not part
of the initial surface and return `NotImplemented` before reading the body.

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
The empty key, a ref without a trailing slash, and a ref root are prefixes, not
objects. `GetObject`, `HeadObject`, and mutations require a non-empty repository
path.

## Supported key profile

S3 keys are restricted to paths representable without loss in a Git tree. The
complete logical key is at most 1024 UTF-8 bytes. Its repository path is
non-empty, uses `/` separators, and has components of 1–255 bytes. Empty, `.`,
`..`, and case-insensitive `.git` components are rejected. NUL, ASCII control
characters, repeated separators, leading or trailing separators, and trailing
folder-marker objects are rejected. The gateway never normalizes Unicode or
path separators.

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

Access-key lookup yields current HMAC verification material and one Crab
principal. Unknown keys fail before repository authorization. Static key
rotation or revocation takes effect when the process reloads its configuration.
Gateway keys are never backend cloud credentials. Secret values must come from
protected files, never command-line arguments, logs, error bodies, persisted
multipart records, or reports.

Every request authorizes its logical repository, ref, path, and action after
signature verification. Historical commit and tag reads require current
repository read permission. Copy independently authorizes the pinned source and
destination. Direct writes to protected branches return `AccessDenied`.

## Object representation

The object ETag is the quoted lowercase hexadecimal MD5 of logical object bytes.
It is stable across metadata-only changes but is not a Git OID. Multipart ETags
use the S3-compatible quoted MD5 of concatenated binary part MD5 values followed
by `-PART_COUNT`. ETags are validators, not integrity claims beyond the exact
response contract.

Object attributes are stored in an immutable versioned manifest named by the
new commit and uploaded before that commit becomes reachable. Readers select the
manifest by their pinned commit, so tree bytes and attributes cannot be mixed
across versions. Attributes contain user metadata, `Content-Type`,
`Content-Encoding`, `Content-Disposition`, `Content-Language`, `Cache-Control`,
`Expires`, ETag, logical size, and modification time. Ordinary Git commits that
lack an attribute entry project an empty metadata map, inferred
`application/octet-stream`, an ETag computed from logical bytes, and the
selected commit's committer time. A content write replaces prior attributes;
`CopyObject` with `COPY` copies them and `REPLACE` uses request attributes.
Deletes remove the current attribute entry. Renames and merges performed outside
the gateway follow normal Git projection until a later gateway write commits an
explicit entry.

`Last-Modified` is the committed attribute modification time, rounded to whole
seconds. A metadata-only write changes `Last-Modified` while retaining the ETag.
Content and attributes are never read from different commits.

## Protocol surface

The initial release supports path-style addressing. Virtual-hosted addressing
is not configured by the gateway. HTTPS is mandatory beyond loopback and is
terminated by the deployment ingress. Browser POST, bucket create/delete, ACLs, policies, IAM,
versioning, tagging, Select, object lock, retention, torrent, website, inventory,
replication, acceleration, notification, storage-class selection, and all SSE
request headers return `NotImplemented` or the operation-specific documented S3
error before mutation.

Supported operations:

| Operation | Supported contract |
| --- | --- |
| `ListBuckets`, `HeadBucket` | Authorized logical repositories only; deterministic order |
| `GetObject`, `HeadObject` | metadata, response overrides, RFC dates, ETag/date conditions, one byte range including open and suffix forms |
| `ListObjects`, `ListObjectsV2` | prefix, delimiter `/`, marker/start-after, max keys, reusable keys, common prefixes |
| `PutObject` | body up to 5 GiB, `Content-MD5`, SigV4 payload hash, CRC32/CRC32C/CRC64NVME/SHA1/SHA256 checksums, metadata and standard content headers |
| `DeleteObject`, `DeleteObjects` | S3 missing-key success, per-key authorization/results, quiet mode, and at most 1000 XML entries |
| `CopyObject` | pinned source, source conditions/range where defined, `COPY`/`REPLACE`, separately authorized destination |
| Multipart create/upload/copy/list/abort/complete | durable opaque sessions, part replacement, ordered selection, 10,000 parts, 5 GiB per part, 50 TB completed objects, restart and multi-instance retry |

Modeled unsupported request headers and query parameters are rejected rather
than ignored. Multi-range GET returns `InvalidRange`. `versionId` returns
`NotImplemented`. Checksums are validated before publication. A response never
attaches a full-object checksum to a partial range unless the protocol defines
the matching checksum mode.

Conditional reads use S3 precedence: match conditions are evaluated before
unmodified conditions, then modified conditions; a failed read condition returns
`NotModified` or `PreconditionFailed` as defined by that header. Conditional
PUT, DELETE, multipart completion, and destination COPY are not in the initial
surface.

## Listings and continuation

Keys are ordered by their complete UTF-8 byte representation, including the
encoded ref prefix. `delimiter=/` groups each subtree once and counts a common
prefix against `MaxKeys`. V1 markers are visible keys and each page resolves the
current branch according to S3's non-snapshot behavior.

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
completion request, and the terminal
completion ETag. The committed attribute manifest carries the upload identity
so an identical retry can recognize a publication that completed before its
multipart record reached the terminal state.

The state machine is `Open -> Completing -> Completed` or `Open -> Aborted`.
Registration, freeze, and abort use conditional revisions. A network transfer
does not hold a branch lock. A part is acknowledged only after its immutable
bytes and catalog registration are durable. Replacing a part number swaps the
record to a new immutable object. Frozen and active objects remain GC roots.

Completion requires 1–10,000 strictly ascending selected parts, matching quoted
ETags, and at least 5 MiB for every selected part except the last. It freezes the
exact ordered identities before execution and persists the committed response.
An identical retry returns the recorded result; a different selection cannot
cause another mutation. An uncertain result remains `Completing` and an
identical retry resumes from the frozen part set.

List APIs are ordered and bounded, reauthorize every page, exclude terminal and
replaced transfers, and work without process-local iterator state. Abort first
persists the terminal state, then synchronously removes part objects. Completion
persists its terminal outcome before best-effort part cleanup; a cleanup failure
does not erase the completed outcome.

Limits follow the S3 general-purpose bucket contract: 5 GiB per single PUT or
multipart part, 10,000 parts per upload, 50 TB per completed multipart object,
and 1000 results per multipart listing page. Unknown-length streams are counted
as they arrive. Requests beyond an operation's S3 limit return `EntityTooLarge`.

## Error and response contract

Errors use S3 XML with a stable `Code` and safe `Message`. HEAD errors have a
status and headers but no body. Internal source errors are logged at the service
boundary without credentials or request bodies.

| Condition | S3 code |
| --- | --- |
| Unknown/revoked credential or invalid signature | `InvalidAccessKeyId` / `SignatureDoesNotMatch` |
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
| Admission capacity limit | `SlowDown` |
| Corrupt/unavailable committed data | `InternalError` |

Request bodies and multipart completion are streamed through bounded memory to
temporary storage while checksums are computed. Objects above the inline Git
threshold are stored through Crab's verified LFS content path; the committed Git
blob is the canonical LFS pointer and the S3 attribute record retains the logical
size and ETag. GET streams LFS content directly and reconstructs Crab pointers to
temporary storage before opening the response. Successful writes are returned
only after their committed outcome is durable and read-ready.

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
RustFS-backed Crab repository. The broader client/backend matrix remains a
release gate, not an inferred claim from that local smoke test.
