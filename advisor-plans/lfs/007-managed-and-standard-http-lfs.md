# Phase 6: Close managed direct-transfer gaps

> **Executor instructions**: This phase stays inside the direct serverless product. Do not add or deploy an LFS HTTP gateway. Update the Phase 6 row when complete.
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

Standalone custom transfer mode is compatible with Git LFS only after repository-specific configuration and direct cloud authorization. Managed repositories currently reject standalone writes because uploads lack protected-push-scoped authorization. This phase closes the managed direct-transfer, authorization, and durability gaps while preserving the no-service deployment model.

## Current state

- `crab/src/cmd/lfs/install.rs:18` configures standalone mode, which official Git LFS says bypasses the API.
- `crab/src/cmd/lfs/store_setup.rs:61` permits managed reads but rejects ordinary managed LFS writes outside protected push.
- `crates/crab-auth-server/README.md:1` is a JSON-speaking execution boundary, not HTTP service composition.
- `crab/src/cmd/lfs/fetch.rs` emits Crab-specific `crab-lfs://` action JSON; it is not a standard Batch response.
- `crab/src/lfs/lock.rs` owns object-store lock mechanics and the CLI adapts them to Git LFS-style lock commands.
- Official contracts: Git LFS custom-transfer and locking docs linked from `advisor-plans/lfs/README.md`.

## Commands you will need

Exact package names depend on the approved composition owner. At minimum:

| Purpose | Command | Expected |
|---------|---------|----------|
| Shared contracts | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo test -p crab-lfs --locked` | all pass |
| Workspace check | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo check --workspace --locked` | exit 0 |
| Git LFS black box | qualification harness standalone profile | unmodified client delegates to Crab and passes |

## Scope

**In scope**:
- a direct-transfer ADR naming object-store authorization, repository identity, and owner crate
- standalone transfer request/response compatibility fixtures
- direct upload/download/verify behavior with scoped credentials
- CLI File Locking create/list/unlock/verify behavior
- repository-scoped authorization and audit
- managed download and upload grants bound to repository, operation, OID, size, and protected-push dependency manifest
- unmodified Git LFS integration tests

**Out of scope**:
- removing native or standalone-agent modes
- exposing or shipping an HTTP LFS gateway
- exposing bucket credentials to clients
- reusing Crab CLI JSON as HTTP protocol models
- SSH server implementation unless the architecture decision explicitly selects it
- vendor-specific behavior in `crab-lfs`

## Git workflow

- Branch: `advisor/lfs-phase-5-direct`
- Land direct-transfer contracts before managed authorization changes. Security review is mandatory for credential changes.

## Steps

### Step 1: Approve the managed direct-transfer architecture

Write an ADR that defines managed standalone download/upload grants and their protected-push binding. Specify repository identity, authorization claims, tenant prefix isolation, token expiry, upload verification, and direct-mode coexistence. Record that no LFS HTTP gateway or service deployment is part of the product.

**Verify**: ADR approval names the direct-transfer owner and no unresolved auth or tenancy decision remains.

### Step 2: Complete managed standalone-agent transfers

Read and validate the agent init operation before selecting credentials. Use repository-scoped hydrate grants for download. For upload, issue a short-lived staging grant restricted to declared OID/size and operation; verified completion creates the Phase 4 receipt and binds the dependency to protected-push admission. Upload alone must never publish a ref.

**Verify**: official Git LFS standalone mode uploads/downloads a managed repository; expired/revoked/cross-prefix/unlisted-OID grants fail closed and ref publication still requires the full dependency manifest.

### Step 3: Implement strict standalone protocol models

Model the standalone init, upload, download, progress, complete, and terminate events exactly as official Git LFS custom-transfer docs. Reject unknown operations, malformed OIDs, negative/overflow sizes, and cross-repository input before storage access.

**Verify**: official/example fixtures round-trip; fuzz/property tests never panic and enforce limits.

### Step 4: Implement direct transfers

For download, resolve only an authorized repository-scoped object store. For upload, require a scoped short-lived grant where managed auth is enabled; completion must verify expected OID/size and create the Phase 4 receipt before success. Never expose long-lived credentials through the agent protocol.

**Verify**: black-box tests with unmodified Git LFS upload/download objects, reject wrong hashes/sizes, reject expired/replayed actions, and isolate two repository prefixes.

### Step 5: Harden File Locking commands

Keep the CLI lock commands aligned with Git LFS lock semantics. Implement create, list with pagination/filtering, unlock with force semantics, and verify with `ours`/`theirs`. Push authorization must verify applicable locks at the final ref-publication boundary.

**Verify**: official locking request fixtures pass; concurrent clients cannot acquire the same path; stale unlock cannot remove a newer lock; locked-path push is rejected.

### Step 6: Add direct authorization, limits, and audit

Wire repository-scoped authorization, transfer/body/time limits, cancellation, and structured audit events into the local agent and direct object-store path. Ensure managed LFS writes use protected-push/finalization policy where required.

**Verify**: negative tests cover anonymous, read-only, cross-tenant, oversized, timeout, throttled, and revoked credentials.

### Step 7: Qualify the local agent and update install/docs

Run supported Git LFS versions on Linux, macOS, and Windows with repository-scoped standalone-agent configuration. Document that standard HTTP discovery remains an external-server profile; `crab lfs install` must only configure the local direct agent.

**Verify**: `git lfs env` reports the Crab standalone agent and the standalone matrix passes.

## Test plan

- Unit-test strict standalone protocol models, limits, serialization, and authorization decisions.
- Integration-test managed grants and direct object-store transfers against ephemeral RustFS with two repository prefixes.
- Black-box test official Git LFS direct standalone-managed mode, including upload/download/lock workflows.
- Fuzz malformed events and inject expiry, replay, wrong size/OID, revocation, throttling, timeout, and storage failures.

## Acceptance criteria

- [ ] An approved ADR names the direct-transfer owner, auth model, tenancy boundary, and grant lifetime; it records that no LFS gateway is shipped.
- [ ] Official Git LFS standalone mode can upload/download managed repositories with least-privilege expiring grants.
- [ ] Unmodified Git LFS clients pass standalone-agent upload and download with Crab's direct object-store authorization.
- [ ] File Locking create/list/unlock/verify and push lock enforcement pass concurrency tests.
- [ ] Every transfer is repository-authorized, size/time bounded, and audited without secrets.
- [ ] Wrong, truncated, expired, replayed, and cross-tenant actions fail closed.
- [ ] Native and standalone-agent profiles remain passing.

## STOP conditions

- A proposed change requires an LFS gateway or HTTP service deployment.
- The only proposed auth model exposes general bucket credentials.
- Upload completion can make refs visible before object verification/receipt publication.
- The managed write path would bypass protected-push authorization.
- Protocol behavior is inferred instead of verified against official Git LFS docs/source.

## Maintenance notes

This phase hardens the local direct-transfer security boundary. Compatibility and security fixtures become release-blocking; no service deployment is introduced.
