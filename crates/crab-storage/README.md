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
`StorageError` values.

The admission token cancels pending admission, response headers, streamed
body reads and pending listings without retrying the cancellation.

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

Non-resumable multipart uploads use one bounded part queue with or without a
progress callback. Part and completion failures attempt abort before returning;
an abort failure does not replace the original error used for retry decisions.
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
