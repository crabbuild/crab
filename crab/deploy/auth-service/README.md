# Crab Auth — Reference Implementation

A deployable reference implementation of the crab-auth protocol.
Enterprises use this to authenticate crab CLI users via their corporate IdP
and return scoped, short-lived cloud credentials.

## Architecture

```
Developer → Corporate IdP (OIDC) → ID Token
                                        ↓
crab CLI → /v1/credentials or /v1/push/* → Crab Auth → Evaluate RBAC policy
                                        ↓
                              Generate scoped credentials / finalize push
                              (AWS STS / GCP / Azure)
                                        ↓
                              Return to crab CLI → Access object store
```

## Quick Start (Docker)

```bash
cd CrabBuild

# 1. Configure your IdP and RBAC policy
cp crab/deploy/auth-service/config/policy.example.yaml crab/deploy/auth-service/config/policy.yaml
# Edit policy.yaml with your users, repos, and permissions

# 2. Build and run
docker build -f crab/deploy/auth-service/Dockerfile -t crab-auth .
docker run -p 8080:8080 \
  -v $(pwd)/crab/deploy/auth-service/config:/etc/crab-auth:ro \
  -e CRAB_AUTH_JWKS_URL=https://your-idp.example.com/.well-known/jwks.json \
  -e CRAB_AUTH_ISSUER=https://your-idp.example.com \
  -e CRAB_AUTH_AUDIENCE=crab-cli \
  -e CRAB_AUTH_AWS_ROLE_ARN=arn:aws:iam::123456789012:role/crab-auth-base \
  -e CRAB_AUTH_AWS_EXTERNAL_ID=crab-auth \
  crab-auth
```

Verify both liveness and protected-push readiness before issuing clients an
endpoint:

```bash
curl http://localhost:8080/health
# {"status":"ok"}

curl http://localhost:8080/ready
# {"status":"ok","auth_config":"ok","policy":"ok","provider_config":"ok","jwks":"ok","receive_helper":"ok","view_helper":"ok",...}
```

`/ready` validates required auth environment, policy loading, provider
configuration, JWKS signing-key availability, and the packaged
`crab-auth-receive doctor` / `crab-auth-view doctor` commands. A `503` means
the runtime should not receive enterprise traffic.

## Deployment Options

| Method | Best For | Directory |
|--------|----------|-----------|
| Docker | Any environment, Kubernetes | `./Dockerfile` |
| AWS Lambda + Terraform | AWS-native, serverless | `./terraform/` |
| AWS SAM | AWS developers familiar with SAM | `./sam/` |
| Cloud Run | GCP-native, serverless | `./cloudrun/` |

### AWS Lambda (Terraform)

```bash
cd crab/deploy/auth-service/terraform
cp terraform.tfvars.example terraform.tfvars
# Edit terraform.tfvars with your settings

terraform init
terraform plan
terraform apply
```

Terraform builds the Lambda zip locally, including Python dependencies and the
Linux `crab-auth-receive` and `crab-auth-view` helpers for
`lambda_architecture`. Docker must be running because the helpers are compiled
in a Linux builder image. The package step runs on each apply so a clean
checkout with remote Terraform state cannot reuse a missing or stale local zip.
Zip-based Lambda deployments must attach a Lambda layer that provides
`/opt/bin/git`; both helpers use Git plumbing for protected push verification
and filtered read-view materialization.

### AWS Lambda (SAM)

```bash
cd crab/deploy/auth-service
./scripts/build-receive-helper.sh --linux-amd64

cd sam/
sam build
sam deploy --guided
```

When prompted, provide `GitLayerArn`, a versioned Lambda layer ARN containing a
`git` executable at `/opt/bin/git`.

### Google Cloud Run

```bash
cd CrabBuild
docker build -f crab/deploy/auth-service/cloudrun/Dockerfile -t gcr.io/YOUR_PROJECT/crab-auth .
docker push gcr.io/YOUR_PROJECT/crab-auth
gcloud run deploy crab-auth \
  --image gcr.io/YOUR_PROJECT/crab-auth \
  --region us-central1 \
  --set-env-vars "CRAB_AUTH_JWKS_URL=https://your-idp.example.com/.well-known/jwks.json,CRAB_AUTH_ISSUER=https://your-idp.example.com,CRAB_AUTH_AUDIENCE=crab-cli"
```

## Configuration

### Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `CRAB_AUTH_JWKS_URL` | Yes | OIDC JWKS endpoint for token verification |
| `CRAB_AUTH_ISSUER` | Yes | Expected `iss` claim in ID tokens |
| `CRAB_AUTH_AUDIENCE` | Yes | Expected `aud` claim (your client_id) |
| `CRAB_AUTH_POLICY_PATH` | No | Path to policy YAML (default: `/etc/crab-auth/policy.yaml`) |
| `CRAB_AUTH_AWS_ROLE_ARN` | For AWS | IAM role to assume for credential generation |
| `CRAB_AUTH_AWS_EXTERNAL_ID` | If trust policy requires it | STS ExternalId sent when assuming `CRAB_AUTH_AWS_ROLE_ARN` |
| `CRAB_AUTH_AWS_REGION` | For AWS | AWS region (default: `us-east-1`) |
| `CRAB_AUTH_S3_ACCESS_KEY_ID` | For S3-compatible | Static access key for self-hosted S3-compatible stores; falls back to `AWS_ACCESS_KEY_ID` |
| `CRAB_AUTH_S3_SECRET_ACCESS_KEY` | For S3-compatible | Static secret key for self-hosted S3-compatible stores; falls back to `AWS_SECRET_ACCESS_KEY` |
| `CRAB_AUTH_S3_REGION` | For S3-compatible | Region returned to clients; falls back to `AWS_REGION`, `AWS_DEFAULT_REGION`, then `us-east-1` |
| `CRAB_AUTH_S3_SESSION_TOKEN` | No | Optional S3-compatible session token; ambient `AWS_SESSION_TOKEN` is intentionally ignored |
| `CRAB_AUTH_GCP_PROJECT` | For GCP | GCP project ID |
| `CRAB_AUTH_GCP_SA_EMAIL` | For GCP | Service account to impersonate |
| `CRAB_AUTH_AZURE_TENANT_ID` | For Azure | Azure tenant ID |
| `CRAB_AUTH_AZURE_SUBSCRIPTION_ID` | For Azure | Azure subscription ID |
| `CRAB_AUTH_LOG_LEVEL` | No | Logging level (default: `INFO`) |
| `CRAB_AUTH_SESSION_DURATION` | No | Credential lifetime in seconds (default: `3600`) |
| `CRAB_AUTH_RATE_LIMIT_PER_MINUTE` | No | Per-instance credential request refill rate (default: `120`) |
| `CRAB_AUTH_RATE_LIMIT_BURST` | No | Per-instance token bucket burst size (default: `30`) |
| `CRAB_AUTH_RATE_LIMIT_MAX_KEYS` | No | Maximum tracked client rate-limit keys per instance (default: `10000`) |
| `CRAB_AUTH_TRUST_PROXY_HEADERS` | No | Trust `X-Forwarded-For`/`X-Real-IP` for rate limit keys (default: `false`) |
| `CRAB_AUTH_RECEIVE_HELPER` | No | Path to packaged `crab-auth-receive` helper (default: `crab-auth-receive`) |
| `CRAB_AUTH_VIEW_HELPER` | No | Path to packaged `crab-auth-view` helper (default: `crab-auth-view`) |
| `CRAB_AUTH_RECEIVE_TIMEOUT_SECONDS` | No | Receive helper timeout for verify/commit (default: `300`) |
| `CRAB_AUTH_STAGING_TTL_SECONDS` | No | Best-effort cleanup TTL for abandoned push staging objects (default: `86400`, `0` disables) |

### RBAC Policy File

See `config/policy.example.yaml` for the full schema. The policy maps
users/groups to repositories and permissions:

```yaml
version: "1"
rules:
  - identity: "alice@corp.example.com"
    repos: ["*"]
    operations: ["push", "fetch", "clone"]
  - group: "ml-team"
    repos: ["ml-models/*"]
    operations: ["push", "fetch", "clone", "gc"]
    read_paths: ["models/**", "metadata/**", "README.md"]
    write_paths: ["models/**", "metadata/**", "README.md"]
  - identity: "*"
    repos: ["public-data/*"]
    operations: ["fetch", "clone"]
```

Rules may include `read_paths` for filtered read views and `write_paths` for
protected push. Omit the relevant field for repo-wide access. Deny rules use
the same fields and always win: denied read paths are subtracted from the
filtered view, and denied write paths reject finalize if any verified path
matches. Matching allow rules for the same identity, repo, operation, and
provider are unioned.

Set `default_provider: s3` for local or self-hosted S3-compatible stores such as
RustFS that use static credentials instead of cloud-native scoped credentials.
This keeps Crab Auth policy checks, path-filtered read views, and protected
push verification in front of the CLI. It does not make a broad static object
store key path-scoped outside Crab; production deployments that need
object-store-enforced least privilege should use `aws`, `gcp`, or `azure`
scoped credentials.
Push clients must use `/v1/push/prepare` and `/v1/push/finalize`; `/v1/credentials`
does not issue push credentials.
For repos matched by `protected_repos`, `/v1/credentials` also rejects direct
write credentials for non-push write operations; those operations need their own
service-owned flow before they can mutate protected repo state.

Policy files are parsed strictly. Unknown top-level fields, unknown rule fields
such as legacy `paths`, unsupported providers, invalid operations, and unsafe
repo/path patterns fail service startup instead of being ignored.

Repository URLs must include a non-empty repo prefix, for example
`crab://bucket/team/repo`. Bucket-root and container-root repo URLs are rejected
because they cannot be safely scoped as an enterprise repo boundary.

Before minting credentials for a path-scoped read, the issuer verifies that
the view helper returned the exact Capsule v2 namespace for the authorized
repository, scope hash, source generation, and source digest. The view's
content prefix must be its `.crab` child.

### CDN viewer grants

Add a `cdn` entry to the issuer's trusted policy file for each approved
repository export. The origin must match the cloud provider selected by policy
and the exact physical bucket or container used for origin credentials.
Each viewer URL path can identify one export. The issuer rejects aliases
formed by moving part of the repository prefix into `base_url`.

```yaml
cdn:
  - provider: cloudfront
    origin: s3://example-repository-bucket
    repo_prefix: org/models
    base_url: https://downloads.example.com
    allow_unfiltered: true
    approval:
      path: /etc/crab-auth/private-binding.json
      sha256: <sha256-of-the-reviewed-binding-file>
    key_id: EXAMPLEPUBLICKEYID
    key_file: /run/secrets/cloudfront-viewer-key.pem
    ttl_seconds: 300
```

After the private deployment passes its control-plane and transport
probes, review the resulting binding candidate and mount its exact bytes
as the approval file. Configure the SHA-256 from that review. Use the
operator collector for customer repositories. It can sign the initial
probe with the local operator key while the issuer is gated. Once approval
is installed, verify a grant from the issuer. [Customer repository approval](../../docs/guides/provider-qualification.md#collect-approval-for-a-customer-repository) · [Qualification fixture approval](../../docs/guides/provider-qualification.md#inspect-a-private-deployment-binding)

The issuer checks the approval after authorizing the native credentials
and selecting the repository or view. It checks the selected placement
before opening the signing key. An export with no approval serves native
credentials only. A configured approval with mismatched bytes, route, or
placement fails the request. Withdraw approval before changing deployment
trust and approve the replacement after rerunning the probes.

Grant the service identity metadata read access only on the approved
targets. S3 uses `s3:GetBucketLocation` with the expected bucket owner and
the region selected by the AWS credential provider. GCS uses
`storage.buckets.get` to check the owning project and location. Azure uses
`Microsoft.Storage/storageAccounts/read` and
`Microsoft.Storage/storageAccounts/blobServices/containers/read` to check
the configured subscription, account, primary location and container.
These lookups run when issuing a viewer grant. [S3 expected bucket owner](https://docs.aws.amazon.com/cli/latest/reference/s3api/get-bucket-location.html) · [GCS bucket metadata](https://docs.cloud.google.com/storage/docs/json_api/v1/buckets/get) · [Azure storage account properties](https://learn.microsoft.com/en-us/rest/api/storagerp/storage-accounts/get-properties)

Mount signing material from the provider's secret manager. CloudFront uses
an RSA 2048 PEM key whose public key belongs to the distribution's trusted
key group. For Google Cloud CDN use `provider: cloud_cdn`, `origin: gs://...`,
the backend bucket's key name, and the base64url form of its 16-byte key.
For Front Door use `provider: front_door` and
`origin: https://<account>.blob.core.windows.net/<container>`. Mount an RSA
2048 PEM key and give the Azure API Management gateway the corresponding
public key and key ID. The gateway validates the CDN base URL as issuer and
audience, the full physical `origin`, and the slash-terminated `prefix`
claim against every requested object path before forwarding to Front Door.

`allow_unfiltered` defaults to false. Enable it only if the entire physical
repository export, including packs and historical data, is appropriate for
every principal authorized for repository-wide reads. For a protected export,
keep `repo_prefix` set to the logical source repository and set
`protected_view_suffix` to the exact deployed suffix from the authorized
`storage_scope.repo_prefix`, including the leading `/acl-views/v2/`. Set
only one delivery mode per origin and logical repository. The issuer grants
that exact materialized view and its view-local content prefix. Other scopes
and generations keep their authorized native origin credentials.

Configure the same suffix on the distribution or gateway. It must enforce
exact immutable key shapes and authorize every viewer request before cache
access. The private fixtures cover repository-local or projected-view data.
Bucket-global content needs a separately qualified physical export.

Clients opt in with the `X-Crab-CDN-Grant-Version: 1` header on
`/v1/credentials` and send their pinned `X-Crab-CDN-Approval-Sha256`
digest. For a locator that maps to multiple approved deployments, the
client sends its allowed digests as one comma-delimited header. The issuer
confirms the selected deployment digest in the response header when it
returns the `cdn` envelope. Missing approval pins leave the read on native
authorization. A conflicting explicit pin fails the request.
Viewer expiry is bounded by the origin credentials and the configured TTL,
with a maximum of 15 minutes. Clients select one credential by physical key
and keep it in memory.

### Local RustFS Smoke

For a local S3-compatible setup with real Crab CLI operations and no cloud
credentials, run the RustFS-backed E2E wrapper:

```bash
cd CrabBuild
crab/deploy/auth-service/scripts/e2e-rustfs-docker.sh
```

The wrapper builds `crab`, `crab-auth-receive`, and `crab-auth-view`; creates a
Python venv under `target/`; starts RustFS in Docker on `127.0.0.1:19000`;
starts local JWKS and Crab Auth servers; then runs the path-ACL E2E. It removes
the RustFS container on exit. Use `--keep-container` to inspect the object store
after a run, `--skip-build` after rebuilding manually, or `--port 19001` if
`19000` is busy.

If you already have RustFS or another S3-compatible server running, call the
lower-level script directly:

```bash
cargo build -p crab -p crab-auth-server --bins --no-default-features

export CRAB_AUTH_RUSTFS_ENDPOINT=http://127.0.0.1:9000
export CRAB_AUTH_RUSTFS_BUCKET=crab
export CRAB_AUTH_S3_ACCESS_KEY_ID=...
export CRAB_AUTH_S3_SECRET_ACCESS_KEY=...

crab/deploy/auth-service/scripts/e2e-path-acl-rustfs.py
```

The script starts a local Crab Auth endpoint, writes a policy with
`default_provider: s3`, creates a real Crab repository in the configured RustFS
bucket, and leaves generated user configs under the printed run directory. The
generated CLI config uses the enterprise issuer/client values:

```toml
[auth]
provider = "crab-auth"
issuer_url = "https://login.corp.example.com"
client_id = "crab-cli"
auth_endpoint = "http://127.0.0.1:<port>/v1/credentials"
token_cache_path = "~/.config/crab/tokens/"
```

For production, keep the same issuer/client values and replace `auth_endpoint`
with the reachable HTTPS endpoint for the deployed Crab Auth service.

## Protocol

Implements the crab-auth protocol as documented in
[Crab Auth endpoint protocol](https://crab.build/docs/cli/configuration/auth/enterprise-auth-crab-auth).

### Request

```
POST /v1/credentials
Authorization: Bearer <id_token>
Content-Type: application/json

{
  "id_token": "<jwt>",
  "repo_url": "crab://bucket/repo/path",
  "operation": "fetch",
  "client_version": "0.1.0"
}
```

### Response

```json
{
  "provider": "aws",
  "credentials": {
    "access_key_id": "ASIAXXX...",
    "secret_access_key": "...",
    "session_token": "...",
    "region": "us-west-2"
  },
  "expires_at": "2026-04-24T18:00:00Z",
  "permissions": ["read"]
}
```

Path-scoped reads include `storage_scope` and return credentials scoped to the
materialized filtered view. Clients must use the returned `repo_prefix` and
`global_prefix`; the original repo prefix and bucket-level `.crab` globals are
not readable with view credentials.

### Retain a projected mount view

`POST /v1/views/retention` takes the same `id_token` and source `repo_url` as
the credentials endpoint. The request also includes `view_prefix` from
`storage_scope.repo_prefix`, a fresh `reader_nonce` of 32 random bytes encoded
as lowercase hexadecimal, the `target` digest of the selected native storage
transport (also 32 bytes encoded as lowercase hexadecimal), and an `operation`
object.

| `operation.action` | Additional operation field | Successful response |
| --- | --- | --- |
| `acquire` | — | `acquired`, `repo_prefix`, `view_state_digest`, `revision`, `remaining_ms` |
| `renew` | `revision` from the preceding response | `renewed`, `revision`, `remaining_ms` |
| `release` | `revision` from the last acknowledged response | `released` |

The caller keeps the same nonce, native target, and projected view for the
life of the claim. Crab Auth checks the verified issuer, subject, and `mount`
policy on every operation. The native helper compares the target against its
resolved transport before accessing the claim and confirms that the source
still matches the view on acquisition. Renewal keeps the exact admitted view
while the same scope remains authorized, even if the source advances.

Use the request's monotonic start time plus the returned `remaining_ms` as
the local deadline. Stop serving new reads after denial, loss of the acknowledged
revision, or expiry. Drain outstanding reads before release. A `409` indicates
that the helper could not acknowledge the requested view or revision. A `503`
leaves its outcome uncertain. A subsequent `acquire` uses a fresh nonce.

For storage credential renewal, a direct mount sends `retained_view` with its
`mount` request to `/v1/credentials`. This object holds `view_prefix`, `target`,
`reader_nonce`, and the `incarnation` returned by acquisition. The issuer
evaluates the current mount policy and verifies that the exact native claim is
still active before issuing read-only credentials for the retained prefix. A
lease renewal can advance its sequence while a credential request is in flight.
The verified incarnation stays the same until that claim ends.

The helper runs with the issuer's native storage identity. For each deployed
view it needs the same source and view reads as materialization, plus writes
to the view's `v2/readers/` subtree and repository GC clock. AWS Lambda
operators must scope that native access to the function execution role. The
existing permission to assume the role for viewer credentials covers a
separate part of the request. The retention reply contains lease metadata only.

### Push Flow

Push uses the existing crab-auth service as a server-verified receive boundary:

1. `POST /v1/push/prepare` verifies the token, resolves repo-level push
   provider policy, snapshots the source ref state, prepares any filtered read
   view needed for the caller's current read scope, and returns short-lived
   staging-only immutable-write credentials.
   Clients can write only under `<repo>/staging/<push_id>/`.
2. The client uploads immutable push objects under the returned `upload_prefix`
   and writes `push-plan.json`.
3. `POST /v1/push/finalize` invokes the packaged `crab-auth-receive` helper.
   The helper reads staged data, verifies object hashes, computes changed paths
   from Git objects, and returns verified paths to Python.
4. Python rechecks policy against verified paths. If allowed, the helper
   promotes immutable objects and CAS-writes `{repo}/manifest`. Stale manifests
   return `409`.

Clients that use `/v1/credentials` for `operation: "push"` are rejected.

Prepare and finalize both use the same atomic ref-update list:

```json
{
  "id_token": "<jwt>",
  "repo_url": "crab://bucket/repo/path",
  "ref_updates": [
    {
      "ref_name": "refs/heads/main",
      "old_oid": "0123456789abcdef0123456789abcdef01234567",
      "new_oid": "89abcdef0123456789abcdef0123456789abcdef"
    }
  ],
  "client_version": "1.0.0"
}
```

Finalize includes the `push_id` returned by prepare.
`push_id` is a server-generated 32-character lowercase hex token and is the
only valid staging directory name under `<repo>/staging/`.

Clients do not send changed paths. Finalize enforces policy against paths
computed by `crab-auth-receive` from the staged Git objects, including both the
ref-tip tree delta and paths touched by commits introduced by the push.
Path-scoped push rules require at least one verified changed path; use a
non-path-scoped push rule for intentional ref-only or metadata-only updates.
Changed path lists must contain unique relative Git paths with no empty,
absolute, parent-directory, doubled-slash, trailing-slash, whitespace-padded,
or control-character components.
Protected push accepts branch ref updates only and the receive helper rejects
non-fast-forward updates server-side. Force/history-rewrite pushes require a
future explicit policy operation; they cannot be smuggled through staged data.
Implicit `--follow-tags` is rejected for protected Crab Auth pushes; push tag
refs explicitly so they are included in the prepare/finalize ref-update list.

### Active-active protected pushes

When a repository is configured for active-active replication, CrabAuth remains
the policy gate. The Crab client sends active-active context to
`/v1/push/finalize`; the service verifies the staged bundle and changed-path
policy first, then passes the approved context to `crab-auth-receive commit`.
The receive helper commits refs through the managed coordinator, materializes
the exact regional Capsule transaction, and returns `operation_id`,
`coordinator_epoch`, `writer_region`, `manifest_generation`, and `commit_state`
to the client. The managed v1 contract shipped in v1.2.4 retains
`manifest_generation: 0` for native Capsules; it does not expose the coordinator
commit sequence.

Set `CRAB_AUTH_ACTIVE_ACTIVE_CONFIG_JSON` on the CrabAuth service to the exact
approved active-active payload shape:

```json
{
  "replication": {
    "mode": "active-active",
    "coordinator": {
      "kind": "managed",
      "url": "dynamodb://crab-coordinator",
      "region": "us-west-2",
      "failover_regions": ["us-east-1"],
      "consistency": "linearizable"
    },
    "writers": [
      {
        "name": "west",
        "url": "crab://bucket/repo",
        "region": "us-west-2",
        "enabled": true
      }
    ]
  },
  "writer": "west"
}
```

If that variable is missing or does not exactly match the finalize payload,
active-active protected push fails closed with `403`. For local tests only,
`CRAB_AUTH_ACTIVE_ACTIVE_ALLOW_CLIENT_CONFIG=true` lets the service accept the
client-sent active-active config without a service-owned allowlist.

## Security

- **JWT signature verification**: All ID tokens are verified against the IdP's
  JWKS (RS256/ES256). Tokens with invalid signatures are rejected with 401.
- **Claims validation**: `iss`, `aud`, and `exp` are all verified.
- **Scoped credentials**: AWS uses inline session policies for repo/view reads
  and staging-only protected-push writes. Azure protected push returns one exact
  staging-write SAS. GCP uses Cloud Storage Credential Access Boundaries for
  repo/view reads and staging-only protected-push writes. Providers fail closed
  when they cannot issue credentials scoped to the requested read view or push
  staging prefix.
- **No secrets in logs**: Token values are never logged. Only identity (email/sub)
  and operation metadata appear in structured logs.
- **Rate limiting**: Built-in per-instance token bucket limiter returns `429`
  with `Retry-After` when clients exceed the configured rate.

## Development

```
cd crab/deploy/auth-service

# Install dependencies
pip install -r requirements.txt

# Local Python testing on this machine
./scripts/build-receive-helper.sh --host

# AWS Lambda zip packaging
./scripts/build-receive-helper.sh --linux-amd64

# Run locally
python -m src.app

# Run tests
pytest tests/ -v

# Lint
ruff check .
ruff format --check .
```

## File Structure

```
deploy/auth-service/
├── README.md                  This file
├── Dockerfile                 Standalone container
├── requirements.txt           Python dependencies
├── src/                       Application source
│   ├── __init__.py
│   ├── app.py                 FastAPI application
│   ├── auth.py                JWT verification
│   ├── policy.py              RBAC policy engine
│   ├── providers/             Cloud credential generators
│   │   ├── __init__.py
│   │   ├── aws.py             AWS STS AssumeRole
│   │   ├── gcp.py             GCP token generation
│   │   └── azure.py           Azure SAS/bearer generation
│   └── lambda_handler.py      AWS Lambda entry point
├── config/
│   └── policy.example.yaml    Example RBAC policy
├── terraform/                 AWS Lambda + API Gateway
│   ├── main.tf
│   ├── variables.tf
│   ├── outputs.tf
│   └── terraform.tfvars.example
├── sam/                       AWS SAM template
│   └── template.yaml
├── cloudrun/                  GCP Cloud Run config
│   └── Dockerfile
└── tests/                     Test suite
    ├── conftest.py
    ├── test_auth.py
    ├── test_policy.py
    └── test_providers.py
```
