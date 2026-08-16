# xet-core Git Integration — Architecture Analysis & Lessons for crab

**A deep dive into how the standard Xet library (xet-core/git_xet) integrates
with Git, and concrete patterns crab can borrow or improve upon.**

-----

## Document Metadata

| Field        | Value                                                         |
|--------------|---------------------------------------------------------------|
| Project      | crab                                                          |
| Scope        | xet-core library internals, git-xet integration, crab gaps    |
| Status       | Reference                                                     |
| Source code  | [Upstream xet-core `git_xet`](https://github.com/huggingface/xet-core/tree/main/git_xet) |
| Companion to | `Crab-xet.md` (crab's own dedup design)                       |
| Version      | 0.1                                                           |

-----

## Table of Contents

1. [Purpose](#1-purpose)
2. [xet-core Crate Topology](#2-xet-core-crate-topology)
3. [git-xet: The Git Integration Binary](#3-git-xet-the-git-integration-binary)
4. [The LFS Custom Transfer Agent Protocol](#4-the-lfs-custom-transfer-agent-protocol)
5. [Authentication Architecture](#5-authentication-architecture)
6. [The Upload Pipeline: clean_file → CAS](#6-the-upload-pipeline-clean_file--cas)
7. [Content-Defined Chunking (Gearhash CDC)](#7-content-defined-chunking-gearhash-cdc)
8. [Three-Tier Deduplication](#8-three-tier-deduplication)
9. [Defragmentation Prevention](#9-defragmentation-prevention)
10. [Xorb Assembly and Upload](#10-xorb-assembly-and-upload)
11. [Shard Construction and Upload](#11-shard-construction-and-upload)
12. [Session Resume](#12-session-resume)
13. [Progress Reporting](#13-progress-reporting)
14. [Pointer File Format (XetFileInfo)](#14-pointer-file-format-xetfileinfo)
15. [Architectural Comparison: xet-core vs crab](#15-architectural-comparison-xet-core-vs-crab)
16. [What crab Can Borrow](#16-what-crab-can-borrow)
17. [What crab Already Does Better](#17-what-crab-already-does-better)
18. [Recommended Action Items](#18-recommended-action-items)

-----

## 1. Purpose

This document captures the internal architecture of the xet-core library's
Git integration layer (`git_xet` crate) and its data pipeline (`xet_data`,
`xet_pkg` crates). The goal is twofold:

1. **Preserve institutional knowledge** about how the standard Xet system
   works, since crab aims for format-level compatibility with Xet-produced
   repositories on Hugging Face Hub.

2. **Identify concrete patterns** that crab can adopt, adapt, or
   intentionally diverge from — with rationale for each decision.

This is a reference document, not a spec. It describes what xet-core *does*,
not what crab *should* do. Action items are collected in §18.

-----

## 2. xet-core Crate Topology

The xet-core monorepo is organized into layered crates. Understanding the
dependency graph is essential for knowing where each responsibility lives.

```
git_xet                    ← the binary: git-xet CLI + LFS transfer agent
  ├── xet_pkg (hf-xet)    ← high-level API: sessions, upload/download
  │     ├── xet_data       ← data pipeline: chunking, dedup, xorb/shard I/O
  │     │     └── xet_core_structures  ← format types: MerkleHash, Shard, Xorb
  │     └── xet_client     ← CAS client: HTTP upload/download, auth tokens
  └── xet_runtime          ← config, tracing, tokio runtime management
```

Key observations:

- **git_xet is thin.** It handles CLI parsing, the LFS protocol state
  machine, and credential resolution. All data-plane work is delegated to
  xet_pkg/xet_data.

- **xet_data owns the pipeline.** Chunking, dedup, xorb packing, shard
  construction, and file reconstruction all live here. This is the crate
  crab's engine most closely parallels.

- **xet_client is the network boundary.** A `Client` trait abstracts
  upload/download. Implementations include `RemoteClient` (HTTP to CAS
  server), `LocalClient` (filesystem), and `MemoryClient` (tests).

- **xet_core_structures defines the wire formats.** `MerkleHash`,
  `MDBShardFile`, `SerializedXorbObject`, `MDBFileInfo` — these are the
  types that must match for format compatibility.


-----

## 3. git-xet: The Git Integration Binary

### 3.1 Binary Identity

`git-xet` is a single binary that serves as a **Git LFS custom transfer
agent**. It does NOT act as a remote helper or filter driver — those are
crab's integration points. The division:

| Integration point       | xet-core (git-xet)        | crab                        |
|-------------------------|---------------------------|-----------------------------|
| Remote helper           | No                        | Yes (`git-remote-crab`)     |
| Filter driver (clean)   | No (uses git-lfs filter)  | Yes (filter-process)        |
| Filter driver (smudge)  | No (uses git-lfs filter)  | Yes (filter-process)        |
| LFS custom transfer     | Yes (`git-xet transfer`)  | Yes (`crab lfs-transfer`)   |
| LFS batch API compat    | Server-side (HF Hub)      | Client-side (crab lfs)      |

git-xet relies on git-lfs being installed and configured. git-lfs handles
the filter driver (clean/smudge) and the Batch API negotiation. git-xet
only takes over the actual data transfer when the server selects the "xet"
transfer agent.

### 3.2 CLI Structure

```
git-xet install [--system|--local|--global] [--concurrency N]
git-xet uninstall [--all|--system|--local]
git-xet transfer          ← invoked by git-lfs, not by users
git-xet track <patterns>  ← thin wrapper around `git lfs track`
```

Source: `git_xet/src/app.rs`, `git_xet/src/bin/main.rs`

### 3.3 Install Mechanics

`git-xet install` writes three git config keys:

```ini
[lfs "customtransfer.xet"]
    path = git-xet
    args = transfer
    concurrent = true
```

It also sets `lfs.concurrenttransfers` if a concurrency value is provided,
and bootstraps `git lfs install` if the LFS filter isn't configured yet.

The install supports three scopes (`--system`, `--global`, `--local`) with
proper precedence. This is a clean pattern crab's own install could adopt
for its LFS transfer agent registration.

Source: `git_xet/src/app/install.rs`

-----

## 4. The LFS Custom Transfer Agent Protocol

### 4.1 Protocol Overview

When git-lfs negotiates with the server (via the Batch API) and the server
selects the "xet" transfer agent, git-lfs spawns `git-xet transfer` and
communicates over stdin/stdout using Line-Delimited JSON.

The protocol has three phases:

```
Phase 1: Init
  git-lfs → agent:  {"event":"init","operation":"upload","remote":"origin","concurrent":true}
  agent → git-lfs:  {}                    (success)
                     {"error":{"code":32,"message":"..."}}  (failure)

Phase 2: Transfer (0..N sequential requests)
  git-lfs → agent:  {"event":"upload","oid":"<sha256>","size":N,
                      "path":"/tmp/lfs/...",
                      "action":{"href":"https://...","header":{...}}}
  agent → git-lfs:  {"event":"progress","oid":"...","bytesSoFar":N,"bytesSinceLast":N}
                     ... (0 or more progress messages)
  agent → git-lfs:  {"event":"complete","oid":"..."}           (success)
                     {"event":"complete","oid":"...","error":{"code":2,"message":"..."}}

Phase 3: Terminate
  git-lfs → agent:  {"event":"terminate"}
  (no response expected; agent exits)
```

### 4.2 State Machine

The agent enforces valid state transitions:

```
PendingInit ──► InitedForUpload ──► Uploading ──► Uploading ──► ...
            └─► InitedForDownload ──► Downloading ──► Downloading ──► ...
```

Invalid transitions (e.g., upload before init, download after upload init)
return protocol errors. This prevents git-lfs bugs from causing silent
data corruption.

Source: `git_xet/src/lfs_agent_protocol/agent_state.rs`

### 4.3 The TransferAgent Trait

```rust
pub trait TransferAgent {
    async fn init_upload(&mut self, req: &InitRequestInner) -> Result<()>;
    async fn init_download(&mut self, req: &InitRequestInner) -> Result<()>;
    async fn upload_one<W>(&mut self, req: &TransferRequest,
                           progress: ProgressUpdater<W>) -> Result<()>;
    async fn download_one<W>(&mut self, req: &TransferRequest,
                             progress: ProgressUpdater<W>) -> Result<PathBuf>;
    async fn terminate(&mut self) -> Result<()>;
}
```

`XetAgent` implements this trait. The protocol loop (`lfs_protocol_loop`)
handles JSON parsing, state validation, and error formatting — the agent
only implements business logic.

This separation is clean and directly reusable. crab's LFS transfer agent
could adopt the same trait boundary.

### 4.4 Critical Constraint: SIGKILL After 30 Seconds

git-lfs sends SIGKILL (not SIGTERM) to the transfer agent 30 seconds after
the terminate event. This is not interceptable. Consequences:

- **Per-file finalization is mandatory.** git-xet calls `session.finalize()`
  after each file upload, not in a batch at the end. Batching would risk
  data loss if the agent is killed before the batch completes.

- **Shard upload must happen per-file.** The shard containing a file's
  reconstruction info must be uploaded before the agent reports success for
  that file. Otherwise, the xorb data is orphaned.

This constraint applies equally to crab's LFS transfer agent.

Source: `git_xet/src/app/xet_agent.rs` (see the comment block in `upload_one`)


-----

## 5. Authentication Architecture

### 5.1 Multi-Strategy Credential Cascade

git-xet resolves credentials in a strict priority order. This is one of the
most well-designed parts of the codebase.

```
Priority 1: AccessMode check
  Read lfs.<url>.access from git config.
  If "none" → NoopCredentialHelper (public repo, no auth needed).

Priority 2: URL-embedded credentials
  Parse https://user:token@hf.co/repo → BearerCredentialHelper("url")

Priority 3: Environment variable
  $HF_TOKEN → BearerCredentialHelper("env")

Priority 4: Netrc file
  ~/.netrc machine match → BearerCredentialHelper("netrc")

Priority 5: SSH (for git@host:repo URLs)
  Shell out to `git-lfs-authenticate` over SSH channel.
  Remote returns JSON: {header:{Authorization:"Basic ..."}, href, expires_in}
  → SSHCredentialHelper

Priority 6: Git credential helper (fallback)
  `git credential fill` with url=<host_url>
  Invokes whatever credential helper the user has configured
  (osxkeychain, wincred, credential-store, etc.)
  → GitCredentialHelper
```

Source: `git_xet/src/auth.rs`, `git_xet/src/auth/git.rs`,
`git_xet/src/auth/ssh.rs`

### 5.2 Token Refresh

CAS tokens have shorter TTLs than upload sessions. The
`DirectRefreshRouteTokenRefresher` handles mid-session token refresh using
the same credential source that provided the initial token.

The refresh route URL comes from the `action.href` field in the LFS Batch
API response. The refresher makes an HTTP request to this URL with the
current credentials to obtain a new token.

Source: `git_xet/src/token_refresher.rs`

### 5.3 SSH Authentication Flow

For SSH remotes (`git@hf.co:user/repo`), git-xet replicates the exact
mechanism git-lfs uses:

1. Parse the remote URL to extract user, host, port, and repo path.
2. Construct an SSH command: `ssh git@hf.co git-lfs-authenticate <repo> upload`
3. Execute via the user's configured SSH client (respects `GIT_SSH_COMMAND`,
   `core.sshCommand`, and `~/.ssh/config`).
4. Parse the JSON response containing an Authorization header and LFS
   endpoint URL.

This ensures git-xet works with any SSH key setup that git-lfs already
works with — no additional configuration needed.

### 5.4 Relevance to crab

crab's LFS compatibility layer needs the same credential cascade for
interoperating with Hugging Face Hub. The SSH flow is particularly important
because many HF users authenticate via SSH keys rather than tokens.

crab's remote helper (for `crab://` URLs) uses IAM/cloud credentials
instead, but the LFS transfer agent path needs git-compatible auth.

-----

## 6. The Upload Pipeline: clean_file → CAS

### 6.1 Entry Point

The upload pipeline starts at `clean_file()` in `xet_data/src/processing/data_client.rs`:

```rust
pub async fn clean_file(
    processor: Arc<FileUploadSession>,
    filename: impl AsRef<Path>,
    sha256_policy: Sha256Policy,
) -> Result<(XetFileInfo, DeduplicationMetrics)> {
    let mut reader = File::open(&filename)?;
    let (_id, mut handle) = processor.start_clean(...)?;

    loop {
        let bytes = reader.read(&mut buffer)?;
        if bytes == 0 { break; }
        handle.add_data(&buffer[0..bytes]).await?;
    }

    handle.finish().await
}
```

This is a streaming pipeline: file data flows through in `ingestion_block_size`
chunks (typically ~4 MB), never fully buffered.

### 6.2 Pipeline Stages

```
File bytes (streaming)
    │
    ▼
┌─────────────────────────────────────────────────────────────┐
│  Stage 1: SingleFileCleaner                                 │
│  - Receives raw bytes from file reader                      │
│  - Feeds Chunker (gearhash CDC) on spawn_blocking thread    │
│  - Feeds SHA-256 hasher in parallel (optional)              │
│  - Passes completed chunks to FileDeduper                   │
└─────────────────────────────────────────────────────────────┘
    │ Vec<Chunk>
    ▼
┌─────────────────────────────────────────────────────────────┐
│  Stage 2: FileDeduper                                       │
│  - 3-tier dedup query per chunk (session → cache → global)  │
│  - Intra-xorb dedup (local hash lookup)                     │
│  - Defragmentation prevention                               │
│  - Accumulates new chunks; cuts xorbs at threshold          │
│  - Builds FileDataSequenceEntry list (reconstruction info)  │
└─────────────────────────────────────────────────────────────┘
    │ RawXorbData (when threshold reached)
    ▼
┌─────────────────────────────────────────────────────────────┐
│  Stage 3: FileUploadSession                                 │
│  - Serializes xorb (compression on blocking thread)         │
│  - Acquires upload permit (backpressure)                    │
│  - Spawns async upload task to CAS                          │
│  - Registers xorb in session shard for future dedup         │
│  - Tracks per-file completion via CompletionTracker         │
└─────────────────────────────────────────────────────────────┘
    │ on finalize()
    ▼
┌─────────────────────────────────────────────────────────────┐
│  Stage 4: SessionShardInterface                             │
│  - Consolidates session shards (merge small ones)           │
│  - Uploads shards to CAS                                    │
│  - Moves uploaded shards to local cache                     │
│  - Cleans up staging from resumed sessions                  │
└─────────────────────────────────────────────────────────────┘
```

### 6.3 Concurrency Model

- **File-level parallelism:** `upload_files()` spawns one tokio task per
  file, gated by `file_ingestion_semaphore`.
- **Chunking:** runs on `spawn_blocking` (CPU-bound gearhash computation).
- **Dedup queries:** async, with global dedup queries running in background
  `JoinSet` tasks.
- **Xorb serialization:** `spawn_blocking` (CPU-bound compression).
- **Xorb upload:** async tasks in a `JoinSet`, gated by
  `client.acquire_upload_permit()`.
- **Shard upload:** parallel `JoinSet` tasks, also permit-gated.

The pipeline is designed so that chunking, dedup, serialization, and upload
can all overlap. A file's first xorb can be uploading while its later chunks
are still being deduped.


-----

## 7. Content-Defined Chunking (Gearhash CDC)

### 7.1 Algorithm

xet-core uses the `gearhash` crate for content-defined chunking. The
algorithm:

1. Maintain a 64-byte sliding window hash.
2. For each byte position, compute `hash = gearhash(window)`.
3. Test `hash & mask == 0`. If true, declare a chunk boundary.
4. The mask is `(target_size - 1) << leading_zeros` — shifted left so the
   high bits of the hash (which are influenced by more bytes) determine
   boundaries.

### 7.2 Parameters (Actual Defaults)

From `xet_core_structures/src/xorb_object/constants.rs`:

```rust
TARGET_CHUNK_SIZE       = 64 * 1024       // 64 KiB
MINIMUM_CHUNK_DIVISOR   = 8               // min = 64 KiB / 8 = 8 KiB
MAXIMUM_CHUNK_MULTIPLIER = 2              // max = 64 KiB × 2 = 128 KiB
MAX_XORB_BYTES          = 64 * 1024 * 1024  // 64 MiB
MAX_XORB_CHUNKS         = 8 * 1024          // 8192 chunks
```

So the effective chunk size range is **8 KiB – 128 KiB** with a **64 KiB**
target. At 64 KiB average, a 64 MiB xorb holds ~1024 chunks.

The minimum prevents pathologically small chunks. The maximum forces a
boundary on highly random data that never triggers the hash condition.

crab's `GearChunker` uses the same gearhash algorithm and should use
identical parameters for format compatibility. Any divergence in chunk
boundaries breaks cross-system dedup.

### 7.3 Implementation Details

- **Minimum skip optimization:** The chunker skips the first
  `minimum_chunk - 64 - 1` bytes without testing the hash. Since the hash
  window is 64 bytes, boundaries before `minimum_chunk` bytes are impossible
  to trigger based on current-chunk content alone.

- **Streaming:** The `Chunker` struct maintains state across `next_block()`
  calls. Partial data is buffered internally (up to `maximum_chunk` bytes).

- **Zero-copy:** `next_block_bytes()` accepts `&Bytes` and produces chunks
  that share the underlying `Bytes` allocation via `Bytes::slice()`.

- **SIMD:** The `gearhash` crate includes AVX2/NEON implementations for
  the boundary search, achieving ~4x speedup over scalar.

### 7.4 Determinism Guarantee

Same input bytes always produce the same chunk boundaries and chunk hashes.
This is the foundation of dedup correctness. The chunker is single-threaded
per file (no parallelism within a file's chunk stream) to preserve this
property.

### 7.5 Comparison with crab

crab uses the same gearhash CDC algorithm (via xet-core's chunker or a
compatible implementation). The parameters should match for format
compatibility with Xet-produced repositories. Any divergence in chunk
boundaries would break cross-system dedup.

-----

## 8. Three-Tier Deduplication

### 8.1 Overview

`FileDeduper::process_chunks()` implements a three-tier dedup lookup for
each chunk. This is the most sophisticated part of the xet-core data
pipeline.

### 8.2 Tier 1: Session-Local Dedup

**What:** Checks the current upload session's in-memory shard manager.

**When it helps:** Two files in the same push share overlapping content.
For example, uploading `model-v1.bin` and `model-v2.bin` where 90% of
the weights are identical.

**How:** `SessionShardInterface` maintains an in-memory `ShardFileManager`
that is populated as xorbs are registered during the session. The
`chunk_hash_dedup_query()` method does a hash lookup against all registered
xorb chunk lists.

**Cost:** In-memory hash table lookup. Essentially free.

### 8.3 Tier 2: Cache Dedup

**What:** Checks the local shard cache directory from previous sessions.

**When it helps:** A user pushes an updated model that shares chunks with
a previously pushed version. The previous session's shards are in the
local cache.

**How:** `ShardFileManager` for the cache directory. Shards are
content-addressed and immutable, so they never need invalidation — only
expiration (configurable TTL).

**Cost:** Disk I/O for shard file reads, but shards are typically small
(< 10 MB) and OS-cached after first access.

### 8.4 Tier 3: Global Dedup

**What:** Queries the CAS server for a "global dedup shard" that might
contain chunk information from other users' uploads.

**When it helps:** User A uploaded a model; user B uploads a fork with
minor changes. Without global dedup, user B re-uploads all shared chunks.
With it, the server provides a shard describing user A's chunks, and user B
deduplicates against them.

**How:**

1. For eligible chunks (first chunk of each file, plus chunks matching a
   hash-based sampling pattern), `register_global_dedup_query()` spawns a
   background task.
2. The task calls `client.query_for_global_dedup_shard()` with the chunk
   hash.
3. If the server returns a shard, it's imported into the cache shard
   manager.
4. `complete_global_dedup_queries()` waits for all background queries,
   then returns whether new shards were added.
5. If new shards were added, the dedup pass re-runs from the beginning
   of the current chunk batch (two-pass design).

**Rate limiting:** Global queries are spaced by
`min_spacing_between_global_dedup_queries` chunks to avoid flooding the
server.

**Cost:** Network round-trip per query. The two-pass design means chunks
are processed twice when global dedup finds new shards, but this is rare
and the second pass is fast (local lookups only).

### 8.5 Intra-Xorb Dedup (Tier 0)

Before the three tiers, `FileDeduper` also checks
`dedup_query_against_local_data()` — a hash lookup against chunks already
accumulated in the current (not-yet-cut) xorb. This catches repetitive
patterns within a single file (e.g., a file with repeated blocks).

### 8.6 Dedup Query Result Format

All tiers return the same type:

```rust
Option<(usize, FileDataSequenceEntry, bool)>
//      ^count  ^reconstruction info    ^is_already_uploaded
```

- `count`: number of consecutive chunks that matched.
- `FileDataSequenceEntry`: xorb hash + chunk index range + byte count.
- `is_already_uploaded`: true if the referenced xorb is known to be on
  the server (from cache or resumed session). False if it's only in the
  current session (not yet uploaded).

### 8.7 Relevance to crab

crab's dedup engine (`engine/dedup.rs`) implements a simpler 2-tier
classification:

- **Tier 1 (ChunkIndex):** Populated during step 3 (`shard_sync`) by
  downloading and parsing shards from the remote. This covers both
  xet-core's "cache" and "session" tiers — the ChunkIndex contains all
  chunk→xorb mappings from previously uploaded shards.

- **Tier 2 (session HashSet):** A `HashSet<MerkleHash>` in the
  `Classifier` tracks chunks seen during the current push. This catches
  intra-push duplicates (equivalent to xet-core's session-local tier).

**What's missing:**

- **Consecutive-chunk run matching.** xet-core's `chunk_hash_dedup_query`
  returns the number of consecutive chunks that match, enabling efficient
  segment construction. crab classifies one chunk at a time, which means
  the shard builder (step 8) must reconstruct runs from individual
  classifications.

- **Global dedup queries.** For `crab://` remotes (pure object storage),
  there's no server to query. However, for HF Hub LFS compatibility,
  crab could implement global dedup queries against the HF CAS API.

- **Intra-xorb dedup during classification.** xet-core's `FileDeduper`
  checks `dedup_query_against_local_data()` against chunks in the
  not-yet-cut xorb. crab's `CdcXorbBuilder` handles this separately
  via its `seen` HashSet, which deduplicates at pack time rather than
  classification time. The effect is similar but the dedup decision
  happens at a different stage.


-----

## 9. Defragmentation Prevention

### 9.1 The Problem

Aggressive dedup can create file reconstruction info with hundreds of tiny
references to different xorbs. Example:

```
File: model.bin (1 GB)
Without defrag prevention:
  segment 1: xorb_A chunks 0-3    (256 KB)
  segment 2: xorb_B chunks 12-14  (192 KB)
  segment 3: xorb_C chunks 0-1    (128 KB)
  segment 4: xorb_A chunks 7-9    (192 KB)
  ... (200+ segments)
```

Reading this file requires 200+ Range GET requests to different xorbs.
Even with HTTP/2 multiplexing, this is slow.

### 9.2 The Solution: DefragPrevention Tracker

`FileDeduper` includes a `DefragPrevention` tracker that monitors the
fragmentation of the file's reconstruction info. When dedup would cause
excessive fragmentation, it's skipped — the chunk is stored as new data
instead.

The tracker uses a rolling window of the last N segments (configured by
`nranges_in_streaming_fragmentation_estimator`) to compute the average
chunks-per-range (CPR):

- `add_range_to_fragmentation_estimate(n_chunks)`: records a new segment.
- `increment_last_range_in_fragmentation_estimate(n_chunks)`: extends the
  current segment (contiguous continuation — no new range added).
- `allow_dedup_on_next_range(n_chunks)`: returns true if dedup is allowed
  given the current fragmentation level.

The algorithm uses **hysteresis** to avoid oscillating between dedup and
no-dedup states:

```
if CPR < min_chunks_per_range (or hysteresis-adjusted threshold):
    if proposed_dedup_range_size < current CPR:
        REJECT dedup (the small match would worsen fragmentation)
        raise threshold (switch to high threshold to recover CPR)
    else:
        ALLOW (large match improves CPR even in fragmented state)
else:
    ALLOW dedup
    lower threshold (switch to low threshold = min * hysteresis_factor)
```

The hysteresis factor (< 1.0) means once the tracker starts rejecting
dedup, it requires CPR to recover above the higher threshold before
re-enabling. This prevents rapid flip-flopping.

Continuing the current segment (same xorb, next chunk index) is always
free — it just extends the existing entry via
`increment_last_range_in_fragmentation_estimate`. Only cross-xorb
references contribute to fragmentation.

### 9.3 Interaction with Dedup

In `FileDeduper::process_chunks()`:

```rust
if self.file_data_sequence_continues_current(&fse)
    || self.defrag_tracker.allow_dedup_on_next_range(n_deduped)
{
    // Accept dedup
    self.add_file_data_sequence_entry(fse, n_deduped);
} else {
    // Reject dedup to prevent fragmentation
    dedup_metrics.defrag_prevented_dedup_chunks += n_deduped;
    dedup_metrics.defrag_prevented_dedup_bytes += fse.unpacked_segment_bytes;
    // Fall through to "store as new data" path
}
```

Continuing the current segment (same xorb, next chunk index) is always
free — it just extends the existing entry. Only cross-xorb references
contribute to fragmentation.

### 9.4 Relevance to crab

crab's dedup engine should implement defragmentation prevention. Without
it, the smudge/hydrate path will suffer from excessive Range GET requests
on files that have been heavily deduped across many xorbs.

The metrics (`defrag_prevented_dedup_chunks/bytes`) are valuable for
tuning — they show how much potential dedup was sacrificed for read
performance.

-----

## 10. Xorb Assembly and Upload

### 10.1 When Xorbs Are Cut

New chunks accumulate in `FileDeduper.new_data`. A xorb is cut when:

```rust
if self.new_data_size + n_bytes > *XORB_CUT_THRESHOLD_BYTES
    || self.new_data.len() + 1 > *XORB_CUT_THRESHOLD_CHUNKS
{
    let new_xorb = self.cut_new_xorb();
    self.data_mng.register_new_xorb(new_xorb).await?;
}
```

Thresholds are defined in `xet_core_structures::xorb_object::constants`:
- `XORB_CUT_THRESHOLD_BYTES`: defaults to `MAX_XORB_BYTES` = **64 MiB**.
- `XORB_CUT_THRESHOLD_CHUNKS`: defaults to `MAX_XORB_CHUNKS` = **8192**.
- In simulation builds, these can be lowered via config for testing.

Note: xet-core uses **fixed-size thresholds** for xorb boundaries. A xorb
is cut when accumulated data exceeds 64 MiB or 8192 chunks. This is simple
but means xorb boundaries shift when chunks are inserted or deleted in the
stream — a single new chunk at the beginning of a file shifts all subsequent
xorb boundaries, preventing cross-push xorb-level dedup.

crab's `CdcXorbBuilder` uses a **content-defined** approach instead (see
§17.6), which is more resilient to insertions.

### 10.2 Cross-File Xorb Merging

When a file finishes cleaning, its remaining (un-cut) chunks go to
`FileUploadSession::register_single_file_clean_completion()`. This method
merges the file's leftover data with the session's `current_session_data`
(a `DataAggregator`).

If the combined size exceeds the threshold, the larger of the two is cut
as a xorb. This means chunks from different files can end up in the same
xorb — which is fine for dedup but means xorb boundaries don't align with
file boundaries.

### 10.3 Upload Flow

```rust
// In FileUploadSession::register_new_xorb():

// 1. Dedup check: skip if this exact xorb was already registered
let xorb_is_new = self.completion_tracker.register_new_xorb(xorb_hash, ...);
if !xorb_is_new { return Ok(false); }

// 2. Register in session shard (so other files can dedup against it)
self.shard_interface.add_xorb_block(xorb_info).await?;

// 3. Serialize (compression, on blocking thread)
let xorb_obj = XetRuntime::current()
    .spawn_blocking(move || SerializedXorbObject::from_xorb(xorb, false))
    .await??;

// 4. Acquire upload permit (backpressure)
let upload_permit = self.client.acquire_upload_permit().await?;

// 5. Spawn upload task
self.xorb_upload_tasks.lock().await.spawn(async move {
    let n_bytes = session.client
        .upload_xorb(&cas_prefix, xorb_obj, Some(progress_callback), upload_permit)
        .await?;
    session.completion_tracker.register_xorb_upload_completion(xorb_hash);
    session.shard_interface.add_uploaded_xorb_block(xorb_info).await?;
    Ok(())
});
```

Key design decisions:

- **Register before upload.** The xorb is added to the session shard
  *before* the upload starts. This allows other files being processed
  concurrently to dedup against it immediately. The shard isn't uploaded
  until all xorbs are confirmed uploaded, so this is safe.

- **Backpressure via permits.** Without this, the pipeline could produce
  xorbs faster than the network can upload them, causing unbounded memory
  growth.

- **Progress callback.** The upload reports byte-level progress, which
  feeds into the `CompletionTracker` for per-file progress reporting.

### 10.4 Finalization

`FileUploadSession::finalize()`:

1. Cut the remaining `current_session_data` as a final xorb.
2. Wait for all in-flight xorb upload tasks to complete.
3. Upload and register session shards (see §11).
4. Return aggregated `DeduplicationMetrics`.

-----

## 11. Shard Construction and Upload

### 11.1 What Shards Contain

Shards are the metadata that maps files to their chunk sequences in xorbs.
Each shard contains:

- **Xorb info blocks:** For each xorb, the list of chunk hashes and sizes.
  This is the dedup index — future sessions query this to find existing
  chunks.

- **File reconstruction info (`MDBFileInfo`):** For each file, a list of
  `FileDataSequenceEntry` records:
  ```rust
  pub struct FileDataSequenceEntry {
      pub xorb_hash: MerkleHash,
      pub unpacked_segment_bytes: u32,
      pub chunk_index_start: u32,
      pub chunk_index_end: u32,  // exclusive
  }
  ```
  This tells the download path: "to reconstruct this file, fetch chunks
  `start..end` from xorb `hash`, then chunks `start..end` from the next
  xorb, etc."

- **File verification entries:** Per-segment range hashes for integrity
  verification during reconstruction.

- **File metadata extension (optional):** SHA-256 hash of the original
  file, used for LFS OID verification.

### 11.2 Shard Lifecycle

```
During upload session:
  ├─ add_xorb_block()              → records xorb metadata
  ├─ add_file_reconstruction_info() → records file → chunk mapping
  └─ add_uploaded_xorb_block()     → stages for session resume

On finalize():
  ├─ flush() all in-memory data to disk
  ├─ consolidate_shards_in_directory() → merge small shards
  ├─ For each consolidated shard:
  │   ├─ Read shard, strip footer (server reconstructs it)
  │   ├─ Acquire upload permit
  │   ├─ Upload to CAS
  │   └─ Move to local cache with expiration TTL
  └─ Clean up obsolete staging shards
```

### 11.3 Shard Caching

Uploaded shards are moved to the local cache directory with a configurable
expiration (`MDB_SHARD_LOCAL_CACHE_EXPIRATION`). Future sessions load these
cached shards for tier-2 dedup without network I/O.

The cache uses content-addressed filenames (shard hash), so there's no
invalidation problem — shards are immutable.

-----

## 12. Session Resume

### 12.1 The Problem

Large uploads can take hours. If the process crashes or the network drops,
re-uploading everything from scratch wastes time and bandwidth.

### 12.2 xet-core's Solution

The `SessionShardInterface` periodically flushes xorb metadata to a staging
directory (`xorb_metadata_staging_dir`):

```rust
// Every flush_interval or when max_count xorbs accumulated:
xorb_shard.write_to_directory(&self.xorb_metadata_staging_dir, Some(expiration))?;
```

On the next session start:

1. `merge_shards_background()` scans the staging directory for valid shards.
2. Valid shards are merged and loaded into a `resumed_session_shard_manager`.
3. Xorbs referenced by resumed shards are treated as "already uploaded" —
   dedup queries against them return `is_external = true`.
4. After successful finalization, obsolete staging shards are cleaned up.

### 12.3 Relevance to crab

crab's push pipeline could benefit from a similar resume mechanism,
especially for large pushes over unreliable networks. The key insight is
that xorb metadata (which xorbs have been uploaded) is cheap to persist
and enables skipping re-upload of already-uploaded data.

Currently, crab's push does HEAD checks for xorb existence (step 6 in
the push pipeline), which achieves a similar effect but requires N HEAD
requests on resume. The staged-shard approach avoids these network calls.


-----

## 13. Progress Reporting

### 13.1 The ProgressUpdater

git-xet's `ProgressUpdater` reports upload progress back to git-lfs via
the stdout JSON protocol. Its design is notable for being lock-free and
wait-free for concurrent callers:

```rust
pub struct ProgressUpdater<W: Write> {
    update_channel: Arc<Mutex<W>>,
    request_oid: String,
    bytes_so_far: AtomicU64,      // monotonic via fetch_max
    bytes_last_sent: AtomicU64,
}

impl ProgressUpdater {
    pub fn update_bytes_so_far(&self, number: u64) -> Result<()> {
        let current = self.bytes_so_far.fetch_max(number, Ordering::Relaxed);
        if current < number {
            self.try_send_update_message()?;
        }
        Ok(())
    }

    fn try_send_update_message(&self) -> Result<()> {
        let Ok(mut channel) = self.update_channel.try_lock() else {
            return Ok(());  // channel busy, skip this message
        };
        // Send with latest bytes_so_far (may have been updated by another thread)
        ...
    }
}
```

Key properties:

- **Monotonic progress:** `fetch_max` ensures `bytes_so_far` never
  decreases, even with out-of-order concurrent updates.
- **Non-blocking:** `try_lock` means concurrent callers don't wait for
  the channel. Only the first to acquire the lock sends; others skip.
  This prevents progress reporting from becoming a bottleneck.
- **First-worker unblock:** git-lfs waits for the first progress message
  before starting additional workers. git-xet sends a dummy progress(1)
  immediately after init to trigger parallel uploads.

### 13.2 CompletionTracker

At a higher level, `CompletionTracker` tracks per-file upload completion
across shared xorbs:

- Each file registers its xorb dependencies.
- As xorbs complete upload, the tracker updates per-file progress.
- A file is "complete" when all its dependent xorbs are uploaded.

This enables accurate per-file progress bars even though xorbs are shared
across files and uploaded asynchronously.

### 13.3 Relevance to crab

crab's LFS transfer agent needs the same non-blocking progress pattern.
The `AtomicU64` + `try_lock` approach is directly adoptable.

The `CompletionTracker` pattern is also valuable for crab's push pipeline,
where multiple files share xorbs and progress needs to be reported per-file
to the user.

-----

## 14. Pointer File Format (XetFileInfo)

### 14.1 Format

```rust
pub struct XetFileInfo {
    pub hash: String,           // MerkleHash hex (64 chars)
    pub file_size: Option<u64>,
    pub sha256: Option<String>, // SHA-256 hex (64 chars), for LFS OID verification
}
```

Serialized as JSON:

```json
{"hash":"7c1f2a3b...","file_size":10737418240,"sha256":"a1b2c3d4..."}
```

### 14.2 Hash Computation

The `hash` field is a MerkleHash computed from the ordered chunk hashes:

```rust
pub fn file_hash(chunk_hashes: &[(MerkleHash, u64)]) -> MerkleHash {
    // Merkle tree construction over chunk hashes
}
```

This is deterministic: same file content → same chunks → same chunk hashes
→ same file hash. The hash is independent of xorb boundaries — two sessions
that chunk the same file identically will produce the same file hash even
if they pack chunks into different xorbs.

### 14.3 SHA-256 Policy

The `Sha256Policy` enum controls whether SHA-256 is computed:

- `Compute`: hash the file data during clean (default for git-xet).
- `Provided(hash)`: use a pre-computed value (e.g., from git-lfs OID).
- `Skip`: don't compute SHA-256 (saves CPU for non-LFS use cases).

When provided or computed, the SHA-256 is stored in the shard's
`FileMetadataExt` for verification during download.

### 14.4 Comparison with crab

crab uses a different pointer format:

```
version https://crab.dev/spec/v1
file-hash 7c1f2a3b...
size 10737418240
shard-hint a1b2c3d4...
```

The crab format is line-oriented (like git-lfs pointers) rather than
JSON. For Xet compatibility, crab needs to be able to read and write
both formats — or at least read XetFileInfo JSON when interoperating with
Xet-produced repositories.

-----

## 15. Architectural Comparison: xet-core vs crab

### 15.1 Integration Model

| Aspect                    | xet-core (git-xet)               | crab                          |
|---------------------------|----------------------------------|---------------------------------|
| Git integration point     | LFS custom transfer agent only   | Remote helper + filter driver + LFS agent |
| Dependency on git-lfs     | Required (handles filter/batch)  | Optional (crab has its own filter) |
| Clean/smudge              | Delegated to git-lfs             | Native filter-process           |
| Remote transport          | Delegated to git-lfs + git       | Native remote helper            |
| Storage backend           | CAS server (HTTP API)            | Object storage (S3/GCS/Azure)   |
| Server requirement        | Yes (HF Hub CAS)                 | No (serverless)                 |

### 15.2 Data Pipeline

| Aspect                    | xet-core                         | crab                          |
|---------------------------|----------------------------------|---------------------------------|
| Chunking algorithm        | Gearhash CDC (64 KiB target)     | Gearhash CDC (compatible)       |
| Chunk hash                | MerkleHash (Blake3-based)        | MerkleHash (same type)          |
| Dedup tiers               | 3 (session + cache + global)     | 2 (ChunkIndex from shard sync + session HashSet) |
| Dedup granularity         | Consecutive-chunk run matching   | Single-chunk classification (A/B/C) |
| Defrag prevention         | Yes (DefragPrevention tracker with hysteresis) | Not yet implemented |
| Xorb boundary strategy    | Fixed threshold (64 MiB / 8192 chunks) | CDC over chunk-hash sequence (rolling polynomial) |
| Cross-file xorb merging   | Yes (DataAggregator merges leftovers) | No (each file's chunks packed independently) |
| Intra-xorb dedup          | Yes (hash lookup in accumulator)  | Yes (HashSet in CdcXorbBuilder) |
| Shard format              | MDB shard (xet_core_structures)  | MDB shard (compatible)          |
| File verification entries | Yes (per-segment range hashes)   | Not yet (empty `verification` vec) |
| SHA-256 / metadata_ext    | Yes (Sha256Policy: Compute/Provided/Skip) | Not yet (metadata_ext: None) |
| Session resume            | Staged xorb metadata to disk     | HEAD checks on resume (step 6)  |
| Backpressure              | Per-upload permits from Client    | Semaphore-based concurrency     |
| Xorb compression          | Per-chunk zstd (configurable level) | Per-chunk zstd (ZSTD_LEVEL constant) |

### 15.3 Authentication

| Aspect                    | xet-core                         | crab                          |
|---------------------------|----------------------------------|---------------------------------|
| Primary auth              | Git credential helpers           | IAM / cloud credentials         |
| LFS auth                  | 6-tier cascade (see §5)          | Basic token support             |
| SSH support               | Full (git-lfs-authenticate)      | Not yet for LFS path            |
| Token refresh             | DirectRefreshRouteTokenRefresher | Not yet implemented             |

-----

## 16. What crab Can Borrow

### 16.1 Defragmentation Prevention (High Priority)

crab's dedup engine currently lacks defrag prevention. The `Classifier`
in `engine/dedup.rs` classifies each chunk independently as A/B/C without
considering the impact on reconstruction fragmentation. On repositories
with heavy cross-version dedup, this will cause smudge/hydrate to issue
excessive Range GET requests, degrading read performance.

**The gap in detail:** crab's push pipeline (step 4: `classify_chunks`)
walks all chunks and classifies them, then step 5 (`pack_xorbs`) packs
only class-C (new) chunks. The reconstruction terms in step 8
(`build_shard`) reference both new xorbs and existing xorbs from the
ChunkIndex. If a file's chunks alternate between new and existing xorbs
(e.g., small edits scattered throughout a large file), the reconstruction
info will have many short segments pointing to different xorbs.

**What xet-core does:** The `DefragPrevention` tracker maintains a rolling
window of the last N segments and computes average chunks-per-range (CPR).
When CPR drops below a configurable threshold (`min_n_chunks_per_range`),
it rejects dedup for small matches — forcing those chunks to be stored as
new data in the current xorb. This trades storage for read performance.
A hysteresis factor prevents oscillation between dedup and no-dedup states.

**Recommended implementation for crab:**

1. Add a `DefragTracker` to the `Classifier` or introduce it as a
   post-classification filter in `classify_chunks`.
2. Track a rolling window of segment lengths (how many consecutive chunks
   reference the same xorb).
3. When the average segment length drops below a threshold (e.g., 4 chunks),
   reclassify small Existing matches as New — forcing them into the current
   xorb.
4. Expose `defrag_prevented_chunks` and `defrag_prevented_bytes` metrics
   for tuning.
5. Make the threshold configurable via `EngineConfig`.

The key insight: this only matters during the classification phase. The
xorb packer and shard builder don't need to change — they just see more
class-C chunks when defrag prevention kicks in.

### 16.2 Session Resume via Staged Metadata (High Priority)

crab currently uses HEAD checks (step 6: `head_check_resume`) to detect
already-uploaded xorbs on push resume. The `StoreHeadBatch` issues batched
HEAD requests (or prefix LIST for large batches) against the remote store.
This works but requires N network round-trips (one per xorb, or one LIST
per prefix batch).

**What xet-core does:** The `SessionShardInterface` periodically flushes
xorb metadata to `xorb_metadata_staging_dir` on disk. On the next session
start, `merge_shards_background()` loads these staged shards into a
`resumed_session_shard_manager`. Xorbs referenced by resumed shards are
treated as already-uploaded — dedup queries return `is_external = true`,
and the upload step skips them entirely. No network I/O needed.

**The gap:** For a push with 1000 xorbs over a high-latency connection
(e.g., 100ms RTT to S3), HEAD checks add ~100 seconds of latency even
with batching. The staged-metadata approach eliminates this entirely.

**Recommended implementation for crab:**

1. During step 7 (`upload_xorbs`), after each successful xorb upload,
   append the xorb hash to a local journal file in the staging area
   (e.g., `.crab/staging/push-journal/{push_id}.json`).
2. On push resume (detected by `list_inflight()` returning a non-empty
   set), load the journal to build a set of already-uploaded xorb hashes.
3. In step 6, skip HEAD checks for xorbs in the journal set.
4. On successful push completion (step 13: `post_success_cleanup`),
   delete the journal file.
5. On push failure, the journal persists for the next attempt.

This is simpler than xet-core's shard-based approach (no need to build
and merge MDB shards) and fits naturally into crab's existing staging
area infrastructure.

### 16.3 LFS Custom Transfer Agent Protocol (Medium Priority)

crab already has LFS support, but the protocol implementation could
benefit from xet-core's patterns:

- **TransferAgent trait:** Clean separation between protocol handling and
  business logic.
- **State machine:** Rigorous validation of protocol state transitions.
- **Per-file finalization:** Mandatory due to git-lfs's 30s SIGKILL.
- **First-worker progress trick:** Send progress(1) immediately to unblock
  parallel git-lfs workers.

### 16.4 Multi-Strategy Auth Cascade (Medium Priority)

For HF Hub LFS compatibility, crab needs the same credential resolution
chain that git-xet uses. The SSH `git-lfs-authenticate` flow is particularly
important for users who authenticate via SSH keys.

**Action:** Implement the 6-tier cascade in crab's LFS transfer agent
path. The `crab://` remote helper path can continue using IAM credentials.

### 16.5 Cross-File Xorb Merging (Medium Priority)

xet-core's `DataAggregator` merges leftover chunks from different files
into shared xorbs. When `SingleFileCleaner::finish()` returns remaining
data, `FileUploadSession::register_single_file_clean_completion()` merges
it with the session's `current_session_data`. If the combined size exceeds
the threshold, the larger of the two is cut as a xorb. This means chunks
from different files can share a xorb, reducing the number of undersized
xorbs.

**crab's current behavior:** The push pipeline (step 5: `pack_xorbs`)
iterates over all pointers and feeds their chunks into a single
`CdcXorbBuilder`. The builder does handle cross-file chunk streams — chunks
from file A and file B flow through the same rolling hash and the same
xorb accumulator. So crab already achieves some cross-file merging at
the xorb level.

However, there's a subtle difference: crab's `CdcXorbBuilder` only
packs class-C (new) chunks. Class-A (existing) chunks are skipped entirely
in step 5. This means the chunk stream fed to the builder has gaps where
existing chunks were removed. These gaps can cause the rolling hash to
produce different boundaries than if the full stream were present.

xet-core's approach is different: the `FileDeduper` interleaves dedup
decisions with xorb cutting. New chunks accumulate in `new_data`, and
when the threshold is reached, a xorb is cut from exactly those new chunks.
The dedup decisions and xorb boundaries are tightly coupled.

**The gap:** For pushes with many small files (e.g., 1000 files of 100 KB
each), crab's approach works well because the `CdcXorbBuilder` naturally
accumulates them into larger xorbs. For pushes with a few large files
where most chunks are existing, the gaps in the chunk stream may cause
suboptimal xorb boundaries.

**Action:** This is lower priority than defrag prevention. The current
approach is functional and the CDC boundary strategy mitigates most of the
boundary-shift problem. Monitor xorb size distribution in production to
determine if further optimization is needed.

### 16.6 Upload Backpressure via Permits (Low Priority)

xet-core's `acquire_upload_permit()` pattern prevents unbounded memory
growth when xorbs are produced faster than uploaded. crab uses
semaphore-based concurrency limiting, which achieves a similar effect but
at a coarser granularity.

**Action:** Evaluate whether crab's current approach is sufficient or
whether per-upload permits would improve memory behavior under load.

### 16.7 Non-Blocking Progress Reporting (Low Priority)

The `AtomicU64` + `try_lock` pattern for progress reporting is elegant
and avoids contention. crab's progress module could adopt this for the
LFS transfer agent path.

### 16.8 File Verification Entries in Shards (High Priority)

xet-core's `MDBFileInfo` includes a `verification` field: a
`Vec<FileVerificationEntry>` with one entry per segment. Each entry
contains a range hash computed from the chunk hashes in that segment:

```rust
let range_hash = range_hash_from_chunks(&chunk_hashes_in_segment);
FileVerificationEntry::new(range_hash)
```

This enables per-segment integrity verification during file reconstruction.
If a single xorb is corrupted, the verification entry pinpoints which
segment is affected without re-downloading the entire file.

**crab's gap:** In `push.rs` step 8 (`build_shard`), crab constructs
`MDBFileInfo` with `verification: vec![]` — an empty verification list.
This means:

- Downloaded files cannot be integrity-checked per-segment.
- `crab fsck` cannot verify reconstruction correctness without
  downloading and re-chunking the entire file.
- Xet-compatible tools that rely on verification entries will skip
  integrity checks for crab-produced shards.

**Action:** In `build_shard`, compute `FileVerificationEntry` for each
segment using `range_hash_from_chunks()` from `xet_core_structures`.
This requires access to the chunk hashes for each segment, which are
already available from the staging area's `chunks_for_file()`.

### 16.9 SHA-256 and Metadata Extension in Shards (High Priority)

xet-core's `MDBFileInfo` includes an optional `metadata_ext` field
containing the file's SHA-256 hash. This is critical for LFS compatibility:
the LFS OID is the SHA-256 of the file content, and the metadata_ext
allows verifying that a reconstructed file matches its LFS OID without
re-hashing the entire file.

**crab's gap:** In `push.rs` step 8, crab constructs `MDBFileInfo`
with `metadata_ext: None`. The `FileDataSequenceHeader` is created with
`has_metadata_ext: false`.

**Consequences:**

- LFS OID verification after download requires re-hashing the entire
  reconstructed file (expensive for multi-GB files).
- Xet-compatible tools that use metadata_ext for fast verification will
  fall back to full re-hash or skip verification entirely.
- The `Sha256Policy::Provided` optimization (passing the LFS OID directly
  to avoid redundant hashing) cannot be used.

**Action:** During the clean phase, compute SHA-256 alongside the Blake3
file hash. Store it in the staging area's file metadata. In `build_shard`,
populate `metadata_ext` with `FileMetadataExt::new(sha256)` and set
`has_metadata_ext: true` in the header.

For the LFS clean path (where the SHA-256 is already known as the LFS OID),
pass it through directly — this is the `Sha256Policy::Provided` pattern
from xet-core.

-----

## 17. What crab Already Does Better

### 17.1 No Server Dependency

crab works against raw object storage (S3/GCS/Azure) with no server
component. xet-core requires the HF Hub CAS server for uploads, downloads,
global dedup queries, and token management. crab's serverless model is
simpler to deploy and operate.

### 17.2 Native Git Integration

crab acts as both remote helper and filter driver, eliminating the
dependency on git-lfs. This means:

- One binary instead of two (git-lfs + git-xet).
- No LFS Batch API negotiation overhead.
- Direct control over the clean/smudge pipeline.
- Custom URL scheme (`crab://`) for seamless remote configuration.

### 17.3 Lazy Checkout and FUSE Mount

crab supports lazy checkout (pointer-only), selective hydration, and
FUSE-based on-demand materialization. xet-core has no equivalent — it
relies on git-lfs's `--skip-smudge` for lazy checkout, which is less
integrated.

### 17.4 Distributed Locking and CAS Consistency

crab implements its own distributed locking (push locks with TTL leases)
and compare-and-swap consistency for manifest updates. xet-core delegates
this to the HF Hub server.

### 17.5 GC and Orphan Cleanup

crab has a garbage collector that identifies and removes unreferenced
xorbs and shards, respecting a grace period. xet-core relies on server-side
GC.

### 17.6 Content-Defined Xorb Boundaries

crab's `CdcXorbBuilder` uses a **rolling polynomial hash over the
chunk-hash sequence** to determine xorb boundaries, rather than xet-core's
fixed-size threshold. The algorithm:

1. After each chunk, update `rolling_hash = hash * PRIME + chunk_hash[0]`.
2. If `rolling_hash & XORB_MASK == 0` AND size ≥ `XORB_MIN_SIZE` (16 MiB),
   cut a boundary.
3. Force a boundary at `XORB_MAX_SIZE` (128 MiB) regardless.

This produces **insertion-stable xorb boundaries**: inserting or deleting
a chunk in the middle of a file only perturbs boundaries locally. After a
few chunks, the rolling hash resynchronizes and subsequent xorb boundaries
are identical to the original. This means cross-push xorb-level dedup is
possible — if two pushes share a long run of identical chunks, they'll
produce identical xorbs even if earlier chunks differ.

xet-core's fixed-threshold approach (`cut at 64 MiB`) shifts all subsequent
xorb boundaries when any chunk is inserted or deleted. This is simpler but
means xorb-level dedup across pushes is unlikely.

crab's CDC xorb boundaries are a genuine architectural advantage for
storage efficiency in repositories with incremental updates.

### 17.7 Staging Area with Segment Files

crab's `StagingArea` uses append-only segment files with a SQLite WAL-mode
index for chunk storage during the clean phase. This is more sophisticated
than xet-core's approach (which stages chunks in memory during the
`FileUploadSession` and uploads them directly). crab's staging enables:

- Crash recovery: staged chunks survive process crashes.
- Deferred upload: chunks are staged at `git add` time, uploaded at
  `git push` time.
- Compaction: old segments can be compacted to reclaim space.

xet-core doesn't have this separation because it uploads during the clean
phase itself (the `FileUploadSession` is both the clean handler and the
uploader). crab's two-phase approach (stage locally, upload later) is
better for offline workflows and unreliable networks.

-----

## 18. Recommended Action Items

Ordered by impact and effort:

| # | Item                              | Priority | Effort | Section |
|---|-----------------------------------|----------|--------|---------|
| 1 | Defragmentation prevention        | High     | Medium | §9, §16.1 |
| 2 | Session resume via staged metadata| High     | Medium | §12, §16.2 |
| 3 | File verification entries in shards| High    | Low    | §16.8 |
| 4 | SHA-256 / metadata_ext in shards  | High     | Low    | §16.9 |
| 5 | LFS agent protocol hardening      | Medium   | Low    | §4, §16.3 |
| 6 | Multi-strategy auth cascade       | Medium   | Medium | §5, §16.4 |
| 7 | Cross-file xorb merging evaluation| Low      | Low    | §10, §16.5 |
| 8 | Upload backpressure evaluation    | Low      | Low    | §16.6 |
| 9 | Non-blocking progress reporting   | Low      | Low    | §13, §16.7 |

Items 1-4 address correctness and compatibility gaps. Items 1-2 will
manifest as performance problems at scale. Items 3-4 are required for
full Xet format compatibility — without verification entries, downloaded
files cannot be integrity-checked per-segment, and without SHA-256
metadata, LFS OID verification is impossible.

Items 5-6 improve HF Hub interoperability. Items 7-9 are optimization
and polish.

-----

*This document is based on analysis of xet-core at git-xet v0.2.1 and
crab at the current HEAD. Source paths reference the public upstream
xet-core repository and the Crab repository.
Last updated after cross-validation of both codebases.*
