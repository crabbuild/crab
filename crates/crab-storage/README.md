# crab-storage

Explicit Azure SAS credentials use the provider's SAS-string parser before
request query encoding. Percent-encoded signature characters must be decoded
exactly once; passing encoded strings as query-pair values corrupts signatures.
The local HTTP regression checks the value actually received by a server.

`crab-storage` is Crab’s provider-neutral object-store boundary. It turns
S3, S3-compatible, GCS, Azure Blob, and local object stores into one `Store`
facade with stable paths, retries, conditional writes, range reads, and
integrity-aware errors.

## Why it exists

Every higher layer needs object storage, but none should duplicate provider
construction, URL handling, retry classification, or the Crab storage layout.
Keeping those decisions here makes a write through S3 behave like a write
through GCS, and makes CAS failures distinguishable from transient transport
failures.

## Architecture

```text
provider credentials / URL
            │
            ▼
   provider_store + options
            │
            ▼
       Store facade
   ┌────────┼─────────┐
   │        │         │
 layout   retry    CAS / ranges / streams
   │        │         │
   └────────┴─────────┘
            │
            ▼
       object_store
```

`StoreLayout` routes content-addressed objects such as shards and xorbs to a
global `.crab/` prefix while keeping manifests, refs, packs, and locks under a
repository prefix. `Store` adds conditional create/update, optional staged
writes, bounded reads, byte/request observers, and provider-neutral
`StorageError` values. `put_if_absent` distinguishes a new immutable object
from an identical retry without a preceding HEAD, and streamed size/hash
verification uses the GET response metadata instead of a separate HEAD.
`create_or_read_immutable` retains both ETag and version from the successful
PUT, verified readback, or occupied-key GET alongside its outcome. Origin proof
consumers must carry that identity with the verified bytes; a later HEAD may
observe another object version. Occupied-key bytes still require caller validation.
`Store::verify_bounded` combines that content-hash contract with pre-body size
admission and streamed length checks. It uses one GET for a successful read;
the limit applies to buffered encoded bytes, not the caller's decoded objects.
Readback-required immutable writes use that same bounded verifier and corruption
retry: one corrupt hash response permits one more GET, while persistent corruption
still fails. An acknowledged PUT is not replayed during verification. A successful
logical create carries the ETag and version from the hash-verified GET, including
when that response follows a corrupt read. Checksum-qualified writes still skip
readback, and occupied logical content addresses still return their body for
caller validation rather than requiring the candidate's physical encoding.

The admission token cancels pending admission, response headers, streamed
body reads and pending listings without retrying the cancellation.

Signed-range acceleration is used only when no object-store read wrapper is
required. Routed reads, caller admission/cancellation, and lifecycle observers
keep the canonical transport path; direct HTTP must not bypass their policy or
accounting. Attaching a signer later does not change that choice. Explicit URL
signing remains available, and unwrapped reads retain the accelerated path.
Signed responses require one valid `Content-Range` matching the requested
offsets before any payload is exposed. Missing, ambiguous, overflowing, or
wrong-offset headers are corruption even when the body length is correct.
Signed file extraction also takes the operation cancellation token. Network and
retry waits stop cooperatively; sibling failures cancel the other downloads and
all local writers flush before return. This is a drain-on-return contract, not
permission to drop the enclosing future before removing its private destination.

`Store::with_read_admission` adds caller-owned asynchronous GET/HEAD/listing admission
after read-route configuration. The supplied policy is shared by clones and
read routes, including access through `inner()`. It sees each facade retry,
reserves the advertised body size before payload polling, and rejects body
length mismatches. `ReadRejected` preserves the caller source and is fatal
through storage/cache/I/O wrappers. Admission and response-framing failures use
a non-retryable object_store envelope so SlateDB cannot retry them indefinitely.
The typed source remains authoritative: the Store facade still retries framing
corruption once, while admission rejection is terminal.
Each listing invocation charges one request; streaming listings defer admission
and backend construction until first poll.
Provider-internal pagination, listing response bytes and provider-internal HTTP
retries are outside this hook; writes retain their existing behavior.

Provider builders also bind `Store::target_identity` to credential-free
transport configuration: provider, bucket/container, effective endpoint and
addressing context. Endpoint URL host/port normalization preserves path case;
credentials, query strings and fragments are rejected in endpoint URLs. GCS
service-account endpoint selection is pinned before the provider loads the
credentials, so file rotation cannot redirect an already identified target.
This digest is separate from the established `BucketIdentity` used for logical
cross-scheme comparison and cache keys. Raw `Store::new` wrappers have no target
identity; integrity callers must not infer one from their display text.

Official AWS S3 builders send an explicit SHA-256 upload checksum and expose
that provider-validated integrity evidence through `Store`. Custom S3
endpoints, GCS, Azure, URL-parsed stores, and raw `Store::new` wrappers remain
readback-required; endpoint names and ETags never qualify an immutable write.

Uploads divide existing byte buffers into zero-copy 8 MiB HTTP frames so
multi-GiB objects do not exceed macOS's vectored-write limit. S3 create-only
bodies of at least 32 MiB use multipart transfer with 16 MiB parts and at most
four active parts per built store, shared by its clones. Part planning grows
the part size when necessary to stay within 10,000 parts. Each part retries its
explicit index, preserving ordering after an uncertain response. Completion
carries `If-None-Match: *`; an occupied destination is never overwritten. The
scoped transport condition does not affect ordinary multipart overwrites,
small creates, or mutable CAS. Returned errors await abort; dropping an upload
schedules abort. Process crashes still require provider multipart-expiration
policy. Immutable readback and checksum contracts remain unchanged, and
same-content conflicts stream their hash verification without buffering
another complete body. Raw URL-parsed stores retain the dependency's own upload
behavior; Crab-built S3 providers own automatic conditional multipart selection.

Non-resumable multipart uploads use one bounded part queue with or without a
progress callback. Part and completion failures attempt abort before returning;
an abort failure does not replace the original error used for retry decisions.
Crab-built S3 multipart handles retry each part at its original explicit index,
including file-backed generated response-pack uploads during clone and fetch.
They share the four-part admission limit with automatic immutable uploads;
ordinary multipart completion retains overwrite semantics. Dropping an active
handle schedules abort without publishing it.
Durable resumable uploads keep their journal lease and recovery protocol.

Stream responses must match the requested range, including EOF clamping. Drain
the stream through EOF to validate its declared length: short or oversized bodies
return `CorruptObject`. Provider errors retain their existing classification.
File downloads share one bounded download path. Returned errors trigger a
best-effort removal of partial files; callers own cleanup if they cancel by
dropping the future. No check requires buffering the complete object.

## Error classification

Classification and source retention are separate contracts in the current
storage API:

| Mapped error | Retained provider source |
| --- | --- |
| `NetworkTransient`, `NotSupported`, `ObjectStore` | Original `object_store::Error` |
| `Throttled` | Original provider error when mapped; local admission failures have no source |
| `StateConflict`, `NotFound`, `Forbidden` | Object path, without the provider source |
| `NoCredentials` | Neither provider source nor object path |

Generic throttling detection currently examines display text; it does not
extract a typed HTTP status or `Retry-After` header. Other generic errors map
to `NetworkTransient`. Auth-specific classification is a separate helper;
callers must invoke it explicitly. See `src/error_map.rs` for classification
and `src/retry.rs` for retry decisions.

## Usage

```rust
use bytes::Bytes;
use crab_storage::{StorageProviderKind, StoreLayout, build_static_env_store};

async fn example() -> Result<(), Box<dyn std::error::Error>> {
    let store = build_static_env_store("models", StorageProviderKind::S3)?;
    let layout = StoreLayout::new(store.clone(), "team/repository".to_owned());

    store
        .put(
            &layout.repo_path("example.txt"),
            Bytes::from_static(b"hello"),
        )
        .await?;
    let (body, _etag) = store
        .get_with_etag(&layout.repo_path("example.txt"))
        .await?;
    assert_eq!(&body[..], b"hello");
    Ok(())
}
```

Use `build_object_store` or `build_object_store_with_endpoint` when credentials
are already resolved by [`crab-auth`](../crab-auth/README.md). Use `cas_update` for
mutable manifest/ref state and `range_get` or `get_stream` for large immutable
objects.

`ObjectStoreCredentials` and `AzureAuthorization` omit secrets from `Debug`
output. Their string fields still contain credential material; pass those
fields only to the provider construction boundary.

## Design boundaries

- Provider credentials are inputs; credential resolution belongs to
  [`crab-auth`](../crab-auth/README.md).
- Metadata schemas and indexes belong to
  [`crab-metadata`](../crab-metadata/README.md).
- Caching belongs to [`crab-cache-store`](../crab-cache-store/README.md), which wraps
  this facade without changing origin semantics.
- `Store::flush_staged_writes` is the publication barrier for protected pushes;
  finalizers must wait for it before committing metadata.

The crate enables the AWS, GCP, Azure, and filesystem `object_store` adapters
for its provider construction API. It has no Crab-specific runtime feature
flags; callers select the provider and optional behavior at the composition
boundary.

`build_explicit_store` consumes explicit credentials, endpoint and allow-HTTP
policy without provider environment overrides. It rejects default credential
chain selection and retains the built store's signing, multipart and target
identity capabilities. Azure logical identity retains account and container.
The existing environment-selection and grant-endpoint constructors retain their
caller policies; all share the provider construction implementation.

## Private maintenance spills

`scratch::ScratchSpace` owns bounded immutable local spill files. `temporary`
selects platform temporary storage; `create` accepts an existing private parent.
It admits live bytes before creation and verifies exact length, EOF and Blake3 on reads. References cannot cross workspace identities.
Catalog maintenance uses it to replace repeated remote joins with sorted local
reductions. This does not change foreground metadata reads or provider routing.

Every blocking operation pins its directory until the worker exits, even when its
awaiting future is dropped. `close` drains those workers before removing the root;
callers retain primary validation errors while reporting cleanup failures. Drop
performs best-effort off-thread cleanup after the last owner exits. No SlateDB,
SQLite, new dependency or additional configuration is introduced by this transport.

The `test-support` `CountingObjectStore::cancel_after_put` hook arms one
cancellation after a successful PUT beneath a selected component prefix. It
leaves failed writes and unrelated keys untouched, so publication tests can
separate immutable-preparation cancellation from cancellation after visibility.
