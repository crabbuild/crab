# Chunk-Level Diff Engine

## Overview

The diff engine compares crab-tracked files between git refs using only
metadata (file-index + shards). It produces per-file reports of which chunks
changed, bytes affected, and reuse ratio — all with zero data transfer. No
actual file content is downloaded.

Source: `crab/src/diff/`

## Architecture

```
crab diff HEAD~1 HEAD
    │
    ▼
Ref Resolver (ref_resolver.rs)
    │  Resolve git refs → tree objects → pointer lists
    ▼
Chunk Sequence Resolver (term_resolver.rs)
    │  For each changed pointer:
    │    file_hash → file-index → shard → reconstruction terms → xorb chunks
    │    falls back to xorb object footer metadata when shard xorb-info is sparse
    ▼
Chunk Sequence Comparator (chunk_sequence.rs)
    │  Compare old chunk hashes vs new chunk hashes
    │  Compute added/removed/shared chunks
    ▼
Format Hints (format_hint.rs)
    │  Annotate changes with format-aware context
    ▼
Formatter (formatter.rs)
    │  Render output (human/JSON/stat/name-only)
    ▼
stdout
```

## Components

### Ref Resolver (`ref_resolver.rs`)

Resolves git refs to lists of crab pointer files:

1. Parse ref strings (branch, tag, SHA, `HEAD~N`)
2. Resolve to commit objects via gitoxide
3. Walk both trees to find crab-tracked files
4. Diff the two pointer lists to identify changed files

### Chunk Sequence Resolver (`term_resolver.rs`)

For each changed file, resolves the pointer's file hash to an ordered chunk
sequence:

1. Extract `(file_hash, shard_hint)` from the pointer
2. Look up file-index: `file_hash → shard_hash`
3. Download shard (from cache or remote)
4. Parse shard to extract reconstruction terms for this file
5. Follow each term's xorb hash to xorb metadata in the same shard
6. If shard xorb-info cannot satisfy the term range, read only the xorb object's
   footer metadata and ordered chunk table
7. Expand the term's chunk range into ordered `(chunk_hash, offset, size)` spans

The resolver processes files in batches with configurable download concurrency
to minimize latency.

### Chunk Sequence Comparator (`chunk_sequence.rs`)

Compares two ordered chunk-hash sequences to produce a diff report:

```
Old chunks: [a, b, c, d, e]
New chunks: [a, b, X, d, e]

Result:
  Shared:  a, b, d, e
  Removed: c
  Added:   X
  
  Chunks changed: 2 (+1 added, -1 removed)
  Delta bytes: +500 KiB
  Reuse ratio: 75%
```

The comparison key is the chunk hash, not the xorb hash or xorb-local chunk
range. This matters because xet-style dedup can repack identical file content
into different xorbs across pushes. Two chunks are equal when their chunk hashes
match, even if their xorb origin differs.

For normal inputs the comparator uses an exact sequence comparison. Highly
repetitive pathological inputs are bounded to avoid unbounded memory growth; in
that case the fallback still compares chunk hashes in file order and reports a
reuse-preserving diff.

### Format Hints (`format_hint.rs`)

Provides format-aware annotations for diff output. For known file formats
(SafeTensors, ONNX, HDF5), the diff can annotate which logical section of
the file changed (e.g., "tensor weights layer 12" vs "metadata header").

Disabled with `--no-annotations`.

### Formatter (`formatter.rs`)

Renders diff output in multiple modes:

| Mode | Flag | Description |
|------|------|-------------|
| Human | (default) | Colored, formatted with chunk counts and sizes |
| Stat | `--stat` | One-line-per-file summary |
| Name-only | `--name-only` | Just file paths |
| Verbose | `--verbose` | Full segment detail with xorb hashes |
| JSON | `--json` | Machine-readable for CI/tooling |

### Types (`types.rs`)

Core data types:

```rust
struct FileDiffReport {
    path: String,
    old_size: u64,
    new_size: u64,
    added_segments: usize,
    removed_segments: usize,
    shared_segments: usize,
    delta_bytes: i64,
    dedup_ratio: f64,
    chunk_metrics: Option<ChunkDiffMetrics>,
    // Optional: per-segment detail, byte ranges
}

struct ChunkDiffMetrics {
    old_source: ChunkSequenceSourceKind,
    new_source: ChunkSequenceSourceKind,
    old_chunks: u32,
    new_chunks: u32,
    unchanged_chunks: u32,
    removed_chunks: u32,
    added_chunks: u32,
    old_bytes: u64,
    new_bytes: u64,
    unchanged_bytes: u64,
    removed_bytes: u64,
    added_bytes: u64,
    signed_delta_bytes: i64,
    reuse_ratio: f64,
    changed_byte_ranges_old: Vec<(u64, u64)>,
    changed_byte_ranges_new: Vec<(u64, u64)>,
}

struct DiffSummary {
    files_changed: usize,
    total_segments_changed: usize,
    total_delta_bytes: i64,
}

enum OutputMode { Human, Json, Stat, NameOnly }
```

## Performance

The diff engine's key performance property is that it never downloads file
content. All comparisons operate on metadata (shards, file-index entries, and
when needed xorb object footer metadata), which are typically kilobytes to
megabytes even for terabyte-scale files.

| Operation | Data Transfer |
|-----------|--------------|
| Resolve refs | None (local git) |
| Fetch file-index entries | ~40 bytes per changed file |
| Fetch shards | ~1-10 KiB per shard |
| Fetch xorb footer metadata | Footer + metadata table only, no payload chunks |
| Compare chunk sequences | None (in-memory) |

A diff of two versions of a 100 GB file downloads ~10 KiB of metadata.

## Diff Driver Integration

The `crab diff-driver` command implements git's external diff driver protocol,
enabling `git diff` to automatically use chunk-level comparison for
crab-tracked files. Pointer-vs-pointer diffs use the same chunk sequence
comparator internally.

For staged but unpushed files, the new pointer may not exist in remote metadata
yet. In that case `diff-driver` reads the local staging index through a
read-only handle and builds the new-side sequence from ordered `(chunk_hash,
size)` rows. Mixed pointer/content diffs still use a labelled size-only
fallback until a read-only worktree chunker is wired.

JSON output keeps the existing report fields and adds `chunk_metrics` when the
canonical chunk-sequence comparator was used. Existing consumers can continue
reading `added_segments`, `removed_segments`, `delta_bytes`, and
`changed_byte_ranges`; new consumers should prefer `chunk_metrics`.

Source: `crab/src/cmd/diff_driver.rs`
