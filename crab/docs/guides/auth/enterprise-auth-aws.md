# AWS OIDC Authentication

Authenticate crab users via your corporate IdP and exchange OIDC tokens for
short-lived AWS STS credentials.

## Overview

The `"aws-oidc"` provider implements this flow:

```
Developer → Corporate IdP (Okta, Entra, etc.) → ID Token
ID Token → AWS STS AssumeRoleWithWebIdentity → Temporary S3 Credentials
Temporary Credentials → S3 Object Operations
```

No long-lived IAM access keys. Credentials expire automatically (default: 1
hour). CloudTrail logs attribute operations to the authenticated user.

## Prerequisites

- crab installed (`make install`)
- An OIDC-compliant corporate IdP (Okta, Azure AD, Google Workspace, Keycloak, Auth0)
- An AWS account with S3 buckets for crab repositories
- IAM permissions to create OIDC providers and roles (platform admin)

## Platform Admin Setup (One-Time)

These steps are performed once by the platform administrator.

### Step 1: Register the OIDC provider in IAM

```bash
aws iam create-open-id-connect-provider \
  --url https://login.corp.example.com \
  --client-id-list crab-cli-prod \
  --thumbprint-list <idp-thumbprint>
```

To get the thumbprint, fetch the IdP's TLS certificate chain and compute the
SHA-1 fingerprint of the root CA certificate.

### Step 2: Create an IAM role with a trust policy

Create `trust-policy.json`:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Principal": {
        "Federated": "arn:aws:iam::123456789012:oidc-provider/login.corp.example.com"
      },
      "Action": "sts:AssumeRoleWithWebIdentity",
      "Condition": {
        "StringEquals": {
          "login.corp.example.com:aud": "crab-cli-prod"
        }
      }
    }
  ]
}
```

```bash
aws iam create-role \
  --role-name crab-developer \
  --assume-role-policy-document file://trust-policy.json \
  --max-session-duration 43200
```

### Step 3: Attach an S3 policy

Create `s3-policy.json`:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "s3:GetObject",
        "s3:PutObject",
        "s3:DeleteObject",
        "s3:ListBucket"
      ],
      "Resource": [
        "arn:aws:s3:::ml-models",
        "arn:aws:s3:::ml-models/*"
      ]
    }
  ]
}
```

```bash
aws iam put-role-policy \
  --role-name crab-developer \
  --policy-name crab-s3-access \
  --policy-document file://s3-policy.json
```

### Step 4: Register the crab CLI app at your IdP

At your IdP (Okta, Entra, etc.), register a public client application:

- Application type: Native / Public client
- Redirect URI: `http://127.0.0.1/callback` (for authorization code flow)
- Grant types: Authorization Code (with PKCE), Device Code
- Scopes: `openid`, `email`, `profile`
- Note the `client_id` — you'll distribute this to developers

### Step 5: Distribute configuration to developers

Share the following values with your team:

| Key | Value |
|-----|-------|
| `issuer_url` | `https://login.corp.example.com` |
| `client_id` | `crab-cli-prod` |
| `role_arn` | `arn:aws:iam::123456789012:role/crab-developer` |
| `region` | `us-west-2` |

## Developer Setup

### Step 1: Configure crab

Create or edit `~/.config/crab/config.toml`:

```toml
[auth]
provider = "aws-oidc"
issuer_url = "https://login.corp.example.com"
client_id = "crab-cli-prod"

[auth.aws]
role_arn = "arn:aws:iam::123456789012:role/crab-developer"
region = "us-west-2"
session_duration_secs = 3600
```

Or use the CLI:

```bash
crab config set auth.provider aws-oidc
crab config set auth.issuer_url https://login.corp.example.com
crab config set auth.client_id crab-cli-prod
crab config set auth.aws.role_arn arn:aws:iam::123456789012:role/crab-developer
crab config set auth.aws.region us-west-2
```

### Step 2: Log in

On a desktop with a browser:

```bash
crab login
```

This opens your browser to the corporate IdP login page. After authenticating,
the browser redirects back to crab and you'll see:

```
Authenticated as alice@corp.example.com (aws-oidc)
```

Over SSH or in a headless environment:

```bash
crab login --headless
```

```
To authenticate, open this URL in a browser:
  https://login.corp.example.com/device

Enter code: ABCD-1234
```

Open the URL on any device, enter the code, and complete authentication.

### Step 3: Verify

```bash
crab auth status
```

```
Provider:     aws-oidc
Identity:     alice@corp.example.com
Token expiry: 2026-04-24T18:30:00Z (52 minutes remaining)
Refresh:      yes
AWS role:     arn:aws:iam::123456789012:role/crab-developer
Region:       us-west-2
```

```bash
crab doctor
```

```
  ✓ auth                     aws-oidc — alice@corp.example.com, expires 2026-04-24T18:30:00Z
  ✓ credentials              bucket 'ml-models' reachable
```

### Step 4: Use crab

```bash
git clone crab://ml-models/team-alpha/gpt4
crab hydrate --all
# ... work on files ...
crab add *.safetensors
git commit -m "update model weights"
git push
```

Credentials are refreshed automatically. A 30-minute push won't fail because
the 1-hour STS token expired — crab refreshes before expiry.

## Configuration Reference

### `[auth.aws]` keys

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `role_arn` | string | (required) | IAM role ARN to assume |
| `region` | string | `AWS_REGION` or `us-east-1` | STS endpoint region |
| `session_duration_secs` | int | `3600` | Session duration (900–43200) |

### Region resolution order

1. `auth.aws.region` config key
2. `AWS_REGION` environment variable
3. `us-east-1` (default)

### Session duration

The `session_duration_secs` value is clamped to the valid STS range:
- Minimum: 900 seconds (15 minutes)
- Maximum: 43200 seconds (12 hours)
- Also capped by the IAM role's `MaxSessionDuration` setting

### CloudTrail attribution

STS sessions are named `crab-{sha256(email)[:12]}` so CloudTrail events
can be traced back to the authenticated user without leaking PII in session
names.

## Troubleshooting

### "STS AccessDenied: Not authorized to perform sts:AssumeRoleWithWebIdentity"

The IAM role's trust policy doesn't accept tokens from your IdP. Verify:

1. The OIDC provider is registered: `aws iam list-open-id-connect-providers`
2. The trust policy's `Federated` ARN matches the provider
3. The `aud` condition matches your `client_id`

### "STS InvalidIdentityToken"

The ID token is malformed or the IdP's signing key has rotated. Try:

```bash
crab logout
crab login
```

### "STS ExpiredTokenException"

The ID token expired before the STS call completed. Crab auto-retries once
with a refreshed token. If this persists, check your IdP's token lifetime
settings (should be at least 5 minutes).

## Related

- [Enterprise Auth Overview](enterprise-auth.md)
- [GCP Workload Identity](enterprise-auth-gcp.md)
- [Azure Entra ID](enterprise-auth-azure.md)
- [Crab Auth](enterprise-auth-crab-auth.md)
