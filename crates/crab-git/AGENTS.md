# crab-git

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-git/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns Git discovery, refs, pointers, packs, and bounded protocol mechanics. Transport authorization and publication policy belong to callers; LFS payload storage belongs to crab-lfs.

## Read first

1. `crates/crab-git/src/lib.rs` — public mechanics and optional facade.
2. `crates/crab-git/src/discover.rs` — `discover_git_dir_from / resolve_common_dir`: Git directory and common-directory discovery.
3. `crates/crab-git/src/receive_plan.rs` — `validate / GraphSource`: ref comparisons and graph validation.
4. `crates/crab-git/src/pack.rs` — `verify_pack_sha1 / install_pack_file_from_path`: pack/index integrity and installation.
5. `crates/crab-git/src/refname.rs` — `validate_push_refname / validate_ref_namespace`: individual names versus final namespace validation.

Trace one path: `crates/crab-http-server/src/receive/validate.rs` → `validate` in
`crates/crab-git/src/receive_plan.rs` → `validate_ref_namespace` in
`crates/crab-git/src/refname.rs`. Publication remains above this chain.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Native receive validation | `crates/crab-git/src/receive_plan.rs` | `crates/crab-http-server/src/receive/validate.rs` |
| Pack decoding | `crates/crab-git/src/incoming_pack.rs` | `crates/crab-remote-git/src/reader.rs` |
| Pointer classification | `crates/crab-git/src/pointer_detect.rs` | `crates/crab-types/src/pointer.rs` |

## Invariants

- Validate the final ref namespace after edits; validate individual names separately. A parent ref cannot coexist with its slash-delimited descendants.
  Source: `crates/crab-git/src/refname.rs`.
- A receive plan is not publication authority: callers supply policy, verify pointer dependencies, and recheck the captured base under writer leases.
  Source: `crates/crab-git/src/receive_plan.rs`.
- Keep common-directory discovery distinct from the current working tree; discovery has an explicit .git result outside repositories.
  Source: `crates/crab-git/src/discover.rs`.

## Features and platform

Only `facade` is declared; there is no declared default feature. It enables the optional high-level gix dependency. Git-spawning tests need Git on PATH; inspect locked gix source before altering its validation or discovery contract.

## Verification

Read inline discovery/ref tests and `crates/crab-git/src/receive_plan/tests.rs`; pack quarantine regressions live in `crates/crab-git/src/incoming_pack/tests.rs`.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-git --locked --lib receive_plan::tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-git --locked --lib discover::tests
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-git/README.md` — usage and detailed contracts.
- `crates/crab-git/Cargo.toml` — dependency and feature authority.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
