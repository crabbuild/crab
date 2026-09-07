# crab-cache

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-cache/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns cache keys, roots, admission, disk lifecycle, and optional remote client contracts. Read routing across local cache, service, and origin belongs to crab-cache-store.

## Read first

1. `crates/crab-cache/src/lib.rs` — feature-gated cache surfaces.
2. `crates/crab-cache/src/key.rs` — `CacheKey`: content identities versus named manifests.
3. `crates/crab-cache/src/path_class.rs` — `classify_path / cache_route_contract`: mutable/immutable service route taxonomy.
4. `crates/crab-cache/src/local_cache.rs` — `LocalCache::get_or_fetch`: verified reads and fallible writes.
5. `crates/crab-cache/src/lifecycle.rs` — `CacheUseGuard / CacheCleanGuard`: shared cache ownership and cleanup.

Trace one path: `CachingStore::get_with_etag` in `crates/crab-cache-store/src/lib.rs`
→ `LocalCache::get_or_fetch_with` in `crates/crab-cache/src/local_cache.rs`
→ its validation/publication helpers. Inspect `CacheKey` and origin closure
semantics rather than treating every cache object as interchangeable.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Key or path admission | `crates/crab-cache/src/path_class.rs` | `crates/crab-cache-store/src/lib.rs` |
| Disk fill or cleanup | `crates/crab-cache/src/local_cache.rs` | `crates/crab-cache/src/lifecycle.rs` |
| Remote client contract | `crates/crab-cache/src/cache_client.rs` | `crates/crab-cache-server/src/state.rs` |

## Invariants

- Manifest keys carry names and optional ETags; do not treat them as content-addressed chunk/shard/xorb identities.
  Source: `crates/crab-cache/src/key.rs`.
- Path taxonomy is shared with the service and store adapter; review both consumers before changing immutable admission.
  Source: `crates/crab-cache/src/path_class.rs`.
- Read-through cache persistence failure and origin fetch/validation failure have different outcomes. Preserve explicit put errors rather than applying read-through behavior to every API.
  Source: `crates/crab-cache/src/local_cache.rs`.

## Features and platform

Empty default. `active-probe` adds HTTP probing; `remote-client` includes active-probe and async client support. `local-cache` and `xet-chunk-cache` enable separate disk-backed surfaces and shared lifecycle code. Test both enabled paths when changing common private filesystem/catalog behavior.

## Verification

Inline local_cache tests exercise fills and corrupt entries; path_class tests cover route admission. Inspect lifecycle/private filesystem tests for cache root changes.

Credential Debug regressions live in `crates/crab-cache/tests/credential_debug.rs`.
Run that integration target with no default features and with `remote-client`;
the latter includes active-probe credentials and stored client headers.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-cache --locked --lib --features local-cache local_cache::tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-cache --locked --lib path_class::tests
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-cache/README.md` — feature selection, usage, and source map.
- `crates/crab-cache/REFERENCE.md` — persistence/lifecycle contracts and qualification limits.
- `crates/crab-cache/Cargo.toml` — dependency and feature authority.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
