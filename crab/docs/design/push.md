# crab — Push Pipeline Deep Dive

**How `git push` transforms local pointer blobs and staged chunks into
durable, deduplicated objects on cloud storage.**

-----

## Document Metadata

| Field        | Value                                                       |
|--------------|-------------------------------------------------------------|
| Project      | crab                                                        |
| Scope        | Push pipeline architecture, data flow, performance analysis |
| Status       | Living document                                             |
| Companion to | `Crab-overview.md` (full workflow), `Crab.md` (arch)        |
| Version      | 0.1                                                         |

-----

## Current publication contract

Native `crab push` and the Git remote helper execute one dependency state
machine. It snapshots published file recipes, classifies the cross-file unique
chunk set, and runs missing-payload read → xorb pack → origin upload through a
bounded backpressured channel while Git candidate locators are prepared in
parallel. CPU compression runs on blocking workers; every stage is drained and
joined before locks or staging handles are released.

Before manifest CAS, push union-registers the complete candidate shard set for
bucket GC, revalidates every referenced xorb against the canonical origin, and
builds a base-bound dependency plan and commit receipt. The manifest CAS is the
only publication point. File indexes, Git locators, and generation
receipts are written after CAS and are rebuildable with `crab metadb rebuild`.
Cache/index hits are candidates only; they never replace origin proof.

-----

## Table of Contents

- [crab — Push Pipeline Deep Dive](#crab--push-pipeline-deep-dive)
  - [Document Metadata](#document-metadata)
  - [Table of Contents](#table-of-contents)
  - [1. Overview](#1-overview)
  - [2. Entry Point: Git Remote Helper Protocol](#2-entry-point-git-remote-helper-protocol)
    - [Protocol Exchange](#protocol-exchange)
    - [Key Design Decisions](#key-design-decisions)
  - [3. Pre-Push: The Clean Filter (git add)](#3-pre-push-the-clean-filter-git-add)
    - [Clean Filter Data Flow](#clean-filter-data-flow)
    - [Staging Area Internals](#staging-area-internals)
    - [What the Push Pipeline Reads from Staging](#what-the-push-pipeline-reads-from-staging)
  - [4. The Push Pipeline](#4-the-push-pipeline)
    - [Pipeline Overview](#pipeline-overview)
    - [Step-by-Step Detail](#step-by-step-detail)
      - [Step 1: Enumerate Pointer Blobs](#step-1-enumerate-pointer-blobs)
      - [Step 2: Staging Lookup](#step-2-staging-lookup)
      - [Step 3: Classify Chunks](#step-3-classify-chunks)
      - [Step 5: Pack Xorbs](#step-5-pack-xorbs)
      - [Step 6: HEAD Check for Resume](#step-6-head-check-for-resume)
      - [Step 7: Parallel Xorb Uploads](#step-7-parallel-xorb-uploads)
      - [Step 8: Build Shard](#step-8-build-shard)
      - [Step 9: Upload Shards and MetaDB Entries](#step-9-upload-shards-and-metadb-entries)
      - [Step 10: Upload Git Pack](#step-10-upload-git-pack)
      - [Step 11: Build Manifest](#step-11-build-manifest)
      - [Step 12: Unified Manifest CAS](#step-12-unified-manifest-cas)
      - [Steps 13–14: Cleanup](#steps-1314-cleanup)
  - [5. End-to-End Data Flow Diagram](#5-end-to-end-data-flow-diagram)
    - [Full Push: Working Tree → Object Storage](#full-push-working-tree--object-storage)
    - [Data Dependencies Between Steps](#data-dependencies-between-steps)
  - [6. Phase Diagrams](#6-phase-diagrams)
    - [Phase 1: Classify](#phase-1-classify)
    - [Phase 2: Pack (Steps 5–6)](#phase-2-pack-steps-56)
    - [Phase 3: Upload (Steps 7–10)](#phase-3-upload-steps-710)
    - [Phase 4: Commit + Cleanup (Steps 11–14)](#phase-4-commit--cleanup-steps-1114)
  - [7. Coordination and Locking](#7-coordination-and-locking)
    - [Push Lock Protocol](#push-lock-protocol)
    - [Lock Lifecycle](#lock-lifecycle)
    - [Lock Expiry and Reclamation](#lock-expiry-and-reclamation)
    - [Heartbeat](#heartbeat)
    - [CAS (Compare-and-Swap) for Manifests](#cas-compare-and-swap-for-manifests)
  - [8. Object Store Layout](#8-object-store-layout)
    - [Object Mutability](#object-mutability)
  - [9. Failure Modes and Recovery](#9-failure-modes-and-recovery)
    - [Failure Matrix](#failure-matrix)
    - [Key Safety Properties](#key-safety-properties)
    - [Cancellation](#cancellation)
  - [10. Performance Analysis and Bottlenecks](#10-performance-analysis-and-bottlenecks)
    - [Latency Breakdown (Typical Push of 100 Files, 5 GB Total)](#latency-breakdown-typical-push-of-100-files-5-gb-total)
    - [Identified Bottlenecks](#identified-bottlenecks)
      - [Resolved (post-v0.1)](#resolved-post-v01)
      - [Partially Resolved](#partially-resolved)
        - [B1: Pointer Walk Coverage](#b1-pointer-walk-coverage)
        - [B10: Progress Reporting](#b10-progress-reporting)
      - [Open](#open)
        - [B3: Sequential Staging Reads](#b3-sequential-staging-reads)
        - [B12: Adaptive Xorb Sizing Feedback Latency](#b12-adaptive-xorb-sizing-feedback-latency)
    - [Bottleneck Priority Matrix](#bottleneck-priority-matrix)
  - [11. Hardening Roadmap](#11-hardening-roadmap)
    - [Phase 1: Remaining Correctness](#phase-1-remaining-correctness)
    - [Phase 2: Walk and Read Performance](#phase-2-walk-and-read-performance)
    - [Phase 3: Scale (100K+ Files)](#phase-3-scale-100k-files)
  - [12. Configuration Reference](#12-configuration-reference)
    - [Push-Related Config Keys](#push-related-config-keys)
    - [Push Lock Config](#push-lock-config)
    - [CAS Config](#cas-config)
    - [Xorb Packing Config](#xorb-packing-config)
  - [13. Source Map](#13-source-map)
  - [14. Concurrent Push Scenarios](#14-concurrent-push-scenarios)
    - [Scenario 1: Two Pushers to the Same Ref (Serial)](#scenario-1-two-pushers-to-the-same-ref-serial)
    - [Scenario 2: Two Pushers to the Same Ref (Late-Lock, Historical)](#scenario-2-two-pushers-to-the-same-ref-late-lock-historical)
    - [Scenario 3: Two Pushers, Lock Contention (Current: Early Lock)](#scenario-3-two-pushers-lock-contention-current-early-lock)
    - [Scenario 4: Pusher Crashes Mid-Upload](#scenario-4-pusher-crashes-mid-upload)
    - [Scenario 5: Manifest CAS Conflict](#scenario-5-manifest-cas-conflict)
    - [Scenario 6: Push to Different Refs (No Contention)](#scenario-6-push-to-different-refs-no-contention)
  - [15. Worked Example: Pushing a 2 GB Model Update](#15-worked-example-pushing-a-2-gb-model-update)
    - [Setup](#setup)
    - [Step 1: Enumerate Pointers](#step-1-enumerate-pointers)
    - [Step 3: Shard Sync](#step-3-shard-sync)
    - [Step 4: Classify](#step-4-classify)
    - [Step 5: Pack Xorbs](#step-5-pack-xorbs-1)
    - [Step 7: Upload Xorbs](#step-7-upload-xorbs)
    - [Step 8: Build Shard](#step-8-build-shard-1)
    - [Step 9: Upload Shards + MetaDB](#step-9-upload-shards--metadb)
    - [Step 10: Upload Git Pack](#step-10-upload-git-pack-1)
    - [Step 11–12: Build Manifest + Unified Manifest CAS](#step-1112-build-manifest--unified-manifest-cas)
    - [Total Push Time](#total-push-time)
  - [16. Streaming Packer Architecture](#16-streaming-packer-architecture)
    - [Sequential Pipeline (v1 baseline)](#sequential-pipeline-v1-baseline)
    - [Streaming Pipeline (default)](#streaming-pipeline-default)
    - [Channel-Based Design](#channel-based-design)
    - [Benefits](#benefits)
    - [When to Use Which](#when-to-use-which)
  - [17. Comparison: crab Push vs Git-LFS Push](#17-comparison-crab-push-vs-git-lfs-push)
    - [Push Size Comparison (2 GB File, 10% Changed)](#push-size-comparison-2-gb-file-10-changed)
    - [Push Size Comparison (2 GB File, First Push)](#push-size-comparison-2-gb-file-first-push)
  - [18. Observability and Metrics](#18-observability-and-metrics)
    - [Push Metrics (Emitted by `Metrics` Struct)](#push-metrics-emitted-by-metrics-struct)
    - [Tracing Spans](#tracing-spans)
    - [Recommended Dashboards](#recommended-dashboards)
  - [19. Invariants Checklist](#19-invariants-checklist)
  - [20. Advanced Capabilities (post-v0.1)](#20-advanced-capabilities-post-v01)
    - [20.1 Streaming Packer (`streaming_classify_pack_upload`)](#201-streaming-packer-streaming_classify_pack_upload)
    - [20.2 Adaptive Compression](#202-adaptive-compression)
    - [20.3 Adaptive Concurrency](#203-adaptive-concurrency)
    - [20.4 Adaptive Xorb Sizing](#204-adaptive-xorb-sizing)
    - [20.5 Cache-Service Dedup Tier](#205-cache-service-dedup-tier)
    - [20.6 Native Push Orchestrator (`run_native_push`)](#206-native-push-orchestrator-run_native_push)
    - [20.7 Push-State Tracking for Incremental Walks](#207-push-state-tracking-for-incremental-walks)
  - [21. Updated Source Map](#21-updated-source-map)
  - [22. Document Status Note](#22-document-status-note)

-----

## 1. Overview

A `git push` in crab does two fundamentally different things compared to
a normal Git push:

1. **Uploads chunk data** — xorbs (compressed chunk aggregates), shards
   (reconstruction metadata), and MetaDB file/chunk-index entries are written
   to object storage. This is the "data plane."

2. **Uploads Git objects** — one or more bounded standard Git packfiles
   containing commits, trees, and pointer blobs are written to object storage.
   This is the "control plane."

Both must be durable before any ref is moved. This ordering invariant is
the single most important property of the push pipeline:

> **Invariant: All immutable data is durable before any ref moves.**
>
> An interrupted push may leave orphaned xorbs or packs (cleaned up by GC)
> but never creates dangling references (refs pointing to missing data).

The pipeline is implemented in `PushPipeline` (`crab/src/git/push.rs`).
Pre-push shard sync has been removed from the classify path; chunk dedup
classification reads the MetaDB-backed `chunk_index_db` instead of downloading
the remote shard list during push.

-----

## 2. Entry Point: Git Remote Helper Protocol

When a user runs `git push`, Git spawns `git-remote-crab` and
communicates via stdin/stdout using the remote helper protocol:

```
┌──────┐                              ┌─────────────────────┐
│  Git │  stdin/stdout line protocol  │  git-remote-crab    │
│      │◄────────────────────────────►│  (remote helper)    │
└──────┘                              └─────────┬───────────┘
                                                │
                                    ┌───────────▼───────────┐
                                    │   run_remote_helper() │
                                    │   remote_helper.rs    │
                                    └───────────┬───────────┘
                                                │
                                    ┌───────────▼───────────┐
                                    │   dispatch_batch()    │
                                    │   Batch::Push(specs)  │
                                    └───────────┬───────────┘
                                                │
                                    ┌───────────▼───────────┐
                                    │   run_push_batch()    │
                                    │   push.rs             │
                                    └───────────────────────┘
```

### Protocol Exchange

```
git → helper:  capabilities
helper → git:  fetch\npush\noption\n\n

git → helper:  option progress true
helper → git:  ok

git → helper:  list for-push
helper → git:  {sha} refs/heads/main\n\n
               (reads refs/ objects from S3)

git → helper:  push refs/heads/main:refs/heads/main
               push refs/heads/feature:refs/heads/feature
               (blank line terminates batch)

helper:        ── executes 14-step push pipeline ──

helper → git:  ok refs/heads/main
               ok refs/heads/feature
               (blank line terminates response)
```

### Key Design Decisions

- **Batch semantics.** Git sends all push refspecs in a single batch
  (terminated by a blank line). The pipeline processes them atomically:
  if any step fails before ref CAS, all refs are marked as errors.

- **Session cache.** The helper maintains a `SessionCache` across the
  protocol loop to avoid redundant config resolution, pack-list fetches,
  and commit-graph probes within a single `git push` invocation.

- **S3 store creation.** The `Store` is created once from the parsed
  `crab://bucket/repo` URL and reused for all operations.

**Source:** `crab/src/git/remote_helper.rs`

-----

## 3. Pre-Push: The Clean Filter (git add)

Before push can happen, the clean filter must have run during `git add`.
Understanding this phase is essential because it determines what data is
available to the push pipeline.

### Clean Filter Data Flow

```
Working Tree File (e.g. model.safetensors, 10 GB)
    │
    ▼  Git sends content to filter-process ("clean" command)
    │
    ├── Single-pass: blake3 hash + CDC chunking simultaneously
    │   ┌─────────────────────────────────────────────────┐
    │   │  for block in content.chunks(128 KiB):          │
    │   │      file_hasher.update(block)                  │
    │   │      cdc_chunks.extend(chunker.feed(block))     │
    │   │  file_hash = file_hasher.finalize()             │
    │   │  last_chunk = chunker.finalize()                │
    │   └─────────────────────────────────────────────────┘
    │
    ├── Fast-path check (large files only, ≥64 MiB):
    │   bloom filter → file-index HEAD → skip staging if known
    │
    ├── Stage chunks to local disk:
    │   ┌─────────────────────────────────────────────────┐
    │   │  .crab/staging/                                 │
    │   │  ├── segments/current.seg   (append-only)       │
    │   │  ├── index.db               (SQLite WAL mode)   │
    │   │  └── lockfile               (advisory flock)    │
    │   └─────────────────────────────────────────────────┘
    │   Batch API: one writer lock + one SQLite txn per file
    │
    └── Emit pointer blob → Git ODB (~200 bytes)
        ┌─────────────────────────────────────────────────┐
        │  version https://crab.dev/spec/v1               │
        │  file-hash 7c1f2a3b...  (blake3, 64 hex chars)  │
        │  size 10737418240                               │
        │  shard-hint a1b2c3d4...  (optional)             │
        └─────────────────────────────────────────────────┘
```

### Staging Area Internals

The staging area is the bridge between `git add` (clean) and `git push`.
Chunks are written to append-only segment files and indexed in SQLite:

```
Segment File Layout:
┌──────────┬──────────┬──────────┬─────┐
│ Frame 0  │ Frame 1  │ Frame 2  │ ... │
└──────────┴──────────┴──────────┴─────┘

Each Frame:
┌──────────────┬─────────────────────────────────────────┐
│ length: u32  │ chunk data: [u8; length] │ crc32: u32   │
└──────────────┴─────────────────────────────────────────┘

SQLite Index (index.db):
┌─────────────────────────────────────────┐
│ files(file_hash, total_bytes)           │
│ chunks(chunk_hash, file_hash, seg_id,   │
│        seg_offset, size, chunk_index)   │
│ pending_chunks(...)  ← pre-flush buffer │
│ segments(id, status, size_bytes,        │
│          live_chunk_count)              │
│ file_push_plans(file_hash, plan_json)   │
│ prepared_xorbs(...)                     │
│ prepared_xorb_chunks(chunk_hash, ...)   │
└─────────────────────────────────────────┘
```

### What the Push Pipeline Reads from Staging

- `load_file_push_plan(file_hash)` → verified add-time plan, when it still
  matches the staged chunk rows
- `chunks_for_file(file_hash)` → ordered list of chunk hashes for a file
- `get_chunk(chunk_hash)` → raw chunk bytes (pread + CRC + blake3 verify)

The plan path is a fast path, not the durability boundary. Push may adopt
prepared xorbs referenced by a verified add-time plan, but segment files remain
the source of staged bytes for fallback packing and re-verification. If the plan
row or prepared xorb payload is missing or stale, push reads from `segments/`
through the chunk locator rows and packs the chunks normally. Successful
post-push cleanup retires chunk rows and removes the indexed add-time plan and
prepared-xorb candidates for the pushed file hash.

**Source:** `crates/crab-staging/src/lib.rs`,
`crates/crab-staging/src/index.rs`, `crab/src/git/push.rs`

-----

## 4. The Push Pipeline

The push pipeline is the core of `git push`. It is implemented as
`PushPipeline` in `push.rs` and grouped into four phases.

### Pipeline Overview

```
                    ┌─────────────────────────────────────┐
                    │         PUSH PIPELINE               │
                    │         PushPipeline                │
                    │                                     │
  ┌─ CLASSIFY ──────┤  1. enumerate_pointers              │
  │                 │  2. lookup_staging                  │
  │                 │  3. classify_chunks                 │
  │                 ├─────────────── cancel check ────────┤
  │                 ├─────────────── acquire push lock ───┤  ← moved here
  │                 │                                     │
  ├─ PACK ──────────┤  5. pack_xorbs                      │
  │  (steps 5–6)    │  6. head_check_resume               │
  │                 ├─────────────── cancel check ────────┤
  │                 │                                     │
  ├─ UPLOAD ────────┤  7. upload_xorbs  (parallel)        │
  │  (steps 7–10)   │  8. build_shard                     │
  │                 │  9. upload_shard_and_file_index     │
  │                 │ 10. upload_packs                    │
  │                 ├─────────────── cancel check ────────┤
  │                 │                                     │
  ├─ COMMIT ────────┤ 11. build_manifest                  │
  │  (steps 11–12)  │ 12. unified_manifest_cas            │
  │                 │                                     │
  └─ CLEANUP ───────┤ 13. post_success_cleanup            │
     (steps 13–14)  │ 14. on_failure (error path only)    │
                    └─────────────────────────────────────┘

The push lock is acquired *between* classify and pack, not in step 12 as
in earlier drafts. Concurrent pushers that lose the lock race fail
immediately instead of redundantly uploading data. See §7.
```

### Step-by-Step Detail

#### Step 1: Enumerate Pointer Blobs

**Purpose:** Discover which files (pointer blobs) need to be pushed.

**How it works:**

```
git rev-parse refs/heads/main  ──►  tip SHA
        │
        ▼
gix-traverse commit walk (from tips)
        │
        ├── for each commit:
        │     read tree object
        │     breadth-first tree walk (gix-traverse)
        │       │
        │       ├── blob ≤ 256 bytes? → try Pointer::parse()
        │       │     ├── valid pointer → add to pointers[]
        │       │     └── not a pointer → skip
        │       │
        │       └── blob > 256 bytes → skip (not a pointer)
        │
        └── collect CommitEntry(oid, parents, gen_number)
            for the manifest-pinned split commit graph
```

**Data produced:**
- `pointers: Vec<PointerBlob>` — `{oid, file_hash, size}` per pointer
- `commit_entries: Vec<CommitEntry>` — `{oid, gen_number, parents}` per commit

**Performance note:** This walks ALL reachable commits and ALL blobs,
not just the delta since the last push. For a repo with 100K files and
deep history, this is O(commits × blobs) — the single largest latency
contributor. See §10 for the incremental walk proposal.

**Source:** `crates/crab-git/src/walk.rs` (`walk_reachable`),
`crab/src/git/push.rs` (`enumerate_pointers`)

#### Step 2: Staging Lookup

**Purpose:** Verify that staged chunks exist for each pointer.

**How it works:** For each pointer, query the staging area's
`chunks_for_file(file_hash)` to confirm chunks are available. Pointers
with no staged chunks are logged (they'd typically come from files
pushed in an earlier session; the chunks are already on the remote and
flow through the class-A path in step 4).

```
for ptr in pointers:
    chunks = staging.chunks_for_file(ptr.file_hash)
    if chunks.is_empty(): missing += 1 else: verified += 1
```

**Source:** `crab/src/git/push.rs` (`lookup_staging`)

#### Step 3: Classify Chunks

**Purpose:** Categorize each chunk into one of three classes:

```
┌─────────────────────────────────────────────────────────┐
│  Class A (Existing): chunk already on remote            │
│    → skip entirely (no pack, no upload)                 │
│    → detected via session ChunkIndex + MetaDB           │
│      chunk_index_db batch lookup                        │
│                                                         │
│  Class B (Staged): chunk seen earlier in this push      │
│    → already packed in a prior xorb this push           │
│    → skip packing again (session dedup)                 │
│                                                         │
│  Class C (New): chunk not yet on remote, not yet seen   │
│    → needs packing into xorb and upload                 │
└─────────────────────────────────────────────────────────┘
```

**How it works:**

```
dedup_ctx = DedupContext {
    chunk_index,          // session-local verified hits
}
classifier = Classifier::new()

for ptr in pointers:
    for chunk_hash in staging.chunks_for_file(ptr.file_hash):
        match classifier.classify_with_context(chunk_hash, &dedup_ctx):
            ChunkClass::Existing → Class A
            ChunkClass::Staged   → Class B (already seen this push)
            ChunkClass::New      → Class C
        classifier.mark_seen(chunk_hash)

// New chunks are batch-looked-up in MetaDB chunk_index_db. Verified hits
// move from Class C to Class A before step 5.
global_hits = chunk_index_store.get_batch(new_set).await
if chunk_index populated or global_hits found:
    new_chunk_hashes = new_set
```

**Dedup ratio** is emitted as a structured field:
`dedup_ratio = (existing + staged) / total_chunks`.

**Source:** `crab/src/git/push.rs` (`classify_chunks`);
`crab/src/engine/dedup.rs` (`Classifier`, `DedupContext`, `ChunkClass`)

#### Step 5: Pack Xorbs

**Purpose:** Read staged chunks and compress them into xorbs (chunk
aggregates) ready for upload.

**How it works:**

```
for each pointer in pointers[]:
    │
    ├── chunks_for_file(file_hash) → [chunk_hash_0, chunk_hash_1, ...]
    │
    └── for each chunk_hash:
            get_chunk(chunk_hash) → raw bytes
                │
                └── XorbBuilder::push(chunk, run_id)
                        │
                        ├── zstd-3 compress
                        ├── session dedup (skip if hash seen)
                        ├── accumulate into current xorb
                        └── finalize xorb at 64 MiB target
                            ┌──────────────────────────────┐
                            │  Xorb Binary Format:         │
                            │  [compressed chunk 0]        │
                            │  [compressed chunk 1]        │
                            │  ...                         │
                            │  [chunk metadata entries]    │
                            │    hash(32) + offset(4)      │
                            │    + comp_len(4)             │
                            │    + uncomp_len(4)           │
                            │  [footer]                    │
                            │    num_chunks(4)             │
                            │    meta_offset(8)            │
                            │    magic "XORB"(4)           │
                            └──────────────────────────────┘
```

**Run-continuity:** The `XorbBuilder` keeps chunks from the same source
file (identified by `RunId`) together within a xorb. A run break is only
allowed after accumulating at least 1 MiB from the current run. This
improves reconstruction locality — fetching a single xorb often provides
all chunks needed for a file.

**Chunk filtering:** When step 4 produced a `new_chunk_hashes` set
(i.e. at least one dedup tier was populated), `pack_xorbs` skips any
chunk not in that set. Class-A and class-B chunks never reach the
builder. When no dedup tier is populated, step 5 packs every chunk
as a fallback.

**Batch reads:** Chunks are read from staging in batches of
`PushConfig::batch_read_size` (default 256). Each batch is one
vectored pread over the segment files plus one SQLite query — much
faster than per-chunk reads.

**After packing:** `XorbBuilder::finalize` returns each xorb's
placement info inline (`XorbResult.placements`). The pipeline
constructs the `ChunkPlacementMap` directly from the results without
re-parsing xorb bytes:

```
chunk_hash → ChunkPlacement {
    xorb_hash,
    chunk_index,
    uncompressed_size,
}
```

This map is consumed by step 8 (shard build) to create reconstruction
terms.

**Source:** `crab/src/git/push.rs` (`pack_xorbs`),
`crab/src/storage/xorb/builder.rs` (`XorbBuilder`, `XorbResult`)

#### Step 6: HEAD Check for Resume

**Purpose:** Query the remote for already-uploaded xorbs and skip them.
This enables retry resilience — if a push fails after uploading 90% of
xorbs, the retry only uploads the remaining 10%.

**How it works:**

```
planned_xorb_hashes = [hash_0, hash_1, ..., hash_N]
    │
    ▼
head_batch(&planned, &head_store, &HeadBatchConfig {
    concurrency: push.head_check_concurrency (default 64),
    max_retries: push.max_retries,
})
    │
    ├── For small batches: parallel HEAD requests
    │   HEAD {prefix}/xorbs/{hash_0}
    │   HEAD {prefix}/xorbs/{hash_1}
    │   …
    │
    ├── For large batches: LIST prefix sweep + filter locally
    │   (crosses over at adaptive threshold to cap total requests)
    │
    └── Returns HeadBatchResult { existing, head_check_errors }

verified = verify remote xorb metadata + referenced chunk payloads
retain verified xorb bytes for post-success local cache warming
xorbs.retain(|x| !verified.contains(&x.hash))
metrics.xorbs_skipped += verified.len()
metrics.head_check_errors += errors
```

**Cancel-safe.** In-flight HEAD requests are dropped on cancellation;
no remote state mutates.

**Source:** `crab/src/git/push.rs` (`head_check_resume`),
`crab/src/storage/head_batch.rs` (`head_batch`, `HeadBatchConfig`,
`StoreHeadBatch`)

#### Step 7: Parallel Xorb Uploads

**Purpose:** Upload xorbs to object storage with bounded concurrency.

**How it works:**

```
               ┌─────────────────────────┐
               │  Semaphore (16 permits) │
               └─────────────┬───────────┘
                             │
              ┌──────────────┼────────────┐
              │              │            │
         ┌────▼────┐   ┌────▼────┐   ┌────▼────┐
         │ Upload  │   │ Upload  │   │ Upload  │  ...
         │ xorb 0  │   │ xorb 1  │   │ xorb 2  │
         └────┬────┘   └────┬────┘   └────┬────┘
              │             │             │
              ▼             ▼             ▼
         ┌─────────────────────────────────────┐
         │  S3: {prefix}/xorbs/{hash}          │
         │                                     │
         │  ≤ 8 MiB → single PUT               │
         │  > 8 MiB → multipart upload         │
         │            (8 MiB parts)            │
         └─────────────────────────────────────┘
```

**Multipart upload:** Xorbs larger than 8 MiB use `put_multipart` from
the `object_store` crate. Data is split into 8 MiB parts and uploaded
via the S3 multipart upload API, avoiding single-PUT timeouts.

**Error handling:** Each upload runs in a `tokio::spawn` task. A
`CasConflict` error (xorb already exists) is treated as success. Any
other error fails the entire push.

**Source:** `crab/src/git/push.rs` (`upload_xorbs`)

#### Step 8: Build Shard

**Purpose:** Create MDB shards containing xorb CAS info and file
reconstruction terms.

**How it works:**

```
ChunkPlacementMap (from step 5)
    │
    ├── Group placements by xorb_hash, sort by chunk_index
    │
    ├── For each xorb:
    │     XorbChunkSequenceHeader(xorb_hash, num_chunks, total_uncompressed)
    │     XorbChunkSequenceEntry(chunk_hash, uncompressed_size, byte_offset)
    │     → shard_session.add_xorb(MDBXorbInfo)
    │
    ├── For each pointer:
    │     chunks_for_file(file_hash) → [chunk_hash_0, ...]
    │     build_file_terms(chunk_hashes, placement_map)
    │       → coalesce consecutive chunks in same xorb
    │       → Vec<FileTerm { xorb_hash, chunk_start, chunk_end, bytes }>
    │     FileDataSequenceHeader + FileDataSequenceEntry[]
    │     → shard_session.add_file(MDBFileInfo)
    │
    └── shard_session.finalize()
          → Vec<(shard_bytes, shard_hash)>
          (splits at 100 MiB soft cap)
```

**File reconstruction terms** are the key data structure. They describe
how to reconstruct a file from xorb chunks:

```
File: model.safetensors (file_hash = 7c1f2a3b...)
Reconstruction Terms:
  ┌─────────────────────────────────────────────────────┐
  │ Term 0: xorb_hash=abc123, chunks [0..47], 3.0 MiB   │
  │ Term 1: xorb_hash=def456, chunks [0..12], 0.8 MiB   │
  │ Term 2: xorb_hash=abc123, chunks [47..94], 3.0 MiB  │
  │ ...                                                 │
  └─────────────────────────────────────────────────────┘
  Concatenate decompressed chunks in order → original file
```

**Source:** `crab/src/git/push.rs` (`build_shard`),
`crab/src/metadata/shard.rs` (`PushShardSession`)

#### Step 9: Upload Shards and MetaDB Entries

**Purpose:** Upload shard files, then atomically commit the per-repo
`file_index_db` and `chunk_index_db` entries.

**How it works:**

```
For each shard (from step 8)
    ↓ parallel, bounded by upload_concurrency
{prefix}/.crab/shards/{first-two-hex}/{shard_hash}
    ↓
MetaDB transaction:
    file_index_db: file_hash → shard_hash
    chunk_index_db: chunk_hash → xorb_ref
```

The legacy per-file `file-index/{file_hash}` PUT loop is gone. File-hash
lookups now use the per-repo `file_index_db`, which avoids cross-repo local
cache collisions and commits file/chunk metadata as one remote transaction.

**Local + optional remote cache warming.** Immediately after uploading,
the uploaded shard bytes and committed chunk-index entries are warmed:

1. Shard bytes go to the local content-addressed cache
   (`~/.cache/crab/shards/...`) so the next hydrate/diff reads locally.
2. Chunk mappings go to the per-repo `PersistentChunkIndex` SQLite tier.
3. The cache service, if one is configured
   (`CachingStore::warm_remote_only`), so peers reading via the cache hit warm
   shard data.

Uploaded xorb bytes are warmed in step 13 after the push succeeds, using the
retained `Bytes` payloads from step 7 or remote-verified step 6 resume.

Cache-warm failures are non-fatal and logged at `debug`/`warn` — a
failed cache PUT never aborts a successful push.

**Completeness guard.** Every pointer surviving step 8 must have an
entry in the `file_shard_index`. Missing entries throw
`CrabError::IncompleteShardReconstruction` with
`example_chunk_index: u32::MAX` as the sentinel distinguishing this
defense-in-depth check from an upstream chunk-coverage gap.

**Source:** `crab/src/git/push.rs` (`upload_shard_and_file_index`)

#### Step 10: Upload Git Pack

**Purpose:** Generate and upload standard Git packfiles containing
commits, trees, and pointer blobs — excluding objects already on the
remote.

**How it works:**

```
1. Compute remote object set:
   GET {prefix}/manifest, then its segmented pack inventory
   For each selected `PackManifestEntry`:
     parse local .git/objects/pack/pack-{id}.idx
     → HashSet<ObjectId> of objects already on remote
   (falls back to full pack if the remote pack list or local idx
   files are unavailable)

2. Generate an incremental bounded pack set via Git:
   git pack-objects --revs --max-pack-size={receive.maxInputSize} {base_name}
   → one or more independent non-thin packs containing only new commits,
     trees, and pointer blobs. The limit applies to each pack. One object that
     cannot fit still fails with `pack-too-large`.

3. For every generated pack, upload:
   PUT {prefix}/packs/pack-{blake3_hash}.pack
       body = pack_bytes (multipart if > 8 MiB)

   PUT {prefix}/packs/pack-{blake3_hash}.idx
       body = verified canonical Git index

   PUT {prefix}/packs/pack-{blake3_hash}.meta
       body = PackMetadata JSON { pack_id, ref_tips, object_count }

4. Install every pack locally and derive exact locators:
   Copy each pack + idx into .git/objects/pack/ so the NEXT push can
   compute an incremental remote-object set without a round-trip.
   Index construction, checksum binding, and canonical `.idx` upload are
   pre-commit requirements. All pack entries are appended to one candidate
   pack index and become visible through one manifest CAS. After that CAS,
   exact offset/length/CRC rows for the whole set are published through one
   renewed `git_object_catalog_db` writer session. Dense object ordinals and
   the matching visibility proof are published after the manifest CAS;
   publication failure is repairable acceleration damage and does not roll
   back committed refs.
```

**Source:** `crab/src/git/push.rs` (`upload_packs`, `compute_remote_objects`),
`crab/src/git/pack.rs` (`generate_push_pack_files_with_exclusions`,
`generate_pack_files_from_object_ids`, `install_pack_file_locally_with_timeout`)

#### Step 11: Build Manifest

**Purpose:** Build the unified manifest pointer and bulk data objects
from the current repo state plus the push's new data.

**How it works:**

1. Read the current manifest pointer (loaded at push start).
2. Load the current bulk shard list and pack list by following the
   content hashes in the base manifest.
3. Clone the base manifest and increment the generation.
4. Apply ref updates from the push specs:
   - Empty `src` → delete the ref from the map.
   - Non-empty `src` → resolve to SHA via `batch_rev_parse`, insert.
5. Append new shard hashes (from step 9) to the bulk shard list.
6. Append every new pack entry (from step 10) to the bulk pack list.
7. Serialize the bulk lists, compute blake3 content hashes, and update
   the pointer's `shard_list_hash` and `pack_list_hash`.
8. Return the new manifest pointer and the `BulkData` struct containing
   the serialized bulk objects and their hashes.

**Data produced:**
- `Manifest` — the new manifest pointer (JSON-serializable).
- `BulkData` — `{ shard_list: (hash, bytes), pack_list: (hash, bytes) }`.

**Source:** `crab/src/git/push.rs` (`build_manifest`)

#### Step 12: Unified Manifest CAS

**Purpose:** Upload bulk data objects, then CAS-write the manifest
pointer as the single atomic commit point for the entire push.

**How it works:**

1. Upload bulk data objects (`shard-list-{hash}`, `pack-list-{hash}`)
   via `upload_bulk_if_absent`. These are content-addressed and
   immutable — safe to upload before the commit point, idempotent on
   retry.
2. Serialize the manifest pointer to JSON.
3. CAS-write the pointer with `If-Match: {etag}` (or `If-None-Match: *`
   for the first push).
4. On success, update the stored ETag and return.
5. On CAS conflict (HTTP 412 — concurrent push):
   - Re-read the current manifest pointer.
   - Check for ref conflicts (same ref updated by another push). If
     conflicting, return `NonFastForward` error.
   - If non-conflicting (different refs), rebuild the manifest on the
     new base, upload any new bulk objects, and retry CAS.
   - Retry uses bounded jittered backoff so many writers do not hammer
     the manifest pointer in lockstep.
   - Max retries: `push.max_cas_retries` (default 64). When the
     retry budget is exhausted without a same-ref conflict, the push
     returns retryable `stale info` instead of `internal`.

**Observability:**
- Tracing span `push.unified_manifest_cas` with fields: `generation`,
  `refs_count`, `pointer_bytes`, `retries`, `duration_ms`.
- Counter `manifest_cas_conflicts_total` — incremented on each CAS retry.
- Counter `manifest_cas_failures_total` — incremented on permanent failure
  (ref conflict or max retries exceeded).

**Source:** `crab/src/git/push.rs` (`unified_manifest_cas`),
`crab/src/metadata/manifest.rs` (`write_manifest_cas`,
`upload_bulk_if_absent`)

**Agent integration retry:** `crab push --rebase-on-non-fast-forward` is an
opt-in command-layer loop for the single current-branch case. If the first push
is rejected as non-fast-forward, Crab runs `git pull --rebase --autostash
<remote> <branch>` and retries up to `--rebase-retry-limit`. Retryable push-lock
contention re-enters the same push path, and this mode applies a 30-second
per-attempt push-lock wait when neither CLI nor repo config supplied one. That
keeps agent swarms from stampeding on immediate lock failures without changing
default Git semantics. The helper spawned by that rebase also enables
conservative ref-aware pack filtering for the pull, so an agent only skips packs
when pack metadata and the commit-graph summary prove they cannot contribute to
the requested ref. The remote protocol and push pipeline still preserve normal
Git behavior; conflicting rebases fail locally and multi-ref, delete, force, or
non-current-branch pushes are not rewritten.

#### Steps 13–14: Cleanup

**Step 13 (success path):**
- Stop heartbeat task
- Release push lock by CAS-writing an expired holder-matched payload
- Install newly uploaded shard mappings into the session `ChunkIndex`
- Warm the xorb cache with uploaded xorb bytes and verified step-6 resume skips
- Retire staged rows for pushed files and sweep empty segment files

**Step 14 (failure path):**
- Stop heartbeat task
- Release push lock
- Leave staging and ChunkIndex unchanged (safe for retry)

**Source:** `crab/src/git/push.rs` (`post_success_cleanup`, `on_failure`)

-----

## 5. End-to-End Data Flow Diagram

### Full Push: Working Tree → Object Storage

```
 LOCAL MACHINE                                          OBJECT STORAGE (S3)
 ─────────────                                          ───────────────────

 ┌─────────────────┐
 │  Working Tree   │
 │  model.sft (10G)│
 └────────┬────────┘
          │ git add (clean filter)
          ▼
 ┌─────────────────┐     ┌──────────────────┐
 │  blake3 + CDC   │────►│  Staging Area    │
 │  single pass    │     │  segments/       │
 └────────┬────────┘     │  index.db        │
          │              └────────┬─────────┘
          ▼                       │
 ┌─────────────────┐              │
 │  Pointer Blob   │              │
 │  (~200 bytes)   │              │
 └────────┬────────┘              │
          │ git commit            │
          ▼                       │
 ┌─────────────────┐              │
 │  Git ODB        │              │
 │  commit + tree  │              │
 │  + pointer blob │              │
 └────────┬────────┘              │
          │ git push              │
          ▼                       │
 ┌───────────────────────────────────────────────────────────────────┐
 │                        PUSH PIPELINE                              │
 │                                                                   │
 │  ┌─────────┐   ┌──────────┐   ┌─────────┐   ┌──────────────────┐  │
 │  │ Step 1  │──►│ Step 3   │──►│ Step 5  │──►│ Step 7           │  │
 │  │ Walk    │   │ Shard    │   │ Pack    │   │ Upload xorbs     │──┼──► xorbs/{hash}
 │  │ commits │   │ sync     │   │ xorbs   │   │ (16 concurrent)  │  │
 │  │ + blobs │   │          │   │         │   └──────────────────┘  │
 │  └─────────┘   └──────────┘   └─────┬───┘                         │
 │                                     │                             │
 │                                     ▼                             │
 │                               ┌──────────┐   ┌──────────────────┐ │
 │                               │ Step 8   │──►│ Step 9           │ │
 │                               │ Build    │   │ Upload shards    │─┼──► .crab/shards/{first-two-hex}/{hash}
 │                               │ shard    │   │ + MetaDB commit  │─┼──► file_index_db/
 │                               └──────────┘   └──────────────────┘ │
 │                                                                   │
 │  ┌──────────────────┐                                             │
 │  │ Step 10          │                                             │
 │  │ git rev-list |   │                                             │
 │  │ git pack-objects │─────────────────────────────────────────────┼──► packs/pack-{sha}.pack
 │  └──────────────────┘                                             │     packs/pack-{sha}.meta
 │                                                                   │
 │  ┌──────────────────┐                                             │
 │  │ Step 11          │                                             │
 │  │ publish graph    │─────────────────────────────────────────────┼──► split graph layers
 │  └──────────────────┘                                             │     (pack-list, shard-list)
 │                                                                   │
 │  ┌──────────────────┐                                             │
 │  │ Step 12          │                                             │
 │  │ Lock → CAS refs  │─────────────────────────────────────────────┼──► refs/heads/main
 │  │ → Release lock   │                                             │
 │  └──────────────────┘                                             │
 └───────────────────────────────────────────────────────────────────┘
```

### Data Dependencies Between Steps

```
Step 1 ──► pointers[], commit_entries[]
           │                    │
           ▼                    │
Step 5 ──► xorbs[], chunk_placement_map
           │              │
           ▼              ▼
Step 7     Step 8 ──► shard_results[], file_shard_index
                      │
                      ▼
                   Step 9
                      │
Step 10 (independent of 7–9, depends on step 1 for ref tips)
                      │
                      ▼
Step 11 ◄── commit_entries[] (from step 1)
                      │
                      ▼
Step 12 ◄── specs[] (from remote helper batch)
```

-----

## 6. Phase Diagrams

### Phase 1: Classify

```
                    ┌──────────────────────────────────────┐
                    │           CLASSIFY PHASE             │
                    │                                      │
  git ODB ────────► │  1. gix-traverse: walk commits       │
  (.git/objects)    │     → discover pointer blobs         │
                    │     → collect commit entries         │
                    │                                      │
  staging ────────► │  2. lookup: verify chunks exist      │
  (index.db)        │                                      │
                    │                                      │
  MetaDB ─────────► │  3. classify: A/B/C per chunk        │
  chunk_index_db    │     session cache + batch lookup     │
                    │     A = remote hit (skip)            │
                    │     B = staged (pack + upload)       │
                    │     C = missing (error)              │
                    │                                      │
                    │  Output: classified pointer list     │
                    └──────────────────────────────────────┘
```

### Phase 2: Pack (Steps 5–6)

```
                    ┌──────────────────────────────────────┐
                    │             PACK PHASE               │
                    │                                      │
  staging ────────► │  5. For each class-B pointer:        │
  (segment files)   │     read chunks from staging         │
                    │     zstd-3 compress each chunk       │
                    │     pack into xorbs (64 MiB target)  │
                    │     maintain run-continuity          │
                    │     session dedup (skip seen hashes) │
                    │                                      │
                    │     Output:                          │
                    │       xorbs: Vec<Xorb>               │
                    │       chunk_placement: HashMap       │
                    │                                      │
  remote ─────────► │  6. HEAD check each planned xorb     │
  (xorbs/)          │     skip already-uploaded (resume)   │
                    │     Output: filtered xorb list       │
                    └──────────────────────────────────────┘
```

### Phase 3: Upload (Steps 7–10)

```
                    ┌──────────────────────────────────────┐
                    │            UPLOAD PHASE              │
                    │                                      │
                    │  7. Parallel xorb uploads:           │
                    │     ┌───┐ ┌───┐ ┌───┐                │
                    │     │ X │ │ X │ │ X │ ... (≤16)      │──► xorbs/
                    │     └───┘ └───┘ └───┘                │
                    │     multipart for > 8 MiB            │
                    │                                      │
                    │  8. Build MDB shard:                 │
                    │     xorb CAS info (chunk→xorb map)   │
                    │     file reconstruction terms        │
                    │     split at 100 MiB soft cap        │
                    │                                      │
                    │  9. Upload shards + MetaDB:          │
                    │ PUT .crab/shards/{first-two-hex}/{hash} │──► .crab/shards/
                    │     commit file_index_db             │──► file_index_db/
                    │     commit chunk_index_db            │──► .crab/chunk_index_db/
                    │     (bounded parallel shard upload)  │
                    │                                      │
                    │ 10. Generate + upload Git pack:      │
                    │     git rev-list | git pack-objects  │
                    │     PUT packs/pack-{sha}.pack        │──► packs/
                    │     PUT packs/pack-{sha}.meta        │
                    └──────────────────────────────────────┘

  ┌─────────────────────────────────────────────────────────┐
  │  ORDERING INVARIANT:                                    │
  │  Steps 7–10 MUST complete before steps 11–12.           │
  │  All immutable data is durable before any ref moves.    │
  └─────────────────────────────────────────────────────────┘
```

### Phase 4: Commit + Cleanup (Steps 11–14)

```
                    ┌──────────────────────────────────────┐
                    │          COMMIT PHASE                │
                    │                                      │
                    │ 11. CAS manifests:                   │
                    │     split commit graph               │──► immutable + manifest CAS
                    │     (pack-list, shard-list: stubs)   │
                    │                                      │
                    │ 12. Ref CAS:                         │
                    │     acquire push lock ───────────────│──► locks/refs/{name}/lock
                    │     spawn heartbeat                  │
                    │     batch rev-parse                  │
                    │     PUT refs/{name} ─────────────────│──► refs/heads/main
                    │     (CAS with etag on conflict)      │
                    └──────────────────────────────────────┘

                    ┌──────────────────────────────────────┐
                    │          CLEANUP PHASE               │
                    │                                      │
                    │ 13. Success:                         │
                    │     stop heartbeat                   │
                    │     release lock (holder CAS)        │
                    │     warm xorb cache                  │
                    │     retire staged rows               │
                    │     sweep empty segments             │
                    │                                      │
                    │ 14. Failure:                         │
                    │     stop heartbeat                   │
                    │     release lock (holder CAS)        │
                    │     staging unchanged (retry-safe)   │
                    └──────────────────────────────────────┘
```

-----

## 7. Coordination and Locking

### Push Lock Protocol

Push locks prevent concurrent pushers from racing on overlapping destination
refs. [Object Storage Layout V2](../architecture/object-storage-layout.md#lock-namespaces)
defines the normative key and hard-cutover protocol. Each lock is a short-TTL
lease stored as a JSON object in the configured object store:

```
Canonical path: {prefix}/locks/{full_ref}/lock
Example:        {prefix}/locks/refs/heads/main/lock

Payload:
{
  "holder": "push-{uuid}",
  "expires_at": 1713800000    // unix timestamp
}

TTL: 5 minutes (default, configurable)
```

Duplicated `locks/refs/refs/...` keys are retired and ignored after the hard
cutover.

### Lock Lifecycle

```
Pusher A                          S3                          Pusher B
────────                          ──                          ────────

PUT each target-ref lock (holder=A, expires=T+5m)
  ◄── 200 OK ──────────────────►  lock file created

                                                    PUT overlapping lock (holder=B)
                                  ◄── 409 Conflict ──────────────────►
                                                    (lock held by A)

... heartbeat renews lock ...
PUT lock (holder=A, expires=T+10m)
  ◄── 200 OK (CAS) ────────────►  lock extended

... push completes ...
PUT each lock (holder=A, expires=0)
  ◄── 200 OK (CAS) ────────────►  lock released

                                                    PUT overlapping lock (holder=B)
                                  ◄── 200 OK ──────────────────────►
                                                    (lock acquired)
```

Each push acquires the sorted, deduplicated set of destination refs it mutates,
including delete refspecs. If `push.lock_wait_secs` or `--lock-wait-secs` is
nonzero, contention releases any partially-acquired locks, waits with jitter,
and retries the full lock set until the wait budget expires.

For direct object-store pushes, repository-wide upload admission begins only
after that ref owner refreshes the manifest and rules out an under-lock no-op.
A same-ref waiter therefore polls only the ref handoff and never scans or
reserves admission slots. Owners
of distinct refs may wait for admission while retaining their renewable ref
leases; once admitted, the existing bounded pack/upload lifecycle applies.
Admission uses five reusable slot objects to cap every probe and avoid one
coordination object per contender. A push with xorb work reserves one slot per
eight configured upload workers, rounded up, and at least one slot per 64 MiB
of its estimated xorb upload-memory window. Memory accounting uses at most four
slots for the normal 256 MiB window, preserving one slot for a small or pure
Git push; a client configured with 40 or more workers still reserves all five.
When a push exhausts storage retries with a throttling response, its slots stay
live for the backend's bounded `Retry-After` interval instead of immediately
admitting another push into the same throttle window.

Managed protected pushes do not write these slots. Their staging credentials
are private to one push, so a slot written through that store could not
coordinate with another session. The managed service instead admits the push
before issuing credentials, using its authenticated repository-to-organization
mapping and the client's estimated byte and object plan. The shipped Crab Auth
compatibility protocol also owns protected publication, but its prepare request
predates plan-based team quota admission; use the managed protocol when team
fairness is required.

### Lock Expiry and Reclamation

If a pusher crashes before its ref-journal active marker, the ref stays
invisible and the lock expires after the TTL. The next pusher detects the
expired lock, reclaims it, and replaces the abandoned prepared head:

```
1. GET lock → { holder: "A", expires_at: T-60 }  (expired)
2. PUT lock → { holder: "B", expires_at: T+5m } with If-Match
```

Cleanup never unconditionally deletes the lock pointer in the hot path.
Without conditional delete support, release marks the holder expired and the
next acquirer reuses the same object via CAS. This prevents a stale owner from
deleting a fresh holder's lock after TTL expiry.

If the active marker is already visible, its immutable edit binds the exact
lock holder that crossed the final ref-critical boundary. A contender may
release that holder immediately with a holder-checked CAS. Prepared
transactions and mismatched holders cannot take this path, and the final CAS
cannot clear a lock that has already been acquired by a successor.

The generation-owner compactor also closes the crash window after the marker
is written: once the compacted manifest is committed, it promotes any
prepared ref heads for that transaction before deleting the marker, then
releases each recorded holder with the same holder-checked CAS. A failed head
promotion retains the marker for a later compaction pass, so upload-pack never
mistakes a half-repaired generation for a stable repository state.

An object-store failure before the active-marker write returns a structured,
retryable `transient` outcome and runs the normal holder-checked release path;
the ref remains invisible. If the immutable marker was stored but its success
response was lost, the exact-byte create retry observes the existing marker
and reconciles the write as success instead of reporting an ambiguous push.
The Git-facing rejection tag remains `transient`. The local push audit adds a
stable `failure_stage` such as `lock`, `xorb-upload`, `git-pack-upload`, or
`ref-commit`, allowing qualification reports to attribute retries without
parsing backend messages or changing the remote-helper protocol.
Retryable failures before pipeline delegation use `store-resolve` or
`discovery` and enter the same structured command-level retry path. Legacy
protected-push prepare failures remain terminal because that endpoint has no
idempotency contract.

### Heartbeat

When configured, a background task renews the lock at regular intervals:

```
Heartbeat interval: clamped to [10s, TTL - 10s]

Every interval:
  1. GET lock → verify holder matches
  2. PUT lock with new expires_at (CAS via etag)
  3. If holder mismatch → lock stolen → cancel push
```

### CAS (Compare-and-Swap) for Manifests

Mutable compatibility manifests (pack-list and shard-list) are
updated via a CAS loop:

```
cas_update(store, path, max_attempts=10, mutate_fn):
    for attempt in 0..10:
        (value, etag) = GET path       // or T::default() if 404
        mutate_fn(&mut value)
        new_body = serialize(value)

        match conditional_PUT(path, new_body, etag):
            Ok  → return value
            412 → backoff = jitter(50ms * 2^attempt, cap=500ms)
                  sleep(backoff)
                  continue

    return Err(CasConflict)
```

-----

## 8. Object Store Layout

After a push, the remote object store contains:

```
s3://{bucket}/{prefix}/
│
├── refs/
│   └── heads/
│       └── main                    ← "{sha}\n" (plain text)
│
├── locks/
│   └── refs/
│       └── heads/
│           └── main/
│               └── lock            ← JSON { holder, expires_at }
│
├── packs/
│   ├── pack-{blake3}.pack          ← standard Git packfile
│   └── pack-{blake3}.meta          ← JSON { pack_id, ref_tips, object_count }
│
├── xorbs/
│   └── {merkle_hash}               ← compressed chunk aggregate (xorb binary)
│
├── shards/
│   └── {merkle_hash}.shard         ← MDB shard (xorb CAS + file reconstruction)
│
├── file_index_db/
│   └── ...                         ← SlateDB file_hash → shard_hash
│
├── .crab/chunk_index_db/
│   └── ...                         ← SlateDB chunk_hash → xorb_ref
│
├── manifests/
│   └── pack-list                   ← JSON { entries: [{ pack_id, ref_tips }] }
│
├── manifests/commit-graph-{hash}   ← immutable split-graph descriptor
├── metadata/commit-graph/layers/
│   └── {hash}.bin                  ← immutable positional commit records
│
├── shard-list                      ← JSON { entries: [{ shard_hash }] }
│
├── config                          ← JSON (repo settings)
│
└── HEAD                            ← "ref: refs/heads/main\n"
```

### Object Mutability

| Object              | Mutability | Update Mechanism        |
|---------------------|------------|-------------------------|
| `refs/*`            | Mutable    | CAS (etag-based)        |
| `locks/*`           | Mutable    | Create/Delete           |
| `pack-list`         | Mutable    | CAS loop                |
| `shard-list`        | Mutable    | CAS loop                |
| `commit-graph-*`    | Mutable    | CAS loop                |
| `HEAD`              | Mutable    | PUT (rare)              |
| `config`            | Mutable    | PUT (rare)              |
| `xorbs/*`           | Immutable  | PUT once, never updated |
| `shards/*`          | Immutable  | PUT once, never updated |
| `packs/*.pack`      | Immutable  | PUT once, never updated |
| `packs/*.meta`      | Immutable  | PUT once, never updated |

**GC safety:** Immutable objects are never deleted by the push pipeline.
Only the GC command deletes unreferenced immutable objects, and only
after the grace period has elapsed.

-----

## 9. Failure Modes and Recovery

### Failure Matrix

| Failure Point          | Data State                          | Recovery                          |
|------------------------|-------------------------------------|-----------------------------------|
| Steps 1–4 (classify)   | No remote writes yet                | Retry from scratch                |
| Step 5 (pack)          | No remote writes yet                | Retry from scratch                |
| Step 7 (xorb upload)   | Some xorbs uploaded, no refs moved  | Orphaned xorbs (GC cleans up)     |
| Step 8–9 (shard)       | Xorbs + some shards uploaded        | Orphaned objects (GC cleans up)   |
| Step 10 (pack upload)  | Xorbs + shards + packs uploaded     | Orphaned objects (GC cleans up)   |
| Step 11 (build manifest) | All data uploaded                   | Rebuild from base manifest      |
| Step 12 (manifest CAS) | All data + bulk objects uploaded     | Retry CAS or merge-and-retry     |
| Step 13 (cleanup)      | Push succeeded, cleanup failed      | Lock expires via TTL              |

### Key Safety Properties

1. **No dangling refs.** Refs are only updated after all data is durable.
   A reader following a ref will always find the data it points to.

2. **Staging is never mutated on failure.** Steps 1–12 read from staging
   but never delete or modify staged chunks. If the push fails, the same
   data is available for retry.

3. **Lock always released or recoverable.** The `on_failure` path always stops
   the heartbeat and releases the lock with a holder-checked CAS update. A
   pre-marker process crash waits for TTL; a visible transaction lets the next
   pusher release only its committed holder immediately.

4. **CAS prevents lost updates.** Concurrent pushers cannot silently
   overwrite each other's ref updates. The CAS loop detects conflicts
   and retries.

### Cancellation

The pipeline checks a `CancellationToken` at four points:

```
Steps 1–4 → cancel check → Steps 5–6 → cancel check →
Steps 7–10 → cancel check → Step 11 → cancel check → Step 12
```

On cancellation, the pipeline returns immediately. The `on_failure`
path releases the lock and heartbeat.

-----

## 10. Performance Analysis and Bottlenecks

### Latency Breakdown (Typical Push of 100 Files, 5 GB Total)

```
Step                          Time      Notes
──────────────────────────── ──────── ──────────────────────────────
 1. Enumerate pointers        ~0.3s    Incremental walk when old_sha known
 2. Staging lookup             <1ms    Fast SQLite query per pointer
 3. Shard sync                 ~0.5s   Generation-based incremental
 4. Classify chunks             ~5ms   Three-tier dedup + optional cache svc
 5. Pack xorbs                 ~4s     Batched reads + zstd (class-C only)
 6. HEAD check                 ~0.2s   Parallel HEAD batch (up to 64)
 7. Upload xorbs               ~20s    Network-bound (≤16 parallel, multipart)
 8. Build shard                ~0.5s   CPU: shard serialization
 9. Upload shard + file-index  ~0.3s   Parallel (buffer_unordered)
10. Upload pack                ~1.5s   Incremental pack + local install
11. Manifest CAS               ~0.2s   Unified CAS (build + CAS pointer)
12. (merged into step 11)      —       Refs inline in manifest pointer
13. Cleanup                    ~50ms   ShardHint persist, cache warm
                              ────────
                              ~33s     Total (with warm dedup)
                              ~90s     First push (cold, no dedup)
```

### Identified Bottlenecks

Most of the bottlenecks identified in the original design have been
addressed. This section tracks both resolved and open items.

#### Resolved (post-v0.1)

| ID | Description                                       | How fixed |
|----|---------------------------------------------------|-----------|
| B2 | Chunk classification                              | Three-tier dedup + cache-service wired in `classify_chunks` |
| B4 | Double xorb parse for placement map               | `XorbBuilder::finalize` returns placements inline via `XorbResult.placements` |
| B5 | HEAD check stub                                   | Parallel `head_batch` with configurable concurrency and retries |
| B6 | Sequential shard/file-index uploads               | `futures_util::stream::buffer_unordered(upload_concurrency)` |
| B7 | Full pack generation                              | `compute_remote_objects` + `git pack-objects --not` for incremental packs |
| B8 | Late lock acquisition                             | Lock now acquired between step 4 and step 5 — no wasted uploads from lock losers |
| B9 | Double rev-parse                                  | `batch_rev_parse` consolidates refs in a single `git rev-parse` invocation |
| B11 | Lock renewal failure propagation                 | Heartbeat loss, deletion, CAS conflict, fatal error, or failed transient retry cancels the push through its shared token |
| — | Locator point-read amplification                   | Each read session uses a 16 MiB SST block/metadata cache; concurrent exact lookups coalesce shared provider reads without SlateDB's 640 MiB default |
| — | Contended lock polling amplification               | One acquisition context remembers existing lock objects and reuses its backend clock sample; repeated live-holder checks need one GET instead of a failed create plus GET, clock PUT, and clock HEAD |

The cache bound is per process: 32 simultaneous fetchers can retain at most
512 MiB in aggregate. In the 32-agent same-branch RustFS profile it reduced
locator HTTP attempts from 17,065 to 3,806, compacted-SST GETs from 11,222 to
940, and total HTTP attempts from 25,211 to 12,722. All 32 agent commits were
present in a fresh protocol-v2 clone and strict Git fsck passed. This is a
workload comparison, not a provider price claim.

The same 32-agent profile with reusable lock acquisition reduced ref-lock HTTP
attempts from 2,556 to 947 and total attempts from 12,722 to 11,117, or 347.41
per successful push. A four-agent comparison reduced ref-lock attempts from 80
to 34 and total attempts from 1,275 to 1,076. Both runs integrated every commit,
served an exact protocol-v2 clone, and passed strict Git fsck. Retry scheduling
and SlateDB compaction are nondeterministic, so the request budget—not one run's
wall time—is the regression signal.

#### Partially Resolved

##### B1: Pointer Walk Coverage

**Current state:** Incremental walk is wired via `walk_incremental` +
`PushState` (last-pushed SHA per `(remote, ref)` persisted at
`.crab/push-state`). On second and subsequent pushes the walk spans
only commits reachable from `new_sha` but hidden by `old_sha`.

**Remaining gap:** The tree walk still descends *every* reachable tree
from new commits, including unchanged subtrees. Dedup in step 4 makes
the result correct, but the walk cost is proportional to tree breadth,
not changed paths. A true incremental tree diff via `gix_diff::tree::Changes`
would walk only modified subtrees.

**Impact:** Secondary; affects repos with wide trees and narrow changes
(e.g. monorepos with one modified subdirectory per push).

**Source:** `crab/src/git/incremental_walk.rs`,
`crates/crab-git/src/push_state.rs`.

##### B10: Progress Reporting

**Current state:** `NativePushProgress` (see `crab/src/git/progress.rs`)
reports phase transitions and per-file upload progress on stderr. The
streaming packer in `push_native` emits structured progress events.

**Remaining gap:** Progress lines are not yet piped through the remote
helper sideband to git, so stock `git push` (rather than
`crab push-native`) still shows limited output during large
operations. This is a protocol surface issue rather than an instrumentation
gap.

**Source:** `crab/src/git/progress.rs`,
`crab/src/git/push_native.rs`.

#### Open

##### B3: Sequential Staging Reads

**Problem:** `pack_xorbs` batches reads via `staging.get_chunks_batch`
(default 256 chunks per batch), but each batch still serializes
`spawn_blocking(pread)` through one segment writer fd. For files that
straddle many segments, this becomes N batch reads in series.

**Impact:** Moderate for multi-hundred-GiB pushes over many segments.

**Proposed fix:** Parallelize batch reads across the `ReaderPool` of
segment fds. Staging already maintains a pool sized via `fd_pool_size`;
`get_chunks_batch` just needs to fan out reads across it.

##### B12: Adaptive Xorb Sizing Feedback Latency

**Problem:** `ThroughputMonitor` in the streaming packer observes
recent upload throughput and tunes xorb target size between 32–128 MiB.
The measurement window is small (3 uploads), so a transient S3 slowdown
can lock in a small target size for the rest of the push.

**Impact:** Low; self-corrects within a few xorbs.

**Proposed fix:** EMA with longer memory, or reset on push start.

**Source:** `crab/src/git/push_native.rs` (`ThroughputMonitor`).

### Bottleneck Priority Matrix

```
Impact ▲
       │
  High │
       │
  Med  │  B1 (tree walk breadth)  B3 (staging reads)
       │
  Low  │  B12 (xorb size feedback)
       │
  UX   │  B10 (remote-helper sideband)
       │
       └──────────────────────────────────────────────────────────►
```

-----

## 11. Hardening Roadmap

### Phase 1: Remaining Qualification and UX

| Item | Description | Bottleneck | Effort |
|------|-------------|------------|--------|
| 1.1  | Qualify bounded locator caching and workload-specific request budgets on S3, GCS, and Azure | locator reads | M |
| 1.2  | Qualify request-timeout recovery and provider-specific transient errors | B11 | S |
| 1.3  | Progress sideband via remote-helper protocol | B10 | S |
| 1.4  | Stabilize adaptive xorb-size EMA feedback | B12 | S |

### Phase 2: Walk and Read Performance

| Item | Description | Bottleneck | Effort |
|------|-------------|------------|--------|
| 2.1  | True incremental tree walk via `gix_diff::tree::Changes` | B1 | L |
| 2.2  | Parallel batch reads across segment fd pool | B3 | M |
| 2.3  | Adaptive shard-cache eviction (spill to disk sooner) | — | M |

### Phase 3: Scale (100K+ Files)

| Item | Description | Bottleneck | Effort |
|------|-------------|------------|--------|
| 3.1  | Parallel pointer enumeration (multi-threaded walk) | B1 | L |
| 3.2  | Shard bloom pre-filter during classification | — | M |
| 3.3  | Adaptive upload concurrency default on (currently opt-in) | — | S |
| 3.4  | Pack-list pruning (merge small packs on push) | — | L |

**Effort key:** S = small (< 1 day), M = medium (1–3 days), L = large (3+ days)

-----

## 12. Configuration Reference

### Push-Related Config Keys

| Key | Default | Description |
|-----|---------|-------------|
| `upload_concurrency` | 16 | Max concurrent xorb uploads (step 7) |
| `receive.maxInputSize` | 2 GiB | Maximum size of each generated standard Git pack; `0` disables the per-pack bound |
| `push_lock_heartbeat_interval` | 60s | Heartbeat interval for push lock renewal |
| `push.lock_wait_secs` | 0 | Opt-in wait budget for contested push locks |
| `perf.enabled` | true | Master switch: streaming packer vs sequential v1 |
| `operation_timeout` | 300s | Per-operation timeout for S3 requests |
| `max_retries` | 3 | Max retries for transient S3 failures |

### Push Lock Config

| Parameter | Default | Description |
|-----------|---------|-------------|
| Lock TTL | 5 min | Time before an unreleased lock expires |
| Heartbeat interval | Clamped to [10s, TTL-10s] | How often the lock is renewed |
| Lock wait | 0s | Fail fast by default; agents can opt in to bounded waiting |
| Holder ID | `pid-{pid}-{nanos}-{seq}` | Unique identifier per push attempt |

### CAS Config

| Parameter | Default | Description |
|-----------|---------|-------------|
| Max attempts | 10 | CAS retry budget before returning error |
| Backoff base | 50ms | Base delay for jittered backoff |
| Backoff cap | 500ms | Maximum delay between retries |

### Xorb Packing Config

| Parameter | Default | Description |
|-----------|---------|-------------|
| Target xorb size | 64 MiB | Compressed size target per xorb |
| Min run size | 1 MiB | Min bytes before allowing a run break |
| Multipart threshold | 8 MiB | Xorbs larger than this use multipart upload |
| Multipart part size | 8 MiB | Part size for multipart uploads |
| Zstd level | 3 | Compression level for chunk data |

-----

## 13. Source Map

| Component | File | Key Functions |
|-----------|------|---------------|
| Remote helper protocol | `git/remote_helper.rs` | `run_remote_helper`, `dispatch_batch`, `format_push_response` |
| Push pipeline orchestrator | `git/push.rs` | `PushPipeline`, `execute_inner`, `run_push_batch` |
| Pointer walk | `git/walk.rs` | `walk_reachable`, `walk_tree`, `check_blob_for_pointer` |
| Pack generation | `git/pack_gen.rs` | `generate_push_pack`, `compute_remote_object_set` |
| Clean filter | `git/clean.rs` | `CleanSession`, `crab_clean_path`, `try_fast_path` |
| Filter process | `git/filter_process.rs` | `run_filter_process`, `dispatch_command` |
| Xorb builder | `storage/xorb/builder.rs` | `XorbBuilder`, `push`, `finalize` |
| Xorb parser | `storage/xorb/parser.rs` | `XorbParser`, `chunk_meta` |
| Staging area | `crates/crab-staging/src/lib.rs` | `StagingArea`, `stage_chunks_batch`, `load_file_push_plan`, `get_chunk` |
| Staging index | `crates/crab-staging/src/index.rs` | `Index`, `chunks_for_file`, `file_push_plan`, `prepared_xorbs_for_chunks` |
| Add-time push plans | `crates/crab-staging/src/push_plan.rs` | `FilePushPlan`, `PreparedXorbCache`, indexed prepared-xorb metadata |
| Shard writer | `metadata/shard.rs` | `PushShardSession`, `ShardWriter`, `ShardReader` |
| Push lock | `coordination/push_lock.rs` | `PushLock`, `acquire`, `release`, `renew` |
| Lock heartbeat | `coordination/heartbeat.rs` | `LockHeartbeat`, `spawn`, `stop` |
| CAS loop | `crates/crab-storage/src/cas.rs` | `cas_update`, `cas_update_default` |
| Ref store | `metadata/refs.rs` | `RefStore`, `ObjectStoreRefStore` |
| Metrics | `core/metrics.rs` | `Metrics` (push_duration_ms, bytes_uploaded, etc.) |
| Config | `core/config.rs` | `Config`, `PushOverlay`, `StagingConfig` |


-----

## 14. Concurrent Push Scenarios

Understanding how multiple pushers interact is critical for correctness.
The push pipeline uses three coordination mechanisms: push locks, CAS on
manifests, and CAS on refs. This section walks through the key scenarios.

### Scenario 1: Two Pushers to the Same Ref (Serial)

```
Pusher A                              Pusher B
────────                              ────────
Steps 1–11: upload data
Step 12: acquire lock ✓
         write ref ✓
Step 13: release lock
                                      Steps 1–11: upload data
                                      Step 12: acquire lock ✓
                                               write ref ✓
                                      Step 13: release lock

Result: Both succeed. B's ref update overwrites A's.
        B's data includes A's commits (fast-forward).
```

### Scenario 2: Two Pushers to the Same Ref (Late-Lock, Historical)

This scenario describes the original v0.1 behavior where the lock was
acquired in step 12. It is preserved for context — current builds
acquire the lock between steps 4 and 5 (see Scenario 3).

```
Pusher A                              Pusher B
────────                              ────────
Steps 1–6: classify + pack
                                      Steps 1–6: classify + pack
Steps 7–10: upload data
                                      Steps 7–10: upload data
Step 11: CAS manifests ✓
                                      Step 11: CAS manifests
                                        (may conflict → retry → ✓)
Step 12: acquire lock ✓
         write ref ✓
         release lock
                                      Step 12: acquire lock ✓
                                               write ref ✓
                                               release lock

Historical result: Both succeed, but both uploaded ALL their data.
        If they pushed overlapping chunks, those chunks exist
        twice in xorbs/ (GC deduplicates eventually).
        Wasted bandwidth = overlap between A and B's uploads.
```

**Why it's no longer current:** Current builds acquire the lock
immediately after step 4 (classify). Both pushers compute the same
dedup decisions, but only one gets to upload — see Scenario 3.

### Scenario 3: Two Pushers, Lock Contention (Current: Early Lock)

```
Pusher A                              Pusher B
────────                              ────────
Steps 1–4: classify
Step 4a: acquire lock ✓
Steps 5–10: pack + upload
                                      Steps 1–4: classify
                                      Step 4a: acquire lock ✗
                                        (CasConflict — lock held by A)
                                        → FAIL immediately
                                        → "push rejected: ref locked by
                                           another push in progress"
Step 11: CAS manifests ✓
Step 12: write ref ✓
Step 13: release lock
                                      (user retries)
                                      Steps 1–4: classify
                                      Step 4a: acquire lock ✓
                                      Steps 5–10: pack + upload
                                      ...

Result: B fails fast, no wasted upload bandwidth.
        B retries after A completes.
```

### Scenario 4: Pusher Crashes Mid-Upload

```
Pusher A                              S3 State
────────                              ────────
Steps 1–6: classify + pack
Step 7: upload xorbs 1..50 of 100
  ── process killed ──
                                      xorbs/hash_1 .. xorbs/hash_50
                                      (orphaned, no ref points to them)

                                      Lock expires after TTL (5 min)

Pusher A (retry):
Steps 1–6: classify + pack
Step 6: HEAD check → 50 xorbs exist
Step 7: upload xorbs 51..100 only    ← resume optimization
Steps 8–13: complete normally

Result: Retry uploads only the missing 50 xorbs.
        Orphaned xorbs from the first attempt are cleaned by GC
        after the grace period.
```

### Scenario 5: Ref-Journal CAS Conflict

```
Pusher A                              Pusher B
────────                              ────────
commit ref-journal head with CAS ✓    observe updated journal head
compact committed transactions       re-evaluate against current refs
append split graph layer              commit only non-conflicting edits
CAS graph hash onto exact manifest    append/compact from that generation
                                               mutate → generation=7
                                               PUT with If-Match: E2 ✓

Result: Both commits are in the graph summary.
        No data lost. CAS loop converges.
```

### Scenario 6: Push to Different Refs (No Contention)

```
Pusher A (refs/heads/main)            Pusher B (refs/heads/feature)
──────────────────────────            ────────────────────────────
Steps 1–13: full pipeline             Steps 1–13: full pipeline
Lock: locks/refs/heads/main/lock      Lock: locks/refs/heads/feature/lock

Result: Both succeed independently. No contention on locks.
        Manifest CAS may conflict briefly but converges.
```

-----

## 15. Worked Example: Pushing a 2 GB Model Update

This section traces a concrete push through every step with real numbers
to illustrate the pipeline's behavior.

### Setup

```
Repository: crab://my-bucket/ml-models
Branch: refs/heads/main
File changed: models/llama-7b.safetensors (2.1 GB → 2.3 GB)
  - ~90% of content unchanged (weight matrices)
  - ~10% changed (fine-tuned layers)
Previous push: commit abc123 (2.1 GB version)
Current commit: commit def456 (2.3 GB version)
```

### Step 1: Enumerate Pointers

```
gix-traverse walks from def456:
  - 1 commit (def456)
  - 1 tree (root)
  - 2 blobs: models/llama-7b.safetensors (pointer, 186 bytes)
             README.md (small file, 2 KB, not a pointer)

Result: pointers = [{ file_hash: 0xA1B2..., size: 2_300_000_000 }]
        commit_entries = [{ oid: "def456", parents: ["abc123"], gen: 1 }]
```

### Step 3: Shard Sync

```
GET shard-list → 3 shards on remote
Local cache has 2 of 3 → download 1 new shard
ChunkIndex rebuilt: 32,768 chunk entries (from 2.1 GB version)
```

### Step 4: Classify

```
File: models/llama-7b.safetensors
  Total chunks: 36,000 (2.3 GB / 64 KiB average)
  Class A (existing, three-tier dedup hit): 32,400 (~90%, unchanged content)
  Class B (already seen earlier in this push):   0
  Class C (new, needs packing):              3,600 (~10%, fine-tuned layers)

Dedup savings: 90% of chunks skipped → only 230 MB to upload
```

### Step 5: Pack Xorbs

```
3,600 class-C chunks read from staging:
  - 3,600 SQLite lookups + pread + blake3 verify
  - Each chunk ~64 KiB → 230 MB raw data

XorbBuilder:
  - zstd-3 compress each chunk (~40% compression ratio)
  - 230 MB raw → ~140 MB compressed
  - 140 MB / 64 MiB target = 3 xorbs

Result:
  xorbs = [
    Xorb { hash: 0xF1..., data: 64 MiB, chunks: 1680 },
    Xorb { hash: 0xF2..., data: 64 MiB, chunks: 1680 },
    Xorb { hash: 0xF3..., data: 12 MiB, chunks: 240 },
  ]
  chunk_placement: 3,600 entries
```

### Step 7: Upload Xorbs

```
3 xorbs, 16 concurrency slots:
  xorb 0xF1 (64 MiB) → multipart upload (8 parts × 8 MiB)  ~4s
  xorb 0xF2 (64 MiB) → multipart upload (8 parts × 8 MiB)  ~4s  (parallel)
  xorb 0xF3 (12 MiB) → multipart upload (2 parts × 8 MiB)  ~1s  (parallel)

Wall time: ~4s (limited by largest xorb)
Bytes uploaded: 140 MiB
```

### Step 8: Build Shard

```
3 xorbs → 3 XorbChunkSequenceHeader + 3,600 XorbChunkSequenceEntry
1 file → 1 FileDataSequenceHeader + 3 FileDataSequenceEntry (terms)

Reconstruction terms for models/llama-7b.safetensors:
  Term 0: xorb=0xF1, chunks [0..1680], 107 MiB
  Term 1: xorb=0xF2, chunks [0..1680], 107 MiB
  Term 2: xorb=0xF3, chunks [0..240],   16 MiB

Shard size: ~180 KB (metadata only, not chunk data)
```

### Step 9: Upload Shards + MetaDB

```
PUT .crab/shards/0xS1 (180 KB)           ~50ms
Commit file_index_db + chunk_index_db    amortized with MetaDB batch

Total: one shard upload round trip plus one MetaDB commit
```

### Step 10: Upload Git Pack

```
git pack-objects --revs --max-pack-size=2147483648 {base_name}
  → 1 commit + 1 tree + 1 pointer blob = 3 objects
  → pack size: ~800 bytes

PUT packs/pack-0xP1.pack (800 bytes)     ~50ms
PUT packs/pack-0xP1.idx                  ~50ms
PUT packs/pack-0xP1.meta (JSON)          ~50ms
```

### Step 11–12: Commit Refs + Attach Acceleration

```
Commit ref-journal transaction under the ref lock
Compact transactions into one manifest generation
Upload one binary graph delta layer and descriptor
CAS the descriptor hash onto that exact manifest generation
Release/hand off maintenance ownership
```

### Total Push Time

```
Step  1: enumerate pointers     0.5s
Step  5: pack xorbs             1.5s  (3,600 chunk reads + compress)
Step  7: upload xorbs           4.0s  (140 MiB, 3 xorbs parallel)
Step  8: build shard            0.1s
Step  9: upload shard/index     0.1s
Step 10: upload pack            0.2s
Step 11: build manifest          0.05s
Step 12: unified manifest CAS    0.2s
                               ─────
Total:                         ~6.7s

Without dedup (all 36K chunks):
  Step 5: ~15s (36K reads + compress)
  Step 7: ~30s (2.3 GB upload)
  Total: ~50s

Dedup savings: 7.7s vs 50s = 6.5x faster
```

-----

## 16. Streaming Packer Architecture

The sequential pipeline (classify all → pack all → upload all) runs
when `perf.enabled = false`. When enabled (the default), the streaming
packer overlaps all three phases to reduce latency and memory use.
This section describes both modes side by side; see §20.1 for a
focused write-up of the streaming implementation.

### Sequential Pipeline (v1 baseline)

```
Time ──────────────────────────────────────────────────────────►

  ┌──────────────┐
  │  Classify    │
  │  all chunks  │
  └──────┬───────┘
         │
         ▼
  ┌──────────────┐
  │  Pack all    │
  │  into xorbs  │
  └──────┬───────┘
         │
         ▼
  ┌──────────────┐
  │  Upload all  │
  │  xorbs       │
  └──────────────┘

Memory peak: all xorbs buffered simultaneously
Latency: sum of all phases
```

### Streaming Pipeline (default)

```
Time ──────────────────────────────────────────────────────────►

  ┌─────┐ ┌─────┐ ┌─────┐ ┌─────┐
  │ C 0 │ │ C 1 │ │ C 2 │ │ C 3 │  Classifier (file-at-a-time)
  └──┬──┘ └──┬──┘ └──┬──┘ └──┬──┘
     │       │       │       │
     ▼       ▼       ▼       ▼
  ┌─────┐ ┌─────┐ ┌─────┐ ┌─────┐
  │ P 0 │ │ P 1 │ │ P 2 │ │ P 3 │  Packer (xorb-at-a-time)
  └──┬──┘ └──┬──┘ └──┬──┘ └──┬──┘
     │       │       │       │
     ▼       ▼       ▼       ▼
  ┌─────┐ ┌─────┐ ┌─────┐ ┌─────┐
  │ U 0 │ │ U 1 │ │ U 2 │ │ U 3 │  Uploader (bounded concurrency)
  └─────┘ └─────┘ └─────┘ └─────┘

Memory peak: 1–2 xorbs buffered (64–128 MiB)
Latency: max(classify, pack, upload) — phases overlap
```

### Channel-Based Design

```
                    ┌───────────────┐
                    │  Classifier   │
                    │  (tokio task) │
                    └───────┬───────┘
                            │ mpsc channel
                            │ ClassifiedChunk { hash, data, class }
                            ▼
                    ┌───────────────┐
                    │    Packer     │
                    │  (tokio task) │
                    └───────┬───────┘
                            │ mpsc channel
                            │ SealedXorb { hash, data, placement }
                            ▼
                    ┌───────────────┐
                    │   Uploader    │
                    │  (task pool)  │
                    └───────────────┘

Backpressure:
  - Classifier → Packer channel: bounded (e.g., 256 chunks)
    If packer is slow, classifier blocks → staging reads pause
  - Packer → Uploader channel: bounded (e.g., 4 xorbs)
    If uploads are slow, packer blocks → no unbounded memory growth
  - Backpressure events counted in metrics:
    push_stream_backpressure_events
```

### Benefits

| Property | Sequential | Streaming |
|----------|-----------|-----------|
| Memory peak | All xorbs (~N × 64 MiB) | 1–2 xorbs (~128 MiB) |
| Latency | Sum of phases | Overlapped (faster) |
| First byte uploaded | After all packing done | After first xorb sealed |
| Backpressure | N/A (batch) | Channel-based, bounded |
| Complexity | Simple | Higher (3 concurrent tasks) |

### When to Use Which

- **Sequential (current):** Small pushes (< 10 xorbs), debugging, tests
- **Streaming (future):** Large pushes (> 10 xorbs), production workloads

The `PushConfig::packer_mode()` method selects the mode based on
`perf.enabled`:

```rust
pub fn packer_mode(&self) -> PackerMode {
    if self.perf_enabled {
        PackerMode::Streaming
    } else {
        PackerMode::SequentialV1
    }
}
```

-----

## 17. Comparison: crab Push vs Git-LFS Push

| Aspect | Git-LFS | crab |
|--------|---------|--------|
| **What's uploaded** | Full file per version | Only new/changed chunks |
| **Dedup** | None (file-level) | 3-tier CDC (session → shard → DB) |
| **Transport** | HTTP Batch API to LFS server | Direct S3 PUT (serverless) |
| **Server required** | Yes (LFS server) | No (object storage only) |
| **Concurrency** | LFS server controls | Client-side semaphore (16) |
| **Resume on retry** | Re-upload entire file | Skip already-uploaded xorbs |
| **Locking** | LFS file locking (optional) | Push lock per ref (TTL lease) |
| **Metadata** | LFS pointer (SHA-256, size) | crab pointer (blake3, size, shard-hint) |
| **Pack format** | Standard Git pack (no LFS objects) | Standard Git pack + xorbs + shards |
| **Consistency** | LFS server handles | CAS on S3 (etag-based) |
| **Progress** | LFS transfer progress | Planned (sideband) |

### Push Size Comparison (2 GB File, 10% Changed)

```
Git-LFS:
  Upload: 2.0 GB (entire new version)
  Time:   ~60s at 250 Mbps

crab (with dedup wired):
  Upload: 140 MB (only changed chunks, compressed)
  Time:   ~4s at 250 Mbps

Savings: 14x less data, 15x faster
```

### Push Size Comparison (2 GB File, First Push)

```
Git-LFS:
  Upload: 2.0 GB (entire file)
  Time:   ~60s at 250 Mbps

crab:
  Upload: ~1.2 GB (all chunks, zstd-3 compressed)
  Time:   ~36s at 250 Mbps

Savings: 1.7x less data (compression only, no dedup on first push)
```

-----

## 18. Observability and Metrics

### Push Metrics (Emitted by `Metrics` Struct)

| Metric | Type | Description |
|--------|------|-------------|
| `push_duration_ms` | Counter | Total push wall time |
| `bytes_uploaded` | Counter | Total bytes sent to S3 |
| `head_list_requests` | Counter | HEAD requests for xorb resume check |
| `head_point_requests` | Counter | HEAD requests for individual xorbs |
| `xorbs_skipped` | Counter | Xorbs skipped by resume check |
| `staging_bytes_read` | Counter | Bytes read from staging during pack |
| `staging_fsyncs` | Counter | fsync calls during staging flush |
| `cas_pipelined_commits` | Counter | CAS commits in manifest updates |
| `push_stream_backpressure_events` | Counter | Streaming packer backpressure |
| `multipart_resumed_uploads` | Counter | Multipart uploads that resumed |

### Tracing Spans

The push pipeline emits structured tracing spans for each phase:

```
push.classify          (steps 1–4)
  └── push.enumerate   (step 1)
push.pack              (step 5)
push.head_check        (step 6)
push.upload_xorbs      (step 7)
push.build_shard       (step 8)
push.upload_pack       (step 10)
push.cas_manifests     (step 11)
push.cas_refs          (step 12)
push.post_commit       (step 13)
```

Each span includes structured fields:

```
push.upload_xorbs { uploaded=3, total_bytes=146800640, concurrency=16 }
push.build_shard  { shards=1, files=1, total_shard_bytes=184320 }
push.cas_refs     { lock_path="locks/refs/heads/main/lock", heartbeat_active=true }
```

### Recommended Dashboards

For production monitoring, track these aggregates:

```
Push Latency:     p50, p95, p99 of push_duration_ms
Upload Throughput: bytes_uploaded / push_duration_ms
Dedup Efficiency:  xorbs_skipped / (xorbs_skipped + xorbs_uploaded)
CAS Contention:    cas_pipelined_commits (high = many concurrent pushers)
Lock Wait Time:    time between lock acquire attempt and success
Retry Rate:        pushes with xorbs_skipped > 0 (indicates retries)
```

-----

## 19. Invariants Checklist

These invariants must hold across all push code paths. Violating any of
them can corrupt the remote repository or lose data.

| # | Invariant | Enforced By |
|---|-----------|-------------|
| 1 | All immutable data durable before any ref moves | Step ordering (7–10 before 11–12) |
| 2 | Lock acquired before ref write, released on every exit | `stop_heartbeat_and_release_lock` in both success and failure paths |
| 3 | Staging never mutated until push succeeds | Steps 1–12 only read from staging; step 13 retires pushed rows and sweeps empty segments |
| 4 | CAS prevents lost ref updates | Etag-based conditional PUT on refs |
| 5 | CAS prevents lost manifest updates | `cas_update` loop with bounded retries |
| 6 | Heartbeat stops on every exit path | `on_failure` and `post_success_cleanup` both call `stop_heartbeat_and_release_lock` |
| 7 | Orphaned immutable objects are safe | GC grace period protects recently-written objects |
| 8 | Cancellation token checked at phase boundaries | 4 `check_cancelled` calls in `execute_inner` |
| 9 | No `unwrap`/`panic` in pipeline code | Code style rule; errors propagated via `?` |
| 10 | Batch semantics: all refs fail if any pre-CAS step fails | `execute` wraps `execute_inner`; error → all refs marked Error |


-----

## 20. Advanced Capabilities (post-v0.1)

The sections above describe the core 14-step pipeline. This section
covers capabilities layered on top that are active in current builds.

### 20.1 Streaming Packer (`streaming_classify_pack_upload`)

The v1 sequential packer runs classify → pack → upload in phases: all
chunks classified, then all xorbs packed, then all xorbs uploaded. The
streaming packer overlaps all three phases in a three-stage async
pipeline.

```
                     mpsc channel (ClassifiedChunk)
  ┌──────────────┐       bounded=256        ┌──────────────┐
  │  Classifier  │────────────────────────► │    Packer    │
  │  (per-file   │                          │ (one tokio   │
  │   workers)   │                          │  task)       │
  └──────────────┘                          └──────┬───────┘
                                                   │
                                  mpsc channel (SealedXorb { placements })
                                  bounded=4
                                                   ▼
                                          ┌──────────────┐
                                          │  Uploaders   │
                                          │  (pool of N) │
                                          └──────────────┘
```

**Backpressure via bounded channels.** When uploads lag, the packer
blocks on `send`. When packing lags, classifier workers block. This
caps memory at ~1–2 xorbs in flight (rather than all xorbs buffered
simultaneously) and auto-tunes the pipeline to the slowest stage.
Backpressure events are counted in `push_stream_backpressure_events`.

**Mode selection.** `PushConfig::packer_mode()` returns
`PackerMode::Streaming` when `perf.enabled = true` (default) and
`PackerMode::SequentialV1` otherwise. The streaming path is the one
actually run by `run_push_batch` today; v1 remains as a regression
baseline for tests.

**Source:** `crab/src/git/push_native.rs`
(`streaming_classify_pack_upload`, `streaming_packer_task`,
`upload_workers`, `process_file_worker`).

### 20.2 Adaptive Compression

Compression policy is selected per-xorb-chunk rather than fixed:

| Policy             | Path                                      | When chosen |
|--------------------|-------------------------------------------|-------------|
| `FixedCompression` | `compression_adaptive = false`            | Deterministic baseline, tests |
| `AdaptiveCompression` | `compression_adaptive = true` (default) | Production; probes entropy per chunk |

`AdaptiveCompression` runs a short entropy probe on each chunk. High-entropy
chunks (already-compressed formats, encrypted data) go through `BG4`
(xet-core's bit-grouped passthrough), which avoids the 30% zstd penalty
on incompressible data. Low-entropy chunks go through zstd-3 as before.

**Source:** `crab/src/storage/xorb/builder.rs`
(`CompressionPolicy`, `FixedCompression`, `AdaptiveCompression`).

### 20.3 Adaptive Concurrency

`push_adaptive_concurrency` is accepted but currently resolves to bounded
fixed concurrency. The pinned xet-core API exposes permit acquisition and
partial-progress callbacks, but keeps final transfer success/failure reporting
crate-private; Crab therefore cannot feed complete upload lifecycle events into
xet-core's controller without depending on a non-public API.

**Source:** `crates/crab-xet/src/upload_concurrency.rs`
(`UploadConcurrency`).

### 20.4 Adaptive Xorb Sizing

When `adaptive_xorb_size = true`, `ThroughputMonitor` observes the
bandwidth of each xorb upload and tunes the target size for subsequent
xorbs:

```
High throughput (≥ target) → larger xorbs (128 MiB)  fewer objects
Medium throughput          → default (64 MiB)
Low throughput (≤ target)  → smaller xorbs (32 MiB)  more objects,
                                                     better resume granularity
```

Bounded by `min_xorb_size` and `max_xorb_size` (defaults 16 MiB / 256
MiB). The controller needs 3+ observations before it adjusts; the first
few xorbs always use the default size.

**Source:** `crab/src/git/push_native.rs` (`ThroughputMonitor`).

### 20.5 Cache-Service Dedup Tier

When a cache service is configured (`CachingStore`), step 4's dedup
lookup adds a remote query as class A:

```
all_hashes = flatten_chunks_across_all_pointers()
known = cache_service.query_known_chunks(&all_hashes).await
→ Any chunk in `known` becomes a class-A candidate after the push
  pipeline verifies the referenced xorb metadata and payload against
  origin object storage.
```

The query is best-effort: on failure, the set is empty and local dedup
handles the full workload. A cache service that already has most chunks
turns a "cold local dedup" push into a "warm remote dedup" push,
often a 10x+ speedup on first-push-from-fresh-clone scenarios.
The cache service's `cache_verified` bit proves its local cache contains
the referenced bytes. In `cache+dedup`, push treats that service proof as
authoritative and does not reread immutable xorbs from origin. In `dedup`,
push still adds the origin proof before publishing shard metadata.

Step 9 warms the cache service with newly-uploaded shards, and step 13
warms small newly-uploaded xorbs. Streamed xorb uploads are reread from
origin within the bounded warm budget when the upload task has released
its body, so cache-service dedup can verify the cached payload before
returning it as known.

**Source:** `crates/crab-cache-server/src/*`,
`crates/crab-cache/src/cache_client.rs`,
`crab/src/git/push.rs` (`classify_chunks`, `upload_shard_and_file_index`).

### 20.6 Native Push Orchestrator (`run_native_push`)

The remote helper route (`git push` → `git-remote-crab` →
`run_push_batch`) is the default. A parallel `run_native_push` entry
point in `push_native.rs` bypasses the remote helper protocol and
drives the pipeline directly from CLI (`crab push-native`).

Differences from the remote-helper route:

- Full progress ticker on stderr (phase bars, per-phase ETA).
- Streaming classifier → packer → uploader wired end-to-end.
- Pre-populates the pipeline's walk via `phase_discover` so step 1
  never re-walks.
- Directly updates `.crab/push-state` on success.

The two entry points converge into the same step 7–14 code in
`PushPipeline::execute_inner`; the native orchestrator's advantage is
that it feeds richer pre-computed state into the pipeline rather than
reconstructing it from the remote-helper batch.

**Source:** `crab/src/git/push_native.rs` (`run_native_push`,
`phase_discover`, `update_push_state_on_success`).

### 20.7 Push-State Tracking for Incremental Walks

`.crab/push-state` is a JSON file mapping `(remote, ref) → last_pushed_sha`.
Updated atomically (temp file + rename) after a successful push batch.
Consumed by `walk_incremental` to set the hidden boundary on the
commit walk:

```
old_sha = push_state.last_pushed(remote, ref).unwrap_or(None)

if old_sha.is_some():
    walk_incremental(git_dir, old_sha, new_sha)    // hide(old_sha)
else:
    walk_reachable(git_dir, &refs)                 // full walk
```

A corrupt or missing push-state file is tolerated: the pipeline falls
back to the full walk. A failed push leaves the file unchanged, so the
next retry still uses the previous `last_pushed_sha` as the boundary.

**Source:** `crates/crab-git/src/push_state.rs`,
`crab/src/git/incremental_walk.rs`.

-----

## 21. Updated Source Map

This table supersedes §13 with entries for capabilities described in
§20.

| Component | File | Key Functions |
|-----------|------|---------------|
| Native push orchestrator | `git/push_native.rs` | `run_native_push`, `phase_discover`, `streaming_classify_pack_upload` |
| Streaming packer | `git/push_native.rs` | `streaming_packer_task`, `packer_task`, `upload_workers`, `process_file_worker` |
| Throughput-adaptive sizing | `git/push_native.rs` | `ThroughputMonitor` |
| Push-state persistence | `git/push_state.rs` | `PushState::load`, `set`, `save`, `last_pushed` |
| Incremental walk | `git/incremental_walk.rs` | `walk_incremental`, `walk_tree_for_pointers` |
| Adaptive compression | `storage/xorb/builder.rs` | `AdaptiveCompression`, `FixedCompression`, `CompressionPolicy` |
| Upload concurrency | `crates/crab-xet/src/upload_concurrency.rs` | `UploadConcurrency` |
| HEAD batch | `storage/head_batch.rs` | `head_batch`, `HeadBatchConfig`, `HeadBatchResult` |
| Persistent chunk index | `metadata/persistent_chunk_index.rs` | `PersistentChunkIndex::open_or_create`, `load_all`, `install_shard` |
| Shard synchronizer | `metadata/shard_sync.rs` | `ShardSynchronizer::sync`, `with_repo_cache_dir`, `with_persistent_index` |
| Three-tier dedup | `engine/dedup.rs` | `Classifier::classify_with_context`, `DedupContext`, `lookup_three_tier` |
| Cache-service tier | `cache_service/*` | `CachingStore::query_known_chunks`, `put` |
| Local cache warming | `cache/local_cache.rs` | `LocalCache::put`, `get`, `default_cache_root` |
| Shard-hint persist | `cache/shard_hints.rs` | `ShardHintCache::store`, `load_sync` |

-----

## 22. Document Status Note

Sections 1–19 describe the canonical pipeline and invariants.
Sections 20–21 track capabilities that were scheduled in §11 and have
since shipped. When this doc is revised, the sections in §20 that are
fully subsumed by the mainline narrative should be folded back into
§4/§10 and these late-appendix sections retired.
