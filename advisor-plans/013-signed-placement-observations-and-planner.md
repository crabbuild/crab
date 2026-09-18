# Signed live placement observations and pure weighted planner

Status: IN PROGRESS — versioned signed placement observations now carry measured memory/disk totals and one coherent runtime Cell/job snapshot; advertised free headroom is clamped by unified runtime reservations and fail-closed nested Linux cgroup probes; cold activation now uses a bounded authenticated activation hint; process-wide probe parity and multi-process convergence remain the only placement release gates
Priority: P0
Effort: XL
Risk: High
Planned against: `4a77b6f1252a` (2026-09-17)
Design authority: `crates/crab-cell-runtime/docs/canonical-ltx-scaling.md`
Dependencies: plans 005 and 012

## Executor instructions

Implement on `codex/013-cell-placement-planner`. Read node advertisement/signing,
peer heartbeat/membership, scheduler, router, runtime resource ledger, server
startup, and wire compatibility tests. Keep observation, planning, and action
separate. This plan may rank/route new ownership but must not implement forced
movement or authority CAS execution.

## Drift check

```bash
git fetch origin main
git diff --stat 4a77b6f1252a..origin/main -- \
  crates/crab-cell-runtime/src/node.rs \
  crates/crab-cell-runtime/src/peer.rs \
  crates/crab-cell-runtime/src/scheduler.rs \
  crates/crab-http-server/src/cells/router.rs \
  crates/crab-http-server/src/server.rs
```

Stop if node advertisement compatibility or signing ownership changed without a
documented migration.

## Why this plan exists

Current ownership is demand-driven with static startup capacity and local
admission; scheduler rendezvous assigns scan responsibility, not weighted Cell
ownership. Node advertisements expose limited free memory/disk/job fields. A
production fleet needs authenticated live observations and a deterministic
planner that considers headroom, pressure, locality, stickiness, and primitive
backlog without creating an unsafe second authority system.

## Required layers

1. **Observation:** server probes cgroup/container/host limits and runtime ledger.
2. **Signed advertisement:** versioned, timestamped/session-bound snapshot.
3. **Pure planner:** snapshot + Cell demand -> ranked desired owners and reason.
4. **Routing use:** prefer a live eligible node for cold activation; authority
   CAS and actor admission remain the only ownership mutation.

The planner never writes control, assumes ownership, or overrides fencing.

## Placement inputs

Include bounded normalized inputs for memory/disk headroom, active/resident
Cells, worker/job utilization, publication/hydration backlog, primitive backlog,
pressure state, draining state, cache/locality signal, and observation age.
Missing/stale values reduce eligibility or confidence; they must not become
optimistic zero load.

## Implementation steps

1. Extend the signed node advertisement with a versioned placement observation.
   Preserve decoding of currently live versions only under an explicit mixed-
   version rule. Unknown future versions fail safe for ownership placement.
2. Add platform-neutral observation traits plus Linux cgroup v2 and host
   fallbacks. Resolve the process's nested cgroup membership from the kernel
   proc files before checking root fallbacks. Unit-test fixture files for
   unlimited, nested, malformed, and changing limits. Do not add a
   cloud-provider dependency.
3. Derive runtime values from plan 012's canonical ledger. Clamp, validate, and
   timestamp the snapshot; sign it with the existing session identity.
4. Implement a pure planner returning ranked candidates, score components,
   eligibility reason, and stable tie-break. Required policies: hard exclusion
   for stale/draining/critical pressure; headroom weighting; locality bonus;
   current-owner stickiness/hysteresis input; maximum per-node/tenant share.
5. Add property/table tests for determinism, permutation invariance, monotonic
   response to worsening capacity, stale exclusion, mixed versions, and no
   candidate behavior. Never use floating NaN-sensitive comparison; use checked
   integer/fixed-point scoring or an explicit total order.
6. Use the planner only for cold/absent activation routing. Revalidate signed
   liveness and let destination admission/authority CAS decide. If forwarding
   fails, refresh observations and replan within a bounded attempt budget; do
   not silently force local activation.
7. Add bounded metrics for score reasons and placement result. Avoid node/Cell
   IDs as metric labels; traces may carry structured IDs under existing policy.
8. Add a mixed-version three-node integration test: old node remains routable
   under the documented rule but cannot receive features it does not advertise;
   stale/forged observations are rejected.

## Verification

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-013-placement \
  cargo test -p crab-cell-runtime scheduler --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-013-placement \
  cargo test -p crab-cell-runtime peer --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-013-placement \
  cargo test -p crab-http-server --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-013-placement \
  cargo clippy -p crab-cell-runtime -p crab-http-server \
  --all-targets --locked -- -D warnings
cargo fmt --all -- --check
node crates/crab-cell-runtime/docs/validate.mjs
git diff --check
```

## Acceptance criteria

- [x] Placement observations are versioned, session-bound, signed, timestamped,
      and rejected when forged or stale.
- [x] Runtime values come from the unified ledger and measured cgroup/host
      limits, with fail-safe missing-value semantics.
- [x] The planner is pure, deterministic, permutation invariant, and uses a
      total ordering with stable tie-breaks.
- [x] Stale, draining, or critically pressured nodes are ineligible.
- [x] Cold activation follows the preferred live eligible node through an
      authenticated activation hint; the destination still requires normal
      actor admission and authoritative CAS.
- [x] No planner code writes ownership/control or moves an active Cell.
- [x] Mixed-version behavior is explicit and tested.
- [x] Existing scheduler scan assignment and peer signature tests pass.

Local provenance proof: `peer::tests::node_publisher_creates_one_local_session_and_publishes_before_serving`
holds a runtime ledger byte and primitive-job reservation while publishing one
advertisement, then checks the signed memory/disk totals, active-Cell/job
snapshot, and clamped free headroom against that same runtime and measured
`LocalResources` sample. `local_resources_include_process_file_capacity` plus
the nested cgroup fixture tests cover the host/cgroup probe boundary and its
fail-closed parsing rules. The remaining process-wide probe parity and
multi-process convergence evidence belongs to plan 015.

## Stop conditions

- Wire compatibility cannot be expressed without an indefinite alias/fallback.
- Required observation data cannot be authenticated with the current session.
- Planner output would be treated as authority.
- Retry could activate on multiple nodes without normal CAS fencing.

## Maintenance note

New score inputs require monotonicity and missing/stale-value tests. Score
changes are protocol policy and need simulator/qualification evidence, not just
unit snapshots.
