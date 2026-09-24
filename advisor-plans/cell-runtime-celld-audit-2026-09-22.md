# Cell runtime audit: celld comparison and primitive production readiness

Date: 2026-09-22
Audited against: `a3edf0b1317` plus the ownership-balance change in this worktree
Celld reference: `10cb1303dac710dcb3b557e318e08c855261f68b` (v0.4.1); upstream
`main` differs from the pinned revision only in `crates/logic/rebalance.rs`,
where `receives_from` adds a `reports_adoption_capacity()` predicate

Verdict: **not a production replacement yet for all primitives.** Every one of
the eight primitives is Level 3 locally (user action through the public host to
a real durable side effect), with a shared 8-primitive x 7-lifecycle
qualification matrix. Two structural prerequisites and a named list of
primitive gaps stand between that and Level 4/5: protected provider/Kubernetes
evidence, and the unwired policy seams listed in "Prioritized opportunities".

## Scope and method

Read for this audit:

| Source | What was read |
| --- | --- |
| Celld `crates/logic` (12,514 lines) | `rebalance.rs`, `pressure.rs`, `drain.rs`, `cron.rs`, `kv.rs`, `queue.rs`, `format.rs`, `wake.rs`, `log_evict.rs`, `log_tier.rs`, `output_gate.rs`, `gate.rs`, `cache.rs`, `restore.rs`, `routing.rs`, `schedule.rs`, `alarm.rs`, `sweep.rs`, `cell.rs`, `peer.rs`, `dead_node_reconciliation.rs`, plus the `surplus`/`receivers` executor in `crates/celld/{main,actor}.rs` |
| Crab runtime | `crates/crab-cell-runtime/src/{placement,node,actor,resource,pressure,eviction,release,maintenance,scheduler,kv,blob,queue,cron,workflow,effects,sql,activity_pool,telemetry,qualification}.rs` and the sibling modules they name |
| Crab composition | `crates/crab-http-server/src/{peer,cells/router,cells/scheduler,metrics}.rs`, `crates/crab-cell-host/src/lib.rs`, `crates/crab-cell-app/src/lib.rs` |
| Crab evidence | `crates/crab-cell-runtime/tests/*`, `crates/crab-cell-runtime/qualification/profiles/*`, `crates/crab-http-server/tests/public_cell_*`, `crates/crab-cell-runtime/model/*` |
| Crab docs | `crates/crab-cell-runtime/docs/*` (7,600 lines) |

Not verified here: protected provider, multi-process, and Kubernetes runs.
Nothing in this document should be read as evidence for those gates.

## Evidence map

| Surface | Current owner | Audit result |
| --- | --- | --- |
| Ownership balancing | `crates/crab-cell-runtime/src/placement.rs`, `crates/crab-http-server/src/cells/router.rs` | Weighted-count balancing implemented in this change beside the headroom-gain path; measured per-Cell cost weights and fleet-wide movement accounting remain open. |
| Pressure policy | `crates/crab-cell-runtime/src/pressure.rs` | Classifier, hysteresis, and movement budget are implemented and tested, but no production caller feeds a sample; advertisement pressure is derived from `free == 0`. |
| Placement inputs | `placement.rs`, `crates/crab-http-server/src/peer.rs` | Signed capacity/backlog block is complete; `locality_bonus` is never populated and `PlacementPressure::{Constrained,Shedding}` is unreachable from a signed observation. |
| Primitives | `src/{sql,kv,blob,queue,cron,workflow,effects}.rs`, `src/activity_pool.rs` | All eight execute through one actor, ledger, and publication path with bounded limits; per-primitive gaps listed below. |
| Durable-work safety | `src/maintenance.rs`, `src/eviction.rs` | Conservative inventory blocks movement of live/leased/due work; stronger than the celld count rule alone. |
| Node lifecycle | `crates/crab-cell-host/src/lib.rs`, `src/release_progress.rs` | Scale-down reports released/blocked/remaining and keeps the node serving; no fleet-wide donor serialization. |
| Observability | `crates/crab-http-server/src/metrics.rs`, `src/telemetry.rs` | Rich durability/LTX/node-log metrics; no per-primitive operation, error, or latency metrics. |
| App surface | `crates/crab-cell-app/src/lib.rs`, `docs/application-framework.md` | Deterministic author compilation and a hand-written all-primitive reference application; code generation and full operator ownership open. |
| Release gate | `qualification/profiles/*`, `src/qualification.rs`, `src/cluster_qualification.rs` | Nine protected profiles, signed receipts, and a fresh-process bundle verifier wired into release CI; protected runs remain outstanding. |

## Part 1 — Celld `crates/logic` review

Celld's logic crate is a **pure policy layer**: every module is "reified
sans-IO", returning decisions or intents that an executor performs. The
production binary and a deterministic simulator drive the same functions. That
single decision is the source of most of its quality, and it is the main thing
worth copying.

Practices worth adopting, with the evidence that makes each credible:

1. **Policy/executor split with one implementation of each rule.** `gate.rs`
   ("the input gate, reified sans-IO"), `output_gate.rs` ("one choke point every
   egress passes through"), `routing.rs`, `schedule.rs`, `alarm.rs`, `cache.rs`,
   `restore.rs`, and `drain.rs` all state the same contract: pure decision, one
   executor, no clock and no I/O in the policy.
2. **Evidence-anchored constants.** `kv.rs` records the measured crossover that
   sets the inline-value bound ("the inline path is faster at 1 MiB and the
   bucket path is faster at 2 MiB"); `restore.rs` records a measurement
   ("46, 0 local reuses in 910 activations"); `cron.rs` proves its 400-year
   lookahead bound (146,097 days) and names two expressions that broke the
   smaller estimates; `rebalance.rs` cites the 2026-09-04 fleet where a batch of
   21 timed out behind queued activations.
3. **Data-driven limits from one source of truth.** `kv.rs` ships the bounds as
   data (`__cell.kvLimits`) so the JavaScript harness and Rust compare against
   the same numbers, and its comment records why the Rust copy was deleted: a
   dead-code gate called `twin_gate` found a rule that was "tested by the DST,
   run by nobody -- production has its own copy".
4. **A structural rollout gate.** `format.rs` publishes the newest bucket format
   a node reads and refuses to write a format that any live lease cannot read.
   This is a fleet-version gate expressed as a write precondition, not a
   convention.
5. **Separation of concerns between counting and pressure.** `pressure.rs` is
   explicit that residency is *not* a pressure resource: "A node's cell count is
   a hard cap enforced at admission ... self-limiting and known exactly; it is
   not a resource that needs a proactive walk down", and it records that
   "conflating the two produced the placement churn and the admission wedge".
   `rebalance.rs` is correspondingly count-only: weighted target per member,
   densest member donates, two-percent receiver deadband, bounded batch, room
   bound, node-id tie-break so one snapshot elects one donor, and a `since_ms`
   guard so pre-batch and post-batch counts never mix.
6. **Drain serialization with a readiness gate.** `drain.rs` serializes donors
   through one bucket token, snapshots every live node's restoration level, and
   refuses to let a joining replacement be considered ready while the fleet is
   unsettled (memory headroom, restore backlog, ownership skew).
7. **A slow-member policy.** `log_evict.rs` evicts a follower whose windowed
   append-latency tail exceeds `max(absolute budget, k x sibling median)`, with
   a hard backstop for a single outstanding append, rate-capped
   reconfigurations, and a separate rule for members that cannot serve at all.

Celld's own limits, stated in its source:

- `log_tier.rs` is "design stage; not yet wired into the engine".
- Cells are weighted by node capacity but counted uniformly; celld does not
  weight by measured per-Cell CPU or memory. Crab's design doc already names
  this as the bar to exceed.
- Configuration is environment-heavy (`CELLD_REBALANCE_INTERVAL_MS`,
  `CELLD_REBALANCE_BATCH_CELLS`, and others). Crab's narrower config surface is
  a deliberate difference, not a gap.
- The isolate/HTTP/WebSocket modules (`isolate.rs`, `http.rs`, `js.rs` in the
  binary) are Cloudflare-Workers compatibility surfaces. Crab's non-goals
  explicitly exclude that host model; nothing here should be copied.

## Part 2 — Crab primitive readiness

Levels use the repository's own feature-validation table
(`AGENTS.md`, "Feature Validation"). "Local L3" means user action through the
public host to a real durable side effect, proven by the tests cited; it does
not mean protected provider evidence.

| Primitive | Implemented surface | Local evidence | Level | Blocking gaps |
| --- | --- | --- | --- | --- |
| SQL | Typed `SqlCell`, bounded batches, read-only query path, request ledger, authorizer | `tests/sql.rs`, `tests/actor.rs`, `tests/migration.rs`, `public_cell_qualification.rs` | 3 (4 partial) | No cross-Cell transactions by design; batch is the only transaction boundary; fixed 5 s wall deadline |
| KV | Atomic checks/mutations, version CAS, get, prefix list, TTL, cleanup | `tests/kv.rs`, `public_cell_retry.rs`, `public_cell_lease_expiry.rs` | 3 | No metadata, no bulk get; 4 MiB values |
| Blob | Multipart, part digests, ETag CAS, ranges, per-shard list, expired-upload cleanup | `tests/blob_cron.rs`, `public_cell_process_fault.rs`, `public_cell_lease_expiry.rs` | 3 | No product collector for unreferenced parts; helper alone scans unbounded listings |
| Queue | Producer-deduped send, claim leases, ack/retry/extend, pause/resume/purge/redrive, info, dead-letter effects | `tests/queue.rs`, `public_cell_retry.rs`, `public_cell_lease_expiry.rs` | 3 | No batch send; no consumer batch/concurrency configuration; 20 attempts; no FIFO |
| Cron | Interval schedules, pause/resume/delete, durable effect delivery, bounded catch-up | `tests/blob_cron.rs`, `public_cell_qualification.rs`, `tests/scheduler.rs` | 3 | Interval-only: no cron expressions, no timezone/DST semantics |
| Workflow | Compiled definitions with digest pinning, timers, signals, pause/resume/restart/cancel, 100k events | `tests/workflow.rs`, `tests/workflow_api.rs`, `public_cell_activity_*` | 3 | No child workflows, no continue-as-new, no patching beyond digest pinning |
| Activity | Blocking pool (<=16 workers), supervised async runner, lease heartbeat, panic-to-failure | `tests/actor.rs`, `public_cell_activity_cancellation.rs`, `public_cell_activity_retry.rs` | 3 | No per-module fairness or quotas; 256 KiB result payload |
| Effects | Typed cross-Cell commands, source ledger, leases, retries, ack, validate, inbox dedup, resolve, status | `tests/actor.rs`, `public_cell_effect_delivery_cancellation.rs`, `public_cell_effect_delivery_expiry.rs` | 3 | At-least-once by design; no ordering; no destination-side admission or quota |

Shared properties that already meet a production bar: one actor and one
durability path for every primitive; exact-root or write-all-follower release;
request and inbox deduplication with durable outcomes; bounded encoders and
declared limits per operation; a conservative durable-work inventory that
blocks movement of live, leased, or due work; and 8 primitives x 7 lifecycle
cases (happy, retry, duplicate, expiry, cancellation, owner loss, recovery)
enforced by coverage bits in signed receipts.

## Part 3 — Prioritized opportunities

### P0

1. **Wire or delete the pressure seam.** `crates/crab-cell-runtime/src/pressure.rs`
   and `CellRuntime::observe_pressure` have no production caller; the signed
   observation derives pressure from `free_memory_bytes == 0 ||
   free_disk_bytes == 0` (`placement.rs:96-106`), so `Constrained` and
   `Shedding` never reach the planner from a peer. Effect: the soft-pressure
   row of the hysteresis table in `docs/canonical-ltx-scaling.md` is not in
   force; a merely hot node cannot shed until its ledger is empty.
   Acceptance: a node sample feeds the classifier on the heartbeat, the tier is
   signed into the placement block, and shedding starts a bounded eviction.
2. **Wire or explicitly defer the Blob collector.** `BlobArtifactStore::sweep_unreferenced`
   (`crates/crab-cell-runtime/src/blob.rs`) is exercised only by tests; the
   production `sweep_unreferenced` call at `crab/src/cmd/metadb.rs:3015` is the
   object-catalog writer, not this one. `docs/primitives.md` states the helper
   "is not wired to a product collector yet" and delegates abandoned parts to
   the provider lifecycle policy. Acceptance: either a quiesced, grace-bound
   collector command with receipts, or a documented provider-lifecycle contract
   with a verification test.
3. **Protected provider and multi-process proof.** Plans 015/023/032 and the
   release bundle gate expect `local-provider-v1`, `scale-v1`,
   `compatibility-v1`, `provider-{s3,gcs,azure}-v1`, and
   `fault-{s3,gcs,azure}-v1` receipts tied to an exact source and image. This is
   the difference between Level 3 and Level 5 for every primitive row above.

### P1

4. **Cron expressions.** The primitive is interval-only
   (`CronMutation::Upsert { interval_ms, next_due_ms }`). Every application
   expectation for "cron" is an expression plus a zone. Celld's `cron.rs` shows
   a bounded design: minute resolution, UTC, a proven lookahead bound, and a
   documented dialect. Either adopt that shape or make interval-only an
   explicit contract in `docs/primitives.md`.
5. **Fleet-wide drain serialization.** `CellNode::drain_for_scale_down` is
   per node; nothing bounds concurrent donors across the fleet or gates a
   replacement's readiness on restoration and skew the way celld's `drain.rs`
   does. Acceptance: either an operator contract with a test that two
   simultaneous drains stay bounded, or a token with the same
   claim/TTL/readiness semantics.
6. **Slow-member policy for the fleet proof.** The node-log gate waits for
   every member (`node_log_shipper` test
   `slow_follower_delays_fleet_proof_until_every_member_acknowledges`). The
   default release races object proof against fleet proof, but the fleet-only
   path has no latency-tail eviction or hard backstop. Celld's `log_evict.rs`
   is the reference; crab additionally needs a metric for blocked-proof time.
7. **Per-primitive observability.** The metric inventory is rich for
   durability, LTX, node log, and resident routes, and empty for the
   primitives: no operation counts, error classes, or latency histograms for
   SQL, KV, Blob, Queue, Cron, Workflow, Activity, or Effects. The
   `CellTelemetry` seam is the right owner; keep labels bounded by module and
   operation.
8. **Queue batch send and consumer shape.** One message per send command means
   one ledger row and one transaction per message. A bounded batch send (and a
   documented consumer batch/concurrency contract) is the cheapest throughput
   win for queue-shaped workloads. Workflow has the matching gap: no
   continue-as-new for long-lived runs, no child workflows.

### P2

9. **Delete or wire the dead placement inputs.** `locality_bonus` is always
   zero in production (`PlacementObservation::from_signed_capacity`) and only
   tests set it; the score term it feeds is inert. Either give it a real
   producer or remove the field and its weight.
10. **Measured per-Cell cost.** Movement projection uses declared constants
    (`ACTIVE_CELL_NATIVE_BYTES + ACTIVE_CELL_PAGE_CACHE_BYTES` for memory,
    `repository_replica_limits().max_plan_bytes` for disk). Both crab and celld
    still weight Cells uniformly; the design doc's exit criteria require
    beating count-only placement with measured cost.
11. **Deterministic simulation parity for primitives.** The coordination kernel
    has a simulator and a TLA+ model with broken variants; primitives have
    integration tests but no replayable schedule. Celld's whole policy layer is
    driven this way, and it is what lets it find "tested but never run" rules.
12. **Adopt a `twin_gate`-style check.** Items 1 and 9 were found by reading
    callers, not by CI. A check that flags public policy entry points with no
    production caller (with an explicit allowlist) would have caught both.

## Part 4 — Ownership balancing landed with this audit

The celld behaviour the request names is now implemented on crab's own
interfaces, without adopting celld's schema or its configuration surface:

- `PlacementPlanner::fleet_balance` (`crates/crab-cell-runtime/src/placement.rs`)
  computes a weighted target per member from declared cell capacity
  (`max_active_cells`), elects exactly one donor (densest by Cells per unit of
  weight, node id then session as tie-breaks), bounds the batch by the donor's
  surplus, the receivers' room below a two-percent deadband, and the existing
  two-Cell tick cap, and returns receivers below their own target, least dense
  first. The view fails closed: unauthenticated, stale, duplicated, or
  pre-batch samples (`since_ms`) yield no plan, and members that are draining,
  shedding, or otherwise ineligible cannot receive.
- `PlacementPlanner::plan_transfers` takes the balance as an explicit input. A
  balancing move is admitted without the headroom-gain gate but keeps every
  other gate (settlement, two stable samples, 60-second residence and cooldown,
  receiver eligibility, and projected memory, disk, slot, and job capacity),
  and each accepted balancing intent consumes the snapshot's donation budget.
  Drain, shedding, and material headroom-gain movement are unchanged.
- `RepositoryCellRouter::rebalance_once_at`
  (`crates/crab-http-server/src/cells/router.rs`) requires a complete live view
  before balancing, records the instant it dispatches a batch, and refuses to
  plan from samples that predate it. Drains and headroom relief keep their own
  per-node freshness checks.

Proof run in this worktree (`CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-7c96`):

| Command | Result |
| --- | --- |
| `cargo test -p crab-cell-runtime --locked --lib placement::tests` | 15 passed (6 new balance tests; the shorter `placement` filter also matches five node-policy tests) |
| `cargo test -p crab-cell-runtime --locked` | 308 lib + all integration suites passed |
| `cargo test -p crab-http-server --locked --lib` | 247 passed (new `fleet_rebalance_donates_ownership_surplus_without_headroom_gain`) |
| `cargo clippy -p crab-cell-runtime -p crab-http-server --all-targets --locked -- -D warnings` | clean |
| `cargo fmt -p crab-cell-runtime -p crab-http-server` | ran; no further changes |

The new router test is the end-to-end shape: two nodes with identical headroom
ratios, two Cells on the smaller node, one large node joining. Exactly one Cell
moves (the donor's surplus, not the batch), a repeat tick over the same samples
moves nothing, and fresh samples show the fleet at target. This is local
in-process evidence; protected multi-process movement proof remains plan 032.

Remaining work on this surface, in order: measured per-Cell cost
(`resource.rs` reservations plus recent demand instead of constants), a
shared fleet-wide movement budget, and the pressure wiring in P0 item 1 so
shedding and balancing cannot both fire from the same skew.

## Status after the first implementation pass

Landed, with the merged PR that carries the evidence:

| Item | Landing | Evidence |
| --- | --- | --- |
| P1 7 per-primitive observability | #303 | `CellTelemetry::primitive_operation` reported from `LocalCellTransport` and `PeerDispatcher`; `crab_cell_primitive_operations_total{module,kind,outcome}` and `crab_cell_primitive_operation_seconds{module,kind}` registered from `Registry::module_names()` |
| P2 9 dead placement inputs | #302 | `locality_bonus` and its score weight removed; `PlacementPressure` documents why peers reach only the critical class |
| P2 12 twin-gate check | #304 | `crab/scripts/check-policy-entry-points.py` runs beside the crate layout gate in the cell runtime workflow |
| Scheduler capability drift | #301 | `PRIMITIVE_TABLES` single-sources the probe, the guards, and the class budget; `MaintenanceBudget::finish` fails a Tick whose reserved class never ran |
| Scheduler gate coverage | #299 | Blob and Cron capability-gate tests |
| Durability submission outcomes | #296 | `DurabilitySubmissionOutcome` plus `crab_cell_durability_submissions_total{outcome}` |
| LTX admission race | #298 | `reconcile_admissions_locked` drops a budget hook that died during the call |
| Closed-ledger attribution | #297 | Ledger admission hooks carry the session that installed them |
| P1 4 Cron contract | this branch | `docs/primitives.md` states that schedules are fixed intervals with an explicit first due time, and that expressions and time zones stay above the primitive |

Deferred seams are now recorded in the policy inventory instead of living only
in review memory: `observe_pressure` and `evict_idle` (P0 1), the Blob
`sweep_unreferenced` helper (P0 2), and `takeover_unpublished`, which the router
never reaches because it fails closed on a rootless control record.

### Unpublished repository initialization needs a decision

`initialize_repository` writes `Control::initial` — `Recovering`, `root: None`,
owner is the job's freshly minted session — and only then bootstraps and
publishes (`crates/crab-http-server/src/cells/initializer.rs`,
`crates/crab-cell-runtime/src/control.rs`). A crash inside that window leaves the
Cell unpublished, and the retry cannot repair it:

- the retry runs with a new session, so it refuses with "offline repository
  initialization cannot fence an existing Cell owner"; it cannot mint a
  `NodeTakeoverProof` because it runs unleased and has no directory;
- catalog `provision` is idempotent for an identical entry
  (`src/cell/catalog.rs`), so the retry reaches that refusal instead of
  re-provisioning;
- the router fails closed on a rootless control record
  (`src/cells/router.rs`), so the Cell stays inactive.

`CellRuntime::takeover_unpublished` implements exactly this recovery: fence the
dead unpublished owner, re-initialize through a caller-supplied closure, and
publish. It has no production caller and is recorded as deferred until the
choice is made: give the initializer (or a directory-backed operator path) that
takeover, give the initializer a stable per-repository owner session so a retry
resumes its own unfinished activation, or state that an unpublished Cell is
abandoned and remove the API. The stable-session option needs the concurrent
initializer case worked out first: today two runs are separated by their session
identities, and reusing one would let both attempt the same publication.

Still open, in the order the audit proposed: P0 1 pressure wiring (needs the
decision to feed the classifier and sign the tier), P0 2 Blob collector,
P0 3 protected provider proof, P1 5 fleet-wide drain serialization, P1 6
slow-member backstop, P1 8 queue batch send (no production sender exists yet,
so this waits for a caller), P2 10 measured per-Cell cost, and P2 11 primitive
simulation parity.
