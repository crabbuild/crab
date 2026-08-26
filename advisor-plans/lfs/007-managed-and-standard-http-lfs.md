# Phase 6: Complete managed transfers and expose a standard Git LFS HTTP gateway

> **Executor instructions**: This is a managed/product composition surface, not a change to direct serverless mode. Implement only after an architecture decision names the deployment and auth owner. Update the Phase 6 row when complete.
>
> **Drift check (run first)**: `git diff --stat 2cbd0d92..HEAD -- crates/crab-auth-server crates/crab-auth crates/crab-lfs crates/crab-storage crab/src/cmd/lfs/install.rs crab/docs packages/web/content/docs`

## Status

- **Priority**: P2
- **Effort**: XL
- **Risk**: HIGH
- **Depends on**: Phases 0–4
- **Category**: feature, security, compatibility
- **Planned at**: commit `2cbd0d92`, 2026-08-25

## Why this matters

Standalone custom transfer mode is compatible with Git LFS only after repository-specific configuration and direct cloud authorization. Managed repositories currently reject standalone writes because uploads lack protected-push-scoped authorization. Unmodified clients also expect HTTPS discovery/authentication, Batch negotiation, basic transfer actions, and File Locking endpoints. This phase closes managed standalone transfers first, then supplies the optional HTTP layer while preserving direct modes.

## Current state

- `crab/src/cmd/lfs/install.rs:18` configures standalone mode, which official Git LFS says bypasses the API.
- `crab/src/cmd/lfs/store_setup.rs:61` permits managed reads but rejects ordinary managed LFS writes outside protected push.
- `crates/crab-auth-server/README.md:1` is a JSON-speaking execution boundary, not HTTP service composition.
- `crab/src/cmd/lfs/fetch.rs` emits Crab-specific `crab-lfs://` action JSON; it is not a standard Batch response.
- `crab/src/lfs/lock.rs` owns object-store lock mechanics but not HTTP request/response contracts.
- Official contracts: Git LFS API README, Batch API, basic transfers, and locking API linked from `advisor-plans/lfs/README.md`.

## Commands you will need

Exact package names depend on the approved composition owner. At minimum:

| Purpose | Command | Expected |
|---------|---------|----------|
| Shared contracts | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo test -p crab-lfs -p crab-auth-server --locked` | all pass |
| Workspace check | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo check --workspace --locked` | exit 0 |
| Git LFS black box | qualification harness HTTP profile | unmodified client passes |

## Scope

**In scope**:
- an ADR/design decision naming gateway deployment, URL discovery, auth, tenancy, rate limits, and owner crate
- standard Batch request/response models
- basic upload/download/verify actions with short expiry
- File Locking create/list/delete/verify adapters
- repository-scoped authorization and audit
- managed download and upload grants bound to repository, operation, OID, size, and protected-push dependency manifest
- unmodified Git LFS integration tests

**Out of scope**:
- removing native or standalone-agent modes
- exposing bucket credentials to clients
- reusing Crab CLI JSON as HTTP protocol models
- SSH server implementation unless the architecture decision explicitly selects it
- vendor-specific behavior in `crab-lfs`

## Git workflow

- Branch: `advisor/lfs-phase-5-http`
- Land ADR/contracts before handlers. Security review is mandatory before deployment.

## Steps

### Step 1: Approve the managed transfer and gateway architecture

Write an ADR that first defines managed standalone download/upload grants and their protected-push binding, then compares HTTP deployment in the managed auth service, a new top-level gateway binary, and provider-native signed URLs. Select one owner. Specify repository identity, URL discovery (`lfs.url`/Git remote derivation), authentication challenge, authorization claims, tenant prefix isolation, token/action expiry, upload verification, rate/size limits, observability, and direct-mode coexistence.

**Verify**: ADR approval names one deployable owner and no unresolved auth or tenancy decision remains.

### Step 2: Complete managed standalone-agent transfers

Read and validate the agent init operation before selecting credentials. Use repository-scoped hydrate grants for download. For upload, issue a short-lived staging grant restricted to declared OID/size and operation; verified completion creates the Phase 4 receipt and binds the dependency to protected-push admission. Upload alone must never publish a ref.

**Verify**: official Git LFS standalone mode uploads/downloads a managed repository; expired/revoked/cross-prefix/unlisted-OID grants fail closed and ref publication still requires the full dependency manifest.

### Step 3: Implement strict protocol models

Model media types, operation, transfers, ref, objects, hash algorithm, per-object actions/errors, href/header/expiry, and pagination exactly as official docs. Reject unknown hash algorithms, malformed OIDs, negative/overflow sizes, excessive batch size, and cross-repository input before storage access.

**Verify**: official/example fixtures round-trip; fuzz/property tests never panic and enforce limits.

### Step 4: Implement Batch and basic transfers

For download, return actions only after authorization and trusted presence. For upload, return a scoped short-lived action; completion must verify expected OID/size and create the Phase 4 receipt before success. Support the `basic` transfer adapter. Never return long-lived bucket credentials.

**Verify**: black-box tests with unmodified Git LFS upload/download objects, reject wrong hashes/sizes, reject expired/replayed actions, and isolate two repository prefixes.

### Step 5: Implement File Locking API adapters

Translate standard owner/timestamp/ID models to the Phase 4 lock state machine. Implement create, list with pagination/filtering, delete with force semantics, and verify with `ours`/`theirs`. Push authorization must verify applicable locks at the final ref-publication boundary.

**Verify**: official locking request fixtures pass; concurrent clients cannot acquire the same path; stale unlock cannot remove a newer lock; locked-path push is rejected.

### Step 6: Add discovery, auth, limits, and audit

Wire HTTPS routes, content negotiation, authentication challenge, repository-scoped authorization, request/body/time limits, rate limiting, cancellation, and structured audit events. Redact authorization headers and action URLs. Ensure managed LFS writes use protected-push/finalization policy where required.

**Verify**: negative tests cover anonymous, read-only, cross-tenant, oversized, timeout, throttled, and revoked credentials.

### Step 7: Qualify unmodified clients and update install/docs

Run supported Git LFS versions on Linux, macOS, and Windows without custom transfer configuration. Only after passing, document HTTP profile support. `crab lfs install` may offer an explicit HTTP mode but must not silently replace working direct mode.

**Verify**: `git lfs env` reports a usable HTTPS endpoint and the Phase 6 HTTP matrix passes.

## Test plan

- Unit-test strict protocol models, limits, serialization, pagination, and authorization decisions.
- Integration-test managed grants and HTTP handlers against ephemeral RustFS with two tenants and two repository prefixes.
- Black-box test official Git LFS direct standalone-managed mode and unmodified HTTP mode, including upload/download/lock workflows.
- Fuzz malformed requests and inject expiry, replay, wrong size/OID, revocation, throttling, timeout, and storage failures.

## Acceptance criteria

- [ ] An approved ADR names service owner, auth model, tenancy boundary, URL discovery, and deployment.
- [ ] Official Git LFS standalone mode can upload/download managed repositories with least-privilege expiring grants.
- [ ] Unmodified Git LFS clients pass Batch/basic upload and download without cloud credentials.
- [ ] File Locking API create/list/delete/verify and push lock enforcement pass concurrency tests.
- [ ] Every request is repository-authorized, size/rate/time bounded, and audited without secrets.
- [ ] Wrong, truncated, expired, replayed, and cross-tenant actions fail closed.
- [ ] Native and standalone-agent profiles remain passing.

## STOP conditions

- No service owner or HTTPS deployment is approved.
- The only proposed auth model exposes general bucket credentials.
- Upload completion can make refs visible before object verification/receipt publication.
- The managed write path would bypass protected-push authorization.
- Protocol behavior is inferred instead of verified against official Git LFS docs/source.

## Maintenance notes

This phase creates an internet-facing security boundary. Compatibility and security fixtures become release-blocking; direct serverless behavior must remain separately tested.
