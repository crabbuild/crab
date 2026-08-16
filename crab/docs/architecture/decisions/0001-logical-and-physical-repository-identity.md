# 0001: Separate Logical And Physical Repository Identity

- Status: Accepted
- Date: 2026-08-11
- Owners: Crab service and CLI maintainers

## Context

Managed repositories need a stable user-facing identity while operators retain
the freedom to move content between accounts, regions, buckets, and prefixes.
The existing direct-storage URL treats the authority as an object-store bucket.
Reusing that field for a managed authority would let managed policy leak into
generic storage construction and could route credentials to an unintended
endpoint.

## Decision

Repository parsing produces a typed locator with mutually exclusive managed
and direct variants. The hosted managed grammar is exactly:

```text
crab://crab.build/{organization}/{repository}
```

It has exactly two normalized, non-empty lowercase path segments and no query
or fragment. `crab.build` is reserved as a managed authority. Other authorities
remain direct-storage authorities unless an installed service profile declares
them managed. The client never probes arbitrary authorities to infer a service.

The managed locator remains in Git configuration and Crab remote metadata. A
service resolves it to an opaque placement only after authentication and
authorization. Physical account, endpoint, region, bucket, prefix, placement
generation, and temporary credentials are runtime data. They must not replace
the logical URL in repository configuration or appear in user-facing identity.

## Invariants

- A managed locator cannot reach a generic object-store builder before service
  resolution returns a validated transfer grant.
- Renaming an organization or repository changes catalog aliases, not storage.
- Discovery is performed only for an explicitly managed authority.
- BYOC `crab://{bucket}/{prefix}` behavior does not depend on network state.
- Logs and audit events identify repositories by service ID, not secret-bearing
  physical credentials or storage URLs.

## Consequences

CLI, SDK, desktop, and service contracts share one locator type. All storage
entry points must exhaustively match it. Resolved placements may be cached only
for their bounded grant/discovery lifetimes and are invalidated by placement
generation changes.

## Rejected Alternatives

- `crab-managed://` duplicates Git remote-helper dispatch and violates the
  public `crab://crab.build/...` contract.
- Treating `crab.build` as a bucket can expose the wrong credential and policy
  path.
- HTTPS probing of every authority makes behavior network-dependent and creates
  credential-routing and SSRF risks.
