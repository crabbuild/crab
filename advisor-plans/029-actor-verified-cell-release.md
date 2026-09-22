# Plan 029: Release only an actor-verified settled Cell

> **Executor**: Start from the isolated `cell-safe-rebalance` worktree.
> Read root,
> `crates/AGENTS.md`, and the runtime's adjacent tests. Do not make the
> planner or host an authority writer.
>
> **Drift check**: `git diff --stat cebc909940f137e4bd8445e524e77a154bf51a29..HEAD -- crates/crab-cell-runtime/src/{actor.rs,coordination.rs,maintenance.rs,executor.rs,worker.rs,eviction.rs} crates/crab-cell-runtime/tests/actor.rs`.
> If any relevant function moved, inspect its callers/callees before editing.

## Status

- Priority: P0; effort: XL; risk: HIGH; category: correctness/feature.
- Depends on: plan 027's measured actor snapshot; may be developed alongside
  plan 028. Planned at `cebc909940f137e4bd8445e524e77a154bf51a29`,
  2026-09-21. Status: IN PROGRESS; implementation in this branch, acceptance gates pending.

## Why and current state

`CellRuntime::evict_idle(limit)` returns the number of drains **started**,
not released (`crates/crab-cell-runtime/src/actor.rs:585-602`).
`begin_idle_evictions` selects any eligible local Cell using
`EvictionObservation` and then calls `CoordinationInput::BeginDrain`
(`actor.rs:2293-2350`). `CellHandle::drain` closes admission before sending
its actor message (`src/actor/handle.rs:327-351`). `BeginDrain` enters a
terminal Draining lifecycle and may immediately report ReadyToDeactivate
(`src/coordination.rs:397-414`); it is not a safe speculative preflight.
`start_deactivate` closes the SQL worker before `CellPublisher::release`
CASes to Idle (`actor.rs:4010+`, `publication.rs:545+`).

`PersistedWorkInventory::is_empty` protects *maintenance contract removal*,
and counts even settled request outcomes, Blob objects, and future Cron
schedules (`src/maintenance.rs:22-115`). Queue states 0/1 are ready/leased
(`src/queue.rs:255,357`); source effects states 0/1 are due/leased
(`src/effects.rs:364,503`); active Workflow runs and activities live in
`src/workflow.rs`. Use their canonical schemas, not invented state values.
The canonical design forbids release before accepted work is durable and
requires normal authority CAS for the successor
(`docs/canonical-ltx-scaling.md:649-667`).

## Scope and contract

In scope: `actor.rs`, `actor/handle.rs` only for handle generation behavior,
`coordination.rs`, `maintenance.rs`, `executor.rs`, `worker.rs`,
`eviction.rs` only to share eligibility, focused runtime tests, and narrow
`lib.rs` exports. Out of scope: changing `CellHandle::drain` semantics,
rewriting `PersistedWorkInventory`, authority/control format, app descriptor,
HTTP, and hot SQLite migration.

Add an exact-Cell actor command accepting Cell ID, source session, and
activation generation. Return a typed `Deferred(reason)`, `Released`,
`Fenced`, or error **after** release reconciliation. A plan is only a hint:
the actor's fresh state and SQL inspection decide eligibility. Keep a
cell-local transfer preparation state separate from `BeginDrain`; only enter
the existing terminal drain after the final settled check.

## Steps and gates

1. Add transfer-specific, bounded SQL inspection through the existing
   executor/worker path. Block unknown inspection, ready or leased Queue
   messages, active Workflow/Activity work, due Cron delivery, and live or due
   source effects. Treat settled request/inbox outcomes, Blob objects, dedup
   rows, and future Cron schedules as durable state carried by the exact
   root. Keep the maintenance inventory unchanged. **Gate:** focused SQL
   tests prove each blocking and permitted row class with its real schema.
2. Add a coordination-kernel transfer preparation transition that blocks new
   Cell admission but can still finish already accepted work. It must not
   produce ReadyToDeactivate. Add confirm and abort transitions: confirm enters
   the canonical drain only after the fresh SQL/actor check; abort restores
   serving with a **new** admission generation, leaving old handles closed.
   Fencing/shutdown wins over abort. **Gate:** kernel tests cover
   accepted-before/after, abort, fence, and shutdown races.
3. Add the exact-Cell actor message. Preflight while serving; if eligible,
   prepare and close admission; await accepted queue/effects/publication and
   inspect SQL again. On a new blocker, abort as above and report Deferred.
   On success, hold the existing movement permit until
   `start_deactivate`/publisher release completes. Use the same release path
   as idle eviction; do not duplicate CAS logic. **Gate:** the response cannot
   report Released while the control is owned or a resource reservation remains.
4. Reconcile ambiguous release replies by rereading the exact control, as
   `CellPublisher::release` already does. On receiver cancellation, keep
   actor cleanup running and release the movement permit exactly once.
   **Gate:** lost-response and cancelled-waiter tests preserve one owner,
   unchanged root, and zero leaked permits.

## Test and verification commands

Add tests in `src/coordination.rs`, `src/maintenance.rs`, and
`tests/actor.rs`. Model after `persisted_work_blocks_idle_eviction_until_explicit_release`
and `released_cell_is_acquired_by_one_successor_runtime`
(`tests/actor.rs:1856,2056`). Include a visible read from a successor for
settled Blob/request/Cron data and refusal for Queue/Workflow/effect work.

Preflight `test -d "$HOME/Workspace" && test -w "$HOME/Workspace"`; create
only `$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance`. Stop if
unavailable. Set this `CARGO_TARGET_DIR` on every compiling command:

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo test -p crab-cell-runtime coordination --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo test -p crab-cell-runtime --test actor --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo test -p crab-cell-runtime maintenance --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo clippy -p crab-cell-runtime --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

All exit zero. If dependencies are missing, run `make install` in `crab/`
with the same target and retry once. Do not change test baselines or expected
failures to silence a gate.

## Acceptance criteria

- [ ] Exact-Cell transfer can release only the requested live generation;
  queued, unpublished, leased, due, unknown, or fenced work never releases.
- [ ] A speculative preflight that becomes blocked resumes service through a
  new admission generation; stale handles cannot submit work.
- [ ] Successful response follows worker close and exact-root release CAS;
  ambiguous/cancelled outcomes preserve authority and return permits.
- [ ] Maintenance inventory and ordinary shutdown/explicit drain contracts
  remain intact; focused tests, format, and Clippy pass.

## STOP and maintenance

Stop if an unsettled primitive cannot be inspected from authoritative SQL
without a full unbounded scan, if abort can reopen an old admission handle, or
if a deadline would discard acknowledged work. Reviewers should scrutinize
actor-generation checks, cancellation, and the release/permit ordering. Any
new primitive must declare transfer blockers alongside its durable schema.
