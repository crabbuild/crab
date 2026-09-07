# crab-storage

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-storage/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns provider construction, object paths, conditional writes, transport retries, and storage errors. Auth resolves credentials; callers supply mutation and publication policy.

## Read first

1. `crates/crab-storage/src/lib.rs` — Store facade and exports.
2. `crates/crab-storage/src/layout.rs` — `StoreLayout / canonical_global_content_path`: global versus repository key routing.
3. `crates/crab-storage/src/store.rs` — `Store`: read/write operations and observers.
4. `crates/crab-storage/src/cas.rs` — `cas_update_bounded`: bounded load/mutate/conditional-write loop.
5. `crates/crab-storage/src/error_map.rs` — `classify_auth_error / map_object_store_error`: dependency error classification.

Trace one path: `crates/crab-auth-store/src/lib.rs` (credential conversion) → provider
builders in `crates/crab-storage/src/provider_store.rs` → object_store builders.
For protocol changes, resolve their locked version from Cargo.lock and read the
dependency source before asserting provider behavior.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| CAS behavior | `crates/crab-storage/src/cas.rs` | `crates/crab-metadata/src/manifest_store.rs` |
| Provider or target identity | `crates/crab-storage/src/provider_store.rs` | `crates/crab-auth-store/src/lib.rs` |
| Object key layout | `crates/crab-storage/src/layout.rs` | `crates/crab-cache/src/path_class.rs` |

## Invariants

- Non-resumable multipart completion uses `crab_storage::multipart::complete_upload`: abort on failure, preserve the completion error, and await cleanup before retry. Durable journal sessions retain their separate recovery protocol.
  Source: `crates/crab-storage/src/multipart.rs`.

- Keep CAS conditional create/update and conflict handling distinct from transport retry. The mutation callback may be evaluated more than once.
  Source: `crates/crab-storage/src/cas.rs`.
- Check both loaded and newly serialized CAS object sizes; a successful write must remain within the same read ceiling.
  Source: `crates/crab-storage/src/cas.rs`.
- Logical bucket identity and credential-free transport target identity serve different consumers; do not derive the latter from display text.
  Source: `crates/crab-storage/src/identity.rs`.

## Features and platform

Empty default; `test-support` exposes shared test helpers. S3/GCS/Azure/fs capabilities are dependency features, not crate flags. Read the locked object_store ObjectStore/PutMode/UpdateVersion contract before changing conditions or error mapping. Live provider behavior needs dedicated credentials and CI, not just in-memory tests.

## Verification

Inline `crates/crab-storage/src/provider_store.rs` tests exercise construction; `crates/crab-storage/src/cas.rs` covers conditional updates and size ceilings.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-storage --locked --lib provider_store
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-storage --locked --lib cas::tests
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-storage/README.md` — usage and detailed contracts.
- `crates/crab-storage/Cargo.toml` — dependency and feature authority.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
