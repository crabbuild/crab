# 0002: Isolate V1 Repository Storage Roots

- Status: Accepted
- Date: 2026-08-11
- Owners: Crab service, storage, and security maintainers

## Context

Direct temporary object-store grants authorize by bucket and key prefix. If two
repositories with different ACLs share content-addressed objects, a grant for
one repository can reveal data from the other. Tenant-only prefix isolation is
therefore insufficient while direct grants are supported.

## Decision

Every V1 repository receives an opaque physical root:

```text
environments/{environment_id}/repositories/{repository_uuid}/
```

The existing Crab storage layout is rooted beneath it without format changes.
Organization and repository slugs never contribute to the physical name.
Canonical service roles and transfer grants are scoped to the stored root.
Delete, export, cleanup, and GC jobs use the immutable repository and placement
IDs and validate that exact root before operating.

V1 performs no cross-repository or cross-tenant physical deduplication. A
future tenant-shared layout requires a separate security decision and must use
gateway or per-object authorization that proves callers cannot observe objects
outside the repository ACL.

## Invariants

- A repository grant cannot list, read, create, overwrite, or delete objects in
  another repository root.
- Client push grants address only `staging/{push_id}/`; clients never mutate
  canonical refs or metadata directly.
- Canonical mutation remains service-owned and preserves Crab locking,
  completeness, and compare-and-swap rules.
- Destructive jobs never operate at bucket scope.
- Renames and transfers do not rewrite object keys.

## Consequences

Repository isolation spends more storage than tenant-wide deduplication but
makes direct grants auditable and enforceable with provider-native prefix
controls. Metrics record the cost so a later gateway-authorized design can be
evaluated without weakening V1.

## Rejected Alternatives

- Tenant-shared content roots are incompatible with repository-specific direct
  credentials.
- Slug-derived paths make rename, transfer, deletion, and collision behavior
  part of the storage contract.
- Bucket-per-repository placement creates operational and quota pressure without
  improving the root-level authorization invariant.
