# crab-read

`crab-read` is Crab's canonical read and hydration orchestration layer. It
turns a Git/Xet pointer into verified bytes by combining manifest metadata,
replica selection, cache-aware storage, shard coverage checks, and Xet file
reconstruction.

## Why it exists

Several product surfaces need the same read guarantees: fetch, hydrate, path
views, and the virtual filesystem. They must all reject unauthorized fetch
wants, select a readable replica, fetch every shard term, and verify the final
file hash. Centralizing that path prevents a surface-specific shortcut from
returning incomplete or unverified content.

## Architecture

```text
Git fetch wants
      │ manifest + admission policy
      ▼
fetch admission and hidden-ref filtering
      │
      ▼
replica readiness / routing policy
      │
      ▼
StoreClient + CachingStore
      │ manifest → file index → shard/xorb objects
      ▼
ShardHydrator → Xet reconstruction → whole-file BLAKE3 verification
```

`FetchAdmissionPolicy` defaults to allowing ref tips while rejecting arbitrary
object wants. It can also admit reachable objects and hide configured refs.
Replica selection reports readiness, generations, fallbacks, and routing
choices so callers can distinguish an unavailable replica from a corrupt
object.

`ShardHydrator` provides memory, file, and half-open byte-range reconstruction.
Before full-file reconstruction it checks that the pointer's shard terms are
covered; after reconstruction it verifies the requested file hash. A shared
adaptive concurrency controller and optional Xet chunk cache keep parallel
reads bounded.

## Usage

Wrap the origin in the cache adapter, create the repository layout, and reuse
one hydrator for a read session:

```rust
use crab_auth::CloudCredentials;
use crab_auth_store::build_store_from_credentials;
use crab_cache_store::{CacheConfig, CachingStore};
use crab_read::{ReadStoreLayout, ShardHydrator};
use crab_types::storage::StorageProviderKind;

# async fn example(pointer_bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
let origin = build_store_from_credentials(
    "bucket",
    CloudCredentials::StaticEnv {
        provider: StorageProviderKind::S3,
    },
)?;
let cached = CachingStore::new(origin.clone(), CacheConfig::default())?;
let layout = ReadStoreLayout::new(origin, "repositories/team/project".to_owned());
let hydrator = ShardHydrator::new(cached, layout, 16)?;
let bytes = hydrator.reconstruct_from_pointer(pointer_bytes).await?;
# let _ = bytes;
# Ok(())
# }
```

Use `reconstruct_range_from_pointer` for partial reads and
`reconstruct_to_path` for large files. The pointer and metadata remain the
source of truth; caches only change where immutable bytes are fetched from.

## Boundaries

- [`crab-metadata`](../crab-metadata/README.md) defines manifests, file
  indexes, and shard metadata; this crate consumes them.
- [`crab-cache-store`](../crab-cache-store/README.md) supplies cache-aware
  object access and origin fallback.
- [`crab-xet`](../crab-xet/README.md) owns pointer/chunk/shard mechanics; this
  crate owns the end-to-end read order and verification.
- [`crab-vfs`](../crab-vfs/README.md) and
  [`crab-auth-server`](../crab-auth-server/README.md) are callers, not
  alternate reconstruction implementations.
