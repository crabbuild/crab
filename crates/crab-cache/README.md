# crab-cache

`crab-cache` provides the cache contracts and implementations shared by
Crab's read paths. It supports hash-verified local disk caching, an optional
HTTP cache-service client, path classification, service capabilities, dedup
queries, prefetch profiles, and Xet-specific chunk hints.

## Why it exists

Crab reads immutable shards, xorbs, chunks, manifests, and workflow artifacts
repeatedly. A cache must be safe to trust: content-addressed objects need
integrity verification, manifests need freshness handling, and an unavailable
remote cache must not become an availability dependency. Keeping those rules
here lets `crab-read`, VFS, fetch, and workflow code use the same behavior.

## Architecture

```text
origin object store
        │
        ├── LocalCache       atomic files, hash checks, LRU limits
        └── CacheClient      optional authenticated HTTP service
                │
                └── capabilities, immutable GET/HEAD/range, dedup query
```

`CacheKey` distinguishes chunks, shards, xorbs, manifests, and stage entries.
`LocalCache::get_or_fetch` coalesces fills, verifies content-addressed data
on read and write, and atomically renames completed files. Manifest entries
use ETags rather than a content hash.

The remote client is intentionally optional. The service contracts describe
auth modes, cache/dedup modes, limits, health, and known chunks without making
every local consumer depend on `reqwest`.

## Usage

Enable the local implementation and cache a verified chunk:

```toml
[dependencies]
crab-cache = { version = "1", features = ["local-cache"] }
```

```rust
use bytes::Bytes;
use crab_cache::{CacheKey, LocalCache};
use crab_xet::hash::compute_data_hash;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let cache = LocalCache::new(".cache/crab".into());
let payload = Bytes::from_static(b"cached chunk");
let hash = compute_data_hash(payload.as_ref());

let result = cache
    .get_or_fetch(&CacheKey::Chunk(hash), || async {
        Ok::<_, crab_cache::CacheError>(payload.clone())
    })
    .await?;
assert_eq!(result, payload);
# Ok(())
# }
```

For a cache service, enable `remote-client` and construct `CacheClient` with
the deployment's PSK, bearer, or mTLS settings. Call `is_healthy` and
`capabilities` before using service-specific features.

## Boundaries

- [`crab-cache-store`](../crab-cache-store/README.md) composes these cache
  primitives with an origin `Store` and owns fallback behavior.
- [`crab-storage`](../crab-storage/README.md) remains the source of truth;
  cache entries are disposable and must never weaken origin integrity checks.
- [`crab-read`](../crab-read/README.md) owns reconstruction and shard
  completeness, while this crate owns object reuse.
