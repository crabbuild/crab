# crab-remote-git

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-remote-git/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns bounded filesystem-free Git reads from committed manifests, catalogs, and immutable packs. Caller authorization and resolved placement identity precede repository opening; logical Xet hydration belongs to crab-read.

## Read first

1. `crates/crab-remote-git/src/lib.rs` — supported public API.
2. `crates/crab-remote-git/src/repository.rs` — `RemoteGitRepository::open / RepositoryIdentity`: identity and opening handshake.
3. `crates/crab-remote-git/src/snapshot.rs` — `RemoteGitSnapshot`: pinned commit/tree operations.
4. `crates/crab-remote-git/src/tree_listing.rs` — bounded prefix/continuation blob pages in canonical Git path order.
5. `crates/crab-remote-git/src/operation.rs` — `OperationContext::finish`: budgets, cancellation, and explicit finish.
6. `crates/crab-remote-git/src/reader.rs` — `RemoteGitReader::read_with_session`: object lookup and verified decoding.

Trace one path: HTTP repository opening in `crates/crab-http-server/src/server.rs`
→ `RemoteGitRepository::open` in `crates/crab-remote-git/src/repository.rs`
→ manifest/catalog acquisition from `crates/crab-metadata/src/manifest_store.rs`
and `crates/crab-metadata/src/git_object_locator`.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Repository consistency | `crates/crab-remote-git/src/repository.rs` | `crates/crab-http-server/src/server.rs` |
| Object range/decode | `crates/crab-remote-git/src/reader.rs` | `crates/crab-remote-git/src/pack.rs` |
| Bounded recursive listing | `crates/crab-remote-git/src/tree_listing.rs` | `crates/crab-s3-gateway/src/gateway.rs` |
| Operation lifetime | `crates/crab-remote-git/src/operation.rs` | `crates/crab-remote-git/src/runtime.rs` |

## Invariants

- Keep repository identity and generation/catalog coverage checks in the open path; locator presence alone is not a visibility proof.
  Source: `crates/crab-remote-git/src/repository.rs`.
- Finish each OperationContext with its semantic result so the locator session closes and close failures remain observable. Runtime shutdown is a separate process-lifecycle obligation.
  Source: `crates/crab-remote-git/src/operation.rs`.
- Preserve operation work budgets and cancellation through object lookup/decode, including cache hits; individual request limits do not bound aggregate work.
  Source: `crates/crab-remote-git/src/operation.rs`.
- Preserve complete-path byte ordering and exclusive continuation when seeking recursive blob pages; an S3-sized page must not traverse or hydrate the complete repository.
  Source: `crates/crab-remote-git/src/tree_listing.rs`.

- Range coalescing limits merging, not individual entry admission. Keep per-entry ReaderLimits and aggregate OperationBudget checks before fetching; do not split or truncate a valid larger pack entry to fit the merge threshold.
  Source: `crates/crab-remote-git/src/reader.rs`.

## Features and platform

No declared Cargo features. Inspect locked gix-pack/delta and SlateDB session contracts before altering decode or close behavior. Dedicated large-repository/HTTP qualification is separate from local fixture tests.

## Verification

Inline operation tests cover close-error precedence. `crates/crab-remote-git/tests/remote_repository.rs` covers repository behavior with real Git fixtures; inspect fixture requirements before broad execution.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-remote-git --locked --lib operation::close_fault_tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-remote-git --locked --lib repository::tests
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-remote-git/README.md` — usage, entry points, and ownership map.
- `crates/crab-remote-git/REFERENCE.md` — detailed consistency, lifecycle, performance, and qualification contracts.
- `crates/crab-remote-git/Cargo.toml` — dependency and feature authority.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
