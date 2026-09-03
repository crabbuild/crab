# Plan 018: Close Crab's product-readiness gaps

## Status and intent

- Status: DESIGN READY FOR REVIEW; implementation acceptance remains open.
- Baseline: `63bfc8c`, inspected with the uncommitted working tree on 2026-09-02.
- Priority: P1 for safety, cache availability, and release evidence; P2 for
  performance improvements beyond the supported workload envelope.
- Scope: CLI/remote-helper product, local caches, storage contracts,
  operational recovery, and the optional protected-read/cache-service surfaces
  that a release explicitly advertises.
- This is an executable coordination plan, not another storage architecture.
  Existing subsystem plans remain the implementation owners.

Crab already has the core mechanics: chunk deduplication, shard/xorb storage,
verified reconstruction, transactional staging, writer coordination, GC
fencing, diagnostics, and release tooling. The next product milestone is
**a bounded, recoverable, explainable system with evidence for every supported
provider, platform, and user journey**. More cache layers are not the priority.

This document is a source-backed design review, not a fresh production audit.
No live providers, credentials, CI run history, or release artifacts were
queried for this refresh. Historical passes/blockers cited below come from
tracked evidence ledgers; they must be revalidated at the release candidate.
Security scenarios listed for qualification are not assertions of discovered
authorization vulnerabilities.

Current local implementation exception to that historical-evidence statement:
the descriptor-bound SQLite owner now passes both original root-replacement
regressions unchanged. Native macOS WAL, writer-exclusion, and recovery
fixtures also pass. The attributed maintenance read-to-write race is fixed;
the reservation regression passes 200 repetitions. These are test-owned
fixtures, not installed-product or full security qualification. Plan 017's
[database lifetime execution slices](017-local-cache-read-hardening.md#database-lifetime-execution-slices)
record exact commands, observed results, and the mitigation contract. Treat
the remaining caller/root lifetime, inspection, accounting, and platform work
as release gates; do not repeat the repaired defect as current behavior.

## How to execute this design

Use this document for product priorities; use Plan 017 for cache/read changes
and the linked GC/provider/scale plans for their respective implementations.
Each phase below provides context, work, acceptance criteria, proof, and STOP
conditions. The dependency/owner ledger and kickoff queue define ordering.
Subsystem ownership is assigned; human DRIs and environment access are not.

The first delivery cut is **safe local state and predictable failure**. Close
the remaining SQLite caller/lifecycle gates, VFS window-cache integrity/failure gaps,
and remaining cache-only error propagation before performance optimization.
Then complete disk/memory admission and diagnostics. Recovery/provider tests
can be prepared in parallel; release acceptance waits for the integrated
candidate, packaged-CLI proof, and the declared support matrix.

Before implementation kickoff, resolve these inputs in the owning ledger:

| Decision | Owner | Required output / acceptance |
|---|---|---|
| Supported product envelope | Product + release | Explicit journeys, providers, OSes, scale limits, and experimental exclusions; every advertised cell has a DRI and runnable fixture. |
| SQLite lifetime qualification | Cache + storage reviewer | Review the implemented descriptor-bound owner against caller/side-file/locking contracts; retain independent and native-platform proof. Connection tests alone do not accept reservation/root integration or non-mutating inspection. |
| Tagged read/VFS API compatibility | Read/VFS maintainers + product owner | Explicit source-compatibility decision before changing tagged signatures; preserve CLI and remote formats. Follow Plan 017's open decision. |
| Resource thresholds and test environments | Subsystem DRIs + release | Fixed limits with units, hardware/fixture identity, isolated provider scope, and access to each required native OS before qualification starts. |

These inputs do not authorize deployment, dependency patches, destructive
provider operations, or the runtime implementation merely by accepting this
document. No calendar estimate substitutes for a phase's exit criteria.

## What local caching already does

| Layer | Current role | Boundary to preserve |
|---|---|---|
| Decoded xorb-range cache | Reuses decompressed chunk ranges for hydration. `crates/crab-cache/src/xet_chunk_cache.rs` implements the pinned Xet `ChunkCache` contract in the working tree. | A warm payload read can avoid xorb body downloads; it does not automatically remove metadata or authorization requests. |
| Full-xorb cache | Stores verified complete bodies for callers that intentionally install them; also supplies indexed local ranges. See `crates/crab-cache/src/local_cache.rs`. | Ordinary hydration need not install a duplicate complete xorb. |
| Shard/object cache | Reuses immutable reconstruction metadata and other classified objects through `crates/crab-cache-store/src/lib.rs`. | Mutable refs/manifests retain their origin freshness/CAS rules; a cache is not publication authority. |
| Dedup lookup | Memory → optional local SQLite → remote committed-candidate lookup in `crab/src/metadata/metadb/stores/chunk_index.rs`. | Local hits are candidates, not proof that remote bytes are committed and readable. Preserve generation invalidation and remote proof validation. |
| Remote xorb proof/index | Avoids repeated metadata/proof work; consumed by `crab/src/git/push.rs`. | Do not remove these live records when removing unused local-placement tables in the same database. |
| Shard hints | Advisory file-to-shard shortcut in `crates/crab-cache/src/shard_hints.rs`. | JSON read-modify-write remains; transactional, storage-scoped hints are Plan 017 work. |

The local cache is therefore present. The open questions are consistent use,
failure isolation, ownership, total resource cost, and observable behavior.
[Plan 017](017-local-cache-read-hardening.md) contains the detailed design and
separates its original findings from partial working-tree implementation.

Dependency proof: `Cargo.lock` selects Xet 1.6.0; its
`xet-client/src/chunk_cache/mod.rs::ChunkCache` contract uses decoded bytes,
half-open chunk-index ranges, and offsets beginning at zero and ending at
data length. Eviction is permitted after a put. Preserve this trait contract;
do not assume upstream cache storage policy is a Crab product guarantee.
Provider write identity is owned by `crates/crab-storage/src/store.rs`:
`ETag` is `object_store::UpdateVersion`, and create/update paths use distinct
conditional-write modes. Provider qualification must prove that contract on
the actual service, not infer it from an emulator pass.

## Gap register and priority

“Implementation gap” means a source-level gap identified here. “Evidence gap”
means the reviewed material does not establish acceptance, not that the
feature is absent or necessarily broken.

| Gap | Classification and evidence | Mitigation owner | Release impact |
|---|---|---|---|
| SQLite lifetime/root replacement | Partial mitigation: `crates/crab-cache/src/private_fs/platform/database.rs` now retains a descriptor-bound owner through close; both original regressions pass unchanged. Maintenance's deferred read-to-write race is also fixed. Plan 017 records focused macOS proof and implementation limits. | Phase 1 / Plan 017 Phase 4 database slices | Keep full caller/root identity, owner cleanup, non-mutating inspection, and native-platform qualification gated. A repaired connection-level defect is not complete maintenance acceptance. |
| Incomplete cache lifecycle and qualification | Implementation in progress: the shared read builder owns range-cache attachment and inline/delayed reconstruction. Operation-local owners now await Xet's detached cache-write attempts before success and cancel pending puts on cancellation/drop; the reopened warm-read regression passes unchanged. Catalog lifecycle, all-family coverage, and separate-process/provider proof remain unfinished. Plan 017 working-tree snapshot. | Phase 1 / Plan 017 | Core availability and disk-safety gate. |
| VFS cache ownership | Partial implementation: chunk-cache initialization now degrades without alternate storage; private-creation, restart, bad-chunk repair, and real warm-range tests pass. The separate file-window cache still has pathname I/O, local-failure propagation, and a verify-once shortcut; VFS-specific roots/capacities are not consolidated. Plan 017 records exact proof and remaining gates. | Phase 1 / Plan 017 Phases 1–4 | Startup acceptance is not complete mounted-read assurance. Close window failure/identity/lifetime and shared accounting, then qualify actual native mounts. |
| Live/retained state under the cache root | Implementation gap: maintenance workspaces, mirrors, and profiles have cache-root consumers in `crab/src/cmd/repack.rs`, `crab/src/cmd/mirror.rs`, and `crab/src/core/tracing_init.rs`. | Phase 1 / Plan 017 | Do not broaden automatic eviction until ownership is resolved. |
| Recovery and destructive maintenance proof | Evidence gap: the GC and scale ledgers retain open race, restart, high-cardinality, provider, and retention gates. GC harness/workflow exist. | Phase 2 / GC plans | No destructive-GC readiness claim without its complete writer matrix. |
| Provider parity | Evidence gap: Plan 010 records absent retained real AWS/GCS/Azure proof. Provider workflow and strict report verifier exist; current run status was not queried. | Phase 3 / Plan 010 | Qualify each advertised provider independently. |
| Private-cache and protected-boundary assurance | Local implementation in progress: Unix payload I/O and inventory pin descriptors; clean/verify/prune share a broad-root guard. Catalog/xorb-index connections pin side-file operations through close. Catalog maintenance, fill publication/registration, and owner release now retain one root; shared payload leases protect the publication handoff. Main-file replacement inside that root, other index owners, non-mutating health, and native Windows ACL support remain open. Service authorization needs release-bound negative proof, not a new auth design. | Phases 1 and 3 | Required for the corresponding private-data surface. |
| Error attribution across owners | Partial implementation: source-specific xorb attempts and the Xet adapter now retain provenance and typed sources. Actual shared/CLI reconstruction tests cover origin integrity, availability denial, writer I/O, cancellation, and unchanged atomic destinations. Remaining gaps: broader shared-error diagnostic classification, family fault coverage, and real restore/service qualification. Plan 017 retains the proof. | Phases 1 and 4 | Recovery must distinguish damaged cache, invalid origin, restore availability, and cancellation end to end. |
| Incomplete local diagnostics | Implementation gap: `crab/src/cmd/doctor.rs::check_cache` reports existence/size. Cache-service support bundles already exist, but that does not establish local read/cache health coverage. | Phase 4 / Plan 017 Phase 5 | Users need a safe explanation and recovery action without maintainer access. |
| Sustained scale and cost envelope | Evidence gap: the large-repository roadmap distinguishes single-host/RustFS successes from pending growth, distributed fanout, failover, retention, and canary gates. | Phase 5 / large-repository roadmap | Gate scale claims; do not block a deliberately smaller, qualified envelope on speculative scale. |
| Read-memory admission | Partial implementation: retained xorb results own/charge small slices and keys; release, speculation, Crab-to-LFS conversion, and protected-view repacking stream. Shared output allocation is checked/fallible, exact sizes are enforced, and operation-owned buffers/writers close on cancellation. Configured output admission, aggregate decode, queued output, caller-held results, and temporary disk remain unqualified. | Phase 1 / Plan 017 Phase 6 | Fallible allocation is not a cap. Resolve the tagged Vec-returning API's lifetime-accounting contract, then prove limits before allocation and qualify malformed/highly overlapping requests. |
| Evidence-to-release enforcement | Source-level gap in default policy: `.github/workflows/release.yml` permits selected cloud/platform and enterprise evidence jobs to be skipped. Repository variables can enable them; their deployed values were not inspected. | Phase 6 | A support claim must select mandatory proof, not rely on an operator remembering a switch. |

## Target ownership and non-goals

Keep these boundaries, not a new central service:

- `crab-read`: canonical selection, reconstruction, verification, cancellation.
- `crab-cache-store`: cache-versus-origin routing and cache-only failure policy.
- `crab-cache`: disposable local state, budget, tenancy, leases, health.
- `crab-staging` / `crab-coordination` / `crab-metadata`: durable publication,
  authoritative state, writer/GC protection, recovery contracts.
- `crab-storage`: provider identity, bounded I/O, conditional writes, retries.
- CLI and server composition: credentials, authorization, selected store,
  availability/restore policy, command output, process lifetime.
- Existing qualification scripts and release workflow: retained proof and
  support-claim enforcement.

Non-goals: new cloud formats; a mandatory Crab data server; new desktop/SDK
products; a second cache daemon; predictive prefetch; blanket refactors;
compatibility shims for disposable development state; new configuration
switches merely to hide incomplete behavior. A future optimization enters
scope only with measured impact and a named owning implementation.

## Execution and acceptance ledger

Owner means subsystem responsibility, not an assigned person. Assign a human
DRI at kickoff. Use small dependency-ordered PRs within each phase; do not put
this entire roadmap into one implementation PR.

| Phase | Outcome | Owner | Prerequisite | Acceptance status |
|---|---|---|---|---|
| 0 | Freeze supported journeys and evidence inventory | Release + subsystem maintainers | — | OPEN |
| 1 | Safe, bounded, consistent local reads | Read/cache | Phase 0; reuse partial Plan 017 work | OPEN |
| 2 | Recoverable publication and maintenance | Staging/metadata/coordination | Phase 0; current Plans 011–016 contracts | OPEN |
| 3 | Provider and private-data boundary qualification | Storage/auth/security | Phase 0; Phases 1–2 for end-to-end rows | OPEN |
| 4 | Self-service diagnosis and safe recovery | CLI/operations | Phase 0; health/error contracts from 1–3 | OPEN |
| 5 | Bounded resource and performance envelope | Read/write/maintenance | Correctness gates from 1–3 | OPEN |
| 6 | Evidence-gated release and canary | Release + operators | Required rows from 0–5 | OPEN |

Phases 1 and 2 are independent tracks after Phase 0. Provider fixtures and
diagnostic design can start alongside them, but final acceptance must run
against the integrated candidate. Existing “DONE” subsystem work is preserved;
it needs requalification when an overlapping invariant changes, not rewriting.

### First implementation slices

This is the kickoff queue, not a second implementation plan. Phase numbers
below refer to this roadmap; Plan 017 retains its own phase numbering. Each
slice updates the owning ledger and can be split further at an owner boundary.
No slice is accepted by compilation alone.

| Slice / owner | Context and deliverable | Dependency | Exit criterion |
|---|---|---|---|
| Support/evidence inventory / release | Existing proofs cover different candidates and environments; bind required journeys to one candidate matrix. | Phase 0 kickoff | Every advertised cell has a DRI, fixture, validator, and explicit status; missing/wrong-SHA proof is rejected. |
| Read semantics / read and CLI | Assembly is centralized and injected restore errors retain their identity; complete command and real restore qualification remain open. | Inventory | All read consumers are mapped; separate-process fetch → hydrate passes byte comparison with zero warmed xorb body GETs; restore/cancellation classifications survive. |
| Cache degradation / cache-store | Xorb body/metadata attempts now retain provenance. Extend that proof to generic bodies, index/hint failures, and reconstruction instead of replacing the verified xorb path. | Read semantics | Fault table passes for read/write/index/eviction failures; valid origin succeeds, invalid origin fails, and repair does not install redundant full xorbs. |
| Filesystem and state ownership / cache | Object/range maintenance uses private payload access; catalog and fills retain one root through publication and release. Shared payload leases protect the registration handoff. Main-file replacement, remaining index owners, live state, and broader lifecycle proof remain open. Follow Plan 017's remaining slices. | Inventory; degradation policy | One root identity covers database, inventory, and deletion; parent-swap/active-owner tests cover every maintenance family; live/retained/unknown sentinels survive; each supported OS has native private-access proof. |
| VFS integration / VFS and read | Chunk startup degradation, private fixtures, and real shared-hydrator ranges now pass. Complete duplicate file-window ownership, failure isolation, and native mount qualification without weakening private validation. | Read semantics; filesystem/state ownership; tagged-API decision for interface consolidation | Same-length post-warm corruption, failed window writes, path swaps, and cancellation cannot corrupt or block valid-origin reads. Live state survives; cached/uncached paths and the consolidated budget pass on native supported runners. |
| Capacity and health / cache and CLI | Catalog admission, owner cleanup, and diagnostics remain partial; finish the shared lifecycle and read-only health contract. | Filesystem/state ownership | New fitting writes displace eligible older entries; quiescent bytes meet Plan 017; no SQLite side-file loss or catalog recreation on Drop; inspection causes no mutation. |
| Dedup locality and concurrency / metadata and cache | Global JSON hints lose updates; unused placements coexist with live proofs; cross-process fills remain open. | Capacity/health; current committed-placement contract | Scoped writers retain both updates; remote proof remains validated; eight cold processes coalesce normally and recover after owner death. |
| Qualification / subsystem and release owners | Local fixtures cannot establish real-provider, packaged-CLI, or sustained-operation support. | Integrated safety/correctness slices and Phase 2 recovery gates | Existing verifiers accept exact-candidate provider/OS and recovery reports; Phase 6 release gate rejects absent evidence. |

Start provider access/environment preparation during inventory. Do not defer
discovering an unavailable provider or OS until the final release step. Those
cells remain open while independent implementation proceeds.

### Delivery cuts and prioritization

1. **Safety and availability first:** preserve typed reconstruction failures;
   close cache-only failure isolation; protect live state and private filesystem
   access before enabling wider eviction. Reuse the partial Plan 017 work.
2. **Predictable operation next:** finish capacity admission, read-memory
   bounds, process coordination, scoped hints, and non-mutating diagnostics.
   Every new limit needs a stated unit, owning boundary, and rejection/bypass
   behavior; a disk-cache budget is not a reconstruction-memory budget.
3. **Release proof before support claims:** run recovery, provider, OS, packaged
   CLI, and canary gates against one candidate. An excluded feature is omitted
   from the support matrix, not silently counted as passing.
4. **Optimization after measurement:** improve sparse cold-read amplification
   and repeat dedup cost only when the baseline identifies them as bottlenecks.
   No extra cache layer or weakening of remote placement proof is assumed.

These are dependency cuts, not calendar estimates. At Phase 0 kickoff assign
DRIs, isolated environments, and workload thresholds; only then estimate each
bounded slice. No phase is accepted by its implementation checklist alone.

## Phase 0: Freeze the supported contract and evidence inventory

**Context.** Many strong tests and plans already exist. They cover different
commits, providers, workloads, and abstraction levels. A green unit test or a
manually injected cache handle cannot prove the installed CLI journey.

**Work and deliverables.**

1. Inventory installed journeys: init/add/commit/push; clone/fetch/hydrate;
   checkout/filter; dedup reuse; repo-scoped maintenance and recovery. Add
   mount, protected views/pushes, replication, workflow, and remote cache only
   where the candidate claims support.
2. Record each journey's entry point, owner, authoritative state, caller,
   callee, sibling surfaces, tests, provider/OS requirements, and evidence ID.
   Reconcile tracked ledgers with current source and working-tree drift.
3. Freeze one candidate support matrix and workload limits. Label each cell
   qualified, experimental, unsupported, or blocked; do not collapse them to
   one “S3 compatible” checkbox.
4. Reuse existing report validators. Add only a small release index that links
   their reports by candidate SHA, artifact digest, provider/OS, fixture,
   dependency versions, and required result. No second telemetry backend.
5. Audit release tags before applying the no-compatibility premise. New real
   user data or a shipped contract requiring preservation stops a hard cutover.

**Acceptance criteria.**

- Every advertised journey has an owner and a reproducible Level 3+ test:
  actual command → real side effect → independently checked result.
- Every evidence cell is explicit; missing or stale evidence is not a pass.
- Candidate and fixture identities are recorded; a report for a different SHA
  cannot silently qualify this candidate.
- Resource thresholds are chosen before runs, with units and a fixed fixture;
  no post-failure threshold relaxation or mock-scale claims about payload I/O.

**Proof.** Reviewed support/evidence matrix, validator negative fixtures for
missing/wrong-candidate reports, and one installed-CLI happy-path smoke.

**STOP.** A journey cannot identify its authority, support scope, or isolated
test environment. Resolve that before changing runtime behavior.

## Phase 1: Finish cache/read hardening

**Context.** Plan 017 already owns eight detailed implementation steps. The
working tree has partial canonicalization, failure handling, capacity, and
Unix permission work. Do not count these as accepted merely because they exist.

**Work and deliverables.**

1. Complete Plan 017 in its dependency order; consolidate cache attachment and
   preserve restore/cancellation behavior across every read consumer.
2. Close its ownership audit before pruning: authoritative staging, mirrors,
   active outputs, retained profiles, and evidence are not generic cache files.
3. Complete Plan 017's database lifetime slices before enabling broad
   destructive maintenance. Preserve the implemented connection/side-file
   owner; finish caller/root identity, cleanup, and non-mutating inspection.
   Connection-level root-swap tests alone are insufficient. Account for reservations,
   temporary copies, and the catalog itself.
4. Preserve origin-bound dedup proof, replace global JSON hints with scoped
   transactional updates, and delete only genuinely unused local placements.
5. Follow Plan 017's [read-memory execution slices](017-local-cache-read-hardening.md#read-memory-execution-slices):
   migrate whole-file consumers to verified streaming before introducing
   in-memory admission limits. Then qualify decode, queued output, caller-held
   results, and temporary disk as separate resource owners. A new limit must
   preserve the supported large-file journeys, not silently shrink the product.
   The existing checked/fallible buffers and exact-length guards are partial
   safeguards, not completed admission. Plan 017 records the open tagged-API
   return-type decision and tests required for retained-result ownership.

**Acceptance criteria.**

- Separate-process fetch → hydrate returns identical bytes with zero origin
  xorb body GETs for warmed ranges; report metadata/auth requests separately.
- Cold ordinary hydration does not install a redundant full-xorb body.
- Corrupt, full, unavailable, or unsafe cache state cannot block valid origin
  data; invalid origin data still fails closed.
- Root replacement cannot redirect database, journal, WAL, or SHM writes or
  cleanup. Both original regressions continue to pass unchanged, and the owner
  survives native lock, read-only inspection, cancellation, and restart tests.
- One effective root/budget covers disposable local families; active users
  survive pruning; fresh working sets can displace older entries.
- Eight cold processes coalesce the same normal-path xorb fill; cancellation
  and owner death do not strand waiters or publish partial files.
- All Plan 017 global criteria pass, including OS tenancy and diagnostic proof.

**Proof.** Plan 017 origin counters, multi-surface CLI tests, filesystem
sentinels, fault/kill tests, million-entry proxy, and provider/OS artifacts.
Pinned Xet trait conformance and minimal/changed feature builds are required.

**STOP.** Cache cleanup can affect authoritative data, authorization is
bypassed, or a new cache representation is proposed without measured benefit.

## Phase 2: Close recovery and destructive-maintenance gates

**Context.** Plans 011–016 record delivered staging/publication invariants.
GC has fences, journals, closures, and an executable RustFS harness. The gap
is the complete interaction and recovery proof, plus the boundedness/retention
work explicitly left open in the GC and large-repository ledgers.

**Work and deliverables.**

1. Execute the existing GC plans: [fencing](001-close-writer-gc-fence.md),
   [bounded run engine](002-durable-bounded-gc-engine.md),
   [closures](003-persist-shard-closures.md), and
   [qualification](005-production-qualification.md). Reconcile older repair
   and compatibility instructions with the root cutover policy first.
2. Inventory direct/protected/coordinated push, repack, restripe, recovery,
   replication, and workflow writers. Each publication boundary must be
   protected during maintenance, or the corresponding operation stays gated.
3. Extend existing crash hooks around publication, manifest CAS, journal
   outcomes, delete batches, lease renewal, and restart. Prove idempotent
   recovery from durable state; preserve acknowledged results.
4. Complete unresolved materialized joins and historical-root/pack retention
   work in the owning plans. Inventory-backed deletion remains separate and
   unavailable until strict source completeness is proven.
5. Write one operator recovery sequence per failure class using current
   commands: inspect → identify exact scope/run → resume or repair → verify.

**Acceptance criteria.**

- No acknowledged push loses a ref or referenced payload across injected
  crashes; retries do not republish inconsistent metadata or duplicate owners.
- Every advertised writer/maintenance race either waits safely, protects the
  object, or fails closed. Grace and force never bypass reference protection.
- Each kill point has a deterministic resumed/failed-closed result; all
  acquired locks and SlateDB instances have verified exit-path ownership.
- Post-scenario fsck plus fresh clone/hydrate matches original refs and hashes.
- Resource use stays within the predeclared limits; historical data required
  for supported recovery survives maintenance.

**Proof.** Existing add/push regression suite, GC crash/race harness and
validator, workflow crash tests, isolated provider runs, and retained recovery
transcripts. Repeat timing-sensitive races with at least ten scheduling seeds.

**STOP.** A root/writer is missing from the inventory, a lease is lost without
failing closed, or cleanup requires a shared bucket. Never run bucket-wide GC
for this qualification; follow repository isolation policy.

## Phase 3: Qualify providers and private-data boundaries

**Context.** `crab-storage` exposes one provider contract, but service behavior
must be proven separately. Auth helpers and cache-service authorization already
exist; qualification must establish that caches and degraded reads do not
cross those boundaries. This is not a request to replace authentication.

**Work and deliverables.**

1. Complete [Plan 010](010-provider-qualification-v1-cutover.md) using
   `crab/tests/provider_qualification.rs`, the provider workflow, and
   `crab/scripts/verify-provider-qualification-report.py`.
2. Retain actual-service create-only, match-token, multipart completion/abort,
   cancellation, exact-range, pagination, retry/error, and receipt results.
   Record emulator and real-service evidence as different classes.
3. Trace request admission through auth/view/cache-service callers before
   adding negative tests: expired/rejected credentials, denied paths, scope
   mismatch, foreign repository hints, and service outage with origin fallback.
4. Qualify local owner permissions and path-swap resistance on every supported
   OS, reusing Plan 017. Exercise service certificate/token rotation only where
   that deployment mode is in the support matrix.
5. Verify diagnostic/report redaction using seeded secrets and private path
   fixtures. Document that revoking remote access cannot erase bytes already
   legitimately downloaded by the same OS user.

**Acceptance criteria.**

- Every supported provider passes the existing strict report verifier on the
  candidate; missing credentials produce blocked/unsupported, never a pass.
- A losing conditional writer cannot overwrite the winner; cancellation and
  retry preserve the storage contract without accepting incomplete payloads.
- Denied protected requests disclose no new unauthorized bytes through a
  cache, hint, fallback, or view-generation path. Existing offline possession
  is distinguished from a new authorized network request.
- Unsafe local roots are bypassed; cache writes cannot escape their root.
- No seeded credential or repository payload appears in logs/support reports.

**Proof.** Provider contract reports plus end-to-end canaries; negative auth
and scope tests at public entry points; Linux/macOS/Windows private-cache
fixtures; security review of owner boundaries and redaction.

**STOP.** No isolated provider scope, no no-follow/ACL enforcement for an
advertised platform, or unreviewed authority expansion in fallback behavior.

## Phase 4: Make diagnosis and recovery self-service

**Context.** Doctor, typed errors, JSON output, and cache-service support
bundles already exist. Local cache inspection is incomplete. A real product
must tell users whether to retry, repair derived state, restore credentials,
or preserve data and seek help—without requiring log archaeology.

**Work and deliverables.**

1. Reuse Plan 017's shared cache health model in stats, verify, and doctor.
   Inspection must not create databases or mutate a missing/damaged root.
2. Audit error propagation on the critical journeys. Preserve source errors
   while classifying cache degradation, origin integrity, authorization,
   cancellation, stale writes, and unavailable replicas at the CLI boundary.
3. Extend existing structured output/support surfaces only where needed; do
   not introduce a second diagnostic command tree. Report effective policy,
   source counts, scope-safe identities, and recovery actions.
4. Validate first-use docs, configuration examples, install/helper discovery,
   cache-root relocation, and recovery instructions with clean environments.
   Reject unknown configuration rather than silently accepting ineffective keys.

**Acceptance criteria.**

- Fixtures for missing credentials, permission denial, cache corruption,
  disk-full, missing origin object, stale CAS, and cancellation each produce
  the expected exit status and one actionable next step.
- Inspection leaves a missing root absent and healthy families visible when
  another family fails. Repair requires explicit action and exact scope.
- JSON consumers distinguish partial/degraded/failed results; success never
  means a pointer was hydrated when its destination was not verified.
- A maintainer unfamiliar with the implementation completes each recovery
  drill using only shipped docs and redacted diagnostic output.

**Proof.** `crab/tests/error_codes.rs`, CLI output tests, doctor/cache tests,
filesystem before/after checks, and recorded clean-install/recovery drills.
Documentation config/link tests accompany behavior changes.

**STOP.** Guidance can delete authoritative state, output exposes secrets, or
an error's source is lost before the boundary can classify it correctly.

## Phase 5: Prove the resource envelope, then optimize measured bottlenecks

**Context.** The large-repository roadmap has substantive Kubernetes/RustFS
proof, but explicitly leaves sustained growth, distributed fanout, provider
faults, owner failover, retention, and canary criteria open. Cache hits alone
also do not prove good cold-read cost or bounded startup.

**Work and deliverables.**

1. Complete the outstanding gates in
   [the large-repository roadmap](001-large-repository-scale-roadmap.md),
   reusing its fixtures, request counters, report verifier, and SLOs.
2. Measure cold/warm fetch and hydrate, sparse reads, repeat add/push, many
   small files, multi-file overlap, and 1/8/100-client workloads where supported.
   Separate real payload runs from metadata-only scale proxies.
3. Track origin request/byte counts, read/write amplification, p50/p95 latency,
   CPU, peak RSS, descriptors, queue depth, cache-root bytes, and retained remote
   bytes. Record dependency, hardware, network, fixture, and configuration.
4. Rank improvements by measured cost: e.g. sparse cold reads currently use
   complete-xorb downloads in the non-installing path. Before designing more
   selective reads, prove footer/range/chunk integrity against pinned Xet and
   storage contracts. Do not weaken verification to improve a benchmark.
5. Admit an optimization only if it improves the chosen metric without
   violating safety/resource thresholds. Delete the replaced path.

**Acceptance criteria.**

- The declared supported envelope passes the owning plans' fixed thresholds
  and correctness checks on two reproducible runs; host contention invalidates
  a timing comparison rather than explaining away a failed result.
- Million-entry cache startup remains bounded and leaves the async runtime
  responsive; 10,000-push growth/retention gates pass before that scale is claimed.
- Shared cold requests coalesce under normal timing; unrelated work retains
  concurrency; lock timeout/cancellation cannot become an availability outage.
- One-client and team-load resource/report fields are complete. No throughput
  or cost claim is inferred from a metadata-only proxy or a warm-only run.

**Proof.** Existing scale verifiers, Plan 017 cache proxies/counters, fault and
owner-failover runs, exact refs/fsck/hash checks, and before/after profiles.

**STOP.** No baseline, changed fixture, unbounded queue/map/scan, or an
optimization that removes integrity or authorization checks.

## Phase 6: Gate the actual release and complete a canary

**Context.** The release workflow already checks versions, archive contents,
native workflows, protocol behavior, checksums, and build provenance. Selected
cloud/platform/enterprise jobs remain optional. The gap is a mandatory link
between the candidate's advertised support and its verified evidence.

**Work and deliverables.**

1. Make Phase 0's support matrix select required release gates. Skipped,
   missing, expired, wrong-SHA, wrong-platform, or emulator-only evidence cannot
   satisfy a required real-provider row. Optional features may remain excluded
   only when release notes and support docs explicitly exclude them.
2. Reuse existing report validators and attestation/archive/install checks.
   Retain verified reports with the release identity; define retention that
   outlasts the advertised support period instead of relying only on ephemeral
   CI artifacts.
3. Run clean install → helper discovery → add/push → fresh clone/hydrate →
   restart → doctor on packaged artifacts for each supported OS. Do not
   substitute a source-tree debug binary for installed-product proof.
4. Resolve required format/build/lint/test failures on the candidate. Old
   ledgers are not current test results; do not suppress or rewrite baselines
   to manufacture a green release.
5. Run a one-week opt-in canary, or an explicitly approved equivalent sustained
   qualification window. Include process restart, network loss, disk pressure,
   maintenance overlap, and operator recovery drills.
6. Publish delivered limits and a stop-release/incident procedure. Before first
   real user data, explicitly establish the shipped-format support policy;
   do not let the pre-user destructive-cutover assumption persist implicitly.
   Do not add speculative legacy readers or blindly downgrade stored formats.

**Acceptance criteria.**

- Automated negative tests prove publication is blocked for every missing or
  mismatched required evidence class—even when a manual flag is omitted.
- Each shipped artifact maps to the verified SHA, version, digest, platform,
  and proof reports; installer/helper/archive/provenance checks pass.
- Canary has zero byte-identity, ref-durability, unauthorized-disclosure, or
  referenced-object-deletion failures; performance stays in the declared envelope.
- Operator drills succeed with retained evidence and published guidance.
- Release notes, website, help, defaults, and provider/platform labels describe
  only the accepted support matrix. Open exclusions remain visible.

**Proof.** Release-gate validator tests, packaged-binary smoke reports,
canary report, recovery transcripts, and the signed/provenance-bound release
artifacts already produced by the release workflow.

**STOP.** Evidence cannot be bound to the shipped candidate, a safety failure
occurs, or production data invalidates an assumed development-only reset.

## Execution rules and handoff

Before each implementation PR: read the owning plan/scoped guidance; inspect
`git status --short` and both committed/uncommitted drift; build the evidence
map; state acceptance rows and destructive targets. Preserve unrelated work.
Every PR closes a bounded behavior/proof slice and updates its owning ledger.

Focused local checks use a unique external Cargo target, for example:

```bash
test -d "$HOME/Workspace" && test -w "$HOME/Workspace"
mkdir -p "$HOME/Workspace/crabbuild-target/crab-f410"
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-f410" \
  cargo test -p crab-cache --locked --features local-cache,xet-chunk-cache
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-f410" \
  cargo test -p crab-cache-store --locked
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-f410" \
  cargo test -p crab-read --locked
git diff --check
```

Stop if the workspace volume is unavailable. Broad suites, provider runs,
cross-platform proof, and scale tests belong in CI/dedicated environments.
Inspect scripts and artifact paths before invoking a build/install target.
No deployment, public claim, destructive cleanup, or provider spend is
authorized merely by approval of this design.

For each accepted phase, retain: candidate SHA; exact commands; source and
dependency contract references; test/report IDs; resource/request measurements;
failure-path proof; residual exclusions; and owner sign-off. **Code present**,
**tests written**, and **a historical green run** are not acceptance states.

Use this acceptance record in the owning plan or PR; do not create another
tracking system:

```text
Phase / slice:
DRI / reviewer:
Candidate SHA / artifact digest / dependency lockfile:
Preconditions and isolated fixture:
Acceptance criterion -> command -> report -> observed result:
Request counts / resource limits / observed peaks:
Fault, cancellation, restart, and sibling-surface proof:
Residual exclusions / reason / owning follow-up:
Decision: OPEN | IN PROGRESS | ACCEPTED | BLOCKED (exact missing input)
```

`ACCEPTED` requires every required criterion, not a percentage of completed
tasks. Any change to the support matrix or an acceptance threshold must be
reviewed before rerunning qualification and remain visible in release notes.
