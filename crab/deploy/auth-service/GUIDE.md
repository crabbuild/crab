# Enterprise Onboarding Guide

Step-by-step setup for crab-auth. By the end of this guide, your team will
be able to `crab clone`, `crab push`, and `crab pull` with identity-based
access control — no shared AWS keys, no manual credential rotation.

**Time estimate**: 30–60 minutes for a platform engineer familiar with AWS and
their corporate IdP.

---

## Prerequisites

Before you start, confirm you have:

- [ ] An AWS account with S3 buckets for crab repositories
- [ ] An OIDC-compliant Identity Provider (Okta, Azure AD, Google Workspace, Keycloak, Auth0, etc.)
- [ ] Admin access to register a new OAuth2 application at your IdP
- [ ] AWS IAM permissions to create roles and policies
- [ ] Docker installed (for local testing) or Terraform/SAM CLI (for AWS deployment)
- [ ] `crab` CLI installed on at least one developer machine (`make install` from the crab directory)

---

## Architecture Overview

```
┌────────────────────────────────────────────────────────────────────┐
│                        Your Infrastructure                         │
│                                                                    │
│  ┌─────────┐    ┌──────────┐    ┌───────────┐    ┌──────────────┐  │
│  │Developer│───▶│ Your IdP │───▶│ crab-auth │───▶│ AWS STS      │  │
│  │ laptop  │    │(Okta/AAD)│    │(Lambda/   │    │              │  │
│  │         │    │          │    │ Container)│    │ AssumeRole   │  │
│  │crab CLI │◀───│ ID Token │    │           │◀───│ Scoped Creds │  │
│  │         │    └──────────┘    │ Verifies  │    └──────────────┘  │
│  │         │◀───────────────────│ token,    │                      │
│  │         │   Read/staging     │ checks    │    ┌──────────────┐  │
│  │         │◀───────────────────│ policy,   │    │ S3 Bucket    │  │
│  │         │   scoped creds     │ receives  │    │ (crab repos) │  │
│  │         │───────────────────▶│ pushes    │    │              │  │
│  └─────────┘                    └───────────┘    └──────────────┘  │
│                                                                    │
└────────────────────────────────────────────────────────────────────┘
```

**Data flow**:
1. Developer runs `crab login` → browser opens → authenticates at your IdP
2. IdP returns a signed JWT (ID token) → stored encrypted on developer's machine
3. Developer runs `crab push` or `git push` → the client calls
   `/v1/push/prepare` with repo and refs
4. crab-auth verifies the JWT signature, snapshots source ref state, checks
   RBAC policy, and returns short-lived staging-only immutable-write credentials
5. CLI uploads staged immutable push data, then calls `/v1/push/finalize`
6. crab-auth verifies staged Git objects and changed paths, rechecks policy,
   and commits `{repo}/manifest` with service-owned credentials

**Key point**: crab-auth never stores repo data and never hands push clients
canonical mutable write credentials. Fetch/clone still use scoped credentials;
push uses a server-verified receive flow.

---

## Step 1: Register the crab CLI at Your IdP

You need to create an OAuth2/OIDC application registration so the crab CLI
can authenticate your developers.

### What to configure

| Setting | Value | Why |
|---------|-------|-----|
| Application type | Public client (native/SPA) | CLI apps can't keep a client secret |
| Grant types | Authorization Code + PKCE, Device Code | Desktop login + headless/SSH login |
| Redirect URI | `http://127.0.0.1:*/callback` | Local callback for auth code flow |
| Scopes | `openid email profile` | Minimum claims needed |
| Token claims | Include `email`, `groups` | Used for RBAC policy matching |

### IdP-specific instructions

<details>
<summary><strong>Okta</strong></summary>

1. Go to **Applications → Create App Integration**
2. Select **OIDC - OpenID Connect** → **Native Application**
3. Name: `crab-cli`
4. Grant types: ✅ Authorization Code, ✅ Device Authorization
5. Sign-in redirect URIs: `http://127.0.0.1:*/callback`
6. Assignments: Assign to the groups that should use crab
7. Note the **Client ID** and your **Okta domain** (e.g., `https://yourcompany.okta.com`)

To include groups in the ID token:
1. Go to **Security → API → Authorization Servers → default**
2. **Claims → Add Claim**: name=`groups`, value type=Groups, filter=Matches regex `.*`
3. Include in: ID Token, Always

</details>

<details>
<summary><strong>Azure AD (Entra ID)</strong></summary>

1. Go to **Azure Portal → App registrations → New registration**
2. Name: `crab-cli`
3. Supported account types: Single tenant (your org only)
4. Redirect URI: Public client → `http://127.0.0.1`
5. Under **Authentication**: ✅ Allow public client flows (for device code)
6. Under **Token configuration → Add groups claim**: Select "Security groups"
7. Note the **Application (client) ID** and **Directory (tenant) ID**

Your issuer URL is: `https://login.microsoftonline.com/{tenant-id}/v2.0`
JWKS URL: `https://login.microsoftonline.com/{tenant-id}/discovery/v2.0/keys`

</details>

<details>
<summary><strong>Google Workspace</strong></summary>

1. Go to **Google Cloud Console → APIs & Services → Credentials**
2. **Create Credentials → OAuth client ID**
3. Application type: Desktop app
4. Name: `crab-cli`
5. Note the **Client ID**

Your issuer URL is: `https://accounts.google.com`
JWKS URL: `https://www.googleapis.com/oauth2/v3/certs`

Note: Google doesn't support device code flow for custom apps. Developers
will use the browser-based authorization code flow.

</details>

<details>
<summary><strong>Keycloak</strong></summary>

1. Go to your realm → **Clients → Create client**
2. Client ID: `crab-cli`
3. Client authentication: OFF (public client)
4. Authentication flow: ✅ Standard flow, ✅ Device authorization grant
5. Valid redirect URIs: `http://127.0.0.1:*`
6. Under **Client scopes → crab-cli-dedicated → Add mapper**:
   - Type: Group Membership
   - Name: `groups`
   - Token Claim Name: `groups`
   - Add to ID token: ON

Your issuer URL is: `https://keycloak.yourcompany.com/realms/your-realm`

</details>

### Verify your IdP setup

After registration, confirm the discovery endpoint works:

```bash
# Replace with your issuer URL
curl -s https://yourcompany.okta.com/.well-known/openid-configuration | python3 -m json.tool
```

You should see `authorization_endpoint`, `token_endpoint`, and `jwks_uri` in the response.

**Record these values** — you'll need them in Step 3:
- **Issuer URL**: `https://yourcompany.okta.com` (or equivalent)
- **Client ID**: The client ID from your app registration
- **JWKS URL**: Usually `{issuer_url}/.well-known/jwks.json` or from the discovery doc

---

## Step 2: Create the AWS IAM Role

crab-auth needs an IAM role it can assume to generate scoped credentials for
your developers. This role should have broad S3 access — the inline session
policy (generated per-request) scopes it down to the specific repo.

### 2a. Create the base role

```bash
# Create the trust policy — allows the crab-auth Lambda/container to assume this role
cat > trust-policy.json << 'EOF'
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Principal": {
        "AWS": "arn:aws:iam::YOUR_ACCOUNT_ID:root"
      },
      "Action": "sts:AssumeRole",
      "Condition": {
        "StringEquals": {
          "sts:ExternalId": "crab-auth"
        }
      }
    }
  ]
}
EOF

# Create the role
aws iam create-role \
  --role-name crab-auth-base \
  --assume-role-policy-document file://trust-policy.json \
  --description "Base role for crab-auth"
```

Set `CRAB_AUTH_AWS_EXTERNAL_ID=crab-auth` in the auth service deployment when
you keep this trust-policy condition.

### 2b. Attach S3 permissions

```bash
# Create the permission policy — access to your crab buckets
cat > s3-policy.json << 'EOF'
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "s3:GetObject",
        "s3:HeadObject",
        "s3:PutObject",
        "s3:DeleteObject",
        "s3:ListBucket",
        "s3:AbortMultipartUpload",
        "s3:ListMultipartUploadParts"
      ],
      "Resource": [
        "arn:aws:s3:::YOUR-CRAB-BUCKET",
        "arn:aws:s3:::YOUR-CRAB-BUCKET/*"
      ]
    }
  ]
}
EOF

aws iam put-role-policy \
  --role-name crab-auth-base \
  --policy-name crab-s3-access \
  --policy-document file://s3-policy.json
```

Replace `YOUR-CRAB-BUCKET` with your actual bucket name(s). If you have
multiple buckets, add them all to the Resource array.

### 2c. Note the role ARN

```bash
aws iam get-role --role-name crab-auth-base --query 'Role.Arn' --output text
# Output: arn:aws:iam::123456789012:role/crab-auth-base
```

**Record this value** — you'll need it in Step 3.

---

## Step 3: Write Your RBAC Policy

Create a `policy.yaml` that defines who can access which repos.

```bash
cd crab/deploy/auth-service
cp config/policy.example.yaml config/policy.yaml
```

Edit `config/policy.yaml`:

```yaml
version: "1"
default_provider: aws

rules:
  # Platform team gets full access.
  - group: "platform-team"
    repos: ["*"]
    operations: ["*"]

  # ML engineers can push/fetch their model repos.
  - group: "ml-engineers"
    repos:
      - "models/*"
      - "datasets/*"
    operations: ["push", "fetch", "clone", "hydrate", "pull", "gc"]

  # Data scientists get read-only access to datasets.
  - group: "data-science"
    repos: ["datasets/*"]
    operations: ["fetch", "clone", "hydrate", "pull"]

  # CI service account can push releases.
  - identity: "ci@yourcompany.com"
    repos: ["releases/*"]
    operations: ["push", "fetch", "clone"]

  # Everyone in the org can read shared assets.
  - identity: "*"
    repos: ["shared/*"]
    operations: ["fetch", "clone", "hydrate", "pull"]

# Explicit denials (checked before allow rules).
deny: []
```

**Tips**:
- `repos` patterns use glob matching (`*` = any characters)
- `identity` matches the `email` claim from the ID token
- `group` matches entries in the `groups` claim array
- Operations: `push`, `gc`, `repack`, `compact`, `lock`, `lfs`, `metadb`,
  `optimize-xorbs`, `tier`, `workflow-push-cache`, `fetch`, `clone`,
  `clone:shard-sync`, `hydrate`, `pull`, `mount`, `fsck`, `du`, `doctor`,
  `smudge`, `ship:manifest-check`, `prune`, `diff`, `workflow-cache-pull`
- Deny rules are checked before allow rules
- Matching allow rules are unioned; provider disagreement fails closed

---

## Step 4: Deploy crab-auth

Choose one deployment method:

### Option A: Docker (any environment)

Best for: Kubernetes, ECS, any container platform.

```bash
cd CrabBuild

docker build -f crab/deploy/auth-service/Dockerfile -t crab-auth .

docker run -d \
  --name crab-auth \
  -p 8080:8080 \
  -v $(pwd)/crab/deploy/auth-service/config:/etc/crab-auth:ro \
  -e CRAB_AUTH_JWKS_URL="https://yourcompany.okta.com/.well-known/jwks.json" \
  -e CRAB_AUTH_ISSUER="https://yourcompany.okta.com" \
  -e CRAB_AUTH_AUDIENCE="your-client-id" \
  -e CRAB_AUTH_AWS_ROLE_ARN="arn:aws:iam::123456789012:role/crab-auth-base" \
  -e CRAB_AUTH_AWS_EXTERNAL_ID="crab-auth" \
  -e CRAB_AUTH_AWS_REGION="us-west-2" \
  crab-auth
```

Verify it's running:

```bash
curl http://localhost:8080/health
# {"status":"ok"}

curl http://localhost:8080/ready
# {"status":"ok","auth_config":"ok","policy":"ok","provider_config":"ok","jwks":"ok","receive_helper":"ok","view_helper":"ok",...}
```

`/health` only proves the HTTP process is alive. `/ready` validates auth
environment, policy loading, provider configuration, JWKS signing-key
availability, packaged helper binaries, and `git` availability.

### Option B: AWS Lambda + Terraform

Best for: Serverless, zero maintenance, auto-scaling.

```bash
cd crab/deploy/auth-service/terraform

cp terraform.tfvars.example terraform.tfvars
```

Edit `terraform.tfvars`:

```hcl
aws_region       = "us-west-2"
jwks_url         = "https://yourcompany.okta.com/.well-known/jwks.json"
issuer           = "https://yourcompany.okta.com"
audience         = "your-client-id"
auth_role_arn    = "arn:aws:iam::123456789012:role/crab-auth-base"
git_layer_arn    = "arn:aws:lambda:us-west-2:123456789012:layer:git:1"
session_duration = 3600
log_level        = "INFO"
```

Terraform builds the Lambda zip locally, including Python dependencies and the
Linux `crab-auth-receive` and `crab-auth-view` helpers for
`lambda_architecture`. Docker must be running because the helpers are compiled
in a Linux builder image. The package step runs on each apply so a clean
checkout with remote Terraform state cannot reuse a missing or stale local zip.
The `git_layer_arn` layer must be built for the same `lambda_architecture` and
must provide `/opt/bin/git`; protected push verification and path-scoped
read-view materialization both use Git plumbing.

Deploy:

```bash
terraform init
terraform plan    # Review what will be created
terraform apply   # Deploy
```

Terraform outputs the API endpoint URL:

```
api_endpoint = "https://abc123.execute-api.us-west-2.amazonaws.com"
```

### Option C: AWS SAM

```bash
cd crab/deploy/auth-service/sam
../scripts/build-receive-helper.sh --linux-amd64
sam build
sam deploy --guided
```

Follow the prompts to provide your parameter values, including `GitLayerArn`
for a Lambda layer that contains `/opt/bin/git`.

---

## Step 5: Verify the Deployment

Test the full flow before distributing to your team.

### 5a. Test health endpoint

```bash
# Replace with your deployed URL
export AUTH_URL="https://abc123.execute-api.us-west-2.amazonaws.com"

curl $AUTH_URL/health
# {"status":"ok"}

curl $AUTH_URL/ready
# {"status":"ok","auth_config":"ok","policy":"ok","provider_config":"ok","jwks":"ok","receive_helper":"ok","view_helper":"ok",...}
```

Do not distribute the endpoint for protected push until `/ready` returns
`status: ok`. A `503` means required auth configuration, policy, provider
configuration, JWKS, helper binaries, or `git` are unavailable.

### 5b. Get a test token from your IdP

Use the crab CLI to authenticate:

```bash
# Temporarily configure crab to point at your new endpoint
cat > ~/.config/crab/config.toml << EOF
[auth]
provider = "crab-auth"
issuer_url = "https://yourcompany.okta.com"
client_id = "your-client-id"
auth_endpoint = "$AUTH_URL/v1/credentials"
EOF

# Login — this opens your browser
crab login
```

You should see:

```
Authenticated as alice@yourcompany.com (crab-auth)
```

### 5c. Test Crab Auth

```bash
crab auth status
```

Expected output:

```
Provider:     crab-auth
Identity:     alice@yourcompany.com
Token expiry: 2026-05-27T19:30:00Z (58 minutes remaining)
Refresh:      yes
Endpoint:     https://abc123.execute-api.us-west-2.amazonaws.com/v1/credentials
```

### 5d. Test actual repo access

```bash
# Clone a repo (replace with your actual bucket/repo)
crab clone crab://your-bucket/shared/test-repo

# If this succeeds, auth is working end-to-end
```

### 5e. Test access denial

Try an operation your policy should deny:

```bash
# If your user isn't in the platform-team group and the repo isn't in your allowed list:
crab clone crab://your-bucket/secret/forbidden-repo
# Expected: error about access denied
```

---

## Step 6: Distribute to Your Team

### 6a. Create a shared config file

Create a `crab.toml` at the root of your crab repositories (or distribute
via your config management system):

```toml
# crab.toml — checked into the repo or distributed via config management
[auth]
provider = "crab-auth"
issuer_url = "https://yourcompany.okta.com"
client_id = "your-client-id"
auth_endpoint = "https://abc123.execute-api.us-west-2.amazonaws.com/v1/credentials"
```

Or instruct developers to add to their global config:

```bash
# ~/.config/crab/config.toml
[auth]
provider = "crab-auth"
issuer_url = "https://yourcompany.okta.com"
client_id = "your-client-id"
auth_endpoint = "https://abc123.execute-api.us-west-2.amazonaws.com/v1/credentials"
```

### 6b. Developer onboarding instructions

Send this to your team:

```
## Getting Started with Crab

1. Install crab:
   cd crab && make install

2. Log in (one-time, opens browser):
   crab login

3. Clone a repository:
   crab clone crab://our-bucket/team/repo-name

4. Work normally with git. Large files are handled automatically.

5. Push changes:
   crab push

For crab-auth repositories, `crab push` and `git push` always use
`/v1/push/prepare` and `/v1/push/finalize`. Direct `/v1/credentials` requests
with `operation=push` are rejected.

Your credentials refresh automatically. If you see an auth error, run:
   crab login
```

### 6c. CI/CD setup

For headless environments (CI runners, SSH sessions):

```bash
# Option 1: Device code flow (interactive, for SSH sessions)
crab login --headless

# Option 2: Pre-authenticated token (for CI)
# Store the refresh token as a CI secret, then:
export CRAB_AUTH_TOKEN="<refresh-token-from-ci-secret>"
# The CLI will use this to obtain credentials without interactive login
```

For fully automated CI, you may prefer the `static` provider with
environment-based AWS credentials instead of the crab-auth flow.

---

## Step 7: Monitor and Maintain

### View logs

**Docker**:
```bash
docker logs crab-auth --follow
```

**Lambda**:
```bash
aws logs tail /aws/lambda/crab-auth --follow
```

### What to monitor

| Metric | Alert threshold | Meaning |
|--------|----------------|---------|
| HTTP 401 rate | >10/min | Expired tokens not refreshing, or IdP issue |
| HTTP 403 rate | Spike | Policy misconfiguration or unauthorized access attempts |
| HTTP 5xx rate | Any | crab-auth or STS is down |
| Latency p99 | >2s | STS or JWKS endpoint slow |
| Credential issuance rate | Baseline ±50% | Unusual activity |

### Update the RBAC policy

Edit `config/policy.yaml` and restart the service:

```bash
# Docker
docker restart crab-auth

# Lambda — redeploy
cd terraform && terraform apply
```

### Rotate the IAM role

If you need to rotate the base role:

1. Create a new role with the same permissions
2. Update `CRAB_AUTH_AWS_ROLE_ARN` in your deployment
3. Restart/redeploy crab-auth
4. Delete the old role after confirming the new one works

Developer-facing credentials rotate automatically (they're short-lived STS
tokens). No developer action needed.

---

## Troubleshooting

### "crab login" opens browser but fails

**Cause**: The redirect URI doesn't match what's registered at your IdP.

**Fix**: Ensure your IdP app registration has `http://127.0.0.1:*/callback`
(with wildcard port) as an allowed redirect URI. Some IdPs require exact port
matching — in that case, you may need to add several ports or use the device
code flow (`crab login --headless`).

### "Crab Auth endpoint returned 401"

**Cause**: The ID token failed signature verification.

**Check**:
1. Is `CRAB_AUTH_JWKS_URL` correct? `curl` it and verify you get a JSON response with `keys`.
2. Is `CRAB_AUTH_ISSUER` exactly matching the `iss` claim in your tokens? (No trailing slash difference)
3. Is `CRAB_AUTH_AUDIENCE` matching the `aud` claim? (Must be your client_id)
4. Has the token expired? Run `crab auth status` to check expiry.

**Fix**: Run `crab logout && crab login` to get a fresh token.

### "Crab Auth endpoint returned 403"

**Cause**: Your RBAC policy denied the request.

**Check**:
1. What identity is the token using? `crab auth status` shows the email.
2. What groups does the token contain? Check your IdP's token preview/debugger.
3. Does your `policy.yaml` have a rule matching that identity/group + repo + operation?

**Fix**: Update `policy.yaml` to add the appropriate rule, then restart crab-auth.

### "Crab Auth request failed" (connection error)

**Cause**: The crab-auth endpoint is unreachable.

**Check**:
1. Is the service running? `curl $AUTH_URL/health`
2. Is protected-push readiness healthy? `curl $AUTH_URL/ready`
3. Is the URL in your crab config correct? `cat ~/.config/crab/config.toml`
4. Are there network/firewall issues between the developer and the endpoint?

### "An error occurred (AccessDenied) when calling the AssumeRole operation"

**Cause**: The crab-auth service can't assume the IAM role.

**Check**:
1. Does the Lambda/container's execution role have `sts:AssumeRole` permission for the base role?
2. Does the base role's trust policy allow the execution role to assume it?
3. Is the role ARN in `CRAB_AUTH_AWS_ROLE_ARN` correct?

### "NoCredentials" error

**Cause**: No cached tokens found. The developer hasn't logged in.

**Fix**: Run `crab login`.

### Tokens expire too quickly

**Default**: ID tokens typically expire in 1 hour (set by your IdP).
Refresh tokens last longer (days/weeks, set by your IdP).

The crab CLI automatically refreshes tokens using the refresh token. If
refresh tokens are disabled at your IdP, developers will need to `crab login`
more frequently.

**Recommendation**: Enable refresh tokens at your IdP with a 7-day lifetime.

---

## Security Checklist

Before going to production, verify:

- [ ] crab-auth is deployed in a private subnet (not publicly accessible) or behind a WAF
- [ ] HTTPS is enforced (API Gateway does this by default; for Docker, use a reverse proxy)
- [ ] `/ready` returns `status: ok` in the deployed runtime
- [ ] The IAM base role follows least-privilege (only the S3 buckets you need)
- [ ] The IAM base role trust policy restricts who can assume it
- [ ] `policy.yaml` has no wildcard rules that grant more access than intended
- [ ] CloudWatch/monitoring alerts are configured for 401/403/5xx spikes
- [ ] The JWKS URL is your IdP's actual endpoint (not a test/dev instance)
- [ ] Refresh tokens have a reasonable lifetime at your IdP (7–30 days)
- [ ] You've tested access denial (not just access grant)
- [ ] You've tested with a user who has left the org (should be denied)

---

## Appendix: Full Configuration Reference

### crab CLI config (`~/.config/crab/config.toml`)

```toml
[auth]
# Required: which auth provider to use
provider = "crab-auth"

# Required: your IdP's OIDC issuer URL
issuer_url = "https://yourcompany.okta.com"

# Required: the OAuth2 client_id registered for crab
client_id = "0oa1b2c3d4e5f6g7h8i9"

# Required: the crab-auth endpoint URL
auth_endpoint = "https://abc123.execute-api.us-west-2.amazonaws.com/v1/credentials"

# Optional: OIDC scopes (default: "openid email profile")
scopes = "openid email profile groups"

# Optional: token cache location (default: ~/.config/crab/tokens)
token_cache_path = "~/.config/crab/tokens"
```

### crab-auth environment variables

| Variable | Required | Description |
|----------|----------|-------------|
| `CRAB_AUTH_JWKS_URL` | Yes | Your IdP's JWKS endpoint |
| `CRAB_AUTH_ISSUER` | Yes | Expected `iss` claim value |
| `CRAB_AUTH_AUDIENCE` | Yes | Expected `aud` claim (your client_id) |
| `CRAB_AUTH_POLICY_PATH` | No | Path to policy.yaml (default: `/etc/crab-auth/policy.yaml`) |
| `CRAB_AUTH_AWS_ROLE_ARN` | Yes (AWS) | IAM role to assume |
| `CRAB_AUTH_AWS_EXTERNAL_ID` | If trust policy requires it | STS ExternalId sent when assuming `CRAB_AUTH_AWS_ROLE_ARN` |
| `CRAB_AUTH_AWS_REGION` | No | AWS region (default: `us-east-1`) |
| `CRAB_AUTH_SESSION_DURATION` | No | Credential lifetime in seconds (default: `3600`) |
| `CRAB_AUTH_LOG_LEVEL` | No | `DEBUG`, `INFO`, `WARNING`, `ERROR` (default: `INFO`) |
| `CRAB_AUTH_DRY_RUN` | No | `true` for testing without real AWS (returns synthetic creds) |
| `CRAB_AUTH_RATE_LIMIT_PER_MINUTE` | No | Per-instance credential request refill rate (default: `120`) |
| `CRAB_AUTH_RATE_LIMIT_BURST` | No | Per-instance token bucket burst size (default: `30`) |
| `CRAB_AUTH_RATE_LIMIT_MAX_KEYS` | No | Maximum tracked client rate-limit keys per instance (default: `10000`) |
| `CRAB_AUTH_TRUST_PROXY_HEADERS` | No | Trust `X-Forwarded-For`/`X-Real-IP` for rate limit keys (default: `false`) |
| `CRAB_AUTH_RECEIVE_HELPER` | No | Path to packaged `crab-auth-receive` helper |
| `CRAB_AUTH_VIEW_HELPER` | No | Path to packaged `crab-auth-view` helper |
| `CRAB_AUTH_RECEIVE_TIMEOUT_SECONDS` | No | Receive helper timeout for push finalize (default: `300`) |
| `CRAB_AUTH_STAGING_TTL_SECONDS` | No | Best-effort cleanup TTL for abandoned push staging objects (default: `86400`, `0` disables) |

Provider scoping:
- AWS credentials are scoped with inline STS session policies: repo/view read for read operations, and staging-only immutable write for protected push.
- Azure uses blob and directory-scoped SAS tokens. Protected push returns one exact staging-write SAS token; canonical read/write happens only through the service-owned finalize flow.
- GCP uses Cloud Storage Credential Access Boundaries for repo/view reads and staging-only protected-push writes.
- Repo URLs must include a non-empty repo prefix such as `crab://bucket/team/repo`; bucket-root and container-root repo URLs are rejected.
- Push staging IDs are server-generated 32-character lowercase hex tokens.

### Policy file schema

```yaml
version: "1"                    # Required, must be "1"
default_provider: aws           # "aws", "gcp", or "azure"

rules:                          # Matching allows are unioned by identity/repo/operation
  - identity: "user@example.com"  # Match by email (or "*" for any)
    # OR
    group: "team-name"           # Match by group membership
    repos:                       # Glob patterns for repo paths
      - "prefix/*"
    operations:                  # Allowed operations (or ["*"] for all)
      - "push"
      - "fetch"
      - "clone"
    read_paths:                 # Optional read-view path globs
      - "src/**"
      - "README.md"
    write_paths:                # Optional verified changed-path globs for push
      - "src/**"
      - "README.md"
    provider: aws                # Optional: override default_provider

deny:                           # Deny rules always win
  - identity: "banned@example.com"
    repos: ["*"]
    operations: ["*"]
  - identity: "*"
    repos: ["*"]
    operations: ["push"]
    write_paths: ["secrets/**"]
  - identity: "*"
    repos: ["*"]
    operations: ["fetch", "clone", "hydrate"]
    read_paths: ["secrets/**"]
```
