---
name: crab-tier-replication
description: Operate Crab storage lifecycle tiers, archived-object restore, read replicas, active-active coordination, backfill, readiness, repair, failover, evidence, runbooks, and replication cost reports. Use whenever a request mentions `crab tier`, `crab replica`, `optimize tiers`, `optimize replicas`, failover, fencing, coordinator, or multi-cloud replication.
compatibility: Crab CLI with provider-specific tier or replication features enabled and the required cloud permissions.
---

# Crab tiering and replication

Treat lifecycle tiering and replication as provider-backed operational
contracts. Verify configuration and readiness before changing a rule or
routing decision, and keep a reversible plan for destructive or expensive
actions.

## Command scope

`tier` plan/apply/rollback, `optimize tiers`, `replica`,
`optimize replicas` (status, doctor, verify, backfill, wait, repair, cost,
runbook, diagnostics, certify, evidence, failover), and hidden coordinator
management when debugging the active-active control plane.

## Tiering loop

1. Inspect provider, bucket, region, current rules, object classes, and the
   configured restore policy.
2. Generate a plan and review conflicts, object counts, estimated cost, and
   whether the rule is scoped to the intended repository/prefix.
3. Apply only after authorization. Retain the operation identity and audit
   event; use rollback when the command provides it.
4. For archived content, verify restore completion and readability before
   asking `crab-large-files` to hydrate. A successful restore request is not
   the same as readable bytes.

## Replica and failover loop

1. Run `doctor`, `status`, and readiness/backfill checks before enabling reads.
2. Verify manifest-referenced objects with the documented exhaustive or sample
   mode, then wait for the replica's read-ready state.
3. For repair, failover, fencing, or resume, capture a plan and confirm the
   coordinator's authoritative state. Do not improvise a ref or region switch.
4. After mutation, verify refs, manifests, object availability, routing state,
   lag, audit/evidence artifacts, and a read from the selected region.

## Safety

- Provider flags and feature gates are part of the implementation contract;
  read `crab/Cargo.toml` and the provider module before claiming support.
- Never print credentials, access tokens, or private endpoints.
- Treat replica repair, failover, fence, tier apply, and early restore/delete
  as externally visible operations with explicit scope.
- Use `crab-cli-verification` for live provider/RustFS proof only when the
  fixture actually exercises the feature; otherwise state what remains
  unverified.

## Read first

- `crab/docs/guides/tier.md`
- `crab/docs/guides/replica.md`
- `crab/docs/guides/cost.md`
- `crab/docs/design/replica-active-active-failover.md`
- `crab/docs/design/replica-enterprise-readiness.md`
- `crab/src/{tier,replication,restripe}/`
- `crab/src/cmd/{tier/,replica,coordinator}.rs`
- `crates/crab-coordination/README.md`
- `.codex/skills/crab-cli-core/references/contracts.md`
