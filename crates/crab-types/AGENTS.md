# crab-types

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-types/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns shared serialized contracts and provider identity; keep runtime, storage transport, and product decisions in their consuming crates.

## Read first

1. `crates/crab-types/src/lib.rs` — exported contract families.
2. `crates/crab-types/src/pointer.rs` — `Pointer::parse / serialize / is_pointer`: wire serialization, parsing, and the separate detection heuristic.
3. `crates/crab-types/src/storage.rs` — `BucketIdentity / StorageScope`: provider aliases, bucket identity, and issued scopes.
4. `crates/crab-types/src/error.rs` — `ErrorCategory`: cross-crate error categories.

Trace one path: `crates/crab-git/src/pointer_detect.rs` (`classify`) →
`Pointer::parse` in `crates/crab-types/src/pointer.rs` → its `parse_hex32`
validation. Inspect detection and parse tests together.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Pointer format | `crates/crab-types/src/pointer.rs` | `crates/crab-git/src/pointer_detect.rs` |
| Timestamp formatting | `crates/crab-types/src/time.rs` | CLI output, `crab-write` journal, `crab-workflow` executor, auth-server manifests, and VFS serializers |
| Storage identity or scope | `crates/crab-types/src/storage.rs` | `crates/crab-storage/src/identity.rs` |

## Invariants

- Keep parsing and heuristic detection distinct; detection is not a substitute for parsing. Preserve canonical serialization and investigate tagged consumers before changing accepted versions.
  Source: `crates/crab-types/src/pointer.rs`.
- Bucket normalization and provider distinctions participate in identity; do not replace them with display URL equality.
  Source: `crates/crab-types/src/storage.rs`.
- Cloud alias parsing deliberately excludes local/file aliases; callers must opt into local storage.
  Source: `crates/crab-types/src/storage.rs`.
- Timestamp formatters reject pre-epoch clocks and dates beyond year 9999. Keep
  errors typed; callers own cleanup and diagnostic-failure policy.
  Source: `crates/crab-types/src/time.rs`.

## Features and platform

No declared Cargo features.

## Verification

Inline tests in `crates/crab-types/src/pointer.rs` cover round trips and detection; `crates/crab-types/src/storage.rs` covers normalization and provider distinctions.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-types --locked --lib pointer::tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-types --locked --lib storage::tests
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-types/README.md` — usage and detailed contracts.
- `crates/crab-types/Cargo.toml` — dependency and feature authority.

Use `CONTEXT.md` terminology: a worktree includes Git metadata; a working tree means checked-out files.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
