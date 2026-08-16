# Azure Entra ID Authentication

Authenticate crab users via Entra ID (formerly Azure AD) for Azure Blob
Storage access.

## Overview

The `"azure-entra"` provider supports two modes:

**Direct bearer** (no Crab Auth endpoint):
```
Developer → Entra ID → ID Token (with Storage scope)
ID Token → Azure Blob Storage (as bearer token)
```

**With Crab Auth endpoint**:
```
Developer → Entra ID → ID Token
ID Token → Crab Auth Endpoint → SAS Token or Scoped Bearer
Token → Azure Blob Storage
```

The direct bearer mode is simpler but requires the Entra ID app registration
to include the `https://storage.azure.com/.default` scope. The Crab Auth
endpoint mode is more flexible and supports SAS tokens with fine-grained
permissions.

## Prerequisites

- crab installed (`make install`)
- An Azure subscription with a storage account
- Entra ID (Azure AD) tenant with admin access (platform admin)

## Platform Admin Setup (One-Time)

### Step 1: Register the crab CLI app in Entra ID

1. Go to Azure Portal → Entra ID → App registrations → New registration
2. Name: `crab-cli`
3. Supported account types: Accounts in this organizational directory only
4. Redirect URI: Public client/native, `http://127.0.0.1/callback`
5. Click Register

Note the **Application (client) ID** and **Directory (tenant) ID**.

### Step 2: Configure API permissions

1. Go to the app registration → API permissions → Add a permission
2. Select "Azure Storage" → Delegated permissions → `user_impersonation`
3. Click "Grant admin consent" for your organization

### Step 3: Enable device code flow

1. Go to the app registration → Authentication
2. Under "Advanced settings", set "Allow public client flows" to Yes
3. Save

### Step 4: Assign Storage Blob Data Contributor role

```bash
az role assignment create \
  --role "Storage Blob Data Contributor" \
  --assignee-object-id <user-or-group-object-id> \
  --scope /subscriptions/<sub-id>/resourceGroups/<rg>/providers/Microsoft.Storage/storageAccounts/<account>
```

Or for all users in the organization, assign the role to a security group.

### Step 5: Distribute configuration

Share with your team:

| Key | Value |
|-----|-------|
| `issuer_url` | `https://login.microsoftonline.com/<tenant-id>/v2.0` |
| `client_id` | `<application-client-id>` |
| `tenant_id` | `<directory-tenant-id>` |
| `storage_account` | `mlmodels` (optional, for SAS scoping) |

## Developer Setup

### Step 1: Configure crab

```toml
# ~/.config/crab/config.toml
[auth]
provider = "azure-entra"
issuer_url = "https://login.microsoftonline.com/xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx/v2.0"
client_id = "yyyyyyyy-yyyy-yyyy-yyyy-yyyyyyyyyyyy"

[auth.azure]
tenant_id = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"
storage_account = "mlmodels"
```

If your organization uses a Crab Auth endpoint for fine-grained
authorization, add:

```toml
[auth]
auth_endpoint = "https://crab-auth.corp.example.com/v1/azure"
```

### Step 2: Log in

```bash
crab login
```

```
Authenticated as alice@corp.example.com (azure-entra)
```

### Step 3: Verify

```bash
crab auth status
```

```
Provider:     azure-entra
Identity:     alice@corp.example.com
Token expiry: 2026-04-24T18:30:00Z (52 minutes remaining)
Refresh:      yes
Tenant:       xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
Storage acct: mlmodels
```

### Step 4: Use crab

```bash
git clone crab://mlmodels/team-alpha/gpt4
crab hydrate --all
```

## Configuration Reference

### `[auth.azure]` keys

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `tenant_id` | string | (required) | Entra ID tenant ID |
| `subscription_id` | string | — | Azure subscription ID (optional, for SAS scoping) |
| `storage_account` | string | — | Storage account name (optional, for SAS scoping) |

### Token modes

| Mode | When | Token Type |
|------|------|------------|
| Direct bearer | No `auth_endpoint` configured | `AzureToken::Bearer` (ID token used directly) |
| Crab Auth SAS | `auth_endpoint` returns `storage_account` plus `sas_token` | `AzureToken::Sas` |
| Crab Auth bearer | `auth_endpoint` returns `storage_account` plus `bearer_token` | `AzureToken::Bearer` (scoped) |

When both `sas_token` and `bearer_token` are present in the Crab Auth response,
the SAS token takes precedence.

## Troubleshooting

### "AADSTS700016: Application not found in the directory"

The `client_id` doesn't match any app registration in the tenant. Verify the
Application (client) ID in Azure Portal → Entra ID → App registrations.

### "AADSTS65001: The user or administrator has not consented"

Admin consent hasn't been granted for the Azure Storage API permission. Ask
your Entra ID admin to grant consent in the app registration's API permissions
page.

### "403 Forbidden" on blob operations

The authenticated user doesn't have the Storage Blob Data Contributor (or
Reader) role on the storage account. Verify role assignments:

```bash
az role assignment list \
  --scope /subscriptions/<sub-id>/resourceGroups/<rg>/providers/Microsoft.Storage/storageAccounts/<account> \
  --output table
```

### Auto-refresh on 401

When Azure Blob Storage returns 401 Unauthorized, crab automatically
refreshes the ID token via the refresh token grant and retries once before
propagating the error.

## Related

- [Enterprise Auth Overview](enterprise-auth.md)
- [AWS OIDC](enterprise-auth-aws.md)
- [GCP Workload Identity](enterprise-auth-gcp.md)
- [Crab Auth](enterprise-auth-crab-auth.md)
