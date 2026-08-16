# Managed Crab Platform Technical Design

Managed Crab is an optional hosted layer for Crab authentication, cache,
repository registry, asset navigation, asset APIs, and managed storage. It must
preserve Crab's current self-managed object-store model while adding a
production-grade path for users who want a unified `crab` UX without deploying
their own auth and cache services.

This document is implementation-ready. Every item marked as a GA requirement is
a launch blocker, not a future enhancement.

## Document Metadata

| Field | Value |
|-------|-------|
| Project | Crab |
| Scope | Managed platform, hosted auth/cache, optional managed storage, asset UI/API |
| Status | Technical design |
| Audience | Crab CLI, SDK, platform, security, SRE, product engineering |
| Compatibility | Self-managed Crab remains supported and does not require the platform |

## Table of Contents

1. [Decision Summary](#1-decision-summary)
2. [Goals And Non-Goals](#2-goals-and-non-goals)
3. [Product Modes](#3-product-modes)
4. [GA Requirements](#4-ga-requirements)
5. [Architecture Overview](#5-architecture-overview)
6. [Trust Boundaries](#6-trust-boundaries)
7. [Current Crab Integration Points](#7-current-crab-integration-points)
8. [Control Plane Services](#8-control-plane-services)
9. [Data Plane](#9-data-plane)
10. [Repository Resolution Contract](#10-repository-resolution-contract)
11. [Authentication And Authorization](#11-authentication-and-authorization)
12. [Storage Architecture](#12-storage-architecture)
13. [Cache Architecture](#13-cache-architecture)
14. [Asset Catalog And API](#14-asset-catalog-and-api)
15. [CLI, Config, SDK, And Python Changes](#15-cli-config-sdk-and-python-changes)
16. [Web Console](#16-web-console)
17. [Data Model](#17-data-model)
18. [Public API Contracts](#18-public-api-contracts)
19. [Background Jobs](#19-background-jobs)
20. [Observability](#20-observability)
21. [Security](#21-security)
22. [Compliance And Governance](#22-compliance-and-governance)
23. [Reliability And Disaster Recovery](#23-reliability-and-disaster-recovery)
24. [Performance And Scale](#24-performance-and-scale)
25. [Billing And Metering](#25-billing-and-metering)
26. [Migration, Export, And Lock-In Avoidance](#26-migration-export-and-lock-in-avoidance)
27. [Failure Modes](#27-failure-modes)
28. [Implementation Plan](#28-implementation-plan)
29. [Validation Plan](#29-validation-plan)
30. [Launch Gates](#30-launch-gates)
31. [Deployment And Runtime Operations](#31-deployment-and-runtime-operations)
32. [Appendix: Schemas](#32-appendix-schemas)

## 1. Decision Summary

The implementation boundaries for managed repository identity, V1 physical
isolation, durable jobs, and portable transfer are recorded in the
[managed service architecture decisions](decisions/README.md).

Managed Crab is a hosted platform with three layers:

1. Managed control plane for identity, policy, repo registry, audit, billing,
   storage connection management, and asset indexing.
2. Managed data-plane services for auth brokering, protected push finalize,
   read-only asset APIs, and tenant-isolated cache.
3. Optional Crab-managed storage for users who do not want to bring S3, GCS, or
   Azure.

Self-managed Crab remains a first-class mode:

```text
Self-managed Crab
  crab CLI -> customer object store
  optional self-hosted crab-auth
  optional self-hosted crab-cache-server

Managed Crab with BYO storage
  crab CLI -> Managed Crab auth/cache/registry -> customer object store

Managed Crab with Crab-managed storage
  crab CLI -> Managed Crab auth/cache/registry -> platform-owned object store
```

The platform must not become the only way to use Crab. Existing `crab://bucket`
repositories, static credentials, direct cloud OIDC providers, self-hosted
`crab-auth`, and self-hosted `crab-cache-server` stay supported.

## 2. Goals And Non-Goals

### Goals

- Provide one command path for users who want hosted onboarding:
  `crab cloud login`, `crab repo create`, `crab clone`.
- Host the auth and cache capabilities that enterprise users currently deploy
  themselves.
- Add optional Crab-managed storage for users who do not want to create cloud
  buckets.
- Let users browse, search, diff, and API-read Crab-managed assets without a
  local clone.
- Preserve byte-identical reconstruction, push serialization, GC safety, and
  current object layout contracts.
- Support BYO storage and Crab-managed storage with the same CLI and SDK
  behavior.
- Make export from Crab-managed storage to BYO storage a documented,
  tested, supported path.
- Provide production operations: SLOs, audit logs, rate limits, metering,
  backups, incident response, and tenant isolation.

### Non-Goals

- Do not remove or deprecate self-managed Crab.
- Do not replace customer Git hosting, pull requests, issue tracking, or code
  review in the first GA scope.
- Do not add a second broad write SDK. Repository mutation remains through Git,
  the Crab CLI, remote helper, protected push, or a future explicitly designed
  write contract.
- Do not expose raw object-store credentials in APIs, logs, support bundles, or
  browser responses.
- Do not share dedup visibility across tenant boundaries.
- Do not require customer data to pass through the control plane for normal CLI
  data transfer when direct scoped object-store credentials are available.

## 3. Product Modes

### Mode A: Self-Managed Crab

Existing product behavior. Users configure storage and credentials directly.

```text
crab CLI
  -> S3/GCS/Azure/R2/MinIO
```

Optional customer-operated services:

- `crab-auth` for identity-backed scoped credentials and protected push.
- `crab-cache-server` for read-through cache and dedup acceleration.

Mode A is not dependent on Managed Crab uptime.

### Mode B: Managed Auth And Cache With BYO Storage

Customers keep their object-store bucket. Managed Crab stores only control-plane
metadata and issues short-lived scoped credentials to the CLI or SDK.

```text
crab CLI
  -> Managed Crab API
  -> short-lived storage credentials
  -> customer bucket
  -> managed tenant cache near bucket region
```

This is the first production wedge because it removes customer service
operation without taking full content ownership.

### Mode C: Managed Auth, Cache, And Crab-Managed Storage

Managed Crab owns the object-store account and physical buckets. Users see
logical repo URLs.

```text
crab CLI
  -> Managed Crab API
  -> short-lived credentials for platform storage
  -> platform bucket/prefix
```

This is required for individual developers, small teams, trials, and hosted
asset API users who do not want a cloud account.

### Mode D: Managed Asset API Over Either Storage Mode

Managed Crab exposes read-only repository and asset APIs backed by the Crab SDK
and the same authorization model as the CLI.

```text
browser / app / agent / notebook
  -> Managed Crab asset API
  -> snapshot-pinned SDK reads
  -> customer or managed storage
```

APIs are bounded, auditable, and revision-aware. Full repository writes remain
outside this API in GA.

## 4. GA Requirements

### Functional Requirements

Managed Crab shall:

- Resolve logical repository URLs to physical storage targets.
- Support BYO S3, GCS, Azure Blob, and S3-compatible storage where the current
  Crab CLI can use the backend safely.
- Support Crab-managed storage in at least one production region at GA.
- Issue short-lived scoped storage credentials for clone, fetch, hydrate, push,
  mount, diff, prefetch, fsck, and maintenance operations.
- Support protected push with server-side policy verification before refs move.
- Provide tenant-isolated cache for read-through caching, push warming, and
  dedup queries.
- Provide a repo registry, asset browser, revision browser, file metadata view,
  and read-only asset API.
- Provide organization, team, user, service-account, and token management.
- Provide audit logs for auth decisions, data-plane API access, protected push,
  storage connection changes, policy changes, key changes, and admin actions.
- Provide billing meters for storage bytes, logical bytes, cache bytes, API
  bytes, egress bytes, object operations, protected push finalizations, and
  asset API requests.
- Provide export from managed storage to customer storage.
- Provide a break-glass support model that does not grant Crab operators raw
  content access by default.

### Non-Functional Requirements

Managed Crab shall:

- Keep mutable repository state consistent under concurrent pushes.
- Never cache mutable ref, lock, manifest discovery, or current SlateDB pointer
  paths in the cache service.
- Degrade cache outages to origin reads when origin access is available.
- Use region-homed tenant placement and avoid cross-region data transfer unless
  explicitly configured.
- Enforce request limits for browser/API reads and agent/tool reads.
- Encrypt control-plane data and content at rest.
- Rotate all platform secrets and keys without tenant downtime.
- Support disaster recovery of control-plane metadata without content loss.
- Provide SLO-backed monitoring and pager coverage.

### Security Requirements

Managed Crab shall:

- Use OIDC for human login and workload identity or signed service tokens for
  automation.
- Support enterprise SSO and SCIM before enterprise GA.
- Use least-privilege cloud roles for BYO storage.
- Avoid storing customer cloud access keys whenever role assumption or workload
  federation is available.
- Store any unavoidable customer secrets only through envelope encryption with
  a KMS-backed key hierarchy.
- Scope all issued credentials by tenant, repo, operation, prefix, duration,
  and, where supported, path policy.
- Use per-tenant encryption domains for managed storage.
- Never log raw tokens, credentials, signed URLs, cloud secrets, or file
  payloads.

## 5. Architecture Overview

```text
                 Human, CI, Agent, SDK, Python, Browser
                                  |
                                  v
                          Managed Crab Edge
             API gateway, WAF, rate limit, request authn
                                  |
             +--------------------+--------------------+
             |                    |                    |
             v                    v                    v
       Control Plane         Asset API Plane      Auth/Data Broker
       orgs, repos,          bounded SDK reads    credentials,
       policy, audit,        catalog, search,     protected push,
       billing, config       signed URLs          cache tokens
             |                    |                    |
             +--------------------+--------------------+
                                  |
                                  v
                        Repository Resolver
              logical repo -> physical storage target
                                  |
                 +----------------+----------------+
                 |                                 |
                 v                                 v
          Customer Object Store             Crab-Managed Storage
          S3/GCS/Azure/R2                   platform buckets
                 |                                 |
                 +----------------+----------------+
                                  |
                                  v
                       Tenant-Isolated Cache Pool
```

The control plane stores metadata, not repository content. Repository content
lives in customer or platform object storage. The asset API reads content only
through scoped credentials and SDK paths that preserve Crab reconstruction and
integrity checks.

## 6. Trust Boundaries

### Boundary Map

| Boundary | Crossing | Required Control |
|----------|----------|------------------|
| User device -> Managed Crab edge | HTTPS API calls | TLS, request auth, rate limit, WAF |
| Managed Crab -> customer IdP | OIDC/SAML/SCIM | issuer allowlist, JWKS validation, replay protection |
| Managed Crab -> customer cloud | role assumption or federation | external ID, scoped role, explicit bucket allowlist |
| Managed Crab -> platform storage | internal workload identity | per-tenant IAM scope, KMS key policy |
| Asset API -> repository data | SDK read | policy decision, byte/path limits, audit event |
| Cache service -> origin | object reads/writes | tenant-specific origin credentials and policy |
| Support staff -> tenant metadata | admin tooling | just-in-time access, approval, audit, no default content access |

### Tenant Isolation Model

GA requires hard isolation for:

- Authentication principals.
- Policy decisions.
- Cache data and dedup index.
- Managed storage prefixes and encryption keys.
- Billing meters.
- Audit streams.
- Background job queues.

Recommended default:

- One control-plane database shared across tenants with tenant ID on every row,
  row-level authorization in the application, and automated tenant-leak tests.
- One tenant-region cache namespace with separate SQLite/dedup index and object
  root per tenant. Enterprise plans can request dedicated cache service pools.
- One managed storage bucket per region and trust tier, with tenant prefixes and
  per-tenant KMS keys. High-regulation tiers can use dedicated buckets.

Dedup visibility never crosses tenant boundaries. Cross-repo dedup inside a
tenant is configurable by policy.

## 7. Current Crab Integration Points

Managed Crab builds on existing contracts:

| Area | Existing Surface | Managed Platform Use |
|------|------------------|----------------------|
| Auth config | `crab/src/core/config.rs` `[auth]` provider and endpoint fields | Managed endpoint can use the existing `crab-auth` provider with extended response contracts |
| Credential dispatch | `crab/src/auth/mod.rs` | Add managed repository resolution and storage target remapping |
| Object-store construction | `crab/src/storage/resolver.rs` and auth store adapters | Build stores from resolved physical targets instead of assuming URL host equals bucket |
| Cache config | `crab/src/core/config.rs` `[cache]` service fields | Managed onboarding writes cache service URL and token mode |
| Cache client/server | `crates/crab-cache`, `crates/crab-cache-server` | Host hardened tenant-isolated cache service |
| Protected push | `crab/docs/guides/auth/enterprise-auth-crab-auth.md` | Managed auth provides prepare/finalize and server-side policy |

Required new shared contract:

- `ResolvedRepositoryTarget`: logical repo URL, physical storage target, cache
  endpoint, policy features, and credential metadata.

This is the core gap in the current implementation. Today `crab://bucket/repo`
implicitly means object-store bucket `bucket`. Managed storage needs
`crab://org/repo` to map to a platform-owned bucket and tenant prefix. The
client must stop assuming the URL host is always the physical bucket after a
managed resolver has returned an explicit target.

## 8. Control Plane Services

Managed Crab can be implemented as a small set of services behind one public
API. Service boundaries are logical; deployment can be a modular monolith for
the first production version if the module boundaries and data contracts stay
clean.

### 8.1 Edge API

Responsibilities:

- Terminate TLS.
- Validate API tokens and browser sessions.
- Apply WAF and request size limits.
- Enforce global and tenant rate limits.
- Route to control-plane, asset, auth, and admin handlers.
- Emit request audit metadata without sensitive headers.

Implementation notes:

- Use one public hostname, for example `api.crab.build`.
- Require all mutating APIs to carry idempotency keys.
- Include request IDs in every response.
- Use structured JSON error bodies with stable error codes.

### 8.2 Identity Service

Responsibilities:

- Human login through Crab-hosted OIDC for personal users.
- Enterprise SSO through OIDC/SAML.
- SCIM user and group provisioning for enterprise tenants.
- Service accounts and workload tokens.
- Session management and token revocation.

Data never stored:

- IdP raw refresh tokens unless required for a provider integration and
  explicitly encrypted.
- User passwords for enterprise SSO tenants.

### 8.3 Organization And Tenant Service

Responsibilities:

- Tenant creation.
- Organization, project, team, user, and service account lifecycle.
- Region home and data residency settings.
- Plan, quota, and feature flag assignment.
- Tenant deletion workflow.

Tenant deletion is a multi-step workflow:

1. Disable new writes.
2. Revoke active tokens.
3. Stop background indexing.
4. Export or tombstone metadata per retention policy.
5. Delete managed cache data.
6. Delete managed storage after retention and legal hold checks.
7. Retain minimal billing/audit records required by law.

### 8.4 Repository Registry

Responsibilities:

- Create and map logical repo URLs.
- Store physical storage target configuration.
- Track current storage mode: BYO or managed.
- Track repo capabilities, default branch, cache policy, and indexing policy.
- Resolve logical URLs for CLI, SDK, asset API, and background workers.

The registry is authoritative for managed logical URLs. Object storage remains
authoritative for repository content.

### 8.5 Policy Service

Responsibilities:

- Evaluate org, repo, branch, path, operation, token, and workload policies.
- Return explicit allow/deny decisions with reason codes.
- Support inherited policies from org -> project -> repo.
- Support default-deny enterprise posture.
- Version policies and expose policy audit history.

Policy inputs:

- Principal: user, group, service account, agent, support actor.
- Resource: org, repo, branch, path prefix, asset, storage connection.
- Operation: clone, fetch, hydrate, push, protected-push-finalize, mount, diff,
  fsck, gc, cache-read, cache-write, dedup, asset-read, signed-url-create,
  repo-admin.
- Context: source IP class, session MFA state, token type, CI workload,
  request byte limit, time, region.

Policy outputs:

```json
{
  "decision": "allow",
  "decision_id": "pd_012345",
  "principal_id": "usr_012345",
  "resource_id": "repo_012345",
  "operation": "fetch",
  "effective_scopes": {
    "refs": ["refs/heads/main"],
    "paths": ["datasets/public/**"],
    "max_read_bytes": 1073741824
  },
  "audit_level": "standard"
}
```

### 8.6 Credential Broker

Responsibilities:

- Exchange user or service-account identity for scoped storage credentials.
- Integrate with customer cloud role assumption for BYO storage.
- Integrate with platform workload identity for managed storage.
- Return `ResolvedRepositoryTarget` plus credentials.
- Refresh credentials before expiry.
- Reject credential requests that do not name a concrete operation.

Credential TTL defaults:

| Use | TTL |
|-----|-----|
| Read and hydrate | 15 minutes |
| Clone/fetch | 30 minutes |
| Protected push staging | 10 minutes |
| Asset API internal read | 5 minutes |
| Cache service origin access | 15 minutes |

Long operations refresh through the existing refresh-capable object-store path.

### 8.7 Protected Push Service

Responsibilities:

- Provide `push/prepare` and `push/finalize`.
- Issue staging-only credentials for immutable upload.
- Verify staged bundles and changed paths.
- Enforce branch, fast-forward, path, and quota policy.
- Commit manifests and refs with service-owned credentials.
- Emit durable audit and billing events.

The CLI must never receive credentials that can directly mutate canonical refs
or manifests for protected repositories.

### 8.8 Storage Connection Service

Responsibilities:

- Register BYO cloud storage connections.
- Verify access with non-destructive probes.
- Store cloud role metadata and external IDs.
- Detect drift in bucket policy, KMS policy, object lock, lifecycle, and region.
- Provide onboarding diagnostics.

BYO storage connection uses role assumption by default:

- AWS: cross-account role with external ID.
- GCP: workload identity federation or service account impersonation.
- Azure: federated credential or managed app role assignment.
- S3-compatible: scoped static credentials only when no federation exists.

Static credentials are allowed only when encrypted and explicitly marked as
rotatable customer secrets.

### 8.9 Storage Provisioning Service

Responsibilities:

- Allocate managed storage physical buckets/prefixes.
- Create per-tenant KMS keys or key aliases.
- Apply lifecycle and versioning policy.
- Apply object lock or legal hold policy when enabled.
- Emit storage inventory snapshots.

Managed storage never exposes platform-wide bucket credentials to users.

### 8.10 Cache Coordinator

Responsibilities:

- Allocate tenant-region cache pools.
- Issue cache access tokens.
- Manage cache service health and capacity.
- Route clients to the nearest allowed cache endpoint.
- Enforce dedup visibility scope.
- Collect low-cardinality cache metrics and billing meters.

### 8.11 Asset Indexer

Responsibilities:

- Index refs, commits, trees, pointer files, logical sizes, content hashes,
  pointer kind, file type hints, and large-file metadata.
- Maintain snapshot-pinned inventory for browsing and search.
- Consume repo update events from protected push finalize and periodic scans.
- Avoid full hydration by default.

The index is a derived cache. Object storage and Git history remain
authoritative. Missing or stale index entries must not affect CLI correctness.

### 8.12 Asset API Service

Responsibilities:

- Serve bounded read-only repo and asset APIs.
- Use SDK snapshots for revision-pinned reads.
- Enforce byte, path, glob, and output limits.
- Provide signed URLs when allowed.
- Provide diff, metadata, and range-read endpoints.
- Emit audit and billing events.

### 8.13 Audit Service

Responsibilities:

- Ingest append-only audit events from every service.
- Store tamper-evident event batches.
- Provide tenant export and filtering.
- Redact secrets.
- Support retention policy and legal hold.

### 8.14 Metering And Billing Service

Responsibilities:

- Ingest usage events.
- Aggregate usage per tenant, repo, region, and storage mode.
- Reconcile with cloud provider inventory.
- Enforce quotas.
- Emit billing exports.

### 8.15 Admin And Support Service

Responsibilities:

- Provide internal support tooling.
- Require just-in-time access grants.
- Emit support audit events.
- Provide read-only diagnostics by default.
- Require explicit tenant approval for content access when enterprise policy
  requires it.

## 9. Data Plane

### 9.1 BYO Storage Clone/Fetch

```text
crab clone crab://acme/models
  -> CLI asks Managed Crab to resolve repo for operation clone
  -> policy allow
  -> broker assumes customer cloud role
  -> response includes physical bucket, repo prefix, cache endpoint, credentials
  -> CLI builds object store from physical target
  -> fetch reads refs/packs/metadata from customer bucket
  -> immutable reads use tenant cache when healthy
```

Control plane receives only metadata requests and audit events. Repository bytes
flow directly between the CLI/cache service and object storage.

### 9.2 Managed Storage Clone/Fetch

```text
crab clone crab://acme/models
  -> CLI resolves repo
  -> policy allow
  -> broker issues scoped platform storage credentials
  -> response maps logical repo to platform bucket/prefix
  -> CLI reads through cache and platform object storage
```

The physical bucket name is not part of the public URL contract.

### 9.3 Protected Push

```text
crab push
  -> prepare(logical repo, ref updates, advisory paths)
  -> policy preflight
  -> staging-only credentials returned
  -> CLI uploads immutable staged data under upload_prefix
  -> finalize(push_id, ref updates)
  -> service verifies staged bundle, changed paths, fast-forward, quotas
  -> service commits manifest and refs using canonical credentials
  -> service emits repo-updated event
  -> cache warming starts after origin durability
```

The publish boundary is finalize. Push warming failures do not roll back a
successful push.

### 9.4 Asset API Read

```text
GET /v1/repos/{repo_id}/snapshots/main/files/data/train.parquet/ranges
  -> API auth
  -> policy allow with byte limit
  -> resolve main to immutable commit
  -> SDK opens snapshot
  -> SDK reads only requested ranges
  -> response includes resolved commit, content hash, range, truncation state
  -> audit and meter events emitted
```

Asset API reads are bounded and revision-pinned. Symbolic refs are resolved at
request start and the resolved commit is returned to the caller.

### 9.5 Asset Catalog Update

```text
protected push finalize or scheduled scan
  -> repo-updated event
  -> indexer resolves refs
  -> walks changed tree(s)
  -> records paths, sizes, hashes, pointer metadata
  -> emits index complete or degraded event
```

Indexing must not block pushes. Index lag is visible in the UI/API.

## 10. Repository Resolution Contract

### 10.1 Problem

Current `crab://bucket/repo` URLs imply that `bucket` is the physical object
store bucket. Managed storage needs logical URLs such as `crab://acme/models`
where `acme` is an organization, not a cloud bucket.

### 10.2 Required Contract

Managed clients must resolve logical repositories before constructing an
object store.

```json
{
  "schema": "crab.repository-target",
  "version": "1.0",
  "logical_url": "crab://acme/models",
  "repo_id": "repo_012345",
  "tenant_id": "ten_012345",
  "storage": {
    "mode": "managed",
    "provider": "s3",
    "bucket": "crab-prod-usw2-managed-a",
    "repo_prefix": "tenants/ten_012345/repos/repo_012345",
    "global_prefix": "tenants/ten_012345/global",
    "region": "us-west-2",
    "endpoint_url": null
  },
  "credentials": {
    "provider": "aws",
    "expires_at": "2026-07-01T12:15:00Z",
    "access_key_id": "<redacted>",
    "secret_access_key": "<redacted>",
    "session_token": "<redacted>"
  },
  "cache": {
    "enabled": true,
    "service_url": "https://cache.us-west-2.crab.build/tenants/ten_012345",
    "service_auth": "bearer",
    "token": "<redacted>",
    "mode": "cache+dedup",
    "dedup_scope": "tenant"
  },
  "policy": {
    "operation": "fetch",
    "decision_id": "pd_012345",
    "path_prefixes": ["*"],
    "max_read_bytes": 1099511627776
  },
  "features": {
    "protected_push": true,
    "asset_api": true,
    "lineage": true
  }
}
```

### 10.3 Client Behavior

Clients must:

- Use `storage.bucket` and `storage.repo_prefix`, not URL host/path, after a
  managed target response.
- Use `storage.global_prefix` for global `.crab` objects when returned.
- Attach cache service configuration only for this resolved target unless the
  user persisted it.
- Treat missing `storage` as a protocol error.
- Treat expired credentials as refreshable when the provider supports refresh.
- Redact credentials in logs, error messages, support bundles, and JSON output.

### 10.4 Backward Compatibility

Legacy self-managed repositories do not call the resolver unless configured
with managed cloud settings. Existing `crab://bucket/repo` behavior stays
unchanged for static credentials, cloud OIDC providers, and self-hosted
`crab-auth` endpoints that do not return `storage`.

## 11. Authentication And Authorization

### 11.1 Principal Types

| Principal | Use |
|-----------|-----|
| User | Interactive CLI, SDK, web console |
| Service account | CI, automation, ETL |
| Agent | Bounded AI/agent tool access |
| Support actor | Internal just-in-time support |
| Cache service | Origin cache fill and push warming |
| Indexer worker | Background asset indexing |

### 11.2 Token Types

| Token | Lifetime | Storage | Notes |
|-------|----------|---------|-------|
| Browser session | hours | secure cookie | Web console only |
| CLI refresh token | days to weeks | encrypted local token cache | Revocable per device |
| CLI access token | minutes | memory/encrypted cache | Used against Managed Crab API |
| Service account token | configurable | CI secret store | Can be exchanged for short-lived access |
| Cache bearer token | minutes | memory/configured client | Scoped to cache routes |
| Signed URL | minutes | caller receives URL | Read-only, path and range scoped |

### 11.3 Authorization Operations

Managed Crab must use concrete operations, not wildcard operation requests.

Required operations:

```text
repo.read
repo.clone
repo.fetch
repo.hydrate
repo.prefetch
repo.diff
repo.mount
repo.fsck
repo.gc
repo.push.prepare
repo.push.finalize
repo.lock
repo.repack
repo.compact
repo.restripe
asset.list
asset.stat
asset.read
asset.range_read
asset.signed_url.create
asset.diff
cache.read
cache.write
cache.dedup
repo.admin
storage.connection.admin
policy.admin
audit.read
billing.read
```

### 11.4 Policy Evaluation Rules

- Default deny.
- Deny rules override allow rules.
- Path policy is evaluated on server-verified changed paths for protected push.
- Advisory client paths can narrow preflight, but cannot authorize finalize.
- Branch policy is evaluated against full ref names.
- Ref-only pushes require an explicit non-path-scoped permission.
- Service account policy must be separate from human user policy.
- Agent policy must have explicit read byte and path limits.

### 11.5 Audit Decisions

Every allow or deny for a credential, protected push, asset API read, signed
URL, cache token, or admin mutation emits an audit event with:

- `event_id`
- `tenant_id`
- `principal_id`
- `principal_type`
- `operation`
- `resource_type`
- `resource_id`
- `logical_url`
- `resolved_revision` when applicable
- `path_or_pattern` when applicable
- `decision`
- `decision_id`
- `reason_code`
- `request_id`
- `source_ip_class`
- `created_at`

No event contains raw credentials, tokens, signed URLs, or file payload bytes.

## 12. Storage Architecture

### 12.1 BYO Storage

BYO storage stores Crab content in customer-owned object storage. Managed Crab
stores connection metadata and uses scoped credentials.

Connection states:

| State | Meaning |
|-------|---------|
| `pending` | Created but not verified |
| `verified` | Probe succeeded and policies are acceptable |
| `degraded` | Previously working, current probes failing |
| `disabled` | Admin disabled new credentials |
| `revoked` | Connection removed or no longer trusted |

Required probes:

- Bucket/container exists.
- Region matches declared region.
- Read, write, list, head, delete for disposable probe prefix.
- Conditional write support for CAS.
- Multipart upload support for large objects where backend requires it.
- KMS key access where configured.
- Lifecycle and object lock compatibility posture.
- No public bucket policy.

### 12.2 Managed Storage

Managed storage uses platform-owned object storage. Physical layout:

```text
s3://crab-prod-{region}-{tier}/
├── tenants/{tenant_id}/
│   ├── repos/{repo_id}/
│   │   ├── HEAD
│   │   ├── refs/
│   │   ├── packs/
│   │   ├── manifests/
│   │   ├── file_index_db/
│   │   ├── locks/
│   │   └── lfs/
│   └── global/
│       ├── xorbs/
│       ├── shards/
│       └── chunk_index_db/
```

`global/` is tenant-scoped, not platform-global. Cross-tenant dedup is not a GA
feature.

Managed storage bucket settings:

- Versioning enabled for mutable metadata recovery.
- Server-side encryption with per-tenant KMS key or key context.
- Block public access.
- Access logs or cloud audit logs enabled.
- Lifecycle policy for incomplete multipart uploads.
- Optional object lock for enterprise retention.
- Replication only when configured by tenant region policy.

### 12.3 Storage Target Identity

Every resolved target includes:

- logical URL
- tenant ID
- repo ID
- provider
- physical bucket/container
- repo prefix
- tenant global prefix
- region
- endpoint URL for S3-compatible stores
- credentials expiry
- storage mode

The tuple `(provider, bucket, repo_prefix, global_prefix, region)` is the
runtime storage identity. It must be included in diagnostic support bundles
with bucket and prefix redacted by default.

### 12.4 Storage Object Classes

Managed storage must classify paths with the same mutable/immutable taxonomy as
the cache service:

| Path Family | Mutable | Cacheable | Notes |
|-------------|---------|-----------|-------|
| refs, HEAD | yes | no | Direct origin only |
| locks | yes | no | Direct origin only |
| manifests current pointers | yes | no | Direct origin only |
| packs, pack indexes | no | yes | Immutable after publish |
| xorbs | no | yes | Tenant-global content data |
| shards | no | yes | Tenant-global metadata |
| versioned SlateDB SSTs/manifests | no | yes | Discovery pointers stay mutable |
| LFS objects | no | optional | Cache only after LFS path contract is explicit |

### 12.5 Garbage Collection

Managed storage GC must reuse Crab's reachability rules:

- Never delete referenced xorbs, shards, packs, or LFS objects.
- Never delete objects inside the configured grace period.
- Respect legal hold and retention locks.
- Produce a plan before deletion.
- Emit audit and billing adjustments.
- Support dry run and sampled verification.

For managed storage, bucket-wide GC is forbidden. GC scope is tenant repo or
tenant global prefix only.

## 13. Cache Architecture

### 13.1 Cache Product Contract

Cache is an acceleration layer. Origin storage remains authoritative.

Managed cache must support:

- Read-through cache for immutable objects.
- Range reads.
- Push warming after origin durability.
- Dedup query for known cached chunks.
- Admin stats for operators.
- Tenant-level isolation.
- Policy-aware access tokens.

### 13.2 Tenant Isolation

GA default:

```text
cache tenant root
├── cache.sqlite
├── xorbs/
├── shards/
├── packs/
└── metadata/
```

No shared `cache.sqlite` across tenants. No shared dedup index across tenants.
Within a tenant, dedup scope is one of:

| Scope | Meaning |
|-------|---------|
| `repo` | Query only the current repo |
| `project` | Query repos in same project |
| `tenant` | Query all tenant repos |

Enterprise policy can force `repo`.

### 13.3 Cache Token Contract

Managed repository resolution returns a cache token scoped to:

- tenant
- repo
- operation set: read, write, dedup, admin
- dedup scope
- expiry
- cache service URL

The cache service validates signed tokens, not raw bearer strings. Self-hosted
bearer mode can remain for private deployments, but managed public cache must
use signed and validated tokens.

### 13.4 Cache Availability

If cache is unavailable:

- CLI reads fall back to origin.
- SDK reads fall back to origin.
- Push warming logs a warning and does not fail push.
- Dedup query falls back to origin chunk index.
- Asset API can fall back to origin if the request budget allows it.

Exceptions:

- Tenants configured for cache-only private network mode fail closed when cache
  is unavailable.
- Offline mode fails with offline cache miss.

## 14. Asset Catalog And API

### 14.1 Catalog Scope

Catalog stores derived metadata:

- refs
- commits
- trees
- paths
- entry kind
- pointer kind
- logical size
- content hash or file hash
- chunk count
- xorb count
- dedup ratio
- MIME/type hints
- latest indexed revision
- indexing status

Catalog does not store full file content.

### 14.2 Indexing Strategy

Index incrementally after protected push finalize:

1. Receive repo-updated event.
2. Resolve changed refs.
3. Compare new and previous indexed commits.
4. Walk changed trees.
5. Stat pointer files without hydration.
6. Store path inventory rows.
7. Emit index completion.

If index cannot keep up, it marks the repo as `index_degraded` and the UI/API
can fall back to on-demand SDK reads with stricter limits.

### 14.3 Asset API Principles

- Snapshot-pinned by default.
- Symbolic refs resolve once per request.
- Raw reads are byte-limited.
- Directory walks are paginated.
- Glob results are paginated and limit-aware.
- Large-file diff returns metadata first, not full payloads.
- Signed URLs are short-lived and exact-path scoped.
- All responses include resolved revision where relevant.

### 14.4 API Read Limits

Default limits:

| Operation | Default Limit |
|-----------|---------------|
| Text preview | 1 MiB |
| Binary range read | 64 MiB |
| Directory page | 1,000 entries |
| Glob result page | 1,000 entries |
| Diff changed files | 10,000 entries |
| Signed URL TTL | 15 minutes |

Tenant and policy can lower or raise these limits.

## 15. CLI, Config, SDK, And Python Changes

### 15.1 CLI Commands

Add `crab cloud` and `crab repo` command namespaces:

```text
crab cloud login [--org <org>] [--headless]
crab cloud logout
crab cloud status [--json]
crab cloud org list
crab cloud org use <org>
crab cloud storage connect <provider> ...
crab cloud storage check <connection>
crab cloud cache status [--json]

crab repo create <org>/<repo> [--storage managed|<connection>] [--region <region>]
crab repo list [--org <org>] [--json]
crab repo info <org>/<repo> [--json]
crab repo export <org>/<repo> --to <crab-url>
```

Existing commands continue to work:

```text
crab init crab://acme/models
crab clone crab://acme/models
crab push
crab hydrate
```

The cloud commands configure the existing auth provider plus managed resolver
settings. They do not create a separate incompatible CLI path.

### 15.2 Config

Add a `[cloud]` section:

```toml
[cloud]
endpoint = "https://api.crab.build"
org = "acme"
profile = "default"

[auth]
provider = "crab-auth"
issuer_url = "https://auth.crab.build"
client_id = "crab-cli"
auth_endpoint = "https://api.crab.build/v1/credentials"

[remote]
url = "crab://acme/models"
managed = true
```

`remote.managed = true` means the URL is logical and must be resolved before
object-store construction. Without it, existing direct `crab://bucket/repo`
behavior applies.

### 15.3 SDK

SDK must support:

- Opening managed logical URLs.
- Resolving repository targets through Managed Crab.
- Snapshot-pinned reads from resolved physical targets.
- Managed auth refresh.
- Managed cache token injection.
- Asset API compatible metadata and errors.

SDK public API shape:

```rust
let repo = CrabRepository::builder()
    .cloud_endpoint("https://api.crab.build")
    .open("crab://acme/models")
    .await?;
let snap = repo.snapshot(Some("main")).await?;
let reader = snap.open("data/train.parquet").await?;
```

### 15.4 Python

Python package must support:

- Managed logical URL open.
- Browser or device-code login.
- Service account token configuration.
- File-like reads over managed storage.
- fsspec-style paths for revision-pinned reads.

Example:

```python
import crab

repo = crab.Repository.open("crab://acme/models")
snap = repo.snapshot("main")
with snap.open("data/train.parquet") as f:
    head = f.read(4096)
```

### 15.5 Git Credential Helper Interop

Managed Crab should not require users to manually configure Git credential
helpers. The CLI token cache is the default. Git credential helper interop
remains for deployments that front a Crab object endpoint with HTTP auth.

## 16. Web Console

GA web console pages:

- Login and organization switcher.
- Repo list.
- Repo detail with refs, commits, storage mode, region, cache status.
- File browser at a selected revision.
- File metadata panel.
- Text preview.
- Binary range/download action when authorized.
- Large-file diff summary.
- Storage connection setup and diagnostics.
- Policy editor or policy viewer depending on plan.
- Audit log viewer.
- Billing usage viewer.

The web console must not include visible raw credentials or signed URLs. Signed
download actions happen through short-lived browser redirects or API responses
with exact-path scope.

## 17. Data Model

Use a transactional relational database for control-plane metadata. Object
storage remains the repository content source of truth.

### 17.1 Core Tables

```text
tenants
  id, slug, name, plan, home_region, status, created_at, deleted_at

organizations
  id, tenant_id, slug, display_name, created_at

users
  id, primary_email, display_name, status, created_at

tenant_memberships
  tenant_id, user_id, role, status, created_at

groups
  id, tenant_id, external_id, name, source, created_at

group_memberships
  group_id, user_id, created_at

service_accounts
  id, tenant_id, name, status, created_at, last_used_at

projects
  id, tenant_id, slug, name, created_at

repositories
  id, tenant_id, project_id, slug, logical_url, storage_mode,
  default_branch, region, status, created_at, archived_at
```

### 17.2 Storage Tables

```text
storage_connections
  id, tenant_id, provider, mode, region, endpoint_url, bucket_or_container,
  role_arn_or_identity, encrypted_secret_ref, status, last_checked_at

repository_storage_targets
  repo_id, storage_connection_id, provider, bucket_or_container, repo_prefix,
  global_prefix, region, endpoint_url, created_at

managed_storage_allocations
  id, tenant_id, repo_id, region, bucket, repo_prefix, global_prefix,
  kms_key_ref, storage_class_policy, status, created_at
```

### 17.3 Policy Tables

```text
policies
  id, tenant_id, scope_type, scope_id, version, status, created_at

policy_rules
  id, policy_id, effect, principal_selector, resource_selector,
  operations, path_patterns, ref_patterns, conditions, priority

policy_decisions
  id, tenant_id, principal_id, resource_id, operation, decision,
  reason_code, created_at
```

`policy_decisions` can be sampled or retained by policy. Complete audit events
are stored in the audit pipeline.

### 17.4 Asset Index Tables

```text
repo_refs
  repo_id, ref_name, target_oid, kind, indexed_at

repo_commits
  repo_id, commit_oid, parents, author, authored_at, message_summary,
  indexed_at

asset_entries
  repo_id, commit_oid, path, entry_kind, pointer_kind, logical_size,
  content_hash, file_hash, chunk_count, xorb_count, dedup_ratio,
  type_hint, indexed_at

asset_index_runs
  id, repo_id, ref_name, commit_oid, status, started_at, completed_at,
  error_code, scanned_entries
```

### 17.5 Audit And Metering Tables

```text
audit_events
  event_id, tenant_id, actor_id, actor_type, operation, resource_type,
  resource_id, decision, reason_code, request_id, source_ip_hash,
  created_at, payload_json

usage_events
  event_id, tenant_id, repo_id, meter, quantity, unit, region,
  storage_mode, created_at, dimensions_json

usage_rollups_hourly
  tenant_id, repo_id, meter, hour, quantity, unit, region, dimensions_hash
```

## 18. Public API Contracts

### 18.1 API Conventions

- Base path: `/v1`.
- All JSON requests include `Content-Type: application/json`.
- All responses include `X-Request-Id`.
- Mutating requests require `Idempotency-Key`.
- Errors use stable codes.

Error body:

```json
{
  "error": {
    "code": "repo.not_found",
    "message": "Repository not found",
    "request_id": "req_012345",
    "retryable": false
  }
}
```

### 18.2 Repository Resolve

```text
POST /v1/repositories/resolve
```

Request:

```json
{
  "logical_url": "crab://acme/models",
  "operation": "fetch",
  "client_version": "0.1.0"
}
```

Response: `crab.repository-target` as defined above.

### 18.3 Credential Endpoint

Existing managed `crab-auth` endpoint:

```text
POST /v1/credentials
```

For managed logical repos, the response includes `storage`. For legacy
self-hosted compatible mode, response may omit `storage` and clients use URL
bucket semantics.

### 18.4 Protected Push

```text
POST /v1/push/prepare
POST /v1/push/finalize
```

Required behavior:

- `prepare` returns staging-only storage credentials.
- `finalize` verifies bundle and changed paths server-side.
- `finalize` rejects stale refs with `409`.
- `finalize` rejects policy violations with `403`.
- Both endpoints are idempotent by `push_id` and idempotency key.

### 18.5 Repo Management

```text
GET  /v1/orgs/{org}/repos
POST /v1/orgs/{org}/repos
GET  /v1/repos/{repo_id}
PATCH /v1/repos/{repo_id}
POST /v1/repos/{repo_id}/archive
POST /v1/repos/{repo_id}/export
```

### 18.6 Asset API

```text
GET  /v1/repos/{repo_id}/refs
GET  /v1/repos/{repo_id}/commits/{rev}
GET  /v1/repos/{repo_id}/snapshots/{rev}/tree?path=&page_token=
GET  /v1/repos/{repo_id}/snapshots/{rev}/stat?path=
GET  /v1/repos/{repo_id}/snapshots/{rev}/read-text?path=&limit=
POST /v1/repos/{repo_id}/snapshots/{rev}/read-range
POST /v1/repos/{repo_id}/diff
POST /v1/repos/{repo_id}/signed-url
```

`read-range` request:

```json
{
  "path": "data/train.parquet",
  "offset": 0,
  "length": 65536
}
```

Response:

```json
{
  "repo_id": "repo_012345",
  "requested_rev": "main",
  "resolved_commit": "0123456789abcdef0123456789abcdef01234567",
  "path": "data/train.parquet",
  "offset": 0,
  "length": 65536,
  "content_hash": "blake3:...",
  "encoding": "base64",
  "data": "<base64>",
  "truncated": false
}
```

### 18.7 Storage Connections

```text
POST /v1/storage-connections
POST /v1/storage-connections/{id}/check
GET  /v1/storage-connections/{id}
PATCH /v1/storage-connections/{id}
DELETE /v1/storage-connections/{id}
```

### 18.8 Audit And Usage

```text
GET /v1/audit-events
GET /v1/usage
GET /v1/usage/export
```

Audit filters must support time range, actor, repo, operation, decision, and
request ID.

## 19. Background Jobs

Required job families:

| Job | Trigger | Idempotency Key |
|-----|---------|-----------------|
| Storage connection probe | create/update/schedule | connection ID + generation |
| Repo index | protected push / schedule | repo ID + commit |
| Cache warm | protected push | repo ID + object hash |
| Usage rollup | usage event ingest | tenant + meter + hour |
| Audit batch seal | time/size threshold | batch ID |
| Managed storage inventory | schedule | tenant + region + date |
| GC plan | schedule/manual | repo + generation |
| Export | user request | export ID |
| Tenant deletion | admin request | tenant deletion ID |
| Key rotation | schedule | key ID + rotation generation |

Job requirements:

- At-least-once execution.
- Idempotent writes.
- Dead-letter queue.
- Retry with exponential backoff and bounded attempts.
- Tenant-aware concurrency limits.
- Secret-safe logs.

## 20. Observability

### 20.1 Metrics

Required platform metrics:

- API request count, latency, errors by route class.
- Credential issuance count, latency, errors by provider and operation.
- Protected push prepare/finalize count, latency, result.
- Storage probe result and latency.
- Cache hit/miss/range hit/dedup known counts.
- Cache origin bytes avoided.
- Asset API read bytes and latency.
- Index lag by repo.
- Audit ingest lag.
- Usage event ingest lag.
- Background job queue depth and age.
- KMS encrypt/decrypt errors.
- Cloud role assumption errors.

Avoid high-cardinality labels such as raw path, token ID, full repo URL, or
user email.

### 20.2 Logs

Structured logs include:

- request ID
- tenant ID
- service name
- route class
- operation
- resource ID
- outcome
- error code

Structured logs exclude:

- raw credentials
- bearer tokens
- signed URLs
- raw file paths when tenant policy marks paths sensitive
- payload bytes

### 20.3 Traces

Distributed traces cover:

- edge request
- policy decision
- credential broker call
- cloud provider role assumption
- object-store probe
- SDK snapshot read
- cache fill
- protected push finalize

Trace attributes follow the same redaction rules as logs.

### 20.4 Support Bundles

Support bundles include:

- redacted repository target
- control-plane request IDs
- auth provider posture
- cache health
- storage connection probe summary
- policy decision IDs
- index status
- relevant audit event IDs

Support bundles never include secrets or payload bytes.

## 21. Security

### 21.1 Threat Model

Primary threats:

- Tenant A accesses Tenant B data.
- User escalates repo/path permissions.
- Stolen CLI token mints long-lived storage credentials.
- Cache dedup query leaks existence of another tenant's data.
- Support tooling exposes customer content.
- Compromised background worker overreads storage.
- Signed URL escapes path or TTL limits.
- BYO storage role is overbroad.
- Logs leak secrets.
- Protected push finalizes unverified or denied content.

### 21.2 Controls

| Threat | Control |
|--------|---------|
| Cross-tenant access | tenant ID on every auth decision, storage target, cache namespace, and DB row |
| Permission escalation | central policy service, deny precedence, server-side protected push verification |
| Token theft | short access-token TTL, refresh token revocation, device/session inventory |
| Dedup leak | no cross-tenant dedup, explicit tenant/repo dedup scope |
| Support overreach | JIT access, approval workflow, audit, content access disabled by default |
| Worker overread | worker identity scoped to exact tenant/repo job |
| Signed URL leak | short TTL, exact path, optional range, no list permission |
| Overbroad BYO role | onboarding probe, least-privilege policy template, drift detection |
| Log secret leak | central redaction, structured logging tests, support bundle verifier |
| Unsafe finalize | staging-only credentials, bundle verification, fast-forward and path policy checks |

### 21.3 Encryption

Control-plane database:

- encrypted at rest
- encrypted backups
- app-layer encryption for customer cloud secrets

Managed storage:

- server-side encryption
- per-tenant key or encryption context
- key rotation
- key disable workflow tested before tenant deletion

In transit:

- TLS 1.2 minimum, TLS 1.3 preferred
- mTLS for internal service-to-service traffic
- private network paths for enterprise dedicated deployments where available

### 21.4 Secret Handling

Secret classes:

- customer cloud secret
- refresh token
- service account token hash
- cache signing key
- KMS data key
- signed URL

Rules:

- Store hashes for user-created tokens, not token values.
- Store customer secrets through envelope encryption.
- Rotate cache signing keys with overlap.
- Rotate internal service credentials automatically.
- Redact by type, not by string matching alone.
- Include secret scanners in CI.

## 22. Compliance And Governance

Enterprise GA requires:

- SSO.
- SCIM.
- Audit log export.
- Data retention controls.
- Tenant deletion workflow.
- Region selection for managed storage.
- DPA-ready data inventory.
- Subprocessor inventory.
- Incident notification procedure.
- Access review reports.

Compliance posture does not need to claim a certification at first GA, but the
technical controls must support SOC 2 readiness.

## 23. Reliability And Disaster Recovery

### 23.1 SLO Targets

Initial production targets:

| Surface | SLO |
|---------|-----|
| Control-plane API availability | 99.9% monthly |
| Credential issuance availability | 99.9% monthly |
| Asset API availability | 99.9% monthly |
| Cache service availability | 99.5% monthly |
| Protected push finalize success excluding user conflicts | 99.9% |
| Audit ingest delay | p95 under 5 minutes |
| Asset index delay | p95 under 15 minutes for normal repos |

Cache SLO is lower because cache is an acceleration layer. Tenants that require
cache as mandatory infrastructure need dedicated SLO terms.

### 23.2 Backup And Restore

Control-plane DB:

- point-in-time recovery
- daily full backups
- restore drills at least monthly before GA
- cross-region backup copy for production

Managed storage:

- object versioning
- lifecycle for incomplete multipart uploads
- optional replication
- inventory reports

Audit:

- append-only batches
- tamper-evident batch hashes
- retention by tenant policy

### 23.3 Disaster Recovery

DR levels:

| Incident | Recovery |
|----------|----------|
| API stateless service failure | restart or scale replacement |
| Region-local cache loss | rebuild from origin, degraded performance |
| Control-plane DB primary loss | promote replica or restore PITR |
| Managed storage accidental metadata delete | recover from object versioning |
| Tenant KMS key disable | fail closed until key restored |
| BYO role revoked | credential issuance fails, existing CLI tokens fail on refresh |

Self-managed Crab is unaffected by Managed Crab incidents unless the repo is
configured to use Managed Crab auth/cache.

## 24. Performance And Scale

### 24.1 Scale Targets

GA targets per tenant:

- 10,000 repositories.
- 100 million indexed asset entries.
- 10,000 active CLI devices.
- 1,000 service accounts.
- 1,000 protected push finalizations per hour.
- 10 GiB/s aggregate cache egress for enterprise dedicated tenants.

Platform-wide targets are a capacity planning exercise, not a protocol limit.

### 24.2 Hot Paths

Hot paths:

- credential issuance
- cache immutable object reads
- cache range reads
- protected push finalize
- asset tree pages
- asset range reads

Required optimizations:

- Cache repository target resolution for short TTLs.
- Cache policy decision inputs, not deny/allow forever.
- Keep cache route low-latency and independent from catalog indexing.
- Use SDK range reads for asset API.
- Paginate every list/walk API.
- Avoid indexing full payload bytes.

### 24.3 Backpressure

Backpressure controls:

- per-tenant API rate limit
- per-token API rate limit
- per-repo protected push concurrency
- per-tenant asset API byte budget
- per-tenant background job concurrency
- cache fill concurrency
- storage credential issuance rate

When limits are hit, return `429` with `Retry-After` for retryable paths and a
stable error code for non-retryable quota denials.

## 25. Billing And Metering

### 25.1 Meters

Required meters:

| Meter | Unit |
|-------|------|
| Managed storage logical bytes | byte-hour |
| Managed storage physical bytes | byte-hour |
| Managed storage operations | count |
| Managed egress | bytes |
| Cache storage | byte-hour |
| Cache egress | bytes |
| Cache origin bytes avoided | bytes |
| Asset API read | bytes |
| Asset API requests | count |
| Protected push finalize | count |
| Index entries | entry-hour |
| Service account seats or tokens | count |

BYO storage tenants are not billed for origin storage bytes by Crab, but Crab
can show estimates based on inventory and cloud pricing tables where available.

### 25.2 Quotas

Quota types:

- managed storage bytes
- monthly egress bytes
- asset API bytes per day
- repositories per tenant
- service accounts
- protected push finalizations per hour
- cache capacity
- concurrent background jobs

Quota enforcement must fail before irreversible work when feasible. Protected
push quota is checked in prepare and finalized again before publish.

## 26. Migration, Export, And Lock-In Avoidance

### 26.1 BYO To Managed

Flow:

1. Register managed storage allocation.
2. Copy existing repo object set to managed target.
3. Verify manifests, refs, xorbs, shards, packs, file indexes, LFS objects.
4. Freeze writes briefly or use a final delta sync.
5. Switch repo storage target in registry.
6. Run `crab fsck` equivalent proof.
7. Re-enable writes.

### 26.2 Managed To BYO Export

Flow:

1. Customer creates and verifies BYO storage connection.
2. Export job copies repo and tenant-global objects for selected repos.
3. Export job writes a direct `.crab/config.toml` target or migration bundle.
4. Export job verifies byte-identical object copy and repository integrity.
5. Customer can switch repository target or continue with managed storage.

Export is a product contract. Managed storage must not be a data trap.

### 26.3 Self-Hosted Auth/Cache To Managed

Flow:

1. Import policy.
2. Register IdP.
3. Register storage connection.
4. Configure managed cache.
5. Run dual-read diagnostic.
6. Switch client config through `crab cloud login`.
7. Retire self-hosted services after audit parity.

## 27. Failure Modes

| Failure | Expected Behavior | User Impact |
|---------|-------------------|-------------|
| Managed API unavailable | Existing self-managed repos unaffected; managed credential refresh fails | Managed operations fail after token expiry |
| Cache unavailable | CLI falls back to origin | Slower reads |
| BYO role revoked | Credential broker returns auth error | Managed access blocked until fixed |
| Protected push worker crash after publish | Retry detects landed item by durable metadata and writes missing result | No duplicate publish |
| Indexer lag | UI shows stale/degraded index and can fall back to on-demand read | Browsing delay |
| Audit ingest lag | API continues, audit lag alert fires | Compliance alert |
| Metering lag | Usage display delayed, raw events retained | Billing delay |
| KMS key disabled | Data access fails closed | Tenant admin action required |
| Signed URL expired | Caller receives 401/403 and must request new URL | Download retry |
| Storage target drift | Probe marks connection degraded, new credentials can fail closed by policy | Admin must repair |
| Partial export | Export remains incomplete and retry resumes idempotently | No cutover |

## 28. Implementation Plan

### 28.1 Repository Structure

Add platform code in a new top-level service area:

```text
crab-cloud/
├── api/                  Edge/control-plane service
├── worker/               Background jobs
├── migrations/           Database migrations
├── openapi/              Public API definitions
├── docs/                 Operator runbooks for hosted service
└── tests/                Service integration and contract tests

crates/
├── crab-cloud-contracts/ Shared DTOs for repository target, API errors, audit
└── crab-cloud-client/    CLI/SDK client for Managed Crab APIs
```

If the crate split is not ready, the same boundaries can initially live under
`crab/src/cloud/`, but the public DTOs must remain
separate from CLI-only errors.

### 28.2 Epics

#### Epic 1: Shared Contracts

Deliver:

- `ResolvedRepositoryTarget`.
- Managed API error schema.
- Cache token claims.
- Audit event schema.
- Usage event schema.
- OpenAPI spec.

Acceptance:

- CLI, SDK, API, and worker compile against the same DTOs.
- Redaction tests cover every secret-bearing field.

#### Epic 2: CLI Cloud Onboarding

Deliver:

- `crab cloud login/logout/status`.
- managed profile config.
- token cache integration.
- device code flow for headless environments.

Acceptance:

- User can login, create config, and view identity without storage access.
- Token cache never prints secrets.

#### Epic 3: Repository Resolver

Deliver:

- `/v1/repositories/resolve`.
- CLI and SDK resolver client.
- Object-store construction from physical target.
- Legacy self-managed compatibility.

Acceptance:

- `crab clone crab://org/repo` works for a managed repo whose physical bucket
  does not equal `org`.
- Existing `crab://bucket/repo` static path still works unchanged.

#### Epic 4: BYO Storage Connections

Deliver:

- storage connection CRUD.
- AWS, GCP, Azure, and S3-compatible probe contracts.
- least-privilege policy templates.
- drift detection.

Acceptance:

- Non-destructive probe proves read/write/list/head/delete/CAS under a
  disposable prefix.
- Overbroad or public bucket posture produces warnings or hard failures by
  policy.

#### Epic 5: Managed Storage

Deliver:

- storage allocator.
- tenant prefix layout.
- KMS key management.
- lifecycle and versioning policy.
- managed repo create.

Acceptance:

- New user can create a repo without any cloud account and push/clone/hydrate.
- Managed storage fsck proof passes after push.

#### Epic 6: Managed Auth And Protected Push

Deliver:

- credential broker.
- extended `crab-auth` response with storage target.
- protected push prepare/finalize.
- path and branch policy.

Acceptance:

- Denied push cannot mutate refs.
- Crash/retry windows are proven with integration tests.
- Finalize audit event includes changed paths and decision ID.

#### Epic 7: Managed Cache

Deliver:

- signed cache tokens.
- tenant-isolated cache pools.
- cache service public-internet hardening.
- cache routing in repository target.

Acceptance:

- Cache dedup does not cross tenant boundary.
- Public cache rejects unsigned or expired tokens.
- Cache outage degrades to origin for normal tenants.

#### Epic 8: Asset Index

Deliver:

- repo update events.
- indexer worker.
- refs, commits, paths, pointer metadata index.
- stale/degraded status.

Acceptance:

- Push updates are visible in catalog without full hydration.
- Index rebuild can recover from empty DB using repository content.

#### Epic 9: Asset API

Deliver:

- refs/tree/stat/read-range/read-text/diff/signed-url APIs.
- SDK-backed snapshot reads.
- policy and byte limits.
- audit and metering.

Acceptance:

- API returns resolved commit for symbolic refs.
- Range reads fetch bounded data and verify integrity.
- Unauthorized path prefix returns structured policy error.

#### Epic 10: Web Console

Deliver:

- repo browser.
- revision browser.
- file browser and metadata panel.
- storage/cache/policy/audit/billing admin views.

Acceptance:

- User can navigate assets without cloning.
- UI never exposes raw credentials or unbounded payloads.

#### Epic 11: Billing, Audit, And Operations

Deliver:

- audit pipeline.
- usage events and rollups.
- quota enforcement.
- support bundles.
- dashboards and alerts.

Acceptance:

- Every credential issuance, protected push, asset read, policy mutation, and
  storage mutation has an audit event.
- Usage rollups reconcile with raw events.

#### Epic 12: Export And Migration

Deliver:

- managed-to-BYO export.
- BYO-to-managed migration.
- self-hosted auth/cache migration guide.

Acceptance:

- Exported repo can be used without Managed Crab.
- Migration has resumable copy and integrity proof.

## 29. Validation Plan

### 29.1 Unit Tests

- DTO serialization and backward compatibility.
- Policy matching and deny precedence.
- Token validation and expiry.
- Repository target redaction.
- Storage target mapping.
- API error schema.
- Usage rollup aggregation.

### 29.2 Integration Tests

- Managed logical URL resolves to physical bucket.
- BYO AWS/GCS/Azure storage connection probes.
- Managed storage repo create and first push.
- Protected push allow/deny/stale/crash retry.
- Cache signed token accept/reject.
- Asset API snapshot reads.
- Export to BYO storage.

### 29.3 E2E Smoke Tests

Required Level 3+ smoke:

```text
new org
  -> cloud login
  -> repo create managed
  -> crab init/ship
  -> clone on clean machine
  -> hydrate file
  -> browse same asset in web console
  -> read range through API
  -> export to BYO storage
  -> clone exported repo without Managed Crab
```

BYO smoke:

```text
connect customer bucket
  -> probe
  -> create repo on BYO target
  -> protected push
  -> cache warm
  -> clone/hydrate through managed auth/cache
  -> disable cache
  -> clone/hydrate falls back to origin
```

### 29.4 Security Tests

- Cross-tenant access attempts.
- Cache dedup leak attempts.
- Path policy bypass attempts.
- Stolen/expired token replay.
- Signed URL path traversal.
- Support role privilege escalation.
- Secret redaction in logs and support bundles.
- BYO role over-permission detection.

### 29.5 Chaos Tests

- Kill protected push service between prepare and finalize.
- Kill finalize after ref update before result/audit write.
- Drop cache during hydrate.
- Revoke BYO role during clone.
- Delay audit ingest.
- Delay indexer.
- Disable KMS key.
- Restore control-plane DB from backup into staging and verify repo access.

## 30. Launch Gates

### Private Alpha

- Managed login.
- Managed repo registry.
- Managed storage in one region.
- Clone/fetch/push/hydrate.
- Basic audit.
- Manual operations runbook.

### Private Beta

- BYO storage connection.
- Protected push.
- Managed cache with signed tokens.
- Asset browser.
- Asset range API.
- Metering events.
- Export proof.

### Public Beta

- SSO for enterprise pilots.
- SCIM for enterprise pilots.
- Policy editor/import.
- Billing dashboard.
- SLO dashboards and alerting.
- Automated backup restore drill.
- Security test suite green.

### GA

GA requires all requirements in this document plus:

- Two successful restore drills.
- Export from managed storage to BYO verified by E2E.
- Cross-tenant isolation tests green.
- Cache public hardening complete.
- Protected push crash-recovery tests green.
- Audit coverage report shows no missing GA operation.
- Billing reconciliation within accepted tolerance.
- Incident response runbooks approved.
- Support access workflow audited.
- Documentation for self-managed, BYO managed, and managed-storage paths.

## 31. Deployment And Runtime Operations

### 31.1 Environments

Managed Crab runs the same service topology in every environment, with
different scale and data retention.

| Environment | Purpose | Data |
|-------------|---------|------|
| Local | Developer loop and contract tests | Disposable seeded fixtures |
| Dev | Shared integration testing | Synthetic tenants only |
| Staging | Production-like release candidate | Synthetic and approved internal tenants |
| Production | Customer traffic | Customer tenant data |

Production data must not be copied to lower environments. Any production issue
that needs replay uses redacted control-plane events, synthetic object-store
fixtures, or tenant-approved support bundles.

### 31.2 Deployment Units

Required runtime units:

```text
edge-api
  public API, browser API, repo registry, policy, credential broker

asset-api
  SDK-backed bounded reads, tree/stat/diff/signed-url APIs

worker
  storage probes, indexing, metering rollups, export, GC planning

cache-pool
  tenant-region cache services and cache token validation

admin-api
  support tooling, break-glass workflow, internal diagnostics
```

`edge-api` and `asset-api` can be one binary initially if route ownership and
resource limits are separated. `cache-pool` remains an isolated data-plane
deployment because cache data and dedup indexes are tenant-sensitive.

### 31.3 Infrastructure As Code

All production infrastructure is declared through reviewed infrastructure code.
Manual console changes are allowed only through break-glass incidents and must
be reconciled back into infrastructure code before the incident closes.

Infrastructure coverage:

- public DNS and TLS certificates
- API gateway, WAF, and rate limits
- service compute
- worker queues
- relational database and backups
- object storage buckets
- KMS keys and key policies
- cache volumes
- metrics, logs, traces, and alerts
- IAM roles and workload identities
- private networking and egress controls

### 31.4 Database Migrations

Database migrations follow expand, migrate, contract:

1. Expand: add nullable columns, tables, indexes, or dual-write paths.
2. Deploy app that writes both old and new shapes when needed.
3. Backfill with resumable tenant-scoped jobs.
4. Verify row counts and checksums.
5. Switch reads.
6. Contract only after the previous version is no longer deployed.

Every migration has:

- forward migration
- rollback plan or explicit no-rollback explanation
- expected duration
- lock-risk assessment
- backfill idempotency proof
- staging run evidence

### 31.5 Release Process

Service releases:

- build immutable artifacts
- run unit, integration, contract, and security tests
- deploy to staging
- run managed-storage and BYO smoke tests
- canary production by tenant allowlist
- monitor SLO and error budget signals
- expand rollout gradually

Client releases:

- preserve compatibility with the previous GA managed API version
- include protocol capability negotiation
- fail with actionable errors when the platform requires a newer client
- never silently fall back from managed logical URL to direct bucket semantics

### 31.6 API Versioning

Public APIs are versioned by URL and schema version:

- URL major version: `/v1`.
- Response schema version inside long-lived machine-readable payloads.
- Capability negotiation for CLI and SDK.
- Additive fields are allowed.
- Removing or changing semantics requires a new major API version.

Managed platform responses include `min_client_version` when a request cannot
be served safely by the caller's client version.

### 31.7 Feature Flags

Feature flags are tenant-scoped and auditable. Required flags:

- managed storage create
- BYO storage connect
- protected push
- managed cache
- asset API
- signed URL
- export
- enterprise SSO
- SCIM

Flags cannot bypass security checks. They only expose or hide already-gated
capabilities.

### 31.8 Configuration And Secrets

Runtime configuration is split into:

- build-time constants
- environment-specific non-secret config
- secret references
- tenant policy/config stored in the control-plane database

Secrets are injected through the secret manager or workload identity, not
checked into source, baked into images, or printed in deployment logs.

### 31.9 Runbooks

GA runbooks:

- API outage
- credential issuance failures
- BYO role revoked
- protected push incident
- cache regional outage
- asset index backlog
- audit ingest backlog
- billing event lag
- KMS key disabled
- database failover
- managed storage export failure
- tenant deletion recovery
- suspected cross-tenant access
- support break-glass access

Every runbook includes severity, symptoms, dashboards, first checks, rollback
or mitigation, customer impact, escalation, and post-incident evidence to
retain.

### 31.10 Capacity Management

Capacity plans are reviewed before each public launch stage:

- API peak request rate
- credential issuance rate
- protected push finalization rate
- cache storage and egress per tenant region
- asset index rows and growth
- database connection count
- worker queue depth
- object-store operation rate
- KMS operation rate

Autoscaling cannot be the only plan for stateful cache pools. Cache pools need
explicit disk capacity, eviction policy, and saturation alerts.

## 32. Appendix: Schemas

### 32.1 Cache Token Claims

```json
{
  "iss": "https://api.crab.build",
  "sub": "cache-client",
  "aud": "crab-cache",
  "tenant_id": "ten_012345",
  "repo_id": "repo_012345",
  "operations": ["read", "write", "dedup"],
  "dedup_scope": "tenant",
  "exp": 1782917700,
  "iat": 1782916800,
  "jti": "tok_012345"
}
```

### 32.2 Audit Event

```json
{
  "schema": "crab.audit-event",
  "version": "1.0",
  "event_id": "aud_012345",
  "tenant_id": "ten_012345",
  "actor": {
    "type": "user",
    "id": "usr_012345"
  },
  "operation": "repo.push.finalize",
  "resource": {
    "type": "repository",
    "id": "repo_012345",
    "logical_url": "crab://acme/models"
  },
  "decision": "allow",
  "decision_id": "pd_012345",
  "reason_code": "policy.allow",
  "request_id": "req_012345",
  "created_at": "2026-07-01T12:00:00Z",
  "details": {
    "ref_updates": 1,
    "changed_paths": 12,
    "storage_mode": "managed"
  }
}
```

### 32.3 Usage Event

```json
{
  "schema": "crab.usage-event",
  "version": "1.0",
  "event_id": "use_012345",
  "tenant_id": "ten_012345",
  "repo_id": "repo_012345",
  "meter": "asset_api_read_bytes",
  "quantity": 65536,
  "unit": "bytes",
  "region": "us-west-2",
  "storage_mode": "managed",
  "created_at": "2026-07-01T12:00:00Z",
  "dimensions": {
    "operation": "read_range"
  }
}
```

### 32.4 Storage Connection Probe Result

```json
{
  "schema": "crab.storage-probe",
  "version": "1.0",
  "connection_id": "stc_012345",
  "tenant_id": "ten_012345",
  "provider": "aws",
  "region": "us-west-2",
  "status": "verified",
  "checks": [
    {"name": "bucket_exists", "status": "pass"},
    {"name": "read_probe", "status": "pass"},
    {"name": "write_probe", "status": "pass"},
    {"name": "cas_probe", "status": "pass"},
    {"name": "delete_probe", "status": "pass"},
    {"name": "public_access_block", "status": "pass"}
  ],
  "created_at": "2026-07-01T12:00:00Z"
}
```
