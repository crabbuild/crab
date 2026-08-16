# 0003: Keep PostgreSQL Authoritative For Durable Jobs

- Status: Accepted
- Date: 2026-08-11
- Owners: Crab service and operations maintainers

## Context

Repository provisioning, cleanup, export, GC, reconciliation, and recovery need
durable background execution. The service must run on AWS, other clouds, and
on-premises without requiring a provider-specific queue. API state changes and
job creation also need one transactional commit boundary.

## Decision

The PostgreSQL `jobs` table is the authoritative queue in every deployment.
Workers claim ready rows in short transactions using
`FOR UPDATE SKIP LOCKED`, assign bounded leases, and heartbeat long-running
work. Handlers checkpoint durable progress and are idempotent. Errors are
classified as retryable, terminal, or requiring operator action; retryable work
uses bounded exponential backoff with jitter and a per-kind retry budget.
Exhausted work enters `dead_letter` and alerts.

API mutations write jobs and transactional-outbox records in the same database
transaction as their domain state. PostgreSQL `NOTIFY`, SQS, or another queue
may carry wake-up hints, but workers always re-read PostgreSQL and correctness
does not depend on delivery, ordering, or uniqueness of a hint.

## Invariants

- No committed domain mutation can lose its corresponding durable job or
  outbox record.
- At-least-once execution cannot corrupt domain or object-store state.
- Lease expiry permits recovery after worker termination.
- A heartbeat never extends ownership after cancellation or loss of the lease.
- Payloads are explicitly versioned; large payloads are digest-addressed in a
  service-owned object root.
- GC and deletion handlers validate repository identity, placement generation,
  snapshot, and grace cutoff before each destructive phase.

## Consequences

PostgreSQL availability is a control-plane dependency and requires HA, bounded
pools, PITR, and tested restore procedures. Provider queues can improve wake-up
latency and autoscaling, but they remain disposable accelerators.

## Rejected Alternatives

- SQS, Kafka, or another provider queue as authority prevents the same service
  profile from running on-premises and makes transactional enqueue harder.
- PostgreSQL `NOTIFY` alone is not durable and can lose signals across client
  disconnects.
- In-memory scheduling cannot recover work after restart or coordinate replicas.
