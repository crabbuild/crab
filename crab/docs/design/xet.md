# crab — Xet Dedup & Transparent Git Integration Design

**How content-defined chunking plugs into Git without users noticing.**

-----

## Document Metadata

|Field       |Value                                          |
|------------|-----------------------------------------------|
|Project     |crab                                           |
|Scope       |Xet dedup layer + Git integration + end-user UX|
|Status      |Design                                         |
|Companion to|crab-design.md (main architecture)             |
|Version     |0.1                                            |

-----

## Table of Contents

1. [Why This Document Exists](#1-why-this-document-exists)
1. [UX Goals & Principles](#2-ux-goals--principles)
1. [The Integration Problem](#3-the-integration-problem)
1. [Dedup Layer Architecture](#4-dedup-layer-architecture)
1. [Chunking Deep Dive](#5-chunking-deep-dive)
1. [Xorb Packing Strategy](#6-xorb-packing-strategy)
1. [Shard Design](#7-shard-design)
1. [Git Integration Points](#8-git-integration-points)
1. [Pointer File Format](#9-pointer-file-format)
1. [The Filter Process](#10-the-filter-process)
1. [Staging & Commit Lifecycle](#11-staging--commit-lifecycle)
1. [Checkout Lifecycle](#12-checkout-lifecycle)
1. [Dedup Decisions in Practice](#13-dedup-decisions-in-practice)
1. [Performance Engineering](#14-performance-engineering)
1. [Transparent UX Patterns](#15-transparent-ux-patterns)
1. [Failure Mode UX](#16-failure-mode-ux)
1. [Progressive Disclosure](#17-progressive-disclosure)
1. [Edge Cases](#18-edge-cases)
1. [What Users Should Never See](#19-what-users-should-never-see)
1. [Testing UX](#20-testing-ux)

-----

## 1. Why This Document Exists

The main crab design doc covers the whole system. This doc zooms in on one slice: the dedup layer and how Git plugs into it. Two reasons to split it out:

- **The integration is where correctness bugs hide.** Clean filter wrong by one byte → every checkout is corrupted. Pointer parser wrong → history is unreadable. This deserves focused attention.
- **UX transparency is the product.** Users choose crab over Git-LFS because it’s supposed to “just work.” If they have to think about chunks, xorbs, or shards, we’ve failed. Articulating what “transparent” means precisely prevents feature creep that violates it.

Readers should have read the main design doc first (especially §4, §6, §7, §9, §10). This doc assumes familiarity with the three-tier data model, the remote helper protocol, and xet object types.

-----

## 2. UX Goals & Principles

### 2.1 The Promise

A user who has used Git before should be able to use crab without learning anything new. They install a binary, run standard Git commands, and their 50 GB model files work.

The following commands must work identically to vanilla Git:

```
git clone crab://bucket/repo
git pull
git add <large-file>
git commit -m "..."
git push
git checkout <rev>
git checkout <branch>
git log
git diff
git blame
git stash
git reset --hard
```

The only commands a user *must* learn are one-time setup:

```
crab init            # once per new repo
crab track "*.gguf"  # once per file pattern
```

Everything else — dedup, chunking, caching, uploads — happens under the hood.

### 2.2 Principles

1. **No new mental model.** Users think in commits, branches, files. Not chunks, xorbs, shards.
1. **No config files to hand-edit.** Setup is via commands, not text editors. The tool writes the configs.
1. **Errors speak in user terms.** Never say “shard hash mismatch.” Say “file X appears corrupted on the server.”
1. **Progress is visible but not noisy.** Users see progress bars for long operations; they don’t see per-chunk log spam.
1. **Performance should feel like Git, not like LFS.** Checkouts are fast because of caching. Pushes are fast because of dedup.
1. **Degrade gracefully.** No network? Show cached state. Old client? Warn but work.
1. **Escape hatches exist.** Expert users can inspect internals via `crab debug` / `crab fsck` / `crab stat`. Normal users never need them.

### 2.3 The Anti-Principle

**It is explicitly NOT a goal to hide that crab exists.** Users know they’re using crab (they installed it, the URLs say `crab://`). Pretending otherwise backfires when things go wrong and users can’t search for help. “Transparent” means “doesn’t require attention during normal use,” not “invisible.”

-----

## 3. The Integration Problem

### 3.1 What Git Expects

Git treats file content as opaque blobs in its object database. When you `git add foo.bin`, Git:

1. Reads `foo.bin` fully.
1. Computes SHA-1 of its content.
1. Zlib-compresses the content.
1. Writes to `.git/objects/<sha[:2]>/<sha[2:]>`.

On checkout, Git reads the blob and writes it to the working tree.

This is fine for small files. For a 10 GB model file, this means:

- 10 GB of zlib-compressed garbage in `.git/objects/` (model weights don’t compress).
- Every `git checkout` across branches rewrites the 10 GB file.
- Every `git push` transfers the full 10 GB.
- Every `git clone` downloads every historical version.

### 3.2 What Git Provides for Intercept

Git has three extension points for customizing blob handling:

**Filter drivers (clean/smudge)**

Defined in `.gitattributes`:

```
*.bin filter=crab
```

And in `.git/config`:

```ini
[filter "crab"]
    clean = git-remote-crab clean %f
    smudge = git-remote-crab smudge %f
    required = true
```

On `git add`, Git pipes the file’s content through the `clean` command and stores the output as the blob. On `git checkout`, Git pipes the blob through the `smudge` command and writes the output to the working tree.

The process-per-file overhead is prohibitive for repos with thousands of large files, so Git also provides:

**Long-running filter process (v2 filter protocol)**

```ini
[filter "crab"]
    process = git-remote-crab filter-process
    required = true
```

A single process handles all clean/smudge calls in a session via a pkt-line-framed stdin/stdout protocol. This is what crab uses.

**Remote helpers**

For custom URL schemes, Git invokes a helper binary that handles all network interaction. This is crab’s push/pull mechanism (covered in main design doc §6).

### 3.3 The Division of Labor

```
┌───────────────────────┬───────────────────────────────────────┐
│   Git responsibility  │   crab responsibility               │
├───────────────────────┼───────────────────────────────────────┤
│ Commit graph          │ Large-file chunking                   │
│ Tree structure        │ Chunk deduplication                   │
│ Ref management        │ Xorb/shard construction               │
│ Merge conflict resol. │ S3 upload/download                    │
│ Diff generation       │ Cache management                      │
│ History traversal     │ Content-hash verification             │
│ User identity (name)  │ Cloud identity (IAM)                  │
│ Working tree writes   │ Pointer ↔ content translation         │
└───────────────────────┴───────────────────────────────────────┘
```

crab never parses commits, trees, or Git’s ref semantics — gitoxide handles that where needed. Git never sees chunks or xorbs — it sees only opaque pointer blobs.

The filter protocol is the seam.

-----

## 4. Dedup Layer Architecture

### 4.1 Layer Stack

```
┌────────────────────────────────────────────────────────────┐
│  Layer 6: User Experience                                  │
│   git commands, progress UI, error messages                │
├────────────────────────────────────────────────────────────┤
│  Layer 5: Filter Protocol Handler                          │
│   long-running filter process loop                         │
│   clean() / smudge() entry points                          │
├────────────────────────────────────────────────────────────┤
│  Layer 4: File Processor                                   │
│   clean: content → pointer (+ stage chunks)                │
│   smudge: pointer → content (+ populate cache)             │
├────────────────────────────────────────────────────────────┤
│  Layer 3: Dedup Planner                                    │
│   consult chunk index, decide new vs reuse                 │
│   group new chunks into xorbs                              │
│   build shards                                             │
├────────────────────────────────────────────────────────────┤
│  Layer 2: Xet Primitives                                   │
│   Gearhash CDC   |  MerkleHash     |  Xorb format          │
│   (xet-core)     |  (xet-core)     |  Shard format         │
├────────────────────────────────────────────────────────────┤
│  Layer 1: Storage Operations                               │
│   object_store trait: get, put, list, cas                  │
│   S3 / GCS / Azure / R2 / MinIO backends                   │
└────────────────────────────────────────────────────────────┘
```

Each layer depends only on the one below. Layer 6 never touches S3 directly; layer 1 never knows about chunks.

### 4.2 Core Types

```rust
// Layer 2: content addresses
pub struct MerkleHash([u8; 32]);   // chunk, xorb, shard hashes
pub struct FileHash([u8; 32]);     // blake3 of full file
pub struct GitSha([u8; 20]);       // Git's SHA-1

// Layer 2: xet objects
pub struct Chunk {
    pub hash: MerkleHash,
    pub bytes: Bytes,  // decompressed
}

pub struct Xorb {
    pub hash: MerkleHash,
    pub chunks: Vec<ChunkSlot>,   // compressed bytes + metadata
}

pub struct Shard {
    pub hash: MerkleHash,
    pub xorbs: Vec<XorbInfo>,      // what chunks are in each xorb
    pub files: Vec<FileInfo>,      // how to reconstruct each file
}

pub struct FileInfo {
    pub hash: FileHash,
    pub size: u64,
    pub terms: Vec<ReconstructionTerm>,
}

pub struct ReconstructionTerm {
    pub xorb_hash: MerkleHash,
    pub chunk_start: u32,
    pub chunk_end: u32,   // exclusive
}

// Layer 3: planning
pub struct DedupPlan {
    pub chunks_to_upload: Vec<Chunk>,
    pub chunks_reused: Vec<(MerkleHash, XorbRef)>,
    pub xorbs_to_assemble: Vec<Vec<Chunk>>,
}
```

### 4.3 Stateful Components

Two stateful components sit across the filter protocol’s invocation lifetime:

**ChunkIndex**: in-memory map of known chunks for dedup queries. Loaded from local shard cache at process start. Queried during clean; updated after successful pushes.

**XetCache**: local filesystem cache of chunks, shards, xorbs. Populated during smudge; consulted to avoid re-downloading.

Both are managed by a single `RepoContext` owned by the filter process. When `git` asks `filter-process` to handle many files, the same `RepoContext` serves them all.

-----

## 5. Chunking Deep Dive

### 5.1 The Algorithm (Gearhash CDC)

Gearhash is a rolling hash with a specific boundary predicate:

```
Given a window of 64 bytes:
  hash = sum over i in [0..64] of GEAR_TABLE[window[i]] << (63 - i)

A boundary is declared at position P when:
  (hash(window ending at P)) & MASK == 0

where MASK is tuned to produce the desired average chunk size.
```

For 64 KiB target: `MASK = (1 << 20) - 1` (20 bits set). Probability of match per byte is 2⁻²⁰ ≈ 1 in 1 million, giving average chunks of ~1 MiB… wait, that doesn’t match 64 KiB.

Actually: `MASK = (1 << 16) - 1` for 64 KiB target. Probability of boundary = 2⁻¹⁶ = 1/65536. Expected distance between boundaries = 65536 bytes = 64 KiB. ✓

xet-core uses parameterized masks to achieve different average sizes. crab uses the Xet defaults (64 KiB target) for compatibility.

### 5.2 Boundary Rules

In practice, boundary placement is more nuanced than “wait for hash match”:

```rust
fn next_boundary(&mut self, bytes: &[u8]) -> Option<usize> {
    let mut hash = self.state;

    for (i, &byte) in bytes.iter().enumerate() {
        let position = self.bytes_since_last_boundary + i + 1;

        // Must be at least minimum
        if position < MIN_CHUNK_SIZE {
            hash = update_hash(hash, byte);
            continue;
        }

        // Hard cap at maximum — forced boundary
        if position >= MAX_CHUNK_SIZE {
            return Some(i);
        }

        // Natural boundary
        hash = update_hash(hash, byte);
        if hash & MASK == 0 {
            return Some(i);
        }
    }

    self.state = hash;
    None   // need more bytes
}
```

Constants (xet-core defaults):

- `MIN_CHUNK_SIZE = 8 * 1024`   (8 KiB)
- `TARGET_CHUNK_SIZE = 64 * 1024`  (64 KiB)
- `MAX_CHUNK_SIZE = 128 * 1024`  (128 KiB)

The MIN cap prevents pathologically small chunks (e.g., a file of all zeros would trigger boundaries everywhere without it). The MAX cap prevents pathologically large chunks on highly random data.

### 5.3 Determinism

**Same input produces same chunks, always.** This is critical for dedup. Any nondeterminism (threading order, floating-point, wall clock) would break the property.

Tests enforce this:

```rust
#[test]
fn chunking_is_deterministic() {
    let data = generate_pseudorandom_bytes(1_000_000, seed=42);
    let chunks_a = chunk(&data);
    let chunks_b = chunk(&data);
    assert_eq!(chunks_a, chunks_b);
}

#[test]
fn chunking_is_order_independent_per_chunk() {
    // Chunks within a file depend only on content up to their boundary.
    // Chunk i has identical content regardless of what comes after it.
    let data = generate_pseudorandom_bytes(1_000_000, seed=42);
    let mut extended = data.clone();
    extended.extend_from_slice(&generate_pseudorandom_bytes(100_000, seed=99));

    let chunks_a = chunk(&data);
    let chunks_b = chunk(&extended);

    // All chunks up to but not including the last should match
    for (a, b) in chunks_a.iter().take(chunks_a.len() - 1).zip(&chunks_b) {
        assert_eq!(a.hash, b.hash);
    }
}
```

### 5.4 Streaming Implementation

Files can be terabytes; we cannot buffer them. The chunker must be streaming:

```rust
pub struct GearhashChunker {
    state: u64,
    buffer: BytesMut,                // pending bytes since last boundary
    bytes_since_boundary: usize,
    hasher: blake3::Hasher,          // for file-hash tracking
    total_bytes: u64,
}

impl GearhashChunker {
    pub fn feed(&mut self, input: &[u8]) -> Vec<Chunk> {
        let mut completed = Vec::new();
        let mut cursor = 0;

        while cursor < input.len() {
            let remaining = &input[cursor..];
            match self.advance(remaining) {
                AdvanceResult::Boundary(offset) => {
                    self.buffer.extend_from_slice(&remaining[..=offset]);
                    let chunk_bytes = self.buffer.split().freeze();
                    completed.push(Chunk::new(chunk_bytes));
                    self.bytes_since_boundary = 0;
                    cursor += offset + 1;
                }
                AdvanceResult::NeedMore => {
                    self.buffer.extend_from_slice(remaining);
                    self.bytes_since_boundary += remaining.len();
                    cursor = input.len();
                }
            }
        }

        self.hasher.update(input);
        self.total_bytes += input.len() as u64;
        completed
    }

    pub fn finalize(mut self) -> (Option<Chunk>, FileHash, u64) {
        let last = if !self.buffer.is_empty() {
            Some(Chunk::new(self.buffer.freeze()))
        } else {
            None
        };
        let file_hash = FileHash::from(self.hasher.finalize());
        (last, file_hash, self.total_bytes)
    }
}
```

Key properties:

- Memory use bounded by `MAX_CHUNK_SIZE` (128 KiB) regardless of file size.
- Output a chunk as soon as its boundary is found — no waiting.
- Computes file-hash and total-size incrementally.

### 5.5 SIMD Acceleration

Gearhash is pointer-chasing through a lookup table, which is hard to SIMD-ify directly. But we can process multiple byte positions in parallel when checking only for the boundary condition, not the full hash. xet-core includes an AVX2/NEON implementation that’s ~4× faster than scalar.

crab uses xet-core’s accelerated chunker when available (via feature flag `simd-accel`). Fallback to scalar on platforms without support.

Throughput target: ≥ 500 MB/s on modern x86_64 with AVX2, single-threaded. For multi-gig files, parallel chunking across file offsets is possible but breaks determinism (boundaries depend on prior state). crab keeps per-file chunking single-threaded; multi-file parallelism handles throughput.

-----

## 6. Xorb Packing Strategy

### 6.1 The Fundamental Tradeoff

Once we have a stream of chunks, we need to group them into xorbs (~64 MiB blobs) for upload. Two extremes:

**Aggressive dedup (scatter):** every chunk goes into the existing xorb that most maximizes dedup. Result: a file’s chunks may be spread across 100+ xorbs. Reading the file requires 100+ Range GETs. Cost: read latency.

**No dedup (coalesce):** every new chunk goes into a new xorb containing only this push’s chunks. Result: chunks shared with existing xorbs get re-uploaded. Cost: storage and bandwidth.

crab takes a middle path: **prefer continuity, accept dedup only when the saved bytes exceed a threshold.**

### 6.2 The Packing Algorithm

```
Input: ordered list of chunks C_1, C_2, ..., C_N from a single file
       (chunks are in the order they appear in the file)
Input: existing chunk index (chunk_hash → existing_xorb_ref)
Output: list of xorb assignments

Let TARGET_XORB_SIZE = 64 MiB
Let MIN_RUN_SIZE = 1 MiB
Let DEDUP_THRESHOLD_RATIO = 0.25  // only dedup if savings > 25% of the run

Algorithm:
  current_run = []
  current_run_size = 0
  current_xorb = new Xorb()
  xorbs = []

  for each chunk C_i:
    if C_i is in chunk index:
      # Chunk exists in some existing xorb X
      if current_run_size >= MIN_RUN_SIZE:
        # We have a continuous run; close it and reference X
        flush(current_run, current_xorb)
        emit reference to existing chunk in X
        current_run = []
        current_run_size = 0
      else:
        # Run is too short to break; decide by savings threshold
        future_run_dedup_savings = estimate(C_i, ...)
        if future_run_dedup_savings > current_run_size * DEDUP_THRESHOLD_RATIO:
          flush(current_run, current_xorb)
          emit reference to existing chunk
          current_run = []
        else:
          add C_i bytes to current_run (no dedup)
          current_run_size += C_i.size
    else:
      # Chunk is new
      add C_i bytes to current_run
      current_run_size += C_i.size

    if current_xorb.size + current_run_size > TARGET_XORB_SIZE:
      flush(current_run, current_xorb)
      xorbs.append(current_xorb)
      current_xorb = new Xorb()
      current_run = []
      current_run_size = 0

  # Finalize
  if current_run not empty:
    flush(current_run, current_xorb)
  if current_xorb not empty:
    xorbs.append(current_xorb)

  return xorbs
```

The `estimate(C_i, ...)` function looks ahead some number of chunks to guess how much dedup we’d get if we broke the run here. This is approximate — we don’t want to re-scan the whole file — so we peek ahead ~1 MiB and count existing chunks.

### 6.3 Why 25%?

Empirical heuristic from xet-core’s production experience. Values tested:

- 0% (always dedup): chunks scattered, reads ~10× slower on fragmented files.
- 10%: marginal; still too much scatter for AI workloads.
- 25%: good balance; typical AI checkpoint has <5 xorbs per file.
- 50%: under-dedups in practice; storage savings drop ~15%.

crab makes this configurable (`dedup.min_savings_ratio`) but defaults to 0.25.

### 6.4 Xorb Content Ordering

Within a xorb, chunks are laid out in the order they were added. Shards store an offset table so that a Range GET can extract any chunk or contiguous chunk range.

Binary layout (matches xet-core’s xorb format):

```
Header:
  magic: "XORB"    (4 bytes)
  version: u16
  chunk_count: u32
  metadata_offset: u64

Chunk data:
  for each chunk:
    length: u32
    compressed_bytes: [u8]

Metadata:
  for each chunk:
    hash: MerkleHash (32 bytes)
    uncompressed_length: u32
    offset_in_xorb: u64

Footer:
  hash_of_metadata: MerkleHash
  hash_of_xorb: MerkleHash     // content hash, used as filename
```

The hash at the end is what names the xorb. Computed over the header + chunks + metadata (excluding the final hash field itself).

### 6.5 Compression

Each chunk is compressed independently with zstd level 3 before being written to the xorb. Per-chunk compression (vs whole-xorb) allows:

- Range GETs to fetch specific chunks without decompressing the whole xorb.
- Mixed content (compressible text + random binary) to compress appropriately.

zstd-3 is chosen for:

- Good compression on text, config, and some model serialization formats.
- Fast decompression (~1 GB/s single-threaded).
- Acceptable compression speed (~500 MB/s).

Random binary data (random weights, encrypted content) won’t compress; zstd detects this quickly and falls through to roughly-raw storage with minimal overhead.

Configurable via `compression.algorithm` and `compression.level`. Supported: `zstd`, `lz4`, `none`. Default: `zstd-3`.

-----

## 7. Shard Design

### 7.1 What a Shard Contains

A shard serves two purposes:

1. **Authoritative record of a push’s file reconstructions.** Every file touched in a push has a `FileInfo` entry.
1. **Dedup index for future pushes.** The `CAS info` section lists all xorbs and the chunks they contain.

Format (matches xet-core’s `mdb_shard`):

```
Header:
  magic: "MDBSHARD"
  version: u16
  shard_hash: MerkleHash
  cas_info_offset: u64
  file_info_offset: u64
  footer_offset: u64

CAS Info:
  xorb_count: u32
  for each xorb:
    xorb_hash: MerkleHash
    chunk_count: u32
    xorb_size: u64
    for each chunk in xorb:
      chunk_hash: MerkleHash
      uncompressed_size: u32

File Info:
  file_count: u32
  for each file:
    file_hash: FileHash
    file_size: u64
    term_count: u32
    for each term:
      xorb_hash: MerkleHash
      chunk_start: u32
      chunk_end: u32   // exclusive

Footer:
  timestamps
  hmac (optional, for global dedup scenarios)
  hash_of_body: MerkleHash
```

### 7.2 Shard Granularity

**One shard per push.** Contains all files and all xorbs touched by that push.

Alternatives considered:

- One shard per file: too many tiny objects; inflates the shard-list manifest.
- One shard per repo (ever): shards grow unboundedly; every push rewrites a monster file.
- One shard per commit: unclear mapping if a push is multiple commits.

Per-push is the natural unit: all the chunks in a shard were generated in one coherent operation, and the shard can be uploaded atomically.

### 7.3 Shard Size Bounds

- Typical AI repo push: ~1 KiB to 10 MiB shards.
- Upper bound: one file per 200 bytes of file-info + xorb info. A push of 100,000 files with 1000 xorbs → ~50 MiB shard.
- Action at bounds: if a push shard would exceed 100 MiB, split into multiple shards at file boundaries. Rare in practice.

### 7.4 Shard Load Performance

On fetch, the smudge filter must load shards to perform reconstruction. A session that checks out many files needs many shards.

Mitigations:

- **Lazy loading**: shards are loaded on demand (not all at process start).
- **Partial parsing**: we can skip to the file-info section via the offset in the header; no need to parse CAS info unless we’re going to dedup against it.
- **Cache**: shards never change (content-addressed, immutable), so local cache is unbounded by freshness concerns.
- **Bloom filter** (future v1.1): a tiny bloom filter per shard lists the files it describes. Lookup becomes: for each file, test bloom filters of local shards; download shard only on match. Avoids downloading shards we don’t need.

-----

## 8. Git Integration Points

### 8.1 Setup Phase

```
$ crab init crab://my-bucket/my-repo
```

Actions taken:

1. Create S3 objects:
- `config` (JSON)
- `HEAD` (`ref: refs/heads/main`)
- `refs/heads/main` (optional; empty repo works without any refs initially)
1. Initialize local `.git` if not already a repo (via `gix init`).
1. Add remote:
   
   ```
   [remote "origin"]
       url = crab://my-bucket/my-repo
       fetch = +refs/heads/*:refs/remotes/origin/*
   ```
1. Register filter process in `.git/config`:
   
   ```
   [filter "crab"]
       process = git-remote-crab filter-process
       required = true
   ```
1. Create `.gitattributes` with no patterns yet.

### 8.2 Track Phase

```
$ crab track "*.safetensors" "*.ckpt" "*.parquet" "models/**/*.bin"
```

Actions:

1. Append patterns to `.gitattributes`:
   
   ```
   *.safetensors filter=crab
   *.ckpt filter=crab
   *.parquet filter=crab
   models/**/*.bin filter=crab
   ```
1. Run `git add .gitattributes`.
1. Warn if tracked patterns match already-committed files:
   
   ```
   Note: 3 files matching these patterns are already committed
   without crab handling. To convert them, run:
     crab migrate-tracked
   ```

### 8.3 Default Patterns

crab ships a curated list of common large-file patterns that users can opt into:

```
$ crab track --preset ml

# Equivalent to:
# *.safetensors, *.ckpt, *.bin, *.pt, *.pth, *.onnx, *.gguf, *.mlmodel
```

Presets: `ml`, `data`, `media`, `all`.

### 8.4 Clean Path (git add → ODB)

```
User runs: git add big_model.safetensors

1. Git stats file, sees 10 GB.
2. Git checks .gitattributes: matches filter=crab.
3. Git sends to filter process (running or starting):
     pkt: command=clean
     pkt: pathname=big_model.safetensors
     pkt: (file content, streamed in chunks)
     pkt: flush

4. Filter process:
   a. Creates GearhashChunker, blake3 hasher.
   b. For each incoming stream chunk:
      - feed chunker, get 0+ boundary chunks
      - for each boundary chunk: stage to ~/.cache/crab/{repo}/staging/chunks/
   c. On stream end: finalize chunker, get last chunk, file_hash, size.
   d. Stage last chunk.
   e. Record (file_hash, [(chunk_hash, offset, size)...]) in staging metadata.
   f. Emit pointer blob:
        version https://crab.dev/spec/v1
        file-hash {blake3}
        size {bytes}

5. Filter returns pointer to git.

6. Git SHA-1s the pointer (small, ~100 bytes), zlib-compresses it, stores in .git/objects.
```

No S3 interaction yet. All chunks are in local staging. This is critical: `git add` must never block on network.

### 8.5 Commit Path

`git commit` just records the current index (with pointer blobs) into a new commit object. No crab involvement.

The commit is local; chunks remain in staging.

### 8.6 Push Path (ODB → S3)

```
User runs: git push

1. Git invokes helper:
     git-remote-crab crab crab://my-bucket/my-repo

2. Helper reads git objects for the push via GIT_OBJECT_DIRECTORY.

3. Helper enumerates large-file pointers in the push:
   - walk commits being pushed
   - for each tree, find blobs
   - for each blob, check if it's a pointer (read first 128 bytes, check prefix)
   - collect (file_hash, git_sha) pairs for pointers

4. For each pointer file_hash, consult staging:
   - if chunks are in staging: proceed with dedup plan
   - if not in staging: this is a re-push of an old file-hash; shard must
     already exist on S3. Verify by HEAD on file-index/{file_hash}.
     If missing, error: "cannot find chunks for file X; re-add the file to restore"

5. Dedup planning (see §13):
   - load shard cache → chunk index
   - partition chunks into (reuse, new)
   - pack new chunks into xorbs

6. Upload xorbs in parallel → upload shard → upload file-index entries
   → update shard-list → upload pack → update pack-list → update refs.

7. Clean up staging: move chunks from staging/ to cache/ (they're now canonical).

8. Report success to git.
```

### 8.7 Smudge Path (S3 → Working Tree)

```
User runs: git checkout v2  (v2 has a different version of big_model.safetensors)

1. Git walks the tree diff, identifies changed files.
2. For files with filter=crab, git:
   a. Reads the blob (pointer) from ODB.
   b. Sends to filter process:
        pkt: command=smudge
        pkt: pathname=big_model.safetensors
        pkt: can-delay=1     (we support delayed processing)
        pkt: (pointer content)
        pkt: flush

3. Filter process:
   a. Parse pointer: (file_hash, size).
   b. Lookup file-index: GET xet/file-index/{file_hash[:2]}/{file_hash}
      Body is shard_hash (32 bytes, hex-encoded).
   c. Load shard: check cache, else GET xet/shards/{shard_hash[:2]}/{shard_hash}.
   d. Extract reconstruction terms for file_hash.
   e. Fetch chunks:
      for each term (xorb_hash, chunk_start, chunk_end):
        compute byte range in xorb (from shard's xorb-info)
        check chunk cache; populate missing chunks via parallel Range GETs
      reassemble chunks in order.
   f. Stream decompressed content to git via stdout.

4. Git writes filter output to working tree.
```

### 8.8 Delayed Smudge for Parallelism

Git’s filter protocol v2 supports a “delay” capability: the filter can tell git “I’m not done with this file yet; give me the next one and come back.” This allows crab to queue up many smudge requests and satisfy them in parallel.

```
git: command=smudge, pathname=a.bin, can-delay=1, (content)
crab: delayed
git: command=smudge, pathname=b.bin, can-delay=1, (content)
crab: delayed
git: command=list_available_blobs
crab: pathname=a.bin   (finished first)
git: command=smudge, pathname=a.bin     (content already received)
crab: (the actual content from parallel fetch)
git: command=list_available_blobs
crab: pathname=b.bin
git: command=smudge, pathname=b.bin
crab: (content)
```

This dramatically speeds up checkouts of repos with many large files.

### 8.9 The Pointer-in-Tree Question

Git dedupes blobs by SHA-1 of content. Two commits referencing the same file-hash have the same pointer content and therefore the same Git blob SHA — Git stores the pointer once.

BUT: a file’s content changing while its name and size stay the same (e.g., editing weights in place, re-saving with same size) yields a *new* file-hash and therefore a *new* pointer and a *new* Git blob. Git sees this as a content change, correctly.

The Git-level history of a file tracks its *versions* (each with a distinct file-hash), while the xet layer tracks the *chunks* (many of which are shared across versions).

-----

## 9. Pointer File Format

### 9.1 The Format

```
version https://crab.dev/spec/v1
file-hash {64-char hex blake3}
size {decimal bytes}
```

Exactly three lines, each terminated by LF (not CRLF, even on Windows).

- Line 1 is a URL, not just a string. The domain identifies the format family; the path identifies the version. Future formats update the path.
- File-hash is blake3, 32 bytes, lowercase hex (64 chars).
- Size is decimal ASCII, no thousand separators, no trailing whitespace.

Rationale for three lines:

- Fewer than three: ambiguous if optional fields are added later.
- More than three: bloats Git blob size unnecessarily.
- Key-value lines (not JSON) for readability when a user accidentally opens a pointer in a text editor.

### 9.2 Parser

```rust
#[derive(Debug)]
pub struct Pointer {
    pub file_hash: FileHash,
    pub size: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum PointerError {
    #[error("not a crab pointer (missing version line)")]
    NotAPointer,
    #[error("unsupported pointer version: {0}")]
    UnsupportedVersion(String),
    #[error("malformed pointer: {0}")]
    Malformed(&'static str),
    #[error("file hash is not 64 hex chars")]
    BadFileHash,
    #[error("size is not a valid u64")]
    BadSize,
}

impl Pointer {
    pub fn parse(bytes: &[u8]) -> Result<Self, PointerError> {
        // Pointers are tiny; parse as UTF-8 string
        let text = std::str::from_utf8(bytes)
            .map_err(|_| PointerError::NotAPointer)?;
        let mut lines = text.lines();

        let version_line = lines.next().ok_or(PointerError::NotAPointer)?;
        let version = version_line
            .strip_prefix("version ")
            .ok_or(PointerError::NotAPointer)?;
        if version != "https://crab.dev/spec/v1" {
            return Err(PointerError::UnsupportedVersion(version.to_owned()));
        }

        let hash_line = lines.next().ok_or(PointerError::Malformed("missing file-hash"))?;
        let hash_hex = hash_line
            .strip_prefix("file-hash ")
            .ok_or(PointerError::Malformed("missing file-hash key"))?;
        let file_hash = FileHash::from_hex(hash_hex)
            .map_err(|_| PointerError::BadFileHash)?;

        let size_line = lines.next().ok_or(PointerError::Malformed("missing size"))?;
        let size_str = size_line
            .strip_prefix("size ")
            .ok_or(PointerError::Malformed("missing size key"))?;
        let size = size_str.parse::<u64>()
            .map_err(|_| PointerError::BadSize)?;

        // Trailing lines are reserved for future extensions — ignore.

        Ok(Pointer { file_hash, size })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        format!(
            "version https://crab.dev/spec/v1\nfile-hash {}\nsize {}\n",
            self.file_hash.to_hex(),
            self.size,
        ).into_bytes()
    }
}
```

### 9.3 Detection Heuristics

When a blob might or might not be a pointer (e.g., during fsck), heuristics:

- Size < 4 KiB (pointers are <256 bytes).
- Starts with ASCII “version “ + URL character set.
- Contains “crab.dev” in line 1.

If all three match, parse; if parse fails, report as corrupt pointer rather than as legitimate file content.

### 9.4 LFS Interop

Git-LFS pointers look similar:

```
version https://git-lfs.github.com/spec/v1
oid sha256:{hash}
size {bytes}
```

Detection: first line starts with `version https://git-lfs.github.com/`.

When crab’s smudge filter sees an LFS pointer, it resolves the repository's
Crab object-store remote and fetches the object directly. The local Crab
transfer agent can also be selected by an unmodified Git LFS client through
the standalone custom-transfer configuration. Crab does not implement the
Git LFS HTTP discovery, Batch, or File Locking APIs; repositories that still
use those APIs keep their external LFS server during migration.

This allows gradual migration: keep existing LFS pointers while Crab handles
direct transfers, then incrementally re-add files with `crab track` or run a
history conversion when the repository is ready.

-----

## 10. The Filter Process

### 10.1 Lifecycle

The filter process is long-running per-git-session. It’s spawned by git on first need and reused for subsequent clean/smudge calls.

```
git invokes: git-remote-crab filter-process

Filter process:
  1. Handshake (see §10.2)
  2. Load RepoContext (find repo root, load config, initialize cache)
  3. Loop:
     - read command
     - dispatch (clean / smudge / list_available_blobs)
     - send response
  4. On EOF or quit: flush caches, exit cleanly.
```

Key optimization: `RepoContext` includes the `ChunkIndex` loaded once at startup. Loading the index from local shards takes 100ms–seconds; amortized over a session that handles hundreds of files, it’s negligible per-file.

### 10.2 Protocol Handshake

Git’s filter protocol v2 uses pkt-line framing (same as the wire protocol). Handshake:

```
git → filter:   0016git-filter-client\n
                000aversion=2\n
                0000

filter → git:   0016git-filter-server\n
                000aversion=2\n
                0000

git → filter:   0015capability=clean\n
                0016capability=smudge\n
                0015capability=delay\n
                0000

filter → git:   0015capability=clean\n
                0016capability=smudge\n
                0015capability=delay\n
                0000
```

crab declares support for `clean`, `smudge`, and `delay` capabilities.

### 10.3 Command Loop

After handshake, git sends commands until EOF or explicit quit:

```rust
loop {
    let command = read_pkt_line_command().await?;

    match command {
        FilterCommand::Clean { pathname, content } => {
            let pointer = clean(&context, &pathname, content).await?;
            write_filter_response(&pointer).await?;
        }
        FilterCommand::Smudge { pathname, content, can_delay } => {
            if can_delay {
                context.queue_smudge(pathname, content);
                write_delayed().await?;
            } else {
                let real = smudge(&context, &pathname, content).await?;
                write_filter_response_stream(real).await?;
            }
        }
        FilterCommand::ListAvailableBlobs => {
            // Return pathnames of smudge operations that finished
            let available = context.poll_finished_smudges().await;
            for pathname in available {
                write_pathname(&pathname).await?;
            }
            write_flush().await?;
        }
        FilterCommand::Blob { pathname } => {
            // Git is retrieving a previously-delayed smudge result
            let content = context.take_smudge_result(&pathname)?;
            write_filter_response(&content).await?;
        }
    }
}
```

### 10.4 Concurrency Within the Filter Process

A single filter process serves one session’s worth of requests. Within it:

- **Clean** operations are streaming but CPU-bound (chunking). Parallelize across files using `tokio::spawn` on a blocking thread pool.
- **Smudge** operations are network-bound (xorb fetches). Parallelize aggressively.
- The `RepoContext`’s `ChunkIndex` is read-only during the session; shared via `Arc` with no locking needed for reads. Writes (after push) are deferred until the session ends.

Queue model for delayed smudges:

```rust
pub struct SmudgeQueue {
    pending: Mutex<VecDeque<SmudgeRequest>>,
    in_progress: Mutex<HashMap<PathBuf, tokio::task::JoinHandle<Result<Bytes>>>>,
    finished: Mutex<HashMap<PathBuf, Bytes>>,
    permit: Arc<Semaphore>,
}

impl SmudgeQueue {
    pub fn submit(&self, request: SmudgeRequest) {
        let permit = self.permit.clone();
        let handle = tokio::spawn(async move {
            let _p = permit.acquire().await?;
            perform_smudge(request).await
        });
        self.in_progress.lock().insert(request.pathname.clone(), handle);
    }

    pub async fn poll_finished(&self) -> Vec<PathBuf> {
        // Move completed tasks from in_progress to finished
        let mut finished = Vec::new();
        let mut in_progress = self.in_progress.lock();
        for (path, handle) in in_progress.drain_completed() {
            let result = handle.await??;
            self.finished.lock().insert(path.clone(), result);
            finished.push(path);
        }
        finished
    }
}
```

Concurrency limit (default 16) via `Semaphore`. Git controls overall pacing by how fast it issues `list_available_blobs`.

### 10.5 Error Handling in the Filter

If the filter process crashes, git sees the broken pipe and aborts the operation with a generic error. Not great UX.

Better: the filter catches all errors, formats them as filter protocol responses, and continues:

```
filter → git:   0015status=error\n
                006eerror=file X could not be retrieved (network timeout).\n
                0000
```

Git displays the error message and aborts the specific file; other files in the batch continue. The user sees which file failed and can retry.

The filter process exits cleanly only on:

- EOF on stdin (git is done).
- Explicit quit (not used in practice).
- Unrecoverable internal error (corrupt cache requiring rebuild).

-----

## 11. Staging & Commit Lifecycle

### 11.1 Why Staging Exists

Clean filter is called during `git add`. At that moment, we don’t know:

- Whether this `git add` will be followed by `git commit`.
- Whether the resulting commit will be pushed (immediately or ever).
- Whether the chunks will dedup against anything we haven’t seen.

Uploading chunks to S3 eagerly at `git add` time is wasteful. Users `git add --amend`, `git reset`, `git stash` — chunks uploaded for discarded commits waste bandwidth and leave S3 garbage.

Staging defers uploads until push time.

### 11.2 Staging Directory

```
~/.cache/crab/{repo-hash}/staging/
├── chunks/
│   └── {hash[:2]}/{hash}           # raw chunk bytes
├── files.db                         # sqlite: file staging state
└── lockfile                         # advisory lock, prevents concurrent writes
```

`files.db` schema:

```sql
CREATE TABLE files (
    file_hash TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    total_chunks INTEGER NOT NULL
);

CREATE TABLE chunks (
    file_hash TEXT NOT NULL,
    chunk_ordinal INTEGER NOT NULL,
    chunk_hash TEXT NOT NULL,
    offset INTEGER NOT NULL,
    length INTEGER NOT NULL,
    PRIMARY KEY (file_hash, chunk_ordinal),
    FOREIGN KEY (file_hash) REFERENCES files(file_hash)
);

CREATE INDEX chunks_by_hash ON chunks(chunk_hash);
```

SQLite chosen for:

- Concurrent read safety (multiple filter processes don’t corrupt the DB).
- Transaction semantics for atomic staging of multi-chunk files.
- Zero-config dependency.

### 11.3 Staging Rules

- **On `git add`**: chunk file, write chunks to staging/chunks/, write file record to files.db.
- **On `git add` of the same file again**: new file-hash (if changed), new chunks; old file-hash entry remains staged until push or cleanup.
- **On `git rm --cached`**: no action in staging (Git removes from index, but the chunks may still be needed if they’re in a previous commit that gets pushed).
- **On push success**: chunks move from staging/chunks/ to cache/chunks/. files.db entries for pushed file-hashes deleted.
- **On `crab staging clean`**: remove staged data older than 7 days (configurable).

### 11.4 Staging Size Management

Staging can grow large. A user who `git add`s a 100 GB file then `git reset`s has 100 GB in staging.

Policies:

- **Age-based cleanup**: staged file-hashes older than 30 days (configurable) are pruned. The chunks may be referenced by other staged files, so reference-count before deleting.
- **Size cap**: default 50 GiB. On exceeding, prune oldest staged file-hashes until under cap. Warn user.
- **Manual**: `crab staging clean [--force]`.

### 11.5 Crash Recovery in Staging

If a `git add` is interrupted mid-chunking:

- Completed chunks are on disk (atomic writes via rename-from-tempfile).
- files.db has no entry yet (transaction didn’t commit).
- Orphaned chunks are garbage: `crab staging clean` reference-counts against files.db and removes unreferenced chunks.

If a push is interrupted mid-upload:

- Some chunks may have been uploaded; some haven’t.
- Staging is unchanged (moves only happen on push *success*).
- Retry of the push idempotently re-issues uploads. HEAD on each xorb checks if it already exists.

### 11.6 Concurrency

Multiple `git add` commands in the same repo (unlikely but possible) coordinate via the lockfile. The filter process holds a shared lock during clean; exclusive lock briefly when writing to files.db. SQLite’s own locking handles the rest.

Across repos: each repo has its own staging directory keyed by repo-hash. No cross-repo contention.

-----

## 12. Checkout Lifecycle

### 12.1 What Triggers Smudge

- `git clone` — smudge runs during initial checkout.
- `git checkout <rev>` across a revision that changed a tracked file.
- `git checkout <file>` — explicit single file.
- `git reset --hard` — working tree rewritten.
- `git stash pop` — if the stash contains changes to tracked files.
- `git read-tree` — low-level, bypasses filters by default but filter-aware with flags.

Git decides which files need smudging based on tree diffs; crab just responds to requests.

### 12.2 The Smudge Pipeline

```
pointer (from git) 
    │
    ▼
parse pointer → (file_hash, size)
    │
    ▼
resolve: file-index/{file_hash} → shard_hash
    │
    ▼
load shard (cache → S3)
    │
    ▼
extract reconstruction terms for this file
    │
    ▼
plan fetches: group terms by xorb, compute byte ranges
    │
    ▼
┌────────────────────────────────────┐
│   parallel xorb Range GETs         │
│   with chunk cache consultation    │
└────────────────────────────────────┘
    │
    ▼
decompress chunks, verify hashes
    │
    ▼
concatenate in file order → stream to git stdout
```

### 12.3 Range GET Coalescing

Reconstruction terms often span contiguous chunks within a single xorb. Rather than issuing a Range GET per chunk, coalesce:

```rust
fn plan_fetches(terms: &[ReconstructionTerm], shard: &Shard) -> Vec<FetchRequest> {
    let mut by_xorb: HashMap<MerkleHash, Vec<ChunkRange>> = HashMap::new();
    for term in terms {
        by_xorb.entry(term.xorb_hash)
            .or_default()
            .push(ChunkRange {
                start: term.chunk_start,
                end: term.chunk_end,
            });
    }

    let mut requests = Vec::new();
    for (xorb_hash, mut ranges) in by_xorb {
        ranges.sort_by_key(|r| r.start);

        // Coalesce adjacent ranges
        let mut coalesced: Vec<ChunkRange> = Vec::new();
        for range in ranges {
            if let Some(last) = coalesced.last_mut() {
                if range.start <= last.end + COALESCE_GAP {
                    last.end = last.end.max(range.end);
                    continue;
                }
            }
            coalesced.push(range);
        }

        // Convert chunk ranges to byte ranges using shard's xorb info
        let xorb_info = shard.xorb(xorb_hash).unwrap();
        for range in coalesced {
            let byte_start = xorb_info.chunk_offsets[range.start as usize];
            let byte_end = xorb_info.chunk_offsets[range.end as usize];
            requests.push(FetchRequest {
                xorb_hash,
                byte_range: byte_start..byte_end,
                chunk_range: range,
            });
        }
    }
    requests
}
```

`COALESCE_GAP` is the number of chunks we’re willing to over-fetch to avoid an extra request. At ~64 KiB chunks and ~100 ms of additional latency per request, over-fetching 5 chunks (~320 KiB) takes ~3 ms at 100 Mbps — better than a new request. Default `COALESCE_GAP = 5`.

### 12.4 Chunk Cache Interaction

Before issuing a Range GET, check local chunk cache:

```rust
async fn fetch_with_cache(req: &FetchRequest, cache: &ChunkCache, store: &dyn ObjectStore)
    -> Result<Vec<Chunk>>
{
    // First, check which chunks in the range are in cache
    let mut cached = Vec::new();
    let mut missing = Vec::new();
    for chunk_idx in req.chunk_range.start..req.chunk_range.end {
        let expected_hash = req.xorb_info.chunk_hash(chunk_idx);
        if let Some(chunk) = cache.get(&expected_hash).await? {
            cached.push((chunk_idx, chunk));
        } else {
            missing.push(chunk_idx);
        }
    }

    if missing.is_empty() {
        return Ok(cached.into_iter().map(|(_, c)| c).collect());
    }

    // Fetch only missing chunks' byte range
    let min = missing.iter().min().unwrap();
    let max = missing.iter().max().unwrap() + 1;
    let byte_start = req.xorb_info.chunk_offsets[*min as usize];
    let byte_end = req.xorb_info.chunk_offsets[max as usize];

    let response = store.get_opts(&xorb_path(req.xorb_hash), GetOptions {
        range: Some(byte_start..byte_end),
        ..Default::default()
    }).await?;

    let fetched = parse_xorb_range(response.bytes().await?, *min, max - *min, &req.xorb_info)?;

    // Populate cache
    for (idx, chunk) in fetched.iter().enumerate() {
        cache.put(&chunk.hash, &chunk.bytes).await?;
    }

    // Merge cached + fetched in chunk-index order
    let mut combined = cached;
    combined.extend(fetched.into_iter().enumerate().map(|(i, c)| (min + i as u32, c)));
    combined.sort_by_key(|(idx, _)| *idx);
    Ok(combined.into_iter().map(|(_, c)| c).collect())
}
```

### 12.5 Streaming to Stdout

For large files, buffering the entire file in memory before writing to git is untenable. Stream:

```rust
async fn smudge_stream(
    pointer: Pointer,
    shard: Arc<Shard>,
    fetcher: Arc<XorbFetcher>,
    mut output: impl AsyncWrite + Unpin,
) -> Result<()> {
    let terms = shard.terms_for(&pointer.file_hash)?;

    // Prefetch ahead ~16 MiB to overlap network and write
    let prefetch = Prefetcher::new(terms.clone(), shard.clone(), fetcher, 16 * 1024 * 1024);

    for term in &terms {
        let chunks = prefetch.fetch_term(term).await?;
        for chunk in chunks {
            output.write_all(&chunk.bytes).await?;
        }
    }
    output.flush().await?;
    Ok(())
}
```

### 12.6 Checkout UX

What users see during a large checkout:

```
$ git checkout v2
Updating files: 100% (47/47), done.
Filtering content: 3/5 files fetched (12.4 GiB / 18.2 GiB)
```

Progress line updated via `tracing` events that the CLI UI layer listens for. On a TTY, the line rewrites in place. On a non-TTY (CI, pipes), output is line-per-file for log-friendliness:

```
[crab] fetching models/base.safetensors (4.2 GiB)
[crab] fetching models/lora.bin (512 MiB)
[crab] fetching datasets/train.parquet (3.7 GiB)
```

Cached files don’t appear in the output — they’re instant.

-----

## 13. Dedup Decisions in Practice

### 13.1 The Decision Point

For each chunk in a new file during `git add` / push, we classify:

- **A: Identical chunk exists in an existing xorb in this repo.** Reference existing; do not upload.
- **B: Identical chunk exists in staging (another file added in this session).** Reference staged; will upload as part of this push.
- **C: Chunk is new to the repo.** Upload.

(Cross-repo dedup is out of scope for this section; covered in main design doc §10.)

### 13.2 The Lookup

On chunker output, for each chunk:

```rust
async fn classify_chunk(
    chunk: &Chunk,
    index: &ChunkIndex,
    staging: &StagingState,
) -> ChunkClassification {
    if let Some(xorb_ref) = index.lookup(&chunk.hash) {
        return ChunkClassification::ExistingRepo(xorb_ref);
    }
    if let Some(staging_ref) = staging.lookup(&chunk.hash) {
        return ChunkClassification::Staging(staging_ref);
    }
    ChunkClassification::New
}
```

Latencies:

- A lookup in an in-memory ChunkIndex: ~100 ns.
- A lookup in staging’s SQLite: ~100 µs.

Both dwarfed by chunking itself (~128 µs per 64 KiB chunk at 500 MB/s).

### 13.3 Per-File Plan Construction

```rust
struct FilePlan {
    file_hash: FileHash,
    size: u64,
    /// Ordered list of chunks with classification
    chunks: Vec<(Chunk, ChunkClassification)>,
    /// For dedup reporting
    stats: FileStats,
}

struct FileStats {
    total_bytes: u64,
    new_bytes: u64,
    reused_bytes: u64,
}
```

The plan is passed to the xorb packer (§6.2), which decides groupings.

### 13.4 Reporting Dedup

Users get dedup info on push:

```
$ git push
Enumerating objects: 3, done.
Counting objects: 100% (3/3), done.
Writing objects: 100% (3/3), 200 bytes, done.

crab: staging analysis
  Files to upload: 2
    models/v2.safetensors   10.4 GiB  [  12% new, 88% deduplicated]
    datasets/train.parquet   2.1 GiB  [   0% new, 100% deduplicated]
  Total:                    12.5 GiB   1.25 GiB new, 11.2 GiB deduplicated

crab: uploading 1.25 GiB to S3...
  [=========>              ] 42%  524 MiB / 1.25 GiB   85 MiB/s   ETA 0:12

crab: push complete
  Uploaded:     1.25 GiB (21 xorbs)
  Deduplicated: 11.2 GiB (saved transfer and storage)
```

### 13.5 Edge Case: Chunk in Staging but Also in Repo

A user adds a file, then adds another file with overlapping chunks, then pushes. A chunk might exist in both staging (from the first file’s `git add`) and already be in the repo (from a previous push).

Resolution: prefer the repo reference (class A) over staging (class B). This avoids an unnecessary upload even for a chunk that happens to be in staging.

Implementation: check repo index first, then staging.

### 13.6 Edge Case: Same Content, Different Files in Same Push

User adds `foo.bin` and `foo-copy.bin`, identical content. Two chunkings produce identical chunks. The second file’s chunks all classify as B (staging) since the first file’s chunks entered staging during its chunking.

The shard for this push records both files with identical reconstruction terms, pointing at the same xorbs. Storage cost: one copy.

### 13.7 When Dedup Hurts

In rare cases, dedup can be net-negative:

- Tiny files that happen to share a chunk with a huge xorb: reading the tiny file requires fetching the huge xorb’s Range. A few KB read becomes a several-MB fetch.

Mitigation: the 25% threshold (§6.2). Tiny shared chunks in small files don’t trigger dedup.

- Highly random binary that happens to dedup by coincidence: the dedup savings are real (a few chunks) but the organizational overhead is annoying.

Mitigation: none really needed; the system handles this correctly, just not dramatically.

-----

## 14. Performance Engineering

### 14.1 The Numbers to Beat

Git-LFS on a repo with a 10 GB checkpoint, ~5% churn per revision:

- Push: 10 GB upload per push. On 1 Gbps link: ~80 s just transfer, plus LFS overhead.
- Pull: 10 GB per pull. Same.
- Clone: 10 GB × N revisions (for full history, though users often filter to tip).

crab on the same workload:

- Push after initial: ~500 MB (5% of 10 GB) + overhead. Target: < 8 s transfer.
- Pull: ~500 MB for new chunks. Target: < 8 s.
- Clone (initial): 10 GB for tip + small fraction for history. Target: ~80 s.

### 14.2 Bottleneck Analysis

Per operation, what’s the likely bottleneck?

**Chunking (clean path):**

- Gearhash CPU: 500 MB/s single-threaded (SIMD).
- blake3 CPU: 3 GB/s single-threaded.
- Disk read: typically 500+ MB/s NVMe, 100+ MB/s SATA.
- **Bottleneck: disk read for big files, Gearhash for small fast-disk.**

**Upload (push path):**

- Chunk compression: 500 MB/s zstd-3 per core.
- Upload: network-bound in most cases.
- S3 multipart: parallelized, can saturate 10 Gbps.
- **Bottleneck: network, unless local CPU is weak.**

**Download (smudge path):**

- S3 Range GET throughput: ~50-100 MB/s per connection, ~1 GB/s with parallelism.
- Decompression: 1 GB/s zstd.
- Disk write: 500+ MB/s.
- **Bottleneck: network.**

### 14.3 Parallelism Defaults

```toml
[network]
upload_concurrency = 16       # parallel xorb uploads
download_concurrency = 16     # parallel xorb Range GETs
multipart_upload_parts = 8    # parallel parts within one multipart

[cpu]
chunker_workers = 4           # parallel chunkers for multi-file clean
compressor_workers = 0        # 0 = auto (cpu count)
```

Tuning guidance documented for users with different connections. Users on 10 Gbps+ links should increase `upload_concurrency` and `multipart_upload_parts`. Users on flaky connections may decrease them.

### 14.4 Prefetching

Two prefetch strategies:

**Shard prefetch on fetch**: after fetching pack-list, eagerly fetch shard-list and any shards referenced by the checkout that’s about to happen. Overlaps S3 latency with Git’s tree walk.

**Xorb prefetch on smudge**: when processing a queue of delayed smudge requests, start fetching xorbs for later requests while earlier ones are still streaming. Bounded by memory (don’t prefetch more than ~1 GiB ahead).

### 14.5 Memory Budget

Target memory footprint during normal operations:

- Filter process baseline: ~50 MB (Rust binary, loaded shards).
- Per-file-in-flight: ~4 MB (chunking buffer + some headroom).
- Chunk cache RAM: configurable, default 256 MB in-memory LRU over the disk cache.
- ChunkIndex: scales with repo chunk count (see main design §10.3).

For a 1 TB repo with 16M chunks, expect ~700 MB RAM total. For 10 TB (~160M chunks), ~6 GB — users with that scale should enable disk-backed chunk index.

### 14.6 Cold vs Warm Performance

**Cold start (first operation after machine reboot):**

- Chunk cache empty: every smudge is a full network fetch.
- Shard cache empty: every file adds shard fetch latency.
- Page cache empty: disk reads for local ops are slow.

**Warm state:**

- Chunk cache populated with recently-used chunks: many smudges are instant.
- Shard cache populated: dedup lookups are in-memory.

crab’s caches are designed to warm quickly: a single clone populates shard cache fully; typical working sets fit in chunk cache.

-----

## 15. Transparent UX Patterns

### 15.1 Invisible When Possible

The ideal crab session:

```
$ git clone crab://bucket/my-repo
Cloning into 'my-repo'...
Receiving objects: 100% (523/523), 50 MiB, done.
Filtering content: 100% (12/12), 48.3 GiB, done.

$ cd my-repo
$ # ... work, edit, commit ...
$ git push
Enumerating objects: 5, done.
Writing: 2.1 MiB delta, 845 MiB content (saved 40.2 GiB via dedup)
Done.
```

No config to edit, no `crab <subcommand>` calls, no thinking about chunks. Git speaks, crab responds.

### 15.2 Progress Indicators

Long operations show progress. Rules:

- **TTY output**: single updating line per active operation, with byte counts, percent, throughput, ETA.
- **Non-TTY output**: one line per file, with summary at end. Avoids carriage-return noise in CI logs.
- **Quiet mode** (`git -q` or `GIT_QUIET=1`): no progress; status-only at operation end.

Progress format:

```
Uploading [===========>          ] 48%  2.1 GiB / 4.4 GiB  82 MiB/s  ETA 0:28
```

### 15.3 Speaking Git’s Language

User-facing messages use Git vocabulary:

|Technical term   |User-facing term                   |
|-----------------|-----------------------------------|
|Xorb             |content block                      |
|Shard            |reconstruction metadata            |
|Chunk            |(usually hidden) byte range        |
|MerkleHash       |(usually hidden) content hash      |
|File-hash pointer|file tracked by crab             |
|CAS conflict     |ref update conflict / push rejected|

### 15.4 Error Messages

Bad:

```
ERROR: ShardNotFoundError { expected_hash: "a1b2c3..." }
```

Good:

```
ERROR: cannot find reconstruction data for file 'models/v2.safetensors'
       This usually means the file was pushed by an old client or the
       repository is corrupted.

       Try: crab fsck --repo crab://your-bucket/your-repo

       Error code: CRAB-E0037
```

### 15.5 Progressive Silence

First time a user does something, explain. After that, don’t.

```
$ crab track "*.safetensors"
Tracking pattern '*.safetensors' with crab.

Note: crab will now handle files matching this pattern.
Committed files with this extension will use content-defined chunking
for efficient storage. See `crab help track` for details.

$ crab track "*.parquet"
Tracking pattern '*.parquet'.
```

The second invocation assumes familiarity.

### 15.6 Autosuggestion

When crab detects a workflow issue, it suggests the fix:

```
$ git push
ERROR: push rejected — non-fast-forward update on refs/heads/main

Your local branch has diverged from the remote.
This happens when someone else has pushed to main since your last pull.

To resolve:
  git pull --rebase
  git push
```

Autosuggestions are opt-in via `crab.help.autosuggest = true` (default on).

-----

## 16. Failure Mode UX

### 16.1 Network Failures

**Transient (timeout, 503):**

```
crab: upload of xorb 12/18 failed (timeout), retrying in 2s...
crab: upload of xorb 12/18 failed (timeout), retrying in 4s...
crab: upload of xorb 12/18 succeeded
```

User sees retries but not individual failures. After N retries, fail hard:

```
crab: upload failed after 5 retries

The connection to S3 is unreliable. Your push has not been completed;
no changes have been made to the remote repository.

Try again when your connection is stable, or check status at:
  https://status.aws.amazon.com/

Error code: CRAB-E0041
```

**Permanent (403, 404 on critical read):**

```
crab: permission denied on s3://bucket/repo/refs/heads/main

Your credentials do not grant write access to this ref. Contact
the repository administrator or check your IAM policy.

Error code: CRAB-E0003
```

### 16.2 Partial Operations

A push that uploads 12 of 18 xorbs then fails:

- The 12 xorbs are on S3 but not referenced by any manifest. They’re orphans.
- No refs have been updated.
- The next push attempt idempotently re-issues the same uploads. HEAD on each xorb reveals which already exist; skip those. Re-upload missing 6.

User sees:

```
$ git push
# ... first attempt fails after 12 xorbs ...

$ git push   # second attempt
crab: resuming interrupted push
  12 chunks already uploaded, resuming from chunk 13
crab: upload complete
```

### 16.3 Corruption Detection

When crab fetches content and hash verification fails:

```
crab: detected corruption in xorb a1b2c3... (content hash mismatch)

This could indicate bit rot on S3, a bug in crab, or tampering.

Retrying with fresh fetch...
crab: corruption persists on fresh fetch

This is a serious problem. The file cannot be reconstructed.

Run `crab fsck --repo crab://...` for a full integrity report.

Error code: CRAB-E0050
```

### 16.4 Quota/Space Failures

Out of local disk space during staging:

```
ERROR: out of disk space in ~/.cache/crab/

crab needs 4.2 GiB of cache space to stage this operation, but
only 1.1 GiB is available.

Free up space or change cache location:
  crab cache prune        # evict old entries
  crab cache clean        # remove everything (safe)
  export CRAB_CACHE_DIR=/path/to/bigger/disk

Error code: CRAB-E0060
```

Out of S3 space (bucket quota, rare):

```
ERROR: S3 storage quota exceeded

The bucket has reached its storage limit. Contact your cloud
administrator or delete unneeded repositories.

Error code: CRAB-E0061
```

### 16.5 Version Skew

User with old crab client tries to push to a repo where `required_cli_version` was bumped:

```
ERROR: this repository requires crab >= 0.5.0

You are running crab 0.3.2.

To upgrade:
  brew upgrade crab            # Homebrew
  cargo install crab --force   # from cargo

Error code: CRAB-E0020
```

-----

## 17. Progressive Disclosure

### 17.1 The Three User Tiers

**Tier 1: First-time user.** Has never used crab. Expects Git to work. Should succeed with `crab init`, `crab track`, and normal Git commands.

**Tier 2: Frequent user.** Uses crab daily. Wants useful feedback on pushes (dedup stats, size saved). Occasionally needs to check cache status.

**Tier 3: Power user.** Manages many repos, automates, debugs issues. Wants full inspection (`crab debug`, `crab fsck`, `crab stat`), metrics, low-level knobs.

crab’s CLI is structured so each tier gets what they need without overwhelming the next-lower tier.

### 17.2 Command Surface by Tier

**Tier 1 visible commands:**

```
crab init <url>
crab track <pattern>
crab help
```

**Tier 2 adds:**

```
crab stat
crab cache stats
crab untrack <pattern>
crab track --preset <name>
```

**Tier 3 adds (hidden from `--help` unless `--advanced`):**

```
crab fsck
crab gc
crab repack
crab debug <operation>
crab migrate --from lfs
crab staging clean
crab cache prune
crab cache verify
```

### 17.3 Help Text

```
$ crab --help
crab: serverless Git platform with large-file dedup

USAGE:
  crab <COMMAND>

COMMAND:
  init      Initialize a crab repository
  track     Configure crab to handle a file pattern
  stat      Show repository statistics
  help      Show help for a command

  Use `crab --advanced` to see additional commands for repository
  management, diagnostics, and migration.
```

```
$ crab --advanced
# ... full command list ...
```

### 17.4 Output Verbosity

Default output is concise. `-v` adds detail; `-vv` adds more; `-vvv` is debug:

```
$ git push
(minimal, default)
  Writing: 845 MiB content (saved 40.2 GiB via dedup)
  Done.

$ CRAB_LOG=info git push
(info level)
  Staging: 3 files, 48.4 GiB total
  Dedup plan: 848 MiB new, 47.6 GiB deduplicated
  Uploading 21 xorbs (16 parallel)
  Updating refs: refs/heads/main
  Push complete

$ CRAB_LOG=debug git push
(debug: full tracing)
```

-----

## 18. Edge Cases

### 18.1 File Modes & Symlinks

Git tracks file modes (executable bit) and symlinks. These are metadata, not content, and are handled by Git in the tree object. crab pointer blobs represent file content; mode is preserved by Git’s own mechanism.

Symlinks: a symlink in Git is a blob whose content is the link target path. crab never treats symlinks as large files; the `.gitattributes` pattern on `*.bin` doesn’t match symlinks because symlink type is separate.

### 18.2 Rename & Copy Detection

Git detects renames/copies heuristically based on content similarity. With pointer blobs, two identical files (same file-hash) have identical pointers; Git sees them as the same content.

File rename detection works correctly: Git sees the same blob SHA (same pointer) at a different path. No issue.

File copy with chunk-level overlap (but different file-hashes): Git sees this as two different files. Git-level diff tools show content differences. At the xet level, the files share chunks and don’t double-store. This is the dedup win.

### 18.3 Merge Conflicts in Pointer Files

Can two branches modify the same large file, causing a merge conflict on the pointer?

Yes, if both branches change the file content. The pointer blobs have different file-hashes. Merging with standard Git produces a conflict in the pointer file (3 lines of text).

crab provides a merge driver:

```ini
[merge "crab"]
    driver = git-remote-crab merge %O %A %B %P
```

Configured in `.gitattributes`:

```
*.safetensors merge=crab
```

The merge driver can’t auto-resolve (the content is binary; we don’t know which version is “correct”), but it can emit a clear error with paths to each version’s reconstruction:

```
CONFLICT: merge conflict in models/v2.safetensors

Both branches modified this file. Manual resolution required.

Version in HEAD:     models/v2.safetensors.HEAD  (10.4 GiB, chunks cached)
Version in merging:  models/v2.safetensors.THEIRS (10.8 GiB, chunks cached)
Original (ancestor): models/v2.safetensors.BASE (10.2 GiB, chunks cached)

Choose a version with:
  git checkout --ours models/v2.safetensors    # or --theirs
  git add models/v2.safetensors
```

### 18.4 `.gitignore` vs Tracked Patterns

User has `big_file.safetensors` in `.gitignore` (shouldn’t be committed) but also has `*.safetensors` tracked by crab.

Git’s `.gitignore` wins at `git add` time: the file won’t be added, filter never runs. Correct behavior.

### 18.5 Interrupted Smudge

User presses Ctrl-C during a large checkout:

- Git sends SIGINT to the filter process.
- Filter catches SIGINT, flushes in-flight writes, exits.
- Git cleans up partial working-tree file (removes).
- Working tree now has some files with new content, some with old content.
- `git status` shows modifications; user can retry `git checkout` to finish.

Cache is unaffected — any chunks that arrived before interruption are still cached for the retry.

### 18.6 Very Large Single File

A 500 GB file stretches the design:

- Chunking: 500 GB / 64 KiB = ~8M chunks for this one file.
- Shard: ~8M term entries = large shard (tens of MB).
- Xorbs: 500 GB / 64 MiB = ~8000 xorbs.
- Smudge: many parallel Range GETs across 8000 xorbs.

The design scales to this, but not elegantly:

- Shard is large but parseable.
- Xorb enumeration: sorted by content, so Range GETs work.
- Download bandwidth: 500 GB at 1 Gbps = ~67 minutes. Unavoidable.

Tuning guidance: for single-file > 100 GB, increase `download_concurrency` to 32+ and `multipart_upload_parts` similarly.

### 18.7 Files That Aren’t Files

What if a tracked pattern matches a directory or a special file type?

- Directories: Git doesn’t store directories as blobs; filter isn’t called.
- FIFOs, devices: Git refuses to track them; filter isn’t called.
- Case: `.gitattributes` pattern `*.bin` matches `file.bin` (blob) and never `dir.bin` (if someone made a directory with that name).

No special handling needed; Git filters reach us only for blobs.

### 18.8 Empty Files

A tracked empty file:

- Chunker produces zero chunks.
- file-hash is blake3 of empty bytes = well-known constant.
- Pointer:
  
  ```
  version https://crab.dev/spec/v1
  file-hash af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262
  size 0
  ```
- Shard for this file has an entry with zero terms.
- Smudge produces zero bytes.

Edge case handled by the chunker accepting zero-length input.

### 18.9 Files Smaller Than the Threshold

A file matching a tracked pattern but under the `chunk_threshold_bytes` (default 1 MiB):

Two possible behaviors:

**A: Always process tracked files through filter.** Small tracked files become pointers, stored via xet. Consistent but adds overhead for small files.

**B: Bypass filter for files under threshold.** Small tracked files are stored as normal Git blobs. Inconsistent (file becomes a real blob at 999 KB, a pointer at 1.1 MB) but avoids overhead.

crab chooses A. Simplicity wins; the overhead for a 100 KB file is negligible (one chunk, one xorb that lives forever in a packing pool). Dedup still works.

-----

## 19. What Users Should Never See

Things that should not appear in normal UX, even in verbose output:

- The words “xorb,” “shard,” or “MerkleHash” in any successful-path message.
- Internal error types or Rust enum names.
- Hex hashes longer than 8 characters (truncate for display).
- Stack traces.
- HTTP status codes in error messages (translate to meaning).
- Chunk boundaries or chunking decisions.
- CAS retry counts (unless debug level).
- S3 bucket structure details (paths like `xet/xorbs/ab/cd...`).

If a user hits one of these, it’s a bug.

Things that *are* acceptable in UX:

- File paths (user gave them to us).
- File sizes in GB/MB.
- Percentages (dedup ratio).
- Throughput (MB/s).
- Counts (files, bytes).
- “Content block” / “tracking metadata” (user-friendly names).

-----

## 20. Testing UX

### 20.1 UX-Focused Tests

Automated tests for specific UX properties:

```rust
#[test]
fn success_path_produces_minimal_output() {
    let output = run_and_capture("git push", &[], &repo);
    // Success path should be < 10 lines, < 500 chars
    assert!(output.lines().count() < 10);
    assert!(output.len() < 500);
    // Must not contain technical jargon
    assert!(!output.contains("xorb"));
    assert!(!output.contains("shard"));
    assert!(!output.contains("MerkleHash"));
}

#[test]
fn errors_include_error_code() {
    let output = run_and_capture_err("git push", &[], &unauthorized_repo);
    assert!(output.contains("CRAB-E"));
}

#[test]
fn errors_include_suggestion() {
    let output = run_and_capture_err("git push", &[], &non_ff_repo);
    assert!(output.contains("git pull --rebase") || output.contains("try"));
}
```

### 20.2 User Testing Protocols

Manual tests run pre-release with real users:

- **New user**: given only `crab init` and `crab track`, can they set up a repo and do a push/pull cycle in 15 minutes? (Target: yes, 80% of devs.)
- **Migration**: can a user migrate a 100 GB LFS repo to crab in < 1 hour? (Target: yes.)
- **Error recovery**: simulated failures (disconnect network mid-push, corrupt cache). Can the user recover by following the suggested remediation? (Target: yes, 90% of cases.)

### 20.3 Benchmarks as UX

Performance is UX. If push feels slow, users blame crab even if S3 is the bottleneck. Publish:

- Benchmark suite comparing crab vs Git-LFS on canonical workloads.
- Expected latency for common operations.
- Clear communication of “this is a network-bound operation” vs “this is CPU-bound in crab.”

Users reading benchmarks form mental models of when crab is slow. Accurate mental models = less perceived slowness.

-----

**End of document.**
