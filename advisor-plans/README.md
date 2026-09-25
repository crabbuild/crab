# Crate quality implementation plans

## crab-ltx foundation hardening

Planned on 2026-09-25 against `3b8d3b3614c`. [035](035-crab-ltx-foundation-durability-and-performance.md)
is a four-slice execution track for the durability mechanics beneath
`crab-cell-runtime`: contract and sparse-activation baseline, a proven
checksum-sidecar barrier reduction, crash/exactness qualification, then one
profile-selected optimization. Execute its slices in order. It depends on the
existing streaming-publication work in 010 and protected qualification in 015
for their respective boundaries; it does not reopen the standalone
epoch-head surface removed by 016–017.

| Plan | Priority | Effort | Depends on | Status |
| --- | --- | --- | --- | --- |
| [035](035-crab-ltx-foundation-durability-and-performance.md) | P0 correctness / P1 performance | L | 010 streaming path; 015 release qualification | IN PROGRESS — local slices 1–3 pass, including modeled cuts, clean process resume, and RustFS sparse-owner process kill; Slice 4 local capture optimization measured; physical power-cut and protected proof gates remain |

## Safe Cell rebalance and scale up/down

Planned on 2026-09-21 against `cebc909940f137e4bd8445e524e77a154bf51a29`
in the `cell-safe-rebalance` worktree. The technical design is
[026](026-safe-cell-rebalance-and-scale.md). These are focused implementation
plans for that design, not a new full-monorepo audit. Each file includes its
own current-state evidence, scope, commands, acceptance gates, and stop
conditions; read the whole file before executing. Do not mark a release claim
complete from local tests or synthetic qualification receipts.

| Order | Plan | Outcome | Depends on | Status |
| --- | --- | --- | --- | --- |
| 1 | [027](027-sign-live-rebalance-inputs.md) | Sign measured Cell, memory, disk, job, and backlog inputs | None | IMPLEMENTED; protected E2E remains 032 |
| 2 | [028](028-project-bounded-cell-transfers.md) | Produce deterministic, projected, paced transfer intents | 027 | IMPLEMENTED; protected E2E remains 032 |
| 3 | [029](029-actor-verified-cell-release.md) | Recheck unsettled work and await exact-Cell release | 027; may proceed alongside 028 | IMPLEMENTED; protected E2E remains 032 |
| 4 | [030](030-host-scale-down-lifecycle.md) | Keep blocked Cells and node lease alive during scale-down | 029 | IMPLEMENTED; protected E2E remains 032 |
| 5 | [031](031-run-fleet-rebalance-controller.md) | Wire signed planning, host movement, and receiver activation | 027–030 | IMPLEMENTED; protected E2E remains 032 |
| 6 | [032](032-prove-cell-rebalance-end-to-end.md) | Prove user-visible scale-up/down and fault safety | 027–031 | IN PROGRESS; protected provider/Kubernetes + multi-process evidence open |

The implementation critical path is 027 → 029 → 030 → 031 → 032; 028 starts
after 027 and joins before 031. Use a different external Cargo target
directory if executing any slice in another worktree; target directories
cannot be shared across checkouts. After each slice, update its status to TODO,
IN PROGRESS, DONE, or BLOCKED with the exact failed gate. The protected
provider/Kubernetes/scale receipt remains an independent release gate after
local implementation proof. The worktree-specific external Cargo target must
be mounted and writable before compiling; never fall back to local
`target/`.

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

## HTTP identity and repository administration

| Plan | Outcome | Priority | Effort | Depends on | Status |
| --- | --- | --- | --- | --- | --- |
| [018](018-auditable-membership-and-backchannel-logout.md) | Auditable browser membership administration and OIDC back-channel session revocation | P1 | XL | None | DONE |

Plan 018 is an independent security and administration track. Its catalog/API,
UI, back-channel logout, and operations-docs surfaces are implemented in the
reviewable commits recorded in the plan. The catalog v3 write and the eight-hour
session-index migration are deployment boundaries; read the plan's rollout
notes before promotion.

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
| [005](005-pure-cell-coordination-kernel.md) | Make protocol decisions pure while retaining one production adapter | P0 | XL | 004 | DONE |
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
| [016](016-standalone-replication-compatibility-decision.md) | Complete tagged-contract audit and named support decision | P1 | M | 015 | DONE — HARD REMOVE |
| [017](017-execute-standalone-replication-decision.md) | Retain, deprecate, or remove exactly as approved | P1 | L-XL | 016 | DONE — HARD REMOVE |
| [018](018-failover-phase-evidence.md) | Phase-level failover metrics, receipt baseline, and accurate operator docs | P0 | M | 015 infrastructure | PARTIAL — strict selection/work receipt contract implemented; protected run pending |
| [019](019-indexed-follower-tail-reads.md) | Crash-rebuildable follower index and seek-only tail pages | P0 | L | 012, 018 | IMPLEMENTED — protected scale evidence pending |
| [020](020-tail-scoped-streaming-recovery.md) | Tail-derived affected-shard/Cell validation and bounded recovery streaming | P0 | XL | 010, 018, 019 | IMPLEMENTED — protected scale evidence pending |
| [021](021-follower-affine-recovery-and-takeover.md) | Deterministic follower-first recovery and takeover with bounded fallback | P0 | XL | 013, 018, 020 | IMPLEMENTED — direct self-discovery; protected scale evidence pending |
| [022](022-local-follower-recovery-fast-path.md) | Same-host follower transport and digest-verified recovery artifact reuse | P1 | L | 012, 019-021 | IMPLEMENTED — protected cache evidence pending |
| [023](023-published-image-failover-qualification.md) | Qualify an immutable candidate image and promote the same digest | P0 release gate | L | 015, 018 | IMPLEMENTED — protected run pending |

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
5. **Release proof and surface convergence:** 015, then 016, then the selected
   hard-removal execution in 017.

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
- Plan 017 must follow the recorded decision and stop if its inventory drifts.
  The current decision is hard removal; no compatibility reader or alias may
  be introduced during execution.

### Follower-affine failover hardening extension

Created 2026-09-20 with the improve skill; planned against `origin/main` at
`c86dd43423ae`. The architecture and safety boundary are recorded in
[Follower-affine Cell failover hardening](follower-affine-failover-design.md).
The source-backed review and remaining gates are recorded in
[the 2026-09-20 audit](failover-design-audit-2026-09-20.md). These plans extend
the canonical Cell track; they do not replace plans 009, 011, 013, or 015.

```text
010 streaming publication ──────────────┐
012 resource/restart accounting ────┐   │
013 signed placement ────────────┐  │   │
                                 │  │   │
015 qualification ──────┐        │  │   │
018 phase evidence ─────┼─> 019 indexed reads
                        │        └──────> 020 tail-scoped recovery
                        │                  └─> 021 follower-affine takeover
                        │                      └─> 022 local fast path
                        └─────────────────────> 023 digest-bound release proof
```

Recommended execution:

1. Finish the named plan-010/012/013 API prerequisites; protected qualification
   is not required for their local APIs to be consumed.
2. Land 018 and preserve its version-6 local baseline receipt.
3. Land 019, then 020. They share follower/recovery contracts and are not a
   parallel wave.
4. Land 021 after 020 so scheduler signatures and phase boundaries are stable.
5. Land 022 after follower affinity and canonical file-backed pinning exist.
6. Land 023 before any release claim. It may develop after 018 in parallel with
   019-022, but its protected proof gates promotion of the finished stack.

Top-priority product outcome is 021: the surviving follower becomes the
preferred recovery executor and owner candidate. Plans 018-020 precede or run
before it because they make the change measurable and prevent follower-local
recovery from retaining quadratic lane/catalog work. Plan 022 is the final RTO
optimization; it must not bypass immutable object pinning. Plan 023 closes the
release-evidence gap: current source-only Compose evidence cannot be relabeled
as proof for a later-built image digest.

Shared extension rules:

- Follower-first is bounded preference. Any eligible node remains the explicit
  availability fallback after the grace interval.
- Stable physical `NodeId` identifies retained follower data; a fresh live boot
  `SessionId` always owns the recovery claim and successor Cell.
- Recovery hints and placement scores are advisory. Node claim CAS, takeover
  proof, Cell control CAS, epoch increment, and actor admission remain authority.
- Every recovered overlay is pinned in object storage before control names it.
  Local bytes may accelerate verification/restore but are never sole authority.
- The successor opens a fresh sparse `crab_ltx::Db`; old writable SQLite state
  is never reopened.
- Existing plan 011 directory-cache and plan 009 hydration work remain canonical.
  Whole-database follower prewarm is deferred until phase evidence proves need.
- No new env/config surface, wire format, or compatibility reader is implied.
  Stop and record a shipped contract if execution discovers one.

### Implementation ledger — 2026-09-18

The local implementation slices are present on the canonical path. The ledger
is deliberately not marked as release-complete where the acceptance criterion
requires a real provider, Kubernetes fault, complete advertised-placement
parity, or an authorized standalone-contract decision.

Local proof completed:

- `crab-cell-runtime`: 210 library tests passed (one provider test ignored),
  47 default actor tests passed (one provider test ignored), and the
  process-support actor matrix runs 52 cases with 51 passing and one provider
  test ignored. All primitive,
  migration, publication, simulator, and workflow suites pass. The ignored
  source-loss and retention tests also pass against an isolated local RustFS
  bucket when their provider variables are supplied. The provider-backed
  `rustfs_mixed_primitive_inventory_churn_preserves_exact_roots` case also
  passes the Queue/Workflow retained-work, exact-root restore, and
  capacity-reuse proof against an isolated RustFS prefix. The shared
  runtime/SQL/hydration/primitive-job ledger (including exported hydration-job
  usage/capacity metrics) and schema-v5/profile-digest receipt evidence path are covered by
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
  architecture target (including the hard-removed standalone-LTX symbol guard),
  architecture regression tests, Clippy, formatting,
  documentation validation, the 512-seed simulator corpus, exact-seed replay,
  and TLC fast/negative/broad safety plus fair stable-provider liveness checks
  pass.

The latest isolated local RustFS run (RustFS 1.0.0-rc.1, 2026-09-18) also
passed the LTX round-trip/parity/CAS-race, Cell source-loss takeover, retention
graph, HTTP receive-fault, native HTTP push, and public collaboration/takeover
cases. These are provider/fault iteration receipts, not signed release
evidence; protected three-Pod, matched-latency, and fleet gates remain open.

The qualification receipt implementation now has a canonical matrix manifest
and fresh-process verifier. It requires exactly one row for protocol, storage,
publication, warm path, churn, fleet, failover, primitives, accounting, and
compatibility; rejects path traversal and duplicate/incomplete manifests; and
recomputes every raw artifact digest before accepting a row. This closes the
validator implementation seam. The contract workflow now exercises all ten
rows with independently signed fixtures and the fresh-process CLI; those
fixtures are explicitly not release evidence. Protected provider/Kubernetes
and release receipts remain open.

The current checkout also passed the full local Compose/RustFS cluster
qualification (version-6 receipt) with two follower-affine owner losses plus an
all-followers-unavailable non-member fallback, exact-root monotonicity,
follower replacement, and follower-only commits under an immutable-object deny
policy. The qualification harness now compares each node's capacity report
with its runtime Prometheus disk and active-Cell ceilings, the signed placement
block returned by `cells node --session SESSION --json`, and an independent df
filesystem probe (within 1 MiB) before workload; a mismatch fails the run and
the receipt records all parity checks. The raw receipt is retained on the
external qualification volume. It strengthens local process/fault evidence but
is not protected Kubernetes or signed release evidence.

The historical RustFS qualification volume also retains the pre-hard-removal
`rustfs_replication_scale_load` 10m receipt: 10,000,000 rows, 200 published
segments, an 838,262,784-byte source database, exact restore of 42,234,991,936
logical object bytes, 46,993 records/second load throughput, 212.797 seconds
wall time, and 25.619 seconds restore verification. That standalone harness is
no longer runnable or part of the Cell architecture; the receipt is retained
only as historical provider evidence. It is not a peak-RSS or multi-Pod receipt.

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
protected multi-process advertised-placement convergence and mixed-workload
resource accounting, multi-process movement/fault proof, protected Kubernetes
receipts, and the named plan-016 hard-removal decision. Plan 017 now deletes the
unshipped standalone exports while retaining canonical Cell mechanics;
standalone stored prefixes are not reinterpreted or deleted.

The signed placement snapshot now has an end-to-end local provenance check:
`NodePublisher` publishes while holding one runtime ledger byte/job reservation,
and the peer test verifies that the signed memory/disk totals and Cell/job
counts come from that coherent runtime sample. Both the signed disk total and
free headroom are clamped by the runtime-owned `DiskBudget`, so a larger
filesystem probe cannot advertise bytes the actor cannot admit; the deliberately
mismatched-capacity peer regression covers this case. Nested cgroup fixture
parsing and process file-capacity checks cover the fail-closed host probe;
local signed-placement parity is now covered by the three-process Compose
receipt, while protected multi-process convergence remains qualification work.

Scheduler maintenance is also ledger-visible: migration and node-log recovery
tasks now retain a `NodeJobReservation` until their spawned futures finish,
alongside the existing per-cell/session guards. This closes the untracked
background-job path without adding a second capacity owner. The HTTP peer
boundary charges authenticated protobuf verification/reply encoding and
node-log append/tail codecs to that same primitive-job ledger; SQL codecs stay
inside their worker-job reservation. Protected advertised-placement convergence
and measured mixed-workload proof remain explicit Plan 012 qualification gates.
Runtime
Prometheus metrics now expose usage and capacity for every host-ledger class.
The HTTP server builds those gauges through one
`RuntimeSnapshot::with_cell_runtime` projection;
`runtime_snapshot_projects_live_cell_ledger` installs a runtime-owned
`DiskBudget`, reserves a nonzero disk amount through its admission hook, and
proves live runtime reservations and capacities reach the rendered exposition
without a second field mapping. The Compose receipt independently measures each
mounted Cell filesystem within the documented 1 MiB tolerance and records the
signed placement projection; provider-scale mixed-workload proof remains a
qualification gate rather than being inferred from this local receipt.

Plan 010's canonical decoder no longer calls unbounded `read_to_end` for the
trailer/index: it drains the remaining authenticated metadata through a fixed
64 KiB buffer. `cell_prepare_bounds_source_and_scratch_transfers` measures the
CellReplica source/scratch/upload path at no more than the 8 MiB multipart and
1 MiB scratch-transfer bounds. The 5 GiB native RustFS receipt now records
592,805,888 bytes maximum RSS while restoring and compacting a 5.1 GiB scratch
LTX; the legacy standalone Replica bundle-copy surface was removed under the
authorized Plan 016 decision. Cell-scoped bundle preparation remains the only
supported bundle path.

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
`Idle`. The complete process-support actor target also passes once without a
retry loop, and the simulator's
`membership_loss_during_movement_preserves_released_root` case preserves the
released root and watermarks when membership disappears before acquisition.
Protected three-Pod movement and membership receipts remain open.

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

The architecture gate now protects the extracted seam as well: it requires the
private coordination types and actor adapter call, and rejects async/runtime or
provider dependencies in production kernel lines while admitting test fixtures.

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

## Production application hardening track

Created 2026-09-19 with the improve skill; planned against `892720ce6a6`.
This track reconciles the current source implementation—not the aspirational
documentation—with the requirements for a supported large-scale Cell
application platform. It supplements plan 015's existing qualification
infrastructure; it does not create a second receipt format or scheduler.

| Plan | Outcome | Priority | Effort | Depends on | Status |
| --- | --- | --- | --- | --- | --- |
| [018](018-fix-queue-scheduler-deadline.md) | Queue consumer readiness no longer causes maintenance commit loops | P0 | S | — | IMPLEMENTED |
| [019](019-command-scoped-effect-ledger.md) | One command-owned effect allocator enforces limits without retained-table scans | P0 | M | — | IMPLEMENTED |
| [020](020-cache-sealed-follower-tail-index.md) | Recovery tail pagination scans a sealed lane at most once per process | P1 | M | — | IMPLEMENTED |
| [021](021-constant-time-primitive-accounting.md) | Workflow capacity and Queue status use transactionally maintained counters | P1 | L | 018 | IMPLEMENTED |
| [022](022-freeze-cell-application-contract.md) | The supported author/operator API and scale envelope are executable contracts | P0 | L | 018–021 | IMPLEMENTED — handwritten API; code generation deferred |
| [023](023-production-cell-node-host.md) | One host facade owns runtime composition and a full-primitive application path | P0 | XL | 022 | IMPLEMENTED locally — protected qualification remains |
| [024](024-large-scale-primitive-qualification.md) | Release qualification proves every primitive, mixed load, faults, and resource bounds | P0 | XL | 018–023, 015 infrastructure | PARTIAL |
| [025](025-cell-runtime-production-readiness-execution.md) | Close host-ownership and protected qualification gates for named production profiles | P0 | XL | 022–024 | IN PROGRESS |

### Execution waves

```text
018 Queue deadline ownership ───────┐
019 effect ledger ─────────────────┼─> 022 supported contract
020 follower tail index ───────────┤       └─> 023 CellNode host
021 primitive counters <── 018 ────┘              └─> 024 release qualification

015 receipt/qualification infrastructure ────────────────┘
```

1. Execute 018 first because the current Queue state can create unbounded
   maintenance publications without useful work.
2. Execute 019 and 020 in parallel after 018 starts; they do not share files.
3. Execute 021 after 018 so Queue state transitions and their deadline
   semantics are stable before counters are attached.
4. Freeze the public support contract in 022 only after the implementation
   behavior is corrected. Do not encode current bugs as contracts.
5. Execute 023 as a bounded composition refactor: one facade becomes canonical
   before the old server assembly is removed.
6. Execute 024 against the integrated candidate. A green unit suite cannot
   mark any production-readiness row complete.

Current boundary: 018–023 are implemented and locally verified. 022 provides a
handwritten full-primitive author contract, deterministic descriptor and
relationship validation, typed capability scope checks, and the owner/source-loss
takeover proof for SQL, KV, Blob, Queue, Cron, Workflow, Activity, and Effects.
023 provides fail-closed serving/maintenance host `start`/`status`, readiness,
bounded ownership of the long-lived server coordination loops and production
router/peer/follower/transport components, provider-neutral NodeDurability
construction/recruitment/rotation ownership, required component slots,
admission-before-facility-drain ordering, and deadline-aware drain. 024
provides bounded profile constructors, deterministic streaming execution with
seed-bound per-primitive lifecycle case hints,
per-primitive verified-progress validation with canonical attempted-count
binding, measured run artifacts, preflight guards, pinned-signer validation,
and fail-closed release packaging. 024 remains partial until protected
provider/Kubernetes/scale and signed release receipts exist.

### Shared release rule

`crab-cell-runtime` may be called production-ready for a named profile only
after plan 024 produces a complete, validated matrix for the exact source and
image. "Large scale" must name workload cardinality, topology, resources,
provider, duration, and latency/error thresholds. The platform must continue
to state its semantic exclusions: no multi-Cell ACID, no exactly-once external
effects, no transparent hot-key splitting, and no general-purpose unbounded
SQL or Blob service.

## Celld comparison and primitive maturity audit

[Cell runtime audit: celld comparison and primitive production
readiness](cell-runtime-celld-audit-2026-09-22.md) reviews Celld's pinned
`crates/logic` policy layer against `crab-cell-runtime`, records an
eight-primitive readiness table with the remaining gaps per primitive, and
lists the prioritized opportunities. It also records the weighted
ownership-balance change implemented on Celld's `rebalance.rs` semantics: one
elected donor, a two-percent receiver deadband, batch and receiver-room bounds,
pre-batch samples rejected, and local planner plus router evidence.

## Cell and LTX layout re-organization for Cellule extraction

Created 2026-09-22; planned against `3ab2526492b`. The four Cell and LTX
crates are the source for the `cellule-*` workspace, which becomes the upstream
after its first release. This track fixes the layout before the extraction:
crate-local `src/` stays production code, `tests/` owns the suites, one
feature-gated `test_support` module replaces path-included source, fixtures
move into a single shared harness, and oversized files split at existing seams.

| Plan | Outcome | Priority | Effort | Depends on | Status |
| --- | --- | --- | --- | --- | --- |
| [033](033-cell-ltx-layout-reorganization.md) | Canonical src/tests layout, `test-support` module, capability-named suites, shared harness, allow-listed in-src tests, and the layout checker for `crab-cell-runtime`/`app`/`host`/`ltx` | P1 | L | None | DONE |

All six stages landed (option A for the module tree): the subsystem modules,
the capability suites with their shared harness, the documented public APIs,
the deduplicated helpers, and the layout gate now on `main`. The Cellule-side
rename, hardening merge, and release work are separate and are described in the
plan's handoff section.

## Cell P0 scale hardening

Created 2026-09-24; planned against `7f36da6bb83`. This track closes the three
structural P0 gaps that bound a Cell deployment before its hardware does:
metadata cost that grows with the Cell population, one durability boundary per
Cell commit, and a restore paid on every wake. Slice 1 (the catalog page
locator) landed with the plan; the remaining slices are protocol work and each
needs its own exit evidence.

| Plan | Outcome | Priority | Effort | Depends on | Status |
| --- | --- | --- | --- | --- | --- |
| [034](034-cell-p0-scale-hardening.md) | Page-locator catalog lookup, due-work hint index with a full-scan backstop, monotone durable-through watermark with pipelined commits, dormant residency with a resume receipt, and phase-attributed diagnosis of the failing qualification tail | P0 | XL | Plans 015, 023, 024, 025, 031, 032 | IN PROGRESS — slices 1, 2, and 4 DONE; slice 3, the restart-wide resume adoption, the scale receipt, and the protected gates remain |

Slice 1 is implemented and tested in `crates/crab-cell-runtime`: a version-two
catalog head carries a page locator, so routing reads one page instead of the
whole shard, and a page that disagrees with its locator is a hard error. Slice
2's shape was refined after reading the Tick and actor paths — resident Cells
tick themselves under the Tick's `expected_commit_sequence` staleness guard,
and only non-resident Cells need hint discovery — and slice 3a was folded into
3b because its trigger is unreachable from the actor today. The metadata
plane's production instrument also landed: `crab_cell_catalog_reads_total` and
`crab_cell_catalog_read_seconds` now count head and page reads, and the runtime
suite pins one lookup to exactly one head and one page read. Slice 2 stage 1
also landed: the actor mirrors the published due time and commit sequence,
`CellRuntime::due_resident` answers from memory, and the product scheduler
ticks resident Cells before it reads any catalog page or control record, with
both a runtime test and an end-to-end scheduler test. Slice 2 stage 2 (hint
discovery for non-resident Cells with a lengthened scan backstop) and slices
3–4 and 6 remain TODO with their proof obligations; none of them may be
promoted from a local run. Slice 5 has its metadata instrument in place:
catalog and control reads are counted per phase, and the runtime suite pins one
cold route at two catalog reads, two control reads, and three origin requests —
four of seven object-store requests are metadata, and a resident route issues
none. The phase-attributed receipt over the qualification workload is still
owed before the provider p99 is attributed. Slice 4 was attempted and
deliberately reverted: warm reuse is fenced inside `crab-ltx`, where one local
database path is one capture session forever ("an existing capture directory is
refused, even after a clean close"), so the slice needs either a `crab-ltx`
resume capability that mints a new session over a proven-clean file or a
fresh-path policy in the product router. The plan records the receipt design
facts the spike established, including why the ownership epoch must not be part
of the match. Slice 2 stage 2 also has its baseline pinned: a shard pass that
finds nothing due costs one control read per Cell in the shard plus two catalog
reads, proven by `due_scan_reads_one_control_record_per_cell` at 40 Cells and
extrapolating to a full shard of GETs per empty pass. Stage 1 also picked up
one defect and its regression test: the resident fast path could exhaust the
per-cycle budget and the shard scan then subtracted past zero, so
`scan_once_bounded` now skips the scan when the cycle is spent and
`resident_ticks_do_not_overspend_the_cycle_budget` fails with the overflow on
the unguarded code. Slice 5 also gained phase timing: activation now reports
`ownership`, `root_open`, `restore`, and `activate` through
`crab_cell_activation_phase_seconds`, and the cold-route test pins the phase
order beside the read counts, so an operator can attribute a cold route's tail
without running the protected workload.

Slice 2 stage 2's first half also landed: the runtime publishes one bounded
hint key when a clean release leaves a Cell with a deadline, the scheduler
consumes hints first each cycle and confirms every candidate against its
control, and the shard scan stays the backstop. Tests cover the write/consume
contract and the hint-only tick. The backstop then moved to a thirty-cycle
period, so the population scan no longer runs per second, and the scheduler's
own catalog and control reads now bind the node telemetry handle — the
`crab_cell_control_reads_total` series is what a scale receipt must watch. What
remains for that slice's exit evidence is that measurement at 10³/10⁵/10⁶
Cells, compared against the pinned 40-control-read empty pass. The instrument
also produced the next concrete optimization: a hinted candidate costs 10
catalog reads and 6 control reads today because the scheduler, router, and
activation each confirm the Cell, so sharing one resolution comes before any
further growth of the backstop period.

The plan closes with a current-state handoff table: which claim each landed test
proves, the three pinned costs to measure against (40 control reads for an empty
shard pass, 10/6 for one hinted candidate, 2/2/3 plus four phases for one cold
route), the exact verification commands, and the last full green run across
`crab-ltx`, `crab-cell-runtime`, `crab-cell-app`, `crab-cell-host`, and
`crab-http-server`. Release-path coverage is pinned too: drain, pressure
eviction, and prepared transfer all publish their hint, and a fenced release
deliberately does not.

The shared-resolution follow-up eventually landed: two failed attempts (a
broad hand-off and an unowned-only hand-off) both overflowed the worker stack in
`pull_request_merge_methods_use_canonical_ref_publication`, and `RUST_MIN_STACK`
bisection put the threshold between 2 MiB and 3 MiB — the routing path already
runs near the default stack, and the extra values tipped it over. Boxing the
activation future fixed it, and the unowned observation is now reused instead of
re-read: one hinted candidate costs 8 catalog + 5 control reads, down from 10 +
6, and the saving applies to every cold activation.

The deep nesting itself is specific to the in-process peer transport the tests
use: production forwarding crosses processes, so the same stack depth does not
arise there. The boxed boundary stays as a hardening — bounded stack per level
is what async routing should have — but no production stack-size knob was added
on the strength of a test-only measurement.

Slice 2's population-independence is now pinned locally: sixteen released Cells
with two hints cost a foreground cycle 16 catalog + 10 control reads (two
candidates at the pinned 8/5, nothing for the other fourteen), where the backstop
cycle over the same Cells costs 355 + 72. A deadline outside the listing window
also publishes no hint now, so the accelerator cannot leave behind keys nothing
would ever list or delete.

Slice 4's blocker is resolved on paper and validated in code: the epoch fence is
per database path, so a cleanly closed database that is renamed to a path
nothing has opened can be opened there with its rows intact
(`a_cleanly_closed_database_survives_a_rename_to_a_fresh_path`). The warm-wake
design therefore does not need a `crab-ltx` resume capability; it needs a
per-activation destination name, the resume receipt, and a bounded sweep of the
older files under that name. That test is kept as a dependency contract, since
the whole warm-path design rests on it.

Slice 4 is now implementation-ready rather than design-blocked: the plan carries
the six ordered steps (open-existing wrapper, receipt module, activation rewrite,
activation-database enum, receipt write on clean close, sweep), and the naming
basis is verified — `Control::takeover` increments the epoch and no other
transition changes it, so one epoch is one ownership session and `<stem>.e<epoch>`
is a path no other session can use. Each step is independently verifiable, and
the receipt deliberately excludes the ownership epoch from its match.

Slice 4's implementation attempt then found the last real blocker and was
reverted cleanly: the warm path failed the executor's open verification with
`restored SQLite position does not match root`, because a plain
`Db::open_with_host` starts an *unseeded* capture session and crab-ltx exposes
no public way to seed one. Warm reuse therefore needs a public "open seeded"
API plus a local source for the continuation — position, page size, and page
count fit in the receipt, but the page-checksum index exists only after a
restore, so its home is a crab-ltx decision. The rename mechanism, the
fresh-path observation, the receipt design, and the verified fallback all
stand; the fallback path passed its test during the attempt.

Slice 4 landed on 2026-09-24 with the capability that attempt needed:
`Db::persist_continuation` writes the dense page checksums and the continuation
record beside the database, and `Db::open_resumed_with_host` seeds a fresh
capture session from them. The runtime writes one fixed-width resume record per
released database, consumes the record that still matches the observed control
(discarding every other one with the file it names), and moves the database onto
the fresh activation path, so a same-node wake reads no origin object at all:
`a_warm_wake_continues_the_local_database_without_the_origin` records zero origin
requests and exactly `[ownership, resume, activate]`, and
`a_resume_record_that_names_another_root_is_discarded` fails against an
always-matching record and passes with the fence restored. Two writer-side
fences keep it honest: a database whose WAL is not checkpointed is refused, and a
sparse activation must be fully materialized, because an unfaulted page is a
hole rather than data. Both `crab-ltx` and runtime tests cover the refusals.
Still open on this slice: charging the dormant window to the disk ledger,
dormant residency (holding ownership across the shed), and the product-level
adoption step that would let the slot survive a process restart.

## Continuous Cell runtime hardening

[035](035-cell-runtime-continuous-hardening.md) is the executable program for
durability seam proof, measured resource admission, hot-Cell cost, hydration and
offline retention, framework reuse, and continuing release gates. It consumes
the unfinished work in plans 009, 012, 015, 024, 025, 032, and 034 instead of
reimplementing their owners. Execute each slice as a separate reviewable change
and update its ledger in the plan after its named evidence passes.

| Plan | Priority | Depends on | Status |
| --- | --- | --- | --- |
| [035](035-cell-runtime-continuous-hardening.md) | P0 safety/capacity; P1 optimization | Plans 009, 012, 015, 024, 032, 034 as named by slice | IN PROGRESS — local durability, hydration/retention, resource-accounting, seven-Cell/two-shard reuse, and CI proof expanded; protected qualification still required |

## Cell read replicas and promotion

[036](036-cell-read-replicas-and-fenced-promotion.md) proposes a variable
number of S3-rooted read replicas on live nodes. An acknowledged write in this
profile must reach the exact S3 control root, so loss of every reader does not
discard acknowledged state. A replacement reader restores from S3, and a
successor becomes the only writer through the existing new-epoch takeover CAS.
The current server still routes reads only to owners. The library now has a
full-restore read-only snapshot opener, a gated typed query path, and an S3
desired-count record. Placement, routing, an S3-only acknowledgement profile,
and production qualification remain open.

| Plan | Priority | Effort | Depends on | Status |
| --- | --- | --- | --- | --- |
| [036](036-cell-read-replicas-and-fenced-promotion.md) | P1 read scaling / P0 safety | XL | 032 and 035 recovery implementation; their protected gates before production enablement | PARTIAL — local snapshot query and desired-count store; product wiring and qualification TODO |
