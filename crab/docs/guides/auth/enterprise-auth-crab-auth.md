# Crab Auth Endpoint

Authenticate crab users via your corporate IdP and obtain scoped credentials
from a custom authorization service.

## Overview

The `"crab-auth"` provider is the most flexible option. It lets your
organization implement arbitrary authorization logic (RBAC, ABAC, policy
engines) without crab needing to understand the policy model:

```
Developer → Corporate IdP → ID Token
Read/maintenance operation → /v1/credentials → Scoped credentials
Path-scoped read → filtered repository view → scoped credentials
Push preflight → /v1/push/prepare → staging-only credentials
Staged push bundle → /v1/push/finalize → Service-owned manifest CAS
```

The Crab Auth endpoint receives the user's identity (via ID token) and the
requested operation, evaluates your policies, and returns short-lived
cloud-native credentials scoped to the specific repository and operation. Push
is the exception: the CLI can only upload staged immutable data, and crab-auth
verifies the staged bundle before committing the canonical manifest.

## Prerequisites

- crab installed (`make install`)
- An OIDC-compliant corporate IdP
- A Crab Auth HTTP service (you build and host this)
- Cloud storage buckets for crab repositories

## Crab Auth Credential Protocol

This is a public specification. Implement it in any language or framework.
`/v1/credentials` is for read and maintenance operations. It rejects
`operation: "push"`; push clients must use the protected push protocol below.

### Request

```
POST /v1/credentials HTTP/1.1
Host: crab-auth.corp.example.com
Content-Type: application/json
Authorization: Bearer <id_token>

{
  "id_token": "<jwt>",
  "repo_url": "crab://ml-models/team-alpha/gpt4",
  "operation": "fetch",
  "client_version": "0.1.0"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `id_token` | string | The OIDC ID token (JWT) from the corporate IdP |
| `repo_url` | string | The crab repository URL being accessed |
| `operation` | string | Concrete non-push Crab operation such as `fetch`, `clone`, `gc`, `fsck`, or `repack`; request operation `"*"` and `push` are rejected |
| `client_version` | string | The crab CLI version |

### Success Response — AWS

```json
{
  "provider": "aws",
  "credentials": {
    "access_key_id": "ASIAXXXXXXXXXXX",
    "secret_access_key": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
    "session_token": "FwoGZXIvYXdzEBYaDH...",
    "region": "us-west-2"
  },
  "expires_at": "2026-04-24T18:00:00Z",
  "permissions": ["read"]
}
```

### Success Response — GCP

```json
{
  "provider": "gcp",
  "credentials": {
    "access_token": "ya29.a0AfH6SM..."
  },
  "expires_at": "2026-04-24T18:00:00Z",
  "permissions": ["read"]
}
```

### Success Response — Azure

```json
{
  "provider": "azure",
  "credentials": {
    "storage_account": "mlmodels",
    "sas_token": "sv=2024-11-04&ss=b&srt=sco&sp=rl&se=..."
  },
  "expires_at": "2026-04-24T18:00:00Z",
  "permissions": ["read"]
}
```

Azure responses can also use `bearer_token` instead of `sas_token`.

Path-scoped read responses include an effective storage scope. The client must
construct its repository layout from this scope, not from the original URL:

```json
{
  "provider": "aws",
  "credentials": {
    "access_key_id": "ASIAXXXXXXXXXXX",
    "secret_access_key": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
    "session_token": "FwoGZXIvYXdzEBYaDH...",
    "region": "us-west-2"
  },
  "expires_at": "2026-04-24T18:00:00Z",
  "permissions": ["read"],
  "storage_scope": {
    "repo_prefix": "team-alpha/gpt4/acl-views/v1/0123/4-abc",
    "global_prefix": "team-alpha/gpt4/acl-views/v1/0123/4-abc/.crab",
    "source_repo": "team-alpha/gpt4",
    "scope_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
  }
}
```

### Error Response

```json
{
  "error": "forbidden",
  "message": "User alice@corp.example.com does not have push access to team-alpha/gpt4"
}
```

### HTTP Status Codes

| Code | Meaning | Crab Behavior |
|------|---------|-----------------|
| 200 | Success | Use returned credentials |
| 401 | Invalid/expired ID token | Refresh token and retry once |
| 403 | User not authorized | Return `AuthFailed` with message |
| 429 | Rate limited | Retry with `Retry-After` header |
| 5xx | Server error | Retry up to 3 times with exponential backoff (1s, 2s, 4s) |

## Protected Push Protocol

Push uses a receive-pack-style flow. The CLI first asks for a staging prefix,
uploads immutable push data under that prefix, then asks crab-auth to finalize.
The service verifies changed paths server-side and commits `{repo}/manifest`
with service-owned cloud credentials.
Path-scoped push rules require at least one server-verified changed path; use a
non-path-scoped push rule for intentional ref-only or metadata-only updates.
Protected push accepts branch ref updates only. The receive helper rejects
non-fast-forward updates server-side, so modified clients cannot force-push or
rewrite branch history unless a future explicit policy operation is added.

### Prepare

```
POST /v1/push/prepare HTTP/1.1
Host: crab-auth.corp.example.com
Content-Type: application/json
Authorization: Bearer <id_token>

{
  "id_token": "<jwt>",
  "repo_url": "crab://ml-models/team-alpha/gpt4",
  "ref_updates": [
    {
      "ref_name": "refs/heads/main",
      "old_oid": "0123456789abcdef0123456789abcdef01234567",
      "new_oid": "89abcdef0123456789abcdef0123456789abcdef"
    }
  ],
  "client_version": "0.1.0"
}
```

A successful response includes `push_id`, `upload_prefix`, and credentials with
staging-only immutable write permissions. These credentials must not read or
write original repo packs, metadata, shards, xorbs, LFS objects, refs, locks,
or `{repo}/manifest`.

### Finalize

```
POST /v1/push/finalize HTTP/1.1
Host: crab-auth.corp.example.com
Content-Type: application/json
Authorization: Bearer <id_token>

{
  "id_token": "<jwt>",
  "repo_url": "crab://ml-models/team-alpha/gpt4",
  "push_id": "0123456789abcdef0123456789abcdef",
  "ref_updates": [
    {
      "ref_name": "refs/heads/main",
      "old_oid": "0123456789abcdef0123456789abcdef01234567",
      "new_oid": "89abcdef0123456789abcdef0123456789abcdef"
    }
  ],
  "client_version": "0.1.0"
}
```

Finalize returns `409` for stale manifest/ref conflicts, `403` for verified path
policy denials, and `400` for malformed or stale staged bundles.

## Platform Admin Setup

### Step 1: Build and deploy the Crab Auth endpoint

Implement the protocol above in your preferred language. The endpoint should:

1. Validate the ID token (verify signature against the IdP's JWKS)
2. Extract the user identity from the token claims
3. Evaluate your authorization policies (RBAC, ABAC, etc.)
4. If authorized, generate scoped cloud credentials:
   - AWS: call `sts:AssumeRole` with a session policy that grants repo/view
     reads, or staging-prefix writes only for protected push
   - GCP: use Cloud Storage Credential Access Boundaries for repo/view reads
     and protected-push staging writes
   - Azure: generate scoped SAS tokens for repo/view reads or
     protected-push staging writes
5. Return the credentials with an expiry timestamp

### Step 2: Register the crab CLI app at your IdP

Same as other providers — register a public client with authorization code
(PKCE) and device code grants.

### Step 3: Distribute configuration

Share with your team:

| Key | Value |
|-----|-------|
| `issuer_url` | `https://login.corp.example.com` |
| `client_id` | `crab-cli-prod` |
| `auth_endpoint` | `https://crab-auth.corp.example.com/v1/credentials` |

## Developer Setup

### Step 1: Configure crab

```toml
# ~/.config/crab/config.toml
[auth]
provider = "crab-auth"
issuer_url = "https://login.corp.example.com"
client_id = "crab-cli-prod"
auth_endpoint = "https://crab-auth.corp.example.com/v1/credentials"
```

### Step 2: Log in

```bash
crab login
```

```
Authenticated as alice@corp.example.com (crab-auth)
```

### Step 3: Verify

```bash
crab auth status
```

```
Provider:     crab-auth
Identity:     alice@corp.example.com
Token expiry: 2026-04-24T18:30:00Z (52 minutes remaining)
Refresh:      yes
Endpoint:     https://crab-auth.corp.example.com/v1/credentials
```

### Step 4: Use crab

```bash
git clone crab://ml-models/team-alpha/gpt4
crab hydrate --all
```

The Crab Auth endpoint is called on each credential resolution. Crab caches
the returned credentials and refreshes them before expiry (5-minute window).

## Configuration Reference

### Required keys

| Key | Type | Description |
|-----|------|-------------|
| `auth.provider` | string | Must be `"crab-auth"` |
| `auth.issuer_url` | string | OIDC issuer URL |
| `auth.client_id` | string | OAuth 2.0 client ID |
| `auth.auth_endpoint` | string | Crab Auth endpoint URL |

### Credential dispatch

Crab reads the `provider` field from the Crab Auth response and constructs the
appropriate cloud client:

| `provider` | Credential Fields | Object Store |
|------------|-------------------|--------------|
| `"aws"` | `access_key_id`, `secret_access_key`, `session_token`, `region` | `AmazonS3Builder` |
| `"gcp"` | `access_token` | `GoogleCloudStorageBuilder` |
| `"azure"` | `storage_account` plus `sas_token` or `bearer_token` | `MicrosoftAzureBuilder` |

The `region` field for AWS defaults to `us-east-1` if not provided.

## Example Crab Auth Endpoint (Python)

A minimal example using Flask:

```python
from flask import Flask, request, jsonify
import boto3
import json
import jwt

app = Flask(__name__)

@app.route("/v1/credentials", methods=["POST"])
def vend_credentials():
    body = request.json
    id_token = body["id_token"]
    repo_url = body["repo_url"]
    operation = body["operation"]

    if operation.lower() == "push":
        return jsonify({
            "error": "protected_push_required",
            "message": "Push must use /v1/push/prepare and /v1/push/finalize"
        }), 400

    # 1. Validate the ID token (verify signature, check expiry)
    claims = jwt.decode(id_token, options={"verify_signature": False})
    email = claims.get("email", claims["sub"])

    # 2. Evaluate authorization policies
    # (your RBAC/ABAC logic here)
    if not is_authorized(email, repo_url, operation):
        return jsonify({"error": "forbidden", "message": f"{email} not authorized"}), 403

    # 3. Generate scoped AWS credentials
    sts = boto3.client("sts")
    resp = sts.assume_role(
        RoleArn="arn:aws:iam::123456789012:role/crab-auth",
        RoleSessionName=f"crab-{email[:12]}",
        DurationSeconds=3600,
        Policy=json.dumps({
            "Version": "2012-10-17",
            "Statement": [{
                "Effect": "Allow",
                "Action": ["s3:GetObject"],
                "Resource": [f"arn:aws:s3:::ml-models/{repo_url.split('/', 3)[3]}/*"]
            }]
        })
    )

    creds = resp["Credentials"]
    return jsonify({
        "provider": "aws",
        "credentials": {
            "access_key_id": creds["AccessKeyId"],
            "secret_access_key": creds["SecretAccessKey"],
            "session_token": creds["SessionToken"],
            "region": "us-west-2"
        },
        "expires_at": creds["Expiration"].isoformat() + "Z",
        "permissions": ["read"]
    })
```

This minimal example is read-only. Do not extend `/v1/credentials` with direct
push writes; production push support must implement `/v1/push/prepare` and
`/v1/push/finalize` with server-side changed-path verification.

## Troubleshooting

### "Crab Auth endpoint returned 401"

The ID token is invalid or expired. Crab auto-retries once with a refreshed
token. If it persists:

```bash
crab logout
crab login
```

### "Crab Auth endpoint returned 403"

Your authorization policies denied the request. Check with your platform admin
about your permissions for the specific repository and operation.

### "Crab Auth request failed after retries"

The endpoint is unreachable or returning 5xx errors. Crab retries up to 3
times with exponential backoff. Check the endpoint's health and logs.

### "unsupported provider in Crab Auth response: oracle"

The Crab Auth endpoint returned a `provider` value that crab doesn't support.
Supported values: `"aws"`, `"gcp"`, `"azure"`.

## Related

- [Enterprise Auth Overview](enterprise-auth.md)
- [AWS OIDC](enterprise-auth-aws.md)
- [GCP Workload Identity](enterprise-auth-gcp.md)
- [Azure Entra ID](enterprise-auth-azure.md)
