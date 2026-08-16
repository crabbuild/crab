# crab-diff

`crab-diff` compares Crab-managed files by their ordered chunk content rather
than by storage placement. It produces structured reports for CLI output,
machine-readable APIs, and transfer-cost estimates.

## Why it exists

A file can be repacked into different Xorbs without changing its content, and
two versions can share most bytes even when their file hashes differ. A raw
Git diff or an Xorb-location comparison cannot answer those questions. This
crate keeps the comparison pure and reusable: no object-store access, cache
policy, or orchestration is hidden inside the algorithm.

## Architecture

```text
old/new pointer maps ──► pair_files
                                  │
old/new chunk sequences ──────────┤
                                  ▼
                 LCS / large-input comparison
                                  │
                                  ▼
                 ChunkDiffReport + metrics/ranges
```

The main entry points are:

- `pair_files` — sorted added/modified/deleted file pairs, skipping identical
  file hashes;
- `ChunkSequence::from_staged` — build a sequence from local chunk hashes and
  sizes;
- `compare_sequences` — compare exact ordered chunk hashes;
- `compare_terms` — compare reconstruction terms when only Xorb ranges are
  available.

For small term lists, `compare_terms` uses LCS to preserve moved-segment
matches. Above its bounded ceiling it uses set-based classification to keep
memory linear. Reports include unchanged/added/removed bytes, dedup ratio,
changed ranges, and optional segment details.

## Usage

```rust
use crab_diff::{ChunkSequence, compare_sequences};
use crab_xet::hash::compute_data_hash;

let shared = compute_data_hash(b"shared");
let old = ChunkSequence::from_staged(
    compute_data_hash(b"old-file"),
    6,
    &[(shared, 6)],
);
let new = ChunkSequence::from_staged(
    compute_data_hash(b"new-file"),
    6,
    &[(shared, 6)],
);

let report = compare_sequences("model.bin", &old, &new);
assert_eq!(report.unchanged_bytes, 6);
```

The example uses a deliberately small synthetic sequence. Production callers
normally obtain sequences from staging, committed metadata, or a worktree and
then serialize the returned report with its `serde` implementation.

## Boundaries

`crab-diff` knows the Xet chunk and term models, but it does not fetch them.
[`crab-read`](../crab-read/README.md) and the product diff command assemble the input;
this crate only decides how the input relates.
