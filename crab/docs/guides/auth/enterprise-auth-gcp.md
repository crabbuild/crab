# GCP Workload Identity Federation

Authenticate crab users via your corporate IdP and exchange OIDC tokens for
GCP credentials through Workload Identity Federation.

## Overview

The `"gcp-workload-identity"` provider implements this flow:

```
Developer → Corporate IdP → ID Token
ID Token → GCP STS (token exchange) → Federated Token
Federated Token → IAM Credentials (service account impersonation) → OAuth2 Token
OAuth2 Token → GCS Object Operations
```

No service account keys on developer machines. Credentials are short-lived and
scoped to the impersonated service account's permissions.

## Prerequisites

- crab installed (`make install`)
- An OIDC-compliant corporate IdP
- A GCP project with GCS buckets for crab repositories
- IAM permissions to create Workload Identity Pools (platform admin)

## Platform Admin Setup (One-Time)

### Step 1: Create a Workload Identity Pool

```bash
gcloud iam workload-identity-pools create crab-pool \
  --project=my-project \
  --location=global \
  --display-name="Crab Developer Pool"
```

### Step 2: Add an OIDC provider to the pool

```bash
gcloud iam workload-identity-pools providers create-oidc corp-idp \
  --project=my-project \
  --location=global \
  --workload-identity-pool=crab-pool \
  --issuer-uri=https://login.corp.example.com \
  --allowed-audiences=crab-cli-prod \
  --attribute-mapping="google.subject=assertion.sub,attribute.email=assertion.email"
```

### Step 3: Create a service account for crab

```bash
gcloud iam service-accounts create crab-dev \
  --project=my-project \
  --display-name="Crab Developer SA"
```

### Step 4: Grant GCS permissions to the service account

```bash
gsutil iam ch \
  serviceAccount:crab-dev@my-project.iam.gserviceaccount.com:objectAdmin \
  gs://ml-models
```

### Step 5: Allow the pool to impersonate the service account

```bash
gcloud iam service-accounts add-iam-policy-binding \
  crab-dev@my-project.iam.gserviceaccount.com \
  --project=my-project \
  --role=roles/iam.workloadIdentityUser \
  --member="principalSet://iam.googleapis.com/projects/123456/locations/global/workloadIdentityPools/crab-pool/*"
```

### Step 6: Register the crab CLI app at your IdP

Same as the AWS guide — register a public client with authorization code (PKCE)
and device code grants. Note the `client_id`.

### Step 7: Distribute configuration

Share with your team:

| Key | Value |
|-----|-------|
| `issuer_url` | `https://login.corp.example.com` |
| `client_id` | `crab-cli-prod` |
| `workload_identity_pool` | `projects/123456/locations/global/workloadIdentityPools/crab-pool/providers/corp-idp` |
| `service_account` | `crab-dev@my-project.iam.gserviceaccount.com` |
| `project_id` | `my-project` |

## Developer Setup

### Step 1: Configure crab

```toml
# ~/.config/crab/config.toml
[auth]
provider = "gcp-workload-identity"
issuer_url = "https://login.corp.example.com"
client_id = "crab-cli-prod"

[auth.gcp]
workload_identity_pool = "projects/123456/locations/global/workloadIdentityPools/crab-pool/providers/corp-idp"
service_account = "crab-dev@my-project.iam.gserviceaccount.com"
project_id = "my-project"
```

### Step 2: Log in

```bash
crab login
```

```
Authenticated as alice@corp.example.com (gcp-workload-identity)
```

### Step 3: Verify

```bash
crab auth status
```

```
Provider:     gcp-workload-identity
Identity:     alice@corp.example.com
Token expiry: 2026-04-24T18:30:00Z (52 minutes remaining)
Refresh:      yes
WI pool:      projects/123456/locations/global/workloadIdentityPools/crab-pool/providers/corp-idp
Service acct: crab-dev@my-project.iam.gserviceaccount.com
Project:      my-project
```

### Step 4: Use crab

```bash
git clone crab://ml-models/team-alpha/gpt4
crab hydrate --all
```

## Configuration Reference

### `[auth.gcp]` keys

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `workload_identity_pool` | string | (required) | Full resource name of the WIF pool provider |
| `service_account` | string | (required) | Service account email to impersonate |
| `project_id` | string | — | GCP project ID (informational) |

### Audience derivation

The WIF audience is derived automatically from the `workload_identity_pool`
value. If the pool resource name doesn't start with `//iam.googleapis.com/`,
crab prepends it:

```
Input:  projects/123456/locations/global/workloadIdentityPools/pool/providers/idp
Output: //iam.googleapis.com/projects/123456/locations/global/workloadIdentityPools/pool/providers/idp
```

## Troubleshooting

### "GCP STS token exchange forbidden: Permission denied"

The Workload Identity Pool doesn't accept tokens from your IdP. Verify:

1. The OIDC provider is configured in the pool with the correct `issuer-uri`
2. The `allowed-audiences` includes your `client_id`
3. The attribute mapping is correct

### "GCP service account impersonation forbidden"

The pool doesn't have permission to impersonate the service account. Verify:

```bash
gcloud iam service-accounts get-iam-policy \
  crab-dev@my-project.iam.gserviceaccount.com
```

The output should include `roles/iam.workloadIdentityUser` for the pool.

## Related

- [Enterprise Auth Overview](enterprise-auth.md)
- [AWS OIDC](enterprise-auth-aws.md)
- [Azure Entra ID](enterprise-auth-azure.md)
- [Crab Auth](enterprise-auth-crab-auth.md)
