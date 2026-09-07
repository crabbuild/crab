# crab-auth-store

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-auth-store/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns composition from resolved cloud credentials/managed grants to Store, including scope routing and refresh wrappers. Auth owns credential policy; storage owns provider construction and transport identity.

## Read first

1. `crates/crab-auth-store/src/lib.rs` — `build_store_from_credentials / build_protected_push_store`: credential conversion and store construction.
2. `crates/crab-auth-store/src/refreshing_store.rs` — `RefreshingObjectStore::refresh_parts`: refresh/retry and target binding.
3. `crates/crab-auth-store/src/gateway_store.rs` — `GatewayObjectStore`: scoped HTTP object adapter.
4. `crates/crab-auth-store/src/managed_repository.rs` — `ManagedRepositoryResolver`: managed discovery, grants, and push finalization.

Trace one path: `crab/src/auth/mod.rs` (`build_store_from_credentials`) → same-named
function in `crates/crab-auth-store/src/lib.rs` → provider construction in
`crates/crab-storage/src/provider_store.rs`.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Credential conversion | `crates/crab-auth-store/src/lib.rs` | `crates/crab-storage/src/provider_store.rs` |
| Refresh target safety | `crates/crab-auth-store/src/refreshing_store.rs` | `crab/src/auth/mod.rs` |
| Managed grant/gateway | `crates/crab-auth-store/src/gateway_store.rs` | `crates/crab-auth/src/managed` |

## Invariants

- Keep Azure split read/write credentials on the protected-push constructor path; a write prefix mismatch must not widen the grant.
  Source: `crates/crab-auth-store/src/lib.rs`.
- Refresh may replace secrets for the same target; reject a changed target before replacing the active store or retrying requests.
  Source: `crates/crab-auth-store/src/refreshing_store.rs`.
- Gateway paths must remain inside the grant repository/staging scope; inspect rejection-before-network tests when changing path mapping.
  Source: `crates/crab-auth-store/src/gateway_store.rs`.

## Features and platform

Empty default. `refreshing-store` enables object-store/HTTP wrappers; `managed-service` also enables refreshing-store and managed resolution with auth OIDC support. Check that exact slice for managed changes. Live refresh/grant behavior needs dedicated identity-service proof.

## Verification

Inline lib tests cover credential conversion; refreshing_store tests cover destination changes including multipart; gateway_store tests cover out-of-scope paths.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-auth-store --locked --lib --features refreshing-store refreshing_store::tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-auth-store --locked --lib --features refreshing-store gateway_store::tests
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-auth-store/README.md` — usage and detailed contracts.
- `crates/crab-auth-store/Cargo.toml` — dependency and feature authority.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
