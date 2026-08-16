# Managed Service Architecture Decisions

These records define the non-negotiable ownership and portability boundaries
for the managed Crab service. Implementation and deployment changes must cite
and preserve the applicable decision or replace it with a superseding record.

| Decision | Status |
|----------|--------|
| [0001: Separate logical and physical repository identity](0001-logical-and-physical-repository-identity.md) | Accepted |
| [0002: Isolate V1 repository storage roots](0002-repository-isolated-v1-storage.md) | Accepted |
| [0003: Keep PostgreSQL authoritative for durable jobs](0003-postgresql-authoritative-jobs.md) | Accepted |
| [0004: Use the gateway as the portable transfer fallback](0004-portable-gateway-fallback.md) | Accepted |
