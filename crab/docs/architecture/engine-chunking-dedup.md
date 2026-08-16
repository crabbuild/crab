# Engine: Chunking & Dedup

## Overview

The engine module implements content-defined chunking (CDC), three-tier
deduplication, xorb packing, and the local staging area. It is the core data
processing pipeline that transforms file content into deduplicated, compressed
chunks ready for upload.

Source: `crab/src/engine/`

## Content-Defined Chunking (CDC)

### Algorithm: Gearhash

Crab uses Gearhash-based CDC from xet-core. A rolling hash scans the input
byte stream and declares chunk boundaries when the hash matches a specific bit
pattern:

```
For each byte position P:
  hash = rolling_hash(window ending at P)
  if hash & MASK == 0:
    → boundary at P
```

### Chunk Size Parameters

| Parameter | Value | Purpose |
|-----------|-------|---------|
| Target | 64 KiB | Average chunk size |
| Minimum | 8 KiB | Prevents pathologically small chunks |
| Maximum | 128 KiB | Prevents pathologically large chunks on random data |

The minimum cap is enforced by skipping boundary checks until `MIN_CHUNK_SIZE`
bytes have accumulated. The maximum cap forces a boundary regardless of hash
value.

### Key Properties

- **Deterministic**: Same input always produces same chunks. Critical for dedup.
- **Content-sensitive boundaries**: An insertion or deletion shifts boundaries
  only locally around the edit. Inserting 4 bytes at the start of a 10 GB file
  re-chunks ~1 chunk near the edit; the rest is unchanged.
- **Streaming**: Memory use bounded by `MAX_CHUNK_SIZE` regardless of file size.
  Chunks are emitted as soon as their boundary is found.
- **SIMD-accelerated**: AVX2/NEON implementation available via xet-core feature
  flag. Throughput target: ≥500 MB/s single-threaded on modern x86_64.

Source: `crates/crab-xet/src/chunker.rs`

### Adaptive Threshold

The `adaptive_threshold` module dynamically adjusts the chunk size threshold
based on file characteristics. For highly compressible content, larger chunks
may be preferred; for random binary data, the default is optimal.

Source: `crab/src/engine/adaptive_threshold.rs`

## Three-Tier Dedup Classification

During the clean filter and push pipeline, each chunk is classified into one
of three tiers:

```
┌─────────────────────────────────────────────────────────┐
│  Class A (Existing): chunk already on remote            │
│    → skip entirely (no pack, no upload)                 │
│    → detected via ChunkIndex lookup                     │
│                                                         │
│  Class B (Staged): chunk in local staging, not remote   │
│    → needs packing into xorb and upload                 │
│    → detected via staging index lookup                  │
│                                                         │
│  Class C (New): chunk not seen before                   │
│    → needs staging, packing, and upload                 │
│    → default classification                             │
└─────────────────────────────────────────────────────────┘
```

The classification cascades: check ChunkIndex first (cheapest), then staging
index, then classify as new.

Source: `crab/src/engine/dedup.rs`

## Xorb Packing Strategy

### The Tradeoff

- **Aggressive dedup (scatter)**: Every chunk goes into the xorb that maximizes
  dedup. Result: a file's chunks spread across many xorbs, requiring many Range
  GETs to reconstruct. Cost: read latency.
- **No dedup (coalesce)**: Every chunk goes into a new xorb. Result: shared
  chunks re-uploaded. Cost: storage and bandwidth.

### Crab's Approach: Prefer Continuity

Chunks from the same source file are kept together within a xorb (run
continuity). A run break is only allowed after accumulating at least 1 MiB.
Dedup is accepted only when savings exceed 25% of the run's size.

```
Parameters:
  TARGET_XORB_SIZE = 64 MiB
  MIN_RUN_SIZE = 1 MiB
  DEDUP_THRESHOLD_RATIO = 0.25
```

This ensures that reconstructing a file typically requires fetching only a
handful of xorbs rather than hundreds.

Source: `crab/src/storage/xorb/builder.rs`, `crab/src/git/push_native.rs`

### Xorb Builder Pipeline

During push, staged chunks are fed into `XorbBuilder`, which keeps chunks from
the same source file together when possible and seals xorbs at the configured
target size:

```
File bytes → GearhashChunker → XorbBuilder → Xorb bytes
```

Source: `crab/src/storage/xorb/builder.rs`, `crab/src/git/push_native.rs`

## Pointer Format

Crab pointer blobs are small (~200 bytes) text files stored in Git's ODB:

```
version https://crab.dev/spec/v1
file-hash 7c1f2a3b4d5e6f...  (blake3, 64 hex chars)
size 10737418240
shard-hint a1b2c3d4...  (optional, for smudge fast path)
```

- `file-hash`: Blake3 hash of the full file content. Stable identity
  independent of chunking parameters.
- `size`: Original file size in bytes.
- `shard-hint`: Optional MerkleHash of the shard containing this file's
  reconstruction terms. Enables the smudge filter to skip the file-index
  lookup.

Detection: A blob is a pointer if it's ≤1024 bytes and starts with
`version https://crab.dev/spec/v1`.

Source: `crab/src/engine/pointer.rs`

## Staging Area

The staging area bridges `git add` (clean filter) and `git push`. Chunks are
written locally during clean and read during push.

### Layout

```
.crab/staging/
├── segments/
│   ├── current.seg          Append-only segment file
│   └── sealed-{id}.seg     Completed segments
├── index.db                 SQLite WAL-mode index
└── lockfile                 Advisory flock
```

### Segment File Format

```
┌──────────┬──────────┬──────────┬─────┐
│ Frame 0  │ Frame 1  │ Frame 2  │ ... │
└──────────┴──────────┴──────────┴─────┘

Each Frame:
┌──────────────┬──────────────────────┬───────────┐
│ length: u32  │ chunk_data: [u8]     │ crc32: u32│
└──────────────┴──────────────────────┴───────────┘
```

### SQLite Index Schema

```sql
files(file_hash, total_bytes)
chunks(chunk_hash, file_hash, seg_id, seg_offset, size, chunk_index)
pending_chunks(...)           -- pre-flush buffer
segments(id, status, size_bytes, live_chunk_count)
```

### Operations

| Operation | Lock | Description |
|-----------|------|-------------|
| `StagingArea::open()` | Exclusive flock | Full read-write access |
| `StagingAreaReadOnly::open()` | Shared flock | Read-only, never blocks writers |
| `stage_file()` | Within exclusive | Write chunks + index entries |
| `chunks_for_file()` | Read | Ordered chunk hashes for a file |
| `get_chunk()` | Read | Raw bytes via pread + CRC + blake3 verify |
| `clean()` | Exclusive | Remove stale markers, sweep orphans, compact |
| `close()` | Release | Flush and release flock |

### Force Mode

`StagingArea::open_force()` breaks a stale lock held by a dead process. Safe
because flock is advisory and a PID liveness check ensures only dead holders'
locks are broken.

Source: `crab/src/engine/staging/`

## Post-Push Retirement and the Xorb Cache

Staging is a *pre-push* buffer, not long-term storage. Its job ends the
moment a push successfully commits. After that point, the local source of
truth for already-uploaded chunk data is the on-disk xorb cache.

### Lifecycle of a Staged Chunk

```
 git add                git push (steps 1–12)           step 13
 ─────────              ──────────────────────           ───────
 clean filter           classify → pack → upload         post_success_cleanup
 writes chunk           reads chunk via                  retires chunks,
 into staging  ───────► StagingAreaReadOnly     ───────► warms xorb cache
                        (chunks_for_file, etc.)
```

### Retirement

Once step 13 runs in the push pipeline, `post_success_cleanup` iterates the
pushed pointers and calls `StagingAreaReadOnly::retire_file(&file_hash)` for
each. Retirement runs in a single SQLite transaction per file:

1. Delete all `chunks` rows matching `file_hash`.
2. Decrement `live_chunk_count` on every segment that had rows removed.
3. Any segment whose count reaches zero is marked `empty` and becomes a
   sweep candidate for the next `StagingArea::clean()` / orphan-sweep pass.

A chunk shared across multiple files stays alive as long as at least one
live file still references it — retirement only removes the *row for this
file*. Fast-pathed pointers (no chunks staged) produce a no-op retirement,
so the caller iterates uniformly without needing to branch.

Source: `StagingAreaReadOnly::retire_file` in
`crab/src/engine/staging/mod.rs`; the step-13 caller is
`PushPipeline::post_success_cleanup` in `crab/src/git/push.rs`.

### Xorb Cache as Persistent Local Storage

Immediately before retirement, the same `post_success_cleanup` pass warms
the on-disk xorb cache with every xorb this push uploaded:

```
~/.cache/crab/xorbs/{hex}        keyed by CacheKey::Xorb(hash)
```

`Bytes` handles captured during upload are drained from `PushPipeline`'s
`uploaded_xorbs` buffer and written via `LocalCache::put`. A subsequent
`crab hydrate` on the just-pushed working tree resolves chunk data out of
this cache without issuing an S3 GET, so hydrate-after-push is bounded by
local disk rather than round-trip latency.

This split replaces the earlier invariant where staging served double-duty
as both the pre-push buffer *and* the post-push local source of truth.
Staging now only covers the `git add` → `git push` window; the xorb cache
covers everything after.

Source: `CacheKey::Xorb` in `crab/src/cache/local_cache.rs`.
