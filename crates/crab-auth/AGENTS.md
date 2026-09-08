# crab-auth

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-auth/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns credential/scope contracts, provider resolution, protected-push validation, and token storage. crab-auth-store translates resolved authority into storage; server callers enforce operation policy.

## Read first

1. `crates/crab-auth/src/lib.rs` — always-available contracts and optional clients.
2. `crates/crab-auth/src/credentials.rs` — `CloudCredentials / CredentialResolution`: provider secrets and scoped resolution.
3. `crates/crab-auth/src/credential_provider.rs` — `CredentialProvider`: resolution/refresh interface.
4. `crates/crab-auth/src/token_cache.rs` — `TokenCache / CachedTokens`: encrypted persistence and locking.
5. `crates/crab-auth/src/protected_push.rs` — `validate_push_ref_updates`: prepare/finalize validation.

Trace one path: `create_credential_provider` in `crates/crab-auth/src/credential_provider.rs`
→ `StaticProvider` in `crates/crab-auth/src/static_credentials.rs` for static
configuration → `CredentialResolution` from `crates/crab-auth/src/credentials.rs`.
Store consumers compose those results in `crates/crab-auth-store/src/lib.rs`.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Credential response | `crates/crab-auth/src/credential_response.rs` | `crates/crab-auth-store/src/lib.rs` |
| Token persistence | `crates/crab-auth/src/token_cache.rs` | `crab/src/cmd/login.rs` |
| Managed API contract | `crates/crab-auth/src/managed` | `crates/crab-auth-store/src/managed_repository.rs` |

## Invariants

- Prepare and sync complete key-file bytes before publishing the final name.
  Use non-overwriting publication so a failed write or a competing initializer
  cannot replace the authoritative key. Preserve private Unix permissions.
  Source: `crates/crab-auth/src/token_cache.rs`.

- Keychain initialization must not update an existing encryption key after a
  failed lookup. Use create-only insertion and read the stored winner after a
  failed insertion. Never return an unstored candidate.
  Source: `crates/crab-auth/src/token_cache.rs`.

- Classify missing tokens from the read result under the cache lock. Never use
  an existence probe to turn filesystem errors into an unauthenticated state.
  The current lock is Unix-only; non-Unix serialization remains unimplemented.
  Source: `crates/crab-auth/src/token_cache.rs`.

- Preserve scopes alongside resolved credentials; translating provider variants must not drop path restrictions.
  Source: `crates/crab-auth/src/credentials.rs`.
- Keep token-cache encryption and file locking together when changing store/load/delete paths; do not expose token material in diagnostics or fixtures.
  Source: `crates/crab-auth/src/token_cache.rs`.
- Validate protected-push response/ref updates at the contract boundary; duplicate destinations and no-op ref updates have explicit rejection tests.
  Source: `crates/crab-auth/src/protected_push.rs`.

## Features and platform

Empty default. Optional features are `oidc-client`, `aws-oidc-client`, `azure-entra-client`, `crab-auth-client`, and `gcp-workload-identity-client`; provider clients enable oidc-client. Token locking has platform-specific code. Credential and token Debug output omits secrets;
raw fields and serialized payloads remain sensitive. TokenCache construction may
use the OS keychain or a fallback key file; inspect fixture setup before running
token persistence tests. Mock OIDC tests do not qualify real identity providers; inspect their contract before changing exchanges.

## Verification

Inline credential_response/protected_push tests validate payloads; token_cache tests exercise encrypted round trips. Managed protocol coverage is in `crates/crab-auth/tests/managed_contract.rs`.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-auth --locked --lib credential_response
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-auth --locked --lib protected_push
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-auth --locked --lib --features oidc-client oidc
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-auth/README.md` — usage and detailed contracts.
- `crates/crab-auth/Cargo.toml` — dependency and feature authority.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
