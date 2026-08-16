# 0004: Use The Gateway As The Portable Transfer Fallback

- Status: Accepted
- Date: 2026-08-11
- Owners: Crab service, provider, and security maintainers

## Context

AWS can issue temporary credentials constrained to a repository prefix, while
some object stores and on-premises installations cannot safely issue an
equivalent grant. Returning permanent storage keys or pretending every provider
has AWS semantics would expand access and make revocation unreliable.

## Decision

Direct transfer is used only when a provider adapter proves scoped temporary
credentials with the required operation, prefix, expiry, and audit semantics.
Otherwise the deployment uses the Crab gateway. The gateway exposes bounded
Crab object operations, not a general S3-compatible credential or arbitrary
bucket/key API.

Gateway grants are short-lived Ed25519-signed tokens with an explicit issuer,
audience, key ID, grant ID, repository and placement IDs, operation, optional
push ID, issued/expiry times, and token generation. The gateway verifies policy,
revocation generation, physical-key mapping, content bounds, and placement
generation locally. Client push grants write only staging; canonical mutation
still passes through protected finalize.

The gateway has a distinct listener/router and token audience from the control
plane. It has no control-plane mutation routes. TLS is terminated by native
rustls or an explicitly trusted proxy boundary.

## Invariants

- Provider selection comes from deployment configuration, never repository URL
  inference.
- Unsupported or static-key origins require gateway mode.
- A gateway grant cannot address arbitrary buckets, prefixes, or canonical
  write keys.
- Revocation is checked for every gateway request within the documented cache
  propagation bound.
- Read data remains byte-verified and push data remains server-finalized in both
  transfer modes.
- Transfer mode is visible in diagnostics and audit events.

## Rollback

A native adapter can be rolled back to gateway mode for the same placement
generation after active direct grants expire. A provider-specific path is
promoted only after it passes the shared scope, expiry, redaction,
cancellation, and real transfer conformance suites.

## Rejected Alternatives

- Permanent per-tenant keys have unacceptable scope, rotation, revocation, and
  compromise impact.
- Presigning every object request amplifies the control plane for Crab's many
  content-addressed objects; it may be added later as another transport.
- Provider-shaped compatibility interfaces hide weaker authorization semantics.
