# Plan 017: Post-launch product hardening

> **Executor instructions**: Treat this as the product-level execution plan
> after Crab 1.0.1. Do not rebuild capabilities that already exist. Close the
> remaining integrity, Git interoperability, scale, performance, and team
> adoption gates using the current canonical owners. A support claim is not
> complete until a release-shaped binary, a real backend, and retained
> exact-commit evidence pass. Read `crates/AGENTS.md` before shared-crate work.
>
> **Drift check (run first)**:
> `git status --short`, then
> `git diff --stat 63bfc8c9 -- crates/crab-metadata crates/crab-storage crates/crab-remote-git crates/crab-auth crates/crab-auth-store crates/crab-auth-server crab/src/cmd/fsck.rs crab/src/cmd/fsck_store.rs crab/src/cmd/mirror crab/src/git crab/src/maintenance.rs crab/deploy/auth-service crab/scripts/e2e .github/workflows packages/web/content/docs plans`
> The diff includes tracked working-tree changes; inspect relevant untracked
> files reported by status separately. A commit-only diff misses local work.
> Rebuild the evidence map below if a persistent format, manifest publication
> owner, fsck contract, Git protocol path, maintenance owner, managed API, or
> qualification schema changed.

## Status

- **Priority**: P0 product roadmap
- **Effort**: XL, delivered as independently releasable phases
- **Risk**: HIGH
- **Depends on**: delivered add/push Plans 011-016; existing GC Plans 001-005;
  `plans/001-large-repository-scale-roadmap.md`
- **Category**: durability, compatibility, scalability, performance, security,
  managed service, migration, release evidence
- **Planned at**: commit `63bfc8c9648dbe790470e2fecbc2abb8529a39a1`,
  2026-09-02
- **Delivery status**: IN PROGRESS — Phase 2 implementation and local proof;
  full Phase 2 acceptance and the other phase outcomes remain open.

### Reading and execution guide

Start with the five-priority decision below, then use the relevant phase as
the implementation brief. Each phase contains context, architecture, scoped
delivery work, acceptance criteria, and reasons to stop. Phases 1–4 also have
milestone handoffs; Phase 5 has equivalent 5A–5D delivery stages. Milestone
exits enable the next increment; they do not replace the full phase criteria.

- [Integrity and recovery](#phase-1-make-integrity-and-recovery-authoritative)
- [Native Git and mirror correctness](#phase-2-finish-native-git-and-mirror-correctness)
- [Metadata and maintenance scale](#phase-3-close-metadata-maintenance-and-gc-scale-gates)
- [Performance and cloud cost](#phase-4-turn-performance-and-cloud-cost-into-an-slo)
- [Team adoption and managed service](#phase-5-deliver-the-team-and-managed-service-product)
- [Current gap register and next handoffs](#current-gap-register-and-next-handoffs)
- [Remaining mirror proof packets](#remaining-mirror-proof-packets)

The evidence map is a tagged-baseline assessment. Dated implementation
checkpoints are historical, not a live completion ledger. In particular,
the exact pre-push publication checkpoint records unshipped hook wiring and
local E2E proof beyond the decoder checkpoint. That proof does not establish
continuous GC protection or the original platform/provider matrix. Reconcile
the working tree with retained evidence before opening new implementation
tickets; preserve existing work.

## Executive decision

The next five product priorities, in order, are:

| Priority | Product outcome | Gap to close | Release exit |
|---|---|---|---|
| 1. Integrity and recovery | Operators can prove whether a repository is healthy and recover safely after corruption, crashes, or provider faults. | `fsck` can warn and continue after a check fails, does not perform complete current-head Git connectivity, and cannot enumerate provider multipart uploads. GC qualification remains incomplete. | Complete, snapshot-bound fsck; plan/apply repair; crash/race/provider qualification; restore drill. |
| 2. Native Git and mirror correctness | Supported ordinary Git operations behave like Git users expect; GitHub/GitLab mirror drift is visible and reconcilable. | Protocol v2 is implemented but provider/release proof is incomplete. Stateful `connect` and several v2 extensions remain deliberately unsupported. Publication across the collaboration remote and Crab is not atomic, and hooks are bypassable. | Published support matrix; real-Git/provider matrix; fail-closed protocol behavior; required mirror integrity check and reconciliation. |
| 3. Metadata and maintenance scale | Repository cost grows with the change, not total history; maintenance and GC remain bounded at 10,000-push and team scale. | Paging, catalog, split graph, geometric packs, and owner budgets exist, but 10,000-push, interruption, provider, distributed-fanout, retention, and owner-failover gates remain open. | Repeated 10,000-push proof; bounded memory/I/O; restartable maintenance/GC; distributed concurrency and failover evidence. |
| 4. Performance and cloud cost | Clone, fetch, push, and hydration have measurable SLOs and predictable provider cost. | Generated-pack caching and request budgets exist, but response-pack construction, client indexing, owner work, provider range behavior, and sustained fanout remain bottlenecks or unqualified. | Versioned SLO report; cold/warm and distributed fanout targets; cost envelope; regression gates. |
| 5. Team adoption and managed control plane | A team can sign in, authorize people and automation, create a repository, migrate, and operate it without sharing cloud credentials. | Contracts, clients, transfer grants, protected-push helpers, and a Python enterprise auth reference exist. The hosted managed service is still preview, the Rust helper crate is not an HTTP service, and no direct-to-managed migration job ships. | Hosted or self-hosted control plane E2E; least-privilege grants; audit/backup/restore; resumable migration and rollback proof. |

This order is intentional. Workflow breadth, additional storage tiers,
experiments, and managed-service feature expansion must not outrun the
repository trust, Git, scale, and SLO foundations.

### Product rationale and scope

This ranking is a risk/dependency judgment from the repository, not a claim
about measured customer demand. The initial target is a developer or small
team keeping large files in its own object storage while retaining familiar
Git collaboration. Validate that target with adopters before expanding the
managed-service scope.

| Priority | User problem and reason for ordering | Product measure, separate from engineering acceptance |
|---|---|---|
| 1. Integrity/recovery | A remote cannot become the trusted copy if a clean diagnostic can conceal an incomplete check. This also blocks safe GC, migration, and restore. | Correct diagnosis and successful timed recovery for every injected incident in the declared support profile; no false-clean results. |
| 2. Git/mirror | Adoption should not require discovering protocol restrictions or recovering two-remote drift by hand. | First clone/push/hydrate success on supported clients; time from injected drift to detection and operator-approved convergence. |
| 3. Scale | A repository that works on day one must remain operable as history, assets, and users accumulate. | Per-change work, maintenance backlog, and storage reclamation remain inside a predeclared envelope as history grows. |
| 4. Performance/cost | Users need faster completion and predictable bills, not only better internal throughput. | End-to-end task latency and requests/bytes per task, split by cold/warm cache, repository size, provider, and concurrency. |
| 5. Teams | Credential distribution and onboarding can block a team even when direct storage works. This is a larger new product surface, so validate demand first. | A second team member and a CI identity can be onboarded, perform authorized work, and be revoked without sharing an owner's cloud credentials. |

Collect adoption measurements through opt-in pilots or local/redacted reports;
this plan does not authorize adding default client telemetry. Set numeric
onboarding and drift-recovery targets before the pilot. Counts of commands,
features, or passing unit tests are not substitutes for these outcomes.

Do not build a PR/issue platform, mandatory hosted dependency, additional
protocol extensions, or another cache/metadata architecture as part of these
priorities without a demonstrated gap in the existing owners. If onboarding
research shows direct-mode setup is the blocker rather than team policy,
improve `configure`/`doctor` and docs before committing to hosted-service work.

### Current gap register and next handoffs

This is the executable starting point for the current worktree, not a new
release verdict. Source inspection confirms the integrity and mirror gaps
below. Scale/performance readiness is taken from the existing qualification
ledger, not a benchmark rerun. The dated Phase 2 checkpoints retain their
original evidence scope; no acceptance checkbox is closed by this table.

| Priority | Existing foundation to preserve | Next mitigation and owner | Observable handoff / acceptance |
|---|---|---|---|
| 1. Integrity and recovery | Fsck orchestration, store checker, origin-byte verifier, recovery and GC owners | **1A first:** represent failed required checks in `FsckOutcome`, not only warning logs; continue to **1B** for one protected snapshot | Independently fail every required checker: text/JSON/JSONL all name incomplete coverage and exit non-zero. A genuinely clean fixture passes. Then concurrent writer/GC tests cannot produce a mixed or false-clean proof. |
| 2. Native Git and mirror reconciliation | Matrix/verifier, isolated cache and process owners, exact pre-push batch, shared LFS input, origin-byte verification; shared snapshot and recipe digests now in the unqualified worktree | **2B → 2C:** qualify identity binding; finish permission-safe inspection and dependency protection through the existing publication owner | Same refs with changed metadata/recipe identity invalidate a saved plan. Read-only checks issue zero remote writes. GC overlap, lost commit responses and stale expected-old refs cannot yield false success; host CI proves exact-candidate enforcement. |
| 3. Metadata and maintenance scale | Partitioned metadata, paged recipes, catalogs/graphs, bounded generation owner and GC plans | **3A → 3B:** retain reproducible growth reports; fix only the demonstrated unbounded join, traversal or resume owner | Two isolated 10,000-push runs and 10x-cardinality tests meet predeclared resource ceilings; killing/resuming owner and GC at durable boundaries preserves refs and bytes. |
| 4. Performance and cloud cost | Existing response assembler, generated-pack cache, request budgets and benchmark runner | **4A → 4B:** measure whole-operation cost before accepting pack/range/cache changes | Matched cold/warm baseline and candidate reports include producer CPU, client indexing, requests, bytes, RSS and latency; phase targets pass without hidden client or cost regressions. |
| 5. Team adoption | Auth/API/grant contracts, protected receive/view helpers and enterprise reference service | **5A:** implement and operate the optional self-hosted control plane; validate demand before hosted expansion | A clean deployment supports two roles and a CI identity through create/push/clone/revoke; isolation, crash replay and timed restore pass on the named real provider. |

Current code anchors: `crab/src/cmd/fsck.rs` (`run_fsck`,
`FsckOutcome::to_summary`); `crab/src/cmd/fsck_store.rs`
(`StoreChecker::verify_pointer_data`); `crab/src/cmd/mirror/types.rs`
(`MirrorReconciliationPlan`); `crab/src/cmd/mirror/reconcile.rs`
(`inspect`, `apply_plan`); `crates/crab-metadata/src/file_index_lookup.rs`
(`open_for_storage`); `crates/crab-auth-server/README.md`.
The scale/performance source of truth remains
`plans/001-large-repository-scale-roadmap.md`.

**Immediate sequencing:** land the independently testable 1A result contract;
resolve the shared 1B/2B snapshot-and-lifetime boundary; complete 2C recovery,
host enforcement and qualification. Instrument 3A/4A alongside that work when
dedicated infrastructure is available. Do not hold the useful direct-mode
release for the optional team service. No staffing or delivery date is assumed.

## Outcome

After this plan, a maintainer can make a support claim using one evidence
bundle tied to a release candidate and answer all of these questions:

1. Is every current ref, Git object, pointer recipe, shard, xorb, manifest, and
   required coordination root present and internally consistent?
2. Does the supported Git operation behave correctly on the declared Git
   versions and providers, including cancellation and expected rejection?
3. Can 10,000 incremental pushes, concurrent users, maintenance, GC, and owner
   failover complete inside fixed memory, I/O, request, and time budgets?
4. What are the cold, warm, and fanout latency, request, byte, CPU, and cache
   costs, and did they regress from the accepted baseline?
5. Can an authorized team create, migrate, push, clone, revoke, audit, restore,
   and roll back a managed repository without receiving canonical-write
   credentials?

The plan does not make Crab a pull-request, issue, CI, or merge-queue product.
GitHub/GitLab remain the recommended collaboration control plane in mirror
mode. Crab remains the Git/data storage plane.

## Current evidence map

The map below records the planning baseline at `63bfc8c9`, not a claim that
every line number still matches a modified worktree. Resolve the named symbols
before implementation. The Phase 2 checkpoint separately records unshipped
work and its remaining gaps; local proof does not change release support.

| Surface | Current behavior | Consequence |
|---|---|---|
| Shipped contract | `crab/Cargo.toml` is 1.0.1 and tag `v1.0.1` exists. `RELEASING.md:3-22` defines immutable published releases. | The pre-launch “no compatibility obligation” premise no longer applies to tagged persistent or public contracts. |
| Manifest | `crates/crab-metadata/src/manifests.rs:16-53` defines strict manifest v1 with generation, refs, index hashes, Git validation digest, graph, and registry roots. | Future changes need additive compatible reads or an explicit migration and admission boundary. |
| Layout admission | `crates/crab-metadata/src/layout_descriptor.rs:7-34` defines strict schema v1; `:54-100` accepts only the canonical partitioned parameters; `:150-206` reads or creates the descriptor. | A future incompatible layout must fence old writers before any migrated state becomes writable. |
| Integrity command | `crab/src/cmd/fsck.rs:1-5` names missing full Git connectivity and provider multipart enumeration. `:562-632` warns on check errors, while `:367-375` can still report `passed`. | An incomplete check can look clean. The outcome needs explicit coverage and fail-closed semantics. |
| Store checker | `crab/src/cmd/fsck_store.rs:370-410` only verifies non-empty listed ref bodies. `:412-455` checks shard/xorb presence but not byte-identical file reconstruction. `:712-725` cannot abort provider-side multipart uploads. | Fsck is useful diagnostics, not yet the authoritative integrity gate implied by a clean result. |
| GC | `plans/README.md` records remaining materialized joins, missing provider cursors, unsegmented closures, strict inventory gaps, and missing race/crash/high-cardinality/real-provider evidence. | Destructive GC support must remain narrower than general read/write support until each backend passes. |
| Git protocol | `crab/docs/architecture/git-protocol-v2.md:18-59` documents the implemented profile and fail-closed filters; `:142-159` defines downgrade and unsupported behavior. | Do not add broad fallback or a server merely to claim every Git extension. Qualify and publish the supported profile. |
| Git provider posture | `crab/docs/architecture/git-integration.md:69-99` says RustFS evidence does not prove GCS/Azure parity. | Provider support is a row-by-row product claim, not inferred from an adapter compiling. |
| Mirror mode | `packages/web/content/docs/cli/getting-started/mirror-mode.mdx:72-85` documents hook ordering and the unavoidable two-remote race; `:148-162` recommends CI enforcement and external collaboration. | Build detection and reconciliation. Do not claim cross-remote atomicity. |
| Scale machinery | `plans/001-large-repository-scale-roadmap.md:1079-1220` inventories implemented bitmap, catalog, split-graph, pack-cache, owner, admission, and bounded-read work. | The remaining priority is qualification and bottleneck closure, not another parallel metadata architecture. |
| Scale gaps | `plans/001-large-repository-scale-roadmap.md:1645-1690` keeps 10,000-push, owner, fault, fanout, provider, retention, and response-pack SLOs open. | Those rows become this plan's scale and performance release gates. |
| Provider harness | `crab/tests/provider_qualification.rs:225-263` runs against an isolated real provider. `.github/workflows/pb-provider-qualification.yml:167-270` runs GCS/Azure only when enabled. | The contract exists; retained real-provider evidence and release gating are incomplete. |
| Managed contracts | `crates/crab-auth/src/managed/transfer.rs:51-200` defines operation-, repository-, scope-, permission-, expiry-, and transport-bound grants. Managed DTOs and OpenAPI compatibility tests already exist. | Reuse these contracts. Do not invent a second auth/grant protocol. |
| Managed runtime | `crates/crab-auth-server/README.md:1-16` states that it provides deterministic helper binaries, not a long-running HTTP server. | A hosted control-plane runtime, durable state, job execution, and operations remain product work. |
| Enterprise reference | `crab/deploy/auth-service/README.md` provides a Python FastAPI reference with RBAC, scoped credentials, and protected push. | Preserve its released direct-repository contract until an explicitly qualified whole-environment migration. No per-request fallback. |
| Managed launch | `packages/web/content/docs/cli/managed-service/index.mdx:10-15` marks the hosted service as preview. `migration.mdx:82-94` says no direct-to-managed copy command exists. | Do not present DTOs, docs, or helper binaries as a working managed product. Build and qualify the full path. |

## Product and architecture rules

### Trust boundaries

- The canonical manifest/ref update remains the repository commit point.
- Clients may upload immutable content before commit. No failed operation may
  expose a ref whose complete Git and data dependencies are not durable.
- A managed client never receives credentials that can mutate canonical refs,
  manifests, policy, audit, or another repository's prefix.
- Derived catalogs, graphs, visibility proofs, closures, indexes, and generated
  packs may be rebuilt. Their absence may reduce availability or performance;
  their corruption must never widen authorization or invent reachability.
- GC and repair may act only from a complete, pinned snapshot and a validated
  plan. Unknown, skipped, or failed discovery is not proof of absence.
- No provider, Git version, feature, or deployment shape is supported merely
  because its code compiles. A passing retained evidence row owns the claim.

### Post-launch compatibility policy

Tagged releases make these durable contracts:

- repository layout and object keys;
- manifest, journal, descriptor, pointer, recipe, shard, pack, and receipt
  formats;
- managed API and transfer-grant contracts;
- structured CLI output and documented configuration;
- Git remote-helper behavior relied on by supported Git versions.

For every persistent change, classify the surface before implementation:

1. **Additive reader-compatible**: old writers remain safe; new readers accept
   old state; derived acceleration can be backfilled asynchronously.
2. **Explicit migration**: freeze writers, prepare immutable replacements,
   verify them, commit one authoritative version/descriptor switch, then resume.
3. **Breaking product change**: requires an approved major-version policy and
   upgrade/rollback plan.

Add a compatibility fixture from the previous supported release to CI. An
older client presented with an incompatible migrated descriptor must fail
before acquiring write admission or uploading bytes. A newer client must read
the previous release's repository and either operate safely or emit the
documented migration requirement. Never solve this with indefinite dual
writers, guessed descriptor-less reads, or silent fallback.

### Target composition

```text
Git / Crab CLI
    |-- direct repository: scoped cloud identity ---------------------.
    |                                                               |
    `-- managed repository: identity + policy API                    |
            |-- repository catalog / membership / audit             |
            |-- read grant ------------------------------------------+--> object storage
            `-- protected push session --> verifier/finalizer helper-'

Pinned manifest generation
    |-- Git packs + locator + visibility + commit graph
    |-- pointer recipes + shards + xorbs
    `-- recovery roots + journals + coordination evidence

Generation owner
    `-- bounded compaction / repair / generated-pack work
```

Bulk bytes continue to flow between client and object storage when the
provider can safely downscope credentials. Use the existing gateway transport
only where direct least-privilege grants are impossible. The control plane
authorizes, coordinates, audits, and publishes; it does not become the default
large-object proxy.

## Execution order

| Phase | Priority | Depends on | Can overlap with | Primary release |
|---|---|---|---|---|
| 0 | Contract and evidence baseline | — | — | Patch/minor |
| 1 | Integrity, recovery, and provider safety | 0 | Early Phase 2 conformance work | Minor |
| 2 | Native Git and mirror correctness | 0; Phase 1 integrity primitives for final gate | Phase 3 harness work | Minor |
| 3 | Metadata and maintenance scale | 0; existing scale roadmap | Phase 2 | Minor |
| 4 | Performance and cloud-cost SLOs | 2 and 3 baselines | Phase 5 service scaffolding | Minor |
| 5 | Team adoption and managed control plane | 1 and 2 for GA; contracts can start after 0 | 3 and 4 | Preview, then minor/GA |
| 6 | Consolidated release and rollout | 1-5 rows being claimed | — | Release candidate |

Phase numbers express dependency order, not calendar estimates. Each phase
lands in small PRs and keeps main releasable.

The first useful release does not wait for all five priorities. Deliver the
coverage-aware fsck result and independently qualified Git/mirror increments
first. Scale/performance work and managed preview follow their own dependency
gates. Completing one increment does not close its parent phase or authorize
broader provider/platform claims.

### Delivery ownership and first usable increments

Owners below are responsibility roles, not assignments to people. Name one
directly responsible maintainer and one reviewer per packet before starting.
The packet list decomposes the acceptance criteria; it does not replace them.

| Track | Accountable role / implementation boundary | First usable increment | Gate before broader rollout |
|---|---|---|---|
| Foundation | Release maintainer; schemas, compatibility fixtures, `.github/workflows/` | Reject incomplete or mismatched evidence without cloud credentials | Exact candidate and N-1 binary fixtures pass |
| 1. Integrity | Storage/integrity maintainer; `crab/src/cmd/fsck.rs`, `fsck_store.rs`, metadata/read/storage owners | A failed required check can no longer produce a clean CLI result | Pinned connectivity, byte verification, safe repair and restore |
| 2. Git / mirror | Git maintainer; `crab/src/git/`, `crab/src/cmd/mirror/` | Read-only drift check with explicit verification coverage and CI policy | Race-safe reconciliation plus real Git/OS/provider evidence |
| 3. Scale | Metadata/maintenance maintainer; existing scale and GC plans | Reproducible distributed workload and resource report | 10,000-push, crash/resume, retention and bounded-resource gates |
| 4. Performance | Read-path/performance maintainer; existing pack assembler and cache owners | Cold/warm end-to-end latency and provider-request baseline | Valid differential improvements without integrity or client regressions |
| 5. Teams | Service/security maintainer; managed auth contracts and server composition | One self-hosted provider: sign in, create, protected push, clone | Tenant isolation, revocation, restore and migration before hosted GA |

Managed mode stays optional. Direct mode must remain usable with the CLI and
the user's object storage, without a Crab account or mandatory control-plane
dependency. Build team-service breadth only after validating that credential
administration is blocking target teams; do not make it a prerequisite for
the first four outcomes.

### First execution batch

Start these as independently reviewable work packets, in this order. Reuse
in-progress work where present; do not implement a second path.

1. **Record the baseline and freeze evidence requirements.**
   Context: code, local smoke results, and release support currently have
   different maturity. Inputs: the support matrix, tagged 1.0.1 contracts,
   existing provider/GC/scale reports and the Phase 2 checkpoint. Deliverable:
   a manifest linking each required check to a named runner and verifier.
   Acceptance: missing, skipped, wrong-platform, wrong-binary, stale, and
   altered reports all fail offline tests; no current acceptance box is
   automatically closed. Dependencies: none.
2. **Make fsck coverage and exit status honest.**
   Context: `run_fsck` logs errors from required checks; `FsckOutcome::to_summary`
   and the `Cmd::Fsck` branch in `crab/src/main.rs` decide success from issue
   counts. Deliverable: one coverage-aware terminal decision shared by text,
   JSON, JSONL and process exit. Acceptance: inject one failure into each
   required checker independently; each result identifies the check, reports
   incomplete coverage and exits non-zero. Clean and genuinely inapplicable
   checks still pass. Preserve error sources and tagged output contracts.
   Dependencies: foundation schema/compatibility decisions; no cloud job is
   needed to prove this first increment.
3. **Close mirror integrity's remaining proof boundary.**
   Context: origin reconstruction, serialized source-cache use, and exact hook
   batches now have local proof. The canonical-recipe follow-up also makes
   pointer lookup non-writing and revalidates its captured metadata afterward.
   The current worktree additionally shares a captured Crab snapshot between
   ref classification and pointer verification, and includes metadata/recipe
   digests in saved plans. This is implementation progress, not complete proof:
   qualified endpoint/layout binding, whole-command non-writing reads, and
   protected publication lifetime remain open. Deliverable: one snapshot-bound,
   permission-safe proof shared by check, plan/apply and hook, with GC-safe
   publication lifetime. Continue the packets below rather than rebuilding
   the existing digest or lookup path.
   Acceptance: retain valid-file and corrupt-origin/healthy-cache tests; add
   same-refs/changed-recipe, metadata-compaction, concurrent-GC, lease-loss and
   read-only-grant tests. Permission, timeout and cancellation remain
   unverifiable, never evidence of absence. Dependencies: shared integrity
   contract from Phase 1. Preserve the existing cache/process owners and their
   race tests; keep ref mutation disabled whenever required proof is incomplete.

After this batch, complete Phase 1 recovery and Phase 2 conformance in their
listed PR slices. Phase 3 baseline collection can overlap; optimization and
hosted launch cannot bypass their predecessor gates.

### Decisions required before committing delivery dates

| Decision | Proposed starting point | Owner and acceptance for the decision |
|---|---|---|
| First release support envelope | Preserve existing tagged contracts; qualify one complete production-provider row first, then expand | Product + release owner publish exact Git versions, OS, modes and operations; no silent demotion of shipped behavior |
| Absolute latency, resource and cost budgets | Use the existing real-repository fixtures and dedicated hosts; do not invent a universal clone time | Performance owner records fixture digest, hardware/network, sample count, p95 target, RSS/spill/request ceilings and run cost cap before qualification |
| Recovery objective | Separate user-owned direct-storage restore from operated managed-service restore | Storage + service owner choose RPO/RTO and retention per profile, then execute timed restore drills |
| Managed deployment | Start with one self-hosted provider and reuse existing API/helper contracts | Service + security owner approve identity provider, transactional state store, durable job mechanism, key management and on-call responsibility before 5A implementation |
| Staffing and sequencing | Small releases with one accountable owner per packet | Maintainers size the PR slices after baseline work; publish estimates with dependencies, not an unsupported calendar promise |

All numeric performance goals below are **proposed acceptance targets**, not
current measurements or public guarantees. The pack-size/CPU/repeatability
targets inherit the existing scale roadmap. Unspecified absolute SLOs and
RPO/RTO are blocking profile decisions, not values an executor may choose after
seeing a run. Report small-sample tail latency as insufficient evidence rather
than asserting a meaningful p99 from a handful of measurements.

### Implementation ticket and evidence contract

Turn each PR slice below into a ticket with these fields before implementation:

1. **Context:** user-visible failure or measured bottleneck, current entry
   point and owner, relevant tagged behavior, and linked evidence.
2. **Scope:** one deliverable, callers/siblings affected, non-goals, dependency
   tickets, and any required design decision from the table above.
3. **Change:** state transitions and commit point, schema/API effects,
   cancellation/timeout behavior, resource bounds, and recovery/rollback.
4. **Acceptance:** fixture and initial state, user action, observable expected
   result, negative case, and exact automated verifier. Include a real side
   effect for product behavior; a read-only check instead proves no mutation.
5. **Handoff:** accountable maintainer/reviewer, candidate SHA/binary digest,
   report location/digest, commands run, remaining gaps, and rollout boundary.

Use `planned`, `in_progress`, `implemented_unqualified`, or `accepted` for
packet status. `accepted` requires its criteria and retained evidence; it does
not imply the entire phase is accepted. An unavailable infrastructure row stays
open, not skipped-success. A report digest proves artifact identity, not
authenticity: release jobs must also verify the producing workflow/ref and
artifact provenance through the trusted CI boundary.

### Verification starting points

These are existing owners and runners to extend, not evidence that the full
phase already passes. The phase criteria define the additional scenarios.

| Phase | Start from | Required extension or retained artifact |
|---|---|---|
| 0 | `crab/scripts/e2e/test_verify_provider_qualification_report.py`; `crab/scripts/e2e/test_verify_large_repo_rustfs_report.py`; release workflow | Offline rejection fixtures plus exact-candidate, previous-release and trusted-provenance evidence. |
| 1 | `crab/src/cmd/fsck.rs` tests; `crab/src/cmd/fsck_store.rs` tests; `crates/crab-read/src/integrity.rs`; `crab/scripts/e2e/gc_qualification.py` and `validate_gc_evidence.py` | Per-check failure/coverage cases, generation/GC races, repair restart and isolated restore report. The shared byte verifier is in-progress work, not a shipped fsck guarantee. |
| 2 | `crab/scripts/verify_git_capability_matrix.py`; `crab/scripts/tests/`; `crab/scripts/e2e/run_protocol_v2_partial_clone_rustfs_smoke.py`; mirror/schema tests | Exact release Git/OS/provider matrix, interrupted streams, mirror snapshot/cache races, complete hook batch and candidate-bound CI proof. |
| 3 | `crab/scripts/e2e/run_large_repo_rustfs.py`; `plans/001-large-repository-scale-roadmap.md`; GC qualification workflow | Repeated 10,000-push and distributed-client reports, bounded-resource curves, kill-point/resume and provider evidence. |
| 4 | The same large-repository runner and report verifier, extended in place | Comparable baseline/candidate reports with sample sufficiency, producer/client split, cost counts, and regression verdict. |
| 5 | `crates/crab-auth-server/README.md`; existing managed API compatibility tests; `crab/deploy/auth-service/README.md` | New service-level E2E runner using real identity/state/job/storage boundaries; isolation, revocation, restore and migration reports. Existing helper tests cannot stand in for it. |

Run narrow/offline checks locally. Run broad, real-provider, destructive-fault,
and distributed qualification in CI or a dedicated environment with an
isolated repository prefix, explicit cost cap, and cleanup report. Never use
production repositories or bucket-wide GC to satisfy an acceptance criterion.

## Phase 0: Freeze contracts and make claims evidence-backed

### Context

Crab 1.0.1 is shipped, but repository planning still contains a global
pre-launch hard-cut policy. The repository already has good evidence schemas
for providers, large repositories, GC, cache, workflows, and protocol v2; the
remaining problem is that support claims and release requirements are not one
mandatory matrix.

### Design and work packets

1. Inventory shipped persistent/public surfaces with owner, schema/version,
   reader, writer, migration policy, and last supported release.
2. Add previous-release fixtures and a released-binary compatibility job:
   create with N-1/read with N; upgrade with N; reject a future descriptor with
   N-1 before writes; validate structured output compatibility.
3. Define one versioned release evidence manifest. Each row records source SHA,
   binary digest, Git version, provider/service, platform, scenario, report
   digest, start/end, status, and support claim.
4. Make release jobs require every row named by the release's support matrix.
   A disabled provider row is `unsupported`, not `passed` or silently skipped.
5. Update product docs from code-derived support data. Keep preview and
   experimental features clearly separated from release-qualified features.

### Acceptance criteria

- [ ] `v1.0.1` repositories created by the released binary are readable by the
      candidate without rewrite, and a candidate write remains readable by the
      oldest version declared compatible.
- [ ] The old binary rejects an incompatible descriptor before any object PUT,
      lock acquisition, or manifest mutation.
- [ ] Every release claim maps to one exact-SHA evidence row; missing, skipped,
      expired, wrong-provider, wrong-platform, or wrong-binary evidence fails
      the release gate.
- [ ] The matrix distinguishes direct S3-compatible, AWS S3, GCS, Azure,
      managed, NFS, cache-service, and emulator evidence.
- [ ] No compatibility test depends on mutable `main`, an unpinned external
      image, ambient credentials, or a shared repository prefix.
- [ ] `plans/README.md` and public support docs no longer state that tagged
      user state can be reset as a general upgrade policy.

### Verification

```bash
python3 crab/scripts/e2e/test_verify_provider_qualification_report.py
python3 crab/scripts/e2e/test_verify_large_repo_rustfs_report.py
git diff --check
```

New compatibility/evidence verifiers must be runnable without cloud
credentials. The live rows remain isolated manual/scheduled jobs.

## Phase 1: Make integrity and recovery authoritative

### Context

The current checker detects many useful conditions but conflates “no issue was
reported” with “every required check completed.” It validates object presence
more often than semantic connectivity or reconstructed bytes. Repair is partly
embedded in the checker and provider multipart cleanup is unavailable through
the generic object-store interface. GC's remaining production proof depends on
an authoritative integrity result.

### Design

#### 1. Coverage-aware fsck result

Give each check a stable identifier and terminal state:

```text
passed | issues_found | skipped_not_applicable | incomplete | failed
```

The summary includes pinned repository generation/digest, completed checks,
incomplete checks, issue counts, byte/object coverage, and `passed`. `passed`
is true only when all checks required by the selected level completed and no
unrepaired error remains. Storage, parse, permission, timeout, cancellation,
and provider failures become typed incomplete/failed results and a non-zero
exit, never warning-only success.

Keep default `fsck` complete for current-head structural integrity. Add an
explicit quick level only if large-repository evidence proves the default is
operationally impractical; its output must say `authoritative=false`. Full
byte reconstruction may be an explicit expensive level, but sampled coverage
must never be labeled complete byte verification.

#### 2. One pinned integrity snapshot

Read and validate the layout descriptor and current manifest once. Pin the
manifest ETag, generation, validation digest, pack/shard index roots, history
policy, and coordinator epoch. Every check consumes that snapshot. Before a
clean result or repair apply, re-read the authoritative tokens; if they moved,
return `stale_snapshot` and retry from the start. Do not mix current and
historical generations.

Bound retries; a busy repository must terminate with an explicit stale result
instead of restarting forever. A successful report is evidence for its named
snapshot at completion, not a promise about future writes. Protect the
snapshot's physical dependencies from concurrent GC using the existing
coordination owner or a proved retention contract. A manifest digest alone
does not keep an object alive. Do not hold an exclusive writer fence across
an hours-long read-only scan merely to avoid designing that lifetime.

#### 3. Complete current-head traversal

- Resolve every advertised ref and peeled tag through the generation-bound
  remote Git reader.
- Walk commit, tree, tag, and blob connectivity with bounded queues and spill
  state; compare with `git fsck --strict --full` in a fresh reconstructed bare
  repository as differential proof.
- Parse every reachable Crab pointer and validate exact recipe-page coverage,
  file-index/shard ownership, chunk placement, xorb footer/ranges, and content
  hashes.
- In full mode, reconstruct every unique reachable file once and verify its
  file hash and length. Deduplicate shared recipes/chunks and meter provider
  reads so full verification is expensive but bounded.
- Validate current and retained recovery roots separately. A corrupt derived
  acceleration object is a repairable availability issue only after canonical
  Git/data state proves it can be rebuilt.

#### 4. Separate detect, plan, and apply

`fsck` produces an immutable repair plan bound to snapshot identity and issue
evidence. `--repair` first prints or persists the plan; apply reacquires the
required writer/maintenance/GC fences, validates the snapshot, performs only
allowlisted actions, journals outcomes, and re-runs affected checks. Missing
canonical bytes are never auto-deleted or fabricated. Recovery from history,
replica, cache, or backup is a named source with a byte/hash proof.

Move provider-only multipart listing/abort behind narrow S3, GCS, and Azure
adapters. If an adapter is unavailable, report that check as incomplete or not
applicable according to the selected support profile. Do not pretend the
generic `object_store` interface performed the operation.

#### 5. Finish GC qualification

Execute `plans/005-production-qualification.md`: writer races, crash/resume,
lease loss, partial listing, conditional identity mismatch, high cardinality,
and real providers. A destructive GC plan requires a clean structural fsck
against the same protected-root policy. Post-GC proof includes fresh clone,
strict Git fsck, full or declared byte reconstruction, and exact-prefix cleanup.

### PR slices

1. Coverage/result schema and fail-closed orchestration.
2. Snapshot owner and complete Git connectivity.
3. Pointer-to-byte traversal with bounded spill/deduplication.
4. Immutable repair plan/journal and safe derived-index rebuilds.
5. Provider multipart adapters and GC qualification matrix.
6. Backup restore and operator runbook.

### Milestone handoffs

**1A — Trustworthy diagnosis.** Context: a failed checker can currently be
absent from the issue count used to decide success. Entry: Phase 0 has
classified the tagged output contract. Implement PR slice 1 in the existing
fsck orchestration and output owners. Exit: independently fail each required
checker and observe a named incomplete/failed result and non-zero exit in
text, JSON, and JSONL; a completed clean fixture still succeeds. No remote
repair is needed or authorized by this milestone.

**1B — Snapshot-bound integrity.** Context: accurate individual checks can
still describe different generations or read bytes collected concurrently.
Entry: 1A's result contract and an approved snapshot-lifetime mechanism.
Implement slices 2–3 using the existing metadata, coordination, Git, and read
owners. Exit: a real fixture passes strict Git connectivity and full-file
hash/length comparison; corrupt-origin/healthy-cache and concurrent writer/GC
fixtures cannot produce a false-clean result. Record memory/spill limits and
prove the same limits at 10x cardinality. No destructive maintenance rollout
until lifetime protection passes its race tests.

**1C — Recovery and safe maintenance.** Context: diagnosis alone cannot restore
lost data or justify deletion. Entry: 1B plus declared recovery sources,
retention, RPO/RTO, and isolated provider fixtures. Implement slices 4–6.
Exit: replay every repair checkpoint after interruption, reject stale/altered
plans, preserve referenced/grace-period objects, and restore a separate
repository prefix with matching refs and bytes inside the approved recovery
budget. An unavailable provider row remains open. Rollback stops mutation and
retains the original data and journal; it never guesses missing content.

### Acceptance criteria

- [ ] Injecting an error into any required check makes `passed=false`, records
      the failed check, and exits non-zero in human, JSON, and JSONL modes.
- [ ] A concurrent manifest/ref change returns `stale_snapshot`; no mixed-
      generation clean result or repair is possible.
- [ ] Every current ref and peeled tag resolves; all reachable commits, trees,
      tags, and blobs pass a Git differential check.
- [ ] Every reachable pointer has complete recipe terms and every term resolves
      to verified shard/xorb bytes; full mode reconstructs byte-identical files.
- [ ] Peak memory, open files, and concurrency stay within fixed report bounds
      as object count grows 10x. No repository-sized `Vec`/`HashSet` is the
      durable traversal authority.
- [ ] Repair is dry-run/plan-first, snapshot-bound, resumable, idempotent, and
      refuses missing canonical data or incomplete discovery.
- [ ] Each advertised provider passes multipart, race, crash-resume, throttling,
      cancellation, and post-repair/post-GC integrity rows on a real service.
- [ ] A backup restore into a new isolated prefix reproduces refs and selected
      full-file hashes and meets declared RPO/RTO.

### STOP conditions

- A clean result requires swallowing an unavailable check.
- Repair would operate without a stable manifest/coordinator/GC fence.
- Full traversal can only fit by removing integrity validation.
- A real-provider claim is being inferred from RustFS or mocks.

## Phase 2: Finish native Git and mirror correctness

### Context

Crab already implements a strong local protocol-v2 upload-pack profile,
complete-pack compatibility path, shallow/deepen handling, partial-clone
filters, multi-ref push, tags, force, and deletion. The work now is support
discipline, interoperability proof, and the mirror-mode gap caused by two
independent remotes.

### Design

#### 1. Publish capability tiers

Maintain one machine-readable matrix keyed by Git version, OS, repository
mode, provider, and operation. Classify each cell `supported`, `preview`, or
`unsupported`. Generate docs and release evidence requirements from it.

The supported v2 profile remains local `stateless-connect git-upload-pack`.
Keep stateful `connect`, `receive-pack` takeover, `packfile-uris`, `object-info`,
`ref-in-want`, and unimplemented shallow selectors unsupported until user
demand and a bounded architecture justify them. Unsupported requests return a
Git protocol error before response-pack bytes; they never cause a surprise
complete fetch.

#### 2. Expand real-Git conformance

Run the release binary against the declared oldest, intermediate, and current
Git versions on Linux, macOS, and Windows. Cover:

- clone, ls-remote, incremental fetch, tags, branch deletion, force, and atomic
  multi-ref push;
- depth, deepen, unshallow, every accepted filter, lazy promised-object fetch,
  clone from empty/unborn, and repositories with SHA-looking ref names;
- cancellation before and after packfile response begins, truncated streams,
  stale proof, corrupt pack/index, auth expiry, and throttling;
- older-client rollback behavior: serve promised objects exactly or reject
  before local pack/promisor mutation.

The production-provider row includes add/push/clone/hydrate and protocol v2,
not only generic object-store CAS.

#### 3. Make mirror drift a product state

Add a read-only mirror integrity command and status payload that compares the
configured collaboration remote with the Crab remote from pinned snapshots.
Report refs as `equal`, `source_ahead`, `crab_ahead`, `diverged`, or
`unverifiable`. For collaboration-source commits, scan reachable pointer blobs
and prove that their recipes and immutable data are available in Crab.

Add a CI mode suitable for a required GitHub/GitLab status check. It fails when
the source contains an unprotected pointer, when refs diverge under the chosen
policy, or when verification is incomplete. Hooks remain a fast local guard,
not the authority.

Bind the CI result to the exact candidate commit/ref set, not a mutable branch
name or the runner's current checkout. A required check can block merging or
deployment through configured branch protection; it cannot prevent a ref
already accepted by the collaboration remote. All-ref rejection requires a
server-side enforcement point supported by that host. Document this boundary
and test the configured merge gate; do not describe post-push CI as an atomic
two-remote transaction.

Add an explicit reconciliation plan:

- source-ahead: publish the missing source refs/data to Crab after integrity
  proof;
- Crab-ahead after a rejected collaboration push: retain as recoverable state,
  then retry source publication or let the operator prune only after policy;
- diverged: stop and require an operator-selected source of truth;
- never schedule `--mirror` in both directions and never delete destination-
  only refs from an implicit repair.

No design can make GitHub and an object-store remote one transaction. Product
success is prompt detection, safe convergence, and clear ownership.

#### 4. Reconciliation transaction and failure contract

1. Resolve repository identity and the explicit ref scope. Capture source
   OIDs and peeled tags, and the Crab manifest generation/digest plus ref
   values. Use an isolated inspection repository or serialized cache refresh;
   a second invocation must not rewrite refs during the first traversal.
2. Walk the captured objects, including tag targets, with bounded traversal.
   Bind pointer proof to exact recipe identity and file hash/size. Verify
   origin availability independently of healthy local caches; a cache-only
   copy is a recovery source, not proof of durable remote availability.
3. Return separate data outcomes: verified, missing, corrupt, or unverifiable.
   Authentication failure, throttling exhaustion, cancellation and timeout do
   not establish absence. Record coverage and freshness. Source unavailability
   must produce the same structured fail-closed contract as Crab unavailability.
4. Persist an immutable plan: schema version, repository identities, captured
   source/destination snapshots, ref scope, per-ref old/new OIDs, dependency
   proof identity, policy, approved deletions and content digest. Never persist
   credentials. Unknown actions, changed targets, altered plans or stale proof
   fail before publication.
5. For source-ahead data, use only a declared source that actually has the
   bytes, such as Crab staging or a verified backup. A Git pointer cannot
   recreate missing content. Upload through the canonical staging/push path,
   flush durable dependencies, then reverify. If no valid source exists,
   return a recovery-needed result; do not publish the refs.
6. Revalidate source OIDs and destination generation immediately before apply;
   enforce expected-old ref values under the destination's existing lock/CAS
   authority. A stale lease rejects even a forced operation. Publish the
   approved destination ref batch atomically within Crab, never across both
   remotes. A source movement afterward is new drift, not a reason to undo a
   correctly published historical snapshot.
7. Re-read destination refs and dependencies, persist the terminal result,
   and make reapply idempotent. Cancellation before commit leaves prior refs;
   an uncertain response after commit triggers read-back to resolve the result,
   not an unconditional retry or compensating deletion. Destination-only refs
   remain intact unless both the saved plan and apply approve their deletion.

The local hook must inspect the complete ref batch supplied on stdin,
including tags, rewrites, deletions and differently named source/destination
refs. Checking only the current branch is insufficient. Preserve unrelated
hooks and honor `core.hooksPath`; direct Crab pushes must not recurse into the
collaboration guard. Validate hook absence and bypass as expected threat
scenarios, not as proof that the data was checked.

#### 5. Shared dependency-proof contract: next implementation boundary

The current origin verifier proves individual file bytes, but that alone is
not a publication authorization. Implement this contract once across fsck,
mirror check/plan/apply, and pre-push; do not add a mirror-only metadata reader.

- **Snapshot identity — metadata owner.** Start from one
  `RepositorySnapshot`, not separate ref advertisement and file-index reads.
  Bind repository/layout identity, manifest ETag and generation, committed
  journal frontier and captured active transactions, materialized ref/peeled
  values, and effective pack/shard inventories. The manifest alone is
  insufficient: committed journal transactions can change visible state before
  manifest compaction. Canonical ordering must make equivalent input stable;
  malformed or unavailable identity fails closed.
- **Recipe identity — metadata/read owners.** Bind each unique file hash and
  declared size to the exact immutable shard and recipe selected from that
  snapshot. Include the recipe format and ordered reconstruction terms in the
  proof digest. Return coverage, verification outcome and proof identity, not
  just a pointer count. Stream/spill the proof inventory under declared limits;
  do not solve determinism with another repository-sized in-memory collection.
- **Permission-safe inspection — storage/metadata owners.** A check must not
  open a SlateDB reader that creates checkpoints, perform read-repair, or
  acquire write credentials. Reuse the existing scoped-store read contract,
  extending it to accept the caller's snapshot. An acceleration miss is not
  proof that canonical data is missing. Inspect all child Git and metadata
  paths with a store that rejects and records every write attempt.
- **Lifetime — coordination/publication owners.** A report identifies a
  verified historical snapshot; a saved report cannot reserve its objects.
  Apply reacquires protection and revalidates before publication, retaining
  protection through durable dependency upload, atomic expected-old ref commit
  and terminal read-back. Prefer the existing shared writer/GC admission,
  with one lifecycle owner; do not stack independent guards in parent and
  child without proving handoff, capacity, cancellation and crash behavior.
- **Read-only lifetime decision — prerequisite, still open.** Choose and prove
  a non-mutating GC observation/retention mechanism before accepting 1B/2B.
  An observation design must detect a sweep that starts and finishes during
  the scan, including state deletion/recreation, expiry and crashes. An ETag
  comparison or an absent lock at two instants is not sufficient by itself.
  If the provider/grant cannot expose the necessary proof, return unverifiable;
  do not silently write a lease or claim publication safety from a read check.

Implement in three reviewable packets: metadata snapshot-bound lookup and
proof types; canonical streaming verification plus non-mutating inspection;
publication-owner lifetime/revalidation and plan/terminal-result integration.
Each packet must trace direct, managed, repair and hook callers before changing
shared APIs. Unshipped plan schemas can be revised in place; tagged persistence
or output changes require Phase 0's compatibility decision.

Acceptance fixtures must include unchanged ref OIDs with a different recipe,
active-journal commit without manifest compaction, compaction during scanning,
valid cache with corrupt origin, absent acceleration with valid canonical
data, denied metadata writes, a GC sweep entirely inside the scan, lease loss
before commit, and a lost response after commit. Observe no false-clean check,
no unproved ref publication, and an explicit stale/unverifiable or resolved
terminal result as appropriate. Retain the original seven phase criteria;
these packets do not replace provider, host-CI or fault qualification.

### Remaining mirror proof packets

These packets refine 2B/2C for the current worktree. They do not replace the
five-priority roadmap or its release gates. Execute in order except where an
explicit dependency permits overlap; assign the accountable owner from the
delivery table before opening a PR.

**Identity qualification — metadata, storage and Git owners.** Context:
`RepositorySnapshot::digest`, caller-pinned pointer verification, and saved
`destination_snapshot`/`recipe_digest` fields now exist. Storage/auth now bind
the effective transport separately from the established logical
`BucketIdentity`; plans retain `destination_identity` even during converged
replay and bind the validated layout descriptor. This does not yet qualify all
managed/provider routes. Deliverable: complete the credential-free identity
contract at the storage/auth/metadata boundaries, consumed by snapshot proof
and plan validation.
Acceptance: the same bucket name and copied metadata on two distinct endpoints
cannot authorize the same plan; scope or layout changes reject it; unchanged
refs with altered manifest/journal/recipe identity invalidate it. Exercise the
actual check → save plan → apply path, including a zero-action plan, not only
hash helpers. Credential rotation must not change repository identity or leak
secrets. Dependencies: Phase 0 contract decision; reuse current digest work.

The layout slice uses `read_canonical_layout`'s bounded, strict parser, not
`StoreLayout` path constants as evidence. `RepositorySnapshot` now captures
the validated descriptor and rechecks it around materialization; shared
pointer verification revalidates the complete snapshot afterward. The saved
destination identity includes the descriptor digest for converged replay.
Identity is semantic: equivalent JSON formatting and layout-object ETags do
not change it. Canonical v1 still admits only its existing parameter set;
missing, malformed, unsupported and oversized descriptors fail closed.
Raw manifest reads retain their contract, and neither reads nor
`create_manifest` initialize layouts. Fixture setup must explicitly initialize
valid repositories. Tests remove, corrupt and change descriptors after open
and during scanning, retain refs/data, and cover zero pointers, converged
replay and scoped storage. Keep production-provider/managed qualification and
the hook's full publication lifetime gate open: layout comparisons neither
establish GC protection nor detect every intermediate sweep.

**Permission-safe inspection — Git, metadata and read owners.** Context:
canonical pointer lookup is non-writing, but child Git fetch/read admission
and derived maintenance are not yet qualified for read-only grants.
Deliverable: inspection through existing owners with no implicit remote
maintenance or write-credential requirement. Acceptance: deny and record every
remote PUT/DELETE/multipart attempt while checking equal, changed and missing
refs, absent acceleration, corrupt data, cancellation and provider denial;
all paths make zero write attempts. Healthy canonical data passes without an
index; incomplete reads return unverifiable. Dependencies: identity contract;
read-only GC lifetime decision in the shared proof design above. Local cache
writes remain allowed and isolated.

**Protected publication lifetime — coordination and publication owners.**
Context: a digest detects identity changes but does not prevent GC from
removing verified objects. Deliverable: one owner acquires existing admission,
revalidates the proof and retains protection through dependency flush, atomic
expected-old publication and read-back. Acceptance: a sweep entirely inside
verification, lease expiry/loss, cancellation and competing ref updates cannot
produce a false clean result or an acknowledged ref with missing dependencies.
Prove direct and managed paths plus hook/apply siblings; release every acquired
guard on each terminal path. Dependencies: identity and inspection packets;
shared Phase 1 lifetime contract. No independent parent/child lease stack.

**Declared-source recovery — staging and push owners.** Context: a source Git
pointer names content but need not contain its bytes. Deliverable: explicitly
selected, verified staging/backup source feeding the canonical upload path.
Acceptance: missing Crab content is restored, flushed and byte-verified before
ref commit; absent/corrupt recovery bytes or interrupted uploads preserve old
refs and give an actionable recovery-needed result. Resume must not duplicate
publication. Dependencies: protected publication lifetime; no implicit search
of unrelated repositories or credentials.

**Terminal outcome and replay — publication owner.** Context: equal refs on
reapply are not durable evidence that this plan committed, and a lost response
does not prove failure. Deliverable: plan-bound terminal receipt and explicit
uncertain-commit read-back using the existing commit authority. Acceptance:
drop the successful commit response, restart the caller, reapply the same plan
and recover its terminal result without a second mutation. A different plan or
unrelated writer cannot inherit that receipt; later metadata movement is new
state, not erased history or proof of current convergence. Journal unresolved
outcomes and their protected dependencies under the existing recovery owner;
never compensate by deleting data. Dependencies: protected publication
lifetime; receipt compatibility decision.

**Bounded execution and support qualification — Git and release owners.**
Context: Crab pointer scanning has explicit object/lookup/allocation limits;
Crab and LFS now share streamed blob verification and supervised subprocesses.
Native Git delta memory, LFS whole-transfer cancellation and maximum-scale
qualification, remaining process-death cases and the production matrix are
still open. Deliverable: bounded,
cancellable traversal in canonical owners and exact-candidate evidence.
Acceptance: declared RSS/spill/time limits hold as history grows; abrupt parent
death, second signals, truncated packs, expired auth and throttling fail safely.
Run the original Git/OS/provider/direct/managed matrix and prove the configured
host merge gate rejects the exact missing-data candidate. Dependencies: harness
work may overlap earlier packets; final acceptance requires all applicable
packets. An emulator pass or successful unit test cannot close a production row.

LFS discovery packet: `crab/src/lfs/discovery.rs` owns discovery through
`visit_lfs_blobs_in_git_command`, called by tree scans and exact pre-push ranges;
mirror consumes `collect_lfs_object_ids_from_range_in`. Preserve local-minus-
remote/base-manifest exclusions, path-to-pointer lock checks, explicit refs and
zero-size pointer behavior. Do not replace these with a full-history scan or a
second mirror-only collector.

1. Move reusable process ownership to the smallest shared product boundary
   required by mirror and LFS. Retain one teardown/cancellation contract;
   concurrently drain both discovery and batch stderr, including spawn failure
   after only the first child exists. Keep framing mechanics in `crab-git`.
2. Bound discovery records, batch headers, retained pointer/path inventory and
   captured diagnostics. Oversized or incomplete input must return an explicit
   error, never an empty successful inventory; preserve underlying I/O errors.
3. Validate complete raw object framing and identity before accepting a pointer.
   Qualify size-filter behavior against corrupt headers rather than treating
   a filtered-out object as a verified non-pointer.
4. Acceptance: real-Git range and tree results retain existing semantic tests;
   large stdout/stderr, blocked stdin, truncated frames, second-child spawn
   failure and cancellation stop/join every owned worker before cache release.
   Memory stays within declared limits as history grows. Re-run direct LFS,
   composed pre-push and mirror siblings; no ref publication on incomplete scans.

The supervised-LFS checkpoint below records the implemented owner, inventory
bounds and verification boundary. Keep maximum-scale acceptance and full
transfer/abrupt-process-death gates open; bounded discovery alone does not
close this packet's entire acceptance contract.

### PR slices

1. Capability matrix and generated documentation.
2. Cross-version/platform transcript and real-Git matrix.
3. Production-provider protocol rows and retained evidence.
4. Read-only mirror status/integrity check.
5. Plan/apply reconciliation and CI integration.

### Milestone handoffs

**2A — Explicit, testable Git contract.** Context: implemented protocol paths
and a local emulator result are narrower than a released compatibility claim.
Entry: Phase 0's candidate identity and support profile. Complete slices 1–2
by extending the existing matrix and real-Git harness. Exit: every declared
operation has an executable positive or expected-rejection case; unknown or
unsupported requests fail before misleading protocol output or local object
installation. Exact Git and binary versions are retained. Release claims
wait for production-provider rows, not merely the harness landing.

**2B — Observable mirror state.** Context: independent remotes can disagree,
and a client hook can be missing or bypassed. Entry: Phase 1's origin-byte and
snapshot-lifetime contracts. Complete slice 4 in the existing mirror command;
do not recreate status, cache, or process owners already in the worktree.
Exit: equal/source-ahead/Crab-ahead/diverged, unavailable source/provider,
missing/corrupt data, and absent hook produce distinct structured results.
Concurrent inspection and cache cleanup cannot mix source snapshots. Assert
zero remote writes for all read-only checks, including failures.

**2C — Safe convergence and release qualification.** Context: correct drift
detection is not safe publication or host enforcement. Entry: 2A–2B and
approved one-way reconciliation policy. Complete slices 3 and 5, including
the full pre-push batch and shared LFS input contract. Exit: mixed branches,
annotated tags, renamed destinations, rewrites, and explicit deletions publish
the frozen approved batch; a stale Crab lease rejects the batch. Inject a
lost commit response and resolve it by read-back without blind replay. Prove
an exact-candidate missing-data check blocks the configured host merge gate.
Retain real-provider/platform evidence and the original seven acceptance
criteria. Failure after Crab commit is recoverable Crab-ahead state, not an
instruction to roll either remote back automatically.

### Acceptance criteria

- [ ] Every supported matrix cell passes with the exact release binary and
      produces retained refs, strict-fsck, pointer, and byte evidence.
- [ ] Every unsupported capability has an explicit transcript test proving no
      complete-fetch fallback and no partial local installation.
- [ ] Partial-clone lazy fetch returns the requested object's exact type, size,
      and SHA-1; hidden-only objects are rejected before object reads.
- [ ] Cancellation/truncation never produces an acknowledged ref, valid-looking
      partial pack, or leaked repository read/write lease.
- [ ] Mirror status detects source-ahead, Crab-ahead, true divergence, missing
      hook, unavailable provider, and missing pointer data without optimistic
      `healthy` output.
- [ ] The required CI check fails for the exact candidate commit containing a
      pointer whose recipe/data is absent from Crab, and configured branch
      protection blocks its merge/promotion. Post-push detection is not claimed
      to prevent every remote ref update.
- [ ] Reconciliation is plan-first, idempotent, never bidirectional, and never
      deletes an unmatched ref without explicit operator approval.

These criteria include the section-4 fault cases: source/provider outages,
concurrent cache use, stale snapshots and leases, healthy-cache/corrupt-origin
disagreement, lost commit responses, and complete multi-ref hook handling.
No byte-verification or ref-publication claim is satisfied by object HEAD
requests alone.

### STOP conditions

- Compatibility requires silently changing requested Git semantics.
- Mirror design claims atomicity across independent providers.
- A protocol feature would require an always-on data server solely for parity.
- Provider qualification lacks strict Git and reconstructed-byte evidence.

### Implementation checkpoint: 2026-09-02, 15:20 UTC

Development-worktree evidence, **not release qualification**:

- Optimized binary SHA-256:
  `208a720327aa5d6e9bdbcf9fc78bbac8502df3b2dfb352e7c04338afddb2dc71`.
- Live run `phase2-native-mirror-20260902-1520`: **315 commands, 96 checks,
  passed** on macOS, Apple Git 2.50.1, and RustFS beta.12. Report and redacted
  logs remain in the dedicated workspace smoke directory under that run ID.
- Released v1.0.1 rollback binary served the promised blob byte-identically.
  The downloaded Darwin archive matched its published SHA-256; the report
  retains the extracted binary digest separately.
- Focused proof passed: 38 mirror-related tests, 34 upload-pack tests,
  5 ref-lease tests, 3 mirror-schema validation tests, schema-drift validation,
  and 10 capability-evidence verifier tests. Workflow YAML and whitespace
  checks passed. This is not a full workspace or cross-platform test result.

| Work packet | Implemented / observed | Remaining acceptance work |
|---|---|---|
| Capability authority | Machine-readable 19-operation matrix; generated docs; report checks bound to binary, source SHA, Git, OS, provider, mode, freshness, and rollback binary. Linux release job downloads checksum-pinned v1.0.1 and invokes the verifier. | Execute clean, exact-release Git-version jobs; complete production-provider and non-Linux lifecycle rows. Preview labels do not discharge the design's conformance requirement. |
| Native Git | Empty/unborn discovery no longer requires a nonexistent object catalog. Live empty clone, SHA-looking ref fetch, atomic updates, force/deletion, filters, and lazy fetch passed. | Complete missing real-client fault scenarios, including cancellation after pack output, truncation, corrupt pack/index, stale proof, expiry, and throttling; retain lease-cleanup and no-install evidence. |
| Ref leases | Git `option cas` reaches push preconditions and the under-lock recheck. Matching leases conditionally permit rewrites; stale leases reject even with force; receive policy remains authoritative. | Complete sibling managed/active-active qualification and live concurrency qualification, beyond the under-lock race regression test. |
| Mirror integrity | Read-only ref comparison, pointer-dependency availability, effective hook state, structured output, CI failure, immutable plan/apply, and explicit deletion approval. Live source-ahead, Crab-only, divergence, missing hook, provider outage, missing pointer, and idempotence checks passed. | Finish the pinned-snapshot/data-integrity audit, source-unavailable and concurrent-cache cases, and reconstructed-byte provider evidence. Recipe/hash plus xorb existence checks are not full byte reconstruction. |
| Hook migration | Correct positional push syntax; direct Crab pushes bypass the collaboration guard. Only the exact released obsolete block is migrated; adjacent/custom commands are preserved or require manual merge. | Qualify complete multi-ref collaboration-hook behavior and platform-specific execution; hooks remain non-authoritative. |

No Phase 2 acceptance checkbox is closed by this checkpoint. The seven
original criteria above remain the completion authority; do not replace them
with the subset exercised by the current smoke runner.

### Origin-byte verification follow-up: 2026-09-02

Implemented after the checkpoint above:

- `crates/crab-read/src/integrity.rs` now owns origin-only recipe verification.
  It uses the existing Xorb parser/chunk decoder, checks serialized payload
  integrity, ordered chunk ranges, segment/header counts, exact file length
  and the final Blake3 hash. One serialized xorb and one decoded chunk bound
  the payload working set. It neither installs cache data nor writes a worktree.
- Mirror pointer proof invokes that verifier after committed-index and shard
  validation. Output now distinguishes missing, corrupt and unverifiable
  dependencies; provider failures are not classified as missing data.
  Conflicting pointer sizes fail closed, and cancellation interrupts pending
  origin reads and is checked during chunk processing.
- Schemas and CLI docs include the corruption state and full-origin-read cost.
  The capability evidence gate now requires a real pointer-byte check and an
  exact-byte hydrated clone, not an empty-pointer mirror fixture.

Retained local proof:

- Optimized binary SHA-256:
  `7738658ca620140a8b26321447f118cda4f1f5c7463a88e9e0483beb9e52ec89`.
- Live run `phase2-origin-bytes-20260902-1535`: **321 commands, 98 checks,
  passed** on macOS / Apple Git 2.50.1 / RustFS. The 1 MiB fixture was staged,
  pushed, origin-verified, cloned and hydrated byte-identically. Existing
  native-Git, reconciliation and v1.0.1 rollback scenarios remained in the run.
- Eight shared origin-verifier tests, six pointer-verification tests,
  38 mirror-related tests, three mirror-schema tests, schema drift and ten
  capability-verifier tests passed. Formatting, whitespace, Python syntax and
  documentation link checks passed. Corrupt-origin/healthy-cache disagreement,
  wrong hashes/lengths/ranges, missing origin data and pending-read cancellation
  have focused regression coverage.

This remains **dirty-development-worktree evidence**, not exact-release,
cross-platform or production-provider qualification. Snapshot/GC lifetime,
source-unavailable reporting, concurrent mirror-cache isolation, full hook ref
batch handling, source-data publication/recovery, remaining protocol faults,
and the original provider/platform matrix remain open. The public verifier's
caller must supply the authoritative provider store and protect recipe
snapshot lifetime; byte verification alone does not establish those guarantees.

### Cache/source isolation follow-up: 2026-09-02

Historical checkpoint after origin-byte verification. The shared lifecycle
follow-up below supersedes the local lock owner and cleanup gap recorded here.

Implemented at that checkpoint:

- `crab/src/cmd/mirror/cache.rs` owns one native advisory lock per physical
  cache directory. Legacy mirror, check and apply share it; apply retains the
  guard through the final destination ref read. Symlink aliases use the same
  lock identity, acquisition is nonblocking, and the sibling lock file is not
  unlinked on release. This adds one owner for three existing paths, not a
  second mirror implementation or new configuration surface.
- Source clone/refresh/ref-read failure returns a structured unverifiable
  check and fails CI. Previously cached refs are not treated as current source
  evidence, and apply refuses the unavailable snapshot. Relative cache paths
  resolve before Git changes its working directory.
- The live runner now exercises cross-process contention for all three mirror
  paths, nested relative cache paths, loss of an already-cached source, refusal
  of apply, unchanged cache/destination refs, then successful apply/reapply
  after the fault clears. All five new checks are required by the capability
  evidence matrix.

Retained local proof:

- Optimized binary SHA-256:
  `f3be1d366709ee4e493edd8daf39a1ee79201cf53ffcd5937ead6177cd2c9464`.
- Live run `phase2-cache-source-20260902-1556`: **331 commands, 103 checks,
  passed** on macOS / Apple Git 2.50.1 / RustFS, including origin reconstruction
  and the released v1.0.1
  rollback client. Report and redacted logs remain under that run ID in the
  dedicated workspace smoke directory. This is dirty-worktree evidence, not
  clean release/provider/platform qualification.
- All 37 tests selected by `cargo test -p crab --lib cmd::mirror --locked`
  passed, including real Git source outages and an ownership assertion at
  every apply subprocess through final read-back. The broader `mirror` filter
  also passed all 45 tests, including hook composition and linked-worktree
  status. Ten capability-verifier tests passed. Formatting, Python syntax,
  generated matrix, whitespace, web
  tests, typecheck and link checks passed; web lint exited successfully with
  16 warnings in unrelated existing sources.

Remaining lifecycle and publication work is explicit:

1. `crab/src/cmd/cache.rs::run_cache_clean_with_cancel` and
   `crates/crab-cache/src/local_cache.rs::clean` recursively remove cache
   contents without the mirror guard. Coordinate cleanup and mirror ownership
   before claiming safety against cleanup races; never unlink a live lock to
   make cleanup succeed. Check all clean callers and preserve the tagged clean
   contract through an explicit busy/refusal outcome.
2. `SystemCommandRunner` still synchronously waits for Git. Cancellation and
   parent/child crash supervision must keep cache ownership until mutating
   subprocesses stop, including helper descendants. A released OS lock alone
   is not proof an orphan Git process stopped. The workflow child supervisor
   is an existing process-lifecycle implementation to inspect before designing
   reusable mechanics; do not add workflow journal policy to mirror.
3. Apply still inspects pointer bytes before invoking the Git-only push path;
   `phase_discover_git_only` intentionally supplies no pointer dependencies.
   Direct push admission has GC fencing in `crab-coordination::push_admission`,
   but this alone does not prove continuous protection from inspection through
   publication. Bind recipe/manifest identity and revalidation to that owner,
   including managed/active-active siblings, and inject GC between verification
   and push. Merely removing Git-only mode is insufficient: the ordinary
   `lookup_staging` path rejects pointers when bare-cache staging is absent.

The original Phase 2 criteria remain open. Complete hook batches, declared
source-data recovery/publication, uncertain commit responses, candidate-bound
CI enforcement and the complete protocol-fault/provider/platform matrix are
still required; the new passing subset does not replace them.

### Shared cache lifecycle follow-up: 2026-09-02

**Context.** Per-mirror serialization was insufficient: both the CLI cleaner
and `LocalCache::clean` could unlink an active mirror's database and lock.
Scanning existing locks without announcing cleanup first would still race a
new mirror. Configured cleanup can target a cache ancestor, the cache itself,
or a nested directory, so protecting only the default mirrors folder is not
sufficient.

**Design and implementation boundary.**

- `crates/crab-cache/src/lifecycle.rs` is now the single owner of cooperative
  local directory admission and recursive cleanup. It replaces the unshipped
  mirror-only lock module. The feature remains optional under `local-cache`;
  the minimal crate does not gain runtime or locking defaults. The lockfile
  adds only the already-resolved `fs4` dependency edge, not a new version.
- `CacheUseGuard` takes a persistent sibling `.crab-cache-use.lock` and probes
  overlapping cleaners/users before permitting mutation. `CacheCleanGuard`
  announces a sibling `.crab-cache-clean.lock` before probing ancestor and
  descendant owners. Either ordering rejects at least one contender before
  data mutation. There is no blocking lock wait or retry loop.
- Legacy mirror, integrity check, and plan apply use the guard's physical
  canonical path. Apply holds it through final ref read-back. Both cleanup
  entry points use the shared cleaner; the CLI acquires every configured root
  before deleting from any. Main and optimize aliases forward caller
  cancellation instead of substituting an inert token.
- Cleanup never unlinks coordination markers, including idle inodes that an
  arriving contender may already have opened. Parent directories needed for
  nested markers remain. Counts exclude markers and record actual removed
  data, not a pre-clean inventory. Root directories are retained, directory
  symlinks are not followed, and removed/empty mirror caches are cloned again.
  Nonempty invalid directories are not treated as new caches.
- Ownership discovery streams entries and checks cancellation; it does not
  materialize a whole-tree file list. Existing-cache acquisition still scans
  descendant metadata to detect nested owners. This is O(entries) work, not a
  constant-time scalability claim. Large-cache admission latency remains a
  performance qualification item.

**Acceptance for this increment.**

1. An active owner blocks ancestor, exact-directory, and nested cleanup;
   cleanup blocks newly arriving owners, including during first creation.
   Simultaneous admission never grants both conflicting operations.
2. A busy second configured root prevents deletion from the first. Both CLI
   aliases and the library cleanup path share that admission contract.
3. Cleanup preserves previously opened lock inodes, excludes them from counts,
   respects cancellation, and does not delete through a directory symlink.
4. The optimized binary refuses cross-process contention before modifying
   refs/cache contents; idle isolated cleanup succeeds; the next mirror check
   reclones with identical refs, strict Git fsck, and origin pointer proof.
5. Minimal/shared feature checks and native lifecycle tests pass; Linux,
   macOS, and Windows CI run the shared test set. Local macOS proof alone does
   not close the other platform rows.

**Retained proof.** Optimized binary SHA-256
`eb2c72fd1dec42184a5f55589f0fcfcbb15155eeb16a098c40fa6f1ab86b309e`;
run `phase2-cache-clean-20260902-1630`: **345 commands, 107 checks, passed**.
Report/logs remain in that run's `artifacts/report.json` and `logs/` under the
dedicated workspace smoke directory. Platform: macOS, Apple Git 2.50.1,
RustFS, direct mode, released v1.0.1 rollback binary. This is local
dirty-worktree proof, not a clean release-candidate or other-provider pass.
Both cleanup CLI aliases refused an externally held use lock without changing
refs; cleanup ownership blocked check/apply; idle cleanup removed only the
explicit test cache; rebuilding passed strict fsck and origin pointer proof.
All four new checks are required in the capability evidence matrix.

Local verification also passed 101 shared-cache tests, all 44 tests selected
by the broader `mirror` filter, five CLI-cache tests, ten capability-verifier
tests, minimal-feature checking, and library Clippy with warnings denied.
All-target cache Clippy stops on two pre-existing `useless_vec` warnings in
`local_cache.rs` (`test_xorb(&vec![3; 50])` and
`test_xorb(&vec![2u8; 80])`, present at the planning baseline); unrelated tests
were not edited to hide them. Rust formatting, whitespace, Python syntax,
generated matrix, and workflow YAML parsing passed. Web: nine tests,
typecheck, link checks passed; lint succeeded with 16 existing unrelated
warnings. Workflow native lifecycle steps are wired for Linux/macOS/Windows
but have not been run remotely in this checkpoint.

**Remaining boundaries.** These are advisory local locks for cooperating
directory owners, not protection against arbitrary external deletion or every
immutable cache fill. Marker preservation intentionally trades a few local
files/directories for stable lock identity. Partial cleanup on cancellation or
I/O error is not rolled back. CLI syntax is unchanged; the library preview
path remains nonmutating, but neither clean CLI alias exposes `--dry-run`.

At this checkpoint there was a remaining cache-recovery case: a prior cleanup
or owner inside a
bare cache can leave nested coordination markers. Full cleanup must preserve
them, leaving a nonempty directory without a Git database. The current
`cache_needs_clone` deliberately refuses to overwrite that nonempty path; it
does not yet rebuild a marker-only skeleton. Use a fresh explicit cache path
for now. Follow-up acceptance: create a nested owner/cleaner marker, clean the
parent while idle, then rebuild the original cache without unlinking any
previously opened marker inode. Choose one canonical initialization path;
do not solve this by deleting supposedly stale locks or hiding a second
fallback clone implementation.

Subprocess cancellation and parent-crash/descendant lifetime remain open:
dropping a lock after the parent dies does not prove its Git children stopped.
Manifest/recipe snapshot binding through GC-safe publication, full pre-push
stdin batches, declared source-data recovery, uncertain-response resolution,
candidate-bound host CI enforcement, and the full protocol/provider/platform
matrix remain required. No original Phase 2 checkbox is closed by this
increment.

### Canonical mirror cache rebuild and source completeness follow-up: 2026-09-03 UTC

**Context.** A marker-only cache cannot be cloned into, and deleting lock
markers would split ownership between old and new inodes. Inspection also
must not equate a successful fetch process with complete source refs: Git
can omit shallow-source refs while returning success.

**Implementation.** `crab/src/cmd/mirror/cache.rs` now owns one preparation
path used by legacy mirror and integrity check/apply. Missing, empty, and
marker-only caches use `git init --bare --object-format=sha1`; unrelated data
must already form a valid bare Git repository or is rejected. The same
refresh replaces the owned origin URL, mirror refspec, and mirror setting,
making a partially completed initialization retryable without a second
initialization mode. Normal caches with a HEAD file avoid a second full-tree
preview scan.

Each refresh advertises source refs and HEAD, fetches `+refs/*:refs/*` with
pruning, and compares the resulting complete ref map to the advertisement.
Source movement or omitted refs fails before either caller can use the cache
as a current source snapshot. Source deletion/forced tag updates remain Git
mirror semantics; this does not authorize deleting Crab-only refs. Symbolic
HEAD is restored; detached HEAD's exact object is fetched without publishing
a synthetic ref. An unadvertised/unborn HEAD does not invent a source branch;
the first advertised branch is adopted on the next refresh.

Repository-selection environment variables cannot redirect initialization,
configuration, or object writes into another repository. Source and Crab-side
Git fetches disable automatic maintenance so a background GC cannot outlive
the mirror's directory owner. Full subprocess cancellation/parent-crash
supervision remains separate required work.

**Dependency contract.** Git init permits existing directories without
overwriting existing files; the Crab admission check limits this to empty or
coordination-only state. Git 2.30.9 supports `--object-format` and
`--no-auto-gc`, but not `fetch --atomic`; the latter appeared in Git 2.31's
fetch documentation. Use one ordinary-fetch path across the declared
versions, protected by directory ownership and post-fetch comparison, not a
version-dependent fallback. These checks add an advertisement round trip and
a local ref listing. They do not establish a constant-time or large-history
performance claim.
Contracts checked against [Git init](https://git-scm.com/docs/git-init),
[Git fetch](https://git-scm.com/docs/git-fetch), and the tagged
[Git 2.30.9 fetch options](https://github.com/git/git/blob/v2.30.9/Documentation/fetch-options.txt).

**Acceptance and verification scope.**

- Repeated parent cleanup/rebuild retains a previously opened nested lock
  inode; the resulting cache has exact branch/tag/notes refs, original file
  objects, and strict Git fsck.
- Detached-only HEAD objects and empty-source-to-first-commit transitions
  work without extra mirrored refs.
- Interrupted origin setup resumes; duplicate stale origin settings are
  replaced; source branch deletion and tag rewriting refresh exactly.
- Unrelated nonempty directories are not initialized or overwritten.
- Shallow-source omissions and source movement cannot produce a verified
  source snapshot; a stable retry can succeed.
- The optimized CLI repeats marker-only cleanup/rebuild against real origin
  pointer bytes and proves ambient Git paths did not mutate a separate
  repository. Native mirror-cache contracts are also wired into the macOS and
  Windows protocol workflow; wiring is not a claim those jobs passed.

**Observed proof.** All 51 mirror tests passed, including eight real-Git cache
contracts. The correctness/suspicious Clippy gate and optimized build passed;
the wider Clippy invocation still emitted 379 nonfatal warnings, with no
diagnostics in the new cache module. Rust formatting, smoke-script syntax,
workflow YAML parsing, generated capability-matrix verification, and all ten
matrix-verifier tests passed. Web proof: nine tests, typecheck, lint (16
pre-existing warnings), and link validation (398 pages, 4,297 fragments).

The optimized-binary live run `phase2-cache-rebuild-20260903-0034` passed all
354 commands and 111 checks, including marker-only rebuilding with stable
lock inodes, ambient Git repository-path isolation, shallow-source deletion
refusal, exact plan/apply publication, and idempotent reapply. Its retained
report is `artifacts/report.json` under that run in the external smoke
workspace. Binary SHA-256:
`72e644a4c86dd069f36a425582a0a4db44baeab71d5e3fc7efc4d7924000cf41`.
Scope: macOS, Apple Git 2.50.1, RustFS 1.0.0-beta.12, direct mode, dirty local
worktree based on `63bfc8c9648dbe790470e2fecbc2abb8529a39a1`, with the retained
v1.0.1 rollback binary. This is local implementation evidence, not an
exact-release, cross-platform, managed-mode, or real-cloud qualification row.

This closes the marker-only rebuild implementation gap recorded above; all
original Phase 2 acceptance boxes remain open until their full evidence and
publication/lifecycle contracts are satisfied.

### Supervised mirror subprocess follow-up: 2026-09-03 UTC

**Context.** The previous synchronous runner wrote the entire stdin payload
before draining stdout/stderr. A child that filled stdout before consuming
stdin could deadlock. Cancellation checks between commands could not interrupt
that wait, and pipe errors could return without reaping the child. These paths
undermined the mirror cache owner's lifetime boundary.

**Implementation.** `crab/src/cmd/mirror/process.rs` owns the single runner used
by legacy mirror and check/apply. Its cancellation token comes from each
public entry point. Up to three short-lived scoped workers concurrently feed
stdin and drain both output pipes. The calling thread supervises exit and
cancellation; worker joining precedes return to the cache owner. An I/O or
worker-start failure stops the child tree before joining already-started
workers. Exit status and existing text/JSON output routing are preserved.

Unix children have a dedicated process group. Cancellation sends SIGTERM,
allows up to ten seconds for child shutdown, then escalates to SIGKILL.
Ordinary completion also stops leftover group members before returning; a
successful leader may not leave an unsupervised background cache writer.
`waitid(WNOWAIT)` observes exit without reaping the leader, keeping its group
identifier reserved until signaling is complete. On macOS, an all-zombie
group returns EPERM rather than ESRCH. The runner accepts that result only
after checking the retained leader's exit and enumerating the group to prove
there are no live members. A truncated enumeration grows and retries; an
uninspectable or live member is not proof of successful cleanup.

Windows uses a job object, with child creation suspended until job assignment.
Cancellation terminates the job, then waits for its processes. Polling targets
the native leader handle, not the wrapper's completion-port-consuming
`try_wait`; the final job wait retains those completion events. Windows does
not yet have the Unix graceful SIGTERM window.

**Dependency and sibling evidence.** Added `process-wrap` 10.0.0, with only
`std`, `process-group`, and `job-object` features. Reviewed its spawn/error,
group signaling/wait, Windows suspended assignment/resume, job completion,
and handle-drop implementations. The lockfile adds this one package and
reuses existing indexmap/nix/windows versions; no overrides or patches.
The workflow supervisor remains a separate workflow/logging owner: it has
different async log/journal behavior and is not silently substituted into
mirror. Both actual mirror public entry points now construct the same
cancellable runner; simulated runners remain test-only.
Contracts: [process-wrap 10.0.0](https://docs.rs/process-wrap/10.0.0/process_wrap/),
[waitid](https://man7.org/linux/man-pages/man2/waitid.2.html), and Apple's
[group-signal implementation](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_sig.c).

**Acceptance for this increment.**

- A real child writes 1 MiB to each output pipe before consuming a 2 MiB
  input; execution completes with exact output/input evidence.
- A pre-cancelled invocation never spawns. A broken stdin pipe stops and
  reaps a still-running child rather than waiting for its natural timeout.
- Cancellation stops a descendant heartbeat writer before the mirror cache
  owner returns; cleanup is refused while the owner is active and admitted
  afterward. A leader that exits with closed-pipe descendants cannot leave
  those descendants writing afterward.
- Unix child shutdown can persist its completion marker before termination;
  a child ignoring SIGTERM is killed after the ten-second grace period.
- A completed nonzero child status survives cleanup unchanged.
- The optimized CLI is cancelled while a real Git upload-pack invokes a
  pack-generation hook. The hook records graceful shutdown, the command
  returns cancellation, cache cleanup becomes available, and destination
  refs remain unchanged. This live signal injection is POSIX-only; Windows
  requires separate native CLI signal evidence.

**Open boundaries.** This is not full cancellation/lease acceptance. Abrupt
parent death (including the second-signal forced exit), helpers that leave
their Unix group, and fatal failures during process-tree cleanup still need
an ownership design and fault qualification. Force-killed metadata writers
need crash/recovery evidence; a process exit alone does not prove SlateDB
closed gracefully. Lost/uncertain ref-publication responses still require
read-back resolution. The synchronous command API also still blocks its
calling thread; this increment adds pipe concurrency, not an async mirror
API or bounded output-memory/performance claim. macOS zombie-group checks
add native process-enumeration work. Full hook batches, snapshot/recipe/GC
publication binding, recovery, candidate-bound CI, and the complete
release/provider/platform matrix remain open. No Phase 2 checkbox closes.

**Observed proof.** All 59 mirror-filtered tests passed, including seven new
subprocess regression tests and their isolated child fixture. The Unix grace
test exercised both a cooperative shutdown and ten-second escalation. The
correctness/suspicious Clippy gate passed; the final new process module has
no Clippy diagnostics, while unrelated repository warnings remain. The
optimized build, Rust formatting, whitespace, workflow YAML parsing, Python
syntax, generated matrix, and ten verifier tests passed. Web: nine tests,
typecheck, lint (16 existing warnings), and 398-page/4,297-fragment link check.

Live run `phase2-process-cancel-20260903-0105` passed **361 commands and 114
checks**, with the three new cancellation checks included. The cancelled
real-fetch command returned the cancellation exit code (10); concurrent
cleanup returned busy (5), and cleanup after cancellation succeeded (0).
The hook's shutdown marker existed before mirror returned, and the retained
before/after ref sets matched. Its report remains in `artifacts/report.json`
under that run in the external smoke workspace. Binary SHA-256:
`738f8f907137d5485bb6904760d4520de3b8f52b6366853137dcb4079dd23843`.
Scope: macOS, Apple Git 2.50.1, RustFS, direct mode, dirty local worktree,
released v1.0.1 rollback binary. Linux/Windows native test jobs are wired but
not remotely run; this is not a release/provider/platform qualification claim.

The earlier `phase2-process-cancel-20260903-0100` run stopped at fixture setup:
Git clears command-scope parameters before local upload-pack, so its hook
never ran. The corrected fixture uses an isolated XDG global Git config,
supported by the tagged Git 2.30.9 configuration reader, without editing the
user's global config. Retain the failed run; it is not cancellation evidence.

The runner's net code growth replaces the old pipe-write/wait implementation
with explicit pipe-worker ownership, escalation, PID-safe teardown, and
macOS permission-error discrimination. There is one execution path, not a
second fallback runner. Added native process enumeration and per-command
worker overhead still need workload-scale measurement.

### Complete pre-push batch admission: 2026-09-03 UTC

**Context and current behavior.** The mirror installer still emits
`crab push crab`, which resolves the current branch rather than Git's stdin
update set. LFS already parsed its full input before uploading, but discarded
destination names, accepted inconsistent deletion markers and mixed object
formats, did not reject duplicate destinations, and read without a byte cap.
Its installer runs the LFS stdin consumer before the mirror block. Simply
adding a second stdin reader would silently give the mirror an empty batch.

**Implemented boundary.** `crates/crab-git/src/pre_push.rs` owns a bounded
whole-batch decoder. It returns exact local object IDs, fully qualified
destination refs, and advertised remote object IDs in input order. Deletion
and creation use explicit absent OIDs. A local revision expression or branch
name is never re-resolved after Git has selected the pushed object. The
advertised old OID belongs to the collaboration remote; it is not a lease
for a different Crab remote. Input errors and ref-validator sources retain
their error chain.

`crab/src/cmd/lfs/push.rs` uses this canonical decoder with a 16 MiB admission
cap, then selects non-deletion object ranges for its existing lock-check and
upload path. It validates the batch before resolving cloud access. Empty and
deletion-only input needs no cloud credentials. Manual hook callers must
supply newline-terminated records; malformed, duplicate, mixed-width,
non-UTF-8, and oversized input fails closed. SHA-256 decoding preserves the
LFS parser's object-ID shape support, not a claim of native SHA-256 transport.

**Evidence map.** CLI entry: `LfsCmd::PrePush` in
`crab/src/cmd/lfs/mod.rs`; orchestration: `run_lfs_pre_push`; shared owner:
`crab_git::pre_push`; callee: existing `validate_push_refname` / locked
`gix-validate` 0.11.1; publication sibling: existing LFS range collection,
lock checking, and `BatchResolver`. Ordinary `crab lfs push`, object-ID push,
and native remote-helper wire parsing do not consume hook stdin and retain
their own contracts. No dependency or error-code catalog additions.
Upstream: [Git pre-push contract](https://git-scm.com/docs/githooks#_pre_push).

**Acceptance for this increment.**

- Decode actual Git output from one atomic five-ref push: renamed branch,
  annotated tag, revision expression, forced rewrite, explicit deletion;
  independently observe the exact resulting remote refs.
- Reject a malformed later record without returning a valid prefix. Preserve
  read failure and invalid-ref causes; read no more than limit plus one byte.
- Keep LFS revision collection based on exact OIDs and exclude deletions
  without substituting the current branch.
- Drive the real CLI without any Git repository: empty and deletion-only
  batches succeed, duplicate/oversized batches fail at input admission, and
  none creates repository/cache state.

**Remaining hook execution packets, in order.**

1. **Publication owner.** Add one hook dispatch path accepting the decoded
   batch, not per-line `crab push` calls. Resolve the configured Crab remote;
   capture its independent old refs and apply policy before constructing one
   atomic canonical push. Use supplied local OIDs and destination names.
   Re-check expected-old values under the existing lock/CAS owner. Do not
   overwrite Crab-ahead/diverged state merely because the source hook ran.
   Acceptance: mapped branches/tags and rewrites publish the exact snapshot;
   one denied/stale ref leaves the whole Crab ref batch unchanged; only
   explicitly requested, policy-approved deletes affect Crab.
2. **One stdin owner.** Compose LFS and mirror checks around a single decoded
   batch, preserving LFS lock checks and failure propagation before refs are
   published. Respect effective `core.hooksPath`; skip recursive Crab pushes.
   Recognize only owned and exact tagged hook blocks. Preserve custom hooks
   and their input, or refuse installation with manual-merge guidance before
   changing them. Acceptance: both install orders, reinstall, LFS uninstall,
   custom stdin consumers, and failing guards retain correct behavior; an
   empty hook batch never becomes a default-current-branch push.
3. **Durable-data proof.** Bind candidate pointer/recipe verification to the
   exact supplied object set. Upload only from actual available bytes through
   the canonical path; missing/corrupt/unverifiable origin dependencies block
   publication. Acceptance: a pointer reachable only through a pushed tag or
   non-current branch cannot escape verification; caches do not hide missing
   origin bytes. Keep the separate GC/snapshot protection work open.
4. **Live hook qualification.** Retain optimized-binary, real Git, real store
   evidence for mixed batches, detached HEAD, source ref movement, rejection
   by the collaboration host, and `--no-verify` bypass. Check destination refs
   and reconstruction independently after each action/failure. Execute the
   native platform and declared provider/version rows; configured host CI
   remains the separate enforcement authority.

**Verification scope.** Local macOS / Apple Git 2.50.1: ten shared decoder
tests (including the real five-ref Git push), sixteen LFS push tests, and
three CLI admission tests passed. Minimal-feature decoder tests and shared
crate all-target Clippy with warnings denied passed. Product-library Clippy
passed its correctness/suspicious gate with existing repository warnings;
the modified admission surface had no diagnostics. Rust formatting,
whitespace checks, and workflow YAML parsing passed. Web: nine tests,
typecheck, and 398-page / 4,297-fragment link checks passed; lint had zero
errors and sixteen existing warnings.

The workflow now selects the decoder/LFS/CLI tests on Linux, macOS, and
Windows, but these updated CI jobs have not run remotely. The real Git capture
fixture currently runs on Unix; Windows runs decoder and CLI admission
contracts. No new release binary or provider smoke was qualified for this
increment; the preceding optimized-binary report predates these parser
changes and must not be cited as their release proof.

The mirror hook is not batch-aware yet; no original Phase 2 acceptance
checkbox is closed here. The decoder replaces the LFS parser rather than
adding a fallback; its net production growth pays for destination/deletion
validation, an explicit update model, and a bounded shared admission owner.

### Exact pre-push publication and deletion visibility: 2026-09-03 UTC

**Implemented.** `crab/src/cmd/mirror/pre_push.rs` is the single owner for
installed mirror pre-push input. It decodes the entire bounded batch before
publication, preserves Git's frozen source OIDs and destination mappings,
captures Crab's own expected-old values, and passes an atomic batch to the
existing native push pipeline. An already-published candidate permits retry;
an independently advanced/diverged Crab ref blocks the whole batch. Empty
input never falls back to the current branch. Direct Crab URLs skip mirror
recursion. The hook validates configured push destinations and limits native
publication to SHA-1 even though the shared decoder accepts SHA-256 records.

The former recovery-only prepared-push function now serves both recovery and
mirror callers; there is no second ref/pack publication implementation.
Mirror batches require a full captured-object walk rather than a local
push-state shortcut. After Crab publication, the hook verifies origin pointer
bytes and re-reads destination refs before allowing collaboration publication.
This ordering does **not** yet prove continuous dependency protection or turn
the two remotes into one transaction. A later failure can leave Crab ahead.

LFS installation and mirror installation now compose one decoded input owner
in either order. LFS uploads target the captured Crab URL. Uninstalling LFS
preserves mirror publication. Known released Crab blocks migrate; unknown
custom pre-push content is preserved with an explicit manual-composition
error. Appending two stdin readers is not a safe migration. `core.hooksPath`
and exact known-hook status remain part of the contract.

Native discovery and follow-tag reachability now peel supplied object targets,
including frozen annotated-tag OIDs, without requiring source keys to be
`refs/tags/*`. Manifest peeling hints keep their existing namespace-limited
contract. This does not add arbitrary tag-to-tree/blob support.

**Failure found and fixed by E2E.** The mixed rewrite/tag-deletion run exposed
`catalog visibility deletion old ref tip is absent` on the next Git read.
The catalog handoff gathered target tips and update deltas, but omitted a
deleted tip that occurred in neither. The shared metadata owner now includes
old tips in its bounded locator lookup before validating/removing a ref's old
closure. A regression test failed with the same error before the fix and
passes for both branch and tag deletion afterward. No membership validation
was removed, no schema changed, and no fallback/rebuild path was added.

The sibling digest-index compactor already removes the named materialized
closure without this ordinal lookup. New-ref and rewrite catalog edits retain
their existing delta/base validation; they share the corrected lookup owner.
Direct read repair, protected receive, ACL-view publication, and repack use
the same `ensure_catalog_bound` boundary. Shared tests and a service-crate
compile check cover that ownership; they are not managed-service E2E proof.

**Retained local evidence.** Run `phase2-hook-batch-20260903-final` passed
**410 commands / 125 checks**, including all **40** matrix-required mirror
checks, on macOS / Apple Git 2.50.1 / direct RustFS. Its optimized binary
SHA-256 is
`ba67a37974657619ec652fec1276076e9118a116c1c316831f4f3d7d22dd102b`.
The report and logs remain in the dedicated workspace smoke directory under
that run ID. The report binary digest matches the final local release build.
The intermediate passing `phase2-hook-batch-20260903-0155-fixed` and initial
failing `phase2-hook-batch-20260903-0155` reports are retained, not overwritten.
The harness pins both the remote helper and the
`crab` executable invoked by hooks to avoid testing an ambient installed CLI.

The fixture performs a real initial collaboration push through a custom hook
directory; mixed renamed branches/revision expressions/annotated tags and
deletion while HEAD is detached; a fresh clone with exact Crab and LFS bytes;
host rejection followed by retained Crab-ahead state and convergent retry;
forced rewrites and explicit tag deletion followed by fetch/strict fsck; and
an independently advanced Crab ref that rejects an entire subsequent batch
without changing either remote. Existing protocol, cache, reconciliation,
failure, and released-v1.0.1 rollback scenarios remain in the same run.

Focused proof: 33 remote-index visibility tests, 15 minimal-feature visibility
tests, 8 shared tag tests, 18 native-push tests, 55 mirror tests, 21 LFS-install
tests, 16 LFS-push tests, 8 installation tests, 4 real-CLI admission tests,
34 recovery tests, and 10 offline capability-verifier tests passed.
`crab-auth-server` compiled. Shared Git all-target and metadata production-lib
Clippy passed with warnings denied. Metadata all-target Clippy found four
unchanged baseline test warnings (two needless borrows and two cloned-slice
arguments); these were confirmed in HEAD, not suppressed. Product-library
Clippy passed the correctness/suspicious gate with 492 existing warnings;
the final install and new hook owners had no diagnostics. Web tests (9),
typecheck, links (398 pages / 4,297 fragments), formatting and YAML checks
passed; web lint had no errors and 16 existing warnings.

**Still open.** Exact manifest/recipe identity and GC-safe lifetime through
publication; bounded/cancellable whole-history pointer and LFS traversal;
missing-source-byte recovery/publication; uncertain commit-response resolution;
exact-candidate host enforcement; remaining protocol/crash fault cases; and
the full Git/OS/production-provider/managed matrix. The newly expanded CI
contracts have not run remotely. Local dirty-worktree evidence closes none
of the original seven Phase 2 acceptance checkboxes.

The next implementation boundary is `StoreChecker::verify_pointer_data`:
it currently opens its own current file-index snapshot, closes that reader
before origin-byte verification, and returns counts/issues rather than a
snapshot-bound proof identity. `MirrorReconciliationPlan` binds ref maps and
pointer count, not manifest and recipe identities. Close this shared boundary
for check, plan/apply, and the new hook together. Preserve scoped-store read
permissions (`FileIndexLookupSession::open_for_storage` exists because ordinary
SlateDB readers may write checkpoints); a successful direct-store test cannot
prove the managed/read-only grant path.

### Canonical recipe lookup and snapshot revalidation: 2026-09-03 UTC

**Implemented.** `FileIndexLookupSession::from_snapshot` consumes the caller's
captured repository inventory and scoped storage layout without opening
SlateDB or writing checkpoints. The shared anchor calculation includes active
journal shards. Canonical duplicate-recipe selection chooses the minimum shard
hash independently of request completion order; the scan folds completed
batches instead of collecting every shard's duplicate hits. The remaining
inventory and requested-file sets are still materialized, so this is not the
full bounded-history scale gate.

`StoreChecker::verify_pointer_data` now uses that canonical lookup instead of
acceleration-only records. An absent file index no longer labels intact
canonical data missing. It preserves scoped repository/global paths, verifies
origin bytes, and re-reads the complete repository snapshot before returning.
Manifest or uncompacted-journal movement returns an error, which mirror check
reports as unverifiable. Snapshot reads and recipe lookup honor cancellation.
No new persistent format, compatibility path, or publication owner was added.

Sibling boundaries remain intentional: fsck's index-damage diagnostics and
repair still inspect acceleration records because they are diagnosing that
index, not proving the absence of canonical file bytes. Their existing tests
pass. Ordinary accelerated readers retain their existing session lifecycle;
the new constructor is explicitly non-writing and does not acquire GC safety.
The selected SlateDB 0.15.0 source confirms that its default reader mode creates
managed checkpoints; neither a read-only label nor a raw store wrapper removes
that behavior.

**Proof.** Eleven shared lookup tests pass, including denied-write credentials,
scoped layouts, captured-vs-newer manifest reads, deterministic duplicate
selection, and the existing journal/acceleration paths. Twenty-six store-checker
tests pass, including intact data with no index and a real metadata CAS or
journal commit injected during origin reading; both movements reject a clean
result without any checker write attempts. Fifty-five mirror tests and eighteen
native-push tests pass. Minimal-feature metadata and the auth-server consumer
compile; shared production Clippy passes with warnings denied. Shared all-target
Clippy has four baseline diagnostics and none in the lookup owner; product
Clippy passes the correctness/suspicious gate with 492 existing diagnostics and
none in the store checker. Ten offline evidence-verifier tests, nine web tests,
typecheck, formatting, and link checks pass. Web lint has zero errors and sixteen
existing warnings.

The optimized-binary RustFS run `phase2-canonical-snapshot-20260903-0225`
passes **425 commands / 126 checks**, including all **41** required mirror
checks, on macOS / Apple Git 2.50.1 / direct RustFS. Binary SHA-256:
`784c16aa28f4a90b067ea93df64b47df9982cec3ba68e08d423953c3af2c82a1`.
The new fault fixture deletes only its isolated repository's twelve rebuildable
file-index objects, retains canonical manifests/shards/xorbs, then proves the
same file bytes and verifies inspection did not recreate the index. Subsequent
clone/hydrate, hook, reconciliation and failure checks pass. This is local
dirty-worktree evidence, not a released provider/platform qualification.

**Still open.** One snapshot shared with Git ref inspection; exact
manifest/recipe proof identity in saved plans; continuous GC-safe dependency
lifetime through publication; bounded whole-history pointer/LFS traversal;
declared-source recovery; uncertain-commit read-back and terminal receipts;
candidate-bound host enforcement; remaining crash/protocol cases; and the full
Git/OS/production-provider/managed matrix. Git subprocesses may still write
read-admission or derived metadata, so the entire `--check` command and its CLI
help must not be treated as qualified for a strictly read-only grant. The web
reference now states that limit. No original Phase 2 checkbox is closed.

### Shared snapshot and recipe identity qualification: 2026-09-03 UTC

**Implemented.** `RepositorySnapshot::digest` streams the complete captured
manifest, CAS token and materialized journal into a domain-separated hash.
The existing manifest Git-validation digest and persistent metadata formats
are unchanged. Mirror inspection now captures one Crab snapshot for ref
classification and `StoreChecker::verify_pointer_data`; changed Crab objects
are fetched using captured OIDs rather than mutable ref names. The hook also
passes an explicit post-publication snapshot into verification and compares
the complete snapshot again before returning.

Pointer proof binds every unique file hash/size, selected immutable shard and
serialized recipe, including ordered terms and optional verification data.
Input order and duplicates do not change the digest; incomplete coverage has
no digest. Saved plans include metadata and recipe identities and are
recomputed before mutation. Even an empty plan rejects a metadata-only change.
Already-converged nonempty plans still use state-based replay, not a durable
receipt; that separate acceptance gate remains open.

**Proof.** Nineteen shared manifest-store tests and twenty-eight store-checker
tests pass. These include stable captured identity, non-Git metadata/journal
changes, pointer ordering/duplication, two recipes reconstructing equivalent
bytes with distinct proof identities, and metadata movement during verification.
Fifty-seven mirror tests, all seventy-one schema validation tests, schema drift,
and ten offline capability-verifier tests pass. Shared metadata Clippy passes
with warnings denied; minimal storage-feature and auth-server consumer checks
pass. The minimal check has one existing visibility dead-code warning. Product
Clippy passes its correctness/suspicious gate with 492 diagnostics, including
two pre-existing mirror-module warnings and none in the new proof owners.
Formatting and diff checks pass. Nine web tests, typecheck and link validation
pass; web lint reports zero errors and sixteen existing warnings.

The optimized-binary run `phase2-proof-identity-20260903-0258` passes
**437 commands / 128 checks**, including all **43** required mirror checks.
Binary SHA-256:
`4663740b12f0f6bff32535a4471995f6992a3e06a8fdab84afcc7b5619c3597a`.
The report's binary identity was checked against that binary. Both new fixtures
save a verified plan, CAS only the isolated mirror manifest's session metadata,
then invoke real apply: the zero-action and source-ahead plans both reject
with the expected protocol error. Refs, saved plans and the changed canonical
manifest remain intact, while fresh checks verify the same origin bytes and
recipe digest under a different snapshot identity. The fixture does not delete
canonical objects; the existing acceleration-loss fixture removes only its
isolated derived index. Subsequent reconciliation, clone/hydrate and hook tests
pass. Evidence remains dirty-worktree macOS / Apple Git 2.50.1 / direct RustFS,
not a release or production-provider qualification.

**Next boundary.** Qualify physical endpoint/layout identity before treating
the current namespace fields as a complete repository binding. The direct auth
constructor still uses the bucket as `BucketIdentity.host`; other consumers
use that field as an Azure account or cache key. Resolve those sibling contracts
at the storage/auth owner, not through a mirror-only environment hash. Whole-
command permission-safe inspection, continuous GC protection, declared-source
recovery, uncertain-commit receipt/read-back, bounded history/process death,
candidate-bound host enforcement and the full platform/provider/managed matrix
remain open. No original Phase 2 acceptance checkbox is closed.

### Transport target and converged replay binding: 2026-09-03 UTC

**Implemented.** The storage provider constructor now derives a separate,
credential-free target digest from the exact selected builder: provider,
bucket/container, effective endpoint, and addressing context. S3-specific
endpoint precedence follows the selected `object_store` 0.14.1 contract. GCS
captures its explicit/service-account/default base URL and pins it before
build, preventing credential-file replacement from redirecting the captured
target. Azure binds account, endpoint and emulator/fabric addressing. URL
normalization preserves path case; credential-bearing endpoint URLs are
rejected without echoing their content. GCS shape validation also avoids
typed JSON errors that could echo a malformed credential scalar.

This is transport-configuration identity, not a claim that every equivalent
route to the same physical bucket shares a digest. Existing `BucketIdentity`
normalization, Azure-account consumers, cross-scheme comparisons and local
cache keys are unchanged. Provider/auth constructors carry the new digest;
raw store wrappers remain unidentified. Refresh refuses a changed target
before replacing its current store or retrying, while same-target secret
rotation remains supported. Gateway grants bind the service endpoint and
retain their repository scope. Staging/read transport equivalence remains
the managed grant owner's contract, not an arbitrary digest equality check.

Mirror persists `destination_identity` separately from mutable metadata
identity, and binds both into the plan. Every apply checks destination
identity before accepting converged replay. This closes the nonempty-plan
shortcut that otherwise could report success against another target with
the same refs/recipes. Empty plans still require the original full snapshot;
nonempty replay remains state-based rather than receipt-backed. Missing
identity blocks planning/apply. Unshipped plan/check schemas are regenerated
in place; no persistence compatibility reader or new configuration option
was added.

**Proof.** All 140 storage tests and fourteen refresh-enabled auth-store tests
pass. Real HTTP fixtures read identical objects from two independent S3
endpoints and prove distinct target identities, including the dependency's
S3-specific endpoint precedence. A GCS HTTP fixture replaces its credential
file after capture and proves the built store still reads the original
endpoint. Refresh tests cover proactive and auth-error refresh, preserve the
old store on target mismatch, and retain same-target rotation behavior.
All fifty-nine mirror tests pass. Real-Git command tests create immutable
plans and exercise empty and nonempty
converged replay against identical repository state on a changed target;
rejection preserves metadata. The nonempty unit fixture models publication
with a manifest CAS; it is not a second production-publication implementation.

All seventy-one schema tests and schema drift pass. Storage/auth-store
production Clippy passes with warnings denied; minimal auth-store and
auth-server consumer checks pass. The refresh test build retains one unrelated
gateway-test unused import warning. Product Clippy's correctness/suspicious
gate passes; pre-existing mirror-command and storage-wrapper warnings remain,
with none in the new identity/replay implementation. Nine web tests, typecheck
and link validation pass; web lint has zero errors and sixteen existing
warnings. Ten offline capability-verifier tests, formatting and diff checks
pass. Net production growth is intentional: one provider-owned target
derivation plus propagation and replay validation; no alternate storage path,
environment mirror hash or compatibility layer was introduced.

The final optimized-binary run `phase2-transport-replay-20260903-0352`
passes **437 commands / 128 checks**, including all **43** required mirror
checks. Its recorded binary identity matches the actual binary SHA-256:
`28c2ec139a3dd46177ff146fbf9d303bc76b2cd7a9363b5b2da99c58384cb725`.
This includes exact ref publication, same-target converged replay, stale
metadata rejection, pointer-origin verification, hooks, cache/cancellation
and approved deletion cases. Evidence is dirty-worktree macOS / Apple Git
2.50.1 / direct RustFS, not a released matrix qualification. The earlier
`phase2-transport-replay-20260903-0330` attempt stopped at fixture preflight
with a bucket-access 403 before invoking Crab; it is retained as failed.
The successful rerun explicitly pinned local fixture credentials and region;
it did not change the shared service or remove the preflight check.

**Still open.** Layout-descriptor capture/revalidation; whole-command strict
read-only qualification; continuous GC-safe protection through publication;
actual-byte recovery; durable terminal receipts and uncertain-commit read-back;
bounded history and remaining process/crash cases; candidate-bound host CI;
and full Git/OS/production-provider/managed proof. Azure emulator construction
assumes process environment is not concurrently mutated while the dependency
reads its emulator URL; this is not qualified against unsafe environment
mutation. The two-endpoint HTTP and command-replay proofs are separate
fixtures, not a full cloud cross-endpoint migration qualification. No original
Phase 2 acceptance checkbox is closed.

### Canonical layout binding and replay qualification: 2026-09-03 UTC

**Implemented.** `RepositorySnapshot` now owns the validated canonical layout
descriptor. The shared reader preserves active-transaction/manifest ordering,
reads the descriptor before materializing the journal, and validates it again
before returning. The existing post-pointer snapshot revalidation consequently
checks layout too, including zero-pointer inspections. Mirror destination
identity binds the descriptor digest independently of mutable metadata, so
converged replay cannot bypass layout admission. Missing, malformed,
unsupported and oversized descriptors fail closed. Equivalent JSON whitespace,
key ordering and layout-object ETags do not change semantic identity.

The existing strict canonical-v1 parser remains authoritative; no alternate
layout, migration reader or optional validation mode was introduced. Raw
manifest reads/writes retain their contracts, including absent-manifest
`NotFound`; they do not initialize layouts. Initialization belongs to repository
creation and explicit fixture setup. This adds two bounded descriptor reads per
snapshot capture. Request-cost/latency qualification remains open; the live
correctness result is not a performance SLO.

**Proof.** All 225 metadata tests with `file-index-reader`, 29 fsck tests,
59 mirror tests and 130 remote-helper unit tests pass. The shared checker fault
fixture covers two scope settings, changes during capture and reconstruction,
and missing/corrupt/unsupported/equivalently-formatted layouts. Its checker
store rejects and records writes; only the independent fixture writer changes
the descriptor. Rejected reads preserve the manifest and make zero checker
writes. Saved-plan command tests cover empty plans and nonempty converged
replay, descriptor rejection, changed scope/transport, unchanged plan bytes,
and equivalent formatting. These are not whole-command read-only-grant proof.

Sixty-seven selected push/GC/replay regression tests and all forty-two
remote-helper transcript integration tests pass. Five further
generation-owner/post-fetch tests pass after explicit layout initialization in
two older fixtures; their behavioral assertions are unchanged. An initial
generation-owner test failed because its fixture seeded only a manifest; the
rerun passes with a valid canonical repository. Earlier remote-helper fixture
failures were resolved the same way. Shared metadata Clippy with reader and
remote-index features passes with warnings denied. Product correctness and
suspicious Clippy gates pass, with no diagnostics in the changed snapshot,
lookup, checker or replay implementation. Existing product warnings remain.
Auth-server and minimal storage-feature consumer checks pass.

All 71 schema tests and schema drift pass. Nine web tests, typecheck and link
validation pass; web lint has zero errors and sixteen existing warnings.
Ten offline capability-verifier tests, formatting and diff checks pass. The
test linker retains its existing compact-unwind size warning. Production
growth is limited to one shared layout capture/recheck and the existing mirror
identity tuple; sibling push, GC, helper, generation-owner and shard-sync edits
in this slice are fixture initialization, not additional runtime readers.

The optimized-binary run `phase2-layout-replay-20260903-0412` passes
**448 commands / 130 checks**, including all **45** required mirror checks.
The report's executable digest matches the tested binary SHA-256:
`6ccd418b15adebf4f94239e6f299145f0d0dd356e5c31bcf6b7faaf11942855b`.
Its isolated live fixture captures the layout, CAS-writes equivalent formatting,
proves unchanged plan identity, CAS-writes an unsupported version, and proves
check/plan/apply rejection with manifest and saved plan unchanged. A `finally`
path restores the exact captured descriptor through CAS; Crab itself does not
repair it. No objects are deleted by this fixture. Existing publication,
pointer-origin, hook, cache, cancellation, stale-proof and approved-ref-deletion
checks also pass. Evidence is dirty-worktree macOS / Apple Git 2.50.1 / direct
RustFS with the retained v1.0.1 rollback binary, not a released support matrix.

**Still open / next boundary.** Choose and prove the non-mutating GC observation
contract, then qualify whole-command permission-safe inspection and protected
publication through the canonical coordination owner. Layout equality is not
an object-retention guarantee or a sweep-history detector. Existing GC fence
state is retained on normal release and advances its epoch, but acquisition
can recreate absent state with reset counters; two reads of an absent lock or
an ETag alone cannot qualify deletion/recreation. Do not infer a lifetime proof
from these layout tests. Declared-source recovery, terminal receipts and
uncertain-commit read-back, bounded history/process-crash cases,
candidate-bound host enforcement, and full Git/OS/provider/managed evidence
remain open. No original Phase 2 acceptance checkbox is closed.

### Bounded pointer inspection and explicit cache release: 2026-09-03 UTC

**Context and implementation.** Mirror check/apply and the pre-push guard now
use `crab_git::walk::scan_pointers`. One object database and the canonical
reachable-object walker own traversal; each ref closure is discarded after
pointer collection instead of retaining every overlapping closure. Actual
tag objects, including nested tags to commits, trees and blobs, determine
peeling. The product admits at most 2,000,000 distinct objects, 8,000,000
header/body lookups and a 64 MiB Gitoxide single-allocation ceiling. Shared
history counts repeatedly against lookups, not distinct objects. These are
admission ceilings, not measured support limits or performance SLOs.

Cancellation is checked between object reads; the async caller awaits the
blocking worker before releasing cache ownership. Missing/unreadable/wrong-kind
small blob candidates and checksum-invalid decoded objects fail closed, without
a partial pointer proof. Shared push, GC and service visibility walkers also
stop silently skipping missing/unreadable blob candidates. Only the new mirror
scanner gets the additional lookup/allocation/cancellation policy; this does
not claim that every publication/GC walker is now bounded equivalently.

**Scope limits.** Header-first classification skips ordinary large blob bodies;
it is not a complete Git object-integrity check. Qualification must still cover
a corrupt object with a plausible oversized header under a pointer OID, not
only invalid compressed bytes and checksum mismatches in small candidates.
No complete-source fsck guarantee is established here. Gitoxide's allocation
ceiling is not a total-RSS bound, and cooperative cancellation cannot interrupt
a stalled filesystem read. Ref-advertisement output and LFS subprocess/history
bounds remain open, as do shared-history traversal cost and maximum-scale
qualification. Preserve the original corruption, cancellation and resource
acceptance gates until those paths are proved.

An intermittent combined-test cache-release failure prompted an independent
deterministic descriptor-lifetime test. Both owner and cleaner tests failed
before the fix: `dup`/`fork` can retain the same Unix `flock` after the original
file closes. The private held-lock owner now explicitly unlocks for persistent
owners and temporary admission probes. Failed contenders never unlock another
owner. This uses fs4 0.13.1's existing unlock contract and the host `flock(2)`
contract, with no new dependency or lock protocol. Callers still join their own
workers before dropping ownership; it does not solve escaped-child or parent
crash safety. The original intermittent failure did not capture enough detail
to prove it was this exact interleaving; do not present the passing rerun as
root-cause proof.

**GC compatibility decision pending.** Released GC coordination shapes are
strictly parsed. Normal release retains state and advances counters, but
recreating absent state can reset them. A new incarnation cannot be silently
added to tagged persistence. The user was asked whether the next release may
require all writers to upgrade before an explicit repository migration. No
approval, schema change, migration, or runtime GC-observation claim is recorded
by this slice. Next acceptance: old binaries refuse before any write; interrupted
migration is recoverable; deletion/recreation and a complete intervening sweep
invalidate observations; direct and managed publication retain the same owner
through flush, commit and read-back. Do not derive approval from this note.

**Proof.** Shared Git tests: 206 pass, one existing microbenchmark ignored;
the fifteen walker tests include loose/packed history, nested and non-commit
tags, missing/corrupt/wrong-kind/checksum-invalid candidates, distinct-object
and repeated-lookup limits, allocation failure and cancellation without partial
results. A real-Git mirror command fixture corrupts only its cache object,
produces an unverifiable check and blocked saved plan, attempts no push,
preserves source/destination and releases cache ownership. All 77 selected
mirror, incremental-walk, visibility and cache-command tests pass. Thirteen
shared cache lifecycle tests pass, including both descriptor-lifetime
regressions; two service visibility tests pass and prove no index is published
from missing/corrupt blob history. These command fixtures use in-memory stores
where stated; they are not provider-grant or whole managed-service proof.

All 42 native remote-helper transcripts, 71 schema checks, schema drift and
two error-code catalog tests pass. A focused conversion test also passes for
retained, downcastable lookup errors and explicit scan outcomes. Git all-targets and cache production-library
Clippy pass with warnings denied. Cache all-targets strict Clippy fails on two
pre-existing `useless_vec` assertions in `local_cache.rs`, confirmed in HEAD;
those unrelated tests were not changed or suppressed. Product correctness and
suspicious Clippy gates pass, with existing warnings outside the new scan,
lock and walk-error mapping. Nine web tests, typecheck and links pass; web lint
reports zero errors and sixteen existing warnings. The existing test-linker
compact-unwind warning remains. Formatting and diff checks pass.

The optimized run `phase2-pointer-scan-20260903-0450` passes **448 commands /
130 checks**, including every one of the **45** required mirror checks. The
report digest matches the tested executable:
`8d3b2809f423b18dca3c1cdc73cda132231492b76478fa5d841e61c3db783d5a`.
Evidence remains dirty-worktree macOS / Apple Git 2.50.1 / direct RustFS, with
the retained v1.0.1 rollback binary. The live suite is regression proof for
publication, origin bytes, cache cleanup/cancellation, layout and saved plans;
new low-limit and corrupt-cache cases are covered by the focused tests above,
not misrepresented as additional live-provider matrix cases.

The new scanner adds one reusable admission/checked-ODB owner while reusing
the canonical traversal. Mirror's separate peel subprocess and detached-worker
wrapper are removed. Cache release adds one private lock owner shared by
owners, cleaners and probes; public lock APIs and marker formats are unchanged.
Growth buys resource/error ownership, not an alternate walker or compatibility
path. Generic Git-walk errors now retain their underlying source in the I/O
envelope (`CRAB-E0070`) instead of stringifying it into the internal-error
envelope; cancellation and budget exhaustion have explicit existing outcomes.
No original Phase 2 acceptance checkbox is closed by this slice.

### Streamed blob identity and bounded process output: 2026-09-03 UTC

**Context.** The preceding scan checkpoint intentionally left a corruption
gap: an object stored under a pointer OID could have a plausible large-blob
header and evade pointer classification. Header size is not an integrity proof.

**Implemented design.** The shared scan now returns pointer candidates and
explicit outstanding large-blob headers. The latter inventory is deduplicated
and object-bounded even during root preflight. Mirror inspection/apply and
pre-push feed those exact OIDs to one native `git cat-file --batch` process,
with replacements disabled and no filters/text conversion. The shared
`crab-git::batch` parser requires the captured OID, blob kind and size, hashes
raw bodies through a 64 KiB buffer with Git-compatible SHA-1 collision
detection, and rejects missing/reordered/truncated/extra responses or checksum
mismatches. Candidates are not trusted until all outstanding bodies pass.
The original `scan_pointers` API is unshipped; no compatibility reader or second
Git graph walker was added. Other reachable-set APIs retain their narrower
contract and do not silently gain full-body verification.

Process ownership remains in the existing mirror runner. Its stdin writer
streams OID lines without building another full text inventory. The stdout
worker verifies raw bytes without UTF-8 conversion or whole-file buffering;
stderr drains concurrently. Ordinary captured stdout/stderr each stop at
64 MiB and report an error on overflow, never a successful truncated result.
Parser errors, pipe errors and cancellation stop/join the owned child and pipe
workers before cache ownership returns. A blocked blob read is supervised by
the same cancellation owner as fetch and LFS commands.

**Contracts and limits.** The raw batch framing and native streaming blob path
are present in [Git 2.30.9's command contract](https://github.com/git/git/blob/v2.30.9/Documentation/git-cat-file.txt)
and [implementation](https://github.com/git/git/blob/v2.30.9/builtin/cat-file.c).
The pinned `gix-hash` 0.25.0 hasher supplies collision detection. This is not
complete `git fsck --strict`, a total-RSS guarantee, or a maximum-scale result:
native Git delta memory, pack/index faults and every declared Git/OS row still
require qualification. Ordinary large Git files now incur a full read during
inspection. `GIT_NO_LAZY_FETCH` is requested for newer Git children, but no
cross-version promisor/no-write guarantee is inferred from an environment flag;
whole-command read-only-grant proof remains open. Standalone LFS history
enumeration is outside this runner and is still the next resource-hardening
packet. GC lifetime, declared-source recovery, receipts/read-back and configured
host enforcement remain unchanged open requirements.

**Focused acceptance.** Shared tests cover binary/empty/large frames, exact
size/kind/OID binding, truncation at every boundary, extra/reordered output,
reader errors and cooperative cancellation. A real-Git command fixture replaces
only a cache entry with both invalid compressed bytes and a plausible oversized
blob: both block the saved plan without push or source/destination mutation.
Process tests stream a valid binary blob larger than 64 MiB, overflow each
capture pipe, and cancel a stalled raw-blob stream while retaining cache ownership.
The stalled-stream fixture initially failed because Rust's test harness emitted
non-protocol stdout before its body; a Git shell alias now produces only the
intended frame. Assertions and parser strictness were not weakened.

**macOS cleanup correction.** The first expanded live run
(`phase2-blob-stream-20260903-0511`) stopped after 372 commands and 100 checks.
The oversized-header corruption blocked publication, but cleanup replaced its
integrity diagnosis with `EPERM`. A real short-producer regression reproduced
the failure before the fix. XNU's group signal recipient lookup can exclude an
exiting process before `waitid` reports its exit. Cleanup now checks the entire
still-pinned group on `EPERM` without requiring that premature exit report;
it still waits/reaps the leader and joins pipe workers before returning.
Live group members and inspection permission failures remain errors. No sleep,
retry loop, blanket permission suppression or dependency patch was added.

The dependency evidence is XNU's
[group signal implementation](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_sig.c)
and [process lifecycle](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_proc.c):
`pinsertchild` publishes `allproc` and clears `P_REF_NEW` under the same
list lock; `proc_find` rejects dead references and waits/retries exec transitions.
[Process inspection](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/proc_info.c)
enumerates that list and reports lookup failure separately from permission
denial. The pinned `process-wrap` 10.0.0 group wrapper signals with `killpg`;
its final wait reaps the leader and only then waits for remaining child members.
This correction does not qualify escaped descendants or arbitrary parent death.

The next live attempt (`phase2-blob-stream-fixed-20260903-0526`) preserved the
integrity diagnosis but exposed an over-specific new smoke assertion: it
required a checksum mismatch even when exact header binding failed first.
A retained native-Git diagnostic showed the requested OID resolving to a valid
121-byte packed pointer while the corrupt loose copy declared a 65,536-byte
blob. Reader disagreement must fail before pointer proof, not be normalized
away. The smoke assertion now requires that exact OID and an explicit header
or checksum mismatch, in addition to a blocked plan, failed CI and unchanged
source/destination. Permission errors and generic failures cannot satisfy it.
Malformed raw bodies remain rejection cases in the parser tests.

**Retained result.** `phase2-blob-identity-20260903-0531` passed 456 commands
and 132 checks, including all 47 required mirror checks. The new fault check
refused the corrupt/mixed cache with exact-OID header mismatch, failed CI and
a blocked plan; restoring the captured cache bytes restored complete pointer
verification. Source bytes and destination refs remained unchanged. Report:
`Workspace/crabbuild-smoke/phase2-blob-identity-20260903-0531/artifacts/report.json`
under the qualification volume. Binary SHA-256:
`d7145b302b4772bd2e15e383302108e5f6c112262250e6567b1c9d3956311dd9`,
unchanged before/after the run. Profile: macOS, Apple Git 2.50.1, local RustFS;
rollback client remains tagged v1.0.1. The source checkout is dirty, so this is
local E2E evidence, not an exact-clean-candidate release claim.
The release verifier was also run against this report and rejected its dirty
source provenance as expected; no provenance field or release gate was relaxed.

Verification: shared Git tests 211 passed/1 existing ignored; mirror tests
66 passed, including loose and packed native binary blobs larger than 64 MiB.
The 32-process macOS cleanup regression also passed eight additional repetitions;
its pre-fix failure is retained above. Product correctness/suspicious Clippy
passed with existing broader warnings; shared Git strict Clippy, formatting,
diff checks, matrix rendering and ten offline matrix-verifier tests passed.
Earlier same-slice protocol/schema/error-code and web checks remain recorded
in their task outputs; no release/provider/platform scope is inferred from them.

The small shared raw-batch parser is additive because existing LFS discovery
also owns revision exclusions and path/lock policy; importing that product
scanner into `crab-git` would reverse ownership. The next LFS slice above must
reuse shared framing/process mechanics while preserving those semantics,
not introduce another Git graph walker or mirror-only collection path.
No original Phase 2 acceptance checkbox is closed by this slice.

### Supervised LFS discovery and shared batch verification: 2026-09-03 UTC

**Context.** The LFS range/tree scanner kept two independently spawned Git
children, drained their stderr only after scanning, and had no caller
cancellation token. A full diagnostic pipe could stall discovery; failure to
spawn the second child could leave the first running. Mirror also calls this
scanner, and `lfs::publication::publish_reachable` uses it inside canonical
Git push. Hardening only the mirror adapter would miss both siblings.

**Implemented boundary.** `crab/src/git/process.rs` now owns process-group/job
creation, cancellation, bounded stderr, concurrent stdin/stdout workers and
joined cleanup. The mirror adapter retains only command/output policy. LFS
discovery is parsed into a bounded inventory and its child is completely
joined before the batch child starts. A second-stage spawn error cannot leave
a first-stage producer. Both stages share the same owner, not parallel
cleanup implementations. The existing macOS exit-race regression moved with
that owner; the workflow selects it and the mirror adapter's process tests.

Each discovery pass accounts input bytes plus retained object/path records
against 64 MiB, with a 1 MiB record ceiling. Pointer results have a separate
64 MiB accounting budget, including aggregate porcelain results across refs;
diagnostic captures remain capped at 64 MiB. Oversized, truncated, cancelled or
failed scans cannot return an empty success or invoke the caller with a partial
pointer prefix. These are logical resource ceilings, not a 64 MiB RSS promise;
allocator overhead, native Git, ref enumeration and I/O deadlines still need
scale qualification. Tests moved out of the large push module without changing
their filters or expected behavior.

`crab-git::batch` now provides the one raw framing/checksum reader for both
captured Crab blob headers and LFS small-blob discovery. It reuses one 64 KiB
buffer per batch, verifies large bodies without retaining them, and preserves
request ordinals when only small blobs become pointer candidates. The LFS
size filter was removed: it could suppress a corrupt pointer with a forged
large header. This adds full reads of ordinary objects in the introduced range;
remote/base-manifest exclusions remain intact. Existing zero-size handling
and path/lock associations remain unchanged. Git's object-name selection is
not a proof that every changed locked path or alias has been enumerated.

SHA-1 retains Git-compatible collision detection. The reader accepts SHA-256
Git OIDs through the already-resolved `sha2` 0.10.9 implementation, adding only
that direct dependency edge to `crab-git`; no dependency override, package
version change, or global Gitoxide SHA-256 feature was introduced. Native Crab
transport admission remains SHA-1. Git 2.30.9's raw batch and revision-range
contracts remain the dependency reference; newer Git is not required for this
framing. `GIT_NO_LAZY_FETCH` is still only a requested child behavior, not
cross-version read-only-grant proof.
The larger collision-detecting SHA-1 hasher deliberately stays on the stack:
only one object hasher is live, so boxing the enum would add a heap allocation
per object without reducing batch-wide memory growth. The local Clippy
expectation documents that decision; shared Git strict Clippy passes.

CLI LFS push/pre-push, composed mirror hooks, incremental mirror scans and
canonical publication now pass the caller's cancellation token into discovery.
Publication awaits the blocking scan before proceeding. Transfer scheduling,
parent death/second signals, escaped descendants and full read-only/grant
qualification retain their separate open gates; this is not a claim that all
LFS transfer phases now honor the CLI token.

**Acceptance and evidence.** The existing LFS revision/base-ref, object-ID,
hook-input and backpressure contracts pass. New tests reject truncated/budget-
exhausted discovery, drain a full stderr pipe while preserving a nonzero exit,
cancel stalled discovery before cache release, and verify/refuse real SHA-1
and SHA-256 Git blob fixtures. Shared tests check request ordinals, large-body
checksum rejection and SHA-256 identity. Mirror and canonical LFS publication
siblings have been rerun.

The first live run, `phase2-lfs-supervision-20260903-0604`, stopped after 301
commands/83 checks because the new harness assertion looked up stderr in its
stdout-only in-memory cache. The actual CLI had already rejected the corrupt
pointer with the exact expected checksum error. Its report and diagnostic log
remain retained. The assertion now reads its existing stderr artifact; no
integrity, unchanged-ref or restoration condition was relaxed.

The corrected run, `phase2-lfs-supervision-20260903-0606`, passed 465 commands
and 134 checks, including all 49 required mirror checks. The two added checks
exercise actual `crab lfs push --dry-run`: a valid large blob substituted at
the pointer's Git OID fails checksum verification; restoring the original
loose object restores successful discovery. Source HEAD, worktree bytes and
destination refs remain unchanged. The subsequent composed-hook publication
and pointer-byte verification also pass. Report:
`Workspace/crabbuild-smoke/phase2-lfs-supervision-20260903-0606/artifacts/report.json`
under the qualification volume. Optimized binary SHA-256, unchanged before and
after the run:
`e77053fa243ccb4e756e42faf32a03e04d829919f77b6ed6b9174703a858b0d0`.
Profile: macOS, Apple Git 2.50.1, local RustFS, tagged v1.0.1 rollback client.
The dirty source provenance remains explicit: this is local E2E evidence,
not clean-candidate release or production-provider qualification.
The release evidence verifier rejects this report's dirty source as expected;
no provenance field or release gate was relaxed.

Verification: shared Git 213 passed/1 existing ignored; mirror 64 passed;
shared process owner 2 passed; LFS discovery 21 passed; canonical LFS
publication 4 passed; remote-helper transcripts 42 passed; pre-push CLI input
4 passed. Shared Git strict Clippy and product correctness/suspicious Clippy
passed; the latter retains existing broader warnings. Optimized build,
formatting, diff checks, workflow YAML parsing, matrix rendering and ten
offline matrix-verifier tests passed. Web: nine tests, typecheck, lint (zero
errors/16 existing warnings), and links (398 pages/4,297 fragments) passed.

Remaining gates: whole-transfer caller cancellation, complete locked-path
association, maximum-scale/RSS/latency qualification, abrupt process death,
read-only/promisor/provider behavior, and protected publication/recovery/receipt
lifetime. The original Git/OS/provider/direct/managed and exact-host-CI matrix
still applies; bounded discovery is not a substitute for those gates.
No original Phase 2 acceptance checkbox is closed by this slice.

### LFS admission liveness and large-repository qualification: 2026-09-03 UTC

The coordinator awaited initial byte admissions before polling its
`FuturesUnordered` transfers. Two requests whose combined permits exceed the
byte budget could deadlock: the admitted request never ran to release its
permits. A new two-second regression failed on that implementation before the
fix. One retained admission future is now polled alongside active transfers;
queue capacity remains bounded and waiting admission does not restart its
semaphore position or reserve a rate-limit slot twice. The duplicate initial
and refill loops, boxed transfer futures and operation `Arc` are removed.

Object- and byte-permit waits now observe the coordinator's cancellation
token. Cancellation stops admission, releases any partially acquired permits,
and drains active attempts; it never drops a multipart-owning future merely
to return faster. An attempt completing after cancellation cannot report
success or begin a retry, and download `skip_download_errors` cannot turn
cancellation into success. Caller-token propagation and bounded cancellation
inside every storage phase remain open; this change does not claim otherwise.

Dependency proof: futures-util 0.3.32 requires polling `FuturesUnordered` to
start queued futures; Tokio 1.52.1's semaphore contract says cancelling an
acquisition loses its queue position; tokio-util 0.7.18's
`run_until_cancelled` drops its future and is safe only when that future is
cancel-safe. `crates/crab-lfs/src/object_store.rs` explicitly aborts multipart
uploads on the error-return path, so dropping the attempt is not equivalent.

Verification so far: the original failing liveness regression now passes;
all 372 selected LFS/product tests pass, covering batch, canonical publication
and custom-agent siblings. New regressions cover byte saturation, cancellation
while waiting for either permit, drain-before-return, skipped-download policy
and cancelled empty queues. The optimized build and product
correctness/suspicious Clippy passed before rebasing the draft PR; rerun proof
on the rebased candidate. Non-test code growth pays for concurrent admission
and explicit terminal handling; tests account for most added lines.

The user additionally requires actual large-repository qualification against
RustFS with `crab`/`crab` credentials and bucket `crabbuild`. The existing
RustFS container and bucket respond successfully. Kubernetes is available as
a read-only input at `Workspace/Github/kubernetes/kubernetes`, revision
`4675851bd198493d2fcd371cf493594ab1933f23`, with about 3.44 million packed
objects/2.51 GiB of packs. Use fresh run-owned repositories beneath
`Workspace/Github` and retain source status/revision checks. Execute actual
Crab add/commit/push/clone/hydrate, native full/partial/incremental Git reads,
strict integrity and byte comparisons; retain timing/resource evidence and
do not infer a no-regression claim from unit tests. This work is not yet
complete. The LFS qualification runner now exercises Crab batch push/fetch
as well as the Git LFS adapters; a two-object workload above the shared byte
budget will qualify the scheduler through real storage.

### Bounded large-history discovery checkpoint: 2026-09-03 UTC

**Observed failure.** Draft PR [#148](https://github.com/crabbuild/crab/pull/148)
is open at `30ce1fb`. The optimized build and post-rebase provider (28), auth
store (16), LFS (372), mirror (64), fsck-store (32), and bounded pack
publication (3) tests passed. The fresh RustFS `crabbuild` Kubernetes run
`phase2-kubernetes-30ce1fb-20260903` then failed its initial import with
`Git LFS scan exceeds inventory limit`. The seed has 138,136 commits; the
HEAD reachable-object listing alone is 113,109,576 bytes / 1,647,929 rows.
The old scan charged both cumulative output and retained records against
64 MiB. It rejected valid large histories before publication. Failure logs,
the run-owned source clone and the report remain under
`Workspace/Github/crab-qualification`; the original Kubernetes checkout was
used only as read-only input.

**Design correction.** `crab/src/cmd/lfs/push.rs` processes discovery in
bounded record batches. One supervised `cat-file --batch` runs per batch;
the discovery process remains owned while its pipe applies backpressure.
No whole-history spool, unbounded vector, size-filter shortcut or alternate
scanner is added. Every object response is still streamed and checksum
verified through `crates/crab-git/src/batch.rs`. The pointer-result budget is
shared across batches, not reset for each subprocess. The 1 MiB record limit
and bounded diagnostics remain. The record budget bounds logical retained
data, not total process RSS; allocator capacity and Git's own memory still
require measurement.

The outer discovery worker owns and joins each batch worker. It returns
candidate pointers only after discovery itself has also exited successfully.
A late process, framing, checksum, cancellation or result-budget failure
invalidates all candidates, including earlier successful batches. This
preserves the mirror/cache-owner lifetime boundary and all-or-error callers.
Canonical publication, mirror LFS discovery and porcelain/pre-push share
this scanner. Fetch uses its existing separate scanner and remains covered
by the LFS sibling tests; this change does not claim to close every fetch
resource or cancellation gap.

**Phased acceptance.**

1. Unit/integration: oversized and truncated records still fail; history can
   exceed the retained-batch budget; batch failure stops further admission;
   the result budget spans batches; a nonzero discovery exit cannot emit
   valid pointers collected before that exit. All 375 selected LFS tests and
   64 mirror tests pass after the correction.
2. Exact-binary live replay: rebuild the optimized candidate, retain its
   hash, and rerun Kubernetes against a fresh `crabbuild` prefix. Initial
   import must pass the formerly failing discovery gate, then complete real
   publication, native full/partial/incremental reads and byte verification.
   Add the requested Crab add / Git commit / Crab push / clone / hydrate
   sequence on run-owned data. This phase is still open.
3. CI/release: rerun supported-platform and exact-candidate evidence gates.
   The first PR run found an invalid job-level `runner.temp` expression;
   its path now follows adjacent jobs' `github.workspace` convention. Cache
   dependency-budget admission and the release check's blanket rollback-flag
   prohibition remain pending explicit policy approval. Do not edit those
   expectations merely to turn CI green.

**Fresh storage proof, separate from Kubernetes.** The existing LFS harness
passed 27 commands and three checks against `crabbuild`, fixture credentials
`crab` / `crab`, with two 65 MiB objects. It exercised Crab LFS push/fetch,
Git LFS adapters, Git push/clone, ref equality, both fsck implementations,
and SHA-256 byte identity. Peak child RSS was 179,634,176 bytes. Report:
`Workspace/Github/crab-qualification/phase2-lfs-30ce1fb-20260903/report.json`.
Binary SHA-256:
`e37e5101f541dc489d5856b4dac93818ee7855050c8a9598a74b9b6a8d61c0f6`.
This precedes the discovery correction and is a development checkpoint,
not clean-release, full-Kubernetes or production-provider qualification.

### Caller-owned LFS cancellation checkpoint: 2026-09-03 UTC

**Context.** The coordinator could cancel its own queue, but its constructor
created an independent token. Cancellation of native publication or an LFS
command therefore did not reach transfers already admitted by that command.
Porcelain fetch/pull and the LFS clone wrapper also dropped the token at their
dispatch boundary. This remained a real gap after admission liveness passed.

**Implementation.** `TransferCoordinator::new` now requires a parent token
and creates a child cancellation domain. Caller cancellation stops admission
and propagates to attempts; a failed batch can cancel its own siblings without
cancelling the caller or unrelated sibling operations. `BatchResolver`
requires the command token, checks it for empty/nonempty batches and local
inventory loops, and supplies it to each coordinator. Native publication,
LFS push/pre-push, fetch/pull, clone-to-fetch/pull, and fetch-triggered prune
now preserve caller ownership. Publication checks cancellation between local
metadata inspection, upload and post-upload verification. Download cancellation
after transfer but before cache installation drops the owned temporary path
instead of installing a result that the caller has rejected.

This uses tokio-util 0.7.18's documented `child_token` contract: parent
cancellation reaches children; child cancellation does not reach its parent.
No task is detached and no multipart-owning future is dropped. The shared
object-store operation still drains before the coordinator returns.

**Acceptance evidence and next boundaries.**

- 379 selected LFS tests passed. Parent cancellation is tested while waiting
  for object/byte permits and while an active attempt is held open. A first
  error cancels batch siblings but leaves parent/unrelated child tokens live.
- Empty/nonempty batch operations reject an already-cancelled caller. New
  fetch/pull/clone tests prove rejection before repository resolution.
- A real in-memory object-store download, delayed by the dependency's
  `ThrottledStore`, is cancelled after creating its temporary path. It drains,
  returns `Cancelled`, installs no cache object, and removes that temporary
  file. The test does not assert that an in-flight store request is aborted.
- Batch tests moved beside the module to keep the production owner readable.
  Cross-platform protocol CI now includes the affected fetch/clone tests.
- Still open: bounded cancellation inside storage requests, multipart abort
  acknowledgement, synchronous local hashing and remaining fetch/clone Git
  subprocesses. The custom-agent's blocking protocol reader still owns a
  separate terminate/EOF lifecycle; it has no caller-token contract. It must
  be made safely cancellable before wiring process cancellation into that
  session. This checkpoint does not close those obligations or claim complete
  end-to-end cancellation qualification.

**Kubernetes progress, separate candidate.** The pinned `d38ab28` binary
passed the formerly failing initial import in 130,051 ms and produced a
1,227,441,712-byte pack containing 1,608,405 objects. More than 300 sequential
incremental pushes have since passed in the still-running 1,000-commit run.
Binary SHA-256:
`0d29c3151d83beb50a9254168a5962a498cd7a108bd5f62d2cb824cf34a79b87`.
The source binary is snapshotted in the run directory, so later builds do not
change the experiment. This is functional development evidence on a host
also running compilation/tests, not an isolated performance comparison.
The run's remaining full/partial/shallow clone and integrity checks, the
requested managed-file daily loop, and final-candidate replay remain open.

### Real-history managed-file qualification: 2026-09-03 UTC

**Context.** Native Git replay alone does not exercise `crab add`, a real Git
commit, managed-object publication, lazy `crab clone`, hydrate, dehydrate,
and rehydrate. The existing small-file harness covered that loop but started
from empty Git history and shared its writer cache with the reader.

**Implementation.** Extend `run_add_commit_push_rustfs_smoke.py` in place with
`--source`. It runs both Crab-porcelain and native-Git staging/push paths on
disposable clones of the captured source HEAD. The source is read-only and
checked again on success/failure. Each reader gets an initially empty cache,
checks its clone tip against the published ref, and verifies payload SHA-256
identity after hydrate and rehydrate. Unsafe source/output overlap, existing
runs, payload-path collisions and configuration symlinks are rejected before
product setup. Early rejection must not write a failure report into the
source. The default synthetic Git matrix remains a separate profile.

**Evidence.** Nine focused harness tests pass, including real Git cloning at
a pinned revision without importing uncommitted contents, failure-path source
checks, and cold-cache environment isolation. Provider evidence tests (six)
and workflow syntax pass. The optimized `aac1534` binary also passed the fresh
two-object 65 MiB LFS qualification; its SHA-256 is
`07902caa8388d2641f34907809555f3b2e78b7a1df98362963846a7e62891386`.

**Observed dependency gap; not a passed large-history gate.** The original
Kubernetes HEAD `4675851bd198493d2fcd371cf493594ab1933f23` contains locally added
historical Docker.dmg pointers. The `d38ab28` replay passed import and 917
incremental pushes, then rejected push 918 on the first unavailable payload.
A subsequent read observed remote main still at successful push 917,
`4327fee8d315798d46a63e6f8e7f42d0b9d2a98f`. No remaining clones/fsck/final
sampling ran. The command misleadingly classified unavailable staging as
`CRAB-E0081` (locked, with no holder); the correction is recorded below.

The `aac1534` managed-file run on the same input also rejected publication:
two historical payloads lacked local staging and destination proofs. It
published zero refs; direct `ls-remote` returned none. Source HEAD/status were
unchanged. Both failed reports are retained under
`Workspace/Github/crab-qualification`, with run IDs
`phase2-kubernetes-d38ab28-20260903` and
`phase2-kubernetes-managed-aac1534-20260903`.

**Independent upstream-history result.** Both managed-file workflows passed
on local upstream revision `160bd16d98b7f688ce4f3b5ab0c5e4c045f36233`: 63
commands, 48 checks, two 65 MiB payload paths per case, matching published
refs/clone tips, lazy pointers, cold-cache hydrate, Git connectivity,
dehydrate and rehydrate. Source HEAD/status and the selected executable digest
were unchanged. Report run ID:
`phase2-kubernetes-origin-managed-aac1534-20260903`. This is separate evidence
from the failed local history above, not a full native replay or performance
comparison. Documentation link validation also passed (398 pages, 4,297
fragments).

The original checkout's configured source, `crab://crab/k8s`, is reachable
read-only and advertises master at `64e363f03f9ac9a338a79a15c550cbd9faa5f521`.
Both historical payloads were subsequently reconstructed in disposable
checkouts using that source (recovery checkpoint below). No source-remote
writes or cleanup were performed.

**Next acceptance gates.**

1. Stage the recovered historical payloads through the canonical add path,
   publish dependencies, then rerun the original input.
   Do not remove pointers or treat another history as a repaired original.
2. Preserve the independent upstream-history result and repeat it after any
   product changes affecting staging, push, clone or reconstruction. Require
   both workflows, cold-cache hydration, byte identity, source preservation,
   and an unchanged selected executable before passing.
3. Repeat final-candidate full native-Git replay and the required provider/OS
   matrix. Use idle, isolated hosts for performance comparisons; these
   development runs do not establish a no-regression performance verdict.

### Staging admission and historical payload recovery: 2026-09-03 UTC

**Context and ownership.** CLI and remote-helper staging openers discarded
every failure into `None`; lower push then guessed a lock holder, even for a
missing directory. `git::push_staging::PushStaging` now owns one classification
path: missing directory, acquired reader, or observed lock contention.
Missing-index, corrupt-index and I/O errors retain their original category
and source. No storage schema or dependency changes are needed.

**Publication rule.** Native discovery may proceed during actual contention
only for pointer-free pushes, preserving the explicit tagged `v1.0.1`
behavior. A discovered pointer returns the observed contention error and
releases acquired remote leases. Truly absent preparation instead returns
`PointerMissingStaging` before upload/ref publication. Import's lower-pipeline
caller already supplies its ingest reader; that sibling path is unchanged.
Protected-push preparation uses the available reader for estimates, while
native admission remains the pointer-publication guard. Managed-provider
live proof remains open.

The remote helper opens staging per push batch, never for fetch/list or the
whole session. A damaged staging index therefore cannot block a read-only
session. Corrupt staging is an explicit push error, not a fallback to empty
staging. This checkpoint does not enable remote-only pointer reconciliation
without local preparation.

**Windows compile correction.** CI on `66b896a` reported `E0133` in
`git::process::OwnedChild::has_exited`. The installed `process-wrap 10.0.0`
contract exposes the immediate lower wrapper through safe `inner_mut`.
Crab constructs exactly one Windows JobObject wrapper; polling its lower
native child preserves cached leader status without consuming job-completion
events. Final kill/wait still targets the whole job. No unsafe escape,
dependency override or policy-budget change is added. Native Windows CI must
confirm this correction; macOS tests do not prove Windows execution.

**Recovery evidence, not a replay pass.** Optimized `aac1534` hydrated both
versions from the declared source in separate task-owned checkouts. The
original Kubernetes checkout remained unchanged. Retained evidence:
`phase2-k8s-payload-recovery-aac1534-20260903/recovery-evidence.json` beneath
the qualification workspace.

| Historical checkout | Recovered bytes | Payload SHA-256 |
| --- | ---: | --- |
| `4caac343bf4aa0604d921167aae1550b0284bc8d` | 581,598,168 | `94102d4fe056bf3a4fde375d693aae96a429157dad0345af9853d7157d6bd5bd` |
| `64e363f03f9ac9a338a79a15c550cbd9faa5f521` | 581,598,166 | `3c1ff06b3b0cef48c72404c45631fe10733f1840e5069c7e5288a6f0a96f65e2` |

**Scoped proof.** 209 selected unit tests and 59 integration tests passed,
covering native/CLI/helper push, missing preparation, exact contention,
remote-lease release, incremental walks, linked worktrees, add/commit/push,
hydration and remote-helper transcripts. Correctness/suspicious Clippy gates
passed; other existing warning categories are not claimed clean. New staging
contracts run in Linux and macOS/Windows protocol CI. Fifteen additional
read-session and subprocess tests passed on macOS, including a damaged local
index, descendant cleanup, cancellation escalation and bounded pipe output.
Optimized candidate build, live diagnostics and the recovered-history replay
are tracked separately as they complete.

**Next executable gates.**

1. Build the committed candidate; pin its digest for a fresh run. Re-stage
   both verified payload versions under distinct task-owned paths without
   changing source Git history. Require exact staged identities, then all
   1,000 native pushes, final clone/filter/shallow/fsck/sample checks.
2. Repeat both managed-file workflows with a cold reader cache; require
   matching published refs and byte-identical hydrate/rehydrate results.
3. Retain earlier failed evidence. A functional run with recovery preparation
   or a busy development host is not a controlled performance comparison.
4. Require Windows native lifecycle tests and the full provider/OS matrix;
   keep Phase 2 open until all original acceptance criteria are satisfied.

### Shared LFS fetch discovery: 2026-09-03 UTC

**Context.** Push, mirror and native publication already verified raw Git
objects under the shared subprocess supervisor. Fetch/pull still materialized
complete `ls-tree` output and used a separate `cat-file` parser: it did not
verify requested OIDs or checksums, skipped missing objects, and could neither
cancel blocked pipes nor reject all incomplete scans reliably. Returning an
empty or partial pointer inventory could falsely report an up-to-date fetch.

**Implementation.** Move the existing verified scanner and its behavioral
tests into `lfs::discovery`, consumed directly by push/pre-push, mirror,
native publication and fetch/pull. Delete fetch's alternate tree/batch
parser. Tree discovery now uses bounded NUL framing, verifies raw SHA-1 or
SHA-256 identity even when the body is too large to be an LFS pointer, and
keeps candidates private until producer and batch children complete. Caller
cancellation reaches these children and joins their pipe workers.

Fetch keeps every unique path/OID association until include/exclude and
checkout policy run, with a global accounted pointer/path inventory budget.
Repeated refs do not duplicate the same association; conflicting declared
sizes for one LFS OID fail closed. Push retains its object-ID deduplication
and bounded aggregate object inventory, rather than retaining every alias
across selected refs. `ls-tree --full-tree` keeps repository-relative paths
when invoked from a subdirectory. Selection policy remains in the commands;
no new public configuration or dependency is introduced.

**Behavior boundary.** Default fetch from an unborn repository remains an
empty inventory, proven by gix 0.83.0's typed `Head::is_unborn` contract.
Explicit invalid refs, absent/corrupt requested Git objects, truncated records
and failed child commands are errors, never successful empty/partial
inventories. This intentionally removes the tagged `v1.0.1` diagnostic-string
shortcut that treated an invalid object name as no data.

**Evidence.** 448 selected LFS and mirror tests passed after consolidation,
including existing exact-range/base-manifest exclusions and publication
callers. Added real-Git fixtures cover duplicate paths, NUL-framed newline
names, subdirectory selection, filter/checkout ordering, distinct ref-tip
versions, conflicting pointer sizes, invalid-ref rejection and pre-cancelled
discovery. Both range and tree paths reject checksum-mismatched pointer blobs
disguised as large non-pointer objects. Moved discovery tests remain enabled
in Linux and macOS/Windows protocol CI. Thirty-eight final focused rechecks,
correctness/suspicious Clippy, formatting, workflow syntax and documentation
link validation passed. Other pre-existing warning categories are not claimed
clean. Optimized build and fresh live LFS qualification are recorded when
complete.

**Separate large-repository evidence.** Optimized `d89a152`, before this
refactor, passed the fresh Kubernetes upstream-history managed-file run:
63 commands, 48 checks, both Crab and native-Git add/commit/push workflows,
cold-cache clone/hydrate, dehydrate/rehydrate, exact bytes and unchanged
source/binary. Report: `phase2-kubernetes-origin-managed-d89a152-20260903`.
Its live staging diagnostics passed 19 commands and 23 checks. The original
history replay remains a separate active run with pinned `d89a152`; neither
result proves a later candidate or a controlled performance comparison.

**Remaining acceptance work.** Recent-ref/config selection still uses its
existing subprocess path; command stdin, local hashing, storage-request
cancellation and the custom agent's protocol reader still need complete
lifecycle work. `--all` selection semantics are not expanded by this packet.
The range scanner still needs complete path association for lock checking;
`rev-list --objects` supplies an object name, not every path using that object.
Do not close those gates from tree-scan tests. Require final-candidate live
LFS and full native replay plus the original provider/OS/host-CI matrix,
publication-lifetime and uncertain-result criteria.

### Partial-clone discovery access boundary: 2026-09-03 UTC

**Context and observed failure.** Before publishing the shared-scanner
refactor, a real partial-clone regression test failed: fetch/pull inherited
the inspection scanner's `GIT_NO_LAZY_FETCH=1`, preventing Git from resolving
an omitted LFS pointer blob. This is a compatibility regression, not missing
LFS payload data. Mirror/native publication must retain local-only inspection.

**Design.** One internal `GitObjectAccess` policy reaches both discovery and
raw-batch children. Fetch/pull tree selection permits promisor reads while
inheriting caller restrictions; push/pre-push/range inspection prohibits them.
No second parser, unverified fallback, public option, or dependency is added.
All retrieved raw objects still pass framing, identity and checksum checks.

Git's [partial-clone contract](https://git-scm.com/docs/partial-clone)
allows demand fetching. However, [Git 2.30.9's promisor implementation](https://github.com/git/git/blob/v2.30.9/promisor-remote.c)
does not honor `GIT_NO_LAZY_FETCH`. Local-only discovery therefore also sets
an empty transport allowlist, supported by [that version's transport policy](https://github.com/git/git/blob/v2.30.9/transport.c).
The sibling mirror raw-blob verifier uses the same restriction. Fetch mode
does not clear or override either restriction supplied by its caller.

**Acceptance and proof.** The formerly failing fixture now proves: a real
`blob:none` clone lacks its pointer blob; local-only range inspection fails
without filling the missing-object set; fetch discovery obtains and verifies
the pointer. Command-policy tests protect inherited restrictions. The native
publication fixture still proves already-published missing pointer blobs are
not hydrated. All 450 selected LFS/mirror tests pass locally after this fix.
Version-specific transport behavior and Windows execution still require the
exact-candidate CI matrix; dependency-source proof is not an E2E substitute.
Retain the broader remaining acceptance work above.

### Git-safe mirror cache paths: 2026-09-03 UTC

**Context.** Windows CI at `d89a152` compiles after the subprocess API fix,
but eight mirror tests fail. The first failure is `git init` rejecting the
cache owner's canonical verbatim path as an operand. Reconciliation then
correctly refuses unavailable source/target proof; those refusals must not be
relaxed. See [Windows job 100566636538](https://github.com/crabbuild/crab/actions/runs/33729717072/job/100566636538).

**Design.** Keep `CacheUseGuard`'s physical path and lock identity unchanged.
Create the owned cache directory, then initialize `.` from inside it instead
of serializing the physical path into Git arguments. This uses the same
initialization path for missing, empty and retained-marker directories.
At the adjacent source transport boundary, encode absolute local paths as
file URLs; retain the original filesystem path for hook checks, cache keys
and reconciliation plan identity. Remote URLs remain unchanged. The existing
`url` dependency supports disk/verbatim-disk and UNC/verbatim-UNC prefixes;
no path-prefix stripping, dependency, platform fallback or alternate cache
owner is introduced. Git documents both local forms as supported
[fetch transports](https://git-scm.com/docs/git-fetch#_git_urls).

**Acceptance and proof.** Existing tests retain lock identity across cleanup
and rebuild, reject unrelated nonempty directories, resume interrupted init,
mirror empty/detached/changed/shallow sources, and enforce plan target binding.
A new real-Git fixture uses canonical source/cache paths with spaces, `#`
and `%`; it verifies configured transport, exact mirrored refs, hook inspection
and strict Git fsck. The command test verifies in-directory init and unchanged
remote URL. All 109 final focused mirror/LFS tests and correctness/suspicious
Clippy pass on macOS; other warning categories are not claimed clean.
Require the next exact-commit Windows/macOS CI run before declaring the
cross-platform failure resolved. No broad Phase 2 acceptance gate is closed.

## Phase 3: Close metadata, maintenance, and GC scale gates

### Context

The canonical partitioned layout, paged recipes, segmented indexes,
generation-bound object catalog, bitmap visibility, split commit graph,
geometric packs, response-pack cache, work budgets, and one generation owner
already exist. `plans/001-large-repository-scale-roadmap.md` is the detailed
owner design. This phase executes its remaining gates and removes proven
bottlenecks inside those owners.

### Design

#### 1. Extend the existing qualification profile

Use the current Kubernetes fixture and report schema. Add repeated isolated
profiles for 1,000 and 10,000 one-commit pushes, long ancestry, branch fanout,
hot-ref contention, retained history, generated packs, workflows, and GC.
Separate correctness, server processing, network transfer, client Git index,
and harness/host contention.

Run team scenarios on distributed clients, not 100 Git processes on one disk:
at least 10 client nodes for clone/fetch fanout and at least 5 writer nodes for
independent/hot-ref pushes. Preserve the existing single-host run only as a
stress profile.

#### 2. Finish bounded GC structures

- Replace remaining snapshot, ref-journal, workflow-registry, preview, and
  repair materialized joins with durable bounded partitions.
- Persist provider enumeration cursor/checkpoint state where the provider
  contract supports safe resume.
- Segment large closure sidecars and bind every segment to one closure root.
- Make inventory parsers strict: malformed rows and invalid sizes reject the
  report. Inventory remains a candidate source, never sole deletion proof.
- Keep bucket GC disabled where complete roots, closure coverage, coordinator
  drain, or provider identity cannot be proven.

#### 3. Make owner work predictably restartable

Retain one generation owner and one-action-per-cycle policy. Every action has a
durable snapshot, selected inputs, byte/request/time budget, heartbeat, and
terminal outcome. On crash, the next owner adopts or abandons immutable work,
then converges without scanning unrelated stable state. Stable large packs and
completed catalog/graph layers are not rewritten by routine maintenance.

#### 4. Prove growth invariants

Measure active pack count, catalog/graph/closure layer counts, manifest and
journal bytes, remote temporary bytes, local spill, RSS, open files, provider
requests, owner backlog, and retained/collectible storage. Doubling history
must not double per-change synchronous work when the architecture promises an
incremental path.

### PR slices

1. 10,000-push and distributed harness/report extension.
2. Remaining durable GC joins and segmented closures.
3. Provider cursor/inventory strictness and resume evidence.
4. Owner crash adoption, failover, and backlog diagnostics.
5. Retention/GC convergence and real-provider matrix.

### Milestone handoffs

**3A — Reproducible scale envelope.** Context: a small successful repository
does not expose growth in historical packs, metadata joins, or maintenance
backlog. Entry: frozen workload seeds, topology, resource ceilings, and cost
cap. Implement slice 1 on the existing qualification runner. Exit: retained
1,000/10,000-push reports distinguish foreground publication, maintenance,
client indexing, and host contention; the verifier rejects missing metrics
and incorrect results. A benchmark that exposes a failing target is useful
baseline evidence, not acceptance of scale support.

**3B — Bounded and restartable owners.** Context: remaining materialized joins
and unsegmented closures can limit repository size even when object transfer
is streaming. Entry: 3A identifies the failing owner; Phase 1 protects all
retained roots. Implement slices 2–4 inside the existing GC and generation
owners. Exit: 10x cardinality stays within predeclared RSS/open-file/spill
ceilings, malformed inventory fails closed, and interruption at each durable
boundary resumes or safely abandons work without a second publication owner.
If safe provider cursor resume is unavailable, retain that limitation in the
profile rather than interpreting an incomplete enumeration as complete.

**3C — Sustained team and provider qualification.** Context: one host's fanout
does not represent a distributed team or prove provider recovery behavior.
Entry: 3B and isolated distributed infrastructure. Complete slice 5 and the
full repeated workload. Exit: the two 10,000-push runs, distributed reader and
writer counts, owner budgets, failover, and retention criteria below pass on
each claimed provider. Capture post-GC fresh-clone and byte proof. A backlog
or capacity breach pauses rollout; it never relaxes GC safety or retention.

### Acceptance criteria

- [ ] Two isolated 10,000-push runs preserve every ref, pass authoritative
      fsck, and produce identical deterministic correctness fingerprints.
- [ ] Active pack and metadata-layer counts stay within the documented
      logarithmic/geometric invariant; stable large packs are not rewritten by
      routine owner cycles.
- [ ] One-commit push publication work stays proportional to new/rebound
      objects. Median publication time from push 1 to 10,000 drifts no more
      than 20% after separately accounting for bounded maintenance cycles.
- [ ] Each automatic owner action respects its existing 128-pack, 2 GiB,
      384-request, and ten-minute selection budget; overruns fail or defer with
      typed evidence rather than extending the lock indefinitely.
- [ ] Killing the owner and GC runner at every durable boundary leaves old or
      complete-new canonical state; restart converges idempotently.
- [ ] A 10x cardinality increase does not create an unbounded RSS, open-file,
      queue, or temporary-space slope. Thresholds are recorded before the run
      and enforced by the verifier.
- [ ] Fifty cold clones, 100 warm clones/fetches, 20 independent-ref pushes,
      and 20 hot-ref pushes run from distributed clients with exact results and
      documented backpressure.
- [ ] S3, GCS, and Azure rows separately prove pagination, throttling, retries,
      cancellation, owner failover, retention, and post-GC integrity before
      their scale support is advertised.

### STOP conditions

- A second metadata/maintenance owner is proposed instead of fixing the
  canonical generation owner.
- A bounded operation needs a repository-sized in-memory authority.
- Scale targets can pass only by reducing retained roots, integrity coverage,
  authorization, or GC grace.
- A single-host run is presented as distributed team evidence.

## Phase 4: Turn performance and cloud cost into an SLO

### Context

Current large-repository evidence shows that response-pack construction,
client `index-pack`, and owner pack generation can dominate. Generated-pack
caching and range/read budgets already exist, so optimization must be driven by
the phase-3 report rather than another cache or configuration surface.

### Design

#### 1. Establish a versioned performance/cost envelope

For each fixture and operation, report p50/p95/p99 wall time plus planner,
origin-read, pack-assembly, artifact-publication, network-drain, client-index,
and connectivity time. Record CPU, peak RSS, disk spill, request counts by
verb, bytes read/written/egressed, retry/throttle rate, cache outcome, selected
objects, and response bytes.

Publish separate SLOs for small, medium, and large reference repositories.
Use counts and bytes as portable gates; accept wall-time comparisons only from
two isolated same-topology runs whose baseline stability passes the existing
20% rule.

#### 2. Optimize only measured owners

Evaluate in this order:

1. reuse verified packed delta entries and bases in the one response assembler;
2. coalesce identical generated-pack work across processes;
3. reduce origin range amplification without overfetching sparse selections;
4. improve generated artifact delivery and cache-service locality;
5. benchmark response pack shape against client `index-pack` CPU/RSS;
6. parallelize owner pack reads/assembly only inside existing byte/request and
   lease budgets.

Every experiment compares producer CPU, response bytes, origin cost, client
index time, memory, correctness, and cache reuse. A server-side win that makes
developer completion or egress materially worse is not accepted.

#### 3. Add regression gates

Keep microbenchmarks for pure codecs/planners, fixture benchmarks for storage
request shape, and distributed E2E for product latency. PR CI runs stable small
tests; scheduled/manual infrastructure runs large SLO profiles. A release is
blocked only by rows the support matrix claims, not by unavailable optional
providers.

### PR slices

1. Unified SLO/cost schema and baseline verifier.
2. Response-pack delta/shape experiments; accept one canonical strategy.
3. Origin range and generated-artifact delivery optimization.
4. Owner pack-generation optimization under existing budgets.
5. Distributed cache/provider requalification and regression wiring.

### Milestone handoffs

**4A — Measure the user's whole operation.** Context: faster pack production
can hide slower client indexing or higher object-store cost. Entry: Phase 3's
stable fixtures and an approved small/medium/large SLO profile. Implement
slice 1. Exit: the same report includes end-to-end latency, producer/client
split, RSS/spill, cache state, requests, transferred bytes, retries, and cost
inputs. Reject unstable topology, mismatched object selections, or insufficient
tail samples. Absolute budgets are selected before the candidate run.

**4B — Accept measured optimizations.** Context: caches, pack policies, and
read budgets already exist; another parallel path adds complexity without
proving user benefit. Entry: 4A identifies the dominant cost. Implement slices
2–4 as separate differential experiments within canonical owners. Exit:
exact objects and strict Git checks remain unchanged, the relevant numerical
targets below pass, and producer wins do not hide client/request/memory
regressions. Reject unsuccessful experiments; do not keep competing runtime
strategies merely to preserve benchmark work.

**4C — Sustained performance and regression gate.** Context: a warm single-user
benchmark misses cache stampedes, noisy neighbors, and maintenance contention.
Entry: accepted 4B changes and the Phase 3 distributed environment. Complete
slice 5. Exit: cold/warm fanout, producer coalescing, cache-hit, maintenance
push-p95, and two-run repeatability criteria pass on claimed providers. The
release gate consumes exact-candidate reports. Roll back a regression only
through a compatible implementation; never serve stale or unauthorized cache
content to maintain an SLO.

### Acceptance criteria

- [ ] Every response passes `git index-pack --strict`, exact selected-object
      comparison, shallow-boundary comparison, and `git fsck --full`.
- [ ] The large full-clone response is no larger than 125% of Git's reference
      pack for the same selected objects.
- [ ] On a valid same-host differential, median response-pack CPU is at most
      50% of the accepted baseline.
- [ ] A repeated identical clone performs at least 90% fewer origin range
      reads and at least 70% less response-pack CPU than its cold miss.
- [ ] Fifty identical cold clones create no more than two producers; 100 warm
      clones achieve at least 90% verified artifact hits and at least 80% fewer
      origin requests than cold fanout.
- [ ] Push p95 during maintenance is no more than 10% above the same workload
      without maintenance, excluding separately reported same-ref contention.
- [ ] No accepted optimization increases response bytes, provider requests,
      client p95, or peak memory by more than 10% without a documented product
      tradeoff approved before rollout.
- [ ] The report estimates request and transfer cost from provider-neutral
      counts; provider pricing is presentation data, not embedded core policy.
- [ ] Two consecutive valid runs have identical correctness and median
      operation times within 20%; otherwise the comparison is invalid.

### STOP conditions

- Performance requires skipping validation, serving unauthorized objects, or
  weakening cancellation and budget enforcement.
- A new knob is proposed before one safe default is tested across supported
  providers.
- Cache identity does not bind repository, manifest, authorization, filter,
  shallow state, and pack policy.
- A benchmark hides client indexing, retries, or origin requests.

## Phase 5: Deliver the team and managed-service product

### Context

The repository already has logical managed locators, discovery/profile/login,
administration DTOs and clients, membership roles, service accounts, transfer
grants, OpenAPI compatibility checks, auth-store composition, protected-push
verification/finalization, path-scoped views, and a Python enterprise reference
service. Missing is the cohesive, operated HTTP control plane and its durable
repository/migration lifecycle.

### Architecture

#### Control plane

Build one versioned HTTP service from the existing managed OpenAPI contract:

- exact-authority discovery and OIDC validation;
- organization, membership, repository, service-account, and lifecycle APIs;
- policy evaluation with explicit deny and non-disclosing not-found behavior;
- repository placement catalog and strong revision/ETag concurrency;
- short-lived transfer-grant broker using direct downscoped credentials or the
  existing gateway contract;
- protected-push session state, idempotency, verification job, and service-
  owned finalization;
- immutable tenant audit events, usage accounting, job status, and quotas;
- health/readiness, metrics, tracing, rate limits, backup, and restore.

The service database stores identity, policy, placement metadata, sessions,
idempotency results, audit, and jobs. It does not store Git packs or Crab large
objects. Secrets use the deployment secret manager; one-time service-account
tokens are stored as verifiers, not recoverable plaintext.

#### Data plane

Reads receive repository- and operation-bound grants. Protected pushes receive
only a push-ID-bound immutable staging prefix. The verifier consumes the
existing Rust receive helper, validates the prepared plan and changed paths,
rechecks current policy, then acquires coordination and commits canonical
state. A retry returns the same terminal result. An expired/revoked actor
cannot start a new grant; already-issued direct grants remain bounded by their
short declared lifetime.

#### Repository lifecycle and migration

Provisioning creates isolated placement, layout descriptor, generation-0
manifest, coordination resources, encryption/retention policy, and backup
registration before setting the logical repository active.

Implement a service-owned direct-to-managed migration job:

1. inventory and authoritative-fsck the direct source;
2. create a durable job and target placement;
3. copy immutable objects with content identity and resumable checkpoints;
4. copy/normalize canonical metadata under a target that is not yet writable;
5. run full target integrity, denied-principal, staging-scope, and restore tests;
6. take a final source delta under a declared writer freeze/fence;
7. atomically activate the managed repository and record cutover evidence;
8. retain the source unchanged through the rollback window.

Changing a URL is never a migration. Rollback before target writes returns to
the source. Rollback after target writes requires a stopped, audited reverse
delta; it is not automatic dual writing.

The Python enterprise auth endpoint remains a separate direct-repository
product. Replace it only through the whole-environment parity/canary process in
`packages/web/content/docs/cli/managed-service/migration.mdx:136-193`. Never
try Rust then Python for an individual request.

### Delivery stages

#### 5A. Self-hosted managed control plane

Context: API contracts and helper binaries do not yet provide an installable,
operable team service. Entry: Phase 0 contracts and the deployment decisions
are approved; Phase 1/2 guarantees are required before claiming safe data use.

Ship discovery, login, org/repo lifecycle, RBAC, service accounts, direct read
grants, protected push, audit, backup/restore, and deployment charts/runbooks.
Use one production backend first; keep other provider cells preview until their
grant and data-path matrices pass.

Acceptance: a documented clean deployment supports two users with different
roles and one CI identity through create/push/clone/revoke; helper/job crashes
resolve idempotently; a restored deployment resolves and hydrates the same
repository. Record the exact service image and backend. No hosted claim yet.

#### 5B. Hosted private preview

Context: running one installation does not prove tenant isolation or an
operational service. Entry: 5A passes and a named operator owns the canary.

Operate tenant isolation, quotas, support diagnostics, billing-grade usage,
on-call alerts, backup restore, regional failure exercises, abuse controls, and
data deletion. Invite only canary tenants with explicit support bounds.

Acceptance: two isolated test tenants pass cross-tenant denial and noisy-
neighbor scenarios; paging, quota exhaustion, backup recovery and tenant
deletion are exercised. Invitees receive explicit limits and a support route.
Usage accounting here does not require launching paid billing.

#### 5C. Migration and automation

Context: existing direct-storage teams need a safe cutover, not a URL rewrite.
Entry: 5A lifecycle and Phase 1 authoritative source/target verification pass.
This stage can precede or overlap 5B; public hosting is not a prerequisite.

Ship resumable direct-to-managed migration, CI workload identities, token
rotation/revocation, mirror integrity integration, and cutover/rollback reports.

Acceptance: terminate and resume each migration boundary, compare exact refs
and bytes, prove a stale source cannot cut over, and exercise rollback both
before and after target writes under the declared freeze/reverse-delta policy.
No source deletion is part of successful migration.

#### 5D. General availability

Context: preview functionality is not a reliability or support commitment.
Entry: the advertised self-hosted/hosted profile passes its applicable earlier
stages; migration claims specifically require 5C.

Remove the preview label only after sustained E2E, security, reliability,
scale, migration, restore, and support evidence passes for the advertised
regions/providers.

Acceptance: the release owner approves the exact supported matrix, security
review, sustained canary, timed restore, rollback rehearsal, on-call/runbooks,
and evidence manifest. No unqualified region/provider is added implicitly.

### PR slices

1. Durable service state model and API handler conformance.
2. Policy/grant broker and least-privilege provider adapters.
3. Protected-push job orchestration and idempotent finalization.
4. Audit/usage/quotas, service accounts, readiness, backup, and restore.
5. Self-hosted deployment and E2E release profile.
6. Direct-to-managed migration job and cutover/rollback report.
7. Hosted tenancy, regional operation, security review, and GA gates.

### Acceptance criteria

- [ ] From a clean client, a user can discover, sign in, create an organization
      and repository, wait for active state, push, clone, fetch, and hydrate
      through the logical URL with real object-store side effects.
- [ ] Owner/admin/writer/reader/billing roles and explicit denies are enforced
      on every route; inaccessible repositories disclose no placement.
- [ ] Read grants contain only required permissions and scope. Push grants can
      create immutable staging objects but cannot read unrelated data or write
      canonical manifests/refs.
- [ ] Finalize rechecks actor, policy, base generation, ref updates, staged
      hashes, dependency closure, and changed paths. Retry is idempotent; stale
      or unauthorized sessions never publish.
- [ ] Revocation blocks new API requests immediately; maximum residual direct-
      grant access is no longer than the documented grant lifetime.
- [ ] Audit records every auth decision, grant, push transition, lifecycle
      mutation, migration transition, and administrative change without tokens
      or physical placement in client-visible errors.
- [ ] Backup restore recreates service state and can resolve, clone, and hydrate
      a representative repository inside declared RPO/RTO.
- [ ] Migration survives interruption at every checkpoint, resumes without
      duplicate publication, proves exact refs/bytes and denied access, and
      exercises rollback before source retirement.
- [ ] Tenant A cannot list, grant, read, write, infer, or exhaust tenant B's
      repository under isolation and load tests.
- [ ] Preview/GA docs match deployed capabilities; the managed-service preview
      label is removed only after the complete release evidence row passes.

### STOP conditions

- The service must proxy all bulk bytes even where safe direct grants exist.
- The client receives canonical mutable storage credentials.
- Policy is checked only at prepare and not again at finalize.
- Migration requires raw provider sync without manifest, identity, audit, and
  cutover proof.
- Hosted launch lacks restore evidence, tenant isolation, or an on-call owner.

## Phase 6: Consolidated release and rollout

### Evidence manifest

Every claimed phase contributes one signed or checksummed report to a single
release evidence manifest:

| Domain | Required proof |
|---|---|
| Compatibility | N-1 repository/API/CLI read-write or fail-before-write result |
| Integrity | Complete fsck coverage, failure injection, repair replay, restore |
| Git | Git/OS/provider matrix, expected rejections, strict fsck, byte checks |
| Mirror | Divergence detection, missing-data block, plan/apply reconciliation |
| Scale | Repeated 10,000-push, distributed fanout, owner/GC fault and failover |
| Performance | Cold/warm SLO, producer/client split, request/byte/cost envelope |
| Managed | Identity/RBAC/grants/protected push/audit/backup/migration E2E |

### Rollout gates

1. Shadow or report-only mode where an alternate derived plan can be compared
   without publishing state.
2. Maintainer-owned isolated repositories.
3. Opt-in canary users/tenants pinned by immutable repository identity.
4. Percentage expansion after seven days, or an explicitly approved equivalent
   sustained test window, with zero correctness/security mismatches.
5. Default-on/support claim only after exact-release evidence passes.

Rollback stops new selection or routing, preserves immutable diagnostic
artifacts, and returns to the prior compatible owner/generation. It never
rewrites or deletes already published immutable content as an emergency step.

### Acceptance criteria

- [ ] The candidate tag, binary digests, reports, and deployed service image all
      resolve to the same source commit.
- [ ] Every claimed support row passes. New unqualified capabilities remain
      explicit preview/unsupported and opt-in. Missing evidence for an existing
      tagged support claim blocks reaffirming that claim; it does not authorize
      silently removing the capability or demoting the shipped contract.
- [ ] One injected correctness mismatch, authorization widening, incomplete
      integrity result, evidence mismatch, or restore failure blocks rollout.
- [ ] Upgrade, downgrade/refusal, rollback, backup restore, and incident
      procedures are executed, timed, and retained before GA.
- [ ] Product docs, CLI help, API schema, dashboards, alerts, and runbooks match
      the final supported surface.

## Cross-phase test strategy

### Unit and property proof

- Manifest/layout/version admission and compatibility fixtures.
- Git DAG/ref/visibility and recipe/chunk/shard reachability properties.
- Snapshot staleness, repair-plan idempotence, and journal resume.
- Grant narrowing, expiry, repository binding, idempotency, RBAC, and tenant
  non-disclosure.
- Pack selection, delta bases, cache identity, budgets, and cancellation.

### Integration proof

- Real object-store APIs injected into the canonical owners.
- Real Git processes and versions, not protocol mocks alone.
- Rust verifier helpers invoked through the service job boundary.
- Owner, writer, fsck, repair, GC, and migration overlap with kill points.
- Managed API database, queue, secret manager, storage grants, and audit sink.

### E2E proof

Every product row reaches Level 3 or higher: user command, real dependency,
real side effect, and visible result. Destructive, security, and recovery rows
also prove error paths and persistence. Required terminal checks include fresh
clone, `git fsck --strict --full`, Crab authoritative fsck, representative or
full byte reconstruction as declared, and exact-prefix cleanup.

## Definition of done

- [ ] All five priority outcomes and their acceptance criteria pass for the
      support matrix being advertised.
- [ ] Existing detailed GC and large-repository plan evidence tables are
      updated rather than duplicated with contradictory status.
- [ ] No required invariant depends on an undocumented fallback, shared static
      credential, client-only hook, or in-memory repository-sized authority.
- [ ] Shipped contract changes have compatibility fixtures and a documented
      migration/refusal boundary.
- [ ] Release evidence is exact-commit, release-binary, real-provider where
      claimed, retained, redacted, and independently verifiable.
- [ ] Docs distinguish shipped, preview, experimental, and unsupported behavior.
- [ ] The work leaves one canonical implementation path per responsibility.

## Maintenance rule

Update the status and evidence references in this document after each phase.
When a detailed subsystem plan owns an implementation, update that plan's
evidence table and link it here; do not fork the design into a second owner.
Any new feature request must show which of the five outcomes it advances and
why it should precede an open P0/P1 acceptance criterion.

## Upstream contracts to retain in implementation evidence

Consulted 2026-09-02. These are design inputs, not substitutes for proving the
selected dependency version, provider API and candidate binary at execution.

- Git helpers advertise a subset of capabilities. `stateless-connect` has a
  defined pre-handoff response contract; it is not permission to substitute a
  full fetch after accepting v2 semantics. Keep ordinary helper push behavior
  distinct from unsupported v2 receive-pack takeover.
  [Git remote-helper contract](https://git-scm.com/docs/gitremote-helpers),
  [Git protocol v2](https://git-scm.com/docs/gitprotocol-v2).
- Pre-push receives a sequence of local/remote ref and object names on stdin.
  The hook's current branch is not the pushed ref set.
  [Git hook contract](https://git-scm.com/docs/githooks).
- S3 conditional writes distinguish create-if-absent and ETag-match updates;
  concurrent operations can produce conflict/precondition failures, with
  multipart-specific retry behavior. Preserve provider error classification
  instead of treating a conflict as a generic retryable write.
  [S3 conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html).
- GCS generation preconditions bind the specific object version. Its XML
  multipart precondition limitations mean S3 qualification cannot stand in
  for GCS qualification.
  [GCS request preconditions](https://docs.cloud.google.com/storage/docs/request-preconditions).
- Azure ETag conditions reject stale updates; blob leases have their own
  enforcement rules. Map these through the canonical storage/coordination
  boundary and test lost leases independently of CAS conflicts.
  [Azure concurrency control](https://learn.microsoft.com/en-us/azure/storage/blobs/concurrency-manage).
