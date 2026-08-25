# crab — Technical Design

**A truly serverless, cloud-native Git platform optimized for AI/ML workloads.**

-----

## Document Metadata

|Field  |Value                      |
|-------|---------------------------|
|Project|crab                     |
|Status |Design — pre-implementation|
|Authors|Xin                        |
|Version|0.1                        |

-----

## Table of Contents

1. [Vision & Non-Goals](#1-vision--non-goals)
1. [Design Principles](#2-design-principles)
1. [System Overview](#3-system-overview)
1. [Data Model](#4-data-model)
1. [Storage Layout](#5-storage-layout)
1. [Git Integration](#6-git-integration)
1. [Xet Integration](#7-xet-integration)
1. [Metadata & Consistency](#8-metadata--consistency)
1. [Protocol Flows](#9-protocol-flows)
1. [Deduplication Strategy](#10-deduplication-strategy)
1. [Caching Architecture](#11-caching-architecture)
1. [Security Model](#12-security-model)
1. [Garbage Collection](#13-garbage-collection)
1. [Observability](#14-observability)
1. [CLI & Configuration](#15-cli--configuration)
1. [Error Handling & Recovery](#16-error-handling--recovery)
1. [Performance](#17-performance)
1. [Multi-Cloud & Multi-Region](#18-multi-cloud--multi-region)
1. [Compatibility & Migration](#19-compatibility--migration)
1. [Threat Model](#20-threat-model)
1. [Cost Model](#21-cost-model)
1. [Engineering Plan](#22-engineering-plan)
1. [Testing Strategy](#23-testing-strategy)
1. [Open Questions](#24-open-questions)
1. [Glossary](#25-glossary)
1. [References](#26-references)

-----

## 1. Vision & Non-Goals

### 1.1 Vision

crab is a client-side Git platform where users host their own Git repositories directly on object storage (S3, GCS, Azure Blob, R2, MinIO) with zero servers to operate, content-defined chunk-level deduplication for large files, and a standard `git` UX. Users install a single binary and can `git clone crab://bucket/repo` using their existing cloud credentials.

The product is optimized for the AI/ML artifact workload: model checkpoints, datasets, embedding tables, and other large binary files that evolve over many small edits. Git-LFS stores a new copy of the entire file on every revision; crab stores only the changed chunks. At a 10-revision, 100 GB dataset with 5% churn per revision, the storage delta is typically 10-30× lower and transfer time is proportionally faster.

crab combines three open technologies:

- **Git** (unchanged) as the versioning interface.
- **gitoxide** as the pure-Rust implementation of Git’s pack format, object model, and protocol primitives.
- **Xet data model** (from HuggingFace’s xet-core) for content-defined chunking, xorb aggregation, and shard-based reconstruction.

The glue between them — the remote helper binary, the S3 metadata layer, the clean/smudge filters, and the dedup orchestration — is the substance of crab.

### 1.2 Goals

- **Zero server operation.** No compute, no database, no queue managed by crab or by users. Object storage is the only dependency.
- **Standard Git UX.** `git clone`, `git push`, `git pull`, `git checkout`, `git log` work unmodified.
- **Chunk-level deduplication** for files above a configurable threshold.
- **Single binary deployment.** Installable via `brew`, `cargo install`, or a release tarball.
- **Multi-cloud.** Works on any object store supporting conditional writes (S3, GCS, Azure, R2, MinIO).
- **Crash-safe writes.** Any interrupted push leaves the repository in a consistent state.
- **Strong integrity.** Every stored object is content-addressed; corruption is detectable.

### 1.3 Non-Goals

- **Not a GitHub replacement.** No pull requests, issues, code review, or web UI in v1. A separate product layer may sit on top later.
- **No deployed Git protocol server.** The local `git-remote-crab` helper
  temporarily performs the `git-upload-pack` role for protocol-v2 fetches over
  its existing stdio; Crab does not deploy a listener or HTTP smart protocol
  endpoint, and `git-receive-pack` takeover is not part of the profile.
- **Not a general-purpose data platform.** Optimized for Git-shaped workloads; not a replacement for Delta Lake or Iceberg.
- **Not an alternative to LFS for small files.** Small files stay in normal Git packfiles.
- **Not magic cross-organization dedup.** Dedup scope is per-bucket (or per-bucket-prefix) by default. Global dedup requires an optional coordination service.

### 1.4 Scope of This Document

This document covers the v1 design: the client binary, the on-disk formats, the S3 layout, and the protocols. It does not cover the optional coordination services (global dedup, Crab Auth, CI webhooks) beyond noting where they plug in.

-----

## 2. Design Principles

These principles guide every tradeoff in the rest of the document. They’re listed in priority order — when two principles conflict, the earlier one wins.

1. **Correctness over performance.** A slow, correct system is a product; a fast, corrupting system is a lawsuit.
1. **Immutable data, mutable pointers.** Every byte of content is content-addressed and write-once. Only a small set of pointer objects mutate, and those mutations use compare-and-swap.
1. **No coordinator.** No component requires “only one writer at a time.” Coordination is achieved through content addressing (for immutable data) and CAS (for mutable pointers).
1. **Fail forward, never back.** An interrupted operation leaves orphaned immutable data (to be garbage-collected) but never dangling references.
1. **Client-local caches, server-less source of truth.** The object store is authoritative. Client caches are performance optimizations that can be rebuilt at any time.
1. **One protocol, many backends.** The Rust code depends on `object_store`’s abstraction. S3 is the reference; GCS/Azure/R2/MinIO are first-class.
1. **Interoperate with standard Git.** Any `git` binary from the last 5 years must be able to clone, pull, and push against a crab repo with only the crab helper installed.
1. **Reuse before reinvent.** Prefer depending on `gix-*` and `xet-core` crates over rewriting equivalents.

-----

## 3. System Overview

### 3.1 Component Diagram

```
┌───────────────────────────────────────────────────────────────────┐
│                        User's workstation                         │
│                                                                   │
│   ┌─────────────┐                                                 │
│   │   git CLI   │   (unchanged vendor binary)                     │
│   └──────┬──────┘                                                 │
│          │ remote helper protocol (stdio)                         │
│          │ clean/smudge filter protocol (stdio)                   │
│   ┌──────▼──────────────────────────────────────────────────┐     │
│   │                   git-remote-crab                       │     │
│   │                    (Rust binary)                        │     │
│   │                                                         │     │
│   │  ┌───────────────┐  ┌───────────────┐  ┌─────────────┐  │     │
│   │  │ Protocol Loop │  │ Filter Loop   │  │ CLI Subcmds │  │     │
│   │  └───────┬───────┘  └───────┬───────┘  └──────┬──────┘  │     │
│   │          │                  │                 │         │     │
│   │  ┌───────▼──────────────────▼─────────────────▼─────┐   │     │
│   │  │              Orchestrator                        │   │     │
│   │  │  (fetch, push, checkout, stage, gc)              │   │     │
│   │  └───┬──────────────┬───────────────┬──────────┬────┘   │     │
│   │      │              │               │          │        │     │
│   │  ┌───▼───┐      ┌───▼───┐       ┌───▼───┐  ┌───▼───┐    │     │
│   │  │ Pack  │      │  Xet  │       │ Meta  │  │ Cache │    │     │
│   │  │ Plane │      │ Plane │       │ Plane │  │ Plane │    │     │
│   │  └───┬───┘      └───┬───┘       └───┬───┘  └───────┘    │     │
│   │      │              │               │                   │     │
│   │      └──────────────┼───────────────┘                   │     │
│   │                     │                                   │     │
│   │              ┌──────▼──────┐                            │     │
│   │              │object_store │                            │     │
│   │              └──────┬──────┘                            │     │
│   └─────────────────────┼───────────────────────────────────┘     │
│                         │                                         │
│   ┌─────────────────────┴────────────────────────┐                │
│   │  Local cache: ~/.cache/crab/                 │                │
│   │   chunks/  shards/  xorbs/  manifests/       │                │
│   └──────────────────────────────────────────────┘                │
└───────────────────────────┼───────────────────────────────────────┘
                            │ HTTPS + cloud-native auth (SigV4, etc.)
                            ▼
                ┌───────────────────────┐
                │   Object Storage      │
                │   (S3/GCS/Azure/...)  │
                │                       │
                │   packs/ refs/        │
                │   xet/xorbs/          │
                │   xet/shards/         │
                │   pack-list           │
                │   shard-list          │
                └───────────────────────┘
```

### 3.2 Process Lifecycle

A `git-remote-crab` process is short-lived, invoked by `git` per operation:

- `git clone crab://bucket/repo` → `git` spawns `git-remote-crab crab crab://bucket/repo` → the process streams refs and packs, exits.
- `git push` → same pattern, reverse direction.
- `git checkout <rev>` → `git` runs the smudge filter on each changed file that has a filter configured → `git-remote-crab smudge` processes materialize content from xet storage.

There is no long-running daemon. State between invocations lives in the local cache directory and the object store.

### 3.3 High-Level Operation Map

|User Action            |Mechanism          |Talks to S3?                                     |
|-----------------------|-------------------|-------------------------------------------------|
|`git clone`            |Remote helper fetch|Yes: read refs/, packs/, xorbs/, shards/         |
|`git pull`             |Remote helper fetch|Yes: read refs/, packs/, incremental xorbs/shards|
|`git push`             |Remote helper push |Yes: write packs/xorbs/shards + CAS refs         |
|`git checkout`         |Smudge filter      |Yes: read missing xorbs for pointer files        |
|`git add`, `git commit`|Clean filter       |No (just writes pointer to local Git ODB)        |
|`git log`, `git diff`  |Native Git         |No (uses local packs)                            |
|`crab gc`            |CLI subcommand     |Yes: list + delete unreachable objects             |
|`crab stat`          |CLI subcommand     |Yes: list objects, compute size                    |

-----

## 4. Data Model

### 4.1 The Three Tiers

crab’s storage model splits content into three tiers by access pattern:

```
Tier 1: Git Pack Plane         Tier 2: Xet Blob Plane        Tier 3: Metadata Plane
───────────────────────        ────────────────────────       ─────────────────────
commits, trees, small blobs    large files (>1MiB)            refs, HEAD, manifests
immutable packfiles            immutable chunks/xorbs/shards  mutable, CAS-updated
Git's native format            Xet's native format            JSON objects on S3
```

**Why three tiers, not two?**

- **Tier 1 (Git)** is where Git’s delta compression and commit graph algorithms win. Small, text-shaped content deltas well. Packing a 100-byte commit into a 64 MiB xorb would waste space and complicate reconstruction.
- **Tier 2 (Xet)** is where CDC-based chunking wins. Large binary content doesn’t delta well with zlib, but massive files often share byte-level substructure across versions (a training checkpoint changing 5% per epoch).
- **Tier 3 (Metadata)** is mutable by necessity — refs move as commits happen. Separating it keeps mutations confined to a small number of tiny objects where CAS is cheap.

### 4.2 Content Addressing Everywhere

Every object in Tiers 1 and 2 is named by its cryptographic hash:

|Object                        |Hash function                |Where used                    |
|------------------------------|-----------------------------|------------------------------|
|Git objects (commit/tree/blob)|SHA-1 (Git default)          |Tier 1                        |
|Git packs                     |SHA-1 of pack content        |Tier 1 file names             |
|Chunks                        |MerkleHash (Xet)             |Content-addressed within xorbs|
|Xorbs                         |MerkleHash over chunks       |Tier 2 file names             |
|Shards                        |MerkleHash over shard body   |Tier 2 file names             |
|Pointer files                 |File hash (blake3 of content)|Key in file-index             |

Immutable + content-addressed means:

- Idempotent writes: PUT the same object twice is a no-op (or overwrites with identical bytes).
- Trivial integrity: re-hash on read to verify.
- Trivial dedup: equal content yields equal name.
- Trivial concurrency: two clients writing the same content don’t conflict.

### 4.3 Why Not Just Use Git’s Native Large-File Support?

Git’s pack format can technically hold gigabyte blobs but performs poorly for AI artifacts:

- **Delta chains** on binary model weights don’t compress. zlib-deflate on random floats yields near-zero savings.
- **Pack rewrites** during `git gc` or `git repack` require reading the entire pack. On a multi-GB pack this is minutes of I/O.
- **Memory pressure** is substantial. Git mmap’s packs; a 30 GB pack needs 30 GB of virtual address space and forces aggressive page-cache churn.
- **No dedup across files.** Git dedups at the blob-SHA level. Two files that share 80% of chunks but differ in 1 byte are stored as 2 full copies.

Xet’s approach — chunk the file, aggregate chunks into 64 MiB xorbs, describe reconstruction with a shard — solves all four problems simultaneously.

-----

## 5. Storage Layout

### 5.1 Bucket Structure

```
s3://{bucket}/{repo-path}/
│
├── config                              (1) repo config JSON
├── HEAD                                (2) symref target
│
├── refs/                               (3) mutable, CAS-updated
│   ├── heads/
│   │   ├── main
│   │   └── dev
│   ├── tags/
│   │   └── v1.0
│   └── notes/
│
├── packs/                              (4) immutable Git packs
│   ├── pack-{sha}.pack
│   ├── pack-{sha}.idx
│   └── pack-{sha}.bitmap              (optional but recommended)
│
├── pack-list                           (5) pack manifest, CAS-updated
│
└── xet/
    ├── xorbs/                          (6) immutable chunk aggregates
    │   └── {hash[:2]}/{hash}
    │
    ├── shards/                         (7) immutable reconstruction metadata
    │   └── {hash[:2]}/{hash}
    │
    ├── file-index/                     (8) immutable pointers
    │   └── {file-hash[:2]}/{file-hash}
    │
    └── shard-list                      (9) shard manifest, CAS-updated
```

### 5.2 Object Descriptions

**(1) config** — JSON document describing repo-level settings:

```json
{
  "version": 1,
  "created_at": "2026-04-19T00:00:00Z",
  "chunk_threshold_bytes": 1048576,
  "default_branch": "main",
  "xet_enabled": true,
  "hash_algorithm": "sha1",
  "compression": "zstd"
}
```

Changes only at repo creation and on explicit config updates. Small (<1 KiB). CAS-updated if modified.

**(2) HEAD** — One line, identical format to `.git/HEAD`:

```
ref: refs/heads/main
```

**(3) refs/** — One S3 object per ref. Content is 40 bytes (SHA-1 hex) + optional trailing newline. Using one object per ref makes individual CAS cheap and avoids a global ref-lock.

**(4) packs/** — Standard Git pack files, identical format to what `git` writes into `.git/objects/pack/`. No transformation. Allows future migration to/from vanilla Git hosts.

The `.bitmap` file (Git’s reachability bitmap format) is optional but dramatically accelerates clone performance. crab generates bitmaps during push when the pack is large enough to justify the compute.

**(5) pack-list** — JSON manifest enumerating active packs:

```json
{
  "version": 1,
  "generation": 42,
  "packs": [
    {"sha": "abc123...", "size": 1048576, "created_at": "...", "has_bitmap": true},
    {"sha": "def456...", "size": 2097152, "created_at": "...", "has_bitmap": false}
  ]
}
```

The `generation` field is a monotonic counter useful for detecting staleness in caches. Updated via CAS on every push.

**(6) xorbs/** — Binary xorb files, compatible with the Xet xorb format (see xet-core’s xet-core-structures crate). Named by MerkleHash. Immutable. Up to ~64 MiB each. First two hex chars of the hash prefix the path to spread load across S3 partitions.

**(7) shards/** — Binary shard files, Xet format. Describe which chunks are in which xorbs and how to reconstruct specific files from chunk ranges.

**(8) file-index/** — Tiny objects mapping a file hash (what a pointer blob references) to the shard hash that describes that file’s reconstruction. 40-byte content; one per file version. Immutable once written (a file’s content determines its hash, which determines its index entry).

**(9) shard-list** — JSON manifest enumerating active shards:

```json
{
  "version": 1,
  "generation": 17,
  "shards": [
    {"sha": "a1b2...", "size": 4096, "xorb_count": 8, "file_count": 3}
  ]
}
```

CAS-updated on every push that touches xet files.

### 5.3 Path Sharding Strategy

S3’s internal partitioning keys on object key prefix. For write-heavy prefixes, AWS recommends randomizing the first characters of keys. crab uses `{hash[:2]}/{hash}` for content-addressed objects (xorbs, shards, file-index), giving 256 subdirectories and spreading writes across 256 S3 partitions.

Packs are fewer in number (typically dozens to hundreds per repo) and don’t need sharding.

Refs are rare writes; no sharding needed.

### 5.4 Repo Identity

A repo’s identity is `{bucket}/{repo-path}`. Examples:

- `crab://my-bucket/my-repo`
- `crab://my-bucket/org/team/project`
- `crab://my-bucket/users/xin/model-a`

A single bucket can host many repos. If repos share an `xet/` prefix (configurable), they share the dedup scope — xorbs uploaded by one repo’s push can be referenced by another repo’s files without re-upload. This is the mechanism for cross-repo dedup within an organization (§10.3).

-----

## 6. Git Integration

### 6.1 The Remote Helper Protocol

Git defines a protocol for custom remote backends via the remote helper mechanism. When `git` encounters a URL with a custom scheme (here, `crab://`), it searches `$PATH` for `git-remote-<scheme>` and invokes it as a subprocess. The subprocess reads commands from stdin and writes responses to stdout.

crab implements the `fetch` and `push` capabilities, which is sufficient for clone, pull, and push. The protocol is line-oriented:

```
git → helper:   capabilities\n
helper → git:   fetch\n
                push\n
                option\n
                \n

git → helper:   list\n
helper → git:   {sha} refs/heads/main\n
                {sha} refs/heads/dev\n
                @refs/heads/main HEAD\n
                \n

git → helper:   fetch {sha} refs/heads/main\n
                fetch {sha} refs/heads/dev\n
                \n
helper → git:   \n   (after writing objects into local Git ODB)

git → helper:   push refs/heads/main:refs/heads/main\n
                push +refs/heads/dev:refs/heads/dev\n
                \n
helper → git:   ok refs/heads/main\n
                error refs/heads/dev non-fast-forward\n
                \n
```

Full spec: `git help remote-helpers`.

### 6.2 Filter Driver Integration

For large files, crab uses Git’s long-running filter protocol (documented in `git help attributes`). This replaces the older clean/smudge-per-file model with a single persistent filter process, drastically reducing overhead on repos with many large files.

The filter is registered in `.git/config`:

```ini
[filter "crab"]
    process = git-remote-crab filter-process
    required = true
```

And declared in `.gitattributes`:

```
*.safetensors filter=crab
*.bin filter=crab
*.parquet filter=crab
*.ckpt filter=crab
```

crab also provides a `crab track <glob>` CLI command that appends a glob to `.gitattributes` and runs `git add --renormalize .` to apply it.

### 6.3 Pointer File Format

When Git’s clean filter processes a large file, crab emits a pointer blob into Git’s object database. The pointer is small (<256 bytes), so it dedupes naturally within Git and doesn’t trigger xet processing itself.

Pointer format:

```
version https://crab.dev/spec/v1
file-hash {blake3-of-full-content}
size {bytes}
```

Example:

```
version https://crab.dev/spec/v1
file-hash 7c1f2a3b4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8
size 10737418240
```

The file-hash is the full content’s blake3 hash. It’s chosen over SHA-256 for speed (blake3 is ~3-5× faster and just as secure) and over the Xet MerkleHash because the MerkleHash depends on chunk boundaries (which are determined by the chunker), while file-hash should be a stable identity independent of chunking parameters.

Future-compatibility: the `version` URL will increment on breaking format changes. Old clients reading a newer pointer will error with a clear message to upgrade.

**Compatibility note**: crab can optionally read Git-LFS pointers in a best-effort mode for seamless migration. LFS pointers are detected by their distinctive `version https://git-lfs.github.com/spec/v1` header and processed through an LFS-compat code path that fetches from the LFS server URL (stored in `.lfsconfig`). See §19 for details.

### 6.4 Object Format Compatibility

crab stores Git packs in Git’s standard format. Users can, in principle, download packs from a crab bucket and reconstruct a normal `.git` directory manually. This is a deliberate escape hatch: users are not locked in.

Hash algorithm: SHA-1 in v1. SHA-256 support (Git’s object-format=sha256) is a v2 feature; the `config` object’s `hash_algorithm` field is forward-compatible.

-----

## 7. Xet Integration

### 7.1 Chunking: Content-Defined Chunking via Gearhash

crab uses the Gearhash-based CDC implementation from `xet-core::deduplication::chunking`. Parameters (matching Xet defaults):

- Target chunk size: 64 KiB
- Minimum: 8 KiB (except last chunk of file)
- Maximum: 128 KiB
- Boundary predicate: rolling hash bits match a configured pattern

CDC’s critical property: chunk boundaries are determined by content, so an insertion or deletion shifts boundaries only locally around the edit. Inserting 4 bytes at the start of a 10 GB file re-chunks ~1 chunk near the edit and leaves the rest unchanged.

### 7.2 Chunk Hashing

Each chunk’s identity is a MerkleHash computed over its bytes. MerkleHash is defined in `xet-core::merklehash`. Hash is 256 bits (32 bytes), stored as 64 hex characters in paths.

### 7.3 Xorb Format

Xorbs aggregate chunks into ~64 MiB content-addressed blobs. Layout (from Xet spec):

```
[Header: magic + version]
[Chunks: for each chunk, [length | compressed bytes]]
[Footer: chunk count, offsets table, content hash]
```

Compression: zstd at level 3 per chunk (configurable). Chunks compress individually so that `Range` GETs on a xorb can extract a specific chunk without decompressing the whole xorb.

The xorb’s content address is a MerkleHash computed over the concatenation of its chunk hashes (not over the compressed bytes). This means the same logical chunk content yields the same xorb hash regardless of compression level, enabling cross-client dedup even when clients disagree on compression.

Xorb packing strategy: prefer contiguous runs of chunks from the same file. xet-core’s recommendation is to keep continuous runs of 1 MiB+ together rather than aggressively deduping single chunks, which would scatter a file’s chunks across many xorbs and make reads slow (many `Range` GETs across many objects). crab follows this: chunks from a run are kept together unless dedup saves more than 25% of the run’s size.

### 7.4 Shard Format

Shards describe file reconstructions. A shard contains:

- **CAS info section**: list of xorbs and the chunks they contain.
- **File info section**: for each file this shard describes, a list of terms `[xorb_hash, chunk_start, chunk_end)` that, when concatenated, produce the file.

crab uses `xet-core::mdb_shard` for the binary format.

One shard per push that touches xet files, named by the MerkleHash of its content. Single pushes with many files get one shard; single pushes with one large file also get one shard. The shard’s size is typically in kilobytes to a few megabytes; large pushes touching tens of thousands of files may yield shards up to tens of MiB.

### 7.5 Reconstruction Walkthrough

Fetching a large file on checkout:

1. `git checkout` invokes the smudge filter with the pointer blob’s content on stdin.
1. Filter parses the pointer: `(file-hash, size)`.
1. Filter GETs `xet/file-index/{file-hash[:2]}/{file-hash}` from S3. Body is the shard hash.
1. Filter checks local shard cache; if miss, GETs `xet/shards/{shard-hash[:2]}/{shard-hash}`.
1. Filter parses shard, extracts reconstruction terms for this file: `[(xorb_a, 0, 47), (xorb_b, 12, 203), ...]`.
1. For each term:
   a. Look up term’s chunk range in the shard’s CAS info section to get a byte range within the xorb.
   b. Check local chunk cache for cached chunks in this range.
   c. For uncached ranges, issue parallel `GET` requests to `xet/xorbs/{xorb[:2]}/{xorb}` with `Range: bytes=start-end` headers.
   d. Decompress chunks, verify hashes, cache locally.
1. Concatenate chunks in reconstruction order, write to stdout.
1. Git writes the smudge output to the working tree.

This path is latency-bound on fresh clones (many round trips to S3) and bandwidth-bound on large files. Optimizations:

- **Parallel xorb fetches** up to a configurable concurrency (default 16).
- **HTTP/2 multiplexing** via reqwest’s default pool.
- **Speculative prefetch**: once a shard is loaded, prefetch referenced xorbs in the background while Git iterates over pointer files.
- **Chunk deduplication in cache**: identical chunks across files fetch once and serve many reads.

### 7.6 The `xet-core` Dependency

crab depends on three crates from `xet-core` (Apache-2.0, HuggingFace):

```toml
merklehash = { git = "https://github.com/huggingface/xet-core", branch = "main" }
mdb_shard = { git = "https://github.com/huggingface/xet-core", branch = "main" }
deduplication = { git = "https://github.com/huggingface/xet-core", branch = "main" }
```

These provide the hash types, the shard binary format, and the chunking algorithm. crab does **not** use:

- `cas_client` (HTTP client for HuggingFace’s CAS service — we talk to S3 directly).
- `xet_pkg` / `hf_xet` (Python bindings).
- `data` (upload/download orchestration — we implement our own against `object_store`).

This keeps the dependency surface minimal and avoids pulling in HuggingFace-specific networking code.

Because xet-core is not yet published to crates.io as standalone library crates, crab pins a specific commit hash in `Cargo.toml` for reproducible builds. A periodic dependency-bump task re-evaluates the pin.

-----

## 8. Metadata & Consistency

### 8.1 What’s Mutable, What Isn’t

|Object                                  |Mutable?    |Concurrency mechanism          |
|----------------------------------------|------------|-------------------------------|
|Packs, xorbs, shards, file-index entries|No          |Content addressing (write-once)|
|Refs (refs/heads/*, refs/tags/*, HEAD)  |Yes         |S3 CAS (If-Match on ETag)      |
|pack-list, shard-list                   |Yes         |S3 CAS                         |
|config                                  |Yes (rarely)|S3 CAS                         |

All concurrency logic operates on the mutable set, which is small.

### 8.2 S3 Conditional Write Primitives

AWS S3 supports two conditional write headers:

- **`If-None-Match: *`** (PutObject) — succeed only if the object does not exist. Used for initial creation.
- **`If-Match: <etag>`** (PutObject, CopyObject) — succeed only if the current object has the given ETag. Used for atomic updates.

Both return HTTP 412 Precondition Failed on mismatch.

The `object_store` crate abstracts these as `PutMode::Create` and `PutMode::Update(UpdateVersion { e_tag })` respectively.

### 8.3 Ref Update Algorithm

```rust
async fn update_ref(
    store: &dyn ObjectStore,
    path: &Path,
    old_sha: Option<Sha1>,
    new_sha: Sha1,
) -> Result<(), PushError> {
    const MAX_RETRIES: u32 = 5;
    let mut attempt = 0;

    loop {
        attempt += 1;
        if attempt > MAX_RETRIES {
            return Err(PushError::TooManyRetries);
        }

        match old_sha {
            // Creating a new ref
            None => {
                let result = store
                    .put_opts(
                        path,
                        new_sha.to_hex().into(),
                        PutOptions {
                            mode: PutMode::Create,
                            ..Default::default()
                        },
                    )
                    .await;

                match result {
                    Ok(_) => return Ok(()),
                    Err(object_store::Error::AlreadyExists { .. }) => {
                        return Err(PushError::RefAlreadyExists);
                    }
                    Err(e) => return Err(e.into()),
                }
            }

            // Updating an existing ref
            Some(expected) => {
                let current = store.get(path).await?;
                let etag = current
                    .meta
                    .e_tag
                    .clone()
                    .ok_or(PushError::MissingETag)?;
                let current_bytes = current.bytes().await?;
                let current_sha: Sha1 = current_bytes.as_ref().try_into()?;

                // Caller has already verified new_sha descends from expected.
                // This check defends against racing pushes:
                if current_sha != expected {
                    return Err(PushError::NonFastForward {
                        have: current_sha,
                        want: expected,
                    });
                }

                let result = store
                    .put_opts(
                        path,
                        new_sha.to_hex().into(),
                        PutOptions {
                            mode: PutMode::Update(UpdateVersion {
                                e_tag: Some(etag),
                                version: None,
                            }),
                            ..Default::default()
                        },
                    )
                    .await;

                match result {
                    Ok(_) => return Ok(()),
                    Err(object_store::Error::Precondition { .. }) => {
                        // Someone else updated the ref; retry from top
                        tokio::time::sleep(backoff_duration(attempt)).await;
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
}

fn backoff_duration(attempt: u32) -> Duration {
    let base_ms = 100 * 2u64.pow(attempt.min(10));
    let jitter = rand::random::<u64>() % base_ms;
    Duration::from_millis(base_ms + jitter)
}
```

Rust code throughout this document uses the `object_store` crate’s error variants. `object_store::Error::Precondition` corresponds to HTTP 412; `AlreadyExists` corresponds to 409/412 on `If-None-Match: *`.

### 8.4 Manifest Update Algorithm

`pack-list` and `shard-list` use the same CAS pattern:

```rust
async fn update_manifest<T: Manifest>(
    store: &dyn ObjectStore,
    path: &Path,
    mutation: impl Fn(&mut T),
) -> Result<T, PushError> {
    const MAX_RETRIES: u32 = 10;
    let mut attempt = 0;

    loop {
        attempt += 1;
        if attempt > MAX_RETRIES {
            return Err(PushError::TooManyRetries);
        }

        let (mut manifest, etag) = load_manifest_with_etag::<T>(store, path).await?;
        mutation(&mut manifest);
        manifest.generation += 1;

        let body = serde_json::to_vec(&manifest)?;

        let result = match etag {
            Some(etag) => store.put_opts(
                path,
                body.into(),
                PutOptions {
                    mode: PutMode::Update(UpdateVersion {
                        e_tag: Some(etag),
                        version: None,
                    }),
                    ..Default::default()
                },
            ).await,
            None => store.put_opts(
                path,
                body.into(),
                PutOptions { mode: PutMode::Create, ..Default::default() },
            ).await,
        };

        match result {
            Ok(_) => return Ok(manifest),
            Err(object_store::Error::Precondition { .. })
            | Err(object_store::Error::AlreadyExists { .. }) => {
                tokio::time::sleep(backoff_duration(attempt)).await;
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
}
```

Manifest updates are idempotent: re-applying the same mutation (after a retry that observed it was already applied) produces the same state. The mutation closure is expected to be a set insertion or similar idempotent operation.

### 8.5 Multi-Ref Atomic Pushes

Git’s push model expects all-or-nothing semantics when pushing multiple refs: either all updates succeed or none do. S3 has no multi-object transaction primitive.

crab approximates this with a “push manifest” object written before individual refs are updated:

```
s3://{repo}/push-manifests/{push-uuid}
```

Body:

```json
{
  "push_id": "uuid-v4",
  "client_id": "machine-user",
  "timestamp": "...",
  "updates": [
    {"ref": "refs/heads/main", "old": "abc...", "new": "def..."},
    {"ref": "refs/heads/dev",  "old": "111...", "new": "222..."}
  ],
  "packs": ["sha-of-pack-uploaded-this-push"],
  "shards": ["sha-of-shard-if-any"],
  "status": "pending"
}
```

Procedure:

1. Upload all packs, xorbs, shards.
1. Update pack-list and shard-list.
1. PUT the push-manifest with `status: "pending"`.
1. Update each ref via CAS.
1. On all refs updated successfully: PUT the push-manifest with `status: "committed"`.
1. On any ref update failing: PUT the push-manifest with `status: "failed"`, rollback attempted ref updates.

The push-manifest is NOT the source of truth for refs — individual ref objects are. The push-manifest is an audit log and a recovery aid for partial failures. A separate recovery routine (run at `crab fsck`) reads pending manifests older than N hours and decides whether to complete or abandon them.

Caveat: true atomic multi-ref pushes across multiple S3 conditional writes are impossible without a coordinator. A crash between step 4 and step 5 leaves some refs updated and others not. This is the same failure mode git has when pushing to a standard server over a flaky network — the client and server may disagree on push outcome. In practice:

- Most pushes update a single ref (non-issue).
- Multi-ref pushes are typically “main + tag” patterns where either order is acceptable.
- Recovery via push-manifest reconciliation handles the edge cases on next operation.

For users who need strict atomic multi-ref, v2 will offer a “staged ref” mode where refs are updated through an indirection object with true transactional semantics using a coordination service (e.g., a small Lambda + DynamoDB table). This is outside the v1 pure-client scope.

### 8.6 Read Consistency

AWS S3 provides strong read-after-write consistency for single-object operations since late 2020. This means:

- After a successful PUT, a subsequent GET returns the new content.
- After a successful DELETE, a subsequent GET returns 404.
- After a conditional PUT succeeds, readers see the new content immediately.

List operations may show stale results briefly — this is why crab uses manifest objects (pack-list, shard-list) as the authoritative enumeration rather than `LIST` operations.

GCS and Azure Blob Storage provide equivalent guarantees. MinIO and R2 provide strong consistency in their modern versions.

### 8.7 Sequence Diagram: Concurrent Pushes

```
Client A                       S3                        Client B
   |                           |                            |
   | - chunk & build packs     |                            |
   | - upload packs/xorbs      |                            |
   |-------- PUT ------------->|                            |
   |                           |                            |
   |                           |                 - chunk & build packs
   |                           |                 - upload packs/xorbs
   |                           |<-------- PUT --------------|
   |                           |                            |
   | GET pack-list (etag=E1)   |                            |
   |<--------------------------|                            |
   |                           |   GET pack-list (etag=E1)  |
   |                           |----------------------------|
   |                           |                            |
   | PUT pack-list             |                            |
   |   If-Match: E1    ────────>                            |
   |   200 OK, new etag=E2     |                            |
   |                           |   PUT pack-list            |
   |                           |     If-Match: E1  ─────────|
   |                           |   412 Precondition Failed  |
   |                           |                            |
   |                           |    GET pack-list (etag=E2) |
   |                           |<---------------------------|
   |                           |    PUT pack-list           |
   |                           |      If-Match: E2  ────────|
   |                           |    200 OK, new etag=E3     |
   |                           |                            |
   | PUT refs/heads/main       |                            |
   |   If-Match: Eref  ────────>                            |
   |   200 OK                  |                            |
   |                           |    PUT refs/heads/dev      |
   |                           |      If-Match: *  ─────────|
   |                           |    200 OK                  |
```

Both pushes succeed; the manifest is updated serially, refs don’t conflict because they’re different.

-----

## 9. Protocol Flows

### 9.1 Clone Flow

```
User runs: git clone crab://bucket/my-repo

1. git parses URL, sees "crab://" scheme, spawns:
     git-remote-crab crab crab://bucket/my-repo

2. Helper initializes:
   - resolves cloud credentials (SigV4 chain: env vars, ~/.aws/, IMDS)
   - constructs ObjectStore client
   - tests bucket access (HEAD on config object)

3. git → helper: capabilities
   helper → git: fetch, push, option, \n

4. git → helper: list
   helper:
     - GETs s3://bucket/my-repo/HEAD
     - LISTs s3://bucket/my-repo/refs/ (one level deep, then recurse)
     - For each ref, GET its content (40-byte SHA)
   helper → git: (for each ref) {sha} refs/...\n
                  @refs/heads/main HEAD\n
                  \n

5. git → helper: fetch {sha1} refs/heads/main
                 fetch {sha2} refs/heads/dev
                 \n

6. Helper fetch pipeline:
   - GET pack-list
   - For each pack not in local .git/objects/pack:
     - GET pack-{sha}.pack, .idx, .bitmap in parallel
     - Write directly into .git/objects/pack/
     - git will index them if needed (though we provide .idx)

7. helper → git: \n   (signals pack download complete)

8. git completes ref update, checks out working tree

9. Smudge filter runs for each pointer file:
   - parse pointer
   - GET file-index entry
   - GET shard (first time)
   - GET xorbs in parallel with Range headers
   - decompress chunks, write to working tree
```

Performance characteristics:

- **Network round trips**: 1 (HEAD+list refs) + N_refs (GET each ref) + 1 (pack-list) + N_packs + M_files_worth_of_xorbs.
- **Parallelism**: refs fetched in a single LIST + parallel GETs (up to 50 concurrent). Packs in parallel (up to 16). Xorbs in parallel (up to 16).
- **Bottleneck for cold clone**: xorb download bandwidth. For a 10 GB model repo, expect clone to be I/O-bound on the user’s internet connection.

### 9.2 Pull/Fetch Flow

Identical to clone but:

- Refs are listed first to determine what’s new.
- Packs already present locally are skipped.
- Only chunks for changed pointer files are fetched (Git’s checkout logic handles this — unchanged files don’t trigger smudge).

### 9.3 Push Flow (Detailed)

```
User runs: git push crab main

1. git computes delta between local refs/heads/main and remote.
   (Remote refs were cached from last fetch; if stale, git fetches first.)

2. git runs clean filters on large files (if any staged).
   Clean filter (long-running process):
     - reads file content from stdin
     - chunks with CDC
     - writes chunks to local staging area: ~/.cache/crab/{repo}/staging/chunks/
     - records (file-hash, [(chunk-hash, offset, size)]) in staging index
     - emits pointer blob to stdout
   git stores pointer blobs in its object database.

3. git builds packfile of new objects, invokes push.

4. git → helper: push refs/heads/main:refs/heads/main
                 \n

5. Helper push pipeline:

   a. Validate:
      - git has provided the pack path (via GIT_OBJECT_DIRECTORY env)
      - read pack, verify SHA trailer, count objects

   b. Dedup planning (if staging area has chunks):
      - Load local shard cache: ~/.cache/crab/{repo}/shards/
      - Build in-memory chunk → xorb index from cached shards
      - For each new chunk in staging:
        - If in local index: note reference (no upload needed)
        - Else: mark as new
      - Optional: issue global dedup query (see §10.3)

   c. Xorb assembly:
      - Group new chunks into xorbs, preferring contiguous runs
      - For each xorb:
        - Write to ~/.cache/crab/{repo}/staging/xorbs/
        - Compute MerkleHash

   d. Shard construction:
      - Build shard describing all files in this push
      - Record all xorb-and-chunk metadata
      - Compute MerkleHash of shard

   e. Upload xorbs (parallel, with backpressure):
      - PUT each to xet/xorbs/{hash[:2]}/{hash}
      - Use multipart upload for xorbs >5 MiB (most are)
      - Retry on 503, 500, network errors with exponential backoff

   f. Upload shard:
      - PUT to xet/shards/{hash[:2]}/{hash}

   g. Upload file-index entries:
      - For each file in this push, PUT xet/file-index/{file-hash[:2]}/{file-hash}
        with body = shard hash
      - Use If-None-Match: * (should not already exist; warn if it does)

   h. Upload Git pack:
      - PUT packs/pack-{sha}.pack, .idx, .bitmap
      - Multipart for large packs

   i. Update pack-list (CAS loop):
      - Append new pack SHA
      - increment generation
      - PUT with If-Match

   j. Update shard-list (CAS loop):
      - Append new shard SHA
      - increment generation
      - PUT with If-Match

   k. Write push manifest (pending):
      - PUT push-manifests/{uuid}

   l. Update refs (CAS loop per ref):
      - CAS old_sha → new_sha
      - If any fails: abort, return error to git
      - If all succeed: continue

   m. Commit push manifest:
      - PUT push-manifests/{uuid} with status=committed

6. helper → git: ok refs/heads/main\n
                  \n

7. helper cleanup:
   - Move staging chunks/xorbs/shards into non-staging cache
     (they're now authoritative on S3 too)
   - Truncate staging
```

### 9.4 Staging & Clean Filter Details

The clean filter is potentially invoked thousands of times per commit (once per file matched by `.gitattributes`). Performance constraints:

- Must be a long-running process, not spawn-per-file.
- Must stream: 50 GB files cannot be buffered in memory.
- Must be interruptible: user cancelling `git add` shouldn’t leave inconsistent state.

Implementation:

```rust
// Long-running filter protocol
// See: https://git-scm.com/docs/gitattributes#_long_running_filter_process

async fn run_filter_process() -> Result<()> {
    // Handshake
    handshake_packet("git-filter-client").await?;
    write_packet("version=2").await?;
    flush_packet().await?;

    // Capabilities
    expect_packet("capability=clean").await?;
    expect_packet("capability=smudge").await?;
    expect_flush().await?;
    write_packet("capability=clean").await?;
    write_packet("capability=smudge").await?;
    write_packet("capability=delay").await?; // enables parallelism
    flush_packet().await?;

    loop {
        match read_command().await? {
            Command::Clean { pathname, content_stream } => {
                let pointer = clean(pathname, content_stream).await?;
                write_result(pointer).await?;
            }
            Command::Smudge { pathname, content_stream } => {
                let real_content = smudge(pathname, content_stream).await?;
                write_result_stream(real_content).await?;
            }
            Command::Quit => break,
        }
    }

    Ok(())
}

async fn clean(path: PathBuf, mut stream: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut hasher = blake3::Hasher::new();
    let mut chunker = GearhashChunker::new();
    let mut total_size = 0;
    let mut chunks_recorded = Vec::new();

    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
        total_size += n;

        // Feed to chunker; for each complete chunk boundary:
        for chunk in chunker.feed(&buf[..n]) {
            let chunk_hash = merkle_hash(&chunk.bytes);
            staging::write_chunk(&chunk_hash, &chunk.bytes).await?;
            chunks_recorded.push((chunk_hash, chunk.offset, chunk.len));
        }
    }
    // Finalize last chunk
    if let Some(chunk) = chunker.finalize() {
        let chunk_hash = merkle_hash(&chunk.bytes);
        staging::write_chunk(&chunk_hash, &chunk.bytes).await?;
        chunks_recorded.push((chunk_hash, chunk.offset, chunk.len));
    }

    let file_hash = hasher.finalize();
    staging::record_file(&file_hash, &chunks_recorded).await?;

    let pointer = format!(
        "version https://crab.dev/spec/v1\nfile-hash {}\nsize {}\n",
        file_hash.to_hex(), total_size
    );
    Ok(pointer.into_bytes())
}
```

Staging directory layout:

```
~/.cache/crab/{repo-hash}/staging/
├── chunks/
│   └── {hash[:2]}/{hash}            (raw chunk bytes, uncompressed)
├── files.db                          (sqlite: file-hash → [(chunk-hash, offset, len)])
└── lockfile
```

On push commit, staging chunks are processed into xorbs and uploaded. On push failure, staging chunks are retained and can be retried. A `crab staging clean` command clears staged data for interrupted pushes beyond some age.

### 9.5 Smudge Filter Details

Smudge is called per pointer on checkout. With the `delay` capability, crab can receive multiple smudge requests and process them in parallel, which is essential for fast initial clones of large repos.

```rust
async fn smudge(path: PathBuf, mut stream: impl AsyncRead + Unpin) -> Result<impl AsyncRead> {
    let mut pointer = String::new();
    stream.read_to_string(&mut pointer).await?;
    let (file_hash, size) = parse_pointer(&pointer)?;

    // 1. Resolve file-hash → shard-hash
    let shard_hash = resolve_file_to_shard(&file_hash).await?;

    // 2. Load shard (from cache if possible)
    let shard = load_shard(&shard_hash).await?;

    // 3. Get reconstruction terms for this file
    let terms = shard.terms_for(&file_hash)?;

    // 4. Fetch chunks in parallel, return a streaming reader
    let reader = XetFileReader::new(terms, chunk_cache, xorb_fetcher);
    Ok(reader)
}
```

The `XetFileReader` is an async stream that:

- Groups terms by xorb to coalesce range requests.
- Issues up to N parallel `Range` GETs against S3.
- Feeds decompressed chunks to the output in file order.
- Populates the local chunk cache as it goes.

For very large files, the reader prefetches ahead by ~8 MiB to overlap network and write latency.

-----

## 10. Deduplication Strategy

### 10.1 Three Scopes

crab supports dedup at three scopes, in increasing order of complexity:

1. **Session-local** (within a single push): easy, free.
1. **Repo-local** (across pushes within one repo): easy, requires local shard cache.
1. **Cross-repo / global**: requires a coordination mechanism.

### 10.2 Session-Local Dedup

During a single push, deduplicate chunks within the same staging operation:

```rust
let mut seen_chunks: HashSet<MerkleHash> = HashSet::new();
for file in files_to_stage {
    for chunk in chunk_file(file) {
        if seen_chunks.insert(chunk.hash) {
            staging::write_chunk(chunk).await?;
        }
        // Either way, record reference in file's reconstruction
        record_reference(file, chunk.hash);
    }
}
```

Free in code complexity. Typical savings on AI workloads: low single-digit % (most within-file chunks are unique).

### 10.3 Repo-Local Dedup

Maintain a local cache of shards from the current repo. On push, consult the cache to avoid re-uploading chunks that already exist.

The chunk index data structure:

```rust
pub struct RepoChunkIndex {
    /// Maps chunk hash to the xorb containing it.
    chunks: HashMap<MerkleHash, XorbRef>,
    /// Source shards, for lineage.
    shards: HashMap<MerkleHash, ShardInfo>,
}

pub struct XorbRef {
    pub xorb_hash: MerkleHash,
    pub chunk_index_in_xorb: u32,
}

impl RepoChunkIndex {
    pub async fn load_from_cache(cache_dir: &Path) -> Result<Self> {
        let mut index = Self::default();
        for shard_path in fs::read_dir(cache_dir.join("shards")).await? {
            let shard = mdb_shard::load(shard_path).await?;
            for (chunk_hash, xorb_ref) in shard.chunks() {
                index.chunks.entry(chunk_hash).or_insert(xorb_ref);
            }
            index.shards.insert(shard.hash(), shard.info());
        }
        Ok(index)
    }

    pub fn lookup(&self, chunk_hash: &MerkleHash) -> Option<&XorbRef> {
        self.chunks.get(chunk_hash)
    }
}
```

On the push path, after chunking files, crab looks up each new chunk hash in the index. Hits become references to existing xorbs. Misses become new chunks to pack into new xorbs.

**Cache freshness**: the shard cache is updated on every pull and every push. After a successful push, locally-generated shards are written into the cache (treating our own uploads as canonical).

**Memory footprint**: at ~64 KiB average chunk size and ~40 bytes per hashmap entry (32-byte hash + 8-byte XorbRef), a 1 TB repo has ~16M chunks consuming ~640 MB of RAM.

For repos where this is too large, crab uses the on-disk SQLite-backed `PersistentChunkIndex` as the warm dedup tier. Lookup latency is higher than an in-memory map, but memory stays bounded and SQLite WAL mode supports concurrent readers with one serialized writer.

### 10.4 Cross-Repo Dedup Within a Bucket

The simplest form of global dedup: multiple repos share an `xet/` prefix in the same bucket.

```toml
# .crab/config in repo A
[dedup]
xet_prefix = "shared-xet"  # overrides default xet/

# .crab/config in repo B
[dedup]
xet_prefix = "shared-xet"
```

Both repos write xorbs and shards to `s3://bucket/shared-xet/`. File-index entries are still per-repo (they reference per-repo shards that can describe reconstruction). Packs remain per-repo.

Effect: if two repos contain the same 10 GB checkpoint, it’s stored once. Fine-tuned model variants that share 90% of their base weights dedupe 90% of their bytes.

**Caveat**: GC becomes cross-repo. A xorb cannot be deleted while any repo references any of its chunks. The GC algorithm (§13) computes the reference set across all repos sharing the prefix.

### 10.5 Global Dedup Across Buckets (Optional v2)

When organizations have multiple buckets or want dedup across tenants, a coordination service is needed. Xet’s original design uses a CAS service with HMAC-protected chunk hashes for privacy. crab can adopt the same approach with a small Lambda + DynamoDB deployment:

- DynamoDB table: `{chunk_hash_hmac → (xorb_url, byte_range)}`.
- Lambda endpoint: POST batch of chunk hashes → return known locations.
- Client consults this endpoint during push, treats responses as additional dedup sources.

HMAC key management: each organization gets its own HMAC key, distributed via IAM-gated SSM parameter. Chunks from different organizations are incomparable (their HMAC’d hashes differ), preserving privacy while allowing dedup within org.

This is explicitly out of v1 scope. V1 ships with bucket-prefix dedup (§10.4), which covers the common enterprise case.

### 10.6 Tuning

Dedup aggressiveness is tuned by one parameter: the minimum run length for xorb continuity. Default: 1 MiB. Rationale: deduping isolated 64 KiB chunks scatters a file’s content across many xorbs, causing many `Range` GETs on read, which can make reads slower than the storage saved.

crab provides `crab bench dedup <path>` to estimate dedup ratios on a directory tree given the current config.

-----

## 11. Caching Architecture

### 11.1 Cache Hierarchy

```
~/.cache/crab/
├── {bucket}/{repo-path-hash}/
│   ├── staging/                     (in-flight push data)
│   │   ├── chunks/
│   │   ├── xorbs/
│   │   └── files.db
│   ├── shards/                      (downloaded shards for dedup index)
│   │   └── {hash[:2]}/{hash}
│   ├── chunks/                      (decompressed chunks for fast checkout)
│   │   └── {hash[:2]}/{hash}
│   ├── manifests/
│   │   ├── pack-list.json           (last-known pack-list)
│   │   └── shard-list.json
│   └── buckets/{bucket}/chunk-index.sqlite (bucket-global warm index)
└── shared/                          (global across repos on this machine)
    └── chunks/                      (chunk hash is global; share if disk space allows)
        └── {hash[:2]}/{hash}
```

### 11.2 Cache Policies

- **Shard cache**: unbounded by default. Shards are small (KiB-MiB) and their fully-populated set is the dedup index — too valuable to evict aggressively. Users can configure a max size; oldest-accessed shards are evicted.
- **Chunk cache (per-repo)**: bounded by disk space, default 10 GiB. LRU eviction. Chunks are easy to refetch (single `Range` GET) so false evictions are cheap.
- **Chunk cache (shared)**: optional, opt-in, default off. When on, chunks are shared across repos on the machine; a checkout of repo B’s large file reuses chunks cached from repo A’s checkout if they’re the same. Default max 50 GiB.
- **Manifest cache**: trivially small. Always keep latest.

### 11.3 Cache Consistency

The cache is a write-through optimization over S3. Writes go to S3 first; cache is populated on write success.

On read, the cache is consulted first; a miss reads S3 and populates the cache.

Stale cache is possible if an object was deleted on S3 (by GC or another user). Handling:

- Packs and shards: `etag` is cached alongside the body. On next use, a HEAD check verifies the etag matches. If not, refetch.
- Chunks: immutable by content-addressing, so the only staleness is a cached chunk whose content corrupted on disk. Periodic fsck catches this.

### 11.4 Cache Lifecycle Commands

```
crab cache stats        # show size, hit rate, file counts
crab cache prune        # evict to target size
crab cache clean        # nuke cache; safe, just slow on next op
crab cache verify       # check content hashes against file names
```

-----

## 12. Security Model

### 12.1 Authentication

crab uses cloud-native authentication exclusively. No custom usernames, passwords, or tokens.

|Cloud     |Mechanism                                           |
|----------|----------------------------------------------------|
|AWS       |Environment credentials, web identity, ECS task credentials, IMDS on EC2|
|GCP       |Service account, ADC                                |
|Azure     |Entra ID, managed identity                          |
|R2 / MinIO|S3-compatible access key pair                       |

Credentials are discovered by `object_store`'s provider chain. The current S3
provider supports environment credentials, web identity, ECS task credentials,
and EC2 instance metadata; it does not read shared AWS profiles or credential
files.

For human users who don’t already have cloud credentials: crab documents how to set up short-lived STS tokens via SSO providers (AWS IAM Identity Center, Google Workload Identity, etc.). Writing yet-another-auth system is explicitly non-goals.

### 12.2 Authorization

Authorization is IAM policy on the bucket. crab ships reference policies:

**Read-only access to a repo:**

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["s3:GetObject", "s3:ListBucket"],
      "Resource": [
        "arn:aws:s3:::{bucket}",
        "arn:aws:s3:::{bucket}/{repo-path}/*"
      ],
      "Condition": {
        "StringLike": {
          "s3:prefix": ["{repo-path}/*"]
        }
      }
    }
  ]
}
```

**Read-write access to a repo (append-only immutable paths):**

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["s3:GetObject", "s3:ListBucket"],
      "Resource": ["arn:aws:s3:::{bucket}", "arn:aws:s3:::{bucket}/{repo-path}/*"]
    },
    {
      "Effect": "Allow",
      "Action": ["s3:PutObject"],
      "Resource": "arn:aws:s3:::{bucket}/{repo-path}/*"
    },
    {
      "Effect": "Deny",
      "Action": "s3:PutObject",
      "Resource": [
        "arn:aws:s3:::{bucket}/{repo-path}/packs/*",
        "arn:aws:s3:::{bucket}/{repo-path}/xet/xorbs/*",
        "arn:aws:s3:::{bucket}/{repo-path}/xet/shards/*",
        "arn:aws:s3:::{bucket}/{repo-path}/xet/file-index/*"
      ],
      "Condition": {
        "Null": {"s3:If-None-Match": "true"}
      }
    },
    {
      "Effect": "Deny",
      "Action": "s3:DeleteObject",
      "Resource": "arn:aws:s3:::{bucket}/{repo-path}/*"
    }
  ]
}
```

Key clauses:

- Immutable paths (packs, xorbs, shards, file-index) are denied PutObject unless it carries `If-None-Match: *`, enforcing create-only semantics at the IAM layer. An authorized writer cannot accidentally or maliciously overwrite content-addressed objects.
- DeleteObject is denied entirely; only GC admin roles can delete.

**GC admin:**

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["s3:DeleteObject", "s3:ListBucket", "s3:GetObject"],
      "Resource": ["arn:aws:s3:::{bucket}", "arn:aws:s3:::{bucket}/{repo-path}/*"]
    }
  ]
}
```

Assumed by the GC process (scheduled Lambda or manual CLI run).

### 12.3 What IAM Can’t Protect Against

**A compromised or malicious authorized writer can:**

- Push bad commits (rewriting ref to point at harmful content).
- Fill the bucket with garbage (DoS on cost).
- Leak repo content via copying to another accessible location.

These are application-layer concerns and require:

- Branch protection: a separate control layer forbidding force-push on protected refs (not expressible in IAM; requires either a coordinator service or bucket-policy trickery with signed assertions).
- Budget alarms on cost.
- Audit logging (CloudTrail).

crab v1 doesn’t solve these; documentation points users at cloud-native tools for each.

**Credential theft**: if a user’s AWS credentials leak, attacker has full access up to the user’s IAM scope. Mitigations (user-responsibility):

- Short-lived STS tokens instead of long-lived access keys.
- MFA on console access.
- CloudTrail monitoring for anomalous bucket access.

crab never stores credentials itself; they pass through to `object_store`.

### 12.4 Integrity

Every non-trivial read verifies content hash before use:

- Chunks: hash of decompressed bytes vs MerkleHash in shard.
- Xorbs: hash of entire xorb vs filename.
- Shards: hash vs filename.
- Packs: SHA-1 trailer vs filename.
- Git objects: standard Git verification.

Hash mismatch is treated as corruption: the object is evicted from cache, a warning is logged, and the fetch retries once. Persistent corruption raises a hard error and aborts the operation.

**Does not protect against**: a malicious writer that computes a valid hash for bad content. IAM governs who can write; content integrity protects against bit rot and transport errors, not adversarial content.

### 12.5 Optional Encryption at Rest

S3 SSE (server-side encryption) with SSE-S3 or SSE-KMS is transparent and enabled at the bucket level. crab has no opinion.

Client-side encryption is **not** supported in v1 because it would break dedup: two users encrypting the same 10 GB file with different keys produce uncorrelated ciphertext.

Convergent encryption (key derived from plaintext hash) preserves dedup but has well-known weaknesses (confirmation-of-a-file attacks). crab may add this in v2 with appropriate warnings.

-----

## 13. Garbage Collection

### 13.1 Why GC Is Necessary

Every push writes new packs, xorbs, shards, and file-index entries. Ref updates (rebase, force push, branch deletion) make older content unreachable. Without GC, storage grows unboundedly.

### 13.2 Reachability

An object is **reachable** if:

- Any ref points to it (ref → commit), OR
- Any reachable commit references it (commit → tree → blob, blob → pointer → file-index entry → shard → xorb → chunks).

An object is **unreachable** if no ref path leads to it.

### 13.3 GC Algorithm

```
1. Snapshot refs:
   - LIST s3://{repo}/refs/
   - GET each ref's content
   - Record {ref_name: sha} at time T0

2. Walk Git reachability:
   - Load all packs (from pack-list)
   - Starting from each ref's commit SHA, traverse commit graph
   - For each commit: walk its tree, enumerate blob SHAs
   - For each blob: if it's a pointer, parse out file-hash
   - Collect: {reachable_commit_shas, reachable_tree_shas, reachable_blob_shas, reachable_file_hashes}

3. Resolve file-hashes to shards:
   - For each reachable_file_hash: GET file-index/{hash[:2]}/{hash}
   - Record: reachable_shard_hashes

4. Resolve shards to xorbs:
   - For each reachable shard: load shard, extract all xorb references
   - Record: reachable_xorb_hashes

5. Enumerate all storage:
   - LIST packs/ → all_pack_shas
   - LIST xet/shards/ → all_shard_hashes
   - LIST xet/xorbs/ → all_xorb_hashes
   - LIST xet/file-index/ → all_file_hashes
   - (pack reachability is at pack-level granularity; we don't GC individual pack objects)

6. Compute unreachable sets:
   - unreachable_packs = all_pack_shas - reachable_packs (where reachable_packs = packs containing at least one reachable commit)
   - unreachable_shards = all_shard_hashes - reachable_shard_hashes
   - unreachable_xorbs = all_xorb_hashes - reachable_xorb_hashes
   - unreachable_file_index = all_file_hashes - reachable_file_hashes

7. Apply grace period:
   - For each unreachable object: check LastModified.
   - Only mark for deletion if LastModified < T0 - GRACE_PERIOD.
   - Grace period default: 14 days.
   - This protects against: recent uploads not yet referenced due to in-flight pushes.

8. Two-phase delete:
   - Phase A (mark): tag unreachable objects with "gc_pending=true, gc_date=T0".
   - Wait at least 24 hours.
   - Phase B (sweep): on next GC run, re-verify unreachability and delete objects tagged in previous run.
   - This protects against: a snapshot taken during concurrent push where we missed a ref.

9. Update manifests:
   - Remove deleted packs from pack-list (CAS).
   - Remove deleted shards from shard-list (CAS).
```

### 13.4 Pack-Level vs Object-Level GC

crab does not implement Git’s object-level GC (rewriting packs to exclude specific unreachable objects within a pack). Instead, packs are GC’d at whole-pack granularity: a pack is deleted only if none of its objects are reachable.

Rationale:

- Pack rewriting requires reading and re-packing the entire pack. On GB-sized packs this is expensive.
- Most unreachable content in AI repos is in xet xorbs (large files), not Git pack objects (small). The meaningful GC happens there.
- Pack consolidation is a separate maintenance operation. Geometric repack rolls
  up small packs while leaving large stable packs intact, avoiding repeated
  whole-history rewrites as the repository grows.

`crab repack` is a distinct command that reads the committed pack inventory,
uses `git repack --geometric=2 -d` to produce a bounded pack progression, and
atomically publishes the replacement inventory. Superseded immutable packs are
retained until the normal recovery and GC policy removes them.

### 13.5 Running GC

Three deployment options:

**Manual**: user runs `crab gc --repo crab://bucket/my-repo`.

**Client-side scheduled**: a systemd timer or cron job on a user’s machine runs gc periodically.

**Server-side scheduled** (optional, not v1-required but “still serverless” in the AWS sense): a Lambda function on a CloudWatch schedule. One function per account, reads repos from a config object, runs GC on each. Deployment template shipped as an optional CDK stack.

All three use the same core GC code, just triggered differently.

### 13.6 GC Safety

Critical invariants:

- **Never delete reachable objects.** Two-phase delete with grace period ensures safety against in-flight pushes.
- **Never delete objects not yet manifested.** The LIST in step 5 may include very recently uploaded objects that aren’t yet in the pack-list/shard-list. The grace period protects these.
- **Concurrent GC is idempotent.** If two GC processes run simultaneously (rare but possible), they compute the same reachable set and tag/delete the same objects. No harm.

### 13.7 Orphan Detection

A separate command, `crab fsck`, identifies inconsistencies:

- Refs pointing at non-existent commits.
- Commits referencing non-existent trees/blobs.
- Pointers referencing non-existent file-index entries.
- Shards referencing non-existent xorbs.
- Xorbs referenced by zero shards (orphans).
- Packs in pack-list not present in storage.

Packs in storage but absent from the manifest are immutable GC candidates, not
integrity failures. Findings are reported with suggested remediations, and
`crab fsck --repair` attempts only safe repairs.

-----

## 14. Observability

### 14.1 Tracing

crab instruments itself with the `tracing` crate. Every top-level operation (fetch, push, clean, smudge) is a root span. Sub-operations (chunk, upload-xorb, cas-ref) are child spans.

Default behavior: traces emitted to stderr at WARN level. Structured (JSON) if stderr is not a TTY.

```
CRAB_LOG=debug crab push                 # increase verbosity
CRAB_LOG=trace crab push 2> trace.log   # full trace to file
```

With OTLP (optional feature):

```
CRAB_OTLP_ENDPOINT=http://localhost:4317 crab push
```

Traces exported to any OTLP collector. Useful for users running crab at scale who already have observability infrastructure.

### 14.2 Metrics

Per-operation metrics emitted via the `metrics` crate:

- `crab.push.duration` (histogram, labeled by repo)
- `crab.push.bytes_uploaded` (counter)
- `crab.push.objects_deduped` (counter)
- `crab.fetch.duration`
- `crab.fetch.bytes_downloaded`
- `crab.fetch.cache_hit_ratio`
- `crab.cas.retries` (counter)
- `crab.xorb.upload_duration`
- `crab.xorb.download_duration`

Emission sinks:

- Default: none (metrics are no-op).
- Local: log-based exporter to stderr on process exit (`crab push --stats`).
- OTLP: if endpoint configured.
- StatsD / Prometheus: via feature flags.

### 14.3 Usage Reports

`crab stat` produces a summary of a repo:

```
Repo: crab://my-bucket/my-repo
  Size on S3:       48.3 GiB
  Unique bytes:     12.1 GiB  (dedup ratio: 4.0×)
  Packs:            14
  Shards:           28
  Xorbs:            742
  Chunks:           89,342
  Refs:             7 branches, 12 tags
  Largest files:
    models/v3.safetensors      10.4 GiB  (chunks shared with 5 other files)
    datasets/train.parquet     8.2 GiB
    ...
```

Implementation: LIST bucket prefix, sum sizes, load all shards for dedup stats.

### 14.4 Debugging

`crab debug <operation>` runs an operation with full tracing enabled and captures timing for each phase. Useful for user bug reports.

-----

## 15. CLI & Configuration

### 15.1 Command Surface

```
crab init <url>                    Initialize a crab repo at the given URL
crab track <glob>                  Add a .gitattributes entry for xet processing
crab untrack <glob>                Remove a .gitattributes entry
crab stat [<url>]                  Show repo statistics
crab gc [<url>]                    Run garbage collection
crab fsck [<url>]                  Integrity check
crab fsck --repair                 Attempt auto-repair
crab repack [<url>]                Consolidate packs
crab cache stats                   Local cache statistics
crab cache prune                   Evict to target size
crab cache clean                   Wipe local cache
crab migrate --from lfs <url>      Migrate from git-lfs to crab
crab bench <file>                  Benchmark CDC chunking on a file
crab version                       Version info
crab filter-process                (internal: invoked by git)
```

The binary is also registered as `git-remote-crab` (same binary, argv[0] dispatch) so `git` finds it.

### 15.2 Repo-Level Configuration

Each repo has a `config` object in its S3 bucket (§5.1). It is read on every connection; changes require a write by an authorized user.

```json
{
  "version": 1,
  "chunk_threshold_bytes": 1048576,
  "xet_enabled": true,
  "xet_prefix": "xet",
  "compression": "zstd",
  "default_branch": "main",
  "required_cli_version": ">=0.5.0"
}
```

`required_cli_version` protects against old clients writing incompatible data after a breaking format change. Enforcement is client-side trust (older clients that predate the field ignore it), so this is a soft guard.

### 15.3 Local Configuration

`~/.config/crab/config.toml` per-user, `.crab/config.toml` per-repo (checked into git alongside `.gitattributes`).

```toml
[cache]
chunk_cache_max_bytes = "10 GiB"
shared_chunk_cache = false
shard_cache_max_bytes = "1 GiB"

[network]
upload_concurrency = 16
download_concurrency = 16
multipart_threshold = "8 MiB"
multipart_chunk_size = "16 MiB"
max_retries = 5

[dedup]
min_run_length_bytes = 1048576
global_dedup = false

[telemetry]
otlp_endpoint = ""
log_level = "warn"
```

### 15.4 URL Format

```
crab://[region.]endpoint/bucket/path/to/repo

Examples:
  crab://s3.amazonaws.com/my-bucket/my-repo
  crab://us-west-2.s3.amazonaws.com/my-bucket/my-repo
  crab://storage.googleapis.com/my-bucket/my-repo
  crab://minio.internal:9000/my-bucket/my-repo
  crab+http://minio.local:9000/my-bucket/my-repo    (plain HTTP for dev)
```

Shorthand for AWS default region:

```
crab://my-bucket/my-repo    → crab://s3.amazonaws.com/my-bucket/my-repo
```

-----

## 16. Error Handling & Recovery

### 16.1 Error Taxonomy

```rust
pub enum CrabError {
    // Transient — retry-worthy
    NetworkTransient(object_store::Error),
    Throttled { retry_after: Option<Duration> },

    // Conflict — retry after re-fetching state
    CasConflict { path: Path, expected_etag: String },
    NonFastForward { ref_name: String, have: Sha1, want: Sha1 },

    // Permanent — surface to user
    NotFound { path: Path },
    Forbidden { path: Path },
    CorruptObject { hash: String, reason: String },
    Configuration(String),
    IncompatibleFormat { required: String, found: String },

    // Environmental — likely user-fixable
    NoCredentials,
    InsufficientSpace { needed: u64, available: u64 },

    // Bug
    Internal(String),
}
```

Each variant maps to a specific retry strategy and user-facing message.

### 16.2 Retry Strategy

- **Network transients**: exponential backoff with full jitter, 5 retries, base 100 ms.
- **Throttled**: respect `Retry-After` header if present; else exponential backoff.
- **CAS conflict**: re-fetch, re-apply mutation, re-attempt. Up to 10 retries.
- **Non-fast-forward**: no retry; user must pull first.
- **NotFound/Forbidden**: no retry; user error.
- **Corruption**: one retry (network corruption vs storage corruption). Then fail.

### 16.3 Crash Recovery

A `crab push` may crash at many points. Recovery logic runs at next operation:

```
On push start:
  1. Load staging area
  2. Check for incomplete uploads (xorbs written locally but not confirmed on S3)
     - Issue HEAD request; if exists, mark complete; if missing, retry upload.
  3. Check for orphan shards (shard uploaded, but file-index entries missing)
     - Re-upload any missing file-index entries.
  4. Check for incomplete manifest updates
     - Re-read manifest; if local state has packs/shards not yet in manifest, apply CAS.
  5. Check for pending push-manifests older than 1 hour
     - If the ref updates are all present on S3, mark as committed.
     - If none of the ref updates are present, mark as failed and clean up refs/packs.
     - If partial, attempt to complete; if unable, mark as failed and warn user.
```

This recovery is fully automatic and idempotent. Users never need to manually “unstick” a push.

### 16.4 User Errors

Every error includes:

- A short message suitable for display.
- A longer explanation of the cause (if known).
- Suggested remediation.
- A unique error code (e.g., `CRAB-E0042`) for documentation lookup.

Example:

```
ERROR [CRAB-E0017]: Non-fast-forward push rejected
  ref: refs/heads/main
  Your local branch is behind the remote. Someone else has pushed
  commits to this branch.

  To resolve:
    git pull --rebase
    git push
```

Error codes are documented in a `crab errors` command and online docs.

-----

## 17. Performance

### 17.1 Targets

|Operation                 |Repo state                   |Target             |Bottleneck                |
|--------------------------|-----------------------------|-------------------|--------------------------|
|Clone (cold)              |1 GB repo                    |< 1 min on 100 Mbps|Network bandwidth         |
|Clone (cold)              |100 GB AI repo               |< 30 min on 1 Gbps |Network bandwidth         |
|Pull (no changes)         |Any                          |< 2 s              |S3 GET latency (list refs)|
|Pull (small change)       |10 GB checkpoint, 10% diff   |< 30 s on 1 Gbps   |Delta bandwidth           |
|Push (small)              |Single commit, no large files|< 5 s              |3-5 S3 RTTs               |
|Push (large, mostly dedup)|10 GB file, 90% dedup        |< 2 min on 1 Gbps  |Chunk hashing CPU         |
|Push (large, no dedup)    |10 GB file, 0% dedup         |< 2 min on 1 Gbps  |Upload bandwidth          |
|Checkout (cached)         |Any                          |< 5 s              |Local disk I/O            |
|Checkout (cold)           |10 GB file                   |< 2 min on 1 Gbps  |Download bandwidth        |
|ls-refs                   |1000 refs                    |< 3 s              |S3 LIST latency           |

### 17.2 Critical Paths

**Clone**: bottleneck is serial object fetches. Parallelize everything:

- Refs: one LIST + parallel GETs (up to 50).
- Packs: parallel GETs.
- Smudge: parallel xorb GETs per file, multiple files in parallel via filter-delay.

**Push**: bottleneck depends on workload:

- Many small files: CPU (chunking) or S3 PUT latency.
- Few large files: upload bandwidth.
- No new content (just ref updates): S3 RTT for CAS sequence.

Optimizations:

- Chunking is CPU-bound; parallelize across files, SIMD-accelerated Gearhash where available.
- Xorb compression (zstd-3): pipelined with upload.
- Multipart uploads: chunk into 16 MiB parts, upload parts in parallel.

**Checkout**: bottleneck is xorb download when not cached. Prefetch based on filter-delay queue.

### 17.3 Memory Footprint

- Chunking: constant, ~1 MiB per active chunker.
- Chunk index: 40 bytes × unique chunks (see §10.3).
- Pack reading: mmap — uses address space but not RAM.
- Shard loading: all shards loaded to build index; bounded by shard cache size.

For a 1 TB repo with 16M chunks: ~640 MB RAM. For a 10 TB repo: ~6.4 GB RAM. Switch to on-disk index at some threshold.

### 17.4 Concurrency Model

crab is tokio-based. Key concurrency points:

- **Top-level operations**: single-threaded event loop is fine; work is IO-bound.
- **Chunking**: `tokio::task::spawn_blocking` onto a rayon pool, one chunker per CPU.
- **Compression**: same pool.
- **Upload/download**: tokio tasks with a `Semaphore` for concurrency limit.
- **gitoxide operations**: synchronous, wrapped in `spawn_blocking` per the gitoxide recommendation.

### 17.5 Throttling

S3 has per-prefix rate limits (3,500 PUT/s, 5,500 GET/s per partition). crab respects:

- Exponential backoff on 503 SlowDown responses.
- Client-side rate limiter (token bucket) per prefix, soft cap at 80% of AWS documented limits.
- Key sharding (§5.3) spreads writes across many partitions.

### 17.6 Cold-Start Overhead

The remote helper is spawned per git operation. Startup cost matters:

- Target: <50 ms from spawn to first useful work.
- Rust + small static binary: typical cold start ~10-30 ms.
- AWS SDK initialization is the long pole; cache resolved credentials in a child process memory where possible, though per-invocation re-init is unavoidable for security.

-----

## 18. Multi-Cloud & Multi-Region

### 18.1 Cloud Portability

All S3 interactions go through `object_store`, which supports:

- Amazon S3 (primary target)
- Google Cloud Storage
- Azure Blob Storage
- Cloudflare R2
- MinIO (self-hosted)
- Local filesystem (for testing)

Each backend’s conditional-write semantics differ slightly. crab abstracts these in a `CasOps` trait and provides backend-specific implementations where needed.

Feature matrix:

|Feature                        |S3 |GCS                      |Azure            |R2     |MinIO|
|-------------------------------|---|-------------------------|-----------------|-------|-----|
|Conditional PUT (If-None-Match)|Yes|Yes                      |Yes              |Yes    |Yes  |
|Conditional PUT (If-Match)     |Yes|Yes                      |Yes              |Yes    |Yes  |
|Strong read-after-write        |Yes|Yes                      |Yes              |Yes    |Yes  |
|Multipart upload               |Yes|Yes                      |Yes (block blobs)|Yes    |Yes  |
|Object tagging                 |Yes|Partial (custom metadata)|Yes (index tags) |No     |No   |
|Lifecycle rules                |Yes|Yes                      |Yes              |Partial|Yes  |

Features crab requires: conditional PUT (both modes), strong read-after-write, multipart upload. All five backends qualify for v1.

### 18.2 Multi-Region

A single bucket is regional. For multi-region use:

**Option A: Single-region, clients worldwide.** Simple. Clients pay egress for cross-region reads. For AI workloads with large artifacts, egress costs matter.

**Option B: Cross-region replication at the bucket level.** AWS S3 CRR, GCS Turbo Replication, and Azure Blob object replication keep the primary bucket replicated to regional secondaries. Crab V1 supports this as primary-write/read-replica: writes, locks, manifest CAS, GC, repair, lifecycle, and tier changes stay on the primary; read paths may use a replica only after the replica manifest and referenced packs, shards, and xorbs are present. During provider replication lag, reads fall back to the primary.

**Option C: Per-region repo mirrors with a promotion mechanism.** Outside v1 scope.

crab v1 officially supports Option A and Option B with primary-write semantics. Option C and active-active writes remain outside v1 because they require a separate distributed coordination and conflict-resolution design.

### 18.3 Edge Caching

CloudFront / GCP CDN / Azure Front Door in front of the bucket, read-only. Benefits:

- Xorbs and shards (immutable) cache trivially with long TTL.
- Packs (immutable) cache trivially.
- file-index entries (immutable) cache trivially.

Does NOT cache:

- Refs (mutable) — cache TTL would cause stale reads.
- Manifests (mutable) — same.

crab v1 doesn’t ship a CDN config, but documents how to layer one. Key: use a CDN that respects `Cache-Control: no-cache` on mutable paths, which S3 doesn’t set by default. Either configure origin headers or use a CDN function (Lambda@Edge) to rewrite caching behavior per path.

-----

## 19. Compatibility & Migration

### 19.1 Git-LFS Compatibility

Git-LFS is the incumbent solution for large files in Git. crab offers a migration path:

**Read-compatibility**: crab’s smudge filter detects LFS-format pointers (they have a distinctive `version https://git-lfs.github.com/spec/v1` header) and can fetch from the LFS server specified in `.lfsconfig`. This means a user migrating a repo can `git pull` from a crab remote and have LFS files work transparently during transition.

**Migration command**: `crab migrate --from lfs` walks repo history, for each commit that references LFS objects:

1. Fetch the LFS object via the LFS protocol.
1. Chunk it with CDC.
1. Generate a crab pointer.
1. Rewrite the commit’s tree to use the new pointer.
1. Push the result as a new history.

This rewrites history (changes commit SHAs) but preserves content. Users coordinate with collaborators to re-clone from the migrated remote.

**One-way migration** in v1. Back-migration to LFS is possible but not automated; crab can reproduce LFS objects by reassembling files, but the tooling to push them to an LFS server is user-provided.

### 19.2 Format Versioning

Every crab-managed object has a format version:

- Pointers: `version https://crab.dev/spec/v1` — bumped on breaking changes.
- `config` object: `version: 1`.
- Xorb/shard formats: inherited from xet-core’s versioning (currently `1`).

Compatibility policy:

- New crab clients read old formats: always.
- Old crab clients read new formats: best-effort; hard error with a clear “please upgrade” message if incompatible.
- crab clients write: always the newest format supported by the config’s `required_cli_version`.

### 19.3 Upgrading

Client upgrades are independent. A team with mixed v1.0 and v1.1 clients can continue working; v1.0 reads new data v1.1 writes, as long as no v1.1-only features are in use.

Format upgrades that require all clients to upgrade are rare and announced. The `config.required_cli_version` field prevents old clients from pushing incompatible data.

-----

## 20. Threat Model

### 20.1 Actors

- **User**: authorized reader/writer of a repo.
- **Malicious user**: authorized writer with bad intent.
- **Unauthorized party**: has no IAM credentials for the bucket.
- **Cloud operator**: has full access to the bucket (trusted by the cloud security model).

### 20.2 Assets

- Repo contents (confidentiality, integrity, availability).
- Access credentials (confidentiality).
- Cost (availability under DoS).

### 20.3 Attack Surfaces

**Surface 1: Network transport.** Assumed HTTPS, SigV4-signed. Compromise requires breaking TLS, which is out of crab’s scope.

**Surface 2: Client binary.** crab runs with the user’s credentials. A supply-chain attack on the binary gets those credentials. Mitigations:

- Signed releases (GitHub Release signatures, optionally minisign).
- Reproducible builds.
- Binary distribution via trusted channels (Homebrew, crates.io, winget).

**Surface 3: Bucket misconfiguration.** User misconfigures IAM and grants too much access. crab ships reference policies; user responsibility to apply them correctly.

**Surface 4: Authorized but malicious writer.** Can push bad content, force-push, delete refs (if DELETE not denied). Mitigations:

- IAM Deny on DeleteObject for write roles (§12.2 reference policy).
- Force-push prevention requires application-layer control; v1 provides documentation but not enforcement.
- Audit logging via CloudTrail.

**Surface 5: Compromised local cache.** An attacker with local machine access could inject content into the cache. Mitigations:

- Cache content is always hash-verified on read.
- Cache dir permissions: user-only (`0700`).

### 20.4 Denials of Service

- **Storage DoS**: malicious writer fills bucket with bogus data. Mitigation: S3 bucket-level size limits or cost alarms; user responsibility.
- **Request-rate DoS**: many clients hammering S3. Mitigation: S3 auto-scales; costs money but doesn’t crash.
- **Client resource DoS**: malicious repo contents designed to exhaust client memory (e.g., a shard referencing 10 million xorbs). Mitigation: `optimize` enforces caps of 1,000,000 canonical shards, 10,000,000 distinct source xorbs, 1,000,000 xorb references per source shard, and 512 MiB per source shard; the inventory and planner remain disk-backed/batched and fail closed before exceeding those limits.

### 20.5 Out of Scope

- Insider threats at the cloud provider.
- Physical attacks.
- Side-channel attacks on cryptographic operations.

-----

## 21. Cost Model

### 21.1 Storage Cost

S3 Standard: ~$0.023/GB/month (us-east-1, Apr 2026 prices approximate).

For a 1 TB AI repo with 10 revisions and 10% churn per revision:

- Git-LFS: 10 TB stored = $235/month.
- crab (chunk dedup): ~1.9 TB = $44/month.

Savings scale linearly with revision count and churn rate.

### 21.2 Request Cost

S3 PUT: $0.005 / 1,000 requests. S3 GET: $0.0004 / 1,000 requests.

Per push (small, just ref update):

- ~3-5 GETs, ~3-5 PUTs = ~$0.00005.

Per push (10 GB file, 90% dedup):

- ~100 PUTs (multipart parts) for new chunks’ xorbs.
- ~5 additional PUTs for shard, file-index, manifest updates.
- ~1 GET per cache check.
- Total: ~$0.0005.

Per clone (10 GB repo):

- ~N GETs for refs/packs/xorbs/shards.
- Dominated by bandwidth cost, not request cost.

Request costs are negligible compared to storage and bandwidth for AI workloads.

### 21.3 Bandwidth Cost

S3 egress: $0.09/GB (first 10 TB, us-east-1).

Cloning 100 GB repo: $9.
Pulling small update (1 GB delta): $0.09.

This is the dominant cost. Mitigations:

- Multi-region / edge caching for teams with geographic spread.
- VPC Endpoint (free egress) when users are on AWS.
- Intelligent-tiering for archival revisions (cheaper storage, occasional retrieval fees).

### 21.4 Cost Comparison Table

For a team of 10 engineers, pulling a 50 GB repo weekly, 4 pushes/week of ~5 GB changes each:

|Scenario                        |Git-LFS + S3|crab    |
|--------------------------------|------------|----------|
|Monthly storage (after 6 months)|$7,500      |$1,100    |
|Monthly egress (pulls)          |$180        |$180      |
|Monthly requests                |negligible  |negligible|
|**Total**                       |**$7,680**  |**$1,280**|

6× cost reduction driven by dedup.

### 21.5 Tiering

Old revisions can be moved to cheaper tiers via S3 Lifecycle:

- After 30 days: Standard-IA ($0.0125/GB).
- After 90 days: Glacier Instant Retrieval ($0.004/GB).
- After 365 days: Glacier Deep Archive ($0.00099/GB) — retrieval takes hours.

crab is aware of object class on read: if a xorb has been archived to Deep Archive, the read fails with a specific error and the user can initiate restoration via the cloud console or `crab restore`.

For git history (packs), lifecycle tiering makes history-traversal operations (`git blame`, deep `git log`) slow for old commits. Not a concern for most workflows.

-----

## 22. Engineering Plan

### 22.1 Milestones

Each milestone is feature-complete and demo-able. Ordering is chosen so that every milestone produces a useful artifact.

**M0 — Foundations (1 week)**

- Rust project scaffolding, CI, release automation.
- `object_store`-backed abstraction for storage ops.
- Local-filesystem backend for testing.
- Reference pack I/O via gix-pack (read existing, generate new).

**M1 — Bare Git over S3, no Xet (2-3 weeks)**

- `git-remote-crab` binary: capabilities, list, fetch, push.
- Ref storage with CAS.
- pack-list manifest.
- End-to-end: `git clone crab://localhost/repo` works on a local-filesystem backend.
- Goal: clone a small (Gitignore-sized) repo, make changes, push.

**M2 — S3 support (1 week)**

- Swap in AWS S3 backend.
- SigV4 auth, credential chain.
- Retry/backoff.
- Real-bucket E2E tests in CI.

**M3 — Xet plane (3-4 weeks)**

- Vendor xet-core primitives.
- Clean/smudge filter process.
- Pointer format, parsing, emission.
- Chunking, xorb building, shard generation.
- Upload path: chunks → xorbs → shards → file-index.
- Download path: pointer → shard → xorb ranges → file.
- Goal: push a 10 GB file, pull it, verify bit-identical.

**M4 — Dedup & caching (2-3 weeks)**

- Local shard cache.
- In-memory chunk index.
- Pre-push dedup check.
- Chunk cache with LRU.
- Parallel xorb fetches.
- Goal: push a 10 GB file with 90% already present; observe 90% upload reduction.

**M5 — Operational (2-3 weeks)**

- Garbage collection (manual, via CLI).
- `crab fsck`.
- Error taxonomy and user-facing messages.
- Tracing, metrics, `crab stat`.
- Crash recovery.
- Goal: production-quality operations.

**M6 — Compatibility & polish (2 weeks)**

- Multi-cloud (GCS, Azure, R2, MinIO).
- Git-LFS read compatibility.
- Migration command.
- Reference IAM policies.
- Documentation site.
- Goal: v1.0.0 release.

**M7 — Extended features (v1.x)**

- Server-side scheduled GC (Lambda template).
- Cross-repo dedup within bucket.
- Optional OTLP metrics.
- SHA-256 support.
- Signed commits.

### 22.2 Team Size Estimates

- Solo founder: ~6-9 months to v1.0 at full-time pace.
- Pair: ~4-5 months.
- Team of 4: ~3 months with appropriate parallelization (one on client UX, one on storage layer, one on xet integration, one on tooling/tests).

### 22.3 Release Strategy

- **Alpha** (M4): invite-only, users with AWS accounts who accept rough edges.
- **Beta** (M5): public, documented, feedback-gathering.
- **v1.0** (M6): production-ready.

Semver from v1.0: MAJOR for breaking on-disk formats (requires migration), MINOR for new features, PATCH for bug fixes.

-----

## 23. Testing Strategy

### 23.1 Test Pyramid

**Unit tests** (60% of effort): each module in isolation. Heavy focus on:

- Pack parsing correctness.
- Pointer format parsing.
- Chunk boundary determinism (same input → same chunks, always).
- CAS retry loop correctness.

**Integration tests** (30%): multi-component flows. Use the local-filesystem backend of `object_store` so tests don’t hit a real cloud.

- End-to-end push/pull roundtrip.
- Concurrent push to same ref (one succeeds, other retries).
- Crash recovery: kill process at every checkpoint, restart, verify consistency.
- Large file flow (10 GB synthetic file).
- Dedup flow (second push of same data uploads zero xorbs).

**E2E tests** (10%): real cloud. Run in CI on a schedule (not per-PR to keep costs manageable).

- Against real S3, GCS, MinIO.
- Verify real-world latency and throughput numbers.

### 23.2 Property-Based Tests

Using `proptest`:

- **Chunking determinism**: for any byte sequence, chunker produces the same chunks across runs.
- **Chunking locality**: inserting N bytes at position P affects at most ⌈(N + chunk_max) / chunk_min⌉ chunks.
- **Roundtrip**: for any byte sequence, clean → smudge produces identical output.
- **CAS convergence**: any sequence of concurrent CAS operations converges to a consistent state.

### 23.3 Fuzzing

Using `cargo-fuzz`:

- Pointer parsing (untrusted input from pack).
- Shard parsing.
- Xorb parsing.
- Pack parsing (delegated to gix-pack, which has its own fuzzing).

### 23.4 Chaos Testing

A test harness that wraps the local-filesystem backend with a chaos layer:

- Random latency injection.
- Random request failures.
- Random process kills mid-operation.

Scenario: “run 1000 random push/pull operations with 10% chaos; expect 100% final consistency.”

### 23.5 Compatibility Matrix

Per release, test against:

- Git versions: 2.30, 2.40, 2.45, 2.50+.
- Rust versions: MSRV (pinned in Cargo.toml, ~1.82) + stable + beta.
- OS: Linux (glibc, musl), macOS (x86_64, arm64), Windows.
- Cloud: S3, GCS, Azure, R2, MinIO.

### 23.6 Benchmarks

Using `criterion`:

- Chunking throughput (GB/s).
- Shard load time by size.
- Chunk index build time.
- Single-file push latency.

Regression detection on every PR.

-----

## 24. Open Questions

These are explicitly unresolved and need prototyping / discussion:

1. **Chunk threshold.** 1 MiB is a starting point. Too low: tiny files wastefully chunked; too high: missed dedup on medium files. Measure on real AI datasets.
1. **Shard granularity.** One shard per push is a clean choice, but large pushes (think: initial import of 1 TB dataset) produce a single massive shard. Consider splitting shards at ~10 MiB.
1. **On-disk chunk index sizing.** At what repo size does the in-memory tier become prohibitive? Probably ~5 TB based on 40 bytes/chunk. Benchmark SQLite batch size, WAL checkpoint policy, and cache-page sizing before adding any new storage engine.
1. **Multipart upload resume.** S3 multipart uploads can be resumed if the upload ID is known. crab should persist upload IDs to survive laptop-closes-lid scenarios on big uploads.
1. **Ref prefetch on list.** Clone currently issues N GETs for N refs after the LIST. Could use `ListObjectsV2` with a manifest object storing all ref contents together for single-request ref listing. Tradeoff: manifest update cost vs clone latency.
1. **How aggressive should the dedup query be?** For repos where 90% of content dedupes, scanning all shards is free. For repos where 1% dedupes, it’s overhead. Auto-tune based on observed hit rate.
1. **Handling partial quota / rate-limit errors mid-multipart.** If we’re halfway through a multipart upload and get throttled, we can pause and resume, but the multipart itself has a timeout. Need a watchdog.
1. **SHA-256 migration.** Git supports SHA-256 object format but most tooling doesn’t. When crab flips to default SHA-256, older Git clients break. Probably wait until Git ecosystem stabilizes (post-2027?).
1. **Submodule handling.** Gitmodules pointing to crab URLs should work transparently; verify this with the actual protocol.
1. **Partial clones.** The proof-gated client-side protocol-v2 profile supports
   `blob:none`, `blob:limit=<n>[kmg]`, `tree:<depth>`,
   `object:type={tag,commit,tree,blob}`, full-SHA-1 `sparse:oid`, and bounded
   repeated/combine intersections. It omits only the objects selected by the
   requested filter, lets Git own promisor pack installation, and serves later
   raw-OID requests from the immutable visibility proof. The `crab clone`
   wrapper still uses its separate lazy pointer-checkout policy. Date/ref
   shallow selectors, stateful `connect`, `packfile-uris`, `object-info`,
   `ref-in-want`, `sparse:path`, and other unlisted filters remain unsupported;
   an incomplete proof never falls back to a complete filtered clone.

-----

## 25. Glossary

- **Blob**: Git’s term for file content; a Git object type.
- **Bucket**: top-level container in object storage.
- **CAS (compare-and-swap)**: an atomic update operation that succeeds only if a precondition on the current state holds.
- **CAS (Content-Addressable Storage)**: a storage paradigm where objects are named by their content hash.
- **CDC (Content-Defined Chunking)**: splitting data into chunks whose boundaries are determined by content via a rolling hash.
- **Chunk**: a variable-sized byte slice, ~64 KiB, the unit of dedup in crab.
- **Clean filter**: a Git filter that transforms file content before storing in the object database (used to emit pointers).
- **Commit**: a Git object type representing a snapshot plus metadata.
- **Delta**: a compressed representation of a Git object as a diff against a base object.
- **Dedup**: deduplication; storing identical content only once.
- **ETag**: an HTTP header representing an object version, used by S3 for CAS.
- **File-hash**: blake3 hash of a file’s full content, used as the key in file-index.
- **Gearhash**: a rolling hash function used by Xet for CDC.
- **GC**: garbage collection.
- **git-remote-helper**: a Git protocol for custom remote backends via subprocess.
- **gitoxide / gix**: pure-Rust implementation of Git.
- **Helper**: shorthand for the `git-remote-crab` binary.
- **IAM**: cloud identity and access management.
- **MerkleHash**: Xet’s hash type; 256 bits, computed as Merkle-tree hash.
- **Multipart upload**: S3 feature for uploading large objects in parts.
- **object_store**: Rust crate abstracting multiple cloud object stores.
- **Pack**: a Git file format aggregating many objects with delta compression.
- **Pointer**: a small text blob replacing a large file’s content in Git, references xet storage.
- **Push manifest**: an object describing an in-flight multi-ref push.
- **Ref**: a named pointer to a commit (e.g., `refs/heads/main`).
- **Shard**: a Xet object describing file reconstruction and chunk-to-xorb mapping.
- **SHA-1 / SHA-256**: cryptographic hashes used by Git.
- **Smudge filter**: a Git filter that transforms object-db content when checking out (used to materialize pointers).
- **STS**: AWS Security Token Service, for short-lived credentials.
- **Xet**: HuggingFace’s chunk-based storage protocol.
- **Xorb**: a Xet object aggregating chunks, ~64 MiB, content-addressed.
- **zstd**: a compression algorithm used for xorb chunks.

-----

## 26. References

### 26.1 Specifications

- Git remote helper protocol: `git help remote-helpers`
- Git filter protocol: `git help attributes` section “Long Running Filter Process”
- Git pack format: `Documentation/gitformat-pack.txt` in the Git source tree.
- Xet specification: https://huggingface.co/docs/xet
- AWS S3 conditional writes: https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-requests.html

### 26.2 Source Repositories

- gitoxide: https://github.com/GitoxideLabs/gitoxide
- xet-core: https://github.com/huggingface/xet-core
- object_store: https://github.com/apache/arrow-rs-object-store
- slatedb: https://github.com/slatedb/slatedb (not used in v1, but referenced)

### 26.3 Related Systems

- Git-LFS: https://git-lfs.com/
- JGit DFS: https://github.com/eclipse-jgit/jgit (reference for pluggable storage)
- lakeFS: https://lakefs.io/ (data-versioning on object storage, different domain)
- Apache Iceberg: https://iceberg.apache.org/ (table format with CAS-based metadata)

### 26.4 Academic

- Rabin, M. “Fingerprinting by Random Polynomials.” 1981. (Rolling hash foundation.)
- Xia et al. “Gearhash: a parallel fingerprint algorithm…” 2018.
- Muthitacharoen et al. “A Low-bandwidth Network File System.” SOSP 2001. (CDC in LBFS.)

-----

**End of document.**
