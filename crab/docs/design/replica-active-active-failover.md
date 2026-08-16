# Crab Active-Active Failover Policy

Status: accepted for the current enterprise replication release.

## Context

Crab active-active writes are linearized by a managed coordinator. Object
storage remains the immutable data plane, but mutable ref authority, repo epoch,
writer admission, transaction state, and regional manifest materialization are
coordinator-owned. A regional outage can affect object writes, object
replication, coordinator reachability, repair materialization, or operator
traffic routing independently.

Automatic write failover is therefore not a retry policy. It is a distributed
systems protocol that must prove the old epoch is fenced before any new write
ingress is admitted elsewhere.

## Decision

Crab supports manual fenced failover with a read-only automation plan:

- `crab replica failover status` reports coordinator and write-admission state.
- `crab replica failover plan --writer-unhealthy <name> --json` turns an
  external regional health signal into the next safe command only after the
  configured writer and linearizable coordinator health are verified.
- `crab replica failover run --apply --writer-unhealthy <name> --json` consumes
  the same decision and applies one safe action, such as fencing writes. It
  returns a blocked no-op for `hold` or `monitor`.
- `crab replica failover fence --apply` increments the coordinator epoch and
  marks writes unhealthy.
- New active-active pushes fail closed while writes are fenced.
- `crab replica repair --from-coordinator` reconciles regional manifests from
  coordinator truth after referenced objects exist in the target region.
- `crab replica failover plan --repair-verified --json` recommends resume only
  after the coordinator is fenced and repair proof is present.
- `crab replica failover run --apply --repair-verified --json` can apply that
  one resume step after the same proof gate passes.
- `crab replica failover resume --repair-verified --apply` re-admits writes
  without rewinding the fenced epoch after repair and external provider failover
  checks are complete.

Crab does not run an always-on autonomous write failover controller in this
release. The machine-readable failover payload advertises a typed
`automation_plan` and `failover run` can apply one actionable decision per
invocation, but Crab also keeps
`automatic_write_failover_supported = false` and
`orchestration = "manual-fence-repair-resume"` so operators and automation do
not infer stronger behavior from the existence of fence/resume commands. A
future autonomous controller must retain the external health and repair
evidence, run repeated failover ticks under supervision, and pass the live
cross-region failover smoke before Crab can set always-on automatic failover
support to true.

## Required Invariants

- Split-brain prevention wins over availability. If coordinator state,
  provider topology, or previous-epoch fencing is ambiguous, writes fail closed.
- Regional manifests are never write authority in active-active mode.
- Same-ref conflicts preserve Git CAS semantics: one update succeeds and
  divergent concurrent updates are rejected.
- Retried pushes reuse stable operation IDs and return the prior committed
  result when the transaction already committed.
- GC and repair read coordinator transaction state before deleting or
  materializing objects.
- Resume is allowed only after external provider failover checks, object
  replication checks, and coordinator-backed repair are complete.

## Future Automated Failover Requirements

An automated write-failover orchestrator must ship under a separate design and
must prove at least:

- A linearizable source of truth for coordinator epoch ownership during regional
  coordinator failure.
- Fencing of stale writer leases before admitting writes in another region.
- Recovery rules for pending, objects-uploaded, committed, materialized, and
  aborted transactions.
- Provider-specific object-write and replication health checks for every writer
  region involved in admission.
- Bounded retry/idempotency behavior for every orchestrator action.
- Operator-visible audit records and retained evidence that bind each failover
  to a run ID, epoch transition, writer region, and coordinator provider.
- Load and failure drills against the live provider topology being certified.

Until those requirements have implementation and live evidence, Crab must keep
manual fence/repair/resume as the only write-failover control surface.
