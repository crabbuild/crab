# crab-storage

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

Provider builders also bind `Store::target_identity` to credential-free
transport configuration: provider, bucket/container, effective endpoint and
addressing context. Endpoint URL host/port normalization preserves path case;
credentials, query strings and fragments are rejected in endpoint URLs. GCS
service-account endpoint selection is pinned before the provider loads the
credentials, so file rotation cannot redirect an already identified target.
This digest is separate from the established `BucketIdentity` used for logical
cross-scheme comparison and cache keys. Raw `Store::new` wrappers have no target
identity; integrity callers must not infer one from their display text.

## Usage

```rust
use bytes::Bytes;
use crab_storage::{StoreLayout, StorageProviderKind, build_static_env_store};

let store = build_static_env_store("models", StorageProviderKind::S3)?;
let layout = StoreLayout::new(store.clone(), "team/repository".to_owned());

store
    .put(&layout.repo_path("example.txt"), Bytes::from_static(b"hello"))
    .await?;
let (body, _etag) = store.get_with_etag(&layout.repo_path("example.txt")).await?;
assert_eq!(&body[..], b"hello");
# Ok::<(), Box<dyn std::error::Error>>(())
```

Use `build_object_store` or `build_object_store_with_endpoint` when credentials
are already resolved by [`crab-auth`](../crab-auth/README.md). Use `cas_update` for
mutable manifest/ref state and `range_get` or `get_stream` for large immutable
objects.

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
