# Cell quiescing, idle eviction, and unified resource accounting

Status: IN PROGRESS — RAII ledger shared by active Cells, resident native bytes, SQL work, hydration jobs, retained publication bytes, and primitive activity/effect jobs; deterministic victim selection, actor eviction, pressure pacing, retained-byte accounting, and fail-closed persisted-work refresh are wired; cross-crate consumer registration/restart-churn proof remains
Priority: P0
Effort: XL
Risk: High
Planned against: `4a77b6f1252a` (2026-09-17)
Design authority: `crates/crab-cell-runtime/docs/canonical-ltx-scaling.md`
Dependencies: plans 008, 009, 010, and 011

## Executor instructions

Implement on `codex/012-cell-lifecycle-accounting`. Read full actor, worker,
server budget/startup/metrics, peer resource advertisement, SQL/queue/workflow/
activity/effect scheduling, and all shutdown/source-loss tests. Resource totals
must describe real owned resources, not estimates disconnected from admission.

## Drift check

```bash
git fetch origin main
git diff --stat 4a77b6f1252a..origin/main -- \
  crates/crab-cell-runtime/src/actor.rs \
  crates/crab-cell-runtime/src/worker.rs \
  crates/crab-cell-runtime/src/peer.rs \
  crates/crab-http-server/src/server.rs
```

Stop if another eviction/accounting owner was introduced or if any primitive
can create untracked work outside runtime admission.

## Why this plan exists

The runtime has static startup capacity and a simple active-Cell reservation.
`ActiveCell` does not model last use, idle/quiescing/resident state, or complete
resource cost. Hydration, directory cache, retained publication, follower logs,
queue/workflow/activity jobs, and scratch disk can consume capacity without one
admission ledger. Under churn, activation can fail while idle Cells retain
resources indefinitely.

## Canonical lifecycle

Use explicit states with kernel-owned transitions:

```text
Activating -> Serving -> Quiescing -> Idle -> Evicting -> Absent
                 |          |          |
                 +----------+----------+-> Fenced/Recovering as required
```

Names may adapt, but admission closes before drain; accepted operations reach
their durability boundary; publication/lease obligations finish or remain
recoverably retained; SQLite closes before local files/cache handles are
released; authority release follows protocol ordering.

## Resource ledger

Track and reconcile at least:

- active/resident/sparse Cell count and estimated resident memory;
- SQL workers and queued commands;
- hydration jobs and reservations;
- publication scratch and retained cuts;
- follower/node-log disk;
- directory-cache disk and index memory;
- queue/workflow/activity/effect runnable and leased jobs;
- local Cell database/scratch disk.

Every reservation has one owner, a limit, and release on all exits. Reported
totals derive from this ledger plus measured filesystem/resource probes; do not
maintain unrelated counters with different semantics.

## Implementation steps

1. Add explicit last-used and lifecycle state to the coordination kernel.
   Update last-used on meaningful admitted/served work, not background timer
   churn. New work during quiescing is rejected/rerouted deterministically.
2. Define a unified `ResourceCost`/ledger with checked arithmetic and RAII or
   explicit tokens. Replace the active-count-only reservation in `worker.rs`.
3. Register all listed consumers. Add tests that inject error/cancel/panic/fence
   at each owner and assert the ledger returns to baseline.
4. When activation lacks budget, select an eligible idle victim deterministically
   (age, state, cost; stable tie-break). Never evict busy, retained-unpublished,
   migrating, backup-pinned, or leased primitive work.
5. Drive victim through quiesce/drain/release/close/evict in the actor. Retry
   activation only after resources are actually released. Bound concurrent
   eviction and activation to avoid stampedes.
6. Reconcile startup/restart accounting from real local files and actor state.
   Fail closed or quarantine unknown owned layouts; do not report free capacity
   by forgetting them.
7. Replace node metrics/advertised resource summaries with one snapshot shape.
   Include confidence/timestamp and bounded primitive backlogs needed later by
   placement. Do not add high-cardinality labels.
8. Add a long churn test across more Cells than capacity with mixed SQL, queue,
   workflow, hydration, publication, and restart. After each acknowledged
   mutation, evict local state and restore from the exact root to prove no loss.

## Verification

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-012-lifecycle \
  cargo test -p crab-cell-runtime eviction resource --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-012-lifecycle \
  cargo test -p crab-cell-runtime --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-012-lifecycle \
  cargo test -p crab-http-server --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-012-lifecycle \
  cargo test -p crab-ltx --features replica --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-012-lifecycle \
  cargo clippy -p crab-ltx -p crab-cell-runtime -p crab-http-server \
  --all-targets --locked -- -D warnings
cargo fmt --all -- --check
node crates/crab-cell-runtime/docs/validate.mjs
git diff --check
```

The churn test must emit peak ledger values and final zero/baseline residuals;
a timeout or process exit is not proof of cleanup.

## Acceptance criteria

- [x] One kernel-owned lifecycle controls admission, quiesce, idle, eviction,
      fencing, and shutdown.
- [x] Activation pressure can reclaim eligible idle Cells without losing any
      acknowledged exact root.
- [x] Busy, unpublished, migrating, pinned, or leased work is never selected.
- [ ] Every listed resource consumer reserves and releases through one ledger.
- [ ] Advertised/metric totals reconcile with actor state and measured local
      disk within a documented tolerance.
- [ ] Restart inventory does not undercount existing owned files.
- [x] Primitive jobs participate in the same limits; activity/effect scheduler
      work and user SQL commands use bounded RAII reservations, while Queue and
      Workflow durable rows are re-inspected after work before an idle victim
      can be selected.
- [ ] Mixed churn/source-loss test passes and final resources return to baseline.
- [x] No new user configuration or second eviction path is added.

## Stop conditions

- Any resource consumer cannot be bounded or inventoried.
- Eviction needs to discard a retained publication obligation.
- Local file ownership is ambiguous at restart.
- Accounting requires blocking filesystem work on the actor loop.

## Maintenance note

Any new background job or local artifact must declare its cost and lifecycle in
the unified ledger before it can be scheduled or advertised.
