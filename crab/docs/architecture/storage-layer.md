# Storage Layer

## Overview

The storage layer provides a unified abstraction over cloud object stores
(S3, GCS, Azure, R2, MinIO) and implements the binary formats, retry logic,
and upload strategies that all higher-level modules depend on.

Source: `crab/src/storage/`

## Store Abstraction

The `Store` struct wraps the `object_store` crate's trait object, adding
crab-specific conveniences:

```
Store
├── inner: Arc<dyn ObjectStore>     ← S3/GCS/Azure backend
├── get(path) → Bytes               ← download object
├── get_with_etag(path) → (Bytes, ETag)
├── put(path, bytes)                 ← upload object
├── put_opts(path, bytes, PutMode)   ← conditional write (CAS)
├── head(path) → ObjectMeta          ← existence + metadata check
├── list(prefix) → Stream<ObjectMeta> ← enumerate objects
└── delete(path)                     ← remove object
```

The `Store` is created once per CLI invocation from the parsed
`crab://bucket/repo` URL and reused for all operations.

## Remote Bucket Layout

The normative object-key contract, including cross-language construction
rules, is [Object Storage Layout V1](object-storage-layout.md). This page
describes storage mechanics and does not redefine that contract.

```
s3://{bucket}/
├── .crab/                              Bucket-global core
│   ├── xorbs/{hash}                    Immutable chunk aggregates
│   ├── shards/{hash}                   Immutable reconstruction metadata
│   ├── chunk_index_db/                 Shared chunk → xorb SlateDB
│   └── ref-registry                    Cross-repo reachability registry
└── {repo-path}/
    ├── manifest                        Authoritative mutable pointer
    ├── manifests/                      Immutable bulk and historical roots
    ├── metadata/{pack,shard}/          Immutable segmented inventories
    ├── packs/                          Immutable Git pack family
    ├── file_index_db/                  Per-repo file → shard SlateDB
    ├── git_locator_db/                 Derived Git object-range SlateDB
    ├── locks/                          Push and native file locks
    └── lfs/                            LFS objects and protocol locks
```

### Path Layout Notes

Xorbs and shards live under the placement's `global_prefix` (normally the
bucket-root `.crab`) without a `xet/` prefix, hash fan-out, or filename
extension. File-index records live inside the opaque per-repository
`file_index_db/` SlateDB; callers do not construct record object keys.

The single `{repo-path}/manifest` owns refs and points at content-addressed
segmented pack and shard inventories. The locator database is derived
acceleration; the manifest plus canonical `.pack`/`.idx` files remain the
correctness boundary.

LFS objects use two-level sharding: `{oid[:2]}/{oid[2:4]}/{oid}` for
compatibility with the standard LFS layout.

### Object Mutability

| Object | Mutable | Update Mechanism |
|--------|---------|------------------|
| xorbs, shards, packs | No | Content-addressed, write-once |
| file/chunk index databases | Mixed | SlateDB-owned protocol |
| manifest | Yes | CAS via `If-Match` ETag |
| segmented inventory objects, locator SSTs | No | Content-addressed/SlateDB publication |
| push locks | Yes | CAS create + TTL expiry |
| LFS locks | Yes | CAS create/delete |
| config | Yes (rarely) | CAS |

## Xorb Binary Format

Xorbs aggregate chunks into ~64 MiB content-addressed blobs:

```
┌──────────────────────────────────────────────────┐
│ Header                                           │
│   magic: "XORB" (4 bytes)                        │
│   version: u16                                   │
│   chunk_count: u32                               │
│   metadata_offset: u64                           │
├──────────────────────────────────────────────────┤
│ Chunk Data                                       │
│   for each chunk:                                │
│     length: u32                                  │
│     compressed_bytes: [u8; length]  (zstd)       │
├──────────────────────────────────────────────────┤
│ Metadata                                         │
│   for each chunk:                                │
│     hash: MerkleHash (32 bytes)                  │
│     uncompressed_length: u32                     │
│     offset_in_xorb: u64                          │
├──────────────────────────────────────────────────┤
│ Footer                                           │
│   hash_of_metadata: MerkleHash                   │
│   hash_of_xorb: MerkleHash  (content address)    │
└──────────────────────────────────────────────────┘
```

Key properties:
- Chunks are compressed individually with zstd (level 3 default), enabling
  Range GETs to extract specific chunks without decompressing the whole xorb.
- The xorb's content address is a MerkleHash over chunk hashes, not over
  compressed bytes. Same logical content → same hash regardless of compression
  level.
- Compatible with xet-core's xorb format.

Source: `crab/src/storage/xorb/`

## Retry Policy

The retry module classifies errors and applies exponential backoff:

| Error Class | Retry? | Examples |
|-------------|--------|----------|
| Transient | Yes | Network timeout, 5xx, throttle (429) |
| Permanent | No | 404 Not Found, 403 Access Denied |
| CAS conflict | Caller decides | 412 Precondition Failed |

Retry parameters:
- Base delay: 50ms
- Max delay: 500ms (capped)
- Max attempts: configurable (default 10 for CAS, 3 for data operations)
- Jitter: randomized to avoid thundering herd

Source: `crab/src/storage/retry.rs`

## Multipart Upload

Objects larger than 8 MiB use S3 multipart upload:
- Part size: 8 MiB
- Concurrent part uploads via `put_multipart` from `object_store`
- Partial state persisted in `MultipartRegistry` (SQLite) for resume across
  process restarts
- Abandoned multipart uploads detected by `crab fsck` and aborted

Source: `crab/src/storage/multipart_resume.rs`

## Batched HEAD Requests

The `head_batch` module provides concurrent HEAD requests for existence
checking. Used during push (step 6) to skip already-uploaded xorbs and
during LFS push to identify missing objects.

Source: `crab/src/storage/head_batch.rs`

## Error Mapping

The `error_map` module translates `object_store::Error` variants into
`CrabError` variants, preserving the original error as a source chain.
It also classifies authentication errors separately for better user-facing
messages.

Source: `crab/src/storage/error_map.rs`
