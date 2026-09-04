# crab-auth-store

`crab-auth-store` composes the credential contracts from
[`crab-auth`](../crab-auth/README.md) with the canonical storage abstraction
from [`crab-storage`](../crab-storage/README.md). It is the narrow adapter to
use when a caller has resolved credentials and needs a correctly configured
`Store`.

## Why it exists

Credential resolution and object-store construction have different ownership:
auth decides *which* credentials and scopes are valid; storage decides *how*
to talk to S3, GCS, Azure, or a compatible endpoint. This crate keeps that
composition in one place, including provider identity, Azure authorization
forms, scoped read routes, staging write routes, and refresh-on-auth-failure.

Without this boundary, every read, push, and service entry point would need to
duplicate credential-to-builder conversions and could accidentally widen a
path-scoped grant.

## Architecture

```text
CloudCredentials / TransferGrant
              │ validate and translate
              ▼
       object_store::ObjectStore
              │ wrap with identity, scope, and write routes
              ▼
          crab_storage::Store
```

The always-available functions build stores from `CloudCredentials`. The
`refreshing-store` feature adds a retrying object-store wrapper that refreshes
credentials once after an authentication failure. The `managed-service`
feature adds direct and gateway transfer-grant resolution plus protected push
and finalize integration.

Constructed stores preserve the storage owner's transport-target digest;
gateway grants bind their service endpoint and retain the repository scope.
Credential refresh accepts new secrets for the same target but refuses a
changed target before replacing the active inner store or retrying a request.
Callers must resolve a new operation after a target change. This preserves
snapshot/plan identity without changing logical bucket comparison or cache keys.

Azure split credentials are intentionally accepted only by
`build_protected_push_store`: read prefixes use their read tokens and the
exact prepared upload prefix uses the write token. A mismatched prefix is an
authorization error, not a fallback to a broader store.

## Usage

Build a canonical store from a resolution produced by `crab-auth`:

```rust
use crab_auth::CloudCredentials;
use crab_auth_store::build_store_from_credentials;
use crab_types::storage::StorageProviderKind;
use object_store::path::Path;

let store = build_store_from_credentials(
    "bucket",
    CloudCredentials::StaticEnv {
        provider: StorageProviderKind::S3,
    },
)?;

let (bytes, _etag) = store
    .get_with_etag(&Path::from("repositories/team/manifest"))
    .await?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

For a protected push, use the upload prefix issued by the prepare step:

```rust
let store = crab_auth_store::build_protected_push_store(
    "bucket",
    resolved.credentials,
    "repositories/team/staging/push-123",
)?;
```

`refreshing-store` and `managed-service` are deliberately opt-in because they
add HTTP clients and managed-service contracts:

```toml
[dependencies]
crab-auth-store = { version = "1", features = ["refreshing-store"] }
```

## Boundaries

- Resolve or refresh credentials with [`crab-auth`](../crab-auth/README.md).
- Use [`crab-storage`](../crab-storage/README.md) directly when credentials
  are already represented as storage-native builder inputs.
- Keep ref CAS and push serialization in
  [`crab-coordination`](../crab-coordination/README.md); a valid store does
  not by itself make a write safe.
