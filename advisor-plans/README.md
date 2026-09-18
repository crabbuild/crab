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
| [008](008-resident-cell-local-routing.md) | Zero-metadata-I/O local handle acquisition for safe resident Cells | P0 | L | 004, 005 | IN PROGRESS |
| [009](009-background-hydration-and-resident-promotion.md) | Bounded sparse hydration and verified resident promotion | P0 | L | 008 | IN PROGRESS |
| [010](010-streaming-cell-ltx-publication.md) | Bounded-memory native and bundle publication | P0 | XL | 005 | IN PROGRESS |
| [011](011-persistent-directory-node-cache.md) | Restart-persistent verified directory acceleration | P1 | L | 010 | IN PROGRESS |
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
requires a real provider, Kubernetes fault, measured multi-GiB RSS run, or an
authorized standalone-contract decision.

Local proof completed:

- `crab-cell-runtime`: 207 library tests passed (one provider test ignored),
  38 actor tests passed (one provider test ignored), and all primitive,
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
- `crab-ltx --features replica`: 45 unit tests, 75 integration tests, and 5
  doctests, plus the new streaming, cache restart, concurrent-fill, and
  fault-injection coverage pass; the ignored RustFS round trip also passes
  against the isolated local provider.
- `crab-ltx --no-default-features`: 12 unit tests, 21 integration/doc tests,
  and 5 doctests pass, so the standalone/minimal feature boundary remains
  buildable.
- `crab-http-server --lib`: 190 tests pass (four provider/browser tests remain
  explicitly ignored); the local RustFS collaboration/takeover, native-push,
  and receive-fault qualifications pass when run with an isolated prefix. The
  architecture target, architecture regression tests, Clippy, formatting,
  documentation validation, the 512-seed simulator corpus, exact-seed replay,
  and TLC fast/negative/broad safety plus fair stable-provider liveness checks
  pass.

The in-repo placement path now consumes the signed observation block for cold
activation: the planner selects a live eligible session, sends one authenticated
activation hint, and the destination enters through the existing router,
authority CAS, and actor admission. Advertised free memory, disk, and job
headroom is conservatively clamped by the same runtime reservations used for
admission. Persisted Queue/Workflow rows are
re-inspected after durable work and an unknown result remains ineligible for
eviction. Remaining release gates are recorded in plans 009–017:
post-promotion zero-origin measurement, multi-GiB/RSS and provider-failure
matrix evidence (with one local fail-first immutable PUT proof now covered),
complete process-wide resource accounting, multi-process
movement/fault proof, protected Kubernetes receipts, and the named plan-016
retain/deprecate/hard-remove decision. Plan 017 remains correctly blocked; no
standalone export or stored prefix was removed.

The signed placement snapshot now has an end-to-end local provenance check:
`NodePublisher` publishes while holding one runtime ledger byte/job reservation,
and the peer test verifies that the signed memory/disk totals and Cell/job
counts come from that coherent runtime sample while free headroom remains
clamped. Nested cgroup fixture parsing and process file-capacity checks cover
the fail-closed host probe; process-wide parity and multi-process convergence
remain qualification work.

Scheduler maintenance is also ledger-visible: migration and node-log recovery
tasks now retain a `NodeJobReservation` until their spawned futures finish,
alongside the existing per-cell/session guards. This closes the untracked
background-job path without adding a second capacity owner; process-wide
codec/dirty/scratch reconciliation and restart inventory are still explicit
Plan 012 qualification gates.

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
