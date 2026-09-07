# crab-xet

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-xet/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns chunk/hash, Xorb, shard, and reconstruction mechanics. Staging owns local durability; storage and higher layers own transport and publication.

## Read first

1. `crates/crab-xet/src/lib.rs` — format modules and feature gates.
2. `crates/crab-xet/src/hash.rs` — `merkle_hex_from_bytes`: data/file hash conversions.
3. `crates/crab-xet/src/xorb/builder.rs` — `XorbBuilder`: container construction and placements.
4. `crates/crab-xet/src/reconstruction.rs` — `FileTermBuilder / validate_term_coverage`: ordered recipe occurrences and term coverage.
5. `crates/crab-xet/src/shard.rs` — `ShardWriter / ShardReader`: file/xorb metadata assembly.

Trace one path: `crab/src/git/push.rs` (`build_file_terms`) → `build_file_terms` in
`crates/crab-xet/src/reconstruction.rs` → `FileTermBuilder::push` and `finish`.
Use the ordered recipe tests when changing this chain.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Recipe coverage | `crates/crab-xet/src/reconstruction.rs` | `crates/crab-staging/src/recipe.rs` |
| Xorb verification | `crates/crab-xet/src/xorb/parser.rs` | `crates/crab-cache-store/src/xorb_read.rs` |

## Invariants

- Consume every ordered recipe occurrence, including duplicates; missing placements and count mismatches must remain errors.
  Source: `crates/crab-xet/src/reconstruction.rs`.
- Term coverage counts are not complete payload integrity proof; retain parser digest/chunk checks and final read-side file verification.
  Source: `crates/crab-xet/src/xorb/parser.rs`.
- Check representable format sizes before changing builder state; do not silently truncate serialized chunk metadata.
  Source: `crates/crab-xet/src/xorb/builder.rs`.

## Features and platform

Empty default. `chunker` enables xet-data; `upload-concurrency` enables Tokio and Xet client/runtime admission. Inspect the locked xet-core-structures/xet-data source and Cargo.lock before altering hashes or serialized formats; do not infer format guarantees from wrapper names.

## Verification

Inline reconstruction tests cover missing placements and duplicate occurrences; builder/parser tests cover overflow and corrupted payloads.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-xet --locked --lib reconstruction::tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-xet --locked --lib --features chunker chunker
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-xet --locked --lib --features upload-concurrency upload_concurrency
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-xet/README.md` — usage and detailed contracts.
- `crates/crab-xet/Cargo.toml` — dependency and feature authority.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
