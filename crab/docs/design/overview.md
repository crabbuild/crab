# crab — Complete Workflow Overview

**How crab turns standard Git commands into serverless, chunk-deduplicated
operations on object storage.**

-----

## Document Metadata

| Field        | Value                                                    |
|--------------|----------------------------------------------------------|
| Project      | crab                                                   |
| Scope        | End-to-end workflow, rationale, and future enhancement map|
| Status       | Living document                                          |
| Companion to | `Crab.md` (architecture), `Crab-xet.md` (dedup/UX)   |
| Version      | 0.1                                                      |

-----

## Table of Contents

1. [Purpose of This Document](#1-purpose-of-this-document)
2. [The Core Idea](#2-the-core-idea)
3. [How crab Plugs Into Git](#3-how-crab-plugs-into-git)
4. [One-Time Setup](#4-one-time-setup)
5. [The Standard Workflow, Step by Step](#5-the-standard-workflow-step-by-step)
6. [Data Flow Diagrams](#6-data-flow-diagrams)
7. [The Three-Tier Storage Model](#7-the-three-tier-storage-model)
8. [Concurrency and Consistency](#8-concurrency-and-consistency)
9. [Caching Architecture](#9-caching-architecture)
10. [Failure Modes and Recovery](#10-failure-modes-and-recovery)
11. [Operational Commands](#11-operational-commands)
12. [Current Implementation Status](#12-current-implementation-status)
13. [Design Rationale and Tradeoffs](#13-design-rationale-and-tradeoffs)
14. [Future Enhancement Areas](#14-future-enhancement-areas)

-----

## 1. Purpose of This Document

The main design doc (`Crab.md`) covers architecture, formats, and protocols
in depth. The Xet integration doc (`Crab-xet.md`) focuses on the dedup
layer and UX transparency. This document fills a different gap: it walks
through the **complete user-facing workflow** from first setup to daily use,
explains **what happens at each step and why**, and identifies **where future
enhancements should plug in**.

Use this document to:

- Onboard new contributors who need the "big picture" before diving into code.
- Evaluate whether a proposed change breaks the workflow invariants.
- Identify the right extension point for a new feature.

-----

## 2. The Core Idea

crab is a single Rust binary that acts as both a **Git remote helper** and
a **Git filter driver**. Users run standard `git` commands; crab intercepts
two specific operations:

1. **Large-file content** — intercepted by the filter driver at `git add` and
   `git checkout` time. Files are chunked, deduplicated, and stored as compact
   pointer blobs in Git's object database. The actual bytes live in object
   storage (S3/GCS/Azure).

2. **Remote transport** — intercepted by the remote helper at `git push` and
   `git fetch`/`git clone` time. Git packs, xorbs (chunk aggregates), shards
   (reconstruction metadata), and refs are read from and written to object
   storage.

Everything else — `git commit`, `git log`, `git diff`, `git branch`, `git
stash`, `git blame` — works natively on the pointer blobs and small files
without crab involvement.

### Why This Architecture

| Alternative              | Problem                                                  |
|--------------------------|----------------------------------------------------------|
| Git-LFS                  | Stores full file copies per version; no chunk-level dedup|
| Custom VCS               | Users must learn new commands; no ecosystem compatibility|
| Server-based Git hosting | Requires infrastructure to operate                       |
| Object-store-only (no Git) | Loses versioning, branching, merge, diff               |

crab combines Git's versioning model with content-defined chunking and
serverless storage. The result: standard Git UX, chunk-level dedup (10-30x
storage savings on evolving large files), and zero servers to manage.

-----

## 3. How crab Plugs Into Git

Git provides two extension mechanisms that crab uses. Understanding these
is essential for understanding every subsequent section.

### 3.1 Remote Helper (`git-remote-crab`)

When Git encounters a URL with a custom scheme like `crab://bucket/repo`,
it searches `$PATH` for an executable named `git-remote-crab` and spawns
it as a subprocess. Communication happens over stdin/stdout using a
line-oriented protocol.

The crab binary detects this mode via `argv[0]`:

```
argv[0] == "git-remote-crab"  →  remote helper mode
argv[0] == "crab"             →  CLI mode
```

Both modes are the same binary. A symlink or hardlink named
`git-remote-crab` pointing to `crab` is all that's needed.

The remote helper advertises three capabilities to Git:

- `fetch` — download objects from the remote
- `push` — upload objects to the remote
- `option` — accept configuration hints from Git

Git then sends commands like `list`, `fetch {sha} {ref}`, and
`push {src}:{dst}` which the helper processes and responds to.

**Source:** `crab/src/git/remote_helper.rs`, `crab/src/main.rs`

### 3.2 Filter Driver (long-running filter process)

Git's filter driver mechanism allows an external process to transform file
content on two occasions:

- **Clean** (working tree → Git ODB): runs at `git add` time
- **Smudge** (Git ODB → working tree): runs at `git checkout` time

crab uses the **long-running filter protocol v2**, where a single persistent
process handles all clean/smudge operations in a session. This avoids the
overhead of spawning a new process per file.

The filter is registered in `.git/config`:

```ini
[filter "crab"]
    process = git-remote-crab filter-process
    required = true
```

And activated per file pattern in `.gitattributes`:

```
*.safetensors filter=crab
*.bin filter=crab
*.parquet filter=crab
```

**Source:** `crab/src/git/filter_process.rs`, `crab/src/git/clean.rs`,
`crab/src/git/smudge.rs`

### 3.3 What Git Handles Natively (No crab Involvement)

| Operation                | Mechanism       | crab involved? |
|--------------------------|-----------------|------------------|
| `git commit`             | Git ODB write   | No               |
| `git log`                | Pack traversal  | No               |
| `git diff`               | Blob comparison | No (compares pointer blobs) |
| `git branch` / `git tag` | Ref management  | No (local refs)  |
| `git merge`              | Three-way merge | No               |
| `git stash`              | Ref + commit    | No               |
| `git blame`              | Annotation walk | No               |

This is by design. crab only intercepts the two operations where it adds
value (large-file handling and remote transport). Everything else is
unmodified Git.

-----

## 4. One-Time Setup

### 4.1 `crab init <url>`

Creates a new repository in object storage and configures the local Git
repository to use it.

**What it does on the remote (S3):**

```
s3://{bucket}/{repo-path}/
├── config              ← repo settings JSON (version, chunk threshold, etc.)
├── HEAD                ← "ref: refs/heads/main\n"
├── refs/               ← empty directory (refs created on first push)
├── pack-list           ← empty JSON manifest
└── xet/
    └── shard-list      ← empty JSON manifest
```

**What it does locally:**

1. Writes `.git/config` entries for the remote and filter driver
2. Creates `.crab/` directory for local staging and cache

**Rationale:** The remote skeleton must exist before any push. The `config`
object establishes the repo's format version and settings. The empty manifests
(`pack-list`, `shard-list`) are needed so the first push's CAS loop has
something to read-modify-write against.

**Source:** `crab/src/cmd/init.rs`

### 4.2 `crab track <glob>`

Tells Git which files should be processed by the crab filter driver.

**What it does:**

1. Appends `<glob> filter=crab diff=crab merge=crab -text` to
   `.gitattributes`
2. Optionally runs `git add --renormalize .` to apply the filter to
   already-tracked files

**Rationale:** Git's filter mechanism is opt-in per file pattern. Without
`.gitattributes` entries, large files would be stored as raw blobs in Git
packs — no chunking, no dedup, no object-storage offloading. The `track`
command is the user-friendly wrapper that writes the correct `.gitattributes`
syntax.

**Source:** `crab/src/cmd/track.rs`

### 4.3 `crab untrack <glob>`

Removes a glob pattern from `.gitattributes`. Files already committed as
pointers remain as pointers until re-added without the filter.

**Source:** `crab/src/cmd/track.rs`

-----

## 5. The Standard Workflow, Step by Step

After one-time setup, the daily workflow uses only standard Git commands:

```
git add .           # clean filter intercepts large files
git commit -m "..."  # normal Git commit (pointer blobs in ODB)
git push            # remote helper uploads to S3
git pull            # remote helper fetches + smudge restores files
git checkout <rev>  # smudge materializes large files on demand
```

### 5.1 `git add` — The Clean Path

When a user runs `git add model.safetensors`, Git checks `.gitattributes`,
sees the `filter=crab` attribute, and sends the file content to the
long-running filter process with a `clean` command.

**Step-by-step data flow:**

```
model.safetensors (10 GB on disk)
    │
    ▼  Git sends to filter-process stdin ("clean" command)
    │
    ├─ 1. Stream through blake3 hasher → file_hash (32 bytes)
    │
    ├─ 2. Content-defined chunking via gearhash (CDC)
    │     Target: 64 KiB chunks (range: 8 KiB – 128 KiB)
    │     Boundaries determined by content, not position
    │
    ├─ 3. Classify each chunk (3-tier dedup):
    │     A (Existing): already in remote storage (ChunkIndex hit)
    │     B (Staged):   already seen this session (local staging hit)
    │     C (New):      needs to be staged locally
    │
    ├─ 4. Stage class-C chunks to local disk:
    │     .crab/staging/segments/current.seg  (append-only segment file)
    │     .crab/staging/index.db              (SQLite WAL-mode index)
    │
    └─ 5. Emit pointer blob to stdout:
          version https://crab.dev/spec/v1
          file-hash 7c1f2a3b...  (64 hex chars, blake3)
          size 10737418240
          shard-hint a1b2c3d4...  (optional, for smudge fast path)
```

**What Git stores:** The ~200-byte pointer blob goes into Git's object
database. The 10 GB file content stays in the working tree and in the local
staging area — it is NOT stored in Git's ODB.

**Key properties:**

- **No network I/O.** The clean path is entirely local. Chunks are staged to
  disk; upload happens later during `git push`.
- **Streaming.** The file is never fully buffered in memory. The chunker
  processes data as it arrives from Git's stdin pipe.
- **Deterministic.** The same file content always produces the same pointer
  blob (same file_hash, same size). This means `git diff` on pointer blobs
  correctly identifies unchanged files.
- **Fast path.** For files already known to the file-index (bloom filter
  check → HEAD request confirms), the clean path skips staging entirely and
  emits the pointer directly with a shard-hint.

**Small files pass through unchanged.** Files below the chunk threshold
(default 1 MiB) are not processed by the filter — Git handles them natively
in its pack format, where delta compression works well for text and small
binaries.

**Source:** `crab/src/git/clean.rs`, `crates/crab-xet/src/chunker.rs`,
`crab/src/engine/dedup.rs`, `crab/src/engine/staging/`

### 5.2 `git commit` — No crab Involvement

`git commit` is completely standard. Git commits the pointer blobs (and any
small files) into its object database as a new commit object. crab does
nothing here.

The commit's tree contains pointer blobs where large files used to be. These
pointer blobs are tiny (~200 bytes), so they compress well in Git's pack
format and don't bloat the repository.

### 5.3 `git push` — The Push Pipeline

When the user runs `git push`, Git spawns `git-remote-crab` and
communicates via the remote helper protocol:

```
git → helper:  capabilities
helper → git:  fetch\npush\noption\n\n

git → helper:  list for-push
helper:        reads refs/ from S3, returns current remote state

git → helper:  push refs/heads/main:refs/heads/main
               (blank line terminates batch)
```

The helper then executes the **14-step push pipeline**:

```
┌─────────────────────────────────────────────────────────────────┐
│                    PUSH PIPELINE (14 steps)                     │
│                                                                 │
│  ┌─ CLASSIFY PHASE (steps 1–4) ──────────────────────────────┐  │
│  │  1. Enumerate pointer blobs via gix-traverse              │  │
│  │  2. Staging/file-index lookup per pointer                 │  │
│  │  3. Pre-push shard sync → refresh ChunkIndex              │  │
│  │  4. Classify chunks: A (existing), B (staged), C (new)    │  │
│  └───────────────────────────────────────────────────────────┘  │
│                                                                 │
│  ┌─ PACK PHASE (steps 5–6) ──────────────────────────────────┐  │
│  │  5. Xorb packer: group class-C chunks into ~64 MiB xorbs  │  │
│  │  6. HEAD check for resume: skip already-uploaded xorbs    │  │
│  └───────────────────────────────────────────────────────────┘  │
│                                                                 │
│  ┌─ UPLOAD PHASE (steps 7–10) ───────────────────────────────┐  │
│  │  7. Parallel xorb uploads (up to 16 concurrent)           │  │
│  │  8. Build shard (reconstruction metadata)                 │  │
│  │  9. Upload shard + file-index entries                     │  │
│  │  10. Upload Git pack (.pack + .idx)                       │  │
│  └───────────────────────────────────────────────────────────┘  │
│                                                                 │
│  ┌─ COMMIT PHASE (steps 11–14) ──────────────────────────────┐  │
│  │  11. CAS update pack-list and shard-list manifests        │  │
│  │  12. Per-ref lock → CAS update → release                  │  │
│  │  13. Post-success cleanup (staging → cache, shard install)│  │
│  │  14. On failure: staging/ChunkIndex unchanged (no-op)     │  │
│  └───────────────────────────────────────────────────────────┘  │
│                                                                 │
│  INVARIANT: All immutable data is durable before any ref moves  │
└─────────────────────────────────────────────────────────────────┘
```

**Critical ordering invariant:** Steps 7–10 (immutable data uploads) must
complete before steps 11–12 (mutable manifest/ref updates). This ensures the
"fail forward" property: an interrupted push may leave orphaned immutable
data (cleaned up by GC) but never creates dangling references (refs pointing
to missing data).

**Concurrency control:**

- Push locks (short-TTL leases in S3) serialize concurrent pushes with
  overlapping destination refs. A full ref maps to `locks/{full_ref}/lock`, so
  `refs/heads/main` maps to `locks/refs/heads/main/lock`. See
  [Object Storage Layout V2](../architecture/object-storage-layout.md#lock-namespaces)
  for the normative key and hard-cutover rules.
- Manifest updates (pack-list, shard-list) use compare-and-swap (CAS) loops:
  read current value + ETag → mutate → conditional PUT with `If-Match`. On
  conflict (HTTP 412), retry with jittered backoff up to 10 attempts.
- Ref updates use the same CAS pattern: read current SHA + ETag → verify
  fast-forward → conditional PUT.

**Source:** `crab/src/git/push.rs`, `crates/crab-coordination/src/push_lock.rs`,
`crates/crab-storage/src/cas.rs`, `crab/src/git/push_manifest.rs`

### 5.4 `git clone` / `git pull` / `git fetch` — The Fetch Path

When Git needs to fetch objects from the remote, it spawns the remote helper:

```
git → helper:  list
helper:        GET HEAD, list refs/ from S3
               returns ref SHAs to Git

git → helper:  fetch {sha} refs/heads/main
               (blank line)

helper:
  1. GET pack-list from S3
  2. Diff against local .git/objects/pack/
  3. Download missing packs in parallel (with SHA1 verification)
  4. Opportunistic shard sync in background (warm the ChunkIndex)
  5. Write packs to .git/objects/pack/

helper → git:  (blank line, signaling completion)
```

Git then updates local refs and checks out the working tree, which triggers
the smudge filter for any changed pointer files.

**Source:** `crab/src/git/fetch.rs`, `crab/src/git/remote_helper.rs`

### 5.5 `git checkout` — The Smudge Path

When Git checks out a file that has `filter=crab` in `.gitattributes`, it
sends the pointer blob content to the filter process with a `smudge` command.

**Step-by-step reconstruction:**

```
Pointer blob (from Git ODB):
  version https://crab.dev/spec/v1
  file-hash 7c1f2a3b...
  size 10737418240
    │
    ▼  Filter process receives "smudge" command
    │
    ├─ 1. Parse pointer → (file_hash, size, optional shard_hint)
    │
    ├─ 2. Resolve file-index:
    │     GET xet/file-index/{hash[:2]}/{hash} from S3
    │     → shard_hash (which shard describes this file)
    │     Fast path: shard-hint in pointer skips the file-index lookup
    │
    ├─ 3. Load shard (local cache or S3):
    │     GET xet/shards/{hash[:2]}/{hash}
    │     → reconstruction terms: [(xorb_hash, offset, length), ...]
    │
    ├─ 4. Coalesce byte ranges (COALESCE_GAP = 5 chunks):
    │     Merge nearby ranges within the same xorb to minimize
    │     the number of Range GET requests
    │
    ├─ 5. Parallel Range GETs on xorbs (up to 16 concurrent):
    │     GET xet/xorbs/{hash[:2]}/{hash}
    │     Range: bytes=start-end
    │
    ├─ 6. Decompress chunks (zstd), verify blake3 hash per chunk
    │     Smudge gate: hold output until final chunk is verified
    │
    └─ 7. Stream reconstructed file to stdout → Git writes to working tree
```

**Performance characteristics:**

- Cold clone (no cache): latency-bound by xorb downloads from S3
- Warm checkout (cached): near-instant, chunks served from local cache
- Parallel fetches: up to 16 concurrent Range GETs
- Range coalescing: reduces round trips by merging nearby byte ranges

**The 100 TB problem:** For very large repositories (100 TB+), eagerly
smudging every file on checkout is impractical — it could take hours or days.
This motivates the lazy checkout and explicit hydration model described in
§5.6.

**Source:** `crab/src/git/smudge.rs`, `crab/src/engine/pointer.rs`

### 5.6 Lazy Checkout and Explicit Hydration

#### The Problem

At 100 TB scale, the eager smudge model breaks down:

- A full checkout would download 100 TB over the network
- Users typically need only a fraction of files at any given time
- Switching branches triggers smudge for every changed pointer file
- CI/CD pipelines may only need specific subdirectories

Git-LFS solves this with `git lfs pull --include="*.bin"` and
`git lfs fetch --include/--exclude` patterns. crab should offer a
similar but improved experience.

#### The Solution: Three-Layer Approach

crab supports three progressively deeper levels of file materialization:

```
Level 0: Pointer-only checkout (instant)
    Working tree contains pointer files — readable as text, not usable as data.
    Enabled by: git config filter.crab.smudge "crab smudge --lazy"
    or: crab config set checkout.lazy true

Level 1: Selective hydration via glob (user-driven)
    User explicitly materializes files matching patterns.
    crab hydrate "models/*.safetensors"
    crab hydrate --include="data/" --exclude="data/archive/"

Level 2: On-demand hydration via FUSE (transparent)
    crab mount provides a read-only FUSE filesystem.
    Files are materialized on first read() — zero upfront cost.
    crab mount ./worktree
```

#### Level 0: Lazy Smudge (Pointer-Only Checkout)

When lazy mode is enabled, the smudge filter returns the pointer blob
unchanged instead of reconstructing the file. The working tree contains
small pointer files that are valid text but not usable as model weights
or datasets.

**How it works:**

```
git checkout main
    │
    ▼  smudge filter receives pointer blob
    │
    ├─ Lazy mode ON?
    │   YES → return pointer blob unchanged (instant)
    │   NO  → full reconstruction (current behavior)
```

**Configuration:**

```ini
# Per-repo: .crab/config.toml
[checkout]
lazy = true

# Or via CLI:
crab config set checkout.lazy true
```

When `checkout.lazy = true`, the filter process detects the mode and
short-circuits: it writes the pointer bytes directly to stdout without
any network I/O. This makes `git checkout`, `git switch`, and `git clone`
effectively instant regardless of repo size.

**Detecting pointer files in the working tree:**

A file is a pointer if it starts with `version https://crab.dev/spec/v1`.
The `crab status` command (future) can report which files are pointers
vs. hydrated.

#### Level 1: `crab hydrate` — Selective File Materialization

After a lazy checkout, users explicitly materialize the files they need:

```bash
# Hydrate specific files
crab hydrate model.safetensors

# Hydrate by glob pattern (like git-lfs pull --include)
crab hydrate "*.safetensors"
crab hydrate "models/**"

# Include/exclude patterns (like git-lfs)
crab hydrate --include="data/" --exclude="data/archive/"

# Hydrate everything (equivalent to eager checkout)
crab hydrate --all

# Dehydrate: replace materialized files with pointers
# (frees local disk space, data remains on S3)
crab dehydrate "data/old-experiments/**"
```

**Data flow for `crab hydrate "*.safetensors"`:**

```
1. Walk working tree, find files matching glob
2. For each file that is a pointer (not already hydrated):
   a. Parse pointer → (file_hash, size, shard_hint)
   b. Resolve file-index → shard_hash
   c. Load shard → reconstruction terms
   d. Coalesce ranges across all files in the batch
   e. Parallel Range GETs on xorbs
   f. Decompress, verify, write to working tree
3. Report: "Hydrated 47 files (12.3 GB) in 2m14s"
```

**Key design decisions:**

- **Glob syntax matches `.gitattributes` patterns.** Users already know
  these patterns from `crab track`. No new syntax to learn.
- **Batch coalescing across files.** When hydrating many files, ranges
  within the same xorb are coalesced into fewer Range GETs. This is the
  same optimization the smudge pipeline already implements via
  `SmudgeQueue` and `COALESCE_GAP`.
- **Progress reporting.** Long hydrations show per-file progress bars
  with ETA, bytes downloaded, and dedup savings.
- **Resumable.** If hydration is interrupted (Ctrl-C, network failure),
  re-running the same command skips already-hydrated files (detected by
  comparing file size against the pointer's `size` field).
- **Include/exclude composability.** Multiple `--include` and `--exclude`
  flags compose like Git-LFS: includes are evaluated first, then excludes
  subtract from the result.

**Persistent include/exclude configuration:**

Like Git-LFS's `lfs.fetchinclude` and `lfs.fetchexclude`, crab supports
persistent patterns so users don't have to specify them on every command:

```ini
# .crab/config.toml
[hydrate]
include = ["models/**", "data/current/**"]
exclude = ["data/archive/**"]
auto = true  # auto-hydrate matching files on checkout
```

When `hydrate.auto = true`, the smudge filter checks each file against
the include/exclude patterns. Matching files are smudged eagerly; non-matching
files get the lazy pointer treatment. This gives users the best of both
worlds: instant checkout for the repo as a whole, automatic materialization
for the subset they care about.

#### Level 2: FUSE Mount (On-Demand, Transparent)

For the most seamless experience, `crab mount` provides a read-only FUSE
filesystem where files are materialized on first `read()`:

```bash
crab mount ./worktree
# Files appear as full-size in ls -la but are fetched on first access
cat worktree/model.safetensors  # triggers download
```

This is the most transparent option — no explicit hydration needed — but
requires FUSE support on the OS and has higher per-access latency than
pre-hydrated files.

**Source (existing infrastructure):** `crates/crab-vfs/src/lib.rs`,
`crab/src/git/smudge.rs` (`SmudgeQueue`, `FileIndexResolver`,
`XorbFetcher` traits)

#### Comparison with Git-LFS

| Feature                    | Git-LFS                          | crab (proposed)                    |
|----------------------------|----------------------------------|------------------------------------|
| Lazy checkout              | `git lfs install --skip-smudge`  | `checkout.lazy = true`             |
| Selective download         | `git lfs pull -I "*.bin"`        | `crab hydrate "*.bin"`             |
| Include/exclude            | `lfs.fetchinclude/fetchexclude`  | `hydrate.include/exclude`          |
| Auto-hydrate subset        | No                               | `hydrate.auto = true` + patterns   |
| Dehydrate (free disk)      | `git lfs dehydrate` (limited)    | `crab dehydrate "glob"`            |
| FUSE mount                 | No                               | `crab mount`                       |
| Chunk-level dedup          | No (full file per version)       | Yes (CDC, 10-30x savings)          |
| Resume interrupted download| No                               | Yes (skip already-hydrated files)  |
| Cross-file range coalescing| No                               | Yes (batch xorb Range GETs)        |

#### UX Flow for a 100 TB Repo

```bash
# Clone is instant — only downloads Git packs (commits, trees, pointers)
git clone crab://bucket/huge-repo
cd huge-repo

# Working tree has pointer files — ls shows them, but they're tiny
ls -la models/
# -rw-r--r--  1 user  staff  186 Apr 21 10:00 gpt4.safetensors
# (186 bytes = pointer, not the 50 GB model)

# Hydrate just what you need
crab hydrate "models/gpt4.safetensors"
# Downloading models/gpt4.safetensors... 50.0 GB [===>    ] 34% 2m12s ETA

# Or set up auto-hydration for your working set
crab config set hydrate.include "models/current/**"
crab config set hydrate.auto true
# Now git checkout automatically hydrates models/current/ files

# Switch branches — instant (only pointers change)
git checkout experiment-branch
# Auto-hydrate kicks in for models/current/ files only

# Free disk space for files you no longer need
crab dehydrate "models/old-experiment/**"
# Replaced 12 files with pointers, freed 340 GB
```

-----

## 6. Data Flow Diagrams

### 6.1 End-to-End: `git add` Through `git push`

```
                        LOCAL                               │    REMOTE (S3)
                                                            │
  Working Tree          Git ODB           Staging           │    Object Storage
  ───────────          ─────────          ─────────         │    ──────────────
                                                            │
  model.safetensors ──► clean filter ──► pointer blob       │
       (10 GB)          │                  (~200 B)         │
                        │                                   │
                        ├─► CDC chunks ──► staging/         │
                        │   (64 KiB ea)    segments/        │
                        │                  index.db         │
                        │                                   │
                        │   git commit                      │
                        │   ──────────                      │
                        │   pointer blob → commit object    │
                        │                                   │
                        │   git push                        │
                        │   ────────                        │
                        │                                   │
                        │   classify chunks (A/B/C)         │
                        │   pack class-C into xorbs         │
                        │                  ─────────────────┼──► xet/xorbs/{hash}
                        │   build shard    ─────────────────┼──► xet/shards/{hash}
                        │   file-index     ─────────────────┼──► xet/file-index/{hash}
                        │   git pack       ─────────────────┼──► packs/pack-{sha}.pack
                        │                                   │
                        │   CAS manifests  ─────────────────┼──► pack-list (CAS)
                        │                  ─────────────────┼──► shard-list (CAS)
                        │   CAS refs       ─────────────────┼──► refs/heads/main (CAS)
                        │                                   │
                        │   cleanup: staging → cache        │
```

### 6.2 End-to-End: `git clone` Through `git checkout`

```
                        LOCAL                               │    REMOTE (S3)
                                                            │
  Working Tree          Git ODB           Cache             │    Object Storage
  ───────────          ─────────          ─────────         │    ──────────────
                                                            │
                        list refs     ◄─────────────────────┼─── HEAD, refs/
                        fetch packs   ◄─────────────────────┼─── pack-list → packs/
                        │                                   │
                        │   git checkout                    │
                        │   ────────────                    │
                        │                                   │
  model.safetensors ◄── smudge filter                       │
       (10 GB)          │                                   │
                        ├─ parse pointer                    │
                        ├─ resolve file-index ◄─────────────┼─── xet/file-index/{hash}
                        ├─ load shard         ◄─────────────┼─── xet/shards/{hash}
                        ├─ coalesce ranges                  │
                        ├─ Range GETs         ◄─────────────┼─── xet/xorbs/{hash}
                        ├─ decompress + verify              │
                        └─ stream to working tree           │
                                              ──► cache/    │
```

-----

## 7. The Three-Tier Storage Model

crab splits content into three tiers by access pattern. Understanding this
split is essential for reasoning about what goes where and why.

### Tier 1: Git Pack Plane

**Contents:** Commits, trees, small blobs, pointer blobs.

**Format:** Standard Git packfiles (`.pack` + `.idx`), identical to what
`git` writes into `.git/objects/pack/`.

**Mutability:** Immutable. Content-addressed by SHA-1.

**Why a separate tier:** Git's delta compression and commit graph algorithms
work well for small, text-shaped content. Packing a 100-byte commit into a
64 MiB xorb would waste space and complicate reconstruction.

**S3 layout:**
```
{repo}/packs/pack-{sha}.pack
{repo}/packs/pack-{sha}.idx
{repo}/pack-list                ← JSON manifest, CAS-updated
```

### Tier 2: Xet Blob Plane

**Contents:** Large file data, stored as content-defined chunks aggregated
into xorbs, with reconstruction metadata in shards.

**Format:** Xet binary format (from `xet-core`). Xorbs are ~64 MiB
content-addressed blobs. Shards describe how to reconstruct files from
chunk ranges within xorbs.

**Mutability:** Immutable. Content-addressed by MerkleHash.

**Why a separate tier:** Large binary content doesn't delta-compress well
with zlib, but massive files often share byte-level substructure across
versions (a training checkpoint changing 5% per epoch). CDC-based chunking
captures this substructure; Git's blob-level dedup cannot.

**S3 layout:**
```
{repo}/xet/xorbs/{hash[:2]}/{hash}         ← chunk aggregates
{repo}/xet/shards/{hash[:2]}/{hash}        ← reconstruction metadata
{repo}/xet/file-index/{hash[:2]}/{hash}    ← file-hash → shard-hash mapping
{repo}/xet/shard-list                      ← JSON manifest, CAS-updated
```

### Tier 3: Metadata Plane

**Contents:** Refs (branch heads, tags), HEAD symref, repo config.

**Format:** Plain text (refs are 40-byte hex SHA strings). Config is JSON.

**Mutability:** Mutable. Updated via S3 conditional writes (CAS).

**Why a separate tier:** Refs must move as commits happen. Separating mutable
metadata from immutable content keeps the mutation surface small and CAS
operations cheap.

**S3 layout:**
```
{repo}/HEAD                     ← "ref: refs/heads/main\n"
{repo}/refs/heads/{branch}      ← 40-byte hex SHA
{repo}/refs/tags/{tag}          ← 40-byte hex SHA
{repo}/config                   ← repo settings JSON
```

### Why Three Tiers, Not Two

Collapsing Tier 1 into Tier 2 (storing commits in xorbs) would mean every
`git log` requires downloading xorbs — unacceptable latency. Collapsing
Tier 2 into Tier 1 (storing large files in Git packs) would mean no
chunk-level dedup — the whole reason crab exists. The three-tier split
gives each data type the storage format optimized for its access pattern.

-----

## 8. Concurrency and Consistency

### 8.1 Design Principle: No Coordinator

crab has no central server, no database, no queue. Coordination is achieved
through two mechanisms:

1. **Content addressing** for immutable data (Tiers 1 and 2). Two clients
   writing the same xorb produce identical bytes at the same path — the
   second PUT is a harmless overwrite.

2. **Compare-and-swap (CAS)** for mutable data (Tier 3). S3's conditional
   write headers (`If-Match`, `If-None-Match`) provide atomic read-modify-write
   semantics per object.

### 8.2 CAS Mechanics

All mutable objects (refs, pack-list, shard-list) are updated via the same
pattern implemented in `crates/crab-storage/src/cas.rs`:

```
loop (up to 10 attempts):
    1. GET current value + ETag
    2. Apply mutation (e.g., append pack SHA to pack-list)
    3. PUT with If-Match: {ETag}
    4. On 412 Precondition Failed → jittered backoff → retry
    5. On success → done
```

This is safe because:
- Mutations are idempotent (set insertions, generation increments)
- The ETag check ensures no lost updates
- Jittered backoff prevents thundering herd

### 8.3 Push Locks

For ref updates specifically, crab adds a short-TTL lease mechanism on top
of CAS. A push lock for `{full_ref}` is a JSON file at
`locks/{full_ref}/lock` containing:

```json
{
  "holder": "machine-uuid-pid",
  "expires_at": 1714000000
}
```

- Created with `PutMode::Create` (fails if lock exists and isn't expired)
- Expired locks are reclaimed by CAS-updating the same object to the new holder
- Release CAS-writes `expires_at: 0` only when the holder still matches
- Default TTL: 5 minutes
- `fsck` marks expired locks released without deleting the lock pointer

Duplicated `locks/refs/refs/...` keys are retired and ignored after the hard
cutover.

This prevents two pushers from racing through the entire 14-step pipeline
only to have one fail at the final ref CAS. Each push locks the sorted,
deduplicated set of destination refs it mutates, saving wasted upload
bandwidth on overlapping pushes.

**Source:** `crates/crab-coordination/src/push_lock.rs`

### 8.4 Concurrent Push Scenario

```
Client A                        S3                       Client B
   │                            │                           │
   │ upload xorbs/packs         │                           │
   │────────────────────────────►                           │
   │                            │         upload xorbs/packs│
   │                            ◄───────────────────────────│
   │                            │                           │
   │ CAS pack-list (etag=E1)    │                           │
   │────────────────────────────►                           │
   │ 200 OK (etag=E2)           │                           │
   │                            │  CAS pack-list (etag=E1)  │
   │                            ◄───────────────────────────│
   │                            │  412 Precondition Failed  │
   │                            │                           │
   │                            │  retry: GET (etag=E2)     │
   │                            ◄───────────────────────────│
   │                            │  CAS pack-list (etag=E2)  │
   │                            ◄───────────────────────────│
   │                            │  200 OK (etag=E3)         │
   │                            │                           │
   │ CAS refs/heads/main        │                           │
   │────────────────────────────►                           │
   │ 200 OK                     │                           │
   │                            │  CAS refs/heads/dev       │
   │                            ◄───────────────────────────│
   │                            │  200 OK                   │
```

Both pushes succeed. Immutable data (xorbs, packs) never conflicts.
Manifests converge via CAS retry. Refs don't conflict because they target
different branches.

-----

## 9. Caching Architecture

crab maintains a local cache hierarchy at `~/.cache/crab/{bucket}/{repo}/`
to minimize redundant network I/O. The chunk cache is unified across all
operations — smudge, hydrate, and FUSE mount share the same cache, so
warming it via any path benefits all others.

### 9.1 Cache Layers

| Layer              | Contents                        | Populated by          | Used by                   |
|--------------------|---------------------------------|-----------------------|---------------------------|
| Chunk cache        | Decompressed chunk data         | Smudge, hydrate, FUSE | Smudge, hydrate, FUSE     |
| Shard cache        | Shard binary files              | Fetch, push           | Clean, smudge             |
| ChunkIndex (SQLite) | chunk-hash → xorb-ref mapping  | Shard install         | Clean (dedup)             |
| File-index cache   | file-hash → shard-hash          | Push, smudge          | Clean fast path           |

The chunk cache is the primary performance lever. It is bounded by
`cache.chunk_bytes` (default 4 GiB) with LRU eviction. In daemon mode,
all mounted repos share a single `ChunkCache` instance, getting cross-repo
dedup for free (content-addressed chunks are identical regardless of which
repo produced them).

### 9.2 Persistent Chunk Index

The `PersistentChunkIndex` is a SQLite database at
`~/.cache/crab/buckets/{bucket-hash}/chunk-index.sqlite`. It maps chunk hashes to
xorb references, enabling the dedup classifier to determine whether a chunk
already exists on the remote without network I/O.

Tables:
- `chunks_v1`: chunk hash (32 bytes) → xorb hash (32 bytes) + chunk index (4 bytes LE) + uncompressed size
- `shards_v1`: shard hash (32 bytes) → presence marker
- `meta_v1`: string key → string value (schema version and cache GC generation)

The index is populated incrementally as shards are installed (during fetch
and after push). SQLite WAL mode gives concurrent readers with a serialized
writer. Schema mismatches or pre-SQLite cache files are discarded and rebuilt
from authoritative remote metadata.

**Source:** `crab/src/metadata/persistent_chunk_index.rs`

### 9.3 Staging Area

The staging area at `.crab/staging/` holds chunks between `git add` and
`git push`. It uses a segment-based layout:

```
.crab/staging/
├── segments/
│   ├── current.seg          ← append-only, active segment
│   └── {id:016x}.seg ...   ← sealed segments
├── index.db                 ← SQLite WAL-mode index
├── lockfile                 ← advisory flock (one process per staging root)
└── push-{uuid}.inflight     ← crash recovery markers
```

Chunks are appended to segment files and indexed in SQLite. The advisory
flock ensures single-writer access. Crash recovery scans for `.inflight`
markers on next operation.

**Source:** `crab/src/engine/staging/`

-----

## 10. Failure Modes and Recovery

### 10.1 Design Principle: Fail Forward, Never Back

An interrupted operation leaves orphaned immutable data (to be
garbage-collected) but never dangling references (refs pointing to missing
data). This is enforced by the push pipeline's ordering: all immutable data
is durable before any ref moves.

### 10.2 Failure Scenarios

| Failure point                    | State after crash                          | Recovery                                    |
|----------------------------------|--------------------------------------------|---------------------------------------------|
| During `git add` (clean)         | Partial chunks in staging                  | Next clean re-stages; staging is append-only|
| During xorb upload (push step 7) | Orphaned xorbs on S3, no ref update        | GC cleans orphans; retry push succeeds      |
| During manifest CAS (step 11)    | Xorbs uploaded, manifests may be stale     | Retry push; CAS loop re-reads and re-applies|
| During ref CAS (step 12)         | Some refs updated, others not              | Push manifest records intent; fsck reconciles|
| During smudge (checkout)         | Partial file in working tree               | `git checkout -- <file>` retries smudge     |
| Process killed (SIGKILL)         | Push lock may be orphaned                  | Lock expires after TTL; next acquire reclaims |

### 10.3 Crash Recovery Scan

On operation start, crab scans for `.inflight` markers in the staging area:

- Stale markers (older than retention period) → clean up
- Live markers → retry pending uploads via HEAD check

This ensures that a crash during push doesn't leave the staging area in an
inconsistent state.

### 10.4 Signal Handling

The binary installs a two-phase signal handler:

1. First SIGINT/SIGTERM → triggers `CancellationToken`, logs "shutting down
   gracefully." Long-running operations (GC, push, fetch) observe the token
   between phases and exit cleanly.
2. Second signal → `std::process::exit(1)` for the case where graceful
   shutdown hangs.

Additionally, `gix-tempfile` signal handlers are registered for
non-cooperative tempfile cleanup — if the process is killed between tempfile
creation and atomic rename, orphan tempfiles are deleted automatically.

**Source:** `crab/src/main.rs` (`spawn_signal_handler`)

-----

## 11. Operational Commands

These commands are crab-specific and used for repository maintenance.
Normal users rarely need them.

| Command                | Purpose                                          | Status        |
|------------------------|--------------------------------------------------|---------------|
| `crab init <url>`    | Create repo skeleton in S3, configure local Git  | Implemented     |
| `crab track <glob>`  | Add file pattern to `.gitattributes`             | Implemented     |
| `crab untrack <glob>`| Remove file pattern from `.gitattributes`        | Implemented     |
| `crab version`       | Print build version, git SHA, timestamp          | Implemented     |
| `crab stat`          | Print staging area statistics                    | Implemented     |
| `crab hydrate <glob>`| Materialize large files matching pattern         | Proposed (§5.6) |
| `crab dehydrate <glob>`| Replace materialized files with pointers       | Proposed (§5.6) |
| `crab gc`            | Garbage collect unreachable objects from S3      | Planned         |
| `crab fsck`          | Check repository integrity                       | Planned         |
| `crab repack`        | Consolidate remote Git pack files                | Stub            |
| `crab mount`         | FUSE mount for on-demand file access             | Proposed (§5.6) |
| `crab cache stats`   | Print local cache statistics                     | Planned         |
| `crab cache clean`   | Clear local cache                                | Planned         |
| `crab staging stats` | Print staging area statistics                    | Planned         |
| `crab staging clean` | Purge stale staging data                         | Planned         |
| `crab errors <code>` | Look up error code explanation                   | Planned         |

-----

## 12. Current Implementation Status

### 12.1 What's Built

| Component                          | Source                              | Notes                                    |
|------------------------------------|-------------------------------------|------------------------------------------|
| Binary dispatch (CLI + remote helper) | `main.rs`                        | argv[0] detection, clap-derive CLI       |
| Remote helper protocol loop        | `git/remote_helper.rs`              | capabilities, list, fetch, push batches  |
| Filter process (v2 long-running)   | `git/filter_process.rs`             | clean/smudge dispatch, session isolation |
| Clean pipeline                     | `git/clean.rs`                      | CDC + blake3 + staging + pointer emit    |
| Smudge pipeline                    | `git/smudge.rs`                     | pointer → shard → Range GETs → reconstruct |
| CDC chunker (gearhash)             | `crates/crab-xet/src/chunker.rs`    | 64 KiB target, SIMD-accelerated          |
| Dedup classifier (A/B/C)           | `engine/dedup.rs`                   | Remote-first precedence invariant        |
| Pointer format                     | `engine/pointer.rs`                 | Parse/serialize, forward-compat          |
| Staging area (segment-based)       | `engine/staging/`                   | SQLite index, advisory flock, recovery   |
| Push pipeline (14-step)            | `git/push.rs`                       | Skeleton with step boundaries            |
| Push lock (TTL lease)              | `coordination/push_lock.rs`         | Acquire/release/reclaim                  |
| CAS update loop                    | `crates/crab-storage/src/cas.rs`    | Jittered backoff, bounded retries        |
| Ref store (S3-backed)              | `metadata/refs.rs`                  | CRUD with CAS semantics                  |
| Fetch pipeline                     | `git/fetch.rs`                      | Pack download, SHA1 verify, shard sync   |
| Push manifest (audit trail)        | `git/push_manifest.rs`              | JSON serialization, object path          |
| Persistent chunk index             | `metadata/persistent_chunk_index.rs`| SQLite-backed warm dedup tier            |
| Xorb format types                  | `crates/crab-xet/src/xorb/format.rs`| MerkleHash, XorbRef, ChunkMeta           |
| URL parser                         | `git/url.rs`                        | `crab://bucket/repo` → components        |
| Error taxonomy                     | `core/error.rs`                     | `CRAB-E####` codes, thiserror            |
| Tracing subscriber                 | `core/tracing_init.rs`              | TTY/JSON auto-detect, CRAB_LOG, OTLP     |
| Signal handler                     | `main.rs`                           | Two-phase SIGINT, CancellationToken      |
| Init command                       | `cmd/init.rs`                       | Remote skeleton + local config           |
| Track/untrack commands             | `cmd/track.rs`                      | .gitattributes management                |

### 12.2 What's Planned (from `crab-ops` spec)

- Config system with TOML overlay (local → user → remote → env)
- Full CLI surface (gc, fsck, repack, cache, staging, errors, sync, bench)
- Garbage collection with prefix-sharded parallel enumeration
- Fsck with gitoxide-based connectivity checks
- Crash recovery (auto-recovery scan, post-ref cleanup)
- Error catalog with long-form explanations
- Integration and fault-injection test suites
- Criterion benchmark suite with CI regression gate

-----

## 13. Design Rationale and Tradeoffs

### 13.1 Why Content-Defined Chunking (CDC)?

Fixed-size chunking (e.g., 64 KiB blocks) is simpler but has a fatal flaw:
inserting or deleting a single byte at the start of a file shifts every
subsequent block boundary, making every chunk "new" and destroying dedup.

CDC uses a rolling hash (gearhash) to find chunk boundaries based on content.
An insertion shifts boundaries only locally around the edit. For a 10 GB
model checkpoint with 5% churn per revision, CDC typically identifies 95% of
chunks as unchanged — a 20x reduction in upload and storage compared to
full-file copies.

**Tradeoff:** CDC is more CPU-intensive than fixed-size chunking. crab
mitigates this with SIMD-accelerated gearhash (AVX2 on x86_64, NEON on
aarch64) and streaming processing (no full-file buffering).

### 13.2 Why Pointer Blobs Instead of Git-LFS Pointers?

Git-LFS uses a similar pointer approach but with a different format and a
server-side component. crab pointers differ in three ways:

1. **blake3 file hash** instead of SHA-256. blake3 is 3-5x faster and
   equally secure. The hash is independent of chunking parameters, so it
   remains stable even if the chunker configuration changes.

2. **Optional shard-hint.** The pointer can include a hint about which shard
   describes this file, enabling the smudge path to skip the file-index
   lookup entirely on cache hits.

3. **No server URL.** LFS pointers contain the server endpoint. crab
   pointers are self-contained — the remote URL comes from `.git/config`,
   not the pointer.

### 13.3 Why Xorbs (~64 MiB Aggregates)?

Storing each chunk (64 KiB) as a separate S3 object would mean millions of
objects per large repo. S3 charges per request and has per-object overhead.
Aggregating chunks into ~64 MiB xorbs reduces:

- Object count by ~1000x
- PUT/GET request count proportionally
- S3 listing time (fewer objects to enumerate)

Chunks within a xorb are individually compressed (zstd) and individually
addressable via Range GETs, so aggregation doesn't sacrifice random access.

**Tradeoff:** A single chunk shared across many files means the containing
xorb can't be deleted until all referencing files are garbage-collected.
This is acceptable because xorbs are immutable and GC operates on the
reachable set.

### 13.4 Why CAS Instead of DynamoDB/Coordination Service?

A coordination service (DynamoDB, Redis, etc.) would provide stronger
transactional guarantees but violates the "zero servers" principle. S3's
conditional writes are sufficient for crab's concurrency model:

- Immutable data never conflicts (content addressing)
- Mutable data is a small set of tiny objects (refs, manifests)
- CAS with retry converges quickly (typically 1-2 attempts)

**Tradeoff:** True atomic multi-ref pushes are impossible without a
coordinator. In practice, most pushes update a single ref. Multi-ref pushes
(main + tag) are handled by the push manifest audit trail and fsck recovery.

### 13.5 Why Segment-Based Staging Instead of Per-Chunk Files?

The v1 staging layout stored each chunk as a separate file. On repos with
millions of chunks, this caused:

- Filesystem inode exhaustion
- Slow directory listings
- Poor I/O performance (many small random writes)

The v2 segment-based layout appends chunks to large segment files and indexes
them in SQLite. This reduces inode usage by ~1000x and converts random writes
to sequential appends.

### 13.6 Why No `git add` Network I/O?

The clean path is intentionally offline. Reasons:

1. **Latency.** `git add` should feel instant. Network round trips to S3
   would add seconds per file.
2. **Offline work.** Users should be able to `git add` and `git commit`
   without network access, then push later.
3. **Atomicity.** If the clean path uploaded chunks, a failed `git add`
   could leave orphaned data on S3 with no commit to reference it.

Chunks are staged locally and uploaded during `git push`, which is already
expected to be a network operation.

### 13.7 Why the 14-Step Push Pipeline?

The push pipeline's step count reflects the number of distinct concerns that
must be sequenced correctly:

- **Steps 1-4 (classify):** Determine what's new. Must complete before
  packing to avoid uploading duplicates.
- **Steps 5-6 (pack + resume):** Group chunks into xorbs and skip
  already-uploaded ones. Must complete before upload.
- **Steps 7-10 (upload):** All immutable data must be durable before any
  mutable state changes.
- **Steps 11-12 (commit):** Manifest and ref updates are the "point of no
  return." After this, the push is visible to other clients.
- **Steps 13-14 (cleanup):** Post-commit housekeeping. Failure here is
  harmless — next operation cleans up.

Collapsing steps would either violate the ordering invariant or make error
handling more complex.

-----

## 14. Future Enhancement Areas

This section identifies where future features should plug into the existing
architecture. Each area includes the relevant extension points in the
codebase.

### 14.0 Bucket Import (shipped)

**Goal:** Onboard an existing object-storage prefix as a Crab-backed
git repo in place — no re-upload, no history rewrite.

`crab import` reads a raw bucket prefix (S3, GCS, Azure, or local
filesystem), detects whether it is flat or versioned, and materializes
a fresh git repo whose history reflects the bucket. Flat buckets produce
a single commit; versioned buckets produce one commit per time window
(default 1 hour), with delete markers surfacing as git deletions.
`--at <rfc3339>` pins the tree to a specific instant; `--since` /
`--until` restrict the history range. Same-bucket imports leave source
objects untouched — xorbs land in the target `.crab/` layout next to
the originals.

See [`crab import`](../guides/crab-import.md) for the full
command reference and recipes. Phases 1–10 of the
The bucket-import design spec is complete.

**Extension points (for the follow-up specs):**

- Incremental sync: `crab sync from-bucket` appends new commits
  based on `last_modified` above the last imported version.
- LFS-format source conversion: read LFS pointers, fetch from the LFS
  store, re-ingest as Crab pointers.
- Streaming `chunk_file`: avoid buffering the full file in RAM for
  10 GiB+ source objects.

### 14.1 Lazy Checkout, Hydration, and FUSE Mount

**Goal:** Support 100 TB+ repos where eager checkout is impractical. See
§5.6 for the full three-layer design (lazy smudge → explicit hydration →
FUSE mount).

**Implementation roadmap:**

1. **Lazy smudge mode** — Lowest effort, highest impact. The filter process
   checks a config flag and short-circuits smudge by returning the pointer
   unchanged. Requires: config system (task 25), one flag check in
   `filter_process.rs`.

2. **`crab hydrate` / `dehydrate` commands** — Walk the working tree,
   match globs, batch-smudge matching pointers. Requires: new `cmd/hydrate.rs`,
   reuse `SmudgeQueue` from `git/smudge.rs` for batch coalescing, glob
   matching via `globset` crate.

3. **Persistent include/exclude with auto-hydrate** — The smudge filter
   checks `hydrate.include` / `hydrate.exclude` patterns from config and
   selectively smudges. Requires: config system, pattern matching in the
   filter dispatch loop.

4. **`crab status`** — Report which files are pointers vs. hydrated.
   Walk working tree, check first line of each tracked file for the version
   header.

5. **FUSE mount** — `crab mount` provides a read-only filesystem where
   files are materialized on first `read()`. Requires: `fuser` crate,
   `crates/crab-vfs/src/` module, reuse `FileIndexResolver` and `XorbFetcher`
   traits from smudge pipeline.

**Extension points:**
- `crates/crab-vfs/src/` — shared FUSE/NFS mount, snapshot, overlay, and hydration logic
- `crab/src/git/smudge.rs` — `SmudgeQueue`, `FileIndexResolver`,
  `XorbFetcher` traits for batch reconstruction
- `crab/src/git/filter_process.rs` — lazy mode flag check in dispatch
- `crab/src/cmd/hydrate.rs` — new hydrate/dehydrate commands
- `Cmd::Hydrate`, `Cmd::Dehydrate`, `Cmd::Mount` variants in `main.rs`

### 14.2 Git LFS Compatibility

**Goal:** Seamlessly read Git-LFS pointers and fetch from LFS servers,
enabling migration from LFS to crab without rewriting history.

**Extension points:**
- `crab/src/lfs/` — new module for LFS transfer agent
- Pointer parser (`engine/pointer.rs`) — detect LFS pointer format
  (`version https://git-lfs.github.com/spec/v1`) and route to LFS code path
- Filter process — dispatch to LFS smudge when LFS pointer detected

### 14.3 Cross-Repo Dedup

**Goal:** Repos sharing a bucket prefix share the dedup scope — xorbs
uploaded by one repo can be referenced by another without re-upload.

**Extension points:**
- `ChunkIndex` — extend to query a shared index across repo prefixes
- Xorb upload (push step 7) — check shared xorb namespace before uploading
- Config system — `xet_prefix` setting to control shared scope

**Rationale:** Content addressing makes this natural. If two repos produce
the same chunk hash, the xorb containing that chunk is identical regardless
of which repo uploaded it. The only change needed is widening the ChunkIndex
lookup scope.

### 14.4 Incremental GC

**Goal:** GC that processes only objects created since the last GC run,
rather than walking the entire object set.

**Extension points:**
- `shard-list` generation counter — GC records the generation it processed
- `pack-list` generation counter — same
- GC sweep — filter candidates to `created_at > last_gc_generation`

### 14.5 Multi-Cloud and Multi-Region

**Goal:** Replicate repository data across cloud providers or regions for
redundancy and latency optimization.

**Extension points:**
- `storage/store.rs` — the `Store` abstraction wraps `object_store`, which
  already supports S3, GCS, Azure, R2, MinIO
- Config system — `[replication]` records a primary remote plus read replicas
- Push pipeline — primary-write only for locks, manifest CAS, GC, repair,
  lifecycle, and tier changes
- Fetch pipeline — read from a ready regional replica with primary fallback

### 14.6 Web UI / API Layer

**Goal:** A read-only web interface for browsing repositories, viewing
commit history, and downloading files.

**Extension points:**
- The S3 bucket layout is self-describing — a web service can read refs,
  pack-list, shard-list, and reconstruct file content using the same logic
  as the smudge pipeline
- No crab binary changes needed — the web layer reads S3 directly

### 14.7 Streaming Push Pipeline

**Goal:** Overlap chunking, classification, packing, and uploading in a
streaming pipeline rather than sequential phases.

**Extension points:**
- Push pipeline step 5 already supports `PackerMode::Streaming` vs
  `PackerMode::SequentialV1`
- `StreamPacker` (referenced in push config) implements the three-stage
  pipeline: classifier → packer → uploader
- `CancellationToken` propagation ensures the streaming pipeline can be
  interrupted cleanly

### 14.8 Smudge Prefetch / Speculative Download

**Goal:** Once a shard is loaded during checkout, prefetch referenced xorbs
in the background while Git iterates over other pointer files.

**Extension points:**
- Filter process session state — track loaded shards and their xorb
  references
- Smudge pipeline — spawn background download tasks for xorbs referenced
  by the current shard but not yet requested
- `delay` capability in filter protocol — defer smudge completion until
  prefetched data arrives

### 14.9 Adaptive Dedup Threshold

**Goal:** Dynamically adjust the dedup threshold (currently fixed at 25%)
based on observed dedup ratios, using EWMA (exponentially weighted moving
average).

**Extension points:**
- `EngineConfig::adaptive_threshold` flag (already defined)
- Dedup classifier — track per-session dedup ratio and adjust threshold
- Config system — persist learned threshold across sessions

### 14.10 Compression Tuning

**Goal:** Per-file-type compression strategy (e.g., skip zstd for
already-compressed formats like `.safetensors`).

**Extension points:**
- Clean pipeline — detect file type from extension or magic bytes
- Xorb packer — per-chunk compression level selection
- Config system — file-type → compression-level mapping

-----

## Appendix A: S3 Bucket Layout Reference

```
s3://{bucket}/{repo-path}/
│
├── config                              repo settings JSON
├── HEAD                                symref: "ref: refs/heads/main\n"
│
├── refs/
│   ├── heads/
│   │   ├── main                        40-byte hex SHA
│   │   └── dev
│   └── tags/
│       └── v1.0
│
├── packs/
│   ├── pack-{sha}.pack                 standard Git packfile
│   ├── pack-{sha}.idx                  pack index
│   └── pack-{sha}.bitmap              (optional)
│
├── pack-list                           JSON manifest (CAS-updated)
│
├── locks/
│   └── refs/{ref}/lock                 push lock (TTL lease)
│
├── push-manifests/
│   └── {uuid}                          audit trail per push
│
└── xet/
    ├── xorbs/{hash[:2]}/{hash}         chunk aggregates (~64 MiB)
    ├── shards/{hash[:2]}/{hash}        reconstruction metadata
    ├── file-index/{hash[:2]}/{hash}    file-hash → shard-hash
    └── shard-list                      JSON manifest (CAS-updated)
```

## Appendix B: Pointer Format Reference

```
version https://crab.dev/spec/v1
file-hash {64-hex-blake3}
size {decimal-bytes}
shard-hint {64-hex-blake3}              (optional)
```

Maximum total size: 256 bytes. Forward-compatible: unknown lines are
tolerated and ignored. Version URL increments on breaking format changes.

## Appendix C: Error Code Ranges

| Range           | Category                |
|-----------------|-------------------------|
| CRAB-E0001–09 | Transient (retry-worthy) |
| CRAB-E0010–19 | Conflict (state-dependent) |
| CRAB-E0020–29 | Data integrity           |
| CRAB-E0030–39 | Permanent (user-facing)  |
| CRAB-E0040–49 | Credentials              |
| CRAB-E0050–59 | Configuration            |
| CRAB-E0060–69 | Protocol/framing         |
| CRAB-E0070–79 | I/O and storage          |
| CRAB-E0080–89 | Internal                 |
| CRAB-E0090–99 | Staging                  |

Every error variant carries its code in the `Display` output so users can
look up remediation via `crab errors <code>`.

## Appendix D: Source Code Map

```
crab/src/
├── main.rs              binary entry: git-remote-crab + crab CLI
├── cmd/                 subcommands
│   ├── init.rs          crab init
│   ├── track.rs         crab track / untrack
│   └── stat.rs          crab stat
├── core/                cross-cutting concerns
│   ├── config.rs        engine configuration (EngineConfig, StagingConfig)
│   ├── context.rs       AppContext (Config + CancellationToken)
│   ├── error.rs         CrabError enum with CRAB-E#### codes
│   ├── metrics.rs       atomic counters for observability
│   └── tracing_init.rs  subscriber setup (TTY/JSON, CRAB_LOG, OTLP)
├── engine/              chunking + dedup
│   ├── chunker.rs       gearhash CDC (64 KiB target)
│   ├── dedup.rs         A/B/C chunk classifier
│   ├── pointer.rs       pointer format parse/serialize
│   └── staging/         segment-based staging area
│       ├── mod.rs        StagingArea (flock + SQLite + segments)
│       ├── index.rs      SQLite WAL-mode chunk index
│       ├── segment.rs    append-only segment files
│       ├── recovery.rs   crash recovery scan
│       └── stats.rs      compaction and clean statistics
├── git/                 Git integration
│   ├── remote_helper.rs remote helper protocol loop
│   ├── filter_process.rs long-running filter v2
│   ├── clean.rs         clean pipeline (chunk + stage + pointer)
│   ├── smudge.rs        smudge pipeline (pointer → reconstruct)
│   ├── push.rs          14-step push pipeline
│   ├── fetch.rs         pack download pipeline
│   ├── push_manifest.rs audit trail for pushes
│   ├── url.rs           crab:// URL parser
│   └── progress.rs      user-facing progress output
├── storage/             object store interaction
│   └── xorb/            crab-side parser/builder adapters
├── crates/crab-xet/
│   └── xorb/format.rs   MerkleHash, XorbRef, ChunkMeta, XorbInfo
├── metadata/            persistent indexes
│   ├── refs.rs          S3-backed ref store (CAS)
│   ├── persistent_chunk_index.rs  SQLite chunk→xorb index
│   └── chunk_index.rs   in-memory chunk index
├── crates/crab-storage/
│   └── cas.rs           JSON CAS update loop (jittered backoff)
├── coordination/        distributed coordination
│   └── push_lock.rs     short-TTL push lease
├── cache/               metadata cache warming
├── vfs/                 FUSE mount (future)
└── lfs/                 Git LFS transfer agent (future)
```
