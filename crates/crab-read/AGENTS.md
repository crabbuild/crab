# crab-read

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-read/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns read selection, fetch admission, term resolution, and verified hydration. VFS and product commands compose this path; storage/cache crates retain transport and cache mechanics.

## Read first

1. `crates/crab-read/src/lib.rs` — public admission and reconstruction APIs.
2. `crates/crab-read/src/selection.rs` — `select_read_replicas / ReadRoutingPolicy`: replica readiness and routing decisions.
3. `crates/crab-read/src/hydrator.rs` — `ShardHydrator / ReadRuntimeBuilder`: whole-file versus range reconstruction.
4. `crates/crab-read/src/term_resolver.rs` — `TermResolver::resolve_batch`: file-index/shard lookup and session closure.
5. `crates/crab-read/src/store_client.rs` — `StoreClient`: cache-aware object reads.

Trace one path: `crates/crab-vfs/src/hydration.rs` →
`ShardHydrator::reconstruct_range_from_pointer` in
`crates/crab-read/src/hydrator.rs` → read-side store/metadata resolution.
For object download changes continue into `crates/crab-read/src/store_client.rs`.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Hydration or range integrity | `crates/crab-read/src/hydrator.rs` | `crates/crab-vfs/src/hydration.rs` |
| Fetch authorization | `crates/crab-read/src/fetch_admission.rs` | `crab/src/git/remote_helper.rs` |
| Term resolution | `crates/crab-read/src/term_resolver.rs` | `crates/crab-metadata/src/file_index_lookup.rs` |

## Invariants

- Requests naming hidden refs remain rejected even when arbitrary wants are enabled. This does not imply that the same SHA is denied under every other name; inspect policy and advertisement together.
  Source: `crates/crab-read/src/fetch_admission.rs`.
- Preserve the difference between full-file hash verification and range/chunk verification; partial output is not proof of the complete file.
  Source: `crates/crab-read/src/hydrator.rs`.
- Close file-index lookup sessions after successful, failed, and cancelled resolution; follow the explicit close helper when changing batching.
  Source: `crates/crab-read/src/term_resolver.rs`.

## Features and platform

No declared Cargo features. The manifest explicitly enables dependency cache/index capabilities. Hydration fixtures cannot prove live replica selection or mounted read behavior; qualify those affected consumers separately.

## Verification

Inline `crates/crab-read/src/fetch_admission.rs` tests cover hidden-ref and non-tip rejection; hydration tests live with `crates/crab-read/src/hydrator.rs` and its adjacent modules.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-read --locked --lib fetch_admission::tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-read --locked --lib hydrator
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-read/README.md` — usage and detailed contracts.
- `crates/crab-read/Cargo.toml` — dependency and feature authority.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
