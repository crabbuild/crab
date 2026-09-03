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

Implement this packet at the ref-journal commit point, not in mirror's Git
subprocess wrapper. The wrapper cannot distinguish its own successful mutation
from a concurrent writer that published the same OIDs, and a local receipt can
disappear with the caller. Use this sequence after the compatibility decision:

1. `mirror --apply-plan` passes the validated 64-hex plan identity through one
   internal child-process field. It is not user configuration. The remote
   helper admits it only for mirror apply and threads it through the existing
   push configuration to the direct or managed commit authority.
2. The authority create-writes an immutable intent keyed by plan and attempt
   immediately before its existing commit boundary. Direct mode does this
   after every expected-old journal head is prepared and binds the transaction
   identity plus dependency digest. Managed finalize does this after verifying
   the current manifest CAS token and binds the exact base and candidate
   manifest generations and canonical body digests. Both forms transitively
   bind the canonical ref edits and repository/storage identity through the
   plan. They contain no URL credentials, provider tokens or local paths.
   Multiple attempts are bounded and ordered; an uncommitted attempt does not
   prevent a later safe retry. An identical immutable candidate reuses its
   existing intent; only a different candidate consumes another attempt, so
   lost terminal writes cannot turn one commit into two apparent results.
3. The existing active marker remains direct mode's atomic ref visibility
   boundary. A direct transaction is attributable only when its exact identity
   is reachable from current journal heads or retained ancestry after
   compaction. Managed mode retains its existing manifest CAS boundary: the
   candidate is attributable only when its exact body is current or present in
   validated immutable manifest history together with its bound base. Ref
   equality, candidate-body existence and intent existence alone are not commit
   proof. GC retains the journal, manifest-history and plan objects.
4. The authority or a later read-back create-writes the plan's terminal receipt
   after proving that transaction committed. The receipt binds the committed
   transaction and dependency proof. Replaying the same plan returns this
   historical commit result without another mutation. The output separately
   reports the newly inspected current drift; it must not reuse the receipt's
   commit-time `equal` state as present convergence. A conflicting receipt,
   target mismatch, unknown version, missing history or more than the attempt
   limit fails closed as corrupt/unverifiable; it never falls back to equality.
5. If the child response is lost, mirror reads the receipt and unresolved
   intents while it still owns the cache. A committed intent is promoted to the
   same terminal result; a definitely uncommitted intent permits a new
   expected-old attempt; indeterminate provider/history reads return an
   uncertain result and preserve dependencies for recovery. Cancellation never
   turns an unknown commit into a retry or compensating delete.

**Compatibility decision.** Keep the canonical-v1 descriptor, transaction and
manifest JSON unchanged. Store versioned intent and terminal objects under a
new repo-local `refs/journal/plans/v1/` namespace. Direct protected pushes keep
push-plan schema v1; a mirror-plan identity uses push-plan schema v2, which new
receive helpers accept and tagged helpers reject before canonical mutation.
Tagged v1.0.1 reads only the
existing head, transaction, active-marker and frontier keys; its compactor
removes the exact active-marker key after promotion and retains immutable
transaction bodies. Existing repo and bucket GC enumerate known pack,
partitioned file-index, shard and xorb candidates rather than deleting unknown
repo-local objects. The additive keys are therefore ignored and retained by
the shipped client, while a new client can trace an intent's transaction
through current heads and preserved parent transactions after compaction.
Do not add fields to the version-1 transaction or marker: both use strict
unknown-field rejection.

Before shipping, add the namespace to explicit backup/restore inventory and
managed-service read/finalize authorization, and add upgrade/downgrade tests
that run tagged v1.0.1 compaction between intent, commit and terminal read-back.
Direct credentials already address the repository namespace, but managed
clients must never receive canonical mutable write permission; the service
commit owner writes intents and receipts. GC gains retention assertions, not
an unknown-key delete fallback. If any provider or future maintenance owner
cannot preserve the namespace, layout admission must refuse receipt-enabled
repositories rather than silently degrading to ref equality.

Acceptance is one deterministic fault table: kill before intent, after intent,
after one/all head prepares, immediately after active-marker creation, after
head promotion, during compaction, after compaction and after terminal-receipt
write. For each point restart the caller and reapply the same plan. Assert the
old batch or the exact new batch, never partial refs; at most one committed
transaction; no duplicate dependency publication; the same terminal result
after commit; safe retry only before commit; and an explicit uncertain result
when history cannot be proved. Add different-plan/same-OID, unrelated-writer,
target-copy, receipt-corruption, attempt-exhaustion, cancellation, lost-lease,
GC-overlap and managed-finalize siblings. Real S3, GCS and Azure rows must use
their selected conditional-write/version contracts; RustFS is local direct-mode
evidence only.

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

### Qualification evidence-path boundary: 2026-09-03 UTC

**Observed failure.** The `d89a152` current-Git CI job stopped while writing
command 377's log: a default label containing two absolute checkout paths
exceeded the filesystem filename limit. No failed product check was recorded
at that point, but the incomplete run is not a pass. The retained artifact is
`git-protocol-v2-git-current-33729717072-1`.

**Correction and acceptance.** Bound only log-label UTF-8 bytes to 120; keep
the monotonically increasing command index for uniqueness and preserve full
command names/arguments in the report. Text and binary command paths share
this logger. Performance fixture labels and other qualification runners are
unchanged; this is not a claim that their separate log owners are bounded.
Two filesystem tests pass after the correction; the long-label regression
fails beforehand. Long ASCII/multibyte labels remain writable, repeated or
truncated labels do not overwrite logs, and short labels stay readable. The
protocol CI job runs these tests; ten existing matrix-verifier tests and
workflow syntax pass.
No expected-failure list, check inventory or evidence baseline is weakened.
Require a fresh terminal current-Git matrix run to close this failure.

**Separate local evidence.** Optimized `87cbfda` passed full and partial Git
clone LFS qualification on RustFS: 36 commands, 8 checks, two 65 MiB objects,
promised Git pointer retrieval, Git/LFS fsck, checkout and exact payload
SHA-256 bytes. Binary SHA-256:
`dab272482a5965ca676d2906eb7a69c7a0510e4482eace6ab079e459f122ff76`.
Report: `phase2-lfs-partial-87cbfda-20260903/report.json`; functional-only.

The separate recovered-history `d89a152` replay passed initial import and
917 incremental pushes, then its diagnostic driver failed before push 918.
It selected a hidden payload directory excluded by add's existing walker;
add selected no files and the driver's subsequent SQLite probe failed.
Retain that failed report and unchanged-source proof. Correct the fixture to
explicitly track visible paths, assert two staged identities and unchanged
Git index before any long replay, then rerun in a new namespace. This failure
neither proves a native push regression nor completes original-history proof.

### Recent-selection process ownership: 2026-09-03 UTC

**Context.** After fetch/pull adopted verified LFS discovery, the preceding
recent-ref selection still used four unsupervised Git command sites:
integer/boolean config, ref listing and commit listing. Fetch carried its
caller token around those calls but not into them. Prune called the same
helpers without a token while retaining its repository-scoped prune lock.
This left a stalled config/ref query outside the shared process lifetime.

**Implementation boundary.** `crab/src/lfs/recent.rs` now routes all four
sites through the existing `git::process` owner. Fetch and prune pass their
same caller token; no fresh token replaces it. Stdout and stderr each have
the existing 64 MiB capture bound, and pipe workers are joined before return.
An oversized stream fails the operation rather than exposing partial output.
Spawn/read errors retain their I/O source. The shared owner retains its
10-second cancellation grace and platform process-tree cleanup. This is a
per-stream bound, not a total-memory SLO. No dependency, config option,
alternate transport policy or subprocess supervisor is added.

Git config still owns typed canonicalization and missing-key status. The
helper preserves existing defaults and invalid-value errors: integer suffixes
remain accepted, zero remains meaningful and negative unsigned values fail.
The contract comes from [Git config](https://git-scm.com/docs/git-config) and
the supported old client's
[`get_value` implementation](https://github.com/git/git/blob/v2.30.9/builtin/config.c#L297-L381).

**Acceptance.** Focused tests must prove pre-cancelled queries do not access
a repository; fresh real-Git ref/commit selection and previous LFS pointer
versions remain visible; typed config defaults, suffixes and invalid values
behave identically; excess output on either pipe fails; and a stalled real
Git child is stopped/joined before cancellation returns. CI runs recent and
prune tests in Linux and native Windows/macOS profiles. The stalled-child and
oversized-pipe fixtures are Unix-only; generic Windows cleanup still requires
its separate process-owner contracts. Qualification is not complete until
these changes have exact-candidate terminal evidence.

Local checkpoint: 55 selected recent, fetch, prune and shared-discovery tests
pass on macOS, including seven new tests. Correctness/suspicious Clippy,
workspace formatting and workflow syntax pass. Other Clippy warning classes
remain in unchanged fetch/prune code; this is not an all-warnings-clean claim.
The new production lines carry the existing token across command signatures
and consolidate four command captures behind one private owner call; they do
not introduce a second selection algorithm.

**Remaining sibling work; not silently treated as fixed.** Prune still has
unsupervised ref/worktree/object commands, an unchecked batch parser and an
optional in-process walker that skips read errors. Both recent callers still
inherit Git's existing repository/transport environment; unifying that policy
requires resolving the whole prune selection against one explicit repository.
The tagged recent implementation's ambiguous-revision-as-empty behavior and
malformed-row skipping remain unchanged in this cancellation-only patch.
Replace those with typed unborn-ref handling and fail-closed inventory proof
in the next selection packet. Acceptance: invalid/missing selected revisions
must never produce a successful incomplete retention set or deletion.

The [Git LFS config contract](https://github.com/git-lfs/git-lfs/blob/main/docs/man/git-lfs-config.adoc)
also reveals selection gaps independent of process ownership: previous-change
windows are relative to each selected ref's tip, remote refs belong to the
selected remote, and prune offsets apply to both recent windows. Current
helpers use wall-clock commit cutoffs and all remote refs. Qualify old ref
tips, two remotes, non-monotonic commit dates, offset boundaries, unborn HEAD
and staged-only pointers before changing policy. Crab documents `--force` as
confirmation-only, whereas upstream Git LFS defines it as pruning pushed
objects even when required by current checkouts, implying `--recent`. This is
a contract discrepancy to resolve explicitly, not evidence that upstream
`--force` merely skips confirmation. This packet does not change that flag;
the cancellation proof does not certify prune safety or selection parity.

### Ref-journal rollback ownership: 2026-09-03 UTC

**Observed failure.** Tracing mirror apply into the canonical native push
owner found a pre-publication cleanup race in
`crates/crab-metadata/src/ref_journal.rs`. When a multi-ref prepare failed,
rollback used CAS for a previously existing head but unconditional DELETE
for a newly created head. After ownership moved to a successor, delayed
cleanup could therefore erase that successor's committed head. A deterministic
new-head/successor/late-rollback test fails before the correction: the visible
head loses its transaction identity. Existing-head rollback already used CAS.

**Correction.** Both cases now restore the original head using the exact
version returned by the prepare write. The initially absent case restores
the existing version-1 empty-head shape. That shape is accepted by tagged
v1.0.1's validator, publishes no ref and can be prepared by the next writer.
No journal schema, storage key, dependency or rollback implementation is
added. Conditional-write conflict preserves the successor and remains a
cleanup warning; the original commit failure still reaches the caller.
Empty heads are retained rather than reclaimed by an unsafe delete.

The dependency contract is `object_store 0.14.1`'s `PutMode::Update`: the
provider atomically checks the supplied object version. Crab's `Store::update`
preserves the complete ETag/version pair and does not blindly retry a stale
CAS. [S3 conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)
document the corresponding If-Match refusal. RustFS proof verifies the actual
configured service, not just a mock interpretation of that contract.

**Acceptance and evidence.** Fourteen focused journal tests pass after the
fix. Three new hermetic tests cover delayed rollback after a successor on
both new/existing heads, ordinary restoration of an existing head, and an
actual multi-ref prepare conflict with no marker/partial publication. They
also verify a subsequent commit retains a coherent parent chain. A one-time
explicit RustFS qualification passed on `crabbuild` with the same successor
scenarios; its small isolated namespace is retained as
`qualification/ref-journal-rollback-91961-1788425898401725000`. The test uses
standard AWS endpoint/bucket/credential inputs and introduces no product
configuration. The live harness is kept outside the shared metadata crate;
CI selects the hermetic journal tests for Linux, Windows and macOS and does
not silently count live-provider proof as part of that default run.

Both `storage`-only and `remote-index` feature selections pass the fourteen
hermetic tests; the minimal no-default-features build also passes. Three
selected Crab integration-owner tests pass, covering ref-journal push
admission and generation-owner compaction. Correctness/suspicious Clippy,
formatting and workflow syntax pass; existing unrelated warning classes
remain. Exact-candidate cross-platform CI still needs a fresh run.

**Sibling boundary and remaining work.** The changed function is the sole
rollback callee of `commit_ref_transaction`; native mirror/direct pushes
reach it through `crab/src/metadata/manifest.rs`. Promotion and existing-head
restoration already use version-checked writes. Protected-server manifest
publication has a separate CAS boundary and does not call this rollback.
No immutable data, active marker or successor ref is deleted by the new path.
This does not establish continuous lease fencing through marker publication,
resolve uncertain commit responses, or implement plan-bound terminal receipts.
The active marker is removed by compaction, and compacted reads can stop at a
frontier without historical transaction bodies. Those are not durable receipt
contracts; ref equality alone remains insufficient to attribute a commit to a
plan. Keep that original terminal-outcome/replay packet open.

### Native-Git/mirror RustFS checkpoint: 2026-09-03 UTC

Optimized `7dd22af` passes the unchanged native-Git/mirror harness: 464
commands, 133 checks, no failed checks. It exercises native ref lifecycle,
full/shallow/partial/filter/security reads, composed mirror hooks,
plan/apply/reapply and approved deletion safety. The optional prior-release
rollback binary was not supplied in this run; prior rollback proof is not
silently attributed to this candidate.

Report: `phase2-protocol-mirror-7dd22af-20260903/artifacts/report.json` under
the external qualification root. Binary SHA-256:
`b2c8baecc06759d62416f9a002692f1cb3010a15c924ec0cc7c908cb03901dce`.
The reported binary/source revision matches. The report correctly records
the checkout as dirty (unrelated generated Fumadocs/runtime files), so this
is functional evidence on a concurrent development host, not a clean release
candidate or controlled performance baseline. Original-history Kubernetes
replay remains separate, pinned to `56d336a` and not yet complete.

### Transfer-agent terminal input ownership: 2026-09-03 UTC

**Context and reproduced failure.** On optimized `7dd22af`, a real
`crab lfs-transfer-agent` process reports the unsupported-init error but does
not exit while its parent keeps stdin open. Closing stdin lets it exit with
code 1. The canonical protocol reader read ahead after sending an event,
while the event loop attempted to abort and join that blocking task after
a fatal semantic decision. The focused regression fails before the fix.

**Design.** Each successfully parsed event carries a one-shot permission for
the next read. The sole protocol owner releases it only when continuing;
fatal init/order decisions drop it, and the reader exits before being joined.
Failed init output uses the same cleanup path. Terminal cleanup joins the
reader; no second parser, protocol/configuration extension or dependency is
added. Pending parsed input is bounded to one event; admitted transfer
concurrency remains owned by the existing coordinator. This protects both CLI entry points,
`lfs-transfer-agent` and `lfs standalone-file`, without duplicating policy.

**Contract and acceptance.** [Git LFS custom transfers](https://github.com/git-lfs/git-lfs/blob/main/docs/custom-transfers.md)
defines fatal process errors separately from per-object completion errors;
normal terminate expects cleanup with no reply. The pinned Tokio 1.52.1
[blocking-task contract](https://docs.rs/tokio/1.52.1/tokio/task/fn.spawn_blocking.html)
does not permit aborting a started blocking read. New loopback-stream tests
keep input open across unsupported/duplicate init and pre-init upload,
failed init output, and normal terminate. They assert both the result and
reader closure before the peer closes, and unblock a regressed reader before
failing so the test itself does not leak it. Existing transfer round-trip,
progress, declared-size, duplicate-OID and concurrency tests pass. Forty-six
focused tests pass: twenty-two agent, seven coordinator and seventeen batch
tests. Cross-platform execution and an updated optimized-binary smoke remain
required for the new reader handshake.

Production-library correctness/suspicious Clippy and formatting pass. Broader
test-target Clippy is not green: the crate-wide unwrap/expect/panic restrictions
also cover test fixtures, including existing fixtures outside this change.
No lint configuration or expectations were changed; this is not a claim of
a clean all-target lint run.

**Remaining boundary.** This fixes terminal decisions, not cancellation of
an idle stdin read, whole-transfer storage cancellation, bounded JSON-line
length, or asynchronous transfer-output error propagation. The session still
creates its own coordinator cancellation token. Those original lifecycle
gates remain open; passing this regression cannot close them.

### Released-claim renewal ownership: 2026-09-03 UTC

**Context and reproduced failure.** Holder-checked release leaves a tombstone
with the original holder and zero expiry. Shared cached renewal reread that
tombstone after its stale CAS failed, checked only holder identity, and could
revive the claim. The independently implemented product heartbeat had the
same defect. Each regression failed before its respective fix; fixing the
shared owner alone did not fix the heartbeat.

**Design.** `crates/crab-coordination/src/push_lock.rs` now rejects released
payloads during renewal. A narrow `PushLock::renew_if_holder` entry point
lets `crab/src/coordination/heartbeat.rs` use that existing bounded renewal
owner without a cached version. The duplicate heartbeat serialization, retry
classification and retry loop are removed. Normal cached renewal retains its
single conditional PUT. No persistent shape, key, dependency or configuration
changes. The dependency boundary remains `object_store 0.14.1` conditional
updates with the complete ETag/version; a successful release invalidates the
former cached token, and a racing release invalidates the heartbeat's token.

**Acceptance.** After explicit release, both renewal entry points fail and
leave the tombstone bytes and version unchanged. The heartbeat cancels its
operation after observing release. Existing acquisition, live renewal,
reacquisition, holder-safe release, retry/deadline and independent-stop
contracts must continue to pass. Push, repository maintenance, repack and
history recovery keep their existing stop-before-holder-checked-release
ordering; their public call signatures do not change. Using one renewal
owner is preferable to adding a second tombstone guard to duplicate policy.

**Proof retained.** Twenty-nine shared-lock tests and ten hermetic heartbeat
tests pass; minimal coordination compilation passes. The opt-in real RustFS
test passes on `crabbuild`, retaining only
`qualification/lease-renewal-52316-1788428553880142000`. It acquires and
explicitly releases a real claim, then exercises cached renewal and the real
scheduled heartbeat, verifies cancellation, and compares bytes plus version.
The test is ignored in ordinary suites, not silently counted as a provider
pass. CI explicitly selects the hermetic contract on Linux, Windows and
macOS and watches both owner paths. Updated release-shaped workflow proof
and exact-candidate cross-platform results remain required.

Four push-acquisition/handoff tests, the long-lived internal-owner renewal
failure test and thirteen repack tests pass. Recovery tests are **7 passed,
1 failed**: `pruning_old_root_makes_its_unique_pack_collectible` creates a
manifest without a layout descriptor and fails at the first GC snapshot,
before pruning or lease renewal. The strict snapshot-layout requirement was
introduced earlier in this PR; this is not an unrelated-main or green-suite
claim. Fixture-initialization approval is pending; production validation and
the test assertions were not weakened. The `maintenance::tests` selector
matches zero tests and is not counted as proof. Production-library
correctness/suspicious Clippy, formatting and workflow syntax pass; other
existing warning categories remain.

**Remaining boundary.** This is revocation safety, not continuous lease
fencing. A non-released expired claim and a later multi-object active-marker
write still require separate ownership/publication proof. Version-1 readers
treat active-marker presence as committed, so an in-place abort payload is
not a safe extension. Durable receipts also require compactor cooperation;
do not infer plan attribution from equal refs or transient journal markers.
An explicit repository-format/writer upgrade decision is pending. No new
format, automatic migration, terminal receipt or acceptance closure is
implied by this packet.

### Per-ref recent commit windows: 2026-09-03 UTC

**Context.** A real-Git regression with two independently dated historical
branches returned an empty selection even though both had commits within
their configured windows. `lfs::recent` used today's wall clock for history;
Git LFS measures history backward from each selected tip. Prune already added
its offset to the ref window but omitted it from the commit window.

**Implementation.** Resolve each requested revision with Git's single-commit
verification before walking frozen OIDs. One streamed topological walk seeds
each tip's cutoff and propagates the widest applicable window to shared
parents, including across non-monotonic dates. Each commit is visited once;
consumed frontier entries release their memory budget. Roots arrive on stdin
instead of a growing command line. Retained graph/selection state uses the shared
discovery inventory budget; input records are separately bounded. Reject failed
revision resolution and malformed timestamped records; never treat a missing selected
revision as an empty successful history. The existing fetch owner supplies
zero extra days; the prune owner supplies its configured offset. A disabled
window remains disabled, and addition/subtraction saturate safely. No new
configuration, dependency, persistent shape or alternate process owner.

**Acceptance.** Old selected tips and two independent windows produce their
own expected commit sets; duplicate aliases/annotated tags do not duplicate
results; a newer-dated ancestor behind an older parent remains discoverable;
offset boundaries and zero-disabled windows behave consistently. Invalid
selected refs fail closed. Existing cancellation and bounded-output tests
must pass. Exact-binary full/partial RustFS fetch must retrieve current and
historical payloads with verified SHA-256 bytes when commit dates are old.

**Pre-fix evidence.** The optimized `b44fe67` binary reproduced the missing-history
failure against RustFS after 39 commands: current payloads and full clone passed,
but the partial clone still lacked historical pointer blobs after recent fetch.
The fixture uses four 65 MiB payload versions, two paths, and two commits dated
in 2001, two days apart. Its original full-clone checks were unchanged. Report:
`phase2-lfs-dated-before-b44fe67-configured-20260903/report.json` in the external
qualification workspace. An earlier attempt used an incorrect remote URL and
failed during setup; it is retained but is not product regression evidence.

**Local proof.** All 62 selected recent/fetch/prune/discovery tests pass,
including shared-ancestor window propagation, long streamed history with a
small live-state budget, truncated/missing/out-of-order graph rejection,
old independent tips, offset boundaries and cancellation. Production-library
correctness/suspicious Clippy and formatting pass; other existing warning
categories are not claimed clean. Production code grows by approximately
125 lines to own one bounded union walk instead of unchecked whole-output
filtering; the larger test module now lives in `lfs/recent/tests.rs`.
The optimized `742e60c` build subsequently passed the dated-history RustFS
driver: 44 commands, 13 checks, full/partial clones, all four current/historical
payload SHA-256 checks, Git/LFS fsck, prune dry-run and unchanged binary.
Report `phase2-lfs-dated-742e60c-20260903/report.json`; binary SHA-256
`d97cb4f3326662f97995552e2f91fda955cec16cf43f832bfc6023499cc2f09c`.
Functional-only on a concurrent development host, not a controlled-performance
or clean-release baseline. Exact-candidate CI is separate.

The preceding `b44fe67` checkpoint now has terminal success in all nine
[protocol CI jobs](https://github.com/crabbuild/crab/actions/runs/33741342130).
That evidence does not cover this newer change or the separately documented
recovery fixture failure.

**Contract sources.** [Git LFS fetch](https://github.com/git-lfs/git-lfs/blob/main/docs/man/git-lfs-fetch.adoc)
and its [fetch owner](https://github.com/git-lfs/git-lfs/blob/main/commands/command_fetch.go)
define the per-tip cutoff; the [prune owner](https://github.com/git-lfs/git-lfs/blob/main/commands/command_prune.go)
adds its offset to both enabled windows. Git 2.30's
[revision verification](https://git-scm.com/docs/git-rev-parse/2.30.0)
defines `--verify`, `--end-of-options` and commit peeling; its
[revision walk](https://git-scm.com/docs/git-rev-list/2.30.0)
defines child-before-parent ordering and stdin roots. Object-ID validation
uses the existing shared discovery contract, independent of optional gix
hash-algorithm features.

**Remaining work.** This selects commit trees, not exact previous pointer
versions removed by changes inside the window. In particular, a replaced
version can live in a parent predating the cutoff. That needs the existing
verified object-discovery owner extended to the changed previous objects,
with merge, deletion, path-filter and boundary tests. Fetch's recent remote
scope, typed default-unborn prune handling, staged-index/stash protection,
prune's unchecked alternate inventory paths and the `--force` contract remain
open. No destructive prune or complete retention/parity claim is authorized
by this evidence. Exact-candidate CI and controlled performance comparisons
remain separate acceptance gates.

### Previous versions across the recent-window boundary: 2026-09-03 UTC

**Context and failure proof.** A pointer replaced or deleted within a selected
tip's window can have its old version only in a parent outside the cutoff.
Selecting resulting commit trees loses that version. A new real-Git fetch
regression failed on `742e60c`; the optimized binary also failed the unchanged
full/partial LFS qualification extended with commits ten days apart and a
three-day history window. Current files passed; old pointer blobs stayed
missing after recent fetch. Report
`phase2-lfs-boundary-before-742e60c-20260903/report.json` retains 39 commands and
the failed `recent-fetch-resolves-history-blobs` check.

**Owner and implementation.** `cmd::lfs::fetch::collect_lfs_pointers` keeps
selected current trees separate from frozen recent commit IDs. The existing
`lfs::discovery` owner combines both into one path-preserving inventory. A
single supervised `git log --stdin --no-walk=unsorted --raw -z` produces the
old sides of selected changes. No second ancestry walk, whole-parent-tree
scan, patch-text pointer decoder, new configuration or storage format.
Raw old-object IDs feed the same bounded batch/checksum reader used by tree
and publication discovery. The common record reader now returns parser state:
the raw-change parser requires a final consumed path; the existing rev-list
parser still permits its terminal unnamed commit/tree state.

Every merge parent is compared separately. This is an explicit product rule
for preserving versions replaced by merges, not a claim that an upstream
text-log default always emits the same merge diffs. Pin merge display policy
to separate diffs, disable rename inference, relative paths, signatures,
notes, color and content transforms. A rename retains the old path; paths and
OID aliases survive until include/exclude and transfer policy run. Additions
and gitlinks have no old blob to fetch. Invalid modes/status/OIDs, inconsistent
absence markers, missing paths, invalid historical path encoding and
truncation reject the scan. Missing or corrupt old blobs and conflicting
declared sizes invalidate the whole inventory, never current-only success.

**Executable acceptance.** Run recent/fetch/prune/discovery focused tests and
the exact optimized candidate against the boundary RustFS driver. Require:

1. Replacement and deletion retain the old pointer from an outside-cutoff
   parent; disabled recent selection does not walk older changes.
2. Merge changes include each parent's previous version, but do not scan
   ancestors of unselected commits. `log.diffMerges` preferences cannot change
   this result. SHA-1 and SHA-256 Git IDs remain verified.
3. Renamed, newline/tab-named and repeated aliases preserve their old path;
   subdirectory invocation and display configuration do not narrow coverage.
4. Truncated/malformed raw records, missing/corrupt old blobs, wrong declared
   sizes and cancellation return failure before starting LFS payload transfer.
   The shared bounded batch reader still streams history larger than one batch.
5. Full clone and partial clone with initially missing pointer blobs both
   retrieve the four 65 MiB versions ten days apart, then pass SHA-256/size,
   Git/LFS fsck, dry-run prune and unchanged-binary checks. Record the candidate
   commit/hash; do not promote concurrent-host timings to performance proof.

**Sibling proof and remaining work.** Publication continues to use local-only
range discovery, not the fetch/promisor path. Pull has no recent history and
keeps its current-tree behavior. Prune currently expands its protected roots
with `rev-list --objects`, including all ancestry of HEAD and each recent
commit. Thus, when that inventory completes, outside-cutoff parents are already
included; routing it through this fetch-specific path would neither fix its
unchecked errors nor establish correct retention scope. Its canonical
fail-closed inventory, worktree indexes/stashes, selected-remote fetch scope,
flag contract and cancellation remain explicit follow-up work. No destructive
prune, full compatibility closure, production-provider or controlled-performance
claim follows from this packet.

**Local proof.** All 72 recent/fetch/prune/discovery tests pass, including the
new replacement/deletion regression, raw framing and bounded batches, all
merge parents under conflicting display preferences, renamed aliases,
SHA-256 Git history and missing/corrupt previous objects. Production-library
correctness/suspicious Clippy and formatting pass. A further 31 selected
publication, LFS-push, mirror-command and subprocess-owner tests pass, including
real Git dependency upload and local-only inspection. Existing warning classes
are not claimed clean. This adds approximately 175 production lines for the
old-object parser/selection boundary and shares the batch/checksum mechanics;
fetch tests move to a separate module without dropping prior assertions.
Optimized build, exact-binary boundary/provider proof and fresh CI remain
required at this source checkpoint.

**Dependency contracts.** Git LFS's
[previous-version scanner](https://github.com/git-lfs/git-lfs/blob/main/lfs/gitscanner_log.go)
defines old-side selection. Git 2.30 documents
[non-recursive log selection](https://git-scm.com/docs/git-log/2.30.0) and the
[raw diff framing and parent comparisons](https://git-scm.com/docs/git-diff-tree/2.30.0).
Git's [merge-option implementation](https://github.com/git/git/blob/v2.50.1/diff-merges.c)
shows why `-m` alone does not override configured merge display policy.

**Subsequent exact-binary proof.** Optimized `2411cb3` passed the boundary LFS
driver (44 commands, 13 checks) and native-Git/mirror harness (464 commands,
133 checks). Reports: `phase2-lfs-boundary-2411cb3-20260903/report.json` and
`phase2-protocol-mirror-2411cb3-20260903/artifacts/report.json`. SHA-256
`98622b53b9132f62ba50349176c854526c12b2c4efbe25b0820becc8a144177a`
was unchanged after both runs. Source/binary identity matched. These remain
functional-only concurrent-host results, not controlled performance or
clean-release proof. Exact-commit protocol CI run 33745743645 was still
running when this next packet was prepared.

### Complete bulk LFS fetch history: 2026-09-03 UTC

**Context and failure proof.** Both current `origin/main` (`e26d139`) and
`2411cb3` implement `fetch --all` using ref-tip trees. Replaced and deleted
versions disappear from the inventory unless another ref happens to retain
their tree. A new one-branch replacement/deletion regression failed before
the fix. The optimized `2411cb3` RustFS driver also planned only two of four
reachable 65 MiB payload versions. Its 34-command failure is retained in
`phase2-lfs-all-before-2411cb3-inventory-20260903/report.json`. Two earlier
driver attempts stopped at missing remote configuration and a wrong JSON
envelope lookup respectively; neither is a product-history verdict or pass.

**Design and owner.** The fetch command routes `--all` to one supervised
full-history `rev-list --objects --stdin` traversal in `lfs::discovery`.
Explicit operands resolve individually with `rev-parse --verify
--end-of-options` and object-existence peeling; bounded full IDs feed owned
stdin. With no explicit refs, Git's `--all` supplies the roots, including
tags and detached HEAD. No whole-tree-per-commit traversal or retained Git
graph. The existing record/batch/checksum owner verifies every selected
object's identity, streams large bodies and retains only a bounded pointer
inventory. Nonzero producer exits, malformed/truncated records, absent or
corrupt objects and cancellation fail rather than returning partial success.
Conflicting LFS sizes fail the shared transfer planner before any payload
downloads. Promisor reads are allowed for fetch only, preserving the caller's
transport restrictions.

The bulk parser includes unnamed objects, including pointer blobs reachable
only through blob tags. Git's optional name is a display hint: it may be
empty, newline-normalized or one of multiple aliases. It is never a path
selection authority for `--all`. CLI include/exclude and recent conflicts
remain rejected; configured path filters do not narrow bulk inventory.
Default/recent fetch and pull retain their path-preserving tree/old-side
selection. No new option, dependency, format or alternate batch parser.

**Executable acceptance.**

1. Without old-tip refs, replaced and deleted versions remain in all-ref and
   explicit-ref inventory. Explicit refs exclude unrelated branches/tags and
   detached commits; omitted refs include them. Unborn all-ref scans are empty.
2. Missing operands, revision options/ranges, newline injection and cancelled
   callers fail. SHA-1/SHA-256 history and corrupt old blobs are covered.
3. A real blobless partial clone retrieves current and historical pointers;
   the same source's publication scan fails without implicitly fetching them.
4. Parser and batch limits reject malformed, oversized and truncated records;
   repeated batches reuse memory rather than imposing a whole-history limit.
5. Run the exact optimized candidate through `qualify_all_history_lfs.py` in
   the dedicated external qualification workspace. Keep the original full
   clone/fetch/check-out/fsck harness, then qualify all-ref full clone and
   explicit-ref partial clone. With `lfs.fetchexclude=*`, dry-run JSON must
   name all four OIDs without payload writes. Real fetch must cache all four
   SHA-256/size-verified payloads; checking out both commits must reproduce
   every original file and pass Git/LFS fsck. Retain binary identity/digest.
6. Run the existing native-Git/mirror RustFS suite and exact-candidate CI.
   Concurrent-host timings cannot close controlled-performance gates.

**Proof at source checkpoint.** All 111 selected fetch, discovery, recent,
prune, publication, LFS-push, mirror-command and process-owner tests pass,
including eight new tests. Production-library correctness/suspicious Clippy,
formatting and diff checks pass; other warning categories are not claimed
clean. Existing CI selectors include the new modules.
The change adds about 81 net production lines for bulk-root selection and
record framing while reusing process ownership and verified batching.
Optimized build, fresh candidate RustFS and cross-platform CI remain required.

**Sibling gaps remain explicit.** Standalone `crab lfs push --all` also uses
tip-tree selection and needs its own local-only full-history gate. Native
publication and mirror dependency scans already traverse introduced history;
they remain local-only and their tests pass. Their path-hint/alias lock
coverage still needs separate proof. Prune's unchecked inventory, retention
scope and flag contract are unchanged. Default remote resolution in a
no-checkout clone without `crab.toml`, idle CLI stdin/caller cancellation,
configured non-bulk filters and selected-remote recent scope remain open.

**Upstream contracts.** The [Git LFS fetch manual](https://github.com/git-lfs/git-lfs/blob/main/docs/man/git-lfs-fetch.adoc)
defines complete reachable history for `--all`, including its filter rules.
The [upstream ref scanner](https://github.com/git-lfs/git-lfs/blob/main/git/rev_list_scanner.go)
uses `rev-list --objects --stdin` for selected histories and `--all` for
all-ref history. Git documents [object-name limitations](https://git-scm.com/docs/git-rev-list)
and [safe single-operand verification](https://git-scm.com/docs/git-rev-parse).

### Complete bulk LFS upload history: 2026-09-03 UTC

**Context and failure proof.** Standalone `crab lfs push --all` used ref-tip
trees, including in tagged `v1.0.1`. A one-branch replacement/deletion test
returned an empty inventory before the fix. Optimized `82bfebc` reported a
successful standalone upload while omitting an older reachable payload:
the dedicated RustFS driver failed its first historical object read after
18 commands, before any native Git or Git LFS push could fill the omission.
Retain `phase2-lfs-bulk-push-before-82bfebc-20260903/report.json`.

**Design and ownership.** Generalize the existing bulk-fetch history owner
into a direction-aware traversal. Fetch retains all-ref/promisor access;
push selects only local branches/tags by default and uses local-only access.
Explicit roots resolve individually before bounded IDs enter owned stdin.
Reuse supervised processes, record framing, checksums and batch budgets.
Remove the old tip enumeration helper; no compatibility alias or second
parser. Deduplicate upload OIDs only after validating declared sizes.
The production delta is five net lines; most additions are behavioral tests.

**Executable acceptance.**

1. Replaced/deleted versions survive both omitted-root and explicit-root
   scans without old-tip refs. Omitted roots include branch/tag history but
   exclude remote-only and detached history; explicit selection includes
   precisely the requested reachable history.
2. Unborn bulk upload is empty. Invalid/missing operands, revision options,
   ranges and newline injection fail. Pre-cancellation avoids repository
   access. SHA-1/SHA-256 corruption and conflicting LFS sizes fail closed.
3. Bulk upload from a real blobless clone fails on missing pointer blobs;
   omitted and explicit root scans leave its missing-object inventory intact.
4. Build the exact optimized candidate. Run `qualify_bulk_push_lfs.py` in the
   dedicated qualification workspace with four 65 MiB payload versions over
   two commits and two paths. Run standalone upload first, then independently
   read and SHA-256/size-check all four remote objects before native Git/LFS
   publication. Preserve the original clone/fetch/checkout/fsck checks.
5. Rerun the bulk-fetch and native-Git/mirror RustFS suites with the same
   candidate; retain identity/digests. Require exact-candidate CI separately.

**Source checkpoint.** All 116 selected LFS push/fetch/discovery/recent/prune,
publication, mirror-command and process-owner tests pass, including five new
bulk-upload tests. Production-library correctness/suspicious Clippy passes.
Optimized build, candidate live verification and cross-platform CI remain
required. These tests do not certify normal standalone push, object-ID/stdin
admission, lock alias coverage or prune safety; those owners are unchanged.

**Contract evidence.** The [Git LFS push manual](https://github.com/git-lfs/git-lfs/blob/main/docs/man/git-lfs-push.adoc)
requires full reachable history and explicitly distinguishes omitted upload
roots from bulk fetch. Upstream [`LocalRefs`](https://github.com/git-lfs/git-lfs/blob/main/git/git.go)
accepts local branches and tags, excluding remote refs and detached HEAD.
The shipped tip-only implementation is not retained as an alternate mode.

**Prior candidate proof, not this candidate.** Optimized `82bfebc` passed
bulk-fetch qualification (57 commands, 27 checks) and native-Git/mirror
qualification (464 commands, 133 checks). Reports are
`phase2-lfs-all-82bfebc-20260903/report.json` and
`phase2-protocol-mirror-82bfebc-20260903/artifacts/report.json`.
Its SHA-256 `e3852d7e573e7b8160c552f38d7f076a9d11685ed37e30d8c4bb823f2bd80cac`
remained unchanged. These are functional-only concurrent-host results.
Protocol CI 33747451500 is separate and was still in progress at this source
checkpoint. Prior run 33745743645 for `2411cb3` was cancelled after its unit
job passed, not a complete CI pass.

### Ordinary LFS upload history and selected-remote scope: 2026-09-03 UTC

**Context and failure proof.** Ordinary `crab lfs push` still inspected only
tip trees after bulk upload was fixed. This behavior also exists in tagged
`v1.0.1` and the locally available `origin/main`. The replacement/deletion
regression returned an empty inventory before this change. Optimized
`60cc9e2` returned successful standalone upload of two current versions while
omitting older reachable payloads. The unchanged ordinary-upload driver
failed its first independent historical object read after 18 commands, before
another publisher could fill the omission. Retain
`phase2-lfs-normal-before-60cc9e2-20260903/report.json`; zero acceptance checks
completed. This is a correctness defect, not a performance result.

**Design and ownership.** `cmd::lfs::push` chooses ordinary or bulk policy;
`lfs::discovery` owns both traversals through its existing bounded, verified
history scanner. Ordinary roots default to `HEAD`; an explicit named remote
excludes only its local remote-tracking history. Direct URLs and the project
default have no named tracking set: do not guess `origin`. Bulk upload retains
its local-branch/tag default and no remote exclusion; bulk fetch retains
all-ref/promisor access. Each explicit root resolves independently before
frozen IDs enter owned stdin. Both uploads remain local-only and fail closed
on missing/corrupt selected objects or conflicting pointer sizes. Delete the
ordinary tip-tree path; add no parser, configuration, format or dependency.
The two production modules grow by nine net lines to carry direction/remote
policy through the existing owner.

**Phased acceptance.**

1. Prove introduced-history selection with unchanged before/after assertions:
   replaced/deleted versions without old-tip refs, omitted/explicit roots,
   selected versus unrelated remote-tracking refs, and unrelated local branch
   exclusion. Preserve bulk scope. Invalid refs/options/ranges/newlines,
   pre-cancellation, SHA-1/SHA-256 corruption, conflicting sizes and real
   blobless-source non-hydration must still fail closed. Run push, fetch,
   discovery, recent, prune, publication, mirror and process-owner siblings.
2. Build one optimized candidate. Run unchanged `qualify_normal_push_lfs.py`
   against a fresh dedicated RustFS prefix with four 65 MiB versions over two
   commits/two paths. Independently GET and verify SHA-256/size of all four
   objects immediately after ordinary push, before any Git/Git LFS publisher.
   Preserve the original ref, clone, fetch, checkout and fsck checks. Add a
   separate selected-remote CLI probe; do not infer its wiring from unit tests.
3. Rerun bulk-upload, bulk-fetch and native Git/mirror qualification on that
   same binary; retain report and binary/driver hashes. Require exact-commit
   CI and the original provider/OS matrix separately. Measure scan CPU, memory,
   remote requests and tail latency on isolated small/large introduced ranges
   before claiming performance parity. Concurrent-host functional runs do not
   close these gates.

**Local source and functional checkpoint.** All 168 selected tests pass:
push 17, discovery 29, fetch 20, recent 15, prune 16, publication 4, mirror 65
and process owner 2. Formatting, diff checks, production-library
correctness/suspicious Clippy and optimized build pass. Other lint categories
are not claimed clean. Candidate SHA-256 is
`be2e08945b7f116786c847bf68346da385af4802d2cd5aeaaf5437c6a56b50af`;
base commit remains `625ff0c`, with the pull and LFS work still uncommitted.
The nine touched production-file hashes and driver hashes are recorded in
`phase2-candidate-be2e089-source-attestation.json` under the qualification root.

Six dedicated RustFS runs pass 259 commands / 79 checks:

| Run ID (`report.json`) | Commands / checks | Acceptance covered |
| --- | --- | --- |
| `phase2-lfs-normal-be2e089-20260903` | 35 / 8 | Ordinary upload, all four historical 65 MiB payloads independently read before another publisher |
| `phase2-lfs-selected-direct-be2e089-20260903` | 44 / 12 | Selected-remote exclusion, unrelated-remote isolation, known-history no-op, direct-URL recovery |
| `phase2-lfs-selected-default-be2e089-20260903` | 44 / 12 | Same selected-remote controls, project-default whole-history recovery |
| `phase2-lfs-selected-bulk-be2e089-20260903` | 44 / 12 | Same selected-remote controls, `--all` recovery despite tracking refs |
| `phase2-lfs-bulk_push-be2e089-20260903` | 35 / 8 | Unchanged standalone bulk-upload regression and independent historical bytes |
| `phase2-lfs-all_history-be2e089-20260903` | 57 / 27 | All-ref/full and explicit-ref/partial fetch, dry-run nonmutation, both commit versions, strict Git/LFS fsck |

The ordinary driver's SHA-256 matches its retained failing baseline:
`3d15dee9672e2d3d799e54fdc4a219110b496ae038a6b4105a0ef26de5536bac`.
Selected-remote probes deliberately install synthetic local tracking refs
without prior remote media, making exclusion and recovery independently
observable. Their initial namespace is empty; only introduced payloads appear
after ordinary upload. No-op inventory retains exact keys/ETags/sizes. Each
recovery mode fills older media, independently hash/size-verified, followed by
the original ref/clone/fetch/checkout/fsck checks. Driver SHA-256:
`1d46c3adedff6ac98077b72678c9e97a8a62f7b73840fcf1df0093d79ec64b85`.
This is not proof that synthetic tracking refs represent actual publication.

The first native Git/mirror run remains failed after 434 commands / 119
recorded checks (118 passed):
`phase2-protocol-mirror-be2e089-20260903/artifacts/report.json`. The launch
incorrectly supplied `GIT_CONFIG_GLOBAL=/dev/null`, suppressing the isolated
XDG `uploadpack.packObjectsHook` used to trigger cancellation. Neither hook
marker existed; mirror completed ordinary inspection before a signal was
sent. Read-only `git config --show-origin --get uploadpack.packObjectsHook`
on the exact fixture returned missing with that variable and the expected
hook path with it unset, matching [Git's configuration contract](https://git-scm.com/docs/git-config#Documentation/git-config.txt-GITCONFIGGLOBAL).
This is a retained launch/fixture failure, not evidence that a signalled child
survived cancellation. Failure-report SHA-256:
`96fc963ab97b00820dcb59674fd508e15b0c0ad087d404c55c7deb406bae784c`.
No production source, fixture assertion or gate was changed. A fresh namespace
with only that conflicting launch override removed passes 464 commands / 133
checks: `phase2-protocol-mirror-be2e089-xdg-20260903/artifacts/report.json`.
Real-child cancellation/cache release, native ref lifecycle, full/shallow/
partial reads, security refusals, mirror hooks and plan/apply/reapply pass.
The optional prior-release rollback executable was omitted. The seven completed
suites total 723 commands / 212 checks; the failed launch is retained separately,
not included in the pass count. Candidate binary and all nine production-source
hashes remain unchanged. The retained pull transport-assertion casing failure
still requires approval; these LFS results do not make the pull suite green.
No clean-commit CI, provider matrix, controlled-performance or full Phase 2
completion claim follows from this concurrent-host checkpoint.

**Contract boundary.** [Git LFS 3.8's push manual](https://github.com/git-lfs/git-lfs/blob/v3.8.0/docs/man/git-lfs-push.adoc)
and [range scanner](https://github.com/git-lfs/git-lfs/blob/v3.8.0/git/rev_list_scanner.go)
define selected-remote exclusions. Its [CLI](https://github.com/git-lfs/git-lfs/blob/v3.8.0/commands/command_push.go)
requires explicit refs for non-stdin ordinary push; Crab's omitted-`HEAD` and
direct-URL extensions are retained from tagged `v1.0.1`, not claimed as exact
Git LFS CLI parity. Crab does not adopt `--ignore-missing`: selected graph
failures must not produce partial upload success. Object-ID/stdin admission,
remote-resolution cancellation, alias-lock coverage, configured non-bulk
filters, selected-remote recent scope and prune safety remain separate work.
The guide names the exact history scopes. The stdin-admission checkpoint below
also aligns generated CLI `--all` help and verifies the rendered release help.

### Whole-request LFS stdin admission: 2026-09-03 UTC

**Context and reproduced failure.** Push and fetch collected stdin lines
without bounds. Push read input before rejecting conflicting flags; object-ID
normalization silently discarded malformed trailing values and accepted a
second remote operand. Empty object-ID stdin could lose its selection mode.
The unchanged release-binary probe against `be2e089` records 9 commands / 10
checks, with 8 failures: malformed requests returned success, empty input was
rejected, or conflicting flags waited for a producer-held-open pipe. The fetch
flag-conflict control passed. Retained report:
`phase2-lfs-admission-before-be2e089-python39-20260903/report.json`, SHA-256
`96516689662e34798ec0f22fc37672afc0c59f9aafab3ccfb77c110cd1d6eb5e`.
The preceding Python 3.9 driver launch lacked `hashlib.file_digest` and ran no
commands; it is not a product failure. The corrected driver uses a bounded
hash loop and is unchanged between the retained baseline and candidate.

**Design and ownership.** `cmd/lfs/input.rs` is one private push/fetch admission
owner, replacing both unbounded readers. It retains exact UTF-8 operand bytes,
accepts LF/CRLF and a final unterminated line, ignores empty lines, and rejects
control bytes. Encoded input is capped at 64 MiB, each encoded line at 1 MiB;
an independent 64 MiB logical budget charges string descriptors and operand
bytes. Reads consume at most the remaining encoded allowance plus one byte
needed to detect overflow. No partial inventory escapes on a read, encoding or
budget failure. Allocation overhead means these are not exact RSS limits.

Push's pure argument resolver rejects mode conflicts and ambiguous remotes
before input, validates every object ID, and keeps object-ID mode even when
its stdin selection is empty. Empty ordinary stdin selects nothing; explicit
`--all` retains whole local branch/tag history. Fetch uses the same framing
owner and preserves empty JSON/prune behavior, with broader selection only
when explicitly requested. No new dependency, config option or storage format.
The shared helper pays for its added code by removing two unbounded readers
and owning one input/error/budget contract. Pre-push's existing bounded
four-field ref-update grammar is distinct and remains under its Git owner;
mirror does not acquire a second reader for that same stream.

**Phased acceptance.**

1. **Admission before work — implemented, locally verified.** Context: a bad
   request must not become a partial or different upload. Exit: mixed CLI/stdin
   operands and incompatible modes reject with stdin held open; malformed final
   OIDs, invalid UTF-8/control bytes, read failures and exhausted bounds reject
   the entire list. Empty streams never default an object-ID request to HEAD.
2. **Preserved user workflow — implemented, RustFS verified.** Context: strict
   parsing must not break valid scripted publication. Exit: ref stdin, uppercase
   object-ID stdin and empty `--all` stdin independently publish four 65 MiB
   historical payloads before any other publisher. Independent GET/hash/size
   proof and exact LFS inventory pass; empty/rejected requests leave no payloads.
   Cold-clone ref and all-history stdin fetches recover all four exact objects;
   empty JSON fetch and invalid input leave an empty local media cache. Retain
   ordinary Git/LFS push, clone, fetch, checkout, fsck and ref checks afterward.
3. **Interruptible input lifecycle — open.** Context: bounded synchronous input
   is not cancellable while a producer stalls inside a read. Exit: idle pipe,
   partial line, EOF, read error and cancellation/second-signal races terminate
   every owned reader before returning on all supported OSes, with no detached
   thread or false success. Preserve transfer-agent and pre-push grammar owners;
   qualify their distinct input lifecycles rather than aliasing parsers.
4. **Release closure — open.** Context: local functional results do not certify
   the full Phase 2 contract. Exit: exact-commit CI, supported Git/OS/providers,
   managed/direct paths, complete large-history and controlled resource gates.
   Object-ID dry-run currently validates syntax but not cache availability;
   remote-resolution/transfer cancellation and output-write errors remain open.

**Current evidence.** Candidate SHA-256
`dbcdf17b6db132a106082fe101d5f2527f30e61124fc8b004f617222639001dc`,
base `625ff0c`, dirty/uncommitted. All 187 command-LFS, 29 shared discovery
and 65 mirror tests pass (281 total). Production correctness/suspicious Clippy, formatting, diff checks,
optimized build and rendered `lfs push --help` pass; 496 other Crab warnings
are not a clean lint result. The unchanged admission driver passes all 9
commands / 10 checks at `phase2-lfs-admission-dbcdf17-20260903/report.json`.
Its held-open conflict probes exit in 10–12 ms on this host, not a latency SLO.
Three fresh RustFS suites each pass 53 commands / 41 checks (159 / 123 total):
`phase2-lfs-stdin-{refs,object-ids,all}-dbcdf17-20260903/report.json`.
Admission driver SHA-256:
`df2419db2fe727ed61266c743b984d6c75c741ec8fae56070484dcc8ef84b56c`;
stdin RustFS driver SHA-256:
`3e04e9e34dcb401f3940b2fecf5246d0ed0509a7373cbf80e74b9670b6dabbf3`.
Native Git/mirror also passes 464 commands / 133 checks:
`phase2-protocol-mirror-dbcdf17-20260903/artifacts/report.json`. Its XDG hook
configuration is not suppressed by `GIT_CONFIG_GLOBAL`; real-child cancellation,
complete hook batches, plan/apply/reapply, native ref lifecycle and partial/
shallow/security gates retain the original assertions. Optional rollback binary
omitted. The four RustFS suites total 623 commands / 256 checks; separate
admission proof is 9 / 10. Binary and twelve focused production-source hashes
remain unchanged; `phase2-candidate-dbcdf17-source-attestation.json` binds them.
No clean-commit CI, controlled-performance or full provider-matrix claim.
The pull casing assertion remains unchanged and failing pending approval.

**Dependency contract.** [Git LFS 3.8 push](https://github.com/git-lfs/git-lfs/blob/v3.8.0/commands/command_push.go)
and [fetch](https://github.com/git-lfs/git-lfs/blob/v3.8.0/commands/command_fetch.go)
separate stdin from positional selections and allow empty stdin; explicit
all-history selection remains distinct. Crab retains its tagged optional
remote/default-HEAD extensions. Whole-request bounds and stricter malformed-ID
rejection are explicit local admission policy, not a claim of complete upstream
CLI parity. No production diagnostic or test expectation was changed to erase
the unrelated pending pull failure.

### Caller-owned LFS remote setup: 2026-09-03 UTC

**Context.** The canonical LFS resolver created a fresh cancellation token,
discarding the command's cancellation domain. Named-remote lookups and fetch's
remote-existence probe used unsupervised Git processes. A retained optimized
`dbcdf17` probe stalled `git remote get-url`: push, object-ID push, fetch, pull
and pre-push each ignored the first SIGINT and exceeded the three-second
deadline. The driver killed only its owned process group. Report:
`phase2-lfs-remote-cancel-before-dbcdf17-20260903/report.json`.

**Implementation.** One private `resolve_lfs_remote_context` accepts the
operation, repository root and caller token. Existing command owners pass
their token through remote selection, client acquisition and readiness setup.
Conversion uses its explicit target root. Remote-verified pruning checks
cancellation before and after verification, but does not yet interrupt the
verification await itself. One bounded, owned Git lookup serves remote URL
resolution, pre-push URL validation and fetch's remote/ref discriminator.
Cancellation and I/O failures propagate instead of becoming a missing remote.
Tagged `v1.0.1` public no-token signatures remain thin entry points to the same
resolver; there is no second routing implementation or new configuration.

The async setup boundary may be cancelled because it acquires read clients,
grants and readiness information, not ref-publication ownership. Do not apply
the same drop-on-cancel rule to mutating transfers without abort/acknowledgment
proof. Local dependency inspection found checks between managed HTTP requests
and before provider resolution, not guaranteed interruption of pending awaits.
The shared Git runner owns process-group/job teardown and joins output workers.

**Phases and acceptance.**

1. **Remote lookup ownership — implemented and qualified locally.** First
   SIGINT during each of the five lookup entry points returns cancellation,
   stops the Git child before returning, and never enters transfer discovery.
   Precancellation beats repository access and remote/ref reinterpretation.
2. **Pending network setup — implemented; qualification incomplete.** A
   stage-specific fixture must serve a valid canonical layout, stall readiness
   or grant acquisition, and verify cancellation with no transfer or mutation.
   The first HTTP probe reached layout validation rather than its intended
   manifest request. All three commands returned cancellation in 1 ms, but its
   exact-stage checks failed. Retain
   `phase2-lfs-readiness-cancel-43fd963-20260903/report.json` as **failed**;
   it is not replica-readiness or managed-grant acceptance. No assertions were
   changed to relabel it. Repair the fixture's stage setup while preserving
   the original report and intended readiness acceptance.
3. **Whole-operation ownership — open.** Carry the caller domain into owners
   still lacking it: transfer-agent sessions, locks, migration, standalone
   filter/smudge and hooks. Prove idle input, identity lookups, storage transfer,
   multipart abort acknowledgment and output failure without detached workers.
   `resolve_crab_read_layout` and conversion/prune transfer awaits remain
   explicit siblings; this patch does not certify them.
4. **Release closure — open.** Exact-commit CI, OS/provider/managed matrix and
   controlled resource measurements remain required. Never describe local
   timing as a latency SLO or reuse a predecessor binary's proof as current.

**Evidence.** Optimized candidate SHA-256
`43fd96348b9e39aa0cc16fd64a546e6ad89d3e6d6be160f16b33359cc96f2bab`,
base `625ff0c`, dirty/uncommitted. The unchanged stalled-Git driver passes
5 commands / 6 checks in
`phase2-lfs-remote-cancel-current-20260903/report.json`: all children stop
before return, cancellation takes 6–15 ms, and binary identity is unchanged.
Command-LFS tests pass 190/190; discovery 29/29; mirror 65/65. The prune
selector passes 16 tests, including eight command-option tests already counted
in command-LFS. Production correctness/suspicious Clippy passes with 495 other
Crab warnings; this is not a globally clean lint result. Three fresh RustFS
stdin suites each pass 53 commands / 41 checks using four 65 MiB payloads,
independent remote GET/hash/size checks and cold fetches:
`phase2-lfs-stdin-{refs,object-ids,all}-43fd963-20260903/report.json`.
The same binary also passes native Git/mirror RustFS qualification, 464
commands / 133 checks, at
`phase2-protocol-mirror-43fd963-20260903/artifacts/report.json`, SHA-256
`68cd6d62ff00b84905afdb82562b9e0c730b0e6ca3703b9b820fd2d0ad287a06`.
The four successful RustFS suites total 623 commands / 256 checks. The XDG
Git hook remains active; optional rollback binary is omitted. Neither the
failed readiness fixture nor Kubernetes replay is counted in these passes.
No storage format, dependency or lockfile changes. Added production surface
owns the missing cancellation boundary and replaces duplicate Git lookup
construction; it does not add fallback policy. The pending pull assertion
and broader clone/recovery fixture failures remain unchanged.

### Kubernetes push 919: journal-only payload visibility gap

**Terminal evidence, not a running replay.**
`phase2-kubernetes-published-full-625ff0c-20260903/artifacts/report.json`
is **failed**: initial import and 918 incremental pushes succeeded; push 919
failed in 7,491 ms with a missing-staged-pointer error. The pinned executable
SHA-256 is
`424d0df7a99920d1396421b86c62cf89c6a3395aee2afc6a0854496727a5a94e`.
The terminal report SHA-256 is
`8930a6edfdeec7ac1be9090d64e07649ad8441132b154327c590d58b6a0f2782`.
The final 1,000-push, clone/hydrate and performance gates did not run to
completion. The original Kubernetes checkout remains unchanged.

**Observed boundary.** Normal add had published both recovered historical
recipes. Push 918 introduced the 581,598,168-byte Docker.dmg pointer; staging
then retired that recipe. Push 919 added only `crab.toml`, so incremental
discovery found no pointers. The live staging entry for the other prepared
payload triggered a full walk of 140,658 commits. The discovered old pointer
was rejected because its retired recipe was not found through remote lookup.
Its wire hash starts `17c73fe6`; the error's `bbb7450f` prefix is the same
32 bytes formatted through Xet's four-word little-endian Merkle hash contract,
not a third payload identity. `crab-types::pointer` serializes raw bytes;
`xet-core-structures` 1.6.0 `DataHash::hex` formats little-endian words.

**Read-only origin audit.**
`phase2-kubernetes-journal-audit-625ff0c-20260903/report.json` passes four
checks using only repository-prefix LIST/GET. Compacted manifest generation
is 6. The current main head names push 918's active transaction
`1b88c516f3bd685ff0316abd024f5b447f9f8a8e779e7f585c1ff3f65db1079d`,
which contains shard
`66dedb49f9d95bebea8c24de03ae66cd0be018db2c2e393baa4cbcab677026ae`.
The ref lock is explicitly released. All 8,461 repository object keys, sizes
and ETags and the failed report are unchanged. This does not independently
prove every global xorb's bytes or a completed GC exclusion interval.

**Source evidence map.** `cmd::push` enters native discovery, then the shared
`PushPipeline`. `read_base_manifest` obtains a `RepositorySnapshot` but keeps
only `materialized_manifest`: current refs, old compacted pack/shard roots.
`lookup_origin_file_index_batch` searches that old shard root, not
`snapshot.journal.shards`. `commit_ref_journal` correctly makes uploaded
shards visible through its active transaction before staging retirement.
The mismatch also exists in the inspected `origin/main`; it is not introduced
by the LFS setup changes. The snapshot-backed `FileIndexLookupSession` and
post-fetch shard synchronization already consume journal shards. Existing
push tests cover compacted-manifest recovery and stale/corrupt/cache-only
rejection, but do not prove this sequential post-retirement journal case.

**Executable repair plan; not implemented by this checkpoint.**

1. **Pin complete push-base ownership.** Preserve one captured base containing
   the compacted manifest, CAS token and committed journal inventory/digest.
   Derive ref decisions and payload proof from that same capture. Exit:
   journal-only shard lookup succeeds with retired staging, before compaction;
   uncommitted/cache-only/corrupt/missing payloads still reject. Do not create
   synthetic persisted roots or accept a freshly read unrelated snapshot as
   proof for an older plan. Reuse canonical snapshot lookup where it meets
   origin/GC/cache contracts instead of adding another recovery search path.
2. **Revalidate every shared consumer.** Cover under-lock refresh, CAS retry,
   remote-only records, candidate index/receipt construction, same-repository
   deduplication and manifest publication. Exit: journal changes at an unchanged
   manifest ETag invalidate stale plans; compaction and concurrent sibling refs
   retain all committed packs/shards; ref rejection cannot publish sibling
   dependencies. Prove native CLI, remote helper and mirror callers; preserve
   protected/active-active service ownership and existing corruption tests.
3. **Focused real-store regression, then full replay.** In a fresh namespace,
   perform add/commit/push, confirm retirement, commit an ordinary file, push
   again before maintenance compacts the journal, then cold clone/hydrate and
   compare original bytes. Include an unrelated staged payload to exercise the
   observed full-walk branch. Exit: exact refs, byte identity, released locks,
   no restaging requirement, then all 1,000 original-history pushes and final
   cold-client gates. Preserve the failed namespace; no repair/retry has run.
4. **Scale and release qualification.** Measure journal inventory reads and
   bounded memory/requests; examine the zero-pointer full-walk trigger separately
   after durability is fixed. A staged unrelated file does not itself prove a
   stale remote frontier. Exit: controlled before/after large-history results,
   broad exact-commit CI and required provider/OS gates. Passing a short repro
   or forcing compaction before the second push is not completion.

### Journal-aware push repair: local implementation checkpoint

The working tree now retains the complete `RepositorySnapshot` in
`crab/src/git/push.rs`. Ref decisions, remote-only file lookup, same-repository
shard lookup and incremental pack inventory derive from that capture.
Candidate indexes union all committed journal packs/shards with newly uploaded
dependencies; membership sets avoid repeated pairwise inventory comparisons.
Under-lock and publication-time refresh compare complete snapshots, not only
the compacted manifest ETag. The private dependency plan binds the snapshot
digest and invalidates receipts when that capture changes. This is not a new
persisted receipt format or continuous publication fencing.

`crates/crab-metadata/src/file_index_lookup.rs` owns the captured-snapshot
acceleration opener. Push's duplicate compacted-only search was removed.
Scoped lookup remains write-free; unscoped SlateDB readers close after lookup,
including errors. SlateDB 0.15.0's managed reader creates checkpoints and its
`close` shuts down the reader task. Acceleration hits still require exact
captured shard membership, then push independently verifies the origin shard,
recipe and xorbs. Existing cache-only and corrupt-origin rejections remain.

**Proof so far.** The new retired-staging/journal-only regression failed with
`PointerMissingStaging` before the source change and passes afterward. Twelve
metadata lookup tests pass with only `file-index-reader` enabled, including
captured lookup after a newer manifest and scoped no-write access. A focused
push selection passes 22 tests and fails one newly added fixture: its sole
committed sibling ref leaves HEAD pointing to absent `main`, so canonical
journal validation correctly rejects the fixture before the intended refresh
assertion. The existing under-lock refresh, ref-lease, candidate inventory,
non-atomic dependency replan and missing/corrupt/cache-only tests pass.
The failing fixture has not been weakened or relabeled.
The minimal no-default-features metadata check also passes; existing unused
type warnings remain. Formatting and whitespace checks pass.

**Real-store gate still open.**
`phase2-journal-reuse-baseline-43fd963-20260903/artifacts/report.json` is failed,
not product qualification. Its new harness checks the wrong journal prefix,
`ref-journal/active`; the storage contract and recorded object listing use
`refs/journal/active`. The run stops before its second payload-reuse push.
Keep that failed report. Correcting these newly authored fixtures and the
previous Git diagnostic casing assertion requires user approval under the
repository's test-edit restriction. Then run a fresh baseline/candidate pair,
confirm staging retirement without intervening compaction, and cold-clone and
hydrate with byte identity. Do not replace the Kubernetes failure with these
unit results or count this fixture failure as a reproduced product failure.

The external release binary remains `43fd963`; no new optimized candidate or
1,000-history replay has started at this checkpoint. PR #148 remains draft at
`625ff0c`; these source changes are not pushed. Native remote-helper/mirror
caller proof, full replay, controlled performance and required provider/OS CI
remain open. Cross-repository committed chunk receipt acceleration still uses
compacted source anchors; its journal fast path needs separate proof, while
full origin verification remains the correctness path. Protected push keeps
service-owned base state; active-active still requires its own qualification.

### Snapshot refresh without redundant immutable repacking

The existing `cas_retry_reuses_dependencies_when_only_unrelated_ref_advanced`
test exposed an implementation regression: the first journal repair rebuilt
all dependencies whenever any snapshot field changed. The unchanged test
observed one dependency replan instead of zero. This was fixed in production,
not by changing its assertion.

`snapshot_retains_dependencies` now permits reuse only when the validated
layout is unchanged and every prior committed shard and complete pack metadata
entry remains in the refreshed inventory. Requested-ref conflicts still force
the existing subset replan. With retained dependencies, push verifies current
pack origin, rechecks remote-only file recipes/bytes, registers the candidate
shard union, refreshes local-placement origin proof, and rebuilds the receipt
against the new snapshot. A removed shard/pack, changed pack metadata or layout
does not qualify for reuse. Same ref OIDs or the same manifest ETag alone are
not sufficient. This preserves the existing immutable-upload reuse contract
without reusing a stale snapshot-bound receipt.

Twenty-two focused current-source tests pass, including the unchanged
unrelated-ref reuse regression, conflicting-ref subset publication, active
marker acknowledgement, compaction/locator handoff, journal-only staging
lookup and corruption/cache-only rejection. A new table-driven policy test
covers identical/additive/ref-only/compacted inventories and removed or
changed dependency identities. Sixty-five mirror tests and fourteen
protected/active-active tests pass; one DynamoDB-local test remains explicitly
ignored and is not cloud-provider proof. The separate new under-lock fixture
still fails for its invalid HEAD and remains unchanged pending approval.

The protocol workflow now schedules these push regressions and the shared
file-index lookup tests. YAML parsing, formatting and whitespace checks pass.
This is workflow wiring, not evidence of a completed CI run. The optimized
candidate is rebuilt for unchanged native-Git/mirror RustFS qualification;
its terminal result must be recorded separately before claiming that gate.

### Journal candidate RustFS result and fresh Kubernetes replay

Optimized binary SHA-256
`771e8bee09211480f7d46b628546547f4ec57f3087af877c9c0276d0fcd0578a`
passes the unchanged native Git/mirror RustFS harness: **464 commands / 133
checks**, no failed checks, macOS / Apple Git 2.50.1 / direct mode. Report:
`phase2-protocol-mirror-journal-771e8be-20260903/artifacts/report.json`, SHA-256
`e65c192ad6d3370b33e6482aa557e74b5694e51548fd72c892c4571c64e621c0`.
The executable and changed journal source hashes match before/after the run.
No rollback binary was supplied; this is not a production-provider, cross-OS
or controlled performance result. The metadata lookup suite also passes all
twelve tests again with only `file-index-reader` enabled.
The product Clippy correctness/suspicious gate exits successfully; 490 other
product warnings remain, so this is not a warnings-free lint claim. Capability
matrix verification and workflow YAML parsing also pass.

The full original Kubernetes qualification is running separately as
`phase2-kubernetes-journal-771e8be-20260903`, with a run-owned pinned copy of
the same executable and a fresh isolated remote prefix. It requests all
1,000 original first-parent pushes and the existing final clone/correctness
gates. Preflight confirms source HEAD
`4675851bd198493d2fcd371cf493594ab1933f23`, replay base
`338e80805f4034fafb9c7344b151b719f9171fc5`, and a clean source checkout.
Both original recovered payloads are published through normal add, the owned
index is restored, and preparation checks pass before replay. No old failure
namespace is repaired or retried. The task-specific driver explicitly marks
this functional qualification invalid for performance comparison.

The small dedicated journal-reuse fixture remains blocked on its path
correction; the larger unchanged real-history workload proceeds after the
focused unit and native/mirror gates. This does not relabel the failed fixture
or remove its acceptance requirement. Neither a running replay nor successful
preparation is a large-repository pass. Retain its terminal result and run the
unchanged full report verifier before closing any original-history gate.
The source/binary/report attestation is
`phase2-candidate-journal-771e8be-source-attestation.json`; its running-replay
field is a checkpoint, not final evidence. No source changes have been pushed.

### Host merge enforcement audit: 2026-09-03 16:54 UTC

Read-only GitHub queries for `crabbuild/crab` / `main` return an empty effective
rules list and HTTP 404 `Branch not protected` from the legacy protection
endpoint. Evidence: `phase2-host-merge-gate-audit-20260903T1654.json` in the
external qualification root. No repository settings were changed, and no
candidate merge was attempted. The existing RustFS mirror-CI failure check
proves command behavior, not host enforcement.

The Phase 2 host gate remains open: authorize/configure the candidate-bound
required check and branch protection, then prove a missing-data candidate is
blocked while a valid candidate can satisfy it. Preserve exact candidate OIDs,
check conclusions and mergeability evidence. Enabling branch protection is a
repository policy change requiring explicit approval; do not infer permission
from this implementation or PR request. This audit covers only the named branch,
not every collaboration host or managed deployment.

### Original Kubernetes replay remains incomplete: 2026-09-03 UTC

The pinned `56d336a` frozen-preparation replay reached push 918 and failed
closed with `CRAB-E0086`: a historical 581,598,168-byte pointer had no
publishable local recipe or remote payload. This is not a 1,000-push pass.
Follow-up inspection disproved the initial cleanup hypothesis: the closed
staging database was byte-identical to the preparation snapshot, with both
files and 18,080 chunk occurrences retained. Both batches remained open,
with no canonical path heads. Preparation used `crab add --skip-git-add`,
which intentionally does not publish the recipe ownership required by push.
Do not weaken `published_recipe_for_file` admission to accept open batches.
Retain `phase2-kubernetes-frozen-prepared-56d336a-20260903/artifacts/report.json`
and the push-918 error log. Original external source remains read-only.

The focused `82bfebc` probe instead uses normal `crab add` on verified
recovered payloads, validates published batch/path-head joins, then restores
only the disposable writer's original Git index. The historical push now
passes, uploading 1,646,511 Git objects; a fresh lazy Crab clone and strict
Git fsck also pass. Hydration still fails, so the probe is **failed**, not a
replacement for the full replay. Retain
`phase2-kubernetes-published-probe-82bfebc-20260903/artifacts/report.json`
(34 commands, 22 recorded checks).

The clone has a valid Git `remote.origin.url` but no committed `crab.toml`.
`Config::resolve_local` does not derive its remote from Git config;
`main::resolve_hydrate_remote_url` consequently returns none. CLI hydration
selects `SmudgeSessionHydrator`, whose default resolvers are no-ops, instead
of the cloud-backed `ShardHydrator`. The resulting file-index miss is not
proof that the uploaded payload is absent. Qualify remote selection across
explicit hydrate, eager clone, filter-process, pull and profile hydration
before changing ownership. Both historical payloads must hydrate to their
original bytes before restarting the long replay. No history substitution,
success-marker relaxation or source-checkout edits.

### Restore clone's canonical remote context: 2026-09-03 UTC

**Context and observed failure.** A historical revision can contain Crab
pointers without `crab.toml`. Clone's checkout child receives a temporary
remote override, but later hydrate does not. The `82bfebc` historical probe
passed push, clone and Git fsck, then failed normal hydration. A separate
diagnostic with only the existing clone-time override reconstructed
581,598,168 bytes, matching original SHA-256
`94102d4fe056bf3a4fde375d693aae96a429157dad0345af9853d7157d6bd5bd`.
The failed report remains failed; `remote-selection-diagnostic.md` records
the manual follow-up. The clone implementation is unchanged between that
candidate and `123884c`, and tagged `v1.0.1` documents clone as replacing the
manual clone/init/hydrate sequence. This is a missing initialization step,
not evidence that the uploaded historical bytes disappeared.

**Design.** Keep `crab.toml` as the existing project remote authority. After
checkout, before optional hydration, create a minimal file from the explicit
resolved clone URL only if the root project file is absent. Existing policy
is parsed but remains byte-identical; no replacement on parse failure. The
new file is deliberately untracked: no index edits or automatic commit.
Use the existing project serializer and a same-directory temporary file,
synchronize its contents, then persist without overwriting a raced-in file.
Preserve underlying persistence errors. No new config key, local remote file,
Git-remote guessing chain, dependency, persistent schema or environment knob.
Before checkout, policy comes only from the cloned revision. Remove the
ancestor-search fallback that could inherit an unrelated enclosing project's
eager hydration policy. Its new isolation regression failed before removal.

**Executable acceptance.**

1. Cloning a revision without project config followed by a separate ordinary
   hydrate command reconstructs exact original bytes without remote overrides.
   Include both historical Kubernetes payloads, cold clones, ref comparisons
   and strict Git fsck before restarting the 1,000-commit replay.
2. Existing project policy is byte-identical, including when it declares a
   primary distinct from the clone location. Invalid version/TOML and dangling
   symlinks fail without replacing the file or writing its target.
3. An enclosing repository's policy is not selected before checkout or when
   creating the new file.
   The new project URL resolves through ordinary configuration; the Git index
   remains byte-identical. Ordinary Git/local-path clones remain untouched.
4. Qualify lazy-plus-explicit-hydrate, eager clone and selective clone with
   real storage. Preserve committed-config behavior and managed canonical
   identity; never persist resolved placement or credentials. Existing
   unconfigured checkouts still use `crab configure <REMOTE>`.
5. Run exact-binary native-Git/mirror and LFS regressions; retain candidate
   identity and cross-OS execution. New project-remote unit cases are selected
   explicitly by the existing Linux/macOS/Windows protocol workflow.

**Source checkpoint.** All sixteen focused clone/configuration tests pass,
including six new cases. The pre-checkout ancestor-policy regression failed
before the fallback was removed and passes after removal.
The larger clone/configuration selection is **41 passed, 1 failed**, not
green: `clone_shard_sync_uses_selected_replica_store` builds two manifests
without their canonical layout descriptors. That unchanged fixture fails
at snapshot admission before its replica assertions; the stricter requirement
originated earlier in this PR. Approval to initialize valid fixture layouts,
with assertions and product validation unchanged, is pending alongside the
existing history-recovery fixture request. No test or expected-failure gate
was removed or weakened. Candidate build/live proof and final checks remain
required. The approximately 30 net production lines establish missing durable configuration
at clone's ownership boundary; they do not add another read-resolution path.

**Sibling work remains.** Explicit hydrate and eager/selective clone already
have real `ShardHydrator` composition once configuration exists. Pull and
always-profile hydration still call `run_hydrate`, whose default SmudgeSession
has no cloud dependencies; profile also resolves configuration from process
cwd instead of its target root. Consolidate those callers into the canonical
hydration owner with explicit roots, cancellation, restore policy and
fail-closed remote selection. Preserve unpublished local-staging hydration
and verify managed/replica behavior. This initialization change alone does not
certify those paths or native Git clone without Crab setup.

**Dependency contract.** [`tempfile` 3.27.0 `persist_noclobber`](https://docs.rs/tempfile/3.27.0/tempfile/struct.NamedTempFile.html#method.persist_noclobber)
never replaces an existing target and preserves the underlying failure. Its
cross-platform contract can leave an extra temporary hard link after a crash;
do not claim universal atomic cleanup or directory durability from it.

**Prior packet, not this candidate.** `123884c` passed 116 focused LFS tests,
optimized build, standalone bulk-upload RustFS (35 commands, 8 checks), bulk
fetch (57 commands, 27 checks) and native Git/mirror (464 commands, 133 checks).
Binary SHA-256 remained
`807126c9c84c1188423b1a1a951468936ecfa6d0ec2aab82d243b3df5ad99db5`.
Protocol CI 33749651295 was still running at this checkpoint; predecessor
33747451500 was superseded with eight jobs passed and Windows cancelled.
Neither narrow local proof nor a cancelled workflow closes the full matrix.

### Share cloud hydration composition across callers: 2026-09-03 UTC

**Context.** The optimized `8456eeb` RustFS caller probe reproduces two
failures: an `always` profile clone succeeds but leaves its selected payload
as a pointer; deferred post-pull hydration advances to the correct Git commit,
then exits 9 with a 121-byte pointer instead of the original 4 MiB payload.
Retain `phase2-hydration-callers-before-8456eeb-isolated-20260903/artifacts/report.json`.
Earlier probe attempts failed in the driver at CLI argument admission or Git
filter configuration, before post-pull hydration. They remain failed reports,
not additional product regressions. The corrected probe disables only the
run-owned Git process filter and uses identity clean/smudge commands, preserving
the tracked pointer bytes so it tests the post-pull owner independently of
filter-process. Git's [filter contract](https://git-scm.com/docs/gitattributes#_filter)
distinguishes process filtering from single-file clean/smudge drivers.

**Design and owner.** Move the existing CLI cloud composition into
`cmd::hydrate::run_hydrate`: explicit root, resolved configuration, existing
restore flags and caller cancellation. Select the existing managed/replica-aware
store, build one `ShardHydrator`, and delegate to `run_hydrate_in`. Preserve the
bulk full-xorb cache policy and restore overrides. Only an absent remote uses
local unpublished staging; malformed remote or returned selection errors must
not fall through to it. Clone eager/selective/profile, pull and init auto-patterns
call this owner. Profile configuration comes from its target root; a configuration
error skips optional hydration with a warning, not silently default policy.
The filter-process parser shares the moved remote parser; its incremental read,
decoded cache, prefetch and failure behavior remain unchanged. No new environment
setting, dependency, storage format, reader implementation or automatic remote
rewrite. The existing uncalled low-level local chunk-cache API is not promoted
as a cloud reader or removed as unrelated cleanup.

**Phased execution and acceptance.**

1. Reproduce both caller failures with the old optimized binary, separate
   empty clone caches, exact source refs and independent SHA-256 payload checks.
   Preserve failed reports and driver corrections. This gate is complete above.
2. Consolidate composition; prove cancellation before root/provider access,
   invalid configured remote rejection, and byte-identical unpublished staging
   reconstruction at an explicit root. Preserve existing clone/config, restore,
   hydration and pull selection tests. No fixture or expected-failure weakening.
3. Build an exact optimized candidate, rerun the same RustFS caller probe, and
   require both original payload hashes plus strict Git fsck. Verify ordinary
   explicit hydrate and eager/selective clone as siblings. Candidate live proof
   is required; unit wiring alone does not close this phase.
4. Rerun native Git/mirror and LFS RustFS regressions, retain binary identity,
   and require fresh native Linux/macOS/Windows workflow results. The new owner
   tests are included in each existing protocol workflow platform selection.
5. Separately qualify managed grants, forced-replica failure semantics, archive
   restore and cancellation during provider construction. The existing store
   resolver already handles managed identity; do not introduce a second resolver.
   Init's warning/auto-pattern policy, pull subprocess ownership/path admission,
   profile cancellation/output semantics and filter-process failure policy still
   need their broader lifecycle gates. This patch does not certify them.

**Source checkpoint.** All 94 selected tests pass: 78 hydration (including three
new owner tests), five restore, three clone profile-skip, four pull-policy and
four binary remote-parser cases. Production-library correctness/suspicious
Clippy, formatting, diff checks and workflow-YAML parsing pass. Other existing
lint warning categories are not claimed clean. The refactor removes about as
many production lines as it adds; the new owner replaces duplicated composition
rather than introducing another reader. Optimized build, fresh caller live
proof and exact-candidate CI remain required at this source checkpoint.

**Prior clone packet evidence.** Optimized `8456eeb` passed native Git/mirror
RustFS (464 commands, 133 checks) and bulk LFS (57 commands, 27 checks), with
SHA-256 `994fef73dac80c222dc6d426915d06f8f8c19d647f9557b8ea60c1ff0104c3b6`.
The historical clone-mode probe passed three ordinary pushes, the first payload
push, cold lazy/explicit and eager clones, strict Git fsck, unchanged Git index,
and original 581,598,168-byte payload identity. Its final status is **failed**
(50 commands, 37 checks): it incorrectly assumed the second revision also
lacked project configuration. That revision commits `crab://crab/k8s` as its
primary. Do not overwrite it merely because the clone transport differs.
Retain `phase2-historical-clone-modes-8456eeb-20260903/artifacts/report.json`.
Continue second-version proof with lazy clone, verify committed policy unchanged,
then explicitly configure the disposable checkout to the isolated RustFS remote
before hydration. Configuration intentionally stages its project changes;
do not conflate this with clone's no-index-change contract. Original source and
history remain untouched. Exact `8456eeb` CI is run 33751971094; predecessor
33749651295 was superseded/cancelled, not a full pass. No controlled-performance
or complete 1,000-push gate is closed by these functional results.

### Pull Git-phase ownership and diagnostic parity

**Context and reproduced behavior.** On optimized `625ff0c`, a real local-Git
merge conflict leaves `file.txt` unmerged, but both text and `--json` modes
return exit 5 / `CRAB-E0070` instead of `CRAB-E0130`. Text mode loses the Git
diagnostic; JSON mode retains the fetch message but emits plain stderr instead
of a JSON terminal result. The retained neutral-path baseline is
`phase2-pull-classification-neutral-625ff0c-20260903/artifacts/report.json`:
73 commands, 22 checks, two failed classification checks. Both merge
and rebase no-op cases with a deliberately stale `ORIG_HEAD` pass on this Git
version; do not describe stale-path selection as a reproduced product failure.
An earlier attempt stopped on an overlong driver log filename before invoking
Crab pull; that failed report is not product-regression evidence. The intervening
`phase2-pull-classification-baseline-625ff0c-20260903` run incorrectly appeared
to pass JSON classification because its remote directory contained `conflict`.
The current substring classifier matched that unrelated fetch diagnostic.
Neutral transport names remove this accidental positive; retain both reports.

**Baseline owner and evidence map.** `crab/src/main.rs` dispatches into
`crab/src/cmd/pull.rs::run_pull`. At `625ff0c`,
`execute_git_pull` blocks in `Command::output`, captures stdout, and inherits
stderr only in text mode. Classification reads only stderr, ignoring the
actual merge-conflict messages captured on stdout. Text mode has no captured
stderr; machine mode can misclassify unrelated diagnostic path text.
`diff_since` separately ignores snapshot/diff failures and consults
`ORIG_HEAD`; candidate selection uses process-relative paths and turns literal
paths into hydrate patterns. The shared cloud hydration owner is now wired,
but does not repair these Git-phase contracts. `crab/src/git/process.rs` owns
cancellation, bounded output, process groups/jobs and joined pipe workers for
mirror and LFS callers. Reuse that owner; do not add another child-cleanup
implementation. Its existing callers are the required sibling regression set.

**Design.** Resolve the worktree root once. Run the blocking Git phase in
`spawn_blocking` with the caller's cancellation token. Extend
the shared process owner's stderr consumption only as necessary to support
bounded capture plus live terminal progress; retain its default behavior for
mirror/LFS. Capture checked before/after commit snapshots. A proven unborn
HEAD permits initial-tree enumeration; command failure is not an unborn HEAD.
Equal snapshots produce no changed paths. Otherwise enumerate the exact pair;
do not substitute an older `ORIG_HEAD` or silently ignore enumeration failure.
Determine conflicts from the unmerged index, not localized progress phrases.
Hydration receives root-relative literal paths without lossy decoding or glob
expansion. Retain provider failures and cancellation, and report Git integration
separately when it succeeded before hydration failed. One terminal outcome
must use the selected text/JSON/JSONL boundary; subprocess stdout must not leak
into the machine stream.

**Phases and acceptance.**

1. Preserve the baseline above. Add real-Git failing tests for text/JSON
   conflict parity, failed transport diagnostics, explicit-root/subdirectory
   execution, malformed enumeration and literal metacharacter filenames.
   Keep merge/rebase no-op cases as positive controls. Test an unborn branch
   separately from corrupt/missing commit objects; no fallback-based success.
2. Integrate the existing process owner. Require bounded stdout/stderr,
   visible text progress, typed cancellation, no acknowledgement on failure,
   and reaped children/descendants with joined readers. Mirror and LFS process
   tests must retain their previous behavior on Linux, macOS and Windows.
3. Replace snapshot/path admission and terminal output together. Verify exact
   changed-path sets for fast-forward, merge, rebase, rename, deletion and
   initial pull. Exercise default pathspec and non-default selector features;
   unrepresentable paths must fail explicitly, never hydrate a different file.
   No-hydrate controls only Crab's post-pull phase: native Git filters can
   still materialize files during checkout. Documentation must reflect that.
4. Rerun the unchanged baseline against an exact optimized candidate, then
   deferred-hydration RustFS with cold caches, independently hashed payloads,
   strict Git fsck and unchanged candidate identity. Inject cancellation during
   Git and hydration, transport failure, malformed output and output-sink
   failure. Publish fresh CI results; unit or local-Git proof alone is not
   provider, lifecycle or full compatibility qualification.

**Dependency contracts.** Git documents NUL-delimited path output and the
unmerged `U` selection in [`git diff`](https://git-scm.com/docs/git-diff).
[`git rev-parse`](https://git-scm.com/docs/git-rev-parse) describes `ORIG_HEAD`
as state recorded by several history-changing commands, not a transaction
receipt for this Crab invocation. Preserve the supported Git-version floor
when choosing snapshot and existence probes. The shared process implementation
and both selector dependencies must be read and tested before extending their
contracts. [`git show-ref`](https://git-scm.com/docs/git-show-ref) documents
exact `--verify --quiet` lookup; use that supported probe rather than newer
existence options. `diff --cached --diff-filter=U -z` inspects the unmerged
index directly without depending on worktree contents or localized messages.

**Local implementation checkpoint, not complete acceptance.** The Git phase
now lives in `crab/src/cmd/pull/git.rs`. It uses the shared subprocess owner
with bounded stderr capture/terminal tee, checked commit snapshots, proven
unborn admission and strict NUL path inventory. An operation-local child
cancellation token also signals the blocking worker if its async caller is
dropped; normal awaited cancellation joins the process/pipe workers. Dropping
an async task is not itself proof that those workers have already joined.
`run_pull` resolves the explicit worktree root for candidate inspection and
propagates inspection failures. `Cmd::output_mode` now includes pull, allowing
its failures to use the existing JSON/JSONL error boundary. This does not yet
provide a unified pull success/hydration outcome.

The old command/snapshot/conflict helpers are removed, not retained as another
path. Existing mirror/LFS callers retain their default stderr reader. Local
sibling tests pass: 12 mirror process, two platform cleanup, 29 discovery and
15 recent-history tests. The pull suite still has a known new-test assertion
failure: its transport expectation uses lower-case `could`, while native Git
returns `Could`. Production preserves the original diagnostic; no lowercasing
or validation bypass was added to make the assertion pass. The CLI mode/schema
test passes. The current pull selection is **23 passed, one failed**; the
stalled-hook fixture entry is a subprocess helper, not an independent product
acceptance scenario. Production-library correctness/suspicious Clippy, formatting,
diff checks and workflow YAML parsing pass; other warning categories are not
claimed clean. The bounded subprocess extension and stricter admission add
roughly 95 production lines after deleting the old Git-phase helpers; new
tests are separate. This added code owns checked failure and cancellation
behavior rather than a second command or reconstruction path.

The optimized **uncommitted** candidate built successfully, SHA-256
`e80061ec30d10683e6ea751b7ad24e987f4d5ad778093d00cbe1cf1fe71018cf`.
Three separate probes pass with that identity unchanged:

- `phase2-pull-classification-local-625ff0c-dirty-20260903`: 73 commands,
  22 checks. Driver SHA-256 matches the retained failed baseline; both former
  conflict-classification failures pass, with the no-op controls unchanged.
- `phase2-pull-machine-local-625ff0c-dirty-20260903`: 85 commands, 39 checks.
  JSON and JSONL each emit one exact typed conflict/transport error, correct
  exit codes and original Git diagnostic casing. Subdirectory execution and
  inherited wrong-repository Git environment variables target the intended
  client. The report also records hashes of the dirty production source files.
- `phase2-hydration-callers-local-625ff0c-dirty-20260903`: 44 commands,
  24 checks. Cold profile-driven clone and deferred post-pull hydration
  reconstruct independently hashed 4 MiB originals; exact refs and strict
  Git fsck pass. Native smudge is deliberately disabled in the deferred case
  to exercise Crab's post-pull owner.

All reports are under their run's `artifacts/report.json`. These functional
developer-host results do not supersede the failing test or certify a clean
commit, full lifecycle, native-platform CI or performance baseline. The
concurrent 1,000-push replay still uses its separate unchanged `625ff0c` binary
copy; rebuilding the candidate did not replace that run's executable.

At this Git-phase checkpoint, literal candidate paths still enter hydrate's
pattern selector; the following packet addresses that boundary. Do not mark
literal metacharacter selection, missing-pointer admission, descriptor-race
protection, output-sink cancellation or the combined hydration/reporting
lifecycle complete. The local Git-phase change is bounded follow-up work, not
closure of this design's four phases or the original Phase 2 gates.

### Literal Git hydration inventory and cache-key identity

**Context and reproduction.** The optimized Git-phase-only candidate
`e80061ec30d10683e6ea751b7ad24e987f4d5ad778093d00cbe1cf1fe71018cf`
still expanded the post-pull inventory. In a fresh RustFS repository, only
`data[1].bin` changed; its sibling `data1.bin` remained at the original commit's
pointer. Pull succeeded and reconstructed the changed file correctly, but also
hydrated the unselected sibling. The retained failure is
`phase2-pull-literal-baseline-e80061e-20260903/artifacts/report.json`:
29 commands, 23 checks, failed `unselected-decoy-remains-pointer`. This is
over-selection, not a failure to reconstruct the selected original. The prior
12-command attempt used an unsupported `crab add --all` invocation and stopped
before pull; it is a driver failure, not product evidence.

**Design and owner boundaries.** Keep Git selection typed as exact absolute
paths paired with parsed pointer identities. The pointer reader returns its
parsed identity while retaining the existing boolean query for its other
callers; reads remain stack-bounded and handle short/interrupted reads through
EOF. Pull passes the typed inventory to `hydrate_selected`, not `HydrateArgs`
patterns. `configured_hydrator` remains the sole provider/restore/local-staging
composition. `run_selected_hydration` owns the existing progress and reporting;
`hydrate/execute.rs` owns shared recovery, verified CoW, reconstruction,
verified cache publication and index refresh. Ordinary hydrate's CLI selection
and clone/profile callers feed the same execution/reporting path. No alternate
reconstructor, provider resolver, configuration flag or storage layout is added.

**Size review.** Across the local Git-phase and literal-inventory changes,
production sections grow by approximately 114 lines including comments and
whitespace, with tests counted separately. The new owners replace the old
pull subprocess/pattern wrapper and move shared hydration execution rather
than copying it. The additional surface carries checked snapshots, bounded
Git diagnostics, explicit-root selection and one typed hydration entry point;
there is still one provider composition and one reconstruction/reporting path.

Execution errors are captured before joining reporters, so an early recovery
or CoW error cannot bypass the normal reporter cleanup. This does not prove
cleanup after abrupt task/process death or blocked output sinks. Pull's text
completion uses the actual verified hydrated count, not candidate count.
The shared reporter also rejects a declared byte total that cannot fit `u64`,
before creating progress tasks or starting reconstruction. Its new regression
panicked on the former unchecked sum and returns `InvalidData` after the fix.
Pull's duplicate byte summation and formatter are removed; byte accounting
stays with the shared hydration owner. This is a numeric admission check, not
the still-required total-memory/inventory budget.

**Shipped reporting contract.** `git show v1.0.1:crab/src/cmd/pull.rs` and its
called hydrate implementation establish the released `hydrate` /
`hydrate.event` result stream during post-pull hydration. This packet reuses
that reporter rather than silently changing the command's emitted schema.
The proposed single pull-level success/partial-failure envelope still needs
an explicit published transition contract and full caller proof; retaining
the released stream is not completion of that broader design.

**Cache identity.** A second regression reproduced conversion of a literal
Unix `data\1.bin` cache key to `data/1.bin`. The failing test observed the
wrong row before the production fix. Shared publication now retains literal
Unix backslashes, normalizes separators only on Windows, and declines an
advisory cache entry when its path is outside the root or is not representable
as UTF-8. It does not write a lossy alias. Rust's
[`Path` contract](https://doc.rust-lang.org/std/path/struct.Path.html#method.to_str)
distinguishes checked Unicode conversion from replacement-character decoding
and documents platform-specific separators. No existing cache is deleted or
migrated; advisory candidates still require byte/stat verification.

**Phases and acceptance.**

1. Retain the failing RustFS probe and cache-key regression. The former must
   pass unchanged with an optimized candidate: only the intended file is
   materialized, the sibling remains byte-identical to its pointer, HEAD and
   strict Git fsck pass, and candidate/driver identities are recorded.
2. Require exact inventory and cache-key tests, precancellation, reporter
   cleanup, and the existing ordinary-hydrate/recovery/CoW/index-proof cases.
   Check the boolean pointer-reader siblings, particularly dehydrate and
   batch hydration. Keep every prior failure visible; no assertion bypasses.
3. Exercise both path-matching feature configurations plus native Windows,
   macOS and Linux CI. Re-run profile clone and deferred post-pull hydration
   against cold RustFS caches, independently checking payload bytes. The
   workflow now selects the complete hydration test module and pointer tests.
4. Continue the original gates: missing-file/index admission, metadata and
   ancestor/descriptor races, output-sink lifecycle, strict manifest literal
   handling, bounded total inventory, full provider matrix and controlled
   performance. Typed pull selection alone does not close those sibling gaps.

**Source checkpoint.** Local implementation compiles. All 83 hydration tests
pass after the cache-key fix, including the unchanged regression that failed
before it. All five new selected-hydration tests also pass without
`gix-pathmatch` (features `simd-accel,tier,watch,nfs,gix-transport`). The shared
pointer-reader selection passes ten tests, dehydrate 42, and batch policy six.
Production-library correctness/suspicious Clippy, formatting, diff checks and
workflow YAML parsing pass; unrelated warning categories are not claimed clean.
The optimized build passes. Fresh local candidate SHA-256 is
`60cc9e27c36e44a256feedd8eed77ac47ba0f153b18661370949d35087da4683`;
the base commit remains `625ff0c` with uncommitted source changes, not a new
published commit or clean release artifact.

The unchanged literal-path driver now passes 29 commands / 23 checks in
`phase2-pull-literal-60cc9e2-serial-20260903/artifacts/report.json`: exact
changed payload bytes, unchanged sibling pointer, expected HEAD, strict Git
fsck and unchanged binary. Its SHA-256
`80b51d6631f949902ebcee222e32f9688ddee06ad2f8d6bf367ad5e6cf750252`
matches the retained baseline. Three sibling runs on the same candidate pass:

| Report run ID (`artifacts/report.json`) | Commands / checks | Proof |
| --- | --- | --- |
| `phase2-hydration-callers-60cc9e2-local-20260903` | 44 / 24 | Cold clone profile and deferred post-pull hydration; independent 4 MiB payload hashes, refs and strict fsck |
| `phase2-pull-classification-60cc9e2-local-20260903` | 73 / 22 | Neutral-path real conflicts plus no-op merge/rebase controls |
| `phase2-pull-machine-60cc9e2-local-20260903` | 85 / 39 | JSON/JSONL errors, original Git diagnostic, no false HEAD advance, explicit nested-root isolation |

These four completed runs total 231 commands / 108 checks. A separate first
attempt, `phase2-pull-literal-60cc9e2-local-20260903`, remains **failed** after
16 commands / 15 completed checks: initial push hit SlateDB writer fencing
before clone or pull. It was run concurrently with the sibling probes. Its
failure is not removed by the serial pass and is not evidence of a literal
selection regression; shared metadata ownership needs separate qualification.
The same candidate also passes full native Git/mirror RustFS qualification:
464 commands / 133 checks, report
`phase2-protocol-mirror-60cc9e2-local-20260903/artifacts/report.json`.
Native ref lifecycle, full/shallow/partial reads, security rejection, mirror
hook composition, plan/apply/reapply and approved deletion checks pass.
Candidate binary and the seven touched production-source file hashes are
unchanged after these runs; the local source attestation is
`phase2-candidate-60cc9e2-source-attestation.json` under the qualification root.
The optional prior-release rollback binary was omitted. The five completed
suites total 695 commands / 241 checks; the separate failed run is excluded
from that pass count, not erased.
The separate 1,000-push replay retains its earlier pinned `625ff0c` executable
and cannot certify this newer source. These are functional checks on a
concurrent development host, not controlled performance or native/provider
matrix proof. The known pull transport-assertion casing mismatch remains
pending approval and is not made green by these results. No original Phase 2
gate is closed by this checkpoint.

### Concurrent bucket-shared metadata writers: qualification follow-up

**Context.** The retained 16-command initial-push failure above reports
`CRAB-E0503` for `chunk_index_db`, with SlateDB's `Fenced` close reason and zero
reported pushed refs. It occurred while independent repositories in the same
RustFS bucket were being qualified concurrently. Distinct repository prefixes
and local cache roots do not imply distinct global chunk-index manifests.
The same literal driver passes in a later run with the three sibling probes
finished; this isolates literal hydration behavior, not concurrent-writer
correctness. The long Kubernetes replay was still active. No exact competing
writer identity or deterministic interleaving has yet been established.

**Independent failed-run readback.** A separate read-only AWS LIST/GET audit
passes six commands / nine checks in
`phase2-failed-push-readback-60cc9e2-20260903/artifacts/report.json`. The exact
failed repository has a generation-zero empty ref manifest and no ref-journal
objects. Its ref lock and all admission slots are explicit released records;
its repository GC fence has no active writer or sweep. Uploaded pack and file
index objects remain retained. The complete 24-object repository inventory
(key, ETag, size) and original failure-report hash are unchanged before/after.
No Crab advertisement, repair, retry, publication or deletion was invoked.
This proves the observed failed push did not expose refs and left these
repository leases released; it does not reproduce the competing writer,
prove bucket-global fencing or resolve the concurrency defect. Original
failure remains failed, with SHA-256
`1f83283fa7f2e3ed7537c28e503dd06f43c088b158844e8b7bdd284dc689ce8f`.
Audit driver SHA-256:
`62ec0502248d6e739950533a61e2da3c87af2afb54dfaa86f413bea56386c7c9`.

**Known ownership contracts.** `build_push_metadb_guard_with_object_store` in
`crab/src/git/push.rs` derives the shared chunk-index path from the global
storage domain. `promote_metadb_to_candidate_writer` deliberately delays
writer creation until candidate publication work, but a short writer lifetime
is not mutual exclusion. The metadata `Db::open_with_cache` path opens a
SlateDB writer; its pinned 0.15.0
[`CloseReason::Fenced` contract](https://docs.rs/slatedb/0.15.0/slatedb/enum.CloseReason.html#variant.Fenced)
makes that instance unusable. Per-ref leases are repository-scoped, while
`PushAdmissionTicket::acquire_fences` uses the GC fence's shared writer mode;
neither fact is proof of exclusive ownership of one global metadata writer.
These boundaries also exist on the currently available `origin/main`; a
deterministic before/after test is still required before attributing the
observed failure to this branch or declaring it pre-existing behavior.

**Phased plan and acceptance.**

1. Reproduce with two barrier-controlled push candidates for different
   repositories sharing one isolated global metadata domain. Record exact
   manifest path, writer epochs, refs, dependency receipts and terminal errors;
   run pinned base and candidate. Acceptance: overlap is proven, the failing
   interleaving is deterministic, and both failed/successful ref outcomes are
   independently read back. Do not interrupt the existing long replay or use
   bucket-wide GC for this test.
2. Establish one owner for global writer admission, database close and lease
   release. Evaluate a short canonical-store-scoped writer lease against an
   immutable-index publication design; do not serialize the entire upload.
   Acceptance: two disjoint pushes cannot fence each other's accepted writer;
   cancellation/timeout/fencing drain the database before ownership handoff;
   uncertain ref publication is read back, never blindly retried or called
   successful. New persistent coordination keys or layouts require the
   explicit writer-format/upgrade decision already tracked by this plan.
3. Cover all writers and callers, including native helper, CLI, mirror,
   protected push and metadata maintenance. Acceptance: no alternate writer
   bypasses the owner; read-only hydration still does not fence writers;
   process death and successor takeover preserve acknowledged data. Validate
   bounded admission and tail latency at 1/2/8 simultaneous repositories on
   the declared provider matrix. Preserve an isolated single-writer baseline
   and distinguish queueing from upload and metadata-commit time.

Status: observed failure plus an executable investigation/design packet;
no concurrent-metadata fix or full scalability claim in this checkpoint.

### Historical first-read admission: retained failure and separate recovery proof

**Context.** Exact `625ff0c` passed three focused RustFS suites: hydration
callers (44 commands / 24 checks), native Git/mirror (464 / 133), and bulk LFS
(57 / 27). Binary SHA-256 is
`424d0df7a99920d1396421b86c62cf89c6a3395aee2afc6a0854496727a5a94e`.
Those passes do not cover the historical first-read failure below.

Exact `625ff0c` protocol CI subsequently completed successfully in all nine
jobs: [run 33753308414](https://github.com/crabbuild/crab/actions/runs/33753308414).
This includes Windows/macOS contracts, Git 2.30/2.40/2.45/current clients and
the released-shape RustFS lifecycle. It is not proof for the newer local pull
changes, production-provider coverage or controlled performance.

`phase2-historical-clone-reconfiguration-625ff0c-20260903/artifacts/report.json`
failed after 37 commands / 24 completed checks. All four pushes succeeded;
the subsequent `git ls-remote` failed after 401,985 ms. Its stderr records
journal compaction followed by locator publication ending in a coordination
renewal deadline error. No clone/payload acceptance gate completed in that run.
Do not relabel it as passing after a retry or call it a hydration failure.

**Observed recovery, not root-cause proof.** A separate diagnostic retry
(`phase2-historical-admission-retry-625ff0c-20260903/artifacts/report.json`)
passed 14 commands / 9 checks and advertised the exact expected ref in 219 ms.
The locator lock was already a released tombstone before retry and unchanged
afterward. Original failure report, candidate hash and source checkout remained
unchanged. This establishes recoverable advertisement on retained state, not
why renewal missed its deadline, absence of earlier lease expiry, cold-read
reliability or controlled performance.

**Phases and acceptance.** First instrument and reproduce the original
first-admission sequence in a new isolated namespace, retaining request,
renewal, locator-work and close timings. Distinguish provider latency, host
contention and long non-yielding work before choosing a fix; increasing TTL
alone is not acceptance. Audit `upload_pack_wire` admission into locator repair,
`push::while_renewing_internal_lock_impl`, locator publication and writer close.
The non-cancelling wrapper currently retains a renewal error while allowing
the operation to finish, and writer close can publish a checkpoint. Prove
mutation/publication behavior under renewal loss with a successor, preserving
mandatory SlateDB close and holder-checked lock release. Cancellation is not
by itself continuous fencing. Keep the existing writer/format and strictly
read-only admission decisions explicit; do not silently extend their contract.

**Post-repair client proof.**
`phase2-historical-cold-clients-625ff0c-20260903/artifacts/report.json` passes
46 commands / 42 checks: separate empty-cache lazy/ordinary-hydrate, eager and
selective clients reconstruct the original 581,598,168-byte version; a fourth
lazy client preserves committed project policy before explicit relocation and
ordinary hydration of the original 581,598,166-byte version. Independent
SHA-256 checks, exact commits, strict Git fsck, unchanged clone index and final
source/binary identity checks pass. The original failed report is unchanged.
This is post-repair client proof, not a new first-admission pass.

Then require fresh first-admission fault/success proof,
the full 1,000-push replay, provider/OS matrix and controlled baseline before
closing the corresponding original gates. No gate is closed by this note.

### Terminal receipt retry and fault-point checkpoint: 2026-09-03 UTC

The direct ref-journal and managed manifest authorities now persist immutable
plan intents and terminal receipts. Apply distinguishes historical commit
attribution from a fresh current-state inspection; equal refs alone cannot
claim another writer's result. Exact immutable retries reuse one intent,
while distinct uncommitted candidates consume the bounded attempt budget.
This prevents a lost terminal write after an identical retry from appearing
to be two committed results. The generated `mirror.apply` schema includes the
commit identity and fresh `current` inspection.

Local deterministic tests cover interruption before head preparation, after
one/all prepares, after intent, and after the active marker but before head
promotion or terminal publication. They preserve the old batch before commit,
recover the exact new batch afterward, and retain one transaction identity.
The real journal compactor is exercised between lost terminal publication and
read-back; managed read-back is exercised after a successor manifest. This is
not yet a process-kill or tagged-client/provider qualification table.

Verification: metadata 299 passed/1 ignored; auth server 83 passed; schema
drift passed. The full Crab library run completed with 4,054 passed, 13 failed
and 3 ignored. Its initial multi-ref stack overflow was reproduced alone and
fixed by heap-pinning the independent initial-publication proof futures; the
existing multi-ref test then passed on the default stack. Remaining failures
include the known layout/HEAD/casing/schema fixtures and a concurrent Git
environment-contamination group. LFS discovery (29), recent selection (15),
and versioned import (1) pass in isolated reruns; this does not make the full
suite green. Fixture/inventory corrections still require explicit approval.

Draft PR #148 carries the implementation and these follow-up proofs. The
pinned Kubernetes replay is independent and still running at this checkpoint.
Managed-service E2E, tagged v1.0.1 compaction,
backup/restore inventory, publication/GC lifetime, declared-source recovery,
host protection and the full platform/provider matrix remain open. No Phase 2
acceptance criterion is closed by this checkpoint.

### First-import receipt and early-return ownership: 2026-09-03 UTC

**Context and change.** A new real-Git pipeline regression reproduced an
initial mirror push that succeeded without a plan receipt: the empty-repository
manifest shortcut bypassed the journal authority. Planned mirrors now use the
same receipt-bound journal path for first and subsequent imports. Ordinary
initial pushes retain the manifest shortcut. Managed publication retains its
service-owned manifest authority. A second regression reproduced a still-held
pre-acquired lease immediately after invalid mirror context returned. Native
validation, empty batches and Git-context errors now await lease cleanup; the
redundant unguarded cancellation check before delegation was removed.

**Proof and acceptance boundary.** Both new tests failed before their source
fixes; all 25 native-push tests pass afterward, including the existing ordinary
push and staging-contention siblings. The debug candidate passed the full
RustFS protocol/mirror runner: 481 commands / 139 checks, report
`phase2-terminal-initial-debug-b05e6f8-20260903/artifacts/report.json` under the
qualification root. New live assertions require an exact two-ref initial
transaction receipt, then replay after source advancement with the same
historical identity, current source-ahead state and no new publication. The
checksum-verified tagged v1.0.1 binary also passes the runner's raw-OID rollback
probe. Debug/emulator evidence is not release/performance/provider acceptance.
The separate pinned Kubernetes replay and the previously failing broad-suite
fixtures/inventories remain open; no original Phase 2 gate is closed here.

**Accepted-marker process-death proof.** The separate local run
`phase2-mirror-authority-loss-debug-b05e6f8-20260903/artifacts/report.json`
passes 23 commands / 11 checks. A forwarding proxy holds the successful active
marker response while the harness kills the publisher's four-process tree.
Only its intent exists afterward. Tagged v1.0.1 then clones, passes strict fsck
and byte comparison, compacts the active marker and preserves the plan intent.
A restarted candidate resolves the exact transaction, writes its terminal
receipt, and replays again without a second marker or transaction. The
reproducible runner is `crab/scripts/e2e/run_mirror_receipt_rustfs_smoke.py`;
pass the candidate, checksum-verified tagged binary, existing isolated bucket
and workspace qualification root through the protocol runner's existing CLI.
Its repository-local rerun also passes 23 commands / 11 checks:
`phase2-mirror-receipt-runner-debug-b05e6f8-20260903/artifacts/report.json`.

The earlier wrapper-only kill report
`phase2-mirror-marker-loss-debug-b05e6f8-20260903/artifacts/report.json` remains
failed: the separately grouped Git publisher survived and finished its receipt.
That was not the intended authority-loss injection. This distinction also
leaves abrupt-parent-death child supervision open; the passing process-tree
kill must not be presented as automatic child cleanup. Other crash points,
GC overlap, managed finalize and production provider rows still require proof.

### Cold-cache inspection requires write admission: 2026-09-03 UTC

**Observed gap.** The local forwarding-proxy qualification denies and records
every PUT, POST and DELETE during inspection. Equal and source-ahead cases
pass with zero write attempts. A fresh cache for a Crab-ahead destination
instead attempts `locks/internal/git-read-admission-*/lock` and reports
unverifiable when that PUT is denied. The canonical Git objects are intact;
the source is an older ancestor of the destination. This is a read-permission
gap, not absent data or a reason to grant a checker mutable credentials.

Evidence: `phase2-mirror-readonly-before-3ed9d1d-20260903/artifacts/report.json`
passes the equal/source-ahead cases. The stronger cold-cache run
`phase2-mirror-readonly-cold-3ed9d1d-20260903/artifacts/report.json` retains
18 commands, three passing checks and the failing Crab-ahead check, plus the
exact denied operation. Run `crab/scripts/e2e/run_mirror_readonly_rustfs_smoke.py`
with `--crab-bin`, `--bucket crabbuild`, `--endpoint-url http://127.0.0.1:9000`,
`--require-existing-bucket`, `--root` on the workspace volume and a fresh
`--run-id`. At this revision the runner was intentionally red pending the
production fix.
The repository-local rerun reproduces the same single failure and denied lock
write in `phase2-mirror-readonly-runner-3ed9d1d-20260903/artifacts/report.json`.

**Owner trace and next implementation.** `mirror::reconcile::inspect` pins the
destination snapshot, then `fetch_changed_crab_objects` delegates missing
destination history to Git. `upload_pack_wire::serve` acquires mutable read
admission before opening the repository. Skipping that one PUT is insufficient:
upload-pack may compact journals, repair locator/visibility state and publish
generated packs. The shared remote-Git opener also requires a current locator;
metadata locator opens without a published checkpoint can create reader state.

1. **Non-writing canonical read boundary.** Extend the existing shared read
   owner to consume the caller's authorized, pinned snapshot and immutable
   pack inventory for inspection. Reuse bounded pack-index/range decoding;
   do not implement a mirror-only object reader, introduce full-pack fallback,
   or disable ordinary fetch admission globally. Published acceleration may
   help, but absent acceleration must not require repair or a checkpoint write.
   Acceptance: cold equal/source-ahead/Crab-ahead/diverged cases use zero
   mutation attempts and retain exact graph classification; missing/corrupt
   canonical objects return unverifiable. All readers close on every exit.
2. **Lifetime prerequisite.** Resolve the non-mutating GC observation/retention
   decision already required by the shared proof contract. Permission-safe
   object reads alone do not prevent an intermediate sweep. Acceptance: a
   sweep entirely inside the scan, state recreation and old-client maintenance
   cannot produce a false-clean result; unavailable proof is explicit.
3. **Caller and release proof.** Route mirror inspection through that owner,
   retaining local cache ownership, scope/hidden-ref authorization, cancellation
   and operation budgets. Extend the denied-write runner with absent indexes,
   corruption, active journals, cancellation and managed read grants. Re-run
   ordinary fetch, lazy fetch, apply and hook siblings; preserve their existing
   admission/publication authority. Production provider rows remain separate.

No cache-probe optimization was added: the live source-ahead test proved Git
already avoids transfer when the needed objects are local. The cold-cache
failure, not a mocked fetch-call count, defines the remaining implementation.

### Canonical history inspection implementation: 2026-09-03 UTC

**Implemented boundary.** `crab-remote-git::OperationContext::from_snapshot`
uses the caller's authenticated layout and pinned journal-projected inventory.
It does not construct a full repository handle, open SlateDB, acquire a lease,
repair acceleration, or expose generated-pack publication. Both individual and
batch object reads now use the existing verified pack-index/range/delta reader;
ordinary locator-backed opens retain their publication and coverage checks.
Snapshot entry counts are bounded before inventory copies. The complete
snapshot digest participates in runtime cache identity: a journal commit can
introduce a new pack without incrementing the compacted manifest generation,
so generation-only negative caching would incorrectly hide a newly added object.

`mirror::history` replaces inspection's child `git fetch crab` with this shared
reader. It reads missing commit/tag ancestry into the owned local bare cache;
source objects already present remain local. Cached parents are still walked,
not trusted as a complete-history frontier. Object hashes are verified before
ancestry is accepted. Local inflation/parsing/writes execute on a joined blocking
worker which retains shared ownership of the cache guard, including if its
awaiting caller is dropped. Existing native Git merge-base classification and
the final pointer/snapshot verification remain the comparison authority.

**Evidence.** The dirty debug candidate based on `230840e`, binary SHA-256
`c64a1296f11fa161cdbc65d6b436d19ee8da1ac08dfdd8c75bc26fb776f33c1f`, passes
`phase2-mirror-readonly-canonical-230840e-20260903/artifacts/report.json`:
25 commands, six checks, no denied mutation attempts. Cases cover equal,
source-ahead, cold Crab-ahead, divergence, and a cached destination tip whose
parent was removed from this run's disposable local cache. The original
failing reports above are retained unchanged. Shared-reader regressions prove
REF/OFS/shared-base delta reads without a catalog or storage mutation, and
isolation of cached misses between same-generation snapshots. The 25 existing
mirror reconciliation tests pass unchanged.

The expanded response-fault run
`phase2-mirror-readonly-faults-230840e-20260903/artifacts/report.json` passes
29 commands and ten checks with zero mutation attempts. Missing and corrupt
immutable index/pack responses are injected only in the forwarding proxy;
each cold check reports unverifiable, and stored objects remain unchanged.
`phase2-protocol-mirror-canonical-230840e-20260903/artifacts/report.json`
passes 481 commands and 139 checks, including ordinary protocol/lazy fetch,
mirror check/plan/apply, hooks and tagged v1.0.1 rollback. Both runs use the
same candidate binary hash above. Seven adjacent read tests and 13 rejection
tests also pass. `crab-remote-git` library Clippy and touched-package formatting
pass. CLI-wide Clippy is not green: 488 diagnostics under the installed
toolchain; the first (`cache/hydrated_pointer.rs:401`, `map_unwrap_or`) is
unchanged on current `origin/main`. Filtering the diagnostic stream to the
touched mirror history/reconciliation files produces no diagnostics. This
does not waive the broader release gate or the previously recorded failures.

**Remaining executable gates.** This is permission-path proof, not completion
of section 2 or a GC safety claim:

1. Complete the non-writing sweep-observation/retention protocol from the prior
   subsection. Acceptance remains no false-clean result during an intermediate
   sweep, ABA recreation, or old-client maintenance.
2. Extend the passing cold missing/corrupt canonical pack/index and cancellation
   tests to managed read-only grants with scope/hidden-ref boundaries. Acceptance:
   explicit unverifiable/refusal, no mutation or grant widening, bounded cleanup.
3. Qualify very long destination-only history and many-pack inventories under
   the shared read budgets. The current defaults permit 10,000 remote logical
   reads, 20,000 storage requests and 512 MiB inflated work per operation; the
   local graph is additionally capped at two million objects and 512 MiB.
   Exceeding a budget is unverifiable, never partial success. Acceptance for
   advertised large-repository support requires measured cold/warm bounds and
   no regression against the baseline, not merely increasing these limits.
4. Retain ordinary fetch/lazy-fetch, mirror apply, hook, crash/receipt and tagged
   rollback proof against the final committed release candidate. The ongoing
   Kubernetes replay uses its earlier pinned release binary and cannot certify
   this new reader. Production-provider and cross-platform rows stay separate.

### Cancellation ownership and command status: 2026-09-03 UTC

**Observed failures.** A deterministic paused-origin test passed cooperative
cancellation but failed when its awaiting task was aborted: cache ownership and
origin work survived the caller. Retaining the cache guard in a blocking worker
was necessary but insufficient; the worker also needed cancellation and runtime
shutdown ownership. Separately, the real RustFS proxy test
`phase2-mirror-readonly-cancel-exit-3b0fee1-20260903/artifacts/report.json`
returned in 342 ms after SIGTERM but emitted `mirror.check` success JSON with
an unverifiable/cancelled diagnostic and exit code zero. This report remains
failed and unchanged. An earlier attempted run was refused by the binary/source
provenance check and is not behavior evidence.

**Implementation.** The inspection caller now owns a child-token drop guard.
Dropping that caller cancels the detached worker; the worker finishes its read
operation and shuts down its own remote-read runtime before relinquishing cache
ownership. Remote read failures are supplied to operation completion rather
than recorded as successful reads. The mirror command boundary rechecks user
cancellation before rendering check/apply success or persisting a plan, covering
diagnostic conversion across the whole inspection rather than matching one error
message. This small ownership change adds lifecycle control, not another reader,
lease or fallback path.

**Proof.** The paused-origin regression failed before the production change and
now passes for both cooperative cancellation and caller abortion. It checks that
origin references are released and the real cache lock can be reacquired while
the injected origin response remains blocked. It uses the existing `testing`
feature's store wrapper; Linux/macOS/Windows mirror CI commands now enable that
feature so the regression cannot silently be skipped. All 70 mirror tests pass
locally with it enabled.

`phase2-mirror-readonly-cancel-fixed-3b0fee1-20260903/artifacts/report.json`
passes 31 commands and 12 checks with zero mutation attempts. The blocked-index
SIGTERM case returns cancellation exit code 10 in 426 ms, before the proxy
releases its response; a following inspection reuses the same cache successfully.
Dirty debug candidate SHA-256:
`295910c5b36b266e561b47ba842b4bd04b49932776b58a6867e4ff96b8ea500c`.
Existing missing/corrupt and drift cases still pass. Cross-platform execution,
managed grants, hard parent death of publishing children and GC lifetime proof
remain separate release gates.

Receipt/rollback siblings pass again on the same binary:
`phase2-mirror-receipt-cancel-owner-3b0fee1-20260903/artifacts/report.json`
retains 23 commands and 11 checks, with four publisher-tree processes killed
after marker acceptance and exact receipt recovery after tagged compaction.
The eight existing store-wrapper tests and formatting also pass. CLI-wide
Clippy still reports the previously observed 488 diagnostics, with none in
the touched mirror history/reconciliation files.

### Thousand-push Kubernetes run completed: 2026-09-03 UTC

`phase2-kubernetes-journal-771e8be-20260903/artifacts/report.json` completed
with status `ok`: 1,152 commands, 1,000 incremental pushes plus the initial push,
and 36 passing checks. Terminal evidence includes cold/warm full clones,
blob-none clone, shallow depths 1/10/100/1000, full and incremental Git fsck,
matching advertised refs/tips, and an unchanged source checkout. Generation
maintenance converged after 31 passes at the final checkpoint. This is the
earlier pinned release binary SHA-256
`771e8bee09211480f7d46b628546547f4ec57f3087af877c9c0276d0fcd0578a`, not
the current cancellation/read-only candidate.

**Evidence audit.** The driver's check named `sampled-objects-byte-identical`
actually compares `cat-file --batch-check` object IDs/types/sizes. To add direct
byte evidence without rewriting that completed report, both source and retained
cold clone were read with `git --no-replace-objects -c protocol.allow=never
cat-file --batch`, `GIT_NO_LAZY_FETCH=1`, and the retained 1,000-OID sample on
stdin. Both complete output streams have SHA-256
`5168883b61da07077b6a4214f7e354241e0f61b2c19342a9e722bcfb3fd29572`;
both pipelines exit zero. These are Git representation bytes, not evidence that
every Crab pointer payload was hydrated. Strengthen the reusable driver to
capture raw-byte evidence directly in a future run.

The report remains `valid_for_comparison: false`: recovered historical payload
preparation and uncontrolled development-host load make it unsuitable as a
performance baseline. The unchanged standard verifier correctly refuses it for
comparison. Do not relabel it, change expected values, or close the performance
gate. Next acceptance: run the exact final release candidate through fresh
large-repository add/commit/push/hydrate/clone and pointer-byte verification,
then repeat baseline/candidate measurements in a controlled environment.

### Raw-object qualification evidence: 2026-09-03 UTC

**Gap closed in the runner.** `run_large_repo_rustfs.py` now compares complete
`cat-file --batch` streams as well as strict object metadata. Both reads disable
replacement objects and lazy fetching; a missing-object response is rejected
even when Git exits zero. Acceptance requires equal metadata, SHA-256 digests,
and the exact stream length implied by every object's header and content size.
The correctness fingerprint also binds the raw-stream digest; baseline and
candidate comparisons must be rerun with this stronger producer.
The existing supervised command path spools binary output to a temporary file
on the workspace volume, hashes it in fixed-size chunks, and retains only the
digest and byte count. Raw object bytes never enter text decoding or retained
logs. Timeout/nonzero exit remains failure, not partial-byte evidence.

**Proof.** All 38 qualification/report tests pass, including real Git objects
with deliberately altered contents but unchanged ID/type/size output, missing
objects, binary/non-UTF-8 output, and timeout rejection. A read-only proof slice
using the retained Kubernetes source/cold clone and 1,000-object sample records
matching 28,169,810-byte streams with SHA-256
`5168883b61da07077b6a4214f7e354241e0f61b2c19342a9e722bcfb3fd29572` in
`phase2-raw-object-proof-6d9fd88-20260903/artifacts/report.json`. The source
checkout remains unchanged. This slice is explicitly invalid for performance
comparison and does not replace or rewrite the earlier full-run report.

**Remaining gate.** The report verifier and historical fixture expectations
are unchanged; old metadata-only reports do not retroactively acquire byte
proof. Release review must require the new per-check source/clone stream
digests and lengths alongside the existing checks. A strict versioned verifier
migration remains separate from this runner correction. These are sampled Git
representation bytes, not exhaustive managed-payload hydration proof.

### Committed release Kubernetes lifecycle: 2026-09-03 UTC

**Scope and provenance.** The existing `run_add_commit_push_rustfs_smoke.py`
source workflow completed on release commit `6d9fd88`, binary SHA-256
`f3b041230b4f6a5d8cfa9f76d710736a4a1acbcb141634903051cba2dc78caee`.
The read-only upstream Kubernetes input was `160bd16d98b7f688ce4f3b5ab0c5e4c045f36233`
with 140,777 reachable commits. Each workflow used its own disposable clone and
isolated remote prefix in local RustFS bucket `crabbuild`.

**Acceptance evidence.**
`phase2-k8s-lifecycle-6d9fd88-20260903/artifacts/report.json` is `passed`,
with 63 commands and 48 checks. Both `crab add`/`crab push` and native
`git add`/`git push` paths use ordinary `git commit`, publish the full input
history plus the new commit, and pass fresh-cache Crab clone, advertised-tip
equality, lazy pointer checks, hydrate, dehydrate and rehydrate. Each case has
two identical 64 MiB managed files, matching pointer hashes and hydrated bytes:

- Crab-command case SHA-256:
  `27e08265ea6f2060869d69a4f7793f696e8445cbbd4c0cd13c6bac598a93174e`.
- Native-Git case SHA-256:
  `4c320548124de32e462e47cdc4ec71c7ab38a7aa9b8541b1fcf698d02c1ab9b4`.

The source checkout and selected binary remain unchanged. Following the
runner's connectivity checks, both retained clones independently pass
`crab fsck --json` with zero errors/repairs and offline
`git --no-replace-objects -c protocol.allow=never fsck --full` with
`GIT_NO_LAZY_FETCH=1`, exit zero. No repair or GC was requested.

**Not established.** This is functional qualification, explicitly not a
performance comparison. Bucket-global xorb/shard count deltas cannot attribute
all concurrent writes to these cases and are not storage-efficiency evidence.
Controlled baseline/candidate measurements, final-candidate protocol/rollback
matrix, GC lifetime, protected publication, managed grants and cross-platform
gates remain open. Do not mark Phase 2 complete from this lifecycle run.

### Multi-ref Kubernetes pointer-scan scaling: 2026-09-03 UTC

**Observed failure.** The mutation-denying proxy run
`phase2-k8s-readonly-mirror-6d9fd88-20260903/artifacts/report.json` failed on
the retained Kubernetes publication: all 1,246 source refs were captured and
the published main ref compared equal, but pointer verification exhausted
8,000,000 lookups. It returned unverifiable, not clean, after 67,327 ms.
There were zero attempted remote writes, and source/binary identity checks
passed. The original failed report remains unchanged. The source has
1,790,727 distinct reachable objects; ref fanout, not an oversized distinct
object set, exposed repeated ancestry work.

**Implementation candidate.** Pointer discovery now resolves roots with the
existing strict annotated-tag parser and runs one multi-tip commit walk over
their union. Gitoxide's `Simple::new` contract visits each reachable commit
once; the existing tree visitor shares its visited set across that walk.
Direct tree/blob roots are then checked against the same closure. Tag objects
consume the same distinct-object budget; lookup/allocation/cancellation and
raw large-blob verification remain enforced. No limits were increased and no
source refs were removed. Shared root resolution avoids a second tag policy.

The direct push and managed receive visibility builders still use per-ref
closures: their authorization indexes cannot substitute an all-ref union.
The added isolation regression protects that caller difference. Non-test
code growth pays for union traversal while retaining the required per-ref
result, not a compatibility fallback or duplicate Git object reader.

**Current proof and next acceptance.** The 100-tag/40-commit regression failed
at 800 lookups before the change and passes afterward. All 20 traversal tests
pass with both minimal and facade features, including missing/corrupt/wrong
kind objects, nested tags, direct tree/blob targets, budgets, cancellation and
per-ref isolation. A shared missing parent also rejects the entire pointer
inventory. All 70 mirror tests and `crab-git` library Clippy pass.

The committed release candidate `1e21e36` passes
`phase2-k8s-readonly-union-release-1e21e36-20260903/artifacts/report.json`:
nine commands and ten checks, including cold/warm exact source-ref inventories,
equal published main, one discovered and verified managed file hash, matching
recipe digests, zero mutation attempts, and unchanged source/binary identities.
Binary SHA-256:
`1bb4fec16e57a444a61f3af4fb1972463d8e18aef225723a34a7f7fd06187555`.
Both raw results correctly remain `source_ahead` with `ci_passed: false`:
the source retains tags absent from the earlier main-only publication. This is
successful full pointer inspection and drift classification, not convergence
or permission to publish those tags. Observed cold/warm durations were
106,418/70,032 ms; neither uncontrolled development-host timings nor the separate
debug run establish a performance improvement. The separate debug candidate
run `phase2-k8s-readonly-union-debug-d45ac62-20260903` also completed with
status `ok`, nine commands, ten checks, and one discovered/verified pointer
hash in both inspections. It does not qualify the later cancellation fix.

The controlled regression gate, long destination-only remote ancestry, and
GC lifetime remain open. The pointer worker's caller-abortion ownership proof
is recorded below: cooperative cancellation coverage alone does not prove
that dropping the caller retains its cache until that worker exits.

### Pointer-scan cancellation ownership: 2026-09-03 UTC

**Context and reproduced gap.** The pointer collector previously moved only
the Git directory path into `spawn_blocking`. Dropping its async caller also
dropped the mirror cache owner, although the queued/running scan could still
access that directory. A deterministic single-worker regression occupied the
blocking pool, queued the real collector, and dropped its caller. Before the
fix, a second cache owner could acquire the directory before the scan ran.
This is separate from ordinary cooperative command cancellation.

**Ownership change.** `crab/src/cmd/mirror/pointers.rs` is now the shared
collector for reconciliation and the collaboration hook. Its source owns
either the mirror's `Arc<CacheUseGuard>` or the user's ordinary Git directory
path. The worker retains that source while queued/running and returns it for
the subsequent raw-blob verification. Dropping the caller cancels a child
token; the parent operation is not cancelled. The ordinary hook source does
not create mirror-cache markers in the user's repository. The old collector
is removed; traversal, limits, pointer validation and raw-byte verification
remain one canonical path. The small non-test growth establishes lifetime
ownership instead of adding a second scanner or compatibility path.

**Dependency and sibling proof.** Tokio's installed `spawn_blocking` contract
does not stop a started worker when its awaiter is dropped; tokio-util's child
token and drop guard provide one-way cancellation. Cache lifecycle ownership
must outlive all cache workers. The ancestry loader already follows this
ownership rule; raw Git verification uses the existing synchronous owned
process runner, which stops children and joins pipe workers before returning.
Both pointer-collector callers now express their actual ownership boundary.

**Acceptance and current evidence.** The reproduced queued-worker regression
passes: competing acquisition fails until the worker drains, then the cache
is reusable and the parent token remains active. A real Git raw-blob test
checks ownership during verification and release after both success and an
injected terminal error. All 72 mirror tests pass; the two focused tests also
pass after tightening the worker-drain check to include actual lock release.

The release build of committed candidate `86744b7` succeeds; binary SHA-256:
`e319c2f960421d0b9982034c5740b8489c40a8086f39b8c4cfe95a180ce3a685`.
`phase2-mirror-readonly-pointer-owner-86744b7-20260903/artifacts/report.json`
passes 31 commands/12 checks: equal/source-ahead/Crab-ahead/diverged inspection,
incomplete-cache recovery, missing/corrupt immutable data refusal, and zero
inspection mutation attempts. SIGTERM during the blocked canonical index read
returns exit 10 in 323 ms before the response is released; retry reuses that
cache successfully. This exercises command cancellation, while the unit
regression specifically covers dropping the pointer worker's async caller.

`phase2-mirror-hook-pointer-owner-86744b7-20260903/artifacts/report.json`
passes 61 commands/16 checks by invoking the existing installed-hook batch
qualification directly. Custom hook location, detached HEAD, revision
expressions, annotated tags, mixed ref updates/deletion, pointer/LFS bytes,
strict clone fsck, second-remote rejection/retry, explicit rewrite and
whole-batch conflict refusal all pass. Binary identity is unchanged. No
mirror-cache use/clean markers appear inside the hook source repository.

`phase2-k8s-pointer-owner-release-86744b7-20260903/artifacts/report.json`
is `ok`, with nine commands/11 checks. Cold/warm inspection retains all 1,246
refs, matches the published main ref, discovers/verifies the one managed file
hash, and yields matching recipe digests. Source and binary identities remain
unchanged; there are zero remote mutation attempts. Both results correctly
remain `source_ahead`/`ci_passed: false` because the source's tags were not part
of the earlier main-only publication. Observed durations are 109,911/69,729 ms,
not a controlled baseline/candidate performance comparison.

These functional checks do not establish controlled performance or complete
Phase 2's remaining gates. The retained dirty GC regression and generated web
files are not part of the release source change; their presence is recorded
by the qualification provenance rather than claimed to be a clean checkout.

### LFS publication admission ordering: 2026-09-03 UTC

**Observed gap.** The shared native/remote-helper push pipeline published LFS
dependencies during preflight, before repository capacity and GC writer
admission. Current `main` has the same ordering. A real-Git fixture with a
locally available LFS payload holds a sweep lease, starts the push, waits for
its admission attempt, and cancels it. Before the change, the repository-sweep
case fails because the LFS object has already been uploaded. This proves an
upload outside admission, not that current GC deletes LFS content or that a
ref was acknowledged with missing data.

**Change and ownership.** Move the existing reachable-LFS publication gate
into `execute_admitted`, ahead of the pack/upload work. Keep the same scanner,
transfer coordinator, byte verification and exact proceeding-ref selection;
use the refreshed, under-lock base for exclusion tips. Do not add a lease,
setting, format or second upload implementation. Direct pushes now perform
this work while their existing capacity/GC permit is renewed. Active-active
pushes have already acquired their existing writer fences. Managed helpers
still upload into session-private staging; service-side promotion remains
inside `ReceiveWriterFences`, not a new client-side repository lease.

**Acceptance.** Repository and global sweep fixtures must reach admission,
then cancel with no LFS upload, unchanged refs and all acquired push/admission
leases released. Existing LFS publication, bounded pack/cancellation and
mirror tests must pass. Both sweep cases now pass, including unchanged refs
and lease release; the focused 80-test run passes. Qualify the committed
release with the installed
mixed-ref hook and protocol/mirror workflows, including fresh-clone pointer
and LFS bytes. A passing admission fixture is not the full publication
lifetime gate.

**Release evidence.** Candidate `802dec8` builds successfully; binary SHA-256
`f4f4e188d69061fae573cf0a47b39ba82ed30b3281fc2a083eeb0fa4a5d82411`.
`phase2-protocol-mirror-lfs-admission-802dec8-20260903/artifacts/report.json`
passes 481 commands/139 checks, including native ref lifecycle, accepted
partial/shallow operations, exact pointer/LFS bytes, mirror checks and
plan/apply/replay, mixed installed-hook publication, deletion safety, and
the tagged v1.0.1 rollback case. This is macOS/local RustFS functional
evidence, not a controlled performance comparison or other provider/OS proof.

`phase2-k8s-lifecycle-lfs-admission-802dec8-20260903/artifacts/report.json`
passes 63 commands/48 checks on read-only Kubernetes input
`160bd16d98b7f688ce4f3b5ab0c5e4c045f36233`. Both Crab-command and native-Git
workflows publish their new commit, clone into fresh caches, and hydrate,
dehydrate and rehydrate two identical 64 MiB managed files. Hydrated SHA-256
matches source bytes in each case: Crab-command
`448586b3d21144bc3f919fa6e49e382e072bf45a95062036ed18bf9bca9fb2a1`;
native-Git `99c0f275dea2c89f3eeb02da90c193487116b63f3f3d31490a37d3368c01341a`.
Original input and selected binary remain unchanged. Both retained clones
contain 140,778 reachable commits and pass offline
`git --no-replace-objects -c protocol.allow=never fsck --full` with lazy fetch
disabled, plus `crab fsck --json` with zero errors, repairs or repair failures.
Bucket-global object-count deltas are not attributed deduplication evidence.

`phase2-k8s-raw-bytes-lfs-admission-802dec8-20260903/artifacts/report.json`
separately passes 12 commands/five checks using the canonical qualification
runner's deterministic 1,000-object sample and raw batch-stream digest. In
both workflows, source/clone metadata and complete 28,169,810-byte streams
match, SHA-256
`5168883b61da07077b6a4214f7e354241e0f61b2c19342a9e722bcfb3fd29572`.
The slice binds the lifecycle report and unchanged candidate binary; it does
not rewrite earlier evidence or substitute sampling for exhaustive Git or
managed-payload verification. Performance comparison remains explicitly
invalid on this shared development host.

**Explicit follow-up.** Pointer/deduplication classification still precedes
writer admission; its reused dependencies require revalidation under the
publication owner. Mirror's separately composed LFS pre-push guard also runs
before the prepared push and needs consolidation with that owner. Protection
through final read-back, managed-grant fault qualification and read-only GC
observation remain open. Do not mark those sibling surfaces complete from
this shared-pipeline ordering change.

### GC observation identity: reproduced upgrade decision, 2026-09-03 UTC

**Evidence and limit.** The local regression
`released_fence_identity_distinguishes_domain_recreation` acquires/releases a
real `GcFenceLease`, deletes only its isolated in-memory fence, then completes
another sweep. It fails: both released states are byte-identical, with schema
1, epoch 2, writer epoch 0, and empty holders/quarantine. The regression is
uncommitted and intentionally red; no production contract has changed.
This proves that current serialized state is not an incarnation identity,
not that a correctly rooted GC run has deleted live data.

`phase2-gc-observation-aba-d45ac62-20260903/artifacts/report.json` separately
checks the provider property on one new, run-owned RustFS object. Identical
bodies before/after delete-and-recreate receive the same ETag
`8106e21a6f59e6b359f354e91a336086` and no version ID. Only that diagnostic key
was deleted and immediately recreated; no GC command or existing repository
data was touched. The different last-modified timestamps in this run are not
evidence of a monotonic, unique provider incarnation contract. The report is
explicitly excluded from performance comparisons.

**Compatibility decision requiring approval.** Tagged v1.0.1 has the same
schema-1 state, strict unknown-field rejection, and schema-version validation.
A versioned incarnation field would therefore make old writers and sweepers
refuse a migrated domain, not coexist transparently. Do not add an optional
field, silently rewrite existing fences, or rely on ETags as an ABA-proof
substitute. Proposed direction: an explicit versioned coordination upgrade,
with a fresh unpredictable incarnation identity on domain creation, persistent
released state, checked epoch advancement, and migration under quiescence.
Arbitrary restoration of an old coordination object remains outside an
unversioned store's observable guarantees and needs an explicit restore policy.

**Executable packets after approval.**

1. Coordination owner: implement the versioned identity and explicit migration;
   refuse absent, malformed, unsupported, saturated or unqualified observation
   state. Acceptance: the current red test passes; old-client refusal, expiry,
   crash, deletion/recreation and restore boundaries are tested. No automatic
   migration of unrelated global domains or bucket-wide mutation.
2. Shared inspection owner: capture non-writing observations for both repository
   and physical global-data domains before snapshot/history/dependency reads;
   revalidate after the complete scan. Reject crossed sweeps and uncertain
   holders without acquiring a write lease. Acceptance: a sweep entirely
   inside the scan is detected, cancellation remains bounded, scoped grants
   cannot read another domain, and all inspection paths attempt zero writes.
3. Publication owner: use existing writer admission, revalidate under that owner,
   and keep it through dependency flush, ref commit and terminal read-back.
   Acceptance: direct, managed, hook and apply paths reject stale proof or lost
   protection; no independent parent/child guard stack is introduced. Run
   isolated RustFS fault tests and the tagged-client migration/rollback matrix
   before repeating the large-repository final-candidate qualification.

The proposed upgrade is not implemented or approved by this evidence packet.
The read-only lifetime and Phase 2 completion gates remain open.

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
