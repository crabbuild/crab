# PB-Scale Repository Technical Design

## Document Metadata

| Field | Value |
|-------|-------|
| Project | Crab |
| Status | Design proposal |
| Scope | Repositories from tens of TB to multiple PB, with TB-scale individual files |
| Primary surfaces | `crab/src/metadata/metadb/`, `crab/src/cmd/add_push_plan.rs`, `crates/crab-staging/`, `crab/src/git/push.rs`, `crab/src/storage/`, `crab/src/coordination/`, `crates/crab-cache-server/` |
| Last updated | 2026-07-01 |

## Executive Summary

Crab's current architecture is already aligned with PB-scale storage in one
important way: large immutable data lives in object storage as content-addressed
xorbs, and mutable state is constrained to small CAS-updated objects. The main
scale risk is not xorb payload storage. The risk is metadata cardinality,
metadata mutation, add-time local duplication, object enumeration, and
read-path fanout.

The current layout uses one bucket-global `chunk_index_db` and one per-repo
`file_index_db`. That is workable for GB/TB repositories and moderate
concurrency, but a PB repository with 64 KiB average chunks can contain roughly
17 billion chunks. A single global metadata database becomes too large to
compact, recover, cache, and mutate predictably. A single reconstruction shard
or a flat `file_hash -> shard_hash` value also becomes too coarse for TB-scale
files.

This design introduces a v2 repository layout with:

1. Partitioned metadata databases for chunk, xorb, file, and recipe state.
2. A recipe-tree reconstruction model for TB-scale files.
3. A miss-only authoritative add-stage store, where prepared xorbs and remote
   chunk proofs can replace raw staging-segment duplication.
4. Inventory-driven GC and fsck, with live object listing only as a fallback for
   small repos and diagnostics.
5. A production PB deployment profile that treats the cache service as an
   origin-shielding and dedup-accelerating tier while keeping object storage as
   the source of truth.

The design preserves Crab's core invariant: all immutable data and metadata
needed to reconstruct content are durable before any ref moves. Partial metadata
writes are allowed to be ahead of refs; they must be idempotent, recoverable,
and safe for retry.

## Goals

- Support one repository prefix at 1 PB logical content and at least 10 PB
  bucket-global dedup scope.
- Support individual Crab-tracked files from tens of GB to tens of TB without
  requiring a single giant reconstruction shard.
- Keep `crab add` bounded by disk read, chunking, compression, and metadata
  proof cost, not by duplicate local writes.
- Keep duplicate-content `crab add` and `git push` from re-packing or
  re-uploading content that is already proven remote.
- Keep object-store LIST operations out of the steady-state PB path.
- Preserve serverless correctness: object storage remains authoritative, and
  optional services remain rebuildable accelerators.
- Provide migration and production gates strong enough to ship incrementally.

## Non-Goals

- This does not propose a new VCS or a Git protocol server.
- This does not make a cache service authoritative for repository state.
- This does not require cross-organization global dedup. Dedup scope remains a
  bucket, bucket prefix, or explicitly configured policy domain.
- This does not preserve indefinite runtime compatibility between every v1 and
  v2 internal shape. Compatibility is selected by repository layout version.
- This does not make bucket-wide destructive GC a default operation.

## Current State

### Data Plane

Large file content is content-defined chunked, compressed into xorbs, and stored
as immutable content-addressed objects. Current documentation describes xorbs as
roughly 64 MiB chunk aggregates with individually compressed chunks and metadata
that supports range extraction. Source: `crab/docs/architecture/storage-layer.md`.

The current chunker targets 64 KiB average chunks, with 8 KiB minimum and
128 KiB maximum chunks. Source:
`crab/docs/architecture/engine-chunking-dedup.md`.

### Metadata Plane

The current metadata subsystem uses:

- `file_index_db`, one SlateDB per repository, mapping
  `file_hash -> shard_hash`.
- `chunk_index_db`, one bucket-global SlateDB, mapping
  `chunk_hash -> XorbRef`.

Both databases are wrapped by `MetaDb`, which opens at most one file index and
one chunk index per session. Writes are split into per-database write batches
and committed in parallel. There is no cross-database transaction; the push
manifest and ref CAS are the user-visible linearization points. Sources:
`crab/docs/architecture/metadata-subsystem.md`,
`crab/src/metadata/metadb/mod.rs`.

### Add and Push

`crab add` streams files into local staging, builds add-time push plans, writes
Crab pointer blobs, and updates the Git index. Current staging stores raw chunks
in append-only segment files plus SQLite rows. Add-time plans can include
remote-existing chunks and prepared xorbs, but the authoritative staging store
is still the segment/index pair for reconstruction, rollback, status, and
fallback push packing. Source: `crab/docs/design/add.md`,
`crates/crab-staging/src/push_plan.rs`.

`git push` must make all immutable data durable before refs move. The current
pipeline can adopt add-time plans, upload xorbs, write shards and MetaDB
entries, upload Git packs, then CAS manifests and refs. Source:
`crab/docs/design/push.md`.

### Cache Service

The cache service is optional. It caches immutable `.crab/*` objects, supports a
dedup query endpoint, and remains a performance optimization. In `cache+dedup`
mode, verified cache-local proof can let push skip duplicate xorb uploads.
Source: `crab/docs/architecture/caching-architecture.md`.

## Scale Model

The following model uses the current 64 KiB chunk target and current 64 MiB
xorb target. PB means 10^15 bytes. PiB means 2^50 bytes.

| Quantity | 1 PB | 1 PiB | Notes |
|----------|------|-------|-------|
| Chunks at 64 KiB average | 15.3 billion | 17.2 billion | `total_bytes / 65536` |
| Xorbs at 64 MiB target | 14.9 million | 16.8 million | `total_bytes / 67108864` |
| Xorbs at 256 MiB target | 3.7 million | 4.2 million | Fewer objects, less fine-grained cache eviction |
| Raw chunk-index bytes | 1.1 TB | 1.3 TB | 33-byte key plus 40-byte value before LSM overhead |
| Practical chunk-index footprint | 2.5-6 TB | 3-7 TB | Depends on compaction, filters, metadata, and versions |

A recent 21 GiB live add/push verification had 350,407 chunks, an observed
average of about 64.35 KiB per chunk, and 339 prepared xorbs before duplicate
remote proof was fixed. That confirms the scale model is close enough for
capacity planning.

The conclusion is direct: PB support is primarily a metadata and operations
problem. The data plane can hold PB-scale immutable objects, but the current
single global chunk index and flat file-to-shard mapping need a v2 layout.

## Production Targets

These are release gates, not promises for every workstation or network.

| Area | Target |
|------|--------|
| First-ingest add | Sustained throughput bounded by local read + CDC + compression, with no avoidable second full payload write |
| Duplicate add | No prepared xorb writes for chunks proven remote; planning throughput at least 5 million chunks/minute with cache service |
| Push | Upload-limited for new data; zero duplicate xorb upload for chunks proven by remote metadata or verified cache service |
| Metadata lookup | Batched vector lookup, grouped by partition; p95 per 1 million chunk classifications tracked by benchmark |
| Hydrate | Coalesced xorb range reads sorted by xorb; no per-chunk independent GET loop for sequential files |
| GC | Inventory-driven mark/sweep with dry-run, grace windows, and resumable journals |
| Recovery | Crash at any phase leaves either no user-visible change or a retryable push with no dangling ref |

## Design Principles

1. Object storage is authoritative. Cache service state can always be rebuilt.
2. Immutable content is written once and addressed by content identity.
3. Mutable refs and manifests use conditional writes.
4. Metadata may be ahead of refs, but refs must never be ahead of required data.
5. Partitioning is by content hash, not by path or branch.
6. Directory listing is not a control plane at PB scale.
7. Add-stage stores only bytes that are still needed locally.
8. Versioning is explicit. A repository opens either v1 or v2 layout based on
   remote configuration.
9. Migration tooling may dual-write for a bounded migration window; normal
   runtime code must not grow indefinite fallback stacks.

## v2 Remote Layout

The v2 layout separates global dedup state from repo-local Git state and adds
hash fanout everywhere object count can reach millions.

```text
s3://{bucket}/
|-- .crab/
|   `-- v2/
|       |-- layout.json
|       |-- chunk-index/p={partition}/chunk_index_db/
|       |-- xorb-catalog/p={partition}/xorb_catalog_db/
|       |-- xorbs/{hh}/{hh}/{xorb_hash}
|       |-- recipes/{hh}/{hh}/{recipe_hash}
|       |-- recipe-catalog/p={partition}/recipe_catalog_db/
|       |-- inventories/{provider}/{generation}/
|       `-- gc-journals/{generation}/
`-- {repo_prefix}/
    `-- v2/
        |-- repo.json
        |-- file-index/p={partition}/file_index_db/
        |-- refs/
        |-- packs/
        |-- manifests/
        `-- locks/
```

`layout.json` is the v2 source of truth for:

- layout version
- partition bits
- chunk profile
- xorb target profile
- recipe-tree profile
- created-at generation
- feature gates enabled for the repository

Clients do not discover partitions by LIST. They derive partition paths from
`layout.json`.

## Partitioning Strategy

### Partition ID

All high-cardinality metadata uses the leading bits of the relevant hash:

```text
chunk_index partition = prefix_bits(chunk_hash, layout.partition_bits)
xorb_catalog partition = prefix_bits(xorb_hash, layout.partition_bits)
file_index partition = prefix_bits(file_hash, repo.partition_bits)
recipe_catalog partition = prefix_bits(recipe_hash, layout.partition_bits)
```

The default should be 8 bits for production PB repos, yielding 256 partitions.
At 1 PiB and 64 KiB chunks, that is roughly 67 million chunk-index entries per
partition before dedup. Layouts above 10 PB can use 10 or 12 bits after
benchmarking open-handle and compaction behavior.

### Handle Pool

Clients must not open every partition eagerly. Add, push, hydrate, fsck, and GC
use a `PartitionHandlePool`:

- max open remote DB handles, default 32
- max concurrent partition operations, default 16
- LRU close for inactive handles
- explicit `close_all` on every exit path
- metrics per partition for open latency, read latency, write latency,
  compaction backlog, and CAS conflicts

### Batch Routing

Batch lookup APIs preserve input order but group work by partition internally:

```rust
struct PartitionedChunkIndex {
    router: MetadataPartitionRouter,
    handles: PartitionHandlePool<ChunkIndexPartition>,
}

impl PartitionedChunkIndex {
    async fn get_batch(&self, hashes: &[MerkleHash]) -> Result<Vec<Option<XorbRef>>>;
    async fn put_batch(&self, entries: &[ChunkIndexEntry], txn: &mut MultiDbTransaction)
        -> Result<()>;
}
```

The important contract is not the exact type shape. The contract is:

- callers issue vector operations
- the router groups by partition
- the result is aligned to caller order
- remote reads and local cache fills are bounded
- partition failures identify the partition and original input range

## Metadata Databases

### `chunk_index_db`

Purpose: `chunk_hash -> XorbRef`.

v2 changes:

- one SlateDB per partition
- grouped batch read/write
- per-partition `sys:format_version`, `sys:epoch`, and `sys:gc_generation`
- per-partition compaction and health metrics
- no range scans on content keys for classification

The value remains small and fixed-width when possible:

```text
chunk_hash -> xorb_hash || chunk_index || uncompressed_size || flags
```

Flags are reserved for future integrity or tiering state. The initial v2 can
keep the current 40-byte value if the partition path alone carries the version.

### `xorb_catalog_db`

Purpose: xorb-level existence and operational metadata.

```text
xorb_hash -> {
  bytes,
  chunk_count,
  storage_path,
  compression_profile,
  created_at_generation,
  indexed_generation,
  producer_id,
  integrity_state
}
```

The catalog prevents millions of HEAD requests during push and fsck. A catalog
entry is written only after the xorb object is durably created or verified.
Corruption handling still verifies content when bytes are read.

The catalog is not a replacement for `chunk_index_db`. It answers "does this
xorb object exist and where is it?" The chunk index answers "which xorb contains
this chunk?"

### `file_index_db`

Purpose: `file_hash -> FileRecipeRoot`.

The current 32-byte `file_hash -> shard_hash` value is too narrow for TB-scale
files. v2 uses a versioned value:

```text
file_hash -> {
  version,
  file_size,
  chunk_count,
  chunk_profile_id,
  recipe_root_hash,
  recipe_root_kind,
  optional_inline_recipe
}
```

Small files can still point to one recipe shard. Large files point to a recipe
tree root.

### `recipe_catalog_db`

Purpose: recipe object existence, size, and tree metadata.

```text
recipe_hash -> {
  bytes,
  recipe_kind,
  covered_chunk_count,
  covered_byte_count,
  child_count,
  created_at_generation
}
```

Recipe objects are immutable and content-addressed. They are stored under
`.crab/v2/recipes/{hh}/{hh}/{recipe_hash}`.

## Recipe-Tree Reconstruction

The current flat shard model is efficient for moderate files but becomes
dangerous for TB-scale files because a single file can have hundreds of
millions of chunks. v2 reconstructs files from recipe trees.

### Recipe Objects

```text
LeafRecipe {
  file_hash,
  chunk_start,
  chunk_count,
  terms: [ChunkTerm]
}

BranchRecipe {
  file_hash,
  chunk_start,
  chunk_count,
  children: [RecipeChild]
}

RecipeChild {
  chunk_start,
  chunk_count,
  byte_start,
  byte_count,
  recipe_hash
}

ChunkTerm {
  chunk_hash,
  uncompressed_size,
  xorb_hash,
  chunk_index
}
```

Leaf targets:

- 64 Ki to 256 Ki terms per leaf, tuned by benchmark
- bounded serialized size, default target 8-32 MiB
- contiguous file chunk ranges

Branch targets:

- shallow tree, usually depth 1 or 2
- children sorted by `chunk_start`
- byte ranges included so hydrate can seek without scanning all leaves

### Read Path

Hydrate and smudge should:

1. Read pointer file.
2. Resolve `file_hash -> FileRecipeRoot`.
3. Fetch only recipe leaves covering requested byte ranges.
4. Group terms by `xorb_hash`.
5. Coalesce adjacent or nearby ranges inside each xorb.
6. Fetch xorb ranges through `CachingStore`.
7. Verify chunks and assemble output in file order.

This prevents a 10 TB file read from first downloading a multi-GB recipe shard.

### Pointer Format

For v2 repos, pointer blobs should use a v2 pointer field:

```text
version https://crab.dev/spec/v2
file-hash <hex>
size <decimal>
recipe-hint <hex>
chunk-profile <id>
```

`recipe-hint` is optional. The authoritative mapping remains `file_index_db`.
The hint only removes one metadata lookup when present and fresh.

## Authoritative Add-Stage Store

### Problem

Current add-time xorb packing is a performance optimization. Segment rows remain
the authoritative staging store. For large adds, that creates avoidable local
I/O:

- raw chunks are written to segment files
- prepared xorbs may also be written
- push reads the same bytes again unless it can adopt the plan

At PB scale and even at tens of GB, this local duplication can dominate add
throughput.

### v2 Design

The v2 add-stage store promotes the verified add-time plan into the
authoritative staging record.

```text
.crab/staging-v2/
|-- manifests/
|   `-- files/{file_hash}.json
|-- xorbs/
|   `-- {hh}/{xorb_hash}.xorb
|-- segments/
|   `-- residual-{id}.seg
|-- index.db
`-- tmp/
```

The file manifest is the authoritative local staging record:

```text
FileStageManifest {
  version,
  file_hash,
  file_size,
  chunk_profile_id,
  chunk_count,
  source_stat,
  chunks: [StageChunkRef],
  prepared_xorbs: [PreparedXorbRef],
  remote_proofs: [RemoteChunkProof],
  integrity: StageIntegrity
}

StageChunkRef =
  RemoteExisting { chunk_hash, xorb_ref, proof_generation }
| PreparedXorb { chunk_hash, xorb_hash, chunk_index, uncompressed_size }
| Segment { chunk_hash, segment_id, offset, size, chunk_index }
```

### Add Flow

1. Stream file once.
2. Compute file hash and CDC chunks.
3. Query remote or cache-service dedup in vector batches.
4. For remote-existing chunks, record `RemoteExisting` and do not write local
   chunk bytes.
5. For new chunks, feed the streaming xorb packer.
6. Seal prepared xorbs to staging-v2 xorb files and fsync them.
7. Spill only residual chunks that cannot yet be packed.
8. Verify the complete chunk sequence and prepared xorb placements.
9. Atomically publish `FileStageManifest`.
10. Only after the manifest is durable, write the Git pointer and update the Git
    index.

### Push Flow

Push no longer treats add-time prepared xorbs as an optional hint. It treats a
valid `FileStageManifest` as the canonical source of staged content.

For each chunk:

- `RemoteExisting`: validate remote proof freshness according to configured
  generation policy; if stale, re-query.
- `PreparedXorb`: upload or verify the prepared xorb object, then write
  `xorb_catalog_db` and `chunk_index_db`.
- `Segment`: pack residual chunks or fail if the segment is missing/corrupt.

If a manifest fails validation, push does not silently reconstruct from partial
state. It returns a staged-content error and tells the user to re-run `crab add`
for the affected paths.

### Correctness Contract

The pointer publication boundary moves from "raw segment rows are durable" to
"the file stage manifest and every referenced local xorb/segment are durable and
verified." This is a product-visible contract and must be covered by tests.

## Remote Dedup Classification

PB-scale dedup classification has three modes.

### Serverless Baseline

Clients query partitioned `chunk_index_db` directly:

- group by chunk-index partition
- issue large vector reads per partition
- fill local persistent cache on hits
- treat misses as new chunks

This is correct and serverless, but it may not meet PB production throughput for
very large duplicate adds because remote metadata reads can dominate.

### Cache-Service Accelerated

Production PB deployments should enable cache service in `cache+dedup` mode.
The service maintains a local verified chunk-to-xorb index from cached recipes
and shards and answers vector dedup queries over local SSD.

Required improvements:

- partition-aware service index
- vector query endpoint that accepts at least 100,000 hashes/request
- streaming response for large requests
- negative result caching by partition generation
- per-tenant dedup-scope enforcement
- origin verification path when service mode is `dedup`
- cache-local proof path when service mode is `cache+dedup`

The service still cannot create authoritative metadata. Push writes origin xorb
objects and origin metadata before ref movement.

### Local Snapshot Assisted

For heavy users, clients can maintain a local partition snapshot:

- local SQLite or RocksDB chunk index seeded by recent pushes and hydrate reads
- optional partition Bloom filters downloaded from cache service
- invalidation by `sys:gc_generation`

This improves repeated workflows but cannot be required for correctness.

## Push Commit Model

The v2 push commit still follows the fail-forward model:

1. Acquire or validate intended destination refs according to existing push
   policy.
2. Validate stage manifests for all pointer files.
3. Upload or verify prepared xorbs.
4. Write `xorb_catalog_db` entries.
5. Write partitioned `chunk_index_db` entries.
6. Write recipe objects and `recipe_catalog_db` entries.
7. Write partitioned `file_index_db` entries.
8. Upload Git pack.
9. CAS manifests.
10. CAS refs.
11. Retire local staging and warm cache.

There is still no cross-partition transaction. The rule is:

- metadata may be ahead of refs
- refs never move until all required metadata and immutable objects are durable
- every metadata write is idempotent for the same key/value
- conflicting key/value writes are corruption errors, not last-writer-wins

Partial success examples:

| Failure point | Required behavior |
|---------------|-------------------|
| Crash after xorb upload before catalog | Orphan xorb, preserved by grace period, upload can be retried or verified |
| Crash after catalog before chunk index | Catalog ahead of index, safe; retry fills chunk index |
| Crash after partial chunk-index partition writes | Some chunks globally visible, safe if xorb durable; retry fills missing partitions |
| Crash after recipe write before file index | Recipe orphan, safe; retry writes file index |
| Crash after file index before ref CAS | File metadata ahead of Git refs, safe; not user-visible until ref moves |
| Crash after ref CAS | Push is successful if required data is durable |

## GC and Fsck

### Problem

Live object listing is too expensive and too slow as the primary GC mechanism at
PB scale. Listing millions of xorbs and recipes also competes with foreground
traffic and creates provider-specific failure behavior.

### Inventory-Driven Model

Production PB GC consumes scheduled object inventory reports:

- AWS S3 Inventory
- Google Cloud Storage Inventory Reports
- Azure Blob Storage Inventory

The inventory report is the object candidate set. Crab reachability metadata is
the root set.

### GC Phases

1. Create a GC generation and journal.
2. Load provider inventory for `.crab/v2/*` and repo prefixes.
3. Build reachable roots:
   - current refs and manifests
   - Git packs reachable from refs
   - `file_index_db` entries reachable from pointer blobs
   - recipe trees reachable from file roots
   - xorbs reachable from recipes
   - chunk-index referenced xorbs newer than the cutoff
   - active push leases and recent staging upload records
4. Compute candidates older than the grace window.
5. Dry-run report by object class, bytes, age, and sample paths.
6. Delete in resumable batches with provider-native batch/delete APIs where
   available.
7. Advance per-partition `sys:gc_generation`.
8. Emit a signed GC journal and update local cache invalidation generation.

### GC Safety Rules

- Never delete objects newer than the minimum grace period.
- Never delete an xorb referenced by any live recipe or by chunk index entries
  that have not been explicitly removed.
- Never use a partial inventory generation as a delete source.
- Never let cache service state define reachability.
- Bucket-scope destructive GC requires an explicit production runbook and
  separate approval. The CLI default remains repo-scoped or dry-run.

## Object Store and Provider Contracts

Crab v2 relies on two provider capabilities.

### Conditional Writes

Refs, manifests, locks, and layout objects require conditional update or create.
Current official provider documentation supports the needed primitives:

- AWS S3 conditional writes:
  https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html
- Google Cloud Storage request preconditions:
  https://docs.cloud.google.com/storage/docs/request-preconditions
- Azure Blob Storage conditional headers:
  https://learn.microsoft.com/en-us/rest/api/storageservices/specifying-conditional-headers-for-blob-service-operations

### Scheduled Inventory

PB operations require scheduled object inventory rather than live recursive
LIST:

- AWS S3 Inventory:
  https://docs.aws.amazon.com/AmazonS3/latest/userguide/storage-inventory.html
- Google Cloud Storage inventory reports:
  https://docs.cloud.google.com/storage/docs/insights/inventory-reports
- Azure Blob Storage inventory:
  https://learn.microsoft.com/en-us/azure/storage/blobs/blob-inventory

Provider-specific inventory setup belongs in deployment runbooks, not inside
normal push/add code paths.

## Cache Service Production Profile

The cache service becomes part of the recommended PB deployment, but not the
source of truth.

### Responsibilities

- Serve immutable xorb, recipe, pack, and SlateDB object reads from local disk.
- Coalesce cold range misses.
- Maintain verified chunk-to-xorb dedup indexes.
- Answer vector dedup queries with bounded cardinality metrics.
- Warm from push after origin durability.
- Expose low-cardinality Prometheus metrics.
- Enforce dedup visibility policy by tenant, bucket prefix, and repo prefix.

### Non-Responsibilities

- It does not move refs.
- It does not decide reachability for GC.
- It does not accept authoritative metadata writes.
- It does not replace object-store conditional writes.

### Deployment Shape

For PB production:

- one cache-service cluster per region and trust domain
- local NVMe or high-IOPS SSD for xorb and index cache
- service partitioning by hash range for dedup index ownership
- health-checked client fallback to origin reads
- cache rebuild from origin recipes, shards, and inventory
- separate cache instances when dedup visibility must be isolated

## Configuration

New config should be explicit and narrow.

```toml
[scale]
layout_version = 2
profile = "pb"

[metadata]
partition_bits = 8
max_open_partitions = 32
max_concurrent_partition_reads = 16
max_concurrent_partition_writes = 8

[engine]
chunk_profile = "cdc-64k-v1"
xorb_target_bytes = 268435456

[staging]
format = "manifest-v2"
authoritative_push_plans = true
residual_segment_limit_bytes = 1073741824

[gc]
mode = "inventory"
minimum_grace_hours = 168
require_dry_run = true

[cache]
service_mode = "cache+dedup"
push_warming = true
```

`chunk_profile` is immutable after first ingest for a repo unless a migration
tool explicitly rewrites recipes and metadata. A PB repo may choose a larger
average chunk profile after benchmarking, but the default remains compatible
with current 64 KiB CDC behavior.

## Observability

PB support is not production-ready without phase-level and partition-level
measurements.

Required client metrics:

- add bytes/sec by phase: scan, chunk, dedup lookup, pack, flush, index
- add chunks/sec and chunks classified/sec
- prepared xorb bytes, residual segment bytes, remote-existing bytes
- per-partition metadata read/write latency
- partition handle opens, closes, and close failures
- cache-service dedup hit, miss, stale-proof, and fallback counts
- push upload bytes, verified-existing bytes, skipped-upload bytes
- recipe tree depth, recipe bytes per file, hydrate recipe leaves fetched
- xorb range GET count and coalescing ratio
- GC inventory generation, candidate bytes, deleted bytes, retained bytes
- local cache invalidations by `gc_generation`

Required service metrics:

- vector dedup query size and latency
- dedup index entries by partition
- cache bytes by object class
- origin fetch bytes avoided
- cold miss coalescing count
- corrupt object detections
- authorization rejects by action, without high-cardinality principal labels

## Failure Handling

| Failure | Detection | Recovery |
|---------|-----------|----------|
| Add crash before file manifest publish | Missing manifest or temp entries | Sweep `tmp/` and partial xorbs not referenced by any manifest |
| Add crash after manifest before pointer publish | Manifest exists, Git index lacks pointer | `crab status` reports staged-only entry; user can publish or retire |
| Prepared xorb corruption | Payload hash or xorb metadata mismatch | Reject manifest or push; require re-add |
| Remote proof stale after GC | Proof generation older than allowed window | Re-query `chunk_index_db`; if missing, treat as new only if local bytes exist |
| Metadata partition write failure | Write receipt has missing partition | Abort before ref CAS; retry idempotently |
| Cache service returns stale placement | Origin verification fails or cache proof generation invalid | Ignore service result and query origin metadata |
| Recipe tree child missing | Recipe catalog or object read miss | Hydrate/push fails with missing recipe error; fsck reports root |
| GC inventory incomplete | Inventory manifest missing expected files | Abort delete phase |
| Ref CAS conflict | Conditional write failure | Retry or report non-fast-forward according to push policy |

## Migration Plan

### New Repositories

New PB repositories should be created directly in v2 layout:

```text
crab init --layout v2 --scale-profile pb crab://bucket/repo
```

The remote `repo.json` and `.crab/v2/layout.json` select v2 behavior. No v1
fallback is attempted for that repository.

### Existing Repositories

Migration is an explicit tool, not normal runtime fallback:

```text
crab migrate layout-v2 --repo crab://bucket/repo --dry-run
crab migrate layout-v2 --repo crab://bucket/repo --write
crab migrate layout-v2 --repo crab://bucket/repo --verify
crab migrate layout-v2 --repo crab://bucket/repo --cutover
```

Phases:

1. Dry-run inventory and capacity estimate.
2. Build v2 xorb catalog from existing xorb objects and shard metadata.
3. Build partitioned chunk index from current chunk index or shards.
4. Build recipe objects from existing shards.
5. Build v2 file index from current `file_index_db`.
6. Verify a sample plus full hash-count reconciliation.
7. Freeze writes or acquire migration lock.
8. Cut over repo layout config with conditional write.
9. Keep v1 data for rollback through the retention window.
10. Delete v1 data only after explicit GC approval.

Dual-write is allowed only inside the migration tool or a clearly bounded
cutover window.

## Implementation Plan

### Phase 0: Measurement and Reproduction

Deliverables:

- `crab bench scale-model` for synthetic chunk, recipe, and metadata cardinality.
- `crab bench add-large` for local files from 10 GiB to 10 TiB.
- JSONL aggregation script for add/push phase metrics.
- Fault-injection harness for add-stage and push commit phases.
- Production reference hardware profile for throughput gates.

Exit gate:

- Current v1 behavior measured for first-ingest, duplicate add, push, hydrate,
  and metadata lookup on at least 1 TB synthetic data.

### Phase 1: v2 Metadata Partition Router

Deliverables:

- `MetadataPartitionRouter`.
- `PartitionHandlePool`.
- Partitioned chunk index and file index interfaces.
- `MultiDbTransaction` that records per-partition write batches.
- Per-partition close and metrics coverage.
- Unit and integration tests for ordered batch routing and partial failures.

Exit gate:

- Existing v1 tests pass.
- v2 test repository can write and read partitioned chunk/file metadata.
- Crash during partition writes is retryable and never moves refs.

### Phase 2: Xorb Catalog

Deliverables:

- Partitioned `xorb_catalog_db`.
- Catalog writes after xorb create or verification.
- Push path uses catalog to avoid HEAD storms.
- Fsck verifies catalog/object consistency.

Exit gate:

- Push of existing xorbs performs O(partitions) metadata proof, not O(xorbs)
  origin HEADs.

### Phase 3: Recipe Trees and File Index v2

Deliverables:

- Recipe object format.
- Recipe tree builder in push.
- File index v2 value codec.
- Hydrate/smudge recipe-tree reader.
- Pointer v2 parser and writer.

Exit gate:

- A synthetic 10 TB file can be represented by bounded recipe leaves and
  reconstructed by range without loading the entire recipe tree into memory.

### Phase 4: Authoritative Add-Stage Store

Deliverables:

- `staging-v2` manifest format.
- Streaming prepared-xorb writer.
- Residual segment path for unpacked chunks.
- Push adoption that treats valid file manifests as authoritative.
- Retire/clean/status support for stage manifests.

Exit gate:

- Duplicate add writes no prepared xorbs and no raw chunk segments for
  remote-existing chunks.
- New add writes prepared xorbs once and does not duplicate the same bytes into
  raw segments except residual chunks.
- Crash tests cover every publish boundary.

### Phase 5: Cache-Service PB Profile

Deliverables:

- Partition-aware dedup index.
- Large vector query endpoint.
- Streaming response support.
- Generation-aware negative cache.
- Origin verification fallback.
- Tenant/dedup-scope policy tests.

Exit gate:

- Duplicate add classification reaches target throughput on a 10 TB synthetic
  dataset with cache service enabled.

### Phase 6: Inventory GC and Fsck

Deliverables:

- Provider inventory readers for S3, GCS, Azure.
- Inventory generation validation.
- Reachability graph builder over refs, recipes, file index, chunk index, and
  catalogs.
- Dry-run reports and resumable delete journals.
- Per-partition `gc_generation` advancement.

Exit gate:

- GC can process a synthetic PB inventory without live recursive LIST.
- Fault injection proves interrupted GC resumes without deleting live objects.

### Phase 7: Migration and Production Rollout

Deliverables:

- `crab migrate layout-v2`.
- v1-to-v2 verification reports.
- Canary runbook.
- Rollback runbook.
- Operator dashboards and alerts.
- Documentation for provider inventory setup and cache-service deployment.

Exit gate:

- One internal multi-TB repo migrated and operated for a retention window.
- One PB synthetic inventory and metadata simulation completed.
- Production readiness checklist signed off.

## Test Strategy

### Unit Tests

- Partition router maps hashes deterministically.
- Batch routing preserves input order.
- File index v2 codec rejects wrong lengths and unknown versions.
- Recipe tree coverage is contiguous and complete.
- Stage manifest cannot reference missing prepared xorbs.
- Prepared xorb payload hash and metadata hash are verified.
- Conflicting metadata writes fail.

### Property Tests

- Random file chunk sequences produce recipe trees that cover every chunk once.
- Random partition failures are retryable without ref movement.
- Stage manifest publish is atomic with respect to visible staged files.
- GC never marks reachable recipes or xorbs as candidates.

### Integration Tests

- Local object store and RustFS/S3-compatible object store for v2 repos.
- Add -> commit -> push -> clone -> hydrate byte-identical checks.
- Crash after each push phase.
- Cache service stale/missing/corrupt proof behavior.
- Inventory GC dry-run and delete with resumable journal.

### Live Validation

Required before production:

- 10 TB first-ingest add and push.
- 10 TB duplicate add and push across two repos in the same dedup scope.
- 100 TB metadata-only simulation with generated chunk/file/recipe keys.
- 1 PB inventory simulation using provider-format inventory files.
- Multi-client concurrent push to different refs and same ref.
- Hydrate of sparse byte ranges from a TB-scale file.

## Production Readiness Checklist

- v2 layout creation is explicit and documented.
- v2 repos do not silently fall back to v1 metadata.
- All metadata handles close on normal return, error, cancellation, and panic.
- Ref movement is impossible before required v2 data and metadata are durable.
- Add-stage v2 has crash recovery and status visibility.
- Cache service outage degrades performance, not correctness.
- Inventory GC defaults to dry-run and has a signed journal.
- Provider inventory configuration is documented per cloud.
- Dashboards exist for add, push, hydrate, metadata, cache service, and GC.
- Alerts exist for corruption, missing recipes, partition write failures,
  compaction backlog, cache proof rejection spikes, and GC inventory lag.
- Migration has dry-run, verify, cutover, rollback, and post-retention cleanup.
- Benchmarks meet release gates on reference hardware.

## Risks and Open Questions

| Risk | Mitigation |
|------|------------|
| Metadata footprint remains large at 64 KiB chunks | Support immutable chunk profiles; benchmark 128 KiB and 256 KiB profiles for PB repos |
| Too many partition handles for large batch classification | Handle pool, larger per-partition batches, cache-service vector query |
| Recipe tree format duplicates existing shard concepts | Keep v2 recipe reader/writer local to metadata boundary; migrate old shards through tooling |
| Cache service becomes operationally required for PB SLOs | Document it as required for PB performance, optional for correctness |
| Inventory lag delays GC | Use storage lifecycle tiering for cost pressure; keep GC grace explicit |
| Dedup visibility leaks content existence across tenants | Enforce explicit dedup scopes and recommend separate buckets/cache services for isolation |
| Migration is expensive | Make v2 default for new PB repos; migrate only repos that need PB behavior |

## Best-Fix Assessment

The best production fix is not simply increasing upload concurrency or xorb
size. Those help data-plane throughput but do not address the PB failure modes:
billions of chunk-index entries, giant reconstruction metadata, duplicate
add-stage writes, and inventory-scale operations.

The best fix is a v2 layout that partitions metadata, promotes verified
add-time plans into the authoritative staging store, introduces recipe trees for
large files, and moves GC/fsck to scheduled inventory inputs. This keeps Crab's
serverless correctness model while allowing internal infrastructure to add
cache-service acceleration where PB performance requires it.
