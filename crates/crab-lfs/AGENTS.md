# crab-lfs

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-lfs/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns LFS object layout, SHA-256 byte verification, receipts, and file-lock storage. crab-git owns pointer parsing; CLI/server layers own protocol and authorization.

## Read first

1. `crates/crab-lfs/src/lib.rs` — payload and lock entry points.
2. `crates/crab-lfs/src/object_store.rs` — `LfsObjectStore`: upload/download integrity and receipts.
3. `crates/crab-lfs/src/object_store/origin.rs` — `inspect`: fresh bounded origin verification.
4. `crates/crab-lfs/src/lock.rs` — `LfsLockManager::unlock_with_id`: holder identity and CAS tombstones.

Trace one path: `crab/src/lfs/transfer_agent.rs` → `LfsObjectStore::put_stream_with_size`
in `crates/crab-lfs/src/object_store.rs` → `Store`/multipart transport from
`crates/crab-storage/src/store.rs`. This transfer chain does not establish
that the shared lock manager is wired into a product endpoint.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Transfer integrity | `crates/crab-lfs/src/object_store.rs` | `crab/src/lfs/transfer_agent.rs` |
| Origin proof | `crates/crab-lfs/src/object_store/origin.rs` | `crates/crab-read/src/dependency_proof.rs` |
| Lock/unlock races | `crates/crab-lfs/src/lock.rs` | `crab/src/lfs/lock.rs` (separate implementation) |

The shared `LfsLockManager` has local tests but no established production caller
in this checkout. Compare the separate CLI lock implementation in
`crab/src/lfs/lock.rs`; HTTP `crates/crab-http-server/src/lfs.rs` currently
reports lock operations unavailable. Do not assume a shared-lock fix reaches
either product surface.

## Invariants

- Keep declared SHA-256 identity and size verification together; presence or a matching path alone does not prove object bytes.
  Source: `crates/crab-lfs/src/object_store.rs`.
- Fresh origin verification differs from receipt-aware reads and replica fallback; callers needing publication proof must retain the origin-only boundary.
  Source: `crates/crab-lfs/src/object_store/origin.rs`.
- Release locks with holder/ID checks and CAS tombstones; a stale unlock must not remove a replacement lock.
  Source: `crates/crab-lfs/src/lock.rs`.

## Features and platform

No declared Cargo features. Local object-store tests do not prove provider multipart/conditional-write behavior; inspect crab-storage and the locked object_store contract before changing those calls.

## Verification

Inline object_store tests cover hash rejection and streamed round trips. Inline lock tests include force_unlock_id_mismatch_preserves_replacement_lock.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-lfs --locked --lib object_store::tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-lfs --locked --lib lock::tests
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-lfs/README.md` — usage and detailed contracts.
- `crates/crab-lfs/Cargo.toml` — dependency and feature authority.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
