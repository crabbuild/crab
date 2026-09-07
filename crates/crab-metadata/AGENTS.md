# crab-metadata

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-metadata/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns metadata schemas, codecs, manifests, visibility evidence, and feature-gated indexes. Storage owns transport; crab-write composes publication and caller-held authority.

## Read first

1. `crates/crab-metadata/src/lib.rs` — payload-only versus persistence exports.
2. `crates/crab-metadata/src/manifests.rs` — `Manifest / validate_manifest_payload`: validation and serialized roots.
3. `crates/crab-metadata/src/manifest_store.rs` — `read_repository_snapshot / RepositorySnapshot`: stored snapshots and CAS.
4. `crates/crab-metadata/src/ref_journal.rs` — `commit_ref_transaction / RefJournalSnapshot`: committed transaction overlay.
5. `crates/crab-metadata/src/git_visibility.rs` — `GitVisibilityIndex / GitCatalogVisibilityIndex`: generation-bound authorization evidence.

Trace one path: `crates/crab-write/src/journal.rs` → `commit_ref_transaction` in
`crates/crab-metadata/src/ref_journal.rs` → bounded marker write/readback on
`Store` in `crates/crab-storage/src/store.rs`.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Manifest/journal change | `crates/crab-metadata/src/manifest_store.rs` | `crates/crab-write/src/journal.rs` |
| Catalog coverage | `crates/crab-metadata/src/git_object_locator` | `crates/crab-remote-git/src/repository.rs` |
| Local dedup index | `crates/crab-metadata/src/persistent_chunk_index.rs` | `crates/crab-staging/src/index.rs` |

## Invariants

- Validate manifest payloads before trusting refs and bulk index pointers; preserve the distinction between compacted manifest and journal-overlay snapshot.
  Source: `crates/crab-metadata/src/manifest_store.rs`.
- Pack presence alone does not authorize object visibility; inspect closure evidence and its generation binding together.
  Source: `crates/crab-metadata/src/git_visibility.rs`.
- Remote index writers reject entries for unopened indexes before buffering either batch. A successful write is not durable until close.
  Source: `crates/crab-metadata/src/remote_index.rs`.
- Shared lookup initialization must not serialize its first lookup. Both point
  and batch reads keep shared access until completion; close takes exclusive
  access and rejects new work. Keep exclusive access through reader cleanup so
  concurrent close calls cannot finish early. Use SharedFileIndexLookup.
  Source: `crates/crab-metadata/src/file_index_lookup.rs`.
- Keep explicit close ownership when adding index readers/writers; inspect error and cancellation paths as well as successful reads.
  Source: `crates/crab-metadata/src/git_object_locator`.

## Features and platform

Empty default. `storage` adds object-store helpers; `file-index-reader` and `remote-index` also enable storage and SlateDB paths; `local-index` adds SQLite. Inspect locked SlateDB session/writer close contracts before changing lifecycle. Payload-only checks must remain a separate slice.

## Verification

Inline manifest-store/visibility tests cover stale proofs and malformed roots. `crates/crab-metadata/src/file_index_lookup.rs` and `crates/crab-metadata/src/persistent_chunk_index.rs` have feature-gated lookup tests.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-metadata --locked --no-default-features --lib
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-metadata --locked --lib --features storage,remote-index git_visibility
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-metadata --locked --lib --features file-index-reader file_index_lookup
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-metadata --locked --lib --features local-index persistent_chunk_index
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-metadata/README.md` — usage and detailed contracts.
- `crates/crab-metadata/Cargo.toml` — dependency and feature authority.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
