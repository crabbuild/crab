# Crab Cache Architecture

This document describes how caching works across every stage of the crab
workflow — from `git add` through `git push`, `git fetch/pull`, `git checkout`,
`crab hydrate`, `crab dehydrate`, `crab diff`, and FUSE mount. It covers
the three cache tiers, what data each tier stores, and how they interact.

## Cache Tiers

Crab uses a three-tier caching architecture. Every tier is optional and
degrades gracefully — a missing tier simply means more S3 round-trips.

```
┌─────────────────────────────────────────────────────────┐
│  Tier 1: In-Memory Caches (per-session, per-process)    │
│  ┌──────────────┐  ┌──────────────┐  ┌───────────────┐  │
│  │  ChunkCache  │  │  ChunkIndex  │  │  Bloom Filter │  │
│  │  (smudge/VFS)│  │  (push dedup)│  │  (clean fast) │  │
│  └──────────────┘  └──────────────┘  └───────────────┘  │
├─────────────────────────────────────────────────────────┤
│  Tier 2: Local Disk Cache (persistent, shared)          │
│  ~/.cache/crab/                                         │
│  ├── shards/{h[:2]}/{h}      hash-verified, LRU-evicted │
│  ├── xorbs/{h[:2]}/{h}       warmed after successful push │
│  ├── manifests/{name}.json   ETag-freshened manifests   │
│  ├── bloom.bin               persisted bloom filter     │
│  ├── chunks/{h[:2]}/{h}      (smudge/hydrate/FUSE only) │
│  ├── buckets/{bucket}/chunk-index.sqlite    dedup tier  │
│  └── profile/                tracing flame graphs       │
│                                                         │
│  {repo}/.crab/staging/     (per-repo, not in cache)     │
│  ├── index.db                SQLite chunk/file metadata │
│  └── segments/{id}.seg       compressed chunk data      │
├─────────────────────────────────────────────────────────┤
│  Tier 3: Remote Cache Service (optional, health-gated)  │
│  HTTP REST at config.cache.service_url                  │
│  GET/PUT immutable objects, POST dedup queries          │
└─────────────────────────────────────────────────────────┘
```

### Tier 1: In-Memory Caches

Short-lived, per-process caches that avoid redundant computation within a
single command invocation.

| Cache | Scope | Contents | Used By |
|-------|-------|----------|---------|
| `ChunkCache` | Per-session `Arc` | Decompressed chunk bytes, keyed by blake3 hash. In-memory LRU with configurable budget (default 4 GiB). Verify-once pattern: blake3 checked on first read, skipped on subsequent reads. | Smudge, hydrate, FUSE, delta reconstruction |
| `ChunkIndex` | Per-push | chunk_hash → (xorb_hash, chunk_index, uncompressed_size). Populated from shards during push step 3. ~40 bytes/entry, 1 GiB ceiling. | Push step 4 (A/B/C classification) |
| `FileHashBloom` | Per-filter-process session | Bit-vector bloom filter of known file hashes. ~1% FPR with k=3. Loaded from disk at session start, saved on exit. | Clean fast path |
| Xorb in-memory cache | Per-hydrate batch | Full xorb bytes keyed by hash. Avoids re-downloading the same xorb when multiple files share chunks. Cleared between batches. | `ShardHydrator` |

### Tier 1.5: Local Persistent Stores

These are not caches in the traditional sense — they are authoritative local
data stores that survive across sessions. They sit between the in-memory
caches and the disk cache.

#### Staging Area (`{repo}/.crab/staging/`)

The staging area lives **inside the git repository** (not in the cache
directory). It is the local chunk store used during `git add` / `crab add`.
It consists of:

- **SQLite index** (`index.db`) — the metadata backbone:
  - `files`: file_hash → shard_hash, total_bytes, created_at
  - `segments`: segment_id → sealed_at, size_bytes, chunk_count, live_chunk_count
  - `chunks`: (file_hash, chunk_index) → chunk_hash, size, segment_id, segment_offset
  - `pending_chunks`: same schema as `chunks`, buffered before flush
  - `file_paths`: optional file_hash → worktree path metadata for UX
  - `file_recipes` and `recipe_occurrences`: immutable verified file recipes
  - `recipe_remote_chunks`: proof-bearing committed remote authority
  - `prepared_payloads`: global local xorb identity, digest, and byte count
  - `prepared_payload_chunks`: one canonical prepared placement per chunk hash
  - `prepared_leases`: many-to-one recipe ownership of prepared payloads
  - `add_preparations` and `prepared_chunk_claims`: temporary cross-file ownership
  - `staging_meta`: key-value store for layout_version and other metadata
- **Segment files** (`segments/{id}.seg`) — append-only binary files containing
  compressed chunk data. Each segment is sealed when it reaches a size threshold.
- **Prepared xorb payload files**
  (`push-plans/payloads/<first-two>/<xorb-hash>.xorb`) — immutable local
  pending authority produced at add time and shared across every recipe lease.

The staging area provides:
- `stage_chunks_batch()` — write positioned chunks to the current segment + index
- `chunks_for_file(file_hash)` — return all chunk hashes for a file version
- `get_chunk(hash)` — read a chunk by hash (segment_id + offset from index)
- `batch_dedup_check()` — check which chunks already exist in staging
- `write_file_push_plan()` — normalize a verified runtime authority DTO into
  recipe remote/prepared relationships
- `load_file_push_plan()` — derive a runtime DTO from the published recipe's
  normalized authority rows
- `load_prepared_xorb_cache_for_chunks()` — load prepared xorb candidates by
  wanted chunk hash from the indexed candidate table
- `compact()` — merge segments with high dead-chunk ratios
- `sweep_orphans()` — remove files/chunks not referenced by any pointer

The staging area is opened with an exclusive flock during `git add` (write
path) and a shared flock during `git push` (read-only path). Stale locks
from crashed processes are detected via PID liveness checks.

Prepared authority is normalized rather than owned by a file or serialized
plan. `prepared_payload_chunks.chunk_hash` is unique, so partial overlaps and
sequential adds reuse one placement. Recipe leases and push snapshots retain a
body globally. Direct-prepared chunks may have no segment copy; a missing or
corrupt body therefore fails closed unless the exact recipe independently has
complete verified segment authority. Writable-open recovery removes unindexed
content-addressed bodies and abandoned stream temps.

#### PersistentChunkIndex (`chunk-index.sqlite`)

A SQLite-backed persistent dedup index. It lives in the bucket-scoped cache
directory as `chunk-index.sqlite` so dedup is immediately effective across
repositories and sessions without rebuilding from shards. Three tables:

- `chunks_v1`: chunk_hash (32 bytes) → xorb_hash (32 bytes) + chunk_index (4 bytes LE) + uncompressed_size (4 bytes LE)
- `shards_v1`: shard_hash (32 bytes) → presence marker, avoiding redundant shard installs
- `meta_v1`: string key → string value (schema version and cache GC generation)

Each shard install is a single SQLite transaction. WAL mode allows
concurrent readers with one serialized writer, matching the push path's
point-lookups plus shard-atomic installs. The process reuses one shared
handle per index path to avoid duplicate writer queues and WAL checkpoint
churn in long-lived daemons.

The push path wires this index through shard sync and `MetaDbGuard`, so
classification can use the warm SQLite tier before falling back to remote
metadata. Corrupt or pre-SQLite cache files are recreated because canonical
metadata remains in remote shard metadata and `chunk_index_db`.

#### Push State (`.crab/push-state.json`)

Tracks the last-pushed SHA per ref per remote. Used by the native push
pipeline for incremental pointer walks — only commits reachable from the
new tip but not from the last-pushed SHA are scanned for pointers. Not a
cache, but reduces the work done by step 1 (enumerate pointers).

### Tier 2: Local Disk Cache (`LocalCache`)

Persistent, shared across all crab commands. A single `LocalCache` instance
rooted at `~/.cache/crab/` (overridable via `CRAB_CACHE_DIR`).

All hash-keyed entries use a two-level directory layout `{type}/{h[:2]}/{h}`
for filesystem friendliness. Reads are hash-verified via `compute_data_hash`
(MerkleHash) on every access; corrupt entries are evicted and re-fetched.
Writes use atomic tempfile-then-rename for crash safety.

**Actual on-disk layout** (as of current implementation):

```
~/.cache/crab/
├── shards/{h[:2]}/{h}         ← shard blobs (push step 3 + step 9 warming)
├── xorbs/{h[:2]}/{h}          ← xorb blobs warmed after successful push
├── manifests/shard-list.json  ← shard-list manifest (ETag-freshened)
├── chunks/{h[:2]}/{h}         ← decompressed chunk data (only created by
│                                 ChunkCache when smudge/hydrate/FUSE runs)
├── buckets/{bucket}/chunk-index.sqlite
│                               ← bucket-global PersistentChunkIndex
└── bloom.bin                  ← persisted clean-filter bloom filter
```

| Object Type | Cache Key | Typical Size | Eviction |
|-------------|-----------|-------------|----------|
| Shards | `CacheKey::Shard(MerkleHash)` | 1–100 MiB | LRU by mtime, optional budget |
| Xorbs | `CacheKey::Xorb(MerkleHash)` | 64–128 MiB | Shared data-object LRU budget |
| Chunks | `CacheKey::Chunk(MerkleHash)` | ~128 KiB | LRU by mtime, 10 GiB default budget |
| Manifests | `CacheKey::Manifest { name, etag }` | 1–100 KiB | ETag freshness check |

The typed cache API has no file-index key. Current file-hash lookups open the
per-repo `file_index_db` through `FileIndexLookupSession`; file-index state is
not part of the local cache API.

The `default_cache_root()` function in `cache/mod.rs` is the single source of
truth for the cache path. Every command calls it instead of hard-coding paths.

Note: the staging area (`{repo}/.crab/staging/`) is **not** in the cache
directory — it lives inside the git repository because it contains data
specific to the current working tree (staged chunks awaiting push). The cache
directory contains hash-addressed shard/xorb bytes, manifest freshness data,
and per-repo SQLite warm indexes that accelerate subsequent operations.

### Tier 3: Remote Cache Service

An optional HTTP service that caches immutable objects closer to the client
(e.g. in the same region or on the same machine). Configured via
`config.cache.service_url`. The `CacheClient` provides:

- `GET /v1/{path}` — fetch an immutable object
- `GET /v1/{path}` with `Range` header — fetch a byte range
- `PUT /v1/{path}` — push-warm an object
- `POST /v1/dedup/query` — batch dedup query (chunk hashes → known/unknown)
- `GET /v1/health` — health check (2-second timeout); `/health` is a compatibility alias

When the service is unhealthy or not configured, all operations fall through
to the local cache and origin S3 transparently.

## CachingStore: The Unified Cache Wrapper

`CachingStore` wraps a raw `Store` (S3) with both local and remote caching.
It mirrors the `Store` interface so callers don't need to know whether caching
is active.

### Construction

```
CachingStore::try_build_healthy(store, &config)
```

Always returns `Some(CachingStore)` because the local cache is unconditional.
The remote cache client is only enabled when configured AND the `/v1/health`
endpoint responds 2xx within 2 seconds. If unhealthy, the remote client is
disabled but local caching stays active.

### Read Path (get_with_etag)

For immutable paths (`.crab/xorbs/`, `.crab/shards/`, repo pack files, and
versioned `file_index_db` / `chunk_index_db` objects):

```
1. Local disk cache lookup for canonical shards/xorbs (hash-verified)
   ↓ miss
2. Remote cache service GET (when configured)
   ↓ miss or error
3. Origin S3 GET
   ↓ success
4. Write-back to local disk cache
```

For mutable paths (`/refs/`, `/manifests/`, `/HEAD`): direct to origin S3.

### Write Path (put)

```
1. PUT to origin S3
2. PUT to local disk cache (immutable objects only)
3. PUT to remote cache service (push warming, when enabled)
```

### Path Classification

`path_class::classify_path()` determines mutability:

- **Immutable**: `.crab/xorbs/`, `.crab/shards/`, repo pack files, and
  versioned MetaDB objects
- **Mutable**: everything else (refs, manifests, config, locks)

`path_to_cache_key()` maps immutable S3 paths to `CacheKey` variants:

- `.crab/shards/{first-two-hex}/{hash}` → `CacheKey::Shard(hash)`
- `.crab/xorbs/{first-two-hex}/{hash}` → `CacheKey::Xorb(hash)`
- Repo packs and MetaDB objects may use the remote cache service, but they do
  not map to the global `LocalCache`.

---

## Cache Usage by Workflow Stage

### 1. `crab add` / `git add` (Clean Filter)

The clean path runs at `git add` time. It hashes file content via blake3,
CDC-chunks it, stages chunks locally, and emits a pointer blob. No S3 I/O
happens during clean.

**Caches involved:**

| Cache | Role |
|-------|------|
| `FileHashBloom` (Tier 1) | Session-scoped bloom filter of known file hashes. Loaded from `~/.cache/crab/bloom.bin` at filter-process startup. Consulted on each clean to decide whether the fast path is worth attempting. |
| Staging area (`{repo}/.crab/staging/`) | SQLite index (`index.db`) + segment files. Lives inside the repo, not in the cache directory. Chunks are CDC'd, compressed, and written to the current segment. The SQLite index records chunk_hash → (segment_id, offset) for later retrieval during push. The `batch_dedup_check()` method avoids re-staging chunks that already exist in the index. |
| `confirmed_hashes` (Tier 1) | `HashSet` of file hashes confirmed via HEAD to the file-index. Avoids redundant HEAD requests within a session. |

**Normal path (all files):**

1. Read file content → single-pass blake3 hash + CDC chunking
2. For each chunk: check staging index for dedup (`batch_dedup_check`)
3. Stage new chunks: compress → append to current segment → insert into SQLite
4. Register file in `files` table with file_hash and total_bytes
5. Run `git add -- <paths>` so the clean filter writes pointer blobs

**Fast path (files ≥ 64 MiB):**

1. Check bloom filter for file_hash
2. If bloom says "maybe known" → HEAD the file-index
3. If file-index confirms → emit pointer with shard-hint, skip staging
4. If bloom says "definitely not known" → stage chunks normally

**Bloom persistence:**

The bloom filter is saved to `~/.cache/crab/bloom.bin` when the
filter-process exits and loaded on the next invocation. This means the
second `git add` session benefits from the fast path immediately without
rebuilding from the file-index.

### 2. `git commit`

No cache interaction. Git commits the pointer blobs produced by the clean
filter.

### 3. `git push` / `crab push`

The push pipeline is a 14-step orchestrator. Cache interactions happen at
multiple steps:

**Step 3: Shard sync → ChunkIndex build**

```
Read shard-list manifest (cached via CacheKey::Manifest with ETag freshness)
    ↓
For each shard in the list:
    Check LocalCache for CacheKey::Shard(hash)
        ↓ hit: install from disk (no S3)
        ↓ miss: download from S3, write to LocalCache, install
    ↓
ChunkIndex populated with chunk→xorb mappings
```

**Step 4: Classify chunks (A/B/C)**

The `Classifier` categorizes each chunk into one of three classes:

- **Class A (Existing):** Already stored remotely. Found in the `ChunkIndex`
  (populated from shards in step 3), reported by the remote cache service's
  dedup query, or found in the global `chunk_index_db`, then verified against
  origin xorb metadata and payload before upload is skipped.
- **Class B (Staged/Seen):** Present in the local staging area or already
  seen earlier in this push session. Deduplicated within the push — packed
  once, not duplicated across xorbs.
- **Class C (New):** Not found anywhere. Must be packed into a new xorb and
  uploaded.

```
If CachingStore has remote cache service:
    Batch query all chunk hashes via POST /v1/dedup/query
    Chunks reported as "known" → origin xorb proof → class A
    ↓
For remaining chunks:
    Classify against ChunkIndex (from step 3)
    class A (existing in remote shards) → skip
    class B (in local staging, seen before) → skip
    class C (new) → mark for packing
```

The `ShardBloom` filter (per-shard bloom over chunk hashes and file hashes)
accelerates negative lookups during shard sync — if the bloom says a chunk
is definitely not in a shard, the shard can be skipped without parsing.

**Step 5: Pack xorbs from staging**

Reads class-C chunks from the staging area's segment files (via SQLite
index lookups) and packs them into xorbs using `XorbBuilder`. Each xorb
targets 64 MiB of compressed data. Chunks from the same source file
(`RunId`) are kept together for locality.

```
For each pointer's chunk list:
    Filter to class-C chunks only (from step 4)
    Read chunk data from staging: index.locate(hash) → segment + offset
    Feed to XorbBuilder → produces sealed Xorb objects
```

**Step 7: Upload xorbs**

Xorbs are uploaded to S3 via multipart (> 8 MiB) or single PUT. Upload keeps
cheap `Bytes` clones of successful payloads; step 13 warms those bytes into
the local xorb cache after the push succeeds.

**Step 9: Upload shards + MetaDB entries**

```
Upload shards to S3
Commit file_index_db and chunk_index_db entries
    ↓
Cache warming:
    Write each shard to LocalCache (CacheKey::Shard)
    Warm the per-repo PersistentChunkIndex SQLite tier
    PUT shards to remote cache service (push warming, when enabled)
```

Step 13 also warms uploaded xorb bytes into `LocalCache`. This means the very
next `hydrate` or `diff` after a push can reuse local shard/xorb bytes and the
SQLite chunk-index tier; file-hash lookups still read the per-repo
`file_index_db` so they cannot collide across repositories.

**Step 11: Manifest CAS (shard-list, pack-list)**

Shard-list and pack-list are CAS-updated on S3. The shard-list is cached
locally via `CacheKey::Manifest` with ETag freshness for subsequent reads.

### 4. `git fetch` / `git pull` / `git clone`

The remote helper fetches packs and refs from S3.

**Caches involved:**

| Cache | Role |
|-------|------|
| LocalCache (shards) | Shards synced during fetch are written to local cache |
| Git pack cache | `.git/objects/pack/` — standard git pack storage |

### 5. `git checkout` / Smudge Filter

The smudge path runs at `git checkout` time. It resolves a pointer blob back
to the original file content.

**Resolution chain:**

```
Parse pointer → file_hash, size, shard_hint
    ↓
Resolve shard:
    file_index_lookup.resolve(file_hash)
        → per-repo file_index_db lookup
    ↓
Load shard:
    shard_loader.load_reconstruction_terms(shard_hash, file_hash)
        → CachingStore.get_with_etag("shards/{hash}")
            → LocalCache hit? return cached shard bytes
            → Remote cache hit? return + write-back to local
            → S3 GET → write-back to local + remote
    ↓
Fetch xorb ranges:
    For each reconstruction term:
        Check ChunkCache (Tier 1) for chunk_hash
            → hit: use cached decompressed chunk
            → miss: Range GET from xorb on S3
                    blake3 verify
                    Store in ChunkCache
    ↓
Smudge gate: verify blake3(content) == file_hash && len == size
    ↓
Output reconstructed content
```

### 6. `crab hydrate`

CLI hydrate, eager/selective clone, clone profiles, post-pull hydration and
init auto-patterns share `cmd::hydrate::run_hydrate`, with an explicit root,
resolved configuration, restore flags and caller cancellation. Configured
repositories use `ShardHydrator`; only an absent remote selects local staging.

**SmudgeSessionHydrator** — retains local unpublished-staging reconstruction
for unconfigured repositories. Its low-level chunk-cache constructor does not
wire cloud dependencies and is not the CLI cloud hydration entry point.

**ShardHydrator** — direct shard-based reconstruction:

```
resolve_file_index(file_hash)
    → per-repo file_index_db lookup
    ↓
get_or_download_shard(shard_hash)
    → LocalCache.get_or_fetch(CacheKey::Shard(shard_hash), fetch_from_s3)
    ↓
Extract reconstruction terms from shard
    ↓
For each term:
    get_xorb(xorb_hash) → in-memory xorb cache (per-batch)
    Parse xorb → extract chunk range → reconstruct
    ↓
blake3 verify → atomic write to working tree
```

**Delta reconstruction** (when a previous version exists on disk):

```
Read existing file → compute base_hash
    ↓
Resolve base terms (same shard lookup chain as above)
Resolve target terms
    ↓
estimate_reuse_ratio(base_terms, target_terms)
    > 10%? → reconstruct_from_delta (reuse unchanged chunks from disk)
    ≤ 10%? → full reconstruction
```

### 7. `crab dehydrate`

No cache interaction. Dehydrate is write-only — it replaces hydrated files
with pointer blobs. No S3 access.

### 8. `crab diff` / `crab diff-driver`

The diff pipeline resolves reconstruction terms for both versions of each
file, then compares chunk-by-chunk.

**Caches involved:**

```
TermResolver:
    resolve_file_index(file_hash)
        → per-repo file_index_db lookup
    ↓
    get_or_download_shard(shard_hash)
        → LocalCache.get_or_fetch(CacheKey::Shard(shard_hash), ...)
    ↓
    In-memory shard dedup map (per-batch)
        → avoids re-downloading the same shard for multiple files
```

### 9. FUSE Mount (`crab mount`)

The VFS layer materializes files on demand.

**Caches involved:**

| Cache | Role |
|-------|------|
| `ChunkCache` (Tier 1+2) | Shared with smudge. Decompressed chunks cached in memory and on disk. |
| `OdbReader` blob cache | Small git blobs cached at `{blob_cache_dir}/{oid}` |
| LocalCache (shards) | Shard downloads cached for reconstruction |

---

## Configuration

### Cache Root

```
CRAB_CACHE_DIR=/path/to/cache   # env var override
~/.cache/crab/                   # default
```

All commands use `cache::default_cache_root()` which checks `CRAB_CACHE_DIR`
first, then falls back to `~/.cache/crab`.

### Cache Service

In `~/.config/crab/config.toml` or `.crab/local.toml`:

```toml
[cache]
service_url = "https://crab-cache.internal:8443"
service_mode = "cache+dedup"   # cache | dedup | cache+dedup
push_warming = true
```

### Cache Budgets

| Budget | Default | Controls |
|--------|---------|----------|
| Chunk cache (in-memory) | 4 GiB | `ChunkCache::open(dir, Some(max_bytes))` |
| Chunk cache (on-disk) | 10 GiB | `LocalCache` chunk LRU ceiling |
| Shard cache (on-disk) | Unlimited | Optional via `LocalCache::with_limits` |
| ChunkIndex (in-memory) | 1 GiB | ~26M entries before `over_ceiling()` |

---

## Cache Maintenance

### `crab prune`

Runs LRU eviction on the local cache. Evicts oldest chunks and shards until
the cache fits within configured budgets. Chunks and xorbs share the
data-object budget.

### `crab doctor`

Checks cache directory existence and reports basic stats.

### `crab cache verify`

Hash-verifies all cached chunks and shards. Corrupt entries are evicted.

### `crab cache clean`

Removes all cached data (chunks, shards, manifests).

---

## Invariants

1. **Hash verification on every read.** Local chunk and shard entries are
   verified with `compute_data_hash`; cached xorbs verify their serialized xorb
   identity. Corrupt entries are evicted and re-fetched transparently.

2. **Atomic writes.** All cache writes use tempfile-then-rename to prevent
   partial reads on crash or SIGINT.

3. **Immutable objects only.** Only content-addressed, immutable objects
   (shards, xorbs, chunks) are cached. Mutable objects (refs,
   manifests, locks) bypass the cache and go direct to S3.

4. **Graceful degradation.** Every cache tier is optional. Remote cache
   down? Local cache serves. Local cache empty? S3 serves. Cache corrupt?
   Evict and re-fetch. No cache failure is fatal.

5. **Shared cache root.** All commands share `~/.cache/crab/`. A shard
   downloaded during push is immediately available for hydrate, diff, FUSE,
   and vice versa.

---

## Data Flow Summary

```
                    ┌─────────────┐
                    │  git add    │
                    │  (clean)    │
                    └──────┬──────┘
                           │ CDC chunks → .crab/staging/
                           │   SQLite index + segment files
                           │ bloom filter consulted (Tier 1)
                           │ fast path: HEAD file-index (Tier 2)
                           ▼
                    ┌─────────────┐
                    │  git commit │  (no cache interaction)
                    └──────┬──────┘
                           │
                           ▼
              ┌────────────────────────┐
              │      git push          │
              │                        │
              │  Step 3: shard sync    │──→ LocalCache (shards)
              │         ↓ populate     │    + shard-list manifest
              │         ChunkIndex     │
              │  Step 4: classify      │──→ ChunkIndex + Remote dedup
              │         read staging   │    + staging SQLite dedup
              │  Step 5: pack xorbs    │──→ read chunks from staging
              │  Step 7: upload xorbs  │──→ S3 (no local cache)
              │  Step 9: upload meta   │──→ LocalCache + Remote warming
              │  Step 11: manifest CAS │──→ LocalCache (shard-list)
              └────────────────────────┘
                           │
                           ▼
              ┌────────────────────────┐
              │   git fetch / pull     │
              │                        │
              │  Packs → .git/objects/ │
              │  Shards → LocalCache   │
              └────────────────────────┘
                           │
                           ▼
              ┌────────────────────────┐
              │  git checkout (smudge) │
              │                        │
              │  file-index → Local    │
              │  shard → Local/Remote  │
              │  xorb chunks → Chunk   │
              │  Cache (Tier 1+2)      │
              └────────────────────────┘
                           │
                           ▼
              ┌────────────────────────┐
              │   crab hydrate         │
              │                        │
              │  Same as smudge, plus: │
              │  delta reconstruction  │
              │  xorb in-memory cache  │
              └────────────────────────┘
```

## Source Files

| File | Role |
|------|------|
| `cache/mod.rs` | Module root, `default_cache_root()` |
| `cache/local_cache.rs` | `LocalCache`, `CacheKey`, LRU eviction, hash verification |
| `cache/chunks.rs` | `ChunkCache` — in-memory LRU with on-disk persistence |
| `cache/caching_store.rs` | `CachingStore` — two-tier wrapper (local + remote) |
| `cache/cache_client.rs` | `CacheClient` — HTTP client for remote cache service |
| `cache/path_class.rs` | `classify_path()` — immutable vs mutable path routing |
| `crates/crab-staging/src/lib.rs` | `StagingArea` — local chunk store plus plan promotion/read APIs |
| `crates/crab-staging/src/index.rs` | SQLite index: files, segments, chunks, verified plans, prepared-xorb candidates |
| `crates/crab-staging/src/segment.rs` | Append-only segment files for compressed chunk data |
| `crates/crab-staging/src/push_plan.rs` | Add-time push plan model, prepared-xorb payload helpers, and diagnostics |
| `engine/dedup.rs` | `Classifier` — A/B/C chunk classification |
| `git/clean.rs` | `FileHashBloom` — bloom filter with save/load persistence |
| `git/filter_process.rs` | Bloom load at startup, save on exit |
| `git/push.rs` | Push pipeline cache integration (steps 3, 4, 7, 9, 11) |
| `git/push_state.rs` | `PushState` — incremental walk state per ref per remote |
| `git/smudge.rs` | Smudge pipeline with `ChunkCache` integration |
| `cmd/hydrate.rs` | `ShardHydrator` with `LocalCache` for shards + xorbs |
| `diff/term_resolver.rs` | `TermResolver` with `LocalCache` for shards + xorbs |
| `metadata/shard_sync.rs` | `ShardSynchronizer` — syncs shards into `LocalCache` |
| `crates/crab-xet/src/shard_bloom.rs` | `ShardBloom` — per-shard bloom filter for chunk/file hashes |
| `metadata/chunk_index.rs` | `ChunkIndex` — in-memory chunk→xorb dedup index |
| `metadata/persistent_chunk_index.rs` | `PersistentChunkIndex` — SQLite-backed warm dedup tier |
