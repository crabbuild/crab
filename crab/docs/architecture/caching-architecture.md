# Caching Architecture

Crab has two cache layers:

1. A client-side local cache under `~/.cache/crab` or `CRAB_CACHE_DIR`.
2. An optional organization cache service, `crab-cache-server`, configured
   through `[cache].service_url`.

Both layers are performance optimizations. The object store remains the
source of truth, and every cache path is designed to degrade to origin reads
or normal uploads when the cache is unavailable.

## Implementation Map

| Area | Source | Responsibility |
|------|--------|----------------|
| Local cache root | `crates/crab-cache/src/root.rs` | Resolves `CRAB_CACHE_DIR` or `~/.cache/crab`. |
| Local object cache | `crates/crab-cache/src/local_cache.rs` | Stores shards, xorbs, manifests, stage cache entries, and legacy cache directories kept for cleanup/stats. |
| xet-core chunk cache | `crab/src/cache/xet_chunk_cache.rs` | Shared decompressed chunk cache used by smudge, hydrate, and prefetch. |
| Store wrapper | `crab/src/cache/caching_store.rs` | Routes immutable reads through local cache, cache service, then origin. |
| Path classifier | `crates/crab-cache/src/path_class.rs` | Shared immutable/mutable path contract for client and server. |
| HTTP cache client | `crates/crab-cache/src/cache_client.rs` | Calls `/v1/health`, `/v1/{path}`, and `/v1/dedup/query`. |
| Cache service router | `crates/crab-cache-server/src/state.rs` | Mounts authenticated object/admin/dedup routes plus public health/metrics routes. |
| Cache service store | `crates/crab-cache-server/src/cache_store.rs` | Persists objects and metadata, tracks byte budget, and evicts by weighted LRU. |
| Dedup index | `crates/crab-cache-server/src/chunk_index.rs` | Maps chunk hash to xorb location from cached production shards. |
| Cache service metrics | `crates/crab-cache-server/src/metrics.rs` | Tracks Prometheus metrics and admin traffic counters for origin avoidance. |

## Client-Side Cache

The client-side cache is always available. It is constructed even when the
remote cache service is not configured.

```text
{CRAB_CACHE_DIR:-~/.cache/crab}/
├── chunks/{hh}/{hash}        xet-core decompressed chunk cache
├── shards/{hh}/{hash}        shard blobs
├── xorbs/{hh}/{hash}         optional local xorb cache entries
├── manifests/{name}.json     manifest body
├── manifests/{name}.etag     manifest ETag
└── stages/{hh}/{hash}        workflow stage cache entries
```

Hash-keyed directories use a two-character fanout based on the first two hex
digits. Writes use temporary files followed by rename.

### Verification Rules

`LocalCache` verifies chunks and shards with `compute_data_hash` on read. If
the bytes do not match the key, the cache entry is removed and the caller
falls back to its fetch path.

Xorbs and workflow stage entries are keyed by domain identities rather than by
`blake3(body)`. Local xorb reads verify the aggregate xorb identity from the
serialized xorb metadata; stage entries are validated by downstream workflow
logic. Current file-index state is read from the SlateDB-backed
`file_index_db`; it is not represented as standalone local cache objects.

### Local Cache Consumers

| Workflow | Local cache use |
|----------|-----------------|
| `crab add` / clean filter | Uses repo-local staging and fast-path metadata; no object-store upload. |
| `crab push` | Queries repo-scoped cache-service dedup before the remote `chunk_index_db`, trusts only cache-verified xorb placements, opens MetaDb and shard proof reads over the cache-aware object store, lazy-loads the advisory commit graph only for shallow-client FF fallback, uses lock and manifest write CAS tokens without extra body reads, then warms immutable objects after origin durability is settled. |
| `crab fetch` / `git fetch` | Uses `CachingStore` for immutable object reads and direct origin for refs. |
| `git checkout` / smudge | Resolves file-index through `file_index_db`, then reads shards and xorbs through `CachingStore`. |
| `crab hydrate` | Uses the shared xet-core chunk cache and local metadata cache. |
| `crab diff` | Resolves reconstruction terms from `file_index_db` and cached shard data. |
| FUSE / VFS | Reuses the xet-core chunk cache for on-demand materialization. |

## Remote Cache Service

The remote service is enabled when the resolved config includes:

```toml
[cache]
service_url = "https://crab-cache.internal:8443"
service_mode = "cache+dedup"   # cache | dedup | cache+dedup
push_warming = true
```

`CachingStore::try_build_healthy` creates a `CacheClient`, probes
`GET {service_url}/v1/health`, and disables only the remote service leg if
the probe fails. The local cache remains active.

### Client Read Path

Immutable full-object reads follow this order:

```text
LocalCache
  -> cache service GET /v1/{object-store-path}
  -> origin object store
  -> LocalCache write-back
```

Mutable paths such as refs, `HEAD`, config, locks, and manifests go directly
to origin. They are never cached by the service.

Git fetch pack bodies and companion indexes use the same immutable route, but
the client streams them directly to the temporary pack file. This keeps a
large-clone cache hit off the Rust heap and enforces the manifest's pack-size
commitment before Git installs the result. A cache miss or cache-service error
falls back to the origin stream without changing the correctness boundary.

Range reads ask the cache service first when service caching is enabled. The
server serves warm ranges directly from cache. On a cold range miss, the server
fetches and caches the full immutable object from origin, then returns the
requested slice so Crab clients do not bypass the shared cache for cold ranges.
Immutable `HEAD` requests to the cache service return metadata only. Warm hits
are answered from local cache state with `X-Cache: HIT`; misses may consult
origin metadata and return `X-Cache: MISS`.

### Client Write Path

Normal object writes still go to the origin first. Push warming is a
best-effort side effect after origin durability is settled:

```text
origin PUT or existing-object verification
  -> local cache write when the object has a local cache key
  -> cache service PUT /v1/{object-store-path} without another origin write
```

Failures in the cache service path are logged and do not fail the push.

### Object Path Contract

The service treats `/v1/{path}` as the object-store key. The canonical Crab
CLI paths are global `.crab/*` objects:

| Object | Canonical path |
|--------|----------------|
| Xorb | `.crab/xorbs/{64-hex-id}` |
| Shard | `.crab/shards/{64-hex-id}` |

Git pack paths are accepted under:

```text
{repo-prefix}/packs/{pack-name}.pack
{repo-prefix}/packs/{pack-name}.idx
{repo-prefix}/packs/{pack-name}.meta
```

Xorb and shard paths require a 64-character hex identity. Pack names do not;
the service derives an internal blake3 storage ID from the object type and URL
pack name so reads, metadata, and eviction use the same canonical key.

Versioned SlateDB metadata objects are cacheable:

```text
{repo-prefix}/file_index_db/wal/{wal-id}.sst
{repo-prefix}/file_index_db/compacted/{sst-id}.sst
{repo-prefix}/file_index_db/manifest/{manifest-id}.manifest
{repo-prefix}/file_index_db/compactions/{compactions-id}.compactions
.crab/chunk_index_db/wal/{wal-id}.sst
.crab/chunk_index_db/compacted/{sst-id}.sst
.crab/chunk_index_db/manifest/{manifest-id}.manifest
.crab/chunk_index_db/compactions/{compactions-id}.compactions
```

The cache key for metadata is a hash of the full object-store path. SlateDB
directory/list discovery remains mutable and goes direct to origin.

## Cache Service Storage

```text
{cache_root}/
├── cache.sqlite
├── xorbs/{hh}/{id}
├── shards/{hh}/{id}
├── packs/{hh}/{internal-id}
└── metadata/{hh}/{path-id}
```

`cache.sqlite` stores object type, size, last access, access count, cached-at
time, and the persistent chunk-to-xorb dedup index. Byte accounting is
idempotent: replacing an existing object updates the current byte count by the
net size delta, not by the full incoming size.
If an object metadata row outlives its on-disk cache file, the next full or
range read miss removes the stale row before returning a miss so the byte
budget cannot stay inflated.

Eviction is weighted LRU:

| Object type | Relative eviction order |
|-------------|-------------------------|
| Xorbs | First |
| Packs / pack indexes | Second |
| Shards | Last |

Shards are kept longest because they repopulate the dedup index.

## Cache Service Observability

`GET /v1/admin/stats` returns cache inventory plus aggregate traffic counters.
The `traffic` object includes cache hits, cache misses, body origin fetch
count and bytes, metadata origin HEAD request count, coalesced misses, bytes
served from cache versus origin, and current in-flight miss fills. These
counters are intentionally low-cardinality so operators can use them safely
across large repositories.

## Dedup Index

`crab-cache-server` maintains a persistent chunk-to-xorb index in
`cache.sqlite`. The index is populated when shard objects are cached.

Current ingestion supports production shard files through `ShardReader` and
`MDBShardInfo::read_all_xorb_blocks_full`. For every xorb chunk in a shard,
the service stores:

```text
chunk_hash -> xorb_hash, chunk_byte_range_start, unpacked_segment_bytes
```

On startup, the service recursively scans `{cache_root}/shards/{hh}/{hash}`
and rebuilds the SQLite chunk-index table from production Crab shard files.
Corrupt or unparseable shard files are skipped with warnings.

## Authorization Notes

For `.crab/*` global objects, including `.crab/xorbs`,
`.crab/shards`, and `.crab/chunk_index_db`, the service uses `.crab` as the
synthetic repo path for policy checks. A policy that wants to allow push
warming and read-through caching for normal Crab CLI traffic must include
`.crab` or `*` in `repos`.

`POST /v1/dedup/query` carries the caller's repo prefix. The service checks
the authenticated principal's `dedup` action against that repo path and then
enforces `dedup.scope`. `all` accepts any repo, `bucket-prefix:<prefix>` accepts
that prefix and nested repos, and `repos:<repo1>,<repo2>` accepts the listed
repo prefixes. Known responses include `xorb_hash`, `chunk_index`, `length`,
and `cache_verified: true`; `xorb_hash` is the canonical hash used in
`.crab/xorbs/{first-two-hex}/{hash}` paths, and `chunk_index` is the chunk's ordinal position
in xorb metadata, not a byte offset. The service returns indexed chunks as
unknown unless the referenced xorb is locally cached and verifies against the
requested chunk. The CLI treats `cache_verified: true` as cache-local proof,
and the publish contract is mode-specific. In `cache+dedup`, that verified
cache proof is authoritative and push skips duplicate xorb uploads without
reading origin object storage. In `dedup`, the service is only an index assist,
so push still verifies the returned xorb placement against origin before
skipping upload. Use separate cache instances when dedup visibility must be
isolated by team, tenant, environment, or regulatory boundary.

## Known Gaps And Opportunities

| Area | Status | Opportunity |
|------|--------|-------------|
| Native mTLS client cert validation | Server auth currently trusts `X-Client-CN` from a TLS terminator. | Wire `tls.client_ca_path` into rustls client-auth validation or document proxy-only mTLS as the product contract. |
| Per-object service observability | Metrics are aggregated by object type. | Add repo/path-class labels only if cardinality can be bounded. |
| Client service URL env override | Only auth secrets have env overrides. | Consider a controlled `CRAB_CACHE_SERVICE_URL` override for CI images that should not edit TOML. |

## Invariants

1. Mutable paths are never cached.
2. Cache service failures do not fail ordinary client operations.
3. Push warming is best-effort and happens only after origin success.
4. `.crab/*` is the canonical CLI object path family.
5. Service byte accounting is based on net growth and must remain idempotent.
6. Service object metadata must not outlive missing cache files after a read
   miss observes the drift.
7. Shard ingestion failure must not reject the cached shard object; it only
   reduces dedup effectiveness until the shard can be parsed.
