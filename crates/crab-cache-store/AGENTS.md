# crab-cache-store

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-cache-store/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns the canonical local-cache/service/origin composition behind CachingStore. crab-cache owns local storage and client contracts; crab-storage owns authoritative transport and retries.

## Read first

1. `crates/crab-cache-store/src/lib.rs` — `CachingStore / CacheStoreError`: ObjectStore adapter, path routing, and error conversions.
2. `crates/crab-cache-store/src/xorb_read.rs` — `get_xorb_chunks / xorb_chunk_metadata`: source-specific verified xorb plans and repair.

3. `crates/crab-cache/src/path_class.rs` — `classify_path`: shared mutable/immutable admission consumed by the adapter.

Trace one path: `StoreClient` in `crates/crab-read/src/store_client.rs` →
`get_xorb_chunks_without_install` in `crates/crab-cache-store/src/xorb_read.rs`
→ parser/range verification in `crates/crab-xet/src/xorb/parser.rs`.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Read routing or ETags | `crates/crab-cache-store/src/lib.rs` | `crates/crab-read/src/store_client.rs` |
| Xorb repair | `crates/crab-cache-store/src/xorb_read.rs` | `crates/crab-xet/src/xorb/parser.rs` |
| Remote service behavior | `crates/crab-cache-store/src/lib.rs` | `crates/crab-cache/src/cache_client.rs` |

## Invariants

- Mutable paths and HEAD operations must retain origin authority; inspect bypass tests before extending cache admission.
  Source: `crates/crab-cache-store/src/lib.rs`.
- One xorb attempt uses one source; retrying cached corruption must not combine cache metadata with origin payload bytes.
  Source: `crates/crab-cache-store/src/xorb_read.rs`.
- Origin corruption is a typed integrity failure with its source retained; optional cache eviction failure must not prevent trying origin.
  Source: `crates/crab-cache-store/src/xorb_read.rs`.

## Features and platform

Empty default. `remote-client` enables crab-cache remote support; local-cache dependencies remain enabled without it. Remote tests may start loopback service fixtures. Do not equate these fixtures with live cache-service qualification.

## Verification

Tests are inline in `crates/crab-cache-store/src/lib.rs`, including mutable-path bypass, corrupt-origin provenance, and repair when cache eviction fails.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-cache-store --locked --lib --no-default-features xorb
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-cache-store --locked --lib --no-default-features --features remote-client xorb
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-cache-store --locked --lib mutable_path_bypasses_cache_even_when_configured
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-cache-store/README.md` — usage and detailed contracts.
- `crates/crab-cache-store/Cargo.toml` — dependency and feature authority.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
