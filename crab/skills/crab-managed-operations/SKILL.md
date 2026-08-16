---
name: crab-managed-operations
description: Manage Crab authentication, managed-service organizations and repositories, memberships, service accounts, audit events, and signed release manifests.
---

# Crab managed operations

Keep identity, service selection, authorization, resource ownership, audit, and
release evidence explicit. A successful API response is not enough: verify the
selected service and the resulting state.

## Authentication

1. Resolve the service origin and provider before login. Use device flow for
   headless sessions and an explicit enterprise CA only when required.
2. Store tokens in the supported credential store; never print them or place
   them in arguments, logs, JSON reports, or generated manifests.
3. `auth status` is read-only. `logout` must make clear whether it removes one
   service/provider or all cached credentials.
4. For static or cloud-provider authentication, validate audience, region,
   certificate, clock, and permission failures separately.

## Administration

- Before create/update/delete, confirm service, organization, repository,
  member, actor, and requested ownership change.
- Read current state, use idempotent operations where supported, and do not
  silently rename or transfer ownership.
- Treat service-account keys and one-time secrets as write-only material. Show
  only identifiers and redacted status after creation.
- Keep pagination, retry, rate limits, and authorization failures distinct in
  errors so automation can decide whether to retry.

## Audit and releases

- Audit log, verify, and export operate on an append-only event chain. Check
  schema, sequence, digest, timestamp, redaction, and operation filters.
- A release manifest binds a Git revision to pointer inventory, workflow
  metadata, parameters, metrics, and optional signatures.
- Release creation should reject unintended dirty state. A forced override is
  an explicit policy decision and must appear in evidence.
- Deep verification reconstructs pointer-backed files and checks identity; JSON
  parsing alone is not content proof.
- Export preserves signatures, identity metadata, and byte content. Verify the
  exported artifact from a clean consumer.

## Verification

Test expired and missing credentials, wrong service, insufficient permission,
duplicate creation, membership removal, audit tampering, invalid signatures,
and deep release verification. Redact all secrets in captured output and
record the actor, resource, revision, and resulting audit event.
