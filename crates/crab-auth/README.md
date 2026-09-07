# crab-auth

`crab-auth` defines how Crab obtains short-lived credentials and authorization
scopes for object storage and managed services. It is the authentication
boundary: it resolves identity and returns provider-neutral credential
contracts, while [`crab-storage`](../crab-storage/README.md) and
[`crab-auth-store`](../crab-auth-store/README.md) build the actual stores.

## Why it exists

Cloud credentials arrive through several incompatible mechanisms: static SDK
environment chains, AWS STS/OIDC, GCP Workload Identity Federation, Azure
Entra ID, and a managed Crab Auth service. Keeping those flows here prevents
storage, Git, and workflow code from each inventing token refresh, expiry, or
scope handling.

The crate also owns the wire contracts used by protected pushes and managed
transfer grants. A caller can therefore validate an auth response before any
credential is handed to an object-store builder.

## Architecture

The main flow is:

```text
CredentialProvider
        │ resolve(bucket, prefix, operation)
        ▼
CredentialResolution
   ├── CloudCredentials       provider-specific secret material
   └── StorageScope            optional path restriction
        │
        ▼
crab-storage / crab-auth-store
```

`CredentialProvider` exposes `resolve`, `needs_refresh`, `refresh`, and an
optional identity. Implementations may use local token caches and OIDC
exchanges, but the trait does not depend on an object-store client. The
`CloudCredentials` variants cover AWS, GCP, Azure, split Azure read/write
scopes, and the `StaticEnv` sentinel for a cloud SDK's default chain.

The `managed` module contains versioned discovery, profile, transfer-grant,
protected-push, and administrative contracts. OIDC and provider-specific
clients are opt-in features so a minimal static-credentials build does not
pull in network clients.

## Usage

Resolve static environment credentials through the same interface used by
dynamic providers:

```rust
use crab_auth::{
    create_credential_provider, CredentialProvider, CredentialProviderConfig,
    StaticAuthConfig,
};
use crab_types::storage::StorageProviderKind;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let provider = create_credential_provider(CredentialProviderConfig::Static(
    StaticAuthConfig {
        storage_provider: StorageProviderKind::S3,
    },
))?;

let resolution = provider.resolve("bucket", "repositories/team", "fetch").await?;
assert!(resolution.storage_scope.is_none());
# Ok(())
# }
```

In a consuming Crab workspace member, enable only the provider clients needed
by the deployment. This crate is not published to the registry:

```toml
[dependencies]
crab-auth = { workspace = true, features = ["oidc-client", "aws-oidc-client"] }
```

Credential and token types omit secrets from `Debug` output. Their string fields
and serialized payloads still contain credentials: pass resolutions directly to
the storage adapter and keep those fields out of logs.

## Token-cache key handling

| Source | Representation | Invalid input |
| --- | --- | --- |
| macOS Keychain | 64 ASCII hex characters | Returns a key-store error; initialization follows its existing key-file path |
| Key file | Exactly 32 raw bytes | Returns a key-store error for an incorrect length |

Decoder errors never include the supplied key. Token-cache tests use synthetic
keys and temporary directories; they do not qualify live Keychain integration.

## Boundaries

- Use [`crab-types`](../crab-types/README.md) for shared storage and scope
  types, not ad-hoc provider strings.
- Use [`crab-auth-store`](../crab-auth-store/README.md) to turn a resolution
  into a `Store`, including refresh and protected-push behavior.
- Use [`crab-coordination`](../crab-coordination/README.md) for write
  serialization and commit authority; authentication does not decide ref
  ownership.
