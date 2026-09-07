# crab-diff

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-diff/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns pure file pairing and chunk/term comparison. Callers gather staging or remote inputs and render results; this crate performs no storage fetch or hydration.

## Read first

1. `crates/crab-diff/src/lib.rs` — public comparison entry points.
2. `crates/crab-diff/src/types.rs` — `ChunkDiffReport / ChunkDiffMetrics`: report fields and byte metrics.
3. `crates/crab-diff/src/chunk_sequence.rs` — `compare_sequences / ChunkSequence`: ordered hash matching and bounded matching work.
4. `crates/crab-diff/src/chunk_comparator.rs` — `compare_terms`: term-level comparison.
5. `crates/crab-diff/src/pointer_pairs.rs` — `pair_files`: sorted file pairing.

Trace one path: `crab/src/cmd/diff.rs` → `compare_sequences` in
`crates/crab-diff/src/chunk_sequence.rs` → its matching/metric helpers
(`lcs_matches`, `build_metrics`), then reports from `crates/crab-diff/src/types.rs`.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Ordered chunk diff | `crates/crab-diff/src/chunk_sequence.rs` | `crates/crab-read/src/term_resolver.rs` |
| File pairing or output shape | `crates/crab-diff/src/pointer_pairs.rs` | `crab/src/cmd/diff.rs` |

## Invariants

- Compare ordered content hashes independently of xorb placement; repeated chunks must retain occurrence and range semantics.
  Source: `crates/crab-diff/src/chunk_sequence.rs`.
- Keep the bounded large-input path in view when changing exact matching; small examples do not qualify large-input complexity.
  Source: `crates/crab-diff/src/chunk_sequence.rs`.
- Pair files deterministically and skip unchanged file hashes; do not add storage reads to resolve missing comparison input.
  Source: `crates/crab-diff/src/pointer_pairs.rs`.

## Features and platform

No declared Cargo features.

## Verification

Inline `crates/crab-diff/src/chunk_sequence.rs` tests include repeated chunk order and replacement ranges; pairing tests are inline in `crates/crab-diff/src/pointer_pairs.rs`.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-diff --locked --lib chunk_sequence::tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-diff --locked --lib pointer_pairs::tests
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-diff/README.md` — usage and detailed contracts.
- `crates/crab-diff/Cargo.toml` — dependency and feature authority.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
