# crab-cache-store

`crab-cache-store` is the read-through cache adapter between Crab's canonical
[`crab-storage`](../crab-storage/README.md) `Store` and higher-level readers.
It gives fetch, hydrate, VFS, and service code one store-shaped interface with
local disk caching, optional remote cache-service access, range reads, and
origin fallback.

Local disk retention defaults to unlimited (`CacheConfig::max_bytes = None`).
An explicit cap is propagated to both the local object cache and the shared
hydrator's decoded-range cache. This does not remove per-request memory limits,
change immutable/mutable path classification, or alter remote service capacity.

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
origin order. Ordinary object reads can warm local cache; remote warming is
part of the write path. Mutable paths bypass caches so refs and manifests
retain real origin ETags for compare-and-swap.

Xorb chunks and metadata share the source-specific reader in `src/xorb_read.rs`.
One attempt never mixes cache and origin bytes. Corrupt or unreadable cache
entries are bypassed even when eviction fails; corrupt origin bytes return
`CacheStoreError::OriginIntegrity` with their verification error retained.
Transport retry policy remains owned by `crab-storage`.

`CacheStoreError::Cache` and `Storage` preserve the domain error itself as
`Error::source()`, including source-free failures such as access denial.
Display text stays unchanged. Reconstruction consumers can classify the typed
failure without parsing text or mistaking an origin SDK's nested I/O for an
output-writer failure.

`xorb_chunk_metadata` verifies bounded footer/metadata ranges and the xorb
identity without reading payload. Selective chunk reads verify the requested
payload ranges. High-coverage reads may install a verified complete xorb, but
do not publish any local placement metadata. Hydration's
`get_xorb_chunks_without_install` reads a bounded complete body and installs
no duplicate full xorb; decoded-range caching belongs to `crab-read`'s runtime.

The process-local xorb result cache retains at most 4,096 entries and charges
up to 64 MiB for owned result buffers, offsets, both range-key copies, and entry
structures. It copies retained slices so a few requested bytes cannot pin an
entire serialized xorb. Oversized keys/results, empty results, and failed
fallible reservations skip this optional cache without changing the verified
read result. Hash-table/queue allocation slack is separate bounded bookkeeping;
this is not a whole-process RSS or reconstruction-memory limit. Caller-owned
results, transient decode buffers, and queued work need their own admission.

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
  reconstruction; this crate supplies verified xorb bytes and metadata.
- [`crab-coordination`](../crab-coordination/README.md) owns mutable write
  authority, which cached reads never replace.
