# Plan 030: Keep blocked Cells alive during host scale-down

> **Executor**: Use the isolated `cell-safe-rebalance` worktree; read root,
> `crates/AGENTS.md`,
> `crates/crab-cell-host/AGENTS.md`, and this full plan. Use the runtime's
> exact-Cell release operation; do not create a second scheduler or publisher.
>
> **Drift check**: `git diff --stat cebc909940f137e4bd8445e524e77a154bf51a29..HEAD -- crates/crab-cell-host/src/lib.rs crates/crab-cell-runtime/src/actor.rs`.
> Recheck shutdown ordering and host tests after drift.

## Status

- Priority: P0; effort: L; risk: HIGH; category: correctness/feature.
- Depends on: plan 029. Planned at
  `cebc909940f137e4bd8445e524e77a154bf51a29`, 2026-09-21.
- Status: IMPLEMENTED; local host/runtime gates pass. Protected end-to-end qualification remains plan 032.

## Why and current state

`CellNode` owns exactly one `CellRuntime` and its task group
(`crates/crab-cell-host/src/lib.rs:620-638`). Its `is_ready` currently
means `NodeState::Ready` plus a healthy group (`lib.rs:656-664`).
`drain_until` immediately sets Draining, cancels the group, drains provider
facilities, then calls `runtime.shutdown()` (`lib.rs:1024-1129`).
Runtime shutdown marks every Cell for terminal drain
(`crates/crab-cell-runtime/src/actor.rs:1879-1927`); it does not select only
settled Cells. A scale-down that calls this first can remove the source's
heartbeat or work processor before unsettled obligations finish. The canonical
design requires stop-acquiring, paced releases, blocker counts, and a source
that stays alive on incomplete drain
(`docs/canonical-ltx-scaling.md:668-674`).

## Scope and contract

In scope: `crates/crab-cell-host/src/lib.rs` and its tests; a narrow runtime
readiness/summary accessor only if plan 029 did not supply it; host docs.
Out of scope: HTTP/auth, provider construction, `crab-cell-app` descriptor,
authority CAS, direct SQL access, changing normal `drain_until` semantics.

Add a separate idempotent, deadline-aware scale-down operation and status
report. It marks the node unavailable for *new Cell acquisition*, not for
operations that settle already owned Cells. Its local report includes bounded
counts for active, released, and blocked Cells with low-cardinality reason
classes; the server controller owns receiver-restoring status. Never use Cell
IDs as metric labels. Keep the lease, heartbeat, durability, and task group
until all owned Cells are safely released. Existing terminal shutdown runs
only after zero owned Cells and baseline reservations are observed.

## Steps and gates

1. Model `Ready -> Draining -> Stopped` for planned scale-down while
   preserving an explicit “serves owned Cells” predicate separate from
   “accepts new Cells.” Do not change normal shutdown readiness by accident.
   **Gate:** host tests show a draining node refuses new acquisition but an
   owned Cell can finish its existing durable work.
2. Serialize concurrent scale-down and shutdown callers with the current
   `shutdown_lock`. Call the runtime's exact-Cell settled-release operation
   in bounded batches, respecting its permits and deadline. Do not duplicate
   SQL inspection in the host. **Gate:** two concurrent callers observe one
   lifecycle and no duplicate release; a blocked Cell remains owned.
3. On an incomplete deadline, return a typed blocker report, retain live
   facilities and node lease, and leave `NodeState::Draining`. Do not call
   `runtime.shutdown()` or mark Stopped. After the blocker settles, a later
   call may resume from that state. **Gate:** before/after deadline tests
   inspect task-group health, authority owner, and unchanged root.
4. At zero owned Cells and zero pending durability reservations, invoke the
   existing `drain_until` once and preserve reverse facility ordering,
   deadline handling, and idempotence. **Gate:** existing shutdown tests plus
   a scale-down completion test show Stopped and baseline reservations.

## Test and verification commands

Extend host tests in `src/lib.rs`; follow
`node_cancels_admission_before_draining_provider_facilities` and
`concurrent_shutdown_waits_for_the_single_runtime_drain`
(`lib.rs:1850,2036`). Test blocked Queue/Workflow work, clean Cell, timeout,
resume, facility failure, and concurrent callers.

Preflight the mounted/writable `$HOME/Workspace` and create only the
per-worktree target before compiling. Never use local `target/`.

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo test -p crab-cell-host --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo test -p crab-cell-runtime --test actor --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo clippy -p crab-cell-host -p crab-cell-runtime --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Each exits zero. Missing dependencies: `make install` from `crab/` with
the same `CARGO_TARGET_DIR`, retry once, then report the first failure.

## Acceptance criteria

- [ ] Planned scale-down starts no new Cell acquisition and keeps unsettled
  owned Cells, their lease, and work facilities live.
- [ ] Deadline reports blockers without claiming a clean stop or zero
  reservations; a later call can finish after settlement.
- [ ] Successful scale-down reaches zero owned Cells, joins facilities in
  existing order, and returns baseline resource reservations.
- [ ] Concurrent calls are idempotent; ordinary shutdown behavior and all
  focused tests/format/Clippy remain green.

## STOP and maintenance

Stop if serving already owned Cells requires bypassing authorization, if the
host cannot distinguish acquisition from serving readiness, or if a blocked
deadline would cause the deployment layer to kill the process automatically.
The server/deployment response is the next plan; do not silently declare
scale-down operational before that integration exists.
