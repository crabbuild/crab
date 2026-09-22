# Plan 028: Produce deterministic bounded Cell transfer plans

> **Executor**: Use the isolated `cell-safe-rebalance` worktree and read
> root/`crates/AGENTS.md`.
> This is a pure planner change. Do not write authority or initiate movement.
>
> **Drift check**: `git diff --stat cebc909940f137e4bd8445e524e77a154bf51a29..HEAD -- crates/crab-cell-runtime/src/{placement.rs,pressure.rs,eviction.rs,node.rs}`.
> Compare the current scorer and tests before applying this plan.

## Status

- Priority: P0; effort: L; risk: MED; category: feature.
- Depends on: plan 027's signed live observations. Planned at
  `cebc909940f137e4bd8445e524e77a154bf51a29`, 2026-09-21.
- Status: IMPLEMENTED; local focused gates pass. Protected end-to-end qualification remains plan 032.

## Why and current state

`PlacementPlanner::rank/choose` scores one Cell against node headroom and
owner stickiness (`crates/crab-cell-runtime/src/placement.rs:220-318`).
Eligibility checks freshness, authenticity, drain, pressure, and nonzero
capacity (`placement.rs:318-350`), but do not account for the Cell's size or
earlier proposed moves. `MovementBudget` limits local count/rate only
(`pressure.rs:138-201`). `select_victims` is deterministic and filters
unsafe actor observations (`eviction.rs:1-82`). The canonical design says
planner output is advisory and release/acquire remains the authority protocol
(`docs/canonical-ltx-scaling.md:554-570, 649-667`).

## Scope and policy

In scope: `crates/crab-cell-runtime/src/placement.rs` and its tests;
`pressure.rs` only if one reusable pure budget type pays for itself;
`src/lib.rs` only for the narrow exported plan type. Out of scope: actor,
host, server, node wire, authority, runtime configuration, and new scheduler.
Use fixed-point integers, checked/saturating arithmetic with an explicit
fail-closed rule, stable Cell/session tie-breaks, and no float comparisons.

Define a bounded `CellDemand` (one Cell slot, memory bytes, disk bytes,
job credits, last move/residence time, source session, and accounting-known
flag), prior-snapshot/cooldown evidence supplied by the caller, and
`TransferIntent` (Cell, exact source session, destination session, snapshot
identity, projected cost, reason). The planner remains stateless: it accepts
that prior evidence as input and never infers repeated observations from one
snapshot. Supply a fixed policy value rather than adding config/env. Missing
cost or source identity excludes the Cell.
The plan must not promise receiver admission.

## Steps and gates

1. Add validation for one immutable signed fleet snapshot and bounded
   locally-owned demand list. Reject duplicate node IDs or sessions, duplicate Cell
   IDs, future/stale samples, negative time, and unknown cost. **Gate:**
   focused planner tests return an error or no intent; none treat missing data
   as free capacity.
2. Extend the existing scorer into one pure planning pass. Exclude same-node
   moves, incompatible/draining/critical/full receivers, and destinations
   whose *absolute* memory/disk/Cell/job headroom cannot admit the projected
   demand. After each selected intent, subtract its demand from the projected
   destination before scoring the next Cell. **Gate:** a destination with room
   for one of two large Cells receives at most one intent regardless of input
   permutation.
3. Sort donor candidates by drain priority, sustained pressure relief, then
   Cell ID; apply owner stickiness, minimum score gain, residence/cooldown,
   and repeated-snapshot requirement for ordinary rebalance. Explicit drain
   bypasses gain only. Bound one tick by move count and projected bytes.
   **Gate:** tests show identical output under permutations, no oscillation
   across alternating samples, and drain never bypasses eligibility or budget.
4. Export only the shape needed by the host/server executor. Document that
   every intent is stale until the actor rechecks it and destination admission
   succeeds. **Gate:** `git diff --check` exits zero and no code in this plan
   calls `CellAuthority` or a runtime movement method.

## Test and verification commands

Add table/property tests beside the existing `placement.rs` tests for
capacity monotonicity, skewed memory/disk/job Cells, duplicates, overflow,
snapshot staleness, same-source exclusion, projected exhaustion, cooldown,
and deterministic tie-breaks. Use
`crates/crab-cell-runtime/src/eviction.rs:89+` as the stable-order test pattern.

Check `$HOME/Workspace` is mounted/writable and create only this worktree's
target directory before compiling. Never fall back to local `target/`.

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo test -p crab-cell-runtime placement --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo test -p crab-cell-runtime pressure --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo clippy -p crab-cell-runtime --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Each command must exit zero. Set `CARGO_TARGET_DIR` on every compiling
command, including any retry. If dependencies are missing, use `make install`
in `crab/` with that target and retry once.

## Acceptance criteria

- [ ] For a fixed snapshot, every input permutation yields byte-identical
  ordered intents; each Cell appears at most once.
- [ ] Each intent passes projected absolute memory, disk, Cell, and job
  admission; aggregate planned demand never exceeds per-destination headroom
  or tick count/byte bounds.
- [ ] Normal rebalance obeys score gain, residence, cooldown, and repeated
  evidence; pressure/drain obey the same safety and capacity limits.
- [ ] Planner never mutates control or runs an actor; focused tests, format,
  and Clippy pass.

## STOP and maintenance

Stop if plan 027 did not provide authenticated backlog and observation age, if
per-Cell demand cannot be bounded from existing reservations/declarations, or
if a new persistent reservation/authority record seems necessary. Later
scoring changes require proof that worsening capacity cannot improve rank and
receipt-backed convergence evidence.
