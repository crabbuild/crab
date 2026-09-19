# Cell quiescing, idle eviction, and unified resource accounting

Status: IN PROGRESS — RAII ledger shared by active Cells, resident native bytes, active-Cell file descriptors, SQL work, hydration jobs, retained publication bytes, primitive activity/effect/migration/recovery jobs, canonical LTX `DiskBudget`, and embedded-host I/O/blocking/recovery/dirty/scratch admissions; deterministic victim selection, actor eviction, pressure pacing, retained-byte accounting, and fail-closed persisted-work refresh are wired; unknown persisted-work inventory is explicitly ineligible for eviction; descriptor admission/metrics and shared local-disk consumer wiring are now explicit; bounded two-slot/three-Cell churn, mixed Queue/Workflow inventory churn, and retained-work eviction guard tests prove canonical capacity reuse and fail-closed obligation handling; runtime Prometheus gauges now expose every ledger class and the HTTP projection consumes one canonical `CellRuntimeStats` mapping; stale session restart inventory now reserves every regular file outside the fresh process session and rejects ambiguous layouts before serving; outer peer and node-log codec work now shares the primitive-job ledger; the Compose qualification harness now asserts runtime disk/active-Cell gauge parity with the capacity report; measured local-disk tolerance and provider-scale mixed-workload proof remain
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

- active/resident/sparse Cell count, estimated resident memory, and per-Cell
  file descriptors;
- SQL workers and queued commands;
- hydration jobs and reservations;
- publication scratch and retained cuts;
- follower/node-log disk;
- directory-cache disk and index memory;
- queue/workflow/activity/effect runnable and leased jobs;
- local Cell database/scratch disk.

The canonical `crab_ltx::DiskBudget` is installed as a runtime-owned admission
on `CellRuntime` startup. It imports existing reservations, reconciles every
reserve/resize/release, and reports the same disk total through runtime stats;
the hook is weakly held so a dropped runtime is removed on the next admission.

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

The first local churn proof is intentionally staged: the actor test uses three
independent repository fixtures with a two-Cell pool, waits for fail-closed
persisted-work inventory, evicts one idle Cell, reacquires its exact idle root
through `CellRuntime::acquire_idle_restored`, and bootstraps the third Cell
after capacity is released. Bootstrap roots contain no retained request rows,
so this test proves the lifecycle/capacity path without deleting durable
contracts behind the runtime's back. Mutation-root restoration is covered by
the existing command, publication, cancellation, and source-loss tests; the
mixed primitive/source-loss/restart churn gate below remains open until one
qualification test exercises those workloads together.

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

Local actor proof:

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-main \
  cargo test -p crab-cell-runtime --test actor \
  churn_evicts_idle_cells_and_restores_exact_roots --locked -- --test-threads=1
```

This test is a local lifecycle/capacity proof only. It asserts that one of two
active Cells reaches `Idle` with the captured root unchanged, that the same
runtime reacquires and reads that root, that a third Cell can then bootstrap,
and that all active reservations return to zero before shutdown. It does not
claim provider, process-restart, mixed-primitive, multi-GiB, or measured RSS
qualification.

The companion `persisted_work_blocks_idle_eviction_until_explicit_release`
test executes a durable mutation, waits for the background inventory refresh,
and proves `evict_idle` still returns no victim while the request outcome is
retained. It then uses the explicit drain/release path and verifies the active
Cell reservation returns to zero. This protects the fail-closed rule without
mutating the database behind the actor.

## Acceptance criteria

- [x] One kernel-owned lifecycle controls admission, quiesce, idle, eviction,
      fencing, and shutdown.
- [x] Activation pressure can reclaim eligible idle Cells without losing any
      acknowledged exact root.
- [x] Busy, unpublished, migrating, pinned, or leased work is never selected.
- [x] Every listed resource consumer reserves and releases through one ledger.
- [x] Canonical LTX local-disk reservations reconcile with the runtime ledger
      without allowing a failed admission to leak bytes.
- [ ] Advertised/metric totals reconcile with actor state and measured local
      disk within a documented tolerance; the local actor-to-metric projection
      is covered by `runtime_snapshot_projects_live_cell_ledger`, while disk
      tolerance still requires provider-scale measurement.
- [x] Restart inventory does not undercount existing owned files.
- [x] Primitive jobs participate in the same limits; activity/effect scheduler
      work, release migrations, node-log recovery, and user SQL commands use
      bounded RAII reservations, while Queue and Workflow durable rows are
      re-inspected after work before an idle victim can be selected.
- [x] Mixed Queue/Workflow churn proves persisted primitive rows block unsafe
      eviction, canonical drain releases both Cells, exact idle-root restore
      preserves the Queue row, and a third Cell reuses the released capacity;
      the companion source-loss takeover test preserves the same exact-root
      and publication contract, with final active reservations returning to
      baseline.
- [x] No new user configuration or second eviction path is added.

## Stop conditions

- Any resource consumer cannot be bounded or inventoried.
- Eviction needs to discard a retained publication obligation.
- Local file ownership is ambiguous at restart.
- Accounting requires blocking filesystem work on the actor loop.

## Maintenance note

Any new background job or local artifact must declare its cost and lifecycle in
the unified ledger before it can be scheduled or advertised.

Scheduler migration and node-log recovery now hold a `NodeJobReservation` for
the complete spawned task, in addition to their per-cell/session duplicate
guards. This prevents maintenance work from consuming unadvertised primitive
capacity while the actor and effect paths are under load. The embedded canonical
LTX host now obtains one runtime admission token for each bounded object-store
I/O operation, blocking host job, recovery cohort, dirty-memory cohort, and
scratch MiB. Tokens follow cancellation-safe work until completion, while the
existing LTX semaphores remain the local waiters. The remaining ledger gates are
complete advertised-metric parity, advertised-placement parity, and measured
mixed-workload proof; those require
qualification rather than another local counter. The HTTP projection
regression installs a runtime-owned `DiskBudget`, reserves a nonzero disk
amount through its admission hook, and checks that the rendered reserved-byte
gauge matches `CellRuntimeStats`; this proves the disk field mapping with live
ledger state while provider-scale tolerance remains a separate gate. HTTP
Prometheus
metrics now export usage and capacity for every host-ledger class, but the
placement advertisement still publishes its narrower job-credit contract until
qualification proves a compatible expanded observation shape.

The HTTP peer boundary now reserves one primitive-job token for the bounded
authenticated request verification and reply protobuf encoding sections. The
node-log append and tail codecs use the same helper; SQL wire codecs remain
inside their already-admitted worker jobs. Admission refusal fails closed with
`503` before the codec executes, and the RAII token is dropped on every return
path, so this closes the process-wide transport-codec gap without adding a
second semaphore, capacity setting, or accounting surface.

Placement disk headroom and the signed placement total now take the minimum of
the measured filesystem capacity and the runtime-owned `DiskBudget` capacity
before subtracting ledger reservations. A host filesystem can be larger than
the runtime admission budget; publishing either the larger total or its
unclamped headroom would let a planner promise work that the destination must
reject. `peer::tests::placement_capacity_respects_runtime_reservations` and
the local publisher regression cover this projection with a deliberately larger
filesystem probe, while the provider-scale disk-tolerance receipt remains a
plan-015 qualification gate.

On server restart, `LocalStaging::new_with_restart_inventory` walks the
dedicated `cells/sessions` namespace before runtime startup. The newly created
session is excluded because its active database, WAL, cache, and transfer
reservations are established by their owning handles; every prior session is
treated as retained/quarantined local state and its regular-file bytes are
reserved on the same `DiskBudget`. Symlinks, special files, non-directory
session entries, path escapes, and capacity overflow fail closed. The
reservation is held by the server's staging owner, so the runtime's disk
admission and Prometheus totals include the inventory before any Cell can be
activated. Focused tests cover nested cache files, current-session exclusion,
symlink rejection, and capacity rejection; follower storage and directory
cache constructors continue to reconcile their own durable namespaces through
the same budget.

The actor churn test is the canonical local regression for this maintenance
path: it uses the same runtime admission, actor eviction, authority release,
and idle reacquisition APIs that production routing uses. A test that removes
`sys_requests` or bypasses authority would not be equivalent evidence and must
not be substituted for the remaining mixed-workload gate.
