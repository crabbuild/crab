# crab-cache-store

`crab-cache-store` is the read-through cache adapter between Crab's canonical
[`crab-storage`](../crab-storage/README.md) `Store` and higher-level readers.
It gives fetch, hydrate, VFS, and service code one store-shaped interface with
local disk caching, optional remote cache-service access, range reads, and
origin fallback.

## Why it exists

Caching policy should not be reimplemented at every call site. Immutable
objects can be reused safely after hash verification; mutable refs and
manifests must continue to use origin ETags and CAS. This wrapper owns that
classification and ensures a cache outage degrades to the origin store rather
than breaking reads.

## Architecture

```text
caller
  │ ObjectStore / get_with_etag / range_get
  ▼
CachingStore
  ├── LocalCache                 always enabled
  ├── CacheClient                optional remote service
  └── origin Store               authoritative fallback
```

Immutable paths are tried in local cache → remote cache (when configured) →
origin order. Successful origin reads warm local cache and, when enabled,
remote cache. Mutable paths bypass caches so refs and manifests retain real
origin ETags for compare-and-swap. Xorb range reads can use the local xorb
index without downloading the full object.

`CacheConfig` controls service URL, cache/dedup mode, push warming, and TLS or
client-authentication material. The `remote-client` feature is required when a
remote service URL is configured; local caching remains available without it.

## Usage

Compose an origin store and wrap it once at the read boundary:

```rust
use crab_auth::CloudCredentials;
use crab_auth_store::build_store_from_credentials;
use crab_cache_store::{CacheConfig, CachingStore};
use crab_types::storage::StorageProviderKind;
use object_store::path::Path;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let origin = build_store_from_credentials(
    "bucket",
    CloudCredentials::StaticEnv {
        provider: StorageProviderKind::S3,
    },
)?;
let cached = CachingStore::new(origin, CacheConfig::default())?;
let (bytes, _etag) = cached
    .get_with_etag(&Path::from("repositories/team/manifest"))
    .await?;
# let _ = bytes;
# Ok(())
# }
```

For an optional service, enable `remote-client` and set
`CacheConfig::service_url`, `service_mode`, and the deployment's auth/TLS
fields. Use `try_build_healthy` when constructing a best-effort client around
a service that may be unavailable at startup.

## Boundaries

- [`crab-cache`](../crab-cache/README.md) owns cache files and HTTP contracts;
  this crate owns store routing and fallback.
- [`crab-read`](../crab-read/README.md) owns shard completeness and full-file
  reconstruction; this crate only supplies bytes.
- [`crab-coordination`](../crab-coordination/README.md) owns mutable write
  authority, which cached reads never replace.
