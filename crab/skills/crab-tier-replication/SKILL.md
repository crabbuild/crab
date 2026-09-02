---
name: crab-tier-replication
description: Operate Crab storage lifecycle tiers, archived-object restore, read replicas, active-active coordination, backfill, failover, cost reporting, and replication evidence.
---

# Crab tiers and replication

Treat tiering and replication as durability and availability policy, not as a
faster garbage collector. Every move must preserve content identity, reference
reachability, and an auditable recovery path.

## Tier lifecycle

1. Inventory live manifests, refs, object age, storage class, restore state,
   retention, and pending transitions.
2. `tier plan` renders provider lifecycle rules. `tier plan --apply` performs
   the provider mutation, `--dry-run` keeps it read-only, `--merge` preserves
   non-Crab rules, and `tier rollback <backup>` restores a saved lifecycle
   configuration.
3. Archived objects require an explicit restore request, selected tier, and
   restore duration. Hydration must wait for readable state or report the
   provider failure; never substitute fabricated bytes.
4. Verify post-transition metadata, object identity, retention and billing
   class. Keep rollback information until the retention boundary passes.

## Replica lifecycle

- `replica status` reports health, lag, read readiness, writer state, and
  evidence freshness.
- `replica doctor` diagnoses credentials, endpoint, control-plane, data-plane,
  permissions, and configuration problems without mutating state.
- `replica add/export/cost/runbook` plans provider resources and operations;
  provider changes require the command's explicit apply boundary.
- Backfill is separate from read enablement. Inventory historical objects,
  copy missing data, verify checksums, then advance the durable watermark.
- Use `wait`, deep `verify`, and `backfill status` before `enable`. Disable or
  remove a replica explicitly; `remove --apply` only owns documented
  Crab-created reversible resources.
- Enable reads only after manifest-referenced objects and current indexes are
  verified in the target region.
- Active-active mode requires declared writers and a managed coordinator.
  `writers` and `coordinator` manage those identities; the `failover` status,
  plan, fence, run, and resume operations form one fenced state machine.
- Read-replica `promote` and guarded `set-primary` differ from active-active
  failover. Preview them and require their explicit force/apply contract.
- Repair must converge regional state from coordinator truth, not from a stale
  replica snapshot.
- `diagnostics` collects a redacted bundle. `certify` runs stricter readiness
  gates, and `evidence verify` validates retained certification artifacts.
  Status, cost, doctor, or a generated runbook alone is not production proof.

## Invariants

- Active-active writes have one authoritative coordination decision per ref.
- Every lock, lease, fence, and promotion has an expiry or release path.
- A replica is not read-ready while any live manifest object is missing or
  unverified.
- Restores and backfills are idempotent and resumable.
- Audit evidence names the source, destination, generation, checksum, actor,
  and time without exposing credentials.

## Verification

Use separate disposable prefixes or buckets for source and replica. Test a
normal transition, delayed restore, missing object, stale watermark, split
brain prevention, fenced failover, repair, and rollback. Prove fresh reads and
byte-identical hydration from the promoted or restored location, then inspect
the retained evidence bundle.
