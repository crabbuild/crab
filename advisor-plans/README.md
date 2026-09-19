# Crate quality implementation plans

Created 2026-09-06 with the improve skill; planned against `ebd0e40d14c`.
This directory separates the selected crate-guidance work from the existing
GC/product roadmap in `plans/`.

| Plan | Scope | Priority | Effort | Depends on | Status |
|---|---|---|---|---|---|
| [001: Per-crate agent guides](001-per-crate-agent-guides.md) | All 21 shared/server crates; AGENTS.md plus CLAUDE.md symlinks | P1 | M–L | None | DONE |

## Execution order

Execute plan 001 in its six batches. Complete source-backed navigation and
validation for every crate before marking it DONE. No dependency on GC plans.

## Source quality follow-up

[002 — Rust crate source quality](002-crate-source-quality.md) is in progress
across all 21 crates. It tracks source fixes, documentation corrections,
regression evidence, and the remaining qualification work.

## Agent-guide scope decisions

- Selected by the user: agent guides across all 21 crates.
- Deferred: README/rustdoc rewrites, executable examples and code decomposition.
- Rejected: copying root instructions into each crate; this adds duplicated policy.
- Existing split-crate CI already checks interfaces, behavior, Clippy and tests;
  new blanket quality gates are not part of this plan.

## Completion evidence — 2026-09-07

Implemented all six batches: 21 crate-local AGENTS.md guides and 21 relative
CLAUDE.md symlinks. Each guide includes named source entry points, a concrete
call path, common-change routes, local invariants, feature/platform notes and
focused verification recipes. Parent review checked the complete guides and
relevant source paths; revisions corrected close-test selection, staging
flush ownership, metadata minimal features and LFS sibling implementation scope.

Checks passed against the completed files:

- Plan membership and structural checks: 21/21 crates and valid relative aliases.
- 210 distinct repository path references exist; 146 navigation symbol tokens
  occur in their referenced source files.
- All declared crate feature names appear in the corresponding guides.
- 47 Cargo test recipes: shell syntax, package names, feature names and
  integration targets checked; 44 library filters map to source modules/functions.
- Exactly 42 guide/alias additions. No Rust source, manifests, tests, existing
  READMEs, inherited guides or CI changes. `git diff --check` passed.

These are static documentation checks and source review, not test execution.
Cargo recipes were not run: this is documentation-only work, and the required
workspace build volume is unavailable on this host. No runtime or provider
qualification is claimed. README rewrites, runnable examples and Rust refactors
remain separate work; this completion covers plan 001's full guide scope.

## Canonical Cell runtime implementation track

Created 2026-09-17 with the improve skill; planned against `4a77b6f1252a`.
The design authority is
`crates/crab-cell-runtime/docs/canonical-ltx-scaling.md`. These plans convert
its 17 delivery slices into 14 reviewable changes. Each plan repeats its own
context, constraints, verification commands, acceptance criteria, and stop
conditions so an executor can use it without relying on conversation history.

| Plan | Outcome | Priority | Effort | Depends on | Status |
| --- | --- | --- | --- | --- | --- |
| [004](004-cell-runtime-architecture-guard.md) | Enforce the canonical server -> runtime -> LTX boundary and lock current behavior | P1 | M | None | DONE |
| [005](005-pure-cell-coordination-kernel.md) | Make protocol decisions pure while retaining one production adapter | P0 | XL | 004 | IN PROGRESS |
| [006](006-deterministic-coordination-simulation.md) | Replayable adversarial schedules and broken-variant proof | P0 | L | 005 | IN PROGRESS |
| [007](007-cell-coordination-tla-model.md) | Small-state formal model and code/model delta ledger | P1 | L | 005 | IN PROGRESS |
| [008](008-resident-cell-local-routing.md) | Zero-metadata-I/O local handle acquisition for safe resident Cells | P0 | L | 004, 005 | DONE |
| [009](009-background-hydration-and-resident-promotion.md) | Bounded sparse hydration and verified resident promotion | P0 | L | 008 | IN PROGRESS |
| [010](010-streaming-cell-ltx-publication.md) | Bounded-memory native and bundle publication | P0 | XL | 005 | IN PROGRESS |
| [011](011-persistent-directory-node-cache.md) | Restart-persistent verified directory acceleration | P1 | L | 010 | DONE |
| [012](012-cell-lifecycle-eviction-and-resource-accounting.md) | Quiescing, idle eviction, and one resource ledger | P0 | XL | 008-011 | IN PROGRESS |
| [013](013-signed-placement-observations-and-planner.md) | Authenticated live observations and deterministic weighted placement | P0 | XL | 005, 012 | IN PROGRESS |
| [014](014-pressure-shedding-and-paced-drain.md) | Hysteretic shedding and safe paced movement | P0 | XL | 013 | IN PROGRESS |
| [015](015-cell-runtime-qualification-receipts.md) | Simulator, provider, fault, scale, latency, and primitive release evidence | P0 | XL | 006-014 | IN PROGRESS |
| [016](016-standalone-replication-compatibility-decision.md) | Complete tagged-contract audit and named support decision | P1 | M | 015 | IN PROGRESS |
| [017](017-execute-standalone-replication-decision.md) | Retain, deprecate, or remove exactly as approved | P1 | L-XL | 016 | BLOCKED |

### Dependency graph and execution waves

```text
004 architecture guard
 └─ 005 pure coordination kernel
     ├─ 006 deterministic simulation ───────────────┐
     ├─ 007 TLA+ model ─────────────────────────────┤
     ├─ 008 resident local routing                  │
     │   └─ 009 background hydration                │
     └─ 010 streaming publication                   │
         └─ 011 persistent directory cache          │
                                                    │
008 + 009 + 010 + 011                               │
 └─ 012 lifecycle, eviction, unified accounting     │
     └─ 013 signed observations + placement         │
         └─ 014 pressure shedding + paced drain     │
                                                    │
006 through 014 ────────────────────────────────────┘
 └─ 015 qualification receipts
     └─ 016 standalone compatibility decision
         └─ 017 execute the approved decision
```

Recommended waves:

1. **Safety seam:** 004, then 005.
2. **Protocol assurance and hot path:** 006 and 007 may proceed independently
   after 005; 008 and 010 may also proceed in parallel in separate worktrees.
3. **Residency and bounded storage:** 009 follows 008; 011 follows 010.
4. **Fleet control:** 012 joins all local resource work, followed by 013 and 014.
5. **Release proof and surface convergence:** 015, then 016. Plan 017 remains
   blocked until the decision record identifies an option and approver.

The design's native/bundle streaming slices are combined in plan 010 because
both must use one verifier/uploader to avoid two memory paths. Actor lifecycle
and node-wide accounting are combined in plan 012 because eviction is unsafe
without complete reservations. The three standalone slices become an audit
(016) and conditional execution (017), so proof cannot be deleted before the
compatibility decision.

### Shared completion rules

- Production composition remains `crab-http-server -> crab-cell-runtime ->
  crab-ltx`; no plan may add a parallel owner.
- Every Rust build/test/lint uses a checkout-specific target directory beneath
  `$HOME/Workspace/crabbuild-target`; stop if that volume is unavailable.
- A response is durable only under the existing exact-root or accepted follower
  proof contract. No latency/placement work weakens fencing or acknowledgement.
- Tests protect canonical behavior and migration boundaries, not obsolete
  internals. Delete the old path when a replacement becomes canonical.
- No new config/env surface, fallback reader, alias, or compatibility shim is
  implicit. Each requires a named shipped contract and reviewed migration.
- Release/scalability claims require plan 015 receipts tied to exact source,
  executable image, workload, environment, and raw artifacts.
- Plan 017 cannot start while its decision is pending or its inventory has
  drifted.

### Implementation ledger — 2026-09-18

The local implementation slices are present on the canonical path. The ledger
is deliberately not marked as release-complete where the acceptance criterion
requires a real provider, Kubernetes fault, complete advertised/metric parity,
or an authorized standalone-contract decision.

Local proof completed:

- `crab-cell-runtime`: 210 library tests passed (one provider test ignored),
  47 default actor tests passed (one provider test ignored), and the
  process-support actor matrix runs 52 cases with 51 passing and one provider
  test ignored. All primitive,
  migration, publication, simulator, and workflow suites pass. The ignored
  source-loss and retention tests also pass against an isolated local RustFS
  bucket when their provider variables are supplied. The shared
  runtime/SQL/hydration/primitive-job ledger (including exported hydration-job
  usage/capacity metrics) and schema-v3 receipt evidence path are covered by
  focused tests; user SQL commands now hold bounded worker
  reservations for their full queued/executing lifetime, and pending
  publication bytes remain ledger-reserved until publication completes. Active
  Cell admission also reserves a fixed descriptor cost in that ledger, and
  runtime statistics/Prometheus gauges expose descriptor usage and capacity.
  `resident_route_reports_zero_origin_reads_and_latency_percentiles` then runs
  64 resident-handle plus SQL reads through the instrumented store; the latest
  local run recorded p50 67us, p95 90us, p99 364us, and zero origin reads.
  `restored_sparse_route_promotes_before_zero_origin_reads` also publishes and
  drains a Cell, reacquires its exact root through a new runtime, waits for
  verified sparse hydration to promote the resident route, and observes zero
  origin calls on the subsequent SQL read. The
  `shutdown_releases_a_hydration_reservation_after_an_origin_wait` regression
  blocks an origin read after sparse activation, shuts down the runtime, and
  verifies the hydration reservation returns to zero. These are repeatable
  local warm-path proofs, not provider or release receipts.
- `crab-ltx --features replica`: 45 unit tests, 79 integration test cases
  (one provider case ignored), and 5 doctests, plus the new streaming, cache
  restart, concurrent-fill, and fault-injection coverage pass; the ignored
  RustFS round trip also passes against the isolated local provider.
- `crab-ltx --no-default-features`: 12 unit tests, 21 integration/doc tests,
  and 5 doctests pass, so the standalone/minimal feature boundary remains
  buildable.
- `crab-http-server --lib`: 196 tests pass (four provider/browser tests remain
  explicitly ignored); the local RustFS collaboration/takeover, native-push,
  and receive-fault qualifications pass when run with an isolated prefix. The
  architecture target, architecture regression tests, Clippy, formatting,
  documentation validation, the 512-seed simulator corpus, exact-seed replay,
  and TLC fast/negative/broad safety plus fair stable-provider liveness checks
  pass.

The latest isolated local RustFS run (RustFS 1.0.0-rc.1, 2026-09-18) also
passed the LTX round-trip/parity/CAS-race, Cell source-loss takeover, retention
graph, HTTP receive-fault, native HTTP push, and public collaboration/takeover
cases. These are provider/fault iteration receipts, not signed release
evidence; protected three-Pod, matched-latency, and fleet gates remain open.

The current checkout also passed the full local Compose/RustFS cluster
qualification (version-5 receipt) with two owner losses, exact-root monotonicity,
follower replacement, and follower-only commits under an immutable-object deny
policy. The raw receipt is retained on the external qualification volume. It
strengthens local process/fault evidence but is not protected Kubernetes or
signed release evidence.

The same RustFS qualification volume ran the documented `10m`
`rustfs_replication_scale_load` profile on the current branch: 10,000,000 rows,
200 published segments, an 838,262,784-byte source database, exact restore of
42,234,991,936 logical object bytes, 46,993 records/second load throughput,
212.797 seconds wall time, and 25.619 seconds restore verification. This
refreshes provider-scale publication/restore evidence only; it is not a peak
RSS or multi-Pod receipt.

The same RustFS qualification volume ran the canonical release
`rustfs_cell_replica_scale_load` example with a 5,368,709,120-byte incompressible
source grown through 160 bounded captures (320 immutable segments). It deleted
the source, restored the published root, compacted the complete range, restored
the compacted root, and matched the source BLAKE3/length exactly. `/usr/bin/time
-l` recorded 1,496.96 seconds wall time and 592,805,888 bytes maximum resident
set size (~565 MiB); the largest observed compaction scratch LTX was about
5.1 GiB on the external qualification volume. This closes the canonical native
multi-GiB/RSS receipt; broader provider matrices remain open.

The in-repo placement path now consumes the signed observation block for cold
activation: the planner selects a live eligible session, sends one authenticated
activation hint, and the destination enters through the existing router,
authority CAS, and actor admission. Advertised free memory, disk, and job
headroom is conservatively clamped by the same runtime reservations used for
admission. Persisted Queue/Workflow rows are
re-inspected after durable work and an unknown result remains ineligible for
eviction. Remaining release gates are recorded in plans 009–017:
matched warm-latency/restart receipts, provider-failure
matrix evidence (with one local fail-first immutable PUT proof now covered),
complete advertised/metric parity and mixed-workload resource accounting,
multi-process
movement/fault proof, protected Kubernetes receipts, and the named plan-016
retain/deprecate/hard-remove decision. Plan 017 remains correctly blocked; no
standalone export or stored prefix was removed.

The signed placement snapshot now has an end-to-end local provenance check:
`NodePublisher` publishes while holding one runtime ledger byte/job reservation,
and the peer test verifies that the signed memory/disk totals and Cell/job
counts come from that coherent runtime sample. Both the signed disk total and
free headroom are clamped by the runtime-owned `DiskBudget`, so a larger
filesystem probe cannot advertise bytes the actor cannot admit; the deliberately
mismatched-capacity peer regression covers this case. Nested cgroup fixture
parsing and process file-capacity checks cover the fail-closed host probe;
advertised/metric parity and multi-process convergence remain qualification
work.

Scheduler maintenance is also ledger-visible: migration and node-log recovery
tasks now retain a `NodeJobReservation` until their spawned futures finish,
alongside the existing per-cell/session guards. This closes the untracked
background-job path without adding a second capacity owner. The HTTP peer
boundary charges authenticated protobuf verification/reply encoding and
node-log append/tail codecs to that same primitive-job ledger; SQL codecs stay
inside their worker-job reservation. Advertised-placement parity and measured
mixed-workload proof are still explicit Plan 012 qualification gates. Runtime
Prometheus metrics now expose usage and capacity for every host-ledger class.
The HTTP server builds those gauges through one
`RuntimeSnapshot::with_cell_runtime` projection;
`runtime_snapshot_projects_live_cell_ledger` installs a runtime-owned
`DiskBudget`, reserves a nonzero disk amount through its admission hook, and
proves live runtime reservations and capacities reach the rendered exposition
without a second field mapping.
Measured local-disk tolerance and provider-scale mixed-workload proof remain
qualification gates rather than being inferred from this unit proof.

Plan 010's canonical decoder no longer calls unbounded `read_to_end` for the
trailer/index: it drains the remaining authenticated metadata through a fixed
64 KiB buffer. `cell_prepare_bounds_source_and_scratch_transfers` measures the
CellReplica source/scratch/upload path at no more than the 8 MiB multipart and
1 MiB scratch-transfer bounds. The 5 GiB native RustFS receipt now records
592,805,888 bytes maximum RSS while restoring and compacting a 5.1 GiB scratch
LTX; the legacy standalone Replica bundle-copy surface remains part of the
pending Plan 016 decision.

The movement simulator now includes an explicit lost-release-response event:
after the authoritative release, the reply can disappear without restoring the
old owner, and a live receiver must still acquire through the normal authority
path. Its existing receiver-crash and membership-loss guards remain model-only;
the protected multi-Pod movement receipt is still open under Plan 014. The
feature-gated `independent_processes_allow_one_idle_cell_winner` test now
launches two OS processes against one shared filesystem CAS store, proves that
exactly one process wins the idle-control race, and verifies the winner drains
back to `Idle` without changing the authoritative root. The actor integration
`released_cell_is_acquired_by_one_successor_runtime` additionally transfers one
idle control between two independent runtimes and verifies the authority owner
and active-cell reservations never overlap. Its companion
`crashed_process_is_fenced_before_successor_restore` exits an acquired process
without draining, fences that stale session, and restores the exact root in a
successor process. The companion
`lost_release_response_is_reconciled_before_successor_acquire` probe commits
the release while dropping its response, verifies reconciliation, and restores
the unchanged root in a successor runtime. The local
`failed_idle_receiver_does_not_leave_authority_owned` case then forces a
receiver activation failure after the idle takeover CAS and verifies the
exact root returns to unowned `Idle`. Its takeover counterpart,
`failed_takeover_receiver_does_not_leave_authority_owned`, proves the same
rollback after fencing an old owner; pinned recovery overlays remain owned
until replay is sealed. The feature-gated
`independent_process_receiver_failure_returns_exact_idle_root` probe repeats
the idle receiver failure in a separate OS process against the shared
filesystem CAS authority and verifies the exact root returns to unowned
`Idle`. Protected three-Pod movement and membership receipts remain open.

Restart inventory is now fail-closed at the HTTP composition boundary. Each
process gets a fresh `cells/sessions/<session-id>` directory; before the
runtime starts, `LocalStaging::new_with_restart_inventory` recursively counts
every regular file in older session directories and holds one shared
`DiskBudget` reservation for those bytes. The fresh session is excluded because
its active database/WAL/cache/transfer owners reserve bytes as they open; the
follower store and directory-cache constructors independently import their
durable namespaces into that same budget. Symlinks, special files, malformed
session roots, and over-capacity inventories fail closed instead of being
silently treated as free space. Focused HTTP tests cover nested stale files,
current-session exclusion, ambiguous symlink rejection, and capacity failure.

The embedded canonical LTX host is now ledger-visible too. `CellRuntime` installs
one weak runtime admission on its `ReplicaHost`; every bounded host I/O, blocking
job, recovery cohort, dirty-memory cohort, and scratch-MiB reservation acquires
an RAII token from the same `ResourceLedger` alongside the existing LTX
semaphore, before host work starts. The token remains attached to dispatched
work across caller cancellation, and the runtime exposes usage/capacity
snapshots for these classes. Standalone LTX callers without a runtime admission
remain unchanged; this is an embedding boundary, not a second accounting owner.

The actor lifecycle path also has a bounded churn regression:
`churn_evicts_idle_cells_and_restores_exact_roots` runs three independent
repository fixtures through a two-Cell runtime, waits for persisted-work
inventory to become known, evicts one idle Cell, reacquires its unchanged root
through the canonical idle-restore path, bootstraps the third Cell after the
reservation is released, and asserts a zero active-Cell baseline before
shutdown. It deliberately uses bootstrap roots because durable command
outcomes are release obligations; mutation-root restoration remains covered by
the command/publication/cancellation tests. The companion
`mixed_primitive_inventory_blocks_churn_until_drain_and_restores_root` test
installs durable Queue and Workflow rows in two Cells, proves both are
ineligible for eviction until the actor drain path runs, restores the exact
Queue root and row, then admits a third Cell and returns the ledger to zero;
the source-loss takeover suite covers the separate owner-loss path.

The companion `persisted_work_blocks_idle_eviction_until_explicit_release`
regression executes a durable SQL mutation, waits for inventory refresh, and
proves that retained request outcomes keep `evict_idle` fail-closed until the
canonical drain/release path runs. It also checks the active-Cell ledger returns
to zero after release.

Plan 011's warm-restart boundary is now locally qualified by
`directory_cache_survives_replica_restart_without_directory_origin_read` in
`crates/crab-ltx/tests/cell_roots.rs`. The test warms the verified directory
cache, drops the first replica, then uses fresh Store identities against the
same backend so the process-local node cache cannot satisfy the read. A
no-cache fresh replica establishes the origin-read baseline; the restarted
cache-enabled replica reads the same page with fewer origin bytes, proving the
directory nodes came from the persisted cache while the page frame still comes
from canonical storage. The test is repeatable and passes with the full
`crab-ltx --features replica` cell-root target.

Plan 008's lifecycle race is also covered by
`resident_lookup_is_invalidated_before_drain_releases_the_cell` in
`crates/crab-cell-runtime/tests/actor.rs`: a resident lookup succeeds before
the canonical drain, then the actor's drain transition makes the next lookup
miss before the handle/resource release completes. This keeps the local route
owned by the actor and prevents a cleanup race from exposing a stale serving
handle.

Plan 010 now has a cancellation regression in
`crates/crab-ltx/tests/host_hooks.rs`:
`cancelled_cell_prepare_releases_scratch_without_publishing_a_root` throttles
the first immutable upload, cancels native preparation, and verifies the
replayable scratch namespace and scratch semaphore return to baseline. Together
with the existing injected filesystem-failure tests and fail-first immutable
PUT test, this closes the local failure/cancellation cleanup gate; the 5 GiB
RSS receipt is now recorded above, while provider-matrix receipts remain
intentionally external.

The coordination kernel now records a typed intent beside every local effect
identity. A completion must match both the activation generation and its
effect family (work, hydration, inventory, publication, proof, or renewal),
so a delayed completion from one adapter cannot release another operation that
reuses an integer identity. The mismatch rule is covered by a pure transition
test and the full runtime target suite.

The actor scheduling seam now supplies queue, publisher, publication high-water,
and lease observations to that same kernel. Dispatch, wait, fence, and
deactivation are selected by `CoordinationInput::Schedule`; `start_next` and
the drain/shutdown/eviction paths do not duplicate busy/renewal/fence or lease
policy. Pure tests cover
publication backpressure and lease loss. Hydration, renewal, and persisted-work
inventory refresh now pass their queue/publication/unknown-work/lease
observations through the kernel as well; the actor only owns resource
reservation, effect execution, and generation-matched inventory application
after a `Started` result.

Task completions now carry their fence observation into the same pure API.
`FinishWork`, `FinishMigration`, `FinishPublication`, `FinishRenewal`, and stale
hydration completion return an explicit `Fence` decision; the actor performs
cleanup only after that decision. Pending commands remain busy until proof, and
the new transition tests cover fenced work and migration completion so a task
cannot accidentally reopen a serving Cell or let a later request overtake an
unpublished result.

The schedule transition now distinguishes `ReadyToDeactivateFenced` from a
normal live drain. Release-path selection therefore comes from the kernel
decision rather than an actor-side fenced-state read, including after node-lease
loss.

The final publication adapter guard was also removed. Publication admission is
now exclusively `CoordinationInput::BeginPublication`, completion is exclusively
`FinishPublication`, and `start_publication` only executes a kernel-approved
effect. This closes the last actor-side lifecycle predicate in the coordination
path; the coordination and actor suites pass with no compatibility branch or
feature flag selecting an alternate decision implementation.

Blob upload lifetimes and Cron first-due windows are validated from the
mutation-issued timestamp, while the serialized Cell still rejects an upload
that has expired before acceptance. This keeps absolute caller deadlines stable
under queue or transport delay without making an overdue-but-valid Cron schedule
ineligible; the next Tick owns its durable catch-up.

The typed primitive owner-loss slice now exercises the canonical fence and
takeover path: `typed_blob_and_cron_recover_after_owner_loss`,
`typed_kv_namespace_recovers_after_owner_loss`, and
`typed_queue_namespace_recovers_after_owner_loss` drop the first runtime,
fence its exact node session, restore the published root, and verify a typed
read/ack/tick after takeover. Workflow/activity owner loss remains covered by
the native activity failover test, SQL publication/source loss remains covered
by the actor takeover suite, and
`typed_effect_source_publishes_claim_validation_ack_and_lost_lease` recovers
the durable effect ledger before claim/ack. These are local in-memory
ownership receipts; protected three-Pod primitive-fault evidence remains open.
