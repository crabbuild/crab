# crab-coordination

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-coordination/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns lease/admission and active-active write coordination mechanics. Callers choose operation policy and must maintain the required ref/GC authority through publication.

## Read first

1. `crates/crab-coordination/src/lib.rs` — feature-gated exports.
2. `crates/crab-coordination/src/push_lock.rs` — `PushLock`: holder-checked lease lifecycle.
3. `crates/crab-coordination/src/lease_operation.rs` — `while_renewing`: renewal while draining an operation.
4. `crates/crab-coordination/src/gc_fence.rs` — `GcFenceLease`: shared/exclusive GC admission.
5. `crates/crab-coordination/src/write_coordinator.rs` — `WriteCoordinator / PushTransactionState`: transaction state contract.

Trace one path: `crates/crab-write/src/generation.rs` → `while_renewing` in
`crates/crab-coordination/src/lease_operation.rs` → `PushLock::renew` in
`crates/crab-coordination/src/push_lock.rs`. The caller still releases its lease.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Lease cancellation | `crates/crab-coordination/src/lease_operation.rs` | `crates/crab-write/src/generation.rs` |
| GC fence lifecycle | `crates/crab-coordination/src/gc_fence.rs` | `crab/src/cmd/gc` |
| Active-active transitions | `crates/crab-coordination/src/write_coordinator.rs` | `crates/crab-coordination/src/active_active_runtime.rs` |

## Invariants

- while_renewing borrows the lease: await it to completion, then explicitly release. Dropping its future does neither cleanup action.
  Source: `crates/crab-coordination/src/lease_operation.rs`.
- Renewal failure signals cancellation and drains work; preserve primary operation errors instead of replacing them with renewal failure.
  Source: `crates/crab-coordination/src/lease_operation.rs`.
- Do not conflate ref leases, GC fences, and active-active transaction state; inspect the corresponding owner contract before moving authority boundaries.
  Source: `crates/crab-coordination/src/write_coordinator.rs`.

- Check both Unix expiry and monotonic renewal-deadline arithmetic. Reject unrepresentable durations before storage access; retain fresh expiry calculation on each renewal attempt.
  Source: `crates/crab-coordination/src/push_lock.rs`.

## Features and platform

Empty default. `object-store-lock` exposes lease/fence/admission code. `coordinator-dynamodb`, `coordinator-spanner`, and `coordinator-cosmosdb` enable their provider adapters. Provider SDK semantics require locked source review and dedicated backend proof; do not assume default tests exercise those services.

## Verification

Inline lease_operation tests cover lost-lease draining and error precedence; `crates/crab-coordination/src/active_active_tests.rs` covers coordinator behavior.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-coordination --locked --lib --features object-store-lock lease_operation::tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-coordination --locked --lib active_active_tests
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-coordination/README.md` — usage and detailed contracts.
- `crates/crab-coordination/Cargo.toml` — dependency and feature authority.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
