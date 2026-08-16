# Enterprise Authentication

Federated identity and multi-cloud credential management for crab.

## Overview

Crab supports six authentication modes for accessing cloud object storage:

| Provider | Config Value | Use Case |
|----------|-------------|----------|
| Static (default) | `"static"` | Environment variables / cloud SDK credentials |
| AWS OIDC | `"aws-oidc"` | Corporate IdP → AWS STS temporary credentials |
| GCP Workload Identity | `"gcp-workload-identity"` | Corporate IdP → GCP federated access token |
| Azure Entra ID | `"azure-entra"` | Corporate IdP → Azure Blob Storage token |
| Crab Auth | `"crab-auth"` | Corporate IdP → custom authorization endpoint |
| None | `"none"` | No auth (local testing with MinIO) |

The default is `"static"` — identical to pre-auth behavior. Existing users
see zero change unless they opt in to a federated provider.

## Quick Start

### Check current auth status

```bash
crab auth status
```

### Log in with your corporate IdP

```bash
crab login
```

On desktop, this opens your browser. Over SSH, use `--headless` for device code
flow:

```bash
crab login --headless
```

### Log out

```bash
crab logout
```

### Force a token refresh

```bash
crab auth refresh
```

## Configuration

Auth settings live in the `[auth]` section of your crab config. You can set
them per-user (`~/.config/crab/config.toml`) or per-repo
(`.crab/config.toml`). The 4-layer config precedence applies:
compiled defaults → user TOML → repo TOML → remote JSON.

### Common keys

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `auth.provider` | string | `"static"` | Authentication provider |
| `auth.storage_provider` | string | `"auto"` | Cloud backend: `s3`, `gcs`, `azure`, `auto` |
| `auth.issuer_url` | string | — | OIDC issuer URL (your corporate IdP) |
| `auth.client_id` | string | — | OAuth 2.0 client ID for the crab CLI app |
| `auth.auth_endpoint` | string | — | Crab Auth endpoint URL |
| `auth.scopes` | string | `"openid email profile"` | OAuth 2.0 scopes to request |
| `auth.token_cache_path` | string | `"~/.config/crab/tokens/"` | Token cache directory |

### Setting via CLI

```bash
crab config set auth.provider aws-oidc
crab config set auth.issuer_url https://login.corp.example.com
crab config set auth.client_id crab-cli-prod
```

### Setting via TOML

```toml
# ~/.config/crab/config.toml
[auth]
provider = "aws-oidc"
issuer_url = "https://login.corp.example.com"
client_id = "crab-cli-prod"
```

## Provider Setup Guides

Each provider has a dedicated step-by-step guide:

| Guide | Description |
|-------|-------------|
| [Static / Multi-Cloud](enterprise-auth-static.md) | Default env-var credentials for S3, GCS, or Azure |
| [AWS OIDC](enterprise-auth-aws.md) | Corporate IdP → AWS STS AssumeRoleWithWebIdentity |
| [GCP Workload Identity](enterprise-auth-gcp.md) | Corporate IdP → GCP Workload Identity Federation |
| [Azure Entra ID](enterprise-auth-azure.md) | Corporate IdP → Azure Blob Storage via Entra ID |
| [Crab Auth](enterprise-auth-crab-auth.md) | Corporate IdP → custom authorization endpoint |

## Token Management

### Where tokens are stored

Tokens are cached at `~/.config/crab/tokens/` (configurable via
`auth.token_cache_path`). Each provider gets its own encrypted file:

```
~/.config/crab/tokens/
├── aws-oidc.json.enc
├── gcp-workload-identity.json.enc
└── azure-entra.json.enc
```

### Encryption

Tokens are encrypted at rest with ChaCha20-Poly1305. The encryption key is
stored in the macOS Keychain (via the `security` CLI) or falls back to
`~/.config/crab/.token-key` with `0600` permissions on Linux.

### Automatic refresh

When a cached access token is within 5 minutes of expiry, crab automatically
refreshes it using the stored refresh token before the next object store
operation. Long-running pushes and hydrations won't fail mid-operation due to
expired tokens.

### Concurrent access

File-level locking (`flock` on Unix) prevents concurrent `crab login` /
`crab logout` from corrupting the token cache.

## Diagnostics

### crab doctor

The `crab doctor` command includes an auth health check:

```bash
crab doctor
```

```
crab doctor

  ✓ git                      git version 2.47.1
  ✓ crab binary            /Users/you/.cargo/bin/crab (on PATH)
  ✓ auth                     aws-oidc — alice@corp.example.com, expires 2026-04-24T18:30:00Z
  ✓ credentials              bucket 'ml-models' reachable
```

### crab auth status

Detailed auth state with `--json` for scripting:

```bash
crab auth status --json
```

```json
{
  "provider": "aws-oidc",
  "identity": "alice@corp.example.com",
  "token_expiry": "2026-04-24T18:30:00Z",
  "token_expired": false,
  "refresh": true,
  "provider_settings": [
    { "key": "AWS role", "value": "arn:aws:iam::123456789012:role/crab-developer" },
    { "key": "Region", "value": "us-west-2" }
  ]
}
```

## Troubleshooting

### "login is not needed for the 'static' provider"

You're using the default static provider. If you want federated auth, set
`auth.provider` first:

```bash
crab config set auth.provider aws-oidc
```

### "no cached tokens — run `crab login`"

Your tokens have been cleared or you haven't logged in yet:

```bash
crab login
```

### "token refresh returned HTTP 400"

Your refresh token has expired. Re-authenticate:

```bash
crab login
```

### "STS AccessDenied: Not authorized to perform sts:AssumeRoleWithWebIdentity"

The IAM role's trust policy doesn't accept tokens from your IdP. Ask your
platform admin to verify the OIDC provider is registered and the trust policy
includes your IdP's issuer URL.

### Device code flow over SSH

When working over SSH, crab can't open a browser. Use `--headless`:

```bash
crab login --headless
```

This displays a URL and code. Open the URL on any device, enter the code, and
complete authentication. The CLI polls until you're done (5-minute timeout).

## Related Commands

- [`crab config`](../crab-config.md) — read/write auth configuration
- [`crab doctor`](../crab-doctor.md) — health check including auth
- [`crab env`](../crab-env.md) — print environment diagnostics

## Credential helper interop

Crab's primary auth path is cloud-native: explicit STS credentials from
environment variables, or the cloud SDK's default credential chain
(`AmazonS3Builder::from_env()` and equivalents). That path covers the vast
majority of deployments.

For deployments that front `crab://` through an HTTP gateway requiring
basic auth or a bearer token — for example, a signed-URL auth service
fronting an S3 bucket — crab integrates with standard git credential
helpers via [`gix-credentials`](https://docs.rs/gix-credentials).

### Precedence

Crab consults auth sources in this fixed order; the first source that
returns a usable credential wins.

1. **Explicit STS credentials** from environment or config
   (`AWS_ACCESS_KEY_ID`, `AWS_SESSION_TOKEN`, `auth.aws.*` config).
2. **Cloud SDK default credential chain** (`from_env()` on the
   object_store builder).
3. **Git credential helper cascade** (`credential.helper`,
   `credential.<url>.helper`), resolved through `gix-credentials`.
4. **Anonymous** (fall-through when no helper is configured).

Explicit cloud-SDK credentials always win; the credential helper is only
consulted for HTTP-style auth when no cloud-SDK path resolves.

### Configuration

Any `credential.helper` entry recognized by git works. Typical shapes:

```gitconfig
# Global helper — applies to every url
[credential]
    helper = osxkeychain

# URL-scoped helper — applies only to the matching prefix
[credential "crab://bucket/repo"]
    helper = !my-bearer-token-vendor --bucket bucket

# Clear an inherited helper for a specific url
[credential "crab://unsafe-bucket"]
    helper =
```

The `!command` shell-execution form is supported; crab delegates parsing
to `gix-credentials`, which knows about bare names (`osxkeychain`,
`libsecret`, `manager-core`), absolute paths with arguments, and shell
scripts.

### Erase on auth failure

A 401 or 403 from the gateway triggers an automatic `erase` on the helper
cascade, so helpers that cache locally (`osxkeychain`, `libsecret`) evict
the stale credential before crab falls through to anonymous. No user
action required.

### Prompting disabled

Crab never prompts interactively. The remote helper runs under `git
push` / `git fetch`, which owns stdin; an interactive prompt would hang
the parent git process. Helpers that would normally prompt fall through
to anonymous instead.
