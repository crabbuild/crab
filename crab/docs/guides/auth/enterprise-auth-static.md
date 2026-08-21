# Static / Multi-Cloud Authentication

Use provider-supported environment or workload credentials with S3, GCS, or
Azure Blob Storage.

## Overview

The `"static"` provider is the default. Crab reads credentials from
environment variables or provider-native workload credentials. No Crab login
flow or Crab-managed token cache is involved.

This guide covers how to use static credentials with each cloud backend.

## Prerequisites

- crab installed (`make install` from the repo root)
- A cloud storage bucket (S3, GCS, or Azure) with appropriate permissions

## Step 1: Choose your storage provider

The `storage_provider` key tells crab which cloud backend to use:

| Value | Backend | Builder |
|-------|---------|---------|
| `"s3"` | Amazon S3 (default) | `AmazonS3Builder::from_env()` |
| `"gcs"` | Google Cloud Storage | `GoogleCloudStorageBuilder::from_env()` |
| `"azure"` | Azure Blob Storage | `MicrosoftAzureBuilder::from_env()` |
| `"auto"` | Auto-detect from env | Reads `CRAB_STORAGE_PROVIDER` |

For a new repository, pass it directly to init:

```bash
crab init --storage-provider s3    crab://my-s3-bucket/my-repo
crab init --storage-provider gcs   crab://my-gcs-bucket/my-repo
crab init --storage-provider azure crab://my-container/my-repo
```

`crab init` writes the choice to `.crab.toml` so collaborators use the same
backend after cloning.

## Step 2: Configure

### Option A: S3 (default — no config needed)

If you're using S3, you don't need to change anything. The default config is:

```toml
# ~/.config/crab/config.toml (or .crab/config.toml)
[auth]
provider = "static"
storage_provider = "s3"
```

Prefer web identity or an attached ECS/EC2 role for teams and CI:

```bash
export AWS_WEB_IDENTITY_TOKEN_FILE=/var/run/secrets/oidc-token
export AWS_ROLE_ARN=arn:aws:iam::123456789012:role/crab-writer
```

Temporary or static environment credentials are also accepted:

```bash
export AWS_ACCESS_KEY_ID=AKIA...
export AWS_SECRET_ACCESS_KEY=wJalr...
export AWS_SESSION_TOKEN=... # required for temporary STS credentials
export AWS_REGION=us-west-2
```

The current S3 provider does not read `AWS_PROFILE`, `~/.aws/config`, or
`~/.aws/credentials`. Export the temporary credentials produced by an SSO or
profile workflow into the Crab process.

### Option B: Google Cloud Storage

```toml
[auth]
provider = "static"
storage_provider = "gcs"
```

Set GCS credentials:

```bash
# Application Default Credentials (recommended):
gcloud auth application-default login

# Or explicit service account key:
export GOOGLE_APPLICATION_CREDENTIALS=/path/to/service-account.json
```

### Option C: Azure Blob Storage

```toml
[auth]
provider = "static"
storage_provider = "azure"
```

Set Azure credentials (any one of these):

```bash
# Account key:
export AZURE_STORAGE_ACCOUNT_NAME=myaccount
export AZURE_STORAGE_ACCOUNT_KEY=base64key...

# Workload identity:
export AZURE_STORAGE_ACCOUNT_NAME=myaccount
export AZURE_CLIENT_ID=...
export AZURE_TENANT_ID=...
export AZURE_FEDERATED_TOKEN_FILE=/var/run/secrets/azure/tokens/azure-identity-token

# SAS token:
export AZURE_STORAGE_ACCOUNT_NAME=myaccount
export AZURE_STORAGE_SAS_TOKEN="sv=2024-11-04&ss=b&..."
```

### Option D: Auto-detect

```toml
[auth]
provider = "static"
storage_provider = "auto"
```

Set the `CRAB_STORAGE_PROVIDER` environment variable:

```bash
# S3 (default when unset):
export CRAB_STORAGE_PROVIDER=s3

# GCS:
export CRAB_STORAGE_PROVIDER=gcs   # also: gs, google

# Azure:
export CRAB_STORAGE_PROVIDER=azure  # also: az, abs
```

This is useful for CI/CD pipelines that deploy to different clouds.

## Step 3: Initialize and verify

```bash
# Initialize a repo (S3 example):
crab init --storage-provider s3 crab://my-bucket/my-repo
crab setup

# Verify credentials work:
crab doctor
```

Expected output:

```
crab doctor

  ✓ auth                     static (no crab-managed auth)
  ✓ credentials              bucket 'my-bucket' reachable
```

## Step 4: Use crab normally

```bash
crab track '*.bin'
crab add *.bin
git commit -m "add models"
git push
```

No `crab login` needed — credentials come from the environment.

## Local testing with MinIO

For local development without cloud access, use `provider = "none"`:

```toml
[auth]
provider = "none"
```

```bash
# Start MinIO:
docker run -p 9000:9000 -e MINIO_ROOT_USER=minioadmin \
  -e MINIO_ROOT_PASSWORD=minioadmin minio/minio server /data

# Point crab at MinIO:
export AWS_ACCESS_KEY_ID=minioadmin
export AWS_SECRET_ACCESS_KEY=minioadmin
export AWS_ENDPOINT_URL=http://localhost:9000

crab init --storage-provider s3 crab://test-bucket/my-repo
```

## Switching from static to federated auth

When your organization adopts federated auth, update the config:

```bash
crab config set auth.provider aws-oidc
crab config set auth.issuer_url https://login.corp.example.com
crab config set auth.client_id crab-cli-prod
crab login
```

See the [AWS OIDC guide](enterprise-auth-aws.md) for full setup.

## Related

- [Enterprise Auth Overview](enterprise-auth.md)
- [AWS OIDC](enterprise-auth-aws.md)
- [GCP Workload Identity](enterprise-auth-gcp.md)
- [Azure Entra ID](enterprise-auth-azure.md)
