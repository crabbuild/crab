# Safe Cell rebalance and scale up/down

Status: TECHNICAL DESIGN — implementation in progress; qualification pending
Base: `origin/main` at `cebc909940f137e4bd8445e524e77a154bf51a29` (2026-09-21)
Design authority: `crates/crab-cell-runtime/docs/canonical-ltx-scaling.md`,
“Balance ownership under live pressure”
Related work: plans 012–015, 022, and 023

Execution plans: 027–032 below are self-contained implementation slices.
Complete their acceptance gates in dependency order; this document remains
the cross-slice design contract.

## Goal and success criteria

Move a bounded number of active Cells toward live capacity after a node is
added, during sustained pressure, or before a node is removed. A planned move
must not release a Cell with unsettled work. Every completed move preserves the
exact published root, acknowledged results, and single authoritative owner.
Scale-down may remain blocked; it must report why and keep the source's lease
and durability owners alive until a safe release or an explicit forced-failure
path takes over.

Success is observable: adding a node causes eligible Cells to move within a
declared window; removing a node reaches zero owned Cells or returns a bounded
blocker report; the measured maximum simultaneous releases, restore bytes, and
receiver activations stay within fixed budgets. An `Idle` control after source
release is a safe intermediate state, not evidence that a receiver is serving.

## Current contract on `origin/main`

| Surface | Current behavior | Gap for this feature |
| --- | --- | --- |
| `crates/crab-cell-runtime/src/placement.rs` | Pure, deterministic rank/choose over memory, disk, active Cells, jobs, and three backlog fields; excludes stale, unauthenticated, draining, and full destinations. | Both advertisement conversion paths fill all backlog fields with zero. Ranking has no per-Cell cost, projected destination capacity, move threshold, or transfer budget. |
| `crates/crab-cell-runtime/src/node.rs` and `crates/crab-http-server/src/peer.rs` | Live session advertisements carry a signed placement block and runtime-clamped free capacity. | The signed block carries Cell/job totals but no measured backlog or explicit drain state. A zero free-capacity hint currently makes a node ineligible. |
| `crates/crab-cell-runtime/src/actor.rs`, `eviction.rs`, and `pressure.rs` | Actor-owned idle eviction checks local work state and holds node-local movement permits until deactivation completes. `evict_idle(limit)` reports drains started. | No exact-Cell, awaitable release result for a fleet plan. The actor cannot yet report each Cell's transfer readiness and measured cost to a planner. |
| `crates/crab-cell-runtime/src/maintenance.rs` | `PersistedWorkInventory` conservatively blocks idle eviction when retained request outcomes, Blob objects, Cron schedules, Queue rows, Workflow rows, or unknown state exist. | Row existence is broader than unsettled work; permanent settled data can prevent scale-down forever. This inventory remains the maintenance-release contract, not the transfer predicate. |
| `crates/crab-cell-runtime/src/publication.rs` and `control.rs` | Drained SQL worker closes before publisher release; release CASes the exact root to unowned `Idle`. Ordinary idle acquisition advances the epoch and restores that root. | No new authority protocol is needed. Fleet action must await the existing release result and reread control before claiming success. |
| `crates/crab-cell-host/src/lib.rs` | One `CellNode` owns one runtime and ordered terminal drain. `drain_until` cancels the task group and calls runtime shutdown. | Terminal shutdown is not a paced scale-down policy and does not use the idle eviction predicate. It cannot be the first step of a safe rebalance. |
| `crates/crab-http-server/src/cells/router.rs` | Cold activation prefers a live ranked peer, then uses normal authority acquisition. | There is no proactive owner movement loop or completion observation. |

`crab-cell-app` keeps stable Cell topology and declared database/capture limits.
It does not own placement, authority, or host lifecycle. Its descriptor bytes
must not change merely to add this policy.

## Ownership and proposed interfaces

Keep one scheduler, one actor, and one authority path:

| Owner | Addition | Boundary |
| --- | --- | --- |
| `crab-cell-runtime` placement | Pure `plan_transfers(snapshot, owned_cells, limits)` returning ordered advisory intents with reason and projected costs. | Never writes authority or starts work. |
| `crab-cell-runtime` actor | `placement_summaries()` and an exact-Cell `release_if_settled(cell, expected_generation)` command. | Actor closes admission, rechecks state, drives existing drain/deactivation, and returns only after release CAS or a typed refusal. No caller-supplied “settled” bool is trusted. |
| `crab-cell-host` | A provider-neutral `CellNode` movement facade and `drain_for_scale_down(deadline)` lifecycle with local progress/blocker status. | Reuses its runtime and task group; does not add a scheduler, publisher, receiver state, or authority implementation. |
| `crab-http-server` | Private fleet controller that reads signed live observations, invokes the planner for locally owned Cells, requests host moves, and uses existing authenticated peer activation. | Product owns timing, peer transport, readiness, and operator exposure. Existing cold routing remains the receiver path. |

These are proposed API shapes, not names that must become public. Keep actor
summaries crate-private unless the host and server need a narrow public type.
Do not expose raw SQL, authority, replica paths, or operation IDs through the
application handle.

## Observation and demand contract

The node publisher samples one coherent runtime ledger snapshot and signs
bounded Cell count, memory and disk headroom, job use, publication backlog,
hydration backlog, primitive backlog, pressure class, and drain status. Each
value has an explicit unit and saturation rule. Missing or stale backlog is
unknown, never optimistic zero. The planner rejects proactive movement when
the required version or measurement is absent; ordinary ownership routing and
authority recovery remain available.

Before changing the signed placement encoding, audit release tags for the
existing version. If it has not shipped, use one updated canonical shape. If
it has shipped, add a versioned signed shape with a bounded mixed-version gate;
do not add an unbounded fallback reader. Preserve the existing signature and
session-validation owner in `node.rs`.

For each locally owned Cell, the actor emits its activation generation,
published-root identity, last foreground use, active/queued job count,
reserved memory and disk, publication bytes, hydration work, primitive backlog,
and a readiness class. Hard destination demand uses current reservations or a
conservative declared bound when accounting is unknown; recent activity can
break ties but cannot weaken admission. The planner records the source session
and observation generation so stale plans are refused by the actor and
receiver. Cell IDs belong in traces, not metric labels.

## Transfer eligibility

Only the actor can make the final decision. It first checks the current Cell
without changing admission, then closes that Cell's admission and checks
again after accepted work drains. A Cell is *settled for movement* when:

1. Its source session and activation generation still match the plan, and the
   node lease is live.
2. No accepted command, actor queue item, SQL job, renewal, migration,
   hydration publication, or coordination effect can still produce a result.
3. Every acknowledged commit is object-covered or protected by the existing
   pinned recovery obligation; no unpublished node-log tail can be discarded.
4. No live Queue, Activity, or Effect lease, due scheduler delivery, pending
   Workflow transition, or unresolved primitive effect requires the source
   actor. Unknown inspection is a blocker.
5. The exact root is publishable and the ordinary release transition is valid.

Durable settled request outcomes, Blob objects, deduplication records, and
future Cron schedules travel with the exact root; their mere presence is not a
blocker. Ready Queue messages and pending Workflow work remain blockers in the
first release because the requested rule forbids moving unsettled work. A
future lease-aware handoff may relax that only with separate proof. Use a
transfer-specific SQL inspection alongside actor state; do not redefine
`PersistedWorkInventory::is_empty`, which protects maintenance contract removal.
Reinspect after closing admission to avoid a check-then-release race. If a
normal rebalance becomes blocked after closure, abort quiescence through the
coordination kernel and install a fresh admission generation before reporting
`Deferred`; stale handles remain closed. A scale-down may retain its draining
admission while internal work settles, but it must still report the blocker
and keep the source lease. Never leave a serving node with an indefinitely
closed Cell because a speculative move failed.

## Bounded planning policy

One planning tick consumes one immutable, signed fleet snapshot, bounded
prior-snapshot/cooldown evidence, and the source's actor summaries. It filters
incompatible, stale, draining, critical,
or capacity-full receivers. For each candidate, rank destinations using the
existing fixed-point placement score, then require absolute headroom for its
projected Cell, memory, disk, and job demand. Subtract planned demand from
each destination before considering the next Cell. Sort intents by drain
priority, sustained resource relief, then Cell ID to make the plan independent
of input order.

Normal rebalance needs a fixed minimum score gain after owner stickiness, a
minimum residence time, a post-move cooldown, and repeated matching snapshots.
Sustained pressure may lower the gain threshold; explicit node drain ignores
the gain but never safety, destination admission, or movement budgets. Do not
add configuration or environment variables until fixed defaults fail measured
qualification. A new node receives work through the same rule once its live
signed advertisement is eligible; no special scale-up ownership path exists.

Hold permits for concurrent source releases, per-window movement count,
projected bytes, and receiver activations until the corresponding operation
finishes. Reuse the actor's `MovementBudget` and add the missing byte/receiver
limits at the owning controller. Admission on the destination is authoritative;
planned capacity is advisory because other donors may race. A failed receiver
cannot cause a second release attempt or an unbounded replan in the same tick.

## Execution and failure protocol

```text
source actor validates plan and closes this Cell's admission
  -> drain accepted work and inspect unsettled obligations
  -> if blocked: retain owner and report reason
  -> close SQL worker and publish exact root/follower obligations
  -> existing release CAS: source owner -> unowned Idle
  -> reread control and report Released(root, revision)
  -> route one bounded authenticated activation hint
  -> receiver reserves capacity, rereads Idle, and uses ordinary takeover CAS
  -> exact-root restore succeeds before the receiver serves
```

Before release, a timeout, fence, failed publication, or unknown result leaves
the source authoritative or its recovery obligation pinned. The caller must
reread control after an ambiguous release response, as the publisher already
does. After release, receiver loss leaves an exact unowned `Idle` root. A later
eligible node can acquire it through the existing route. Never roll back by
asserting the old owner; it may reacquire only through normal authority CAS.
Planner intent and peer acknowledgement are never ownership proof.

The transfer result distinguishes `Deferred(blocker)`, `Released`,
`Activated`, `Fenced`, and `Unknown`/error. `Released` is not counted as
receiver capacity, nor as successful scale convergence. Count resource relief
only after worker close and release completion; count service restoration only
after exact-root activation and a visible read.

## Scale-down lifecycle

`CellNode::drain_for_scale_down` is distinct from terminal `drain_until`:

1. Publish a draining observation before selecting victims and stop new local
   Cell acquisition. Existing Cells may still accept the operations needed to
   settle their durable work until each individual Cell begins quiescence.
   Keep node lease, heartbeat, durability, and task group running while Cells
   remain owned.
2. Run the same bounded transfer controller over locally owned Cells. The host
   reports active, released, and blocked counts; the product controller reports
   receiver-restoring state. Retry only on a fresh observation generation and
   within the deadline/rate budget.
3. If owned Cells reach zero and durability reservations reach baseline, close
   remaining external admission, call the existing terminal `drain_until`
   once, and join facilities in their existing order.
4. At deadline with blockers, return an incomplete drain result and keep the
   source process and lease alive. Do not mark the node `Stopped` or let an
   orchestrator terminate it as a successful scale-down. An explicit forced
   stop remains a separate failure/recovery operation with its own evidence.

This requires a serving-versus-acquiring distinction in readiness: a draining
node rejects new Cells while it may still serve owned Cells to finish durable
work. Ensure the server's shutdown hook and deployment health policy consume
the incomplete result rather than treating a deadline as clean termination.

## Verification and rollout

Implement in reviewable slices: (1) measured signed observation and wire
tests; (2) pure projected planner and property tests; (3) actor exact-Cell
settlement/release result and focused primitive tests; (4) host scale-down
lifecycle; (5) server controller and multi-process qualification. Update the
canonical scaling document and operator guidance with each behavior change.

Required local tests:

- Permutation-invariant plans, stale/forged/mixed-version exclusion, projected
  destination capacity, large Cell versus count-only placement, cooldown, and
  a fixed concurrency/byte budget under simultaneous donors.
- A command accepted just before quiesce commits once; one attempted just
  after is rejected. Unknown inventory, live leases, Queue ready messages,
  pending Workflow work, unpublished publication/log tail, and migration each
  block source release. Settled durable Blob/request/Cron data may move.
- Lost release response, source fence, destination admission failure, receiver
  crash, and two receiver claims preserve one owner and the exact root.
- Scale-up of a three-node fleet moves eligible Cells; scale-down with one
  unsettled Cell reports a blocker and keeps its lease; after settlement it
  reaches zero reservations and joins all host facilities.

Use the existing tests in `crates/crab-cell-runtime/tests/actor.rs`,
`src/node/tests.rs`, `src/eviction.rs`, `src/pressure.rs`, and the host tests as
regression anchors. Run focused runtime, app, host, and server tests; format,
Clippy, dependency-tree checks for any app API change; then the existing
three-process/RustFS qualification. Every compiling Cargo command must use the
worktree-specific external target directory
`$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance` after checking the
volume is mounted and writable. Protected multi-Pod/provider/fault/scale
receipts remain a separate release gate; local Compose is not that evidence.

Do not claim completion until an operator action causes a real authority
release, a different node restores and serves the exact acknowledged state,
blocked scale-down stays live, and the measured bounds hold under concurrent
movement. No new authority record, hot SQLite copy, second scheduler, or
application-handle lifecycle API is part of this design.
