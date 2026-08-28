# Plan: Harden Crab for large repositories and teams

> **Executor instructions**: This roadmap is divided into independently
> reviewable phases. Execute one phase per branch and pull request. Follow each
> phase's steps and run every verification gate before proceeding. Do not begin
> a dependent phase until the prerequisite phase has landed. Record benchmark
> reports, commands, commit IDs, and CI links in the phase evidence table near
> the end of this file. If a STOP condition occurs, stop and report it instead
> of inventing a compatibility path or weakening an invariant.
>
> **Drift check (run first for every phase)**:
> `git diff --stat aa150868..HEAD -- crab/src/git crab/src/cmd crates/crab-git crates/crab-metadata crates/crab-read crates/crab-remote-git crab/scripts .github/workflows packages/web/content/docs`
> If an in-scope file changed, compare the current implementation with the
> relevant "Current state" section before proceeding. Update this roadmap in a
> planning-only commit if the ownership boundary or dependency contract moved.

## Status

- **Priority**: P1
- **Effort**: L, expected as 7 independently reviewable phases
- **Risk**: HIGH
- **Depends on**: none; Phase 0 is the prerequisite for all implementation
- **Category**: performance, correctness, architecture, operations
- **Planned at**: commit `aa150868`, 2026-08-23
- **Foundation PR**: https://github.com/crabbuild/crab-oss/pull/59
- **Implementation PR**: https://github.com/crabbuild/crab-oss/pull/75 (merged)
- **Large-repository follow-up**: https://github.com/crabbuild/crab-oss/pull/87
- **Pack/repack follow-up**: https://github.com/crabbuild/crab-oss/pull/96

### 2026-08-25 execution update

The current branch includes the following bounded large-repository hardening
that must be qualified before this roadmap can claim acceptance:

- `crates/crab-metadata/src/git_visibility.rs` now uses the immutable
  post-checkpoint marker for readiness, so a healthy catalog check does not
  open SlateDB or scan the object locator. The legacy ensure path still
  backfills that marker before publication, and the marker is bound to the
  catalog generation and validation digest.
- `crab/src/git/upload_pack_wire.rs` now admits one repository together with
  its catalog-bound ordinal proof. Protocol-v2 upload-pack, exact shallow fetch,
  and promisor fetch reuse that proof without materializing the complete OID
  dictionary. Selected ordinals are resolved through the operation's pinned
  catalog session.
- `crates/crab-metadata/src/git_object_locator/reader.rs` uses bounded
  read-ahead and parallel fetch tasks for dense ordinal selections, while
  retaining exact point reads for sparse selections.

The c412 baseline qualification
`local-k8s-marker-c412-1-20260825` was run against Kubernetes revision
`b3bc2ac5` and external local RustFS before the lazy-catalog follow-up. Its
server-side incremental fetch was bounded (zero-millisecond visibility
planning, 72 ms response operation, and 4 ms pack generation), but the full
clone helper still spent roughly 90 seconds before its first request because
the old path opened the visibility dictionary twice. That report remains
diagnostic evidence; the full-profile report below is the pre-lazy baseline,
and the post-`cbe848f4` run below proves the lazy startup path. The latency
comparison remains open because the first post-lazy run was not isolated
enough for a valid differential result.

### Current execution state

The implementation work for Phases 1 through 5 is assembled on one integration
branch so reviewers can inspect the complete generation-binding contract across
push, read, maintenance, and GC. The post-lazy full-profile qualification is
now complete for an older baseline and the current branch head on K8s/RustFS;
each current binary still has only one full-profile run. Repeatability, the
10,000-push differential, fault, provider, concurrency, and rollout gates are
still open.

The current branch adds `bdfae4f2` and `c5797d8f` as intake-containment
follow-ups. Format-
derived bounds now protect every known-size pack index, reverse index, and kind
sidecar read in owner, receive, recovery, and push-probe paths; protected
staged-object reads also stop at their declared byte size before validating the
content hash. Generated-pack cache descriptors also reject a self-contained
artifact whose object count is smaller than the requested selection. The bound
helpers and cache invariant have fixture coverage, and the focused locator,
receive, push, and remote-Git cache tests pass. This prevents malformed or
unexpected provider bodies from filling memory or maintenance workspaces, but
it does not close the separate 10,000-push, provider-fault, fanout, retention,
or rollout qualifications.

`local-k8s-final-04655f3b-1000-20260825` used Kubernetes revision
`b3bc2ac58fa173967f27ade80f28cc5015b8c1c3`, isolated external RustFS, and the
installed binary built from committed source `04655f3b`. The standalone
verifier reports `status=ok`, `profile=full`, and `replay_count=1000`. All 22
harness checks passed: 1,001 pushes completed, advertised refs and clone tips
matched the source, full and incremental fsck passed, 1,000 sampled objects
were byte-identical, the source checkout was unchanged, and the run-owned
RustFS prefix was cleaned. Correctness fingerprint:
`7d97627cf1f4de8b87679dea53d99916df42c3152dc765399d4494c43af09624`.

The earlier 100-push smoke and pre-rebase 1,000-push reports remain useful
historical baselines. The post-lazy run below is the current correctness
baseline for this branch, but its push/clone comparison is explicitly invalid
for performance promotion. Repeatability and the remaining large-team
rollout gates are still open.

The earlier 2026-08-26 follow-up is committed as `c57ee1f4`. The anchored
visibility continuation is committed as `ba7aa84b`, with the exact-visible-tip
correction in `cdc2335e`:

- `GitObjectLocatorWriter` keeps exact OID point reads for sparse batches but
  switches to one validated object-family scan once accumulated candidates
  cover at least 1/64 of the current ordinal universe. New rows remain in the
  same in-memory map, so the scan is amortized across the writer lifetime.
- Suffix consolidation asks Git to reuse existing objects and deltas, and the
  catalog writer keeps the dense ordinal universe across pure repacks. A full
  ordinal rebuild is still required only after object rows are actually lost.
- Ref-journal compaction can stage an immutable, target-digest-bound ordinal
  visibility handoff. The owner resolves only changed tips/evidence after the
  target catalog checkpoint, validates sequential edits for the same ref, and
  publishes the V5 proof; the current manifest roots the pending object for
  repository-scoped GC.
- Owner publication verifies pack indexes and reads an immutable `.kinds`
  sidecar for each new or rebound pack, avoiding pack-body downloads during
  normal generation handoff. Direct pushes, protected receives, and
  synthesized packs publish the sidecar; legacy missing sidecars remain
  repairable through the bounded full-pack path. Filtered reads retain the
  canonical bounded traversal path when kind metadata is unavailable.
- `repair_required` no longer treats incomplete bucket-wide discovery as
  repository-local repair failure. The bucket-wide state remains visible in
  diagnostics and destructive bucket GC remains disabled until its separate
  completeness gate is proven.

- A new destination ref at a large existing commit tip emits bounded
  first-parent visibility evidence instead of attempting to walk the complete
  history. Materialized and catalog compaction reuse that base closure only
  when the parent is an exact visible ref tip in the target manifest; otherwise
  the handoff defers conservatively. This closes the `ls-remote`/catalog-proof
  failure found by the current Kubernetes qualification while preserving the
  bounded-walk guard for tags, unrelated bases, and missing history.
- A new destination ref whose tip already equals an existing visible ref tip
  reuses that exact tip as the visibility base, producing an empty delta rather
  than anchoring to its non-visible first parent. The source-side tip set is
  captured from the manifest snapshot used by the ref journal, so catalog
  handoff remains bound to a real target-manifest ref and still defers
  conservatively when no reusable base exists.

- Locator stale-pack cleanup now uses a derived `(pack_slot, oid)` membership
  index. Canonical OID rows remain authoritative; routine sweeps read only
  stale-slot memberships and point-validate their canonical rows, while a
  marker-less or interrupted catalog performs one idempotent rebuild before
  returning to the bounded path. Rebuilds use an explicit in-progress marker
  and verified retained-pack object counts, so coverage cannot advance over a
  partial reverse index. Pure repacks remove old memberships as they rebind
  OIDs, so they do not trigger a complete object-catalog scan.

The next owner hardening keeps the documented one-action-per-cycle contract
literal: a graph rebuild/compaction now prevents shallow-closure rebuilding
from running in the same poll. Owner JSONL samples also expose a stable
`maintenance_reason` and `next_eligibility_secs`, with immediate rechecks
represented as zero after supersession. This makes maintenance backlog and
lease occupancy observable without adding public configuration knobs.

The owner compaction budget is now bounded to the uncovered portion of the
current pack inventory (`4a8fc34e`). Before opening SlateDB, the owner reads the
published pack bindings and counts only packs whose physical facts are not
already covered. A same-inventory pass uses a metadata-only writer, while a
delayed pass no longer starts a repository-sized compactor merely because the
historical catalog is large. The current-head full-profile run below now
proves this budget through 1,000 replay pushes on a real Kubernetes repository;
sustained and repeated owner-budget gates remain open.

The generation-owner geometric repack now applies an explicit source-pack,
source-byte, source-request, and phase-deadline budget. When the full
geometric suffix exceeds those limits, the owner compacts the largest suffix
that fits and reports `geometric_repack_bounded`; if even two source packs do
not fit, it reports `geometric_repack_deferred` and retries on the next
eligibility interval. Explicit `crab repack` remains the unbounded operator
path, while automatic owner work cannot monopolize one repository's
maintenance lease. The bounded selector has unit coverage; sustained
10,000-push owner convergence and interruption evidence remain open.

The background owner now adds an incremental pack-tier accumulation gate. A
single undersized pack is retained while lower-tier siblings accumulate; once
their verified compressed bytes are comparable to the next tier, only that
suffix is promoted and the largest stable pack remains untouched. This avoids
rewriting a repository-sized pack on every small push while preserving the
explicit full geometric policy for operator-invoked `crab repack`; sustained
10,000-push convergence and interruption evidence remain open.

The regular post-CAS locator publication now uses the same uncovered-pack
budget (`88deb4e0`). Its snapshot is taken while holding the repository locator
lock, so stable historical packs do not make an ordinary small push start a
repository-sized SlateDB compactor. Publication and repair regressions pass,
and the current full-profile qualification below uses a binary built from
`d8de9d12`, which includes this path and the stale-snapshot guard.

The generation owner now re-reads the committed manifest after acquiring the
locator publication lock (`5465031e`). If the pre-lock owner snapshot is already
superseded, it exits before opening the locator session or planning pack rows;
the existing final manifest check still protects the smaller race where a push
wins during the bounded publication itself. The regression passes with a
missing stale-snapshot pack, proving the stale path cannot turn into an object
read or a failed repository-sized plan.

The locator writer now keeps small catalogs on bounded OID point reads and
selects the full object-family scan only when the pinned catalog has at least
4,096 objects and the accumulated batch covers at least 1/64 of that catalog
(`62d35c14`). The policy regression passes, and the current release-binary
RustFS same-ref smoke completed four integrated pushes at 288 locator requests
per successful push, below the 500-request budget that exposed the prior
small-catalog scan amplification.

The workflow scheduler lock now retains its lock inode across handoffs
(`d9c93263`). This prevents a releasing holder from unlinking the pathname
after a waiter has acquired the same inode, and the focused scheduler-lock
suite covers the handoff. The change is part of the large-team concurrency
hardening because a path-disappearing lock can strand or misdiagnose workers
under contention.

Focused source proof for this follow-up passes formatting, `cargo check -p
crab`, the complete Crab library suite, the complete metadata suite, strict
metadata clippy, and the targeted exact-tip visibility regressions. The
release binary embeds `d8de9d12` and is the binary used by the current-head
Kubernetes/RustFS qualification below.

The current release-binary qualification
`crabbuild-team-load-anchored-cdc2335e-k8s-20260826` used Kubernetes revision
`b3bc2ac58fa173967f27ade80f28cc5015b8c1c3`, the external local RustFS
endpoint, `replay_count=100`, and the team-load harness with one client per
fanout. It completed with `status=ok`, 27/27 checks passed, the source
checkout was unchanged, all sampled objects were byte-identical, full and
incremental fsck passed, and both local worktrees and the remote qualification
prefix were cleaned. The run saw the full 140,054-commit source history and
1,643,211 Git objects; the binary provenance and report are retained with the
run artifacts.

- Generation maintenance reduced the active generated-pack inventory from 92
  packs to 2 and swept 91 obsolete locator-pack rows.
- Cold full clone completed in 254 seconds end to end. Its two-pack response
  was 1,241,817,145 bytes, generated from 1,264,940,590 source bytes in
  174 seconds of server-side response-pack work.
- Warm full clone hit the generated response-pack cache and completed in 138
  seconds end to end; server-side response-pack work was 83 seconds.
- Blobless planning used `catalog_filter`, planned 1,102,159 objects, and
  omitted 541,052 blobs. Depth-100 planning used the shallow-closure index in
  240 ms; depth-1,000 planning used it in 424 ms.
- The one-client incremental fetch completed in 674 ms. One independent ref
  push completed in 10,076 ms and one same-ref push completed in 6,309 ms;
  both were accepted with zero unexpected failures. The exact-tip seed push
  emitted `added_objects=0`, `deferred_updates=0`, and the later advertised
  refs check passed, reproducing the failure boundary fixed by `cdc2335e`.

This is a current-binary single-host qualification, not production SLO proof:
the 100-client synthetic fanout, repeated isolated runs, 1,000/10,000-push
differentials, fault/provider/failover/canary evidence, and default-on rollout
gates remain open below.

The exact post-budget Kubernetes/RustFS smoke
`crabbuild-f2a941ce-k8s-20260827-smoke` used the release binary whose embedded
source revision is `f2a941ce`, the same Kubernetes revision, and 100 replay
commits. The standalone verifier returned `status=ok`; 101 pushes and 21/21
checks passed, including full, blobless, depth-1/10/100/1,000, and incremental
fetch correctness, exact object sampling, fsck, source immutability, and
remote cleanup. The correctness fingerprint remained
`7d97627cf1f4de8b87679dea53d99916df42c3152dc765399d4494c43af09624`.

- Physical pack objects grew from 1 at seed to 2/12/103 at checkpoints
  1/10/100; physical pack bytes grew from 1,254,754,431 to 1,297,738,866.
  This is retained immutable history, not a claim that active serving packs
  should grow without bound.
- Owner totals were 83.8/77.8/224.8/244.7 seconds at seed/1/10/100. After
  the geometric repack, the bounded follow-up catalog passes scanned and
  deleted 10 and 91 stale pack rows, reading 14.4 MB and 37.2 MB respectively.
  The earlier pre-repack catalog advance remains an intentional O(N) rebuild
  path, so the 1,000-push owner latency, memory, and interruption budgets are
  still open.
- Cold full-clone response generation took 110.0 seconds for 1.24 GB; the
  warm clone reused the verified response-pack cache with 4.0 seconds of
  server operation. Depth-1/10/100/1,000 clone generation took
  10.0/11.7/101.0/116.9 seconds. These measurements establish the response
  pack and fanout bottleneck but do not satisfy the final SLO.

This smoke is current release-binary evidence for the compaction budget and
large-batch read path, not the full 1,000-push qualification. The independent
full-profile repeatability run, differential, concurrency, fault/provider,
owner-failover, and rollout gates remain open. The later regular post-CAS
budget change is release-built and covered by focused publication tests; the
current-head full-profile run below now covers it on the full replay path.

The current-head full-profile qualification
`codex-d8de9d12-k8s-20260827-current-full` used the release binary whose
embedded source revision is `d8de9d12`, the same Kubernetes revision, and
`replay_count=1000`. The standalone verifier returned `status=ok`; all 1,001
pushes and 23/23 checks passed. Advertised refs, full and incremental clone
tips, full and incremental fsck, and a deterministic 1,000-object sample
matched the source; the source checkout was unchanged and both local
worktrees and the run-owned remote prefix were cleaned. The correctness
fingerprint remained
`7d97627cf1f4de8b87679dea53d99916df42c3152dc765399d4494c43af09624`.

- The 1,000-push summary was 1.806 seconds median, 2.778 seconds p95, and
  3.433 seconds p99; the 259.613-second maximum was the initial import.
- The 1,000-push owner checkpoint took 976.987 seconds, reduced 902 active
  packs to 2, swept 901 stale pack-membership rows, and scanned/deleted zero
  canonical object rows. This confirms the intended O(N) initial catalog
  advance and indexed post-repack cleanup, but is not a sustained owner SLO.
- Cold and warm full clones completed in 160.5 and 57.4 seconds; blobless and
  depth-1/10/100/1,000 clones completed in 93.5/16.5/22.4/146.2/160.7
  seconds. Incremental fetches at 1/10/100/1,000 commits completed in
  1.860/2.093/3.002/6.685 seconds.

This is current-head full-profile correctness and bottleneck evidence on one
host, not production SLO proof. An independent current-head repeatability run,
valid isolated growth comparison, and the remaining team-load, fault,
provider, failover, canary, and rollout gates remain open below.

### Current-head owner-locator qualification after bounded maintenance hardening

Run profile: `codex-5ea595f6-k8s-20260827-current-full`, Kubernetes revision
`b3bc2ac58fa173967f27ade80f28cc5015b8c1c3`, isolated local RustFS, and the
release binary built from `5ea595f6`. The standalone verifier returned
`status=ok`, `profile=full`, and `replay_count=1000`; all 1,001 pushes and
23/23 checks passed. Advertised refs, full and incremental clone tips, full
and incremental fsck, and a deterministic 1,000-object sample matched the
source; the source checkout was unchanged and both local worktrees and the
run-owned remote prefix were cleaned. The correctness fingerprint remained
`7d97627cf1f4de8b87679dea53d99916df42c3152dc765399d4494c43af09624`.

- The generation-1,000 owner checkpoint fell from 976,987 ms on the prior
  `d8de9d12` current-head run to 409,251 ms here (2.39x faster, a 58% local
  single-host reduction). The owner repacked 902 active packs to 2, then the
  indexed stale sweep scanned and deleted 901 pack-membership rows while
  deleting zero canonical object rows. The routine sweep loaded 1,643,211
  existing ordinals in 2,598 ms; the pre-repack locator pass downloaded 900
  remote pack-evidence records in 1,754 ms and did not rebuild the catalog.
- Pushes remained bounded after the seed import: median/p95/p99 were
  1,610/2,782/3,242 ms across 1,001 pushes. Incremental fetches after
  1/10/100/1,000 commits took 363/505/1,024/2,369 ms of server operation
  time and transferred 22,589/454,861/5,805,626/39,894,943 response bytes.
- The remaining dominant read-path cost is response-pack construction. Cold
  full-clone pack generation took 106,674 ms for 1,241,023,010 response
  bytes; blobless, depth-1, depth-10, depth-100, and depth-1,000 generation
  took 65,962/6,657/10,412/98,258/101,290 ms. The warm full clone was a
  3,988 ms verified cache hit. These are measured local RustFS results, not
  production SLO proof; reusable filtered response packs, pack reuse, and
  sustained cache/fanout behavior remain open optimization work.

This run is the first current-head qualification after the bounded remote
evidence and in-memory ordinal-map changes. It strengthens the owner and
correctness evidence but does not close independent repeatability, valid
growth differentials, the 10,000-push/ancestry matrices, interruption and
provider faults, large-team concurrency, owner failover, or rollout gates.

The stale-pack membership-index change (`ad2554fa`, with the deterministic
delta-base regression in `b9859f28`) and the bounded owner-locator follow-up
(`5ea595f6`) are represented in the current-head 1,000-push report above. Its
final sweep reports only stale membership rows and does not scan the retained
OID catalog; the exact refs, fsck, and byte-equivalence checks still pass.
Repeated isolated runs and the full interruption/maintenance matrix remain
required.

### Current-head incremental reader-repair qualification

The reader-repair hardening is committed as `d85d7d15`. It addresses a
large-repository failure found by replaying a real commit against the existing
Kubernetes-sized Crab/RustFS remote `e2e-large-repository/crabbuild-team-load-pack-index-1d93e8a0`.
The remote contained the Kubernetes `b3bc2ac58fa173967f27ade80f28cc5015b8c1c3`
tip and a 1,643,211-object locator/catalog. A same-tree child commit was
created with `git commit-tree` and pushed as a one-object incremental update,
so the test isolated post-push metadata work from pack growth.

- Before the fix, the first fetch from the old tip spent 90,137 ms in
  reader-side locator publication. A one-object repair crossed the locator's
  compaction threshold, started a repository-sized SlateDB compactor, and then
  failed because the target catalog-bound visibility proof was still pending.
  The generic protocol session itself was not the bottleneck: a no-op fetch
  closed in about 0.56 seconds.
- `d85d7d15` gives reader repair a no-compaction locator writer and applies the
  existing target-digest-bound catalog handoff before the large-catalog
  materialization limit. Owner publication retains the compaction-aware path,
  so geometric locator maintenance remains on the repository generation owner.
- After the fix, the same fetch advanced from
  `b3bc2ac58fa173967f27ade80f28cc5015b8c1c3` to
  `7658d5ad745afa0e28b3a5dff20cae886e77d197` in about 2.9 seconds, published
  the 744,837-byte catalog proof, and passed `git fsck --full`; the new commit
  was readable by `git cat-file`. The trace showed the pending catalog handoff
  completing before protocol-v2 admission and no locator compaction wait.

This is a targeted current-binary regression qualification, not a substitute
for repeated full-profile or team-load evidence. The 1,000/10,000-push
differentials, interruption matrix, provider matrix, concurrent clone/cache
fanout, owner failover, and rollout gates remain open.

### Current-head locator fanout and compaction qualification

The locator fanout hardening is committed as ad93a23d, with the follow-up
compaction throughput correction in dd881d75. Reader and writer
lookup selectors now account for active SlateDB SST fan-out: sparse requests
keep exact OID gets, while dense requests use one bounded object-family scan
when that is cheaper. A scan cannot read beyond the catalog's ordinal row
bound; it falls back to exact gets if the bound is exceeded. Locator
compaction-aware writers also use one full L0 frontier with one compaction and
one subcompaction, using four bounded read-ahead fetch tasks. This prevents
repeated short-lived publishers from rewriting the same history through
several smaller jobs without serializing a repository-sized compaction behind
one remote block fetch at a time.

The exact release binary ad93a23d completed the RustFS smoke
codex-locator-scan-writer-ad93-20260827 with status=ok. The run used eight
independent-ref agents and four same-ref agents against isolated local RustFS;
it intentionally used --skip-fsck, so this is request-amplification evidence,
not a full correctness gate.

- Branch fan-out recorded 232 locator requests across 8 successful pushes,
  or 29.0 per success.
- Same-ref contention recorded 1,266 locator requests across 4 successful
  pushes, or 316.5 per success, below the 500-request regression budget.
  The writer log recorded bounded scan mode with 4 requested objects, 10 rows
  scanned, 48 catalog objects, and 11 active SSTs.

The policy and in-memory ordering regressions pass, and the result closes the
observed small-catalog/SST-fanout request spike for this workload. Repeated
isolated runs, full-profile growth, interruption, provider, failover, and
rollout evidence remain open.

The release binary dd881d75 completed the isolated RustFS Kubernetes smoke
codex-dd881d75-k8s-100-20260827 with 101 successful pushes, including the
generation-100 maintenance checkpoint. The previously failing single-fetch
configuration stopped at that checkpoint with a throttled error after 340.5
seconds; four bounded read-ahead fetch tasks completed the generation-100
compaction pass in 145.0 seconds and the complete owner stage in 301.9
seconds. Cold, warm, blobless, depth-1/10/100/1000 clones and full fsck all
passed, and the run-owned remote prefix was cleaned. The report is accepted
by the verifier with --allow-smoke; it is evidence for the maintenance
regression only, not the required 1,000-replay full-profile gate.

### Current-head strict qualification after catalog-visibility handoff repair

Run profile: `codex-8fa065f0-k8s-1000-20260828`, Kubernetes revision
`b3bc2ac58fa173967f27ade80f28cc5015b8c1c3`, isolated local RustFS, 1,000
first-parent replay pushes, and the release binary built from `8fa065f0`.
The standalone verifier returned `status=ok`, `profile=full`, and
`replay_count=1000`; all 1,001 pushes, 28/28 report stages, full/filtered/
shallow/incremental reads, full and incremental fsck, source immutability,
1,000-object byte-equivalence sampling, and run-scoped cleanup passed. The
correctness fingerprint remained
`7d97627cf1f4de8b87679dea53d99916df42c3152dc765399d4494c43af09624`.

- Pushes after the seed were bounded at 1,585 ms median, 2,932 ms p95, and
  3,408 ms p99; the 261,066 ms maximum was the initial import. Fetches at
  checkpoints 1/10/100/1,000 took 1,805/2,442/3,418/7,306 ms end to end.
- The generation-1,000 owner converged in 1,227,393 ms across ten passes,
  with 1,658,929,152 bytes peak child RSS, 138,235,644 maintenance bytes
  read, and 31,636,299 bytes written. Its actions were
  `ref_journal_compaction`, `catalog_advance`,
  `catalog_visibility_handoff`, `commit_graph_incremental`,
  `commit_graph_compaction`, `shallow_closure_rebuild`, `geometric_repack`,
  `catalog_advance`, `catalog_visibility_handoff`, and `none`.
- The 900-pack locator advance took 729,938 ms; the geometric repack of 991
  packs took 244,610 ms and the follow-up catalog advance took 129,151 ms,
  deleting 991 stale locator-pack rows. Serving inventory then converged to
  two active packs. The long locator/object-ordinal materialization remains
  the dominant owner bottleneck and is not an accepted large-team SLO.
- Cold and warm full clones completed in 163,503 and 57,016 ms; blobless and
  depth-1/10/100/1,000 clones completed in 25,612/17,359/135,988/176,986 ms.
  Incremental fetches at 1/10/100/1,000 commits completed in 1,805/2,442/
  3,418/7,306 ms, with the final fetch planning 38,745 logical objects.
- The object-store snapshot retained 1,003 physical pack objects after the
  run, while the active serving inventory was two packs. The extra objects
  are immutable manifest-history recovery roots, so they do not enter normal
  clone planning; history-prune and grace-aware repo GC must still be run to
  reclaim them when the recovery policy permits. This run therefore proves
  active-pack consolidation, not bounded long-term storage retention.
- The report verifier now treats `catalog_visibility_handoff` as a
  metadata-only proof transition and reserves visibility telemetry checks for
  an actual `visibility_repair`. This keeps strict report validation aligned
  with the owner contract without weakening the visibility-current gate.

This closes the current-head 1,000-replay correctness and active-pack
consolidation evidence for the PR. Independent repeatability, valid growth
differentials, 10,000-push ancestry, interruption/GC, provider, concurrency,
owner-failover, storage-retention, and rollout SLO gates remain open.

### Current-head planned locator lookup qualification

Commit `ba6311dc` seeds the locator writer's existing-ordinal lookup policy
from the already-known uncovered object-row bound when a bounded publication
writer opens. This makes the first large rebind eligible for one in-memory
ordinal scan before its initial write batches, instead of accumulating the
candidate threshold one batch at a time through exact remote lookups. The
writer regression `publication_hint_primes_existing_ordinals_before_first_rebind`
proves that a 4,096-object catalog takes the ordinal path before the first
replacement object is written; the full writer test module passes 27 tests with
one large-repository qualification stress test ignored by default.

The release binary from `ba6311dc` completed the Kubernetes smoke
`ba6311dc-k8s-100-20260828` against isolated local RustFS with 100 replayed
first-parent commits. The standalone verifier returned `status=ok`,
`profile=smoke`, and the same correctness fingerprint
`7d97627cf1f4de8b87679dea53d99916df42c3152dc765399d4494c43af09624` as the
current-head full run. Cold/warm full clones completed in 155,746/60,204 ms;
blobless and depth-1/10/100/1,000 clones completed in 22,670/16,033/21,750/
136,761/163,695 ms; and incremental fetches at 1/10/100 commits completed in
2,269/2,720/3,816 ms. All report stages, fsck checks, byte-equivalence
sampling, and run-scoped cleanup passed.

- The generation-100 owner converged in 275,636 ms across ten passes, with
  2,294,431,744 bytes peak child RSS, 38,946,586 maintenance bytes read, and
  8,996,304 bytes written. It completed the expected catalog, graph, shallow,
  and geometric-repack actions and reduced the serving inventory while
  deleting 94 stale locator-pack rows.
- This single smoke run is correctness evidence for the new lookup admission
  path, not a performance SLO closure: host-to-host variation prevents a
  timing claim against the earlier run. Independent repeatability, 10,000-push
  growth, interruption/GC, provider, concurrency, failover, retention, and
  rollout gates remain open pending the full-profile result below.

The exact-head release binary from `0bcd2f41` then completed the Kubernetes
full-profile qualification `0bcd2f41-k8s-1000-20260828` against the same
revision and isolated RustFS endpoint. The standalone verifier returned
`status=ok`, `profile=full`, and `replay_count=1000`; all 1,001 pushes, clone
and fetch stages, full and incremental fsck, deterministic 1,000-object
byte-equivalence sampling, source immutability, and run-scoped cleanup passed.
The correctness fingerprint remained
`7d97627cf1f4de8b87679dea53d99916df42c3152dc765399d4494c43af09624`.

- Compared with `codex-8fa065f0-k8s-1000-20260828`, the same-host full-profile
  comparison was valid and stayed within the 20% drift limit: push median
  drift was 0.38%, clone median drift 6.36%, and fetch median drift 7.33%.
  This closes the roadmap's core-operation repeatability differential for
  these two full reports.
- The generation-1,000 owner converged in 443,329 ms across ten passes, with
  1,033,715,712 bytes peak child RSS, 138,237,647 maintenance bytes read, and
  32,092,121 bytes written. The 900-pack catalog advance loaded 1,604,551
  existing ordinals in 2,370 ms and closed in 5.77 s; the prior run's
  equivalent pass took 729,938 ms. The full owner stage fell from 1,227,393
  ms to 443,329 ms, a 64% reduction, while the geometric repack remained the
  dominant 245 s pass and reduced 992 packs to 2 active serving packs.
- Full/warm clones completed in 163,503/60,645 ms; blobless and depth-1/10/
  100/1,000 clones completed in 24,481/16,109/22,871/136,248/173,643 ms; and
  incremental fetches at 1/10/100/1,000 commits completed in 1,777/2,263/
  3,373/6,581 ms. The report's physical snapshot still retained 1,003 pack
  objects after the run, so active-pack consolidation is proven but
  long-term history retention and reclamation remain separate gates.

This closes the diagnosed large-catalog locator lookup amplification and
provides same-host full-profile repeatability evidence for the PR. The
10,000-push growth, interruption/GC, provider, concurrency, owner-failover,
retention, and rollout SLO gates remain open.

### Current-head pack-source repack qualification

The pack-source hardening is committed as `f79fa1b9`, `fd468679`,
`022919d2`, and `e01fdf56`. Repack download now streams only the pack body
whose size is committed by the pinned manifest. The local Git worker rebuilds
the derived `.idx` and `.rev` once, verifies all selected source pack bodies
and indexes with one batched `git verify-pack` invocation, and then performs
the existing exact-object-set validation. Remote source indexes and
reverse indexes are no longer fetched merely to start a repack, and the
source body is no longer hashed once during download and again before
installation.

The worker deliberately does not use `git index-pack --fsck-objects` on each
partial suffix. A selected suffix can contain valid delta or attribute links
to stable packs outside the operation, so per-source object fsck would reject
valid repositories. Pack-local body/index validation is followed by exact
object-set comparison and the post-commit full/incremental Git fsck checks.
The body-only regression proves a missing remote `.idx` is not a repack input
requirement; the full qualification below proves the selected suffix behavior
with real cross-pack links.

Focused proof for the exact current source passes:

- `cargo test -p crab-git --lib pack --locked`: 31 passed;
- `cargo test -p crab-git --lib repack --locked`: 9 passed;
- `cargo test -p crab --lib cmd::repack::tests --locked`: 10 passed;
- release build with the binary provenance bound to `e01fdf56`; and
- `git diff --check`.

The exact-head full qualification `e01fdf56-k8s-1000-20260828` used the
Kubernetes revision `b3bc2ac58fa173967f27ade80f28cc5015b8c1c3`, isolated local
RustFS, 1,000 first-parent replay pushes, and the release binary whose
embedded source revision is `e01fdf56`. The standalone verifier returned
`status=ok`, `profile=full`, and 23/23 checks passed. Advertised refs, full
and incremental clone tips, full and incremental fsck, a deterministic
1,000-object byte-equivalence sample, source immutability, and run-scoped
cleanup all passed. The correctness fingerprint remained
`7d97627cf1f4de8b87679dea53d99916df42c3152dc765399d4494c43af09624`.

- Active serving packs were 1/2/2/92/2 at seed and replay checkpoints
  1/10/100/1,000. The final snapshot retained 1,003 immutable physical pack
  objects for manifest-history recovery while serving converged to two packs;
  this remains subject to the separate retention and grace-aware GC gates.
- The generation-1,000 owner converged in 471,124 ms across ten passes,
  processed an inventory of 992 packs, swept 991 stale pack-membership rows,
  and left two active serving packs,
  read 138,240,647 maintenance bytes, and peaked at 1,027,833,856 bytes of
  child RSS. The repack pass itself closed in about 265 seconds. This proves
  bounded source selection, cross-pack correctness, and active-pack
  consolidation, but it is not a measured wall-time improvement over the
  `0bcd2f41` baseline (443,329 ms owner / about 245 seconds repack); pack
  generation remains the dominant maintenance bottleneck.
- Full/warm clones completed in 160,360/58,845 ms; blobless and
  depth-1/10/100/1,000 clones completed in 25,745/15,583/21,957/137,555/
  161,303 ms; incremental fetches at 1/10/100/1,000 completed in
  1,826/2,258/3,395/6,835 ms. The default comparison against
  `0bcd2f41-k8s-1000-20260828` was valid and stayed within the 20% drift
  limit: clone/fetch/push medians changed by 2.97%/0.22%/0.44%.

This closes the redundant remote-index dependency and its cross-pack
validation regression on a real large repository. It does not claim a
repack-latency SLO win: the next performance slice should benchmark parallel
local index generation or pack-objects scheduling without weakening the
post-commit fsck boundary. The 10,000-push growth, interruption/GC, provider,
concurrency, owner-failover, retention, and rollout SLO gates remain open.

The current follow-up also bounds response-pack source acquisition to four
concurrent manifest-pack streams, reuses that downloader for both complete and
dense selected producers, restores manifest order before assembly, and records
`source_download_ms` separately from total pack-generation time. The existing
operation byte/request budgets and process-wide origin semaphore remain the
authoritative limits. The focused downloader regression and remote-Git suite
pass, but a new large-repository run is still required to quantify the
wall-time change on a provider with multiple large source packs.

The owner-side cold repair path now uses the same fixed four-way ceiling while
materializing committed packs for visibility, commit-graph, and shallow-closure
rebuilds. Each worker uses the manifest size as a streaming upper bound, checks
cancellation, verifies the BLAKE3 identity, and validates the installed Git
index before the temporary ODB is used. The aggregate request/byte budgets and
the single generation-owner lock remain unchanged; a multi-pack qualification
run is still required before claiming an owner wall-time SLO improvement.

### Current-head locator startup fanout qualification

Commit `b8c51985` adds two bounded startup paths for the Git object locator.
Each published locator coverage checkpoint now carries a compact binding
dictionary keyed by the catalog identity and next pack slot, so readers do not
scan the historical pack family across every active SlateDB SST. Binding,
stale-pack sweep, and catalog-reset mutations invalidate the snapshot before
publishing new state; legacy checkpoints fall back to the existing validated
scan, and malformed snapshots fail closed. A large push whose committed pack
inventory has a complete locally resolvable ref-tip frontier now uses those
tips before opening the locator; partial or legacy local inventories retain the
exact locator/index classification path.

The source regressions `published_pack_binding_snapshot_round_trips_in_slot_order`
and `large_locator_basis_prefers_local_manifest_ref_tips` cover identity,
ordering, invalidation boundaries, and the no-catalog-read fast path. Focused
source proof passes the metadata locator module (`54` passed, `1` ignored),
all Crab push tests (`269` passed, `1` ignored), and the complete remote-Git
suite (`92` unit tests plus `61` repository tests). The release binary was
built with `--locked --features gix-transport` and its embedded source
revision was verified.

The isolated RustFS contention run
`b8c51985-locator-ref-tip-only-concurrent-20260828` used four same-ref agents. It
completed with `status=ok`: 4/4 pushes integrated with zero integration
retries, the final protocol-v2 clone exposed all four files, and the measured
catalog request budget was `194.5` requests per successful push against the
`500` regression ceiling. The run still observed catalog SST reads during
remote-read session startup, so this is a bounded regression qualification,
not proof that catalog startup is constant-time for every repository shape.
Repeated full-profile comparisons, the 10,000-push growth/ancestry matrix,
provider/fault/failover/retention, sustained team fanout, and rollout SLO
gates remain open.

### Current-head shared-visibility and team-load qualification

The shared-object visibility admission fix is committed as `eb3ced6b`. It
counts distinct objects in the V5 visibility dictionary rather than summing
per-ref memberships, while retaining serialized-proof byte limits and
per-closure ordinal validation. The corresponding bounded owner loop now
allows a complete finite maintenance wave after one replay checkpoint, and
the RustFS verifier fetches and compares every advertised namespace.

The current-head smoke `pr96-eb3ced6b-k8s-team-smoke-20260827d` used Kubernetes
revision `b3bc2ac58fa173967f27ade80f28cc5015b8c1c3`, the release binary whose
embedded source revision is `eb3ced6b`, and an isolated local RustFS endpoint.
It completed with `status=ok`, 27/27 checks passed, 101 pushes completed, and
the run-owned remote prefix was cleaned. The verifier accepted the report with
`--allow-smoke`; the default full-profile verifier correctly rejects this
report because its 100 replay commits are a smoke profile, not the required
1,000-commit qualification. The correctness fingerprint remained
`7d97627cf1f4de8b87679dea53d99916df42c3152dc765399d4494c43af09624`.

- Full and warm clones completed in 157,897 ms and 45,093 ms; blobless and
  depth-1/10/100/1,000 clones completed in 16,457/6,163/14,844/115,879/
  137,991 ms. Every advertised ref matched the source, full and incremental
  fsck passed, and the deterministic 1,000-object sample was byte-identical.
- The 100-client incremental fetch fanout had 0 failures, with a 29,563 ms
  fanout duration and 3,663/7,570/9,057 ms median/p95/p99 client latency.
  The independent 20-writer fanout accepted all 20 pushes with no unexpected
  failures; the 20-writer same-ref race produced exactly one winner and 19
  push-lock rejections, with no unexpected failures.
- Generation-owner checkpoints converged after 100 replay pushes. The
  bounded maintenance pass reduced the active serving inventory to 2 packs;
  the checkpoint-100 owner completed in 79,069 ms after scanning/deleting 91
  stale pack-membership rows and reading 37,179,406 bytes. The final retained
  store snapshot contained 124 physical pack objects, which is immutable
  history/storage retention and not the active serving inventory.

This closes the previously observed shared-ref visibility false rejection and
adds real large-team correctness evidence. It also quantifies the remaining
read bottleneck: a full cold clone is still about 158 seconds and a 100-client
fetch p99 is about 9 seconds on this single local RustFS host. The 1,000-push
full profile, independent repeated run, valid growth differential, fault and
provider matrices, owner failover, and rollout gates remain open; this smoke
does not promote a production SLO.

### Current-head dense response-pack qualification

The dense response-pack follow-up is committed as `b69ecb47`. It centralizes
the catalog-exact filter predicate in `UploadPackFilter::is_catalog_exact()`
and routes only no-have, non-shallow `blob:none`, `object:type`, or their
kind-only combinations to the verified packed-entry assembler. The assembler
keeps REF_DELTA payloads when the base is selected, rewrites OFS_DELTA to
REF_DELTA, materializes a base only when it is outside the selected set, and
retains the existing full-pack consolidation and selected-object repack paths
for full, shallow, depth, path-context, and negotiated requests.

The live RustFS smoke `codex-dense-pack-final-k8s-20260827-smoke` used
Kubernetes revision `b3bc2ac58fa173967f27ade80f28cc5015b8c1c3`, Git 2.50.1,
and the release binary built from the implementation worktree at
`113fce69`. The shared filter-policy change was already present in that build;
the subsequent `b69ecb47` diff only renamed the internal selection parameter
and added the final strict-pack regression. Its standalone report has
`status=ok`, 17/17 checks passed, full and incremental fsck passed,
the deterministic 1,000-object sample remained byte-identical, the source
checkout was unchanged, and the run-owned remote prefix was cleaned. The
correctness fingerprint remained
`7d97627cf1f4de8b87679dea53d99916df42c3152dc765399d4494c43af09624`.

- The unfiltered cold clone used the existing `complete_pack_consolidation`
  strategy: 2 origin reads, 1,244,177,064 response bytes, and 117,295 ms of
  server-side pack generation. Depth-100 and depth-1,000 also stayed on the
  existing selected-repack/complete-consolidation paths with 2 origin reads.
- The `blob:none` clone used `selected_packed_entries`: 1,102,159 selected
  objects, 211,026,080 response bytes, 6,322 ms of pack generation, zero
  inflated bytes, 3,082 bounded range reads, and 444,376,175 fetched bytes.
  Its client-side clone and fsck checks passed. Compared with the prior
  selected-object repack measurement (65,962 ms and 198,749,524 response
  bytes), this is a large CPU reduction with a measured response-size and
  range-read trade-off that still needs provider/SLO qualification.
- A ranked experiment rejected broad direct assembly for full clones: the
  same two-pack snapshot produced a correct 1,263,644,281-byte response but
  needed 155,695 range reads and 8,270,537,029 fetched bytes at essentially
  the same 106-second generation cost. That candidate was removed before the
  final smoke, so full clones retain the low-request consolidation path.

Focused proof after the final source change passes 19 `crab-remote-git` pack
tests, including strict forward REF_DELTA and OFS_DELTA rewriting, 24
`crab-read` upload-pack tests, and 27 Crab upload-pack-wire tests. The smoke
report is correctness evidence rather than a full Phase 2 performance gate:
repeatability, full-profile response-pack SLOs, provider range-request
behavior, and the remaining roadmap phases are still open.

The generated-pack cache follow-up is committed as `fd7e6121`. Cache descriptor
loads now use the storage layer's bounded single GET, which checks the provider
advertised size before consuming the body and preserves the existing 4 KiB
corruption boundary. This removes the separate descriptor HEAD from both cache
hits and misses; the remote-repository regression asserts one descriptor GET
for a warm lookup, while the artifact checksum, response limit, and corruption
tests remain unchanged. It is a request-amplification fix, not a substitute
for the still-required response-pack SLO and provider-range qualification.

The assembled-pack follow-up is committed as `d50cbd89`. `PackWriter` already
updates the Git SHA-1 and content hash on every bounded write, so its finish
path no longer rereads the complete temporary response solely to recompute
those values. The new checksum regression recomputes both digests in the test
and strict-pack coverage remains in place; externally sourced repack and cache
artifacts continue to use independent full-file verification. This removes a
second full disk pass from sparse/dense response assembly without weakening
the response-size, cancellation, or downstream Git integrity boundaries.

The complete-response consolidation follow-up now feeds Git's verified source
pack basenames through `pack-objects --stdin-packs`. This removes the
repository-sized temporary OID-list serialization from the cold full-clone
producer while retaining source-pack validation and the caller's exact
authorized-object-set check. The minimum supported Git compatibility matrix
must continue to cover this mode; a current-head Kubernetes measurement is
required before claiming the response-pack SLO is closed.

The generated response-pack cache key now includes the descriptor format
version as part of its domain. Older derived descriptors therefore cannot
collide with the current request namespace and force a false corruption result;
they miss and regenerate under the current descriptor contract.

The upload-pack admission boundary is now explicit: capability discovery reads
the manifest, active ref-journal marker presence, and generation-owner lease
without mutating derived state. It withholds protocol-v2 while an active marker is
protected by a live generation owner, so Git can use the journal-backed ordinary
ref advertisement instead of entering a terminal session that must fail closed
on mixed-generation locator state. An orphaned marker remains eligible for
protocol-v2 because terminal upload-pack admission can compact it under the
manifest lock; admission then repairs the current locator and catalog-bound
visibility proof before serving filtered fetches. Owner-probe and admission
errors fail closed. This closes the capability-to-admission crash recovery gap
found by the released-shape RustFS lifecycle while preserving the push
acknowledgement boundary.

The reader-fanout follow-up is committed as `fd95e8fd`. Protocol-v2
upload-pack sessions now hold one of 16 repository-scoped object-store read
leases for the session lifetime, renew the lease, cancel on renewal failure,
and release it on every terminal path. Contended readers rotate and jitter a
single slot per attempt, so admission is bounded by repository lease capacity
instead of creating a metadata storm. Reader-side locator, visibility, and
ref-journal repairs use non-blocking writer-lock probes; the owner path keeps
the existing blocking serialization. The lease is deliberately internal and
fixed-size: normal completion/error/cancellation releases it, while a crashed
helper leaves only a bounded TTL lease for backend-clock reclaim.

The exact post-commit RustFS team-load smoke
`crabbuild-team-load-smoke-fd95` used the 100-commit synthetic fixture and the
release binary whose embedded source revision is `fd95e8fd`. The standalone
report verifier returned `status=ok` with 27 checks and 909 recorded commands:

- 100/100 seed clones succeeded; total 26,853 ms, median client 20,949 ms,
  p95 25,384 ms, p99 26,366 ms;
- 100/100 concurrent incremental fetches succeeded; total 13,580 ms, median
  client 6,933 ms, p95 11,913 ms, p99 12,924 ms;
- 20/20 independent-ref pushes succeeded; total 5,323 ms, median client
  2,034 ms, p95 3,623 ms, p99 5,303 ms;
- same-ref contention produced exactly 1 success and 19 typed lock rejections;
  total 785 ms, median client 378 ms, p95 617 ms, p99 773 ms, with zero
  unexpected failures.

This is current-binary smoke evidence, not completion of the full Kubernetes
gate. The full Crab suite passed (3,745 library tests, 49 binary tests, and
all enabled integration suites) with the documented 32 MiB macOS test-stack
workaround; coordination passed 93/93 and the release build passed. Full
Kubernetes repeatability, the 1,000-push differential, 10,000 ancestry and
shallow differential proof, provider/fault/failover/canary evidence, and the
remaining owner O(N) budgets remain open below.

The active-marker recovery path is now complete for the discovered crash
boundary: compaction promotes any prepared ref heads after the manifest CAS,
retains the marker when that CAS repair fails, and releases the exact recorded
ref-lock holder after successful compaction. The metadata regression and Crab
upload-pack admission regression both exercise the prepared-head state left by
process death. The first released-shape run after that work exposed one more
shared-dictionary invariant: an incremental edit resized current-ref and
history bitmaps but left transition bitmaps for unrelated refs at their old
length. `01d588ea` resizes every retained transition bitmap when the dictionary
grows and adds a regression that binds the repaired index. The fresh
[released-shape RustFS workflow](https://github.com/crabbuild/crab-oss/actions/runs/32917566230)
passed the real-Git lifecycle, response-loss/crash-recovery lifecycle, and all
Git 2.30/2.40/2.45/current compatibility jobs on that fix.

Implemented on the current branch (pre-lazy qualification evidence at
`04655f3b`; latest admission hardening at `0ba86693`; qualification-contract
fix at `0a8e4aa8`; capability-admission fix at `3bd7a02b`; filtered-fetch
recovery fix at `be27f458`; active-marker recovery fix at `73ef4035`;
transition-bitmap fix at `01d588ea`; lazy catalog follow-up at `cbe848f4`;
reader-fanout hardening at `fd95e8fd`; owner compaction budgeting at
`4a8fc34e`; regular locator budgeting at `88deb4e0`; stale owner-plan guard at
`5465031e`; small-catalog point-read policy at `62d35c14`):

- Phase 0 qualification/report tooling and scheduled/manual workflow;
- bitmap-native visibility planning and bounded transfer admission;
- delta-preserving response assembly and generation-bound pack caching;
- a generation-bound object catalog used as the visibility identity universe;
- a complete, versioned split commit graph with append and geometric compaction;
- one bounded generation-owner maintenance action per cycle;
- selected-suffix geometric repack that leaves stable large packs untouched;
- generation-bound graph/catalog/visibility publication after push and repack;
- repository GC classification for active, retained-history, grace-period, and
  collectible pack storage;
- LFS dependency publication bounded to newly introduced history and
  pointer-sized blobs, including partial-clone push proof;
- cold/warm clone fanout controls in the qualification harness.
- SlateDB 0.15.0 cancellation-safe reader behavior and explicit initialization
  of temporary bare Git repositories before pack/index operations;
- catalog-exact dense-filter response assembly from verified packed entries for
  `blob:none`/`object:type`, with exact generated-object-set verification and
  conservative repack fallback for contextual requests;
- safe OID deduplication for initial absolute-depth traversal, preserving
  context-sensitive behavior for relative deepening and existing shallow
  boundaries;
- a generation-bound shallow-closure descriptor with content-addressed,
  depth-1/10/100/1,000 object selections and Git-derived shallow boundaries;
- legacy remote-helper integration for fresh single-tip absolute-depth clones,
  with conservative fallback for relative deepen, filtered, multi-tip, and
  already-shallow requests;
- generated-pack cache descriptors that distinguish requested object count
  from the larger self-contained pack count required by delta bases.
- repository GC now resolves recent generated-pack descriptors with bounded
  list-concurrency and streams validated descriptor/artifact roots without
  accumulating the cache namespace in memory.
- an immutable catalog-readiness marker that makes healthy admission checks
  metadata-only while preserving generation and validation-digest binding;
- one catalog-bound ordinal-proof handoff across protocol-v2 upload-pack, exact
  shallow-fetch, promisor-fetch, and legacy remote-helper paths, removing the
  full OID dictionary materialization from normal helper admission;
- catalog ordinal scan read-ahead and bounded fetch parallelism for dense
  selected-object resolution;
- a locator lookup policy that retains exact reads for sparse requests and
  switches dense or SST-fanout-amplified waves to one bounded, read-ahead scan,
  including small compacted catalogs when the request density justifies it.
- locator compaction uses one full L0 frontier and one bounded compaction and
  subcompaction worker with four read-ahead fetch tasks, so short-lived
  publishers do not repeatedly compact overlapping history or serialize a
  repository-sized compaction behind one remote block fetch.
- owner locator planning serialized under the repository publication lock,
  with uncovered-pack budgeting and a metadata-only writer for unchanged
  inventories.
- regular post-CAS locator publication reuses the same lock-scoped uncovered
  pack budget, so stable inventory does not inflate push-side compaction.
- owner locator planning rechecks its manifest anchor after lock acquisition,
  skipping stale repository snapshots before locator session or pack planning
  work begins.
- small locator catalogs retain bounded OID point reads; full ordinal scans are
  reserved for large dense batches on catalogs with at least 4,096 objects.
- generated response-pack cache publication trusts the private verified-pack
  invariant, avoiding a second repository-sized hash scan before multipart
  upload on every cold cache miss.
- generated response-pack cache selection identity is canonicalized by sorted
  OIDs, so equivalent large-team dense requests coalesce despite traversal
  order differences.
- LFS pre-push range scans exclude every compacted manifest ref tip, not only
  refs listed in the current stdin batch, so multi-branch pushes do not
  rescan already-published pointer history.
- full-clone qualification explicitly fetches all advertised namespaces into
  a verification ref namespace and compares every resulting ref to the
  remote advertisement, including annotated-tag peeled values.
- long fast-forward visibility history retained across a bounded 1,000-edit
  window, so incremental planning does not lose old haves after 64 cumulative
  transitions;
- deterministic sorting and deduplication of visibility delta positions,
  plus checkpoint flush ordering that publishes dirty locator rows before
  readers or manifest checks can observe the checkpoint;
- sparse response-pack delta-base prefetching in locator batches, bounded range
  coalescing, local verified reconstruction, and shared decode admission;
- conservative `include-tags` handling for exact shallow closures: lightweight
  tags may use the index, while annotated or incomplete tag state falls back to
  the canonical planner.
- locator publication before stale-pack sweep, preserving the dense object
  catalog across pure repacks and rebuilding it only when the object universe
  actually loses rows;
- qualification-verifier coverage for abbreviated build revisions, with the
  acceptance rule shared by the live harness and the standalone verifier.
- protocol-v2 qualification now settles the expected post-push admission repair
  before taking the filter-matrix baseline, keeping the steady-state
  read-only remote assertion strict.
- active-marker compaction repairs prepared ref heads before marker cleanup and
  releases the committed holder with a holder-checked ref-lock CAS, so a
  process death after the durable ref boundary does not strand the next push.
- visibility edits resize bitmap closures in refs, incremental history, and
  retained transitions together whenever the shared object dictionary grows,
  so validation cannot observe a shorter transition bitmap after an unrelated
  ref update.
- generation-owner cycles execute at most one derived-state action, and owner
  samples identify the action reason and next eligibility for operational
  scheduling.
- protocol-v2 upload-pack sessions use bounded repository-scoped read
  admission with renewal, cancellation, crash-reclaimable TTL leases, and
  non-blocking reader-side repair probes.
- stale locator-pack cleanup uses an atomic derived pack-slot membership index;
  marker-less or interrupted catalogs rebuild it once, while routine sweeps
  avoid scanning retained OID rows. Rebuild completion is count-checked before
  coverage can advance.
- the workflow scheduler lock retains its diagnostic inode across handoffs,
  preventing an earlier holder from unlinking the pathname after a waiter has
  acquired it; the focused handoff regression is committed as `d9c93263`.

### PR #87 push-admission follow-up

The released-shape PR workflow `gha-33086829129-1-concurrent` failed only the
`same-branch-locator-request-budget` check. Reproducing the exact four-agent
same-ref workload locally recorded 2,028 locator-catalog requests for four
successful pushes, including 1,443 compacted catalog reads. The request spike
was in the short-lived push process: post-CAS locator publication and its
catalog-bound readiness check reopened the repository-sized SlateDB catalog on
each incremental push.

The follow-up moves that work to the existing generation-owner boundary:

- ref-journal admission publishes only an immutable digest-bound V4 visibility
  proof and uses the catalog identity/checkpoint marker for a metadata-only
  readiness check;
- once that V4 proof exists, later admission probes only its bounded immutable
  object with a HEAD request, avoiding another remote-pack walk while the
  generation owner upgrades the state to V5; the regression also proves the
  repeated admission path performs no pack reads;
- a normal committed push no longer opens the locator catalog, publishes
  locator rows, or writes a generation receipt after the active marker;
- the generation owner retains compaction-aware locator publication and
  catalog-bound V5 visibility repair, while reader repair remains bounded and
  uses the existing complete-pack fallback for oversized repositories;
- the initial bounded-pack E2E test now asserts the authoritative pack/index
  publication, and focused owner/repair tests prove that deferred locator
  coverage remains repairable.

The released Crab binary passed the exact RustFS lifecycle locally as
`codex-pr87-owner-deferred-20260827`: 1,001 pushes, 23/23 checks, all crash,
marker-fault, branch-fanout, same-ref integration, and FSCK checks passed. The
four same-ref pushes observed 483.75 locator requests per successful push
against the 500-request budget, and the final protocol-v2 clone saw all four
same-ref files. Focused and broad proof also passed: Crab library tests
`3752 passed, 0 failed, 2 ignored`, `crab-remote-git` tests `89 + 61 passed`,
`crab-metadata` remote-index/storage tests `258 passed, 0 failed, 1 ignored`,
and the qualification verifier/smoke harness tests `35 passed`.

This closes the observed PR admission regression, but it does not close the
remaining roadmap gates below: independent Kubernetes repeatability, valid
1,000-push and 10,000-ancestry differentials, sustained owner/rebuild and
response-pack SLOs, provider/fault/failover/canary evidence, and the final
catalog/readiness handoff proof.

Still required before the roadmap is DONE:

- an independent repeatability full-profile report from the current binary
  after the stale-owner and small-catalog changes. The current
  `codex-d8de9d12-k8s-20260827-current-full` is the first current-head
  full-profile run; the older `f12e2d9e` run is not a repeat of this source;
- sustained owner-budget, interruption, and memory evidence after `4a8fc34e`,
  `88deb4e0`, and the current locator hardening. The current full run proves
  the 902-to-2 pack transition and 901-row indexed sweep once, but the
  intentional O(N) rebuild path and repeated owner behavior still need bounded
  multi-run evidence;
- a valid 1,000-push growth and latency comparison across isolated runs; the
  current post-lazy comparison is invalid because push and clone medians drifted
  by roughly 41% on the shared host;
- 10,000 deterministic Kubernetes ancestry pairs and depth-1/10/100/1,000
  shallow differential proof;
- full shallow differential proof and the final response-pack SLO report. The
  exact closure index now handles all four measured depths, and sparse
  delta-base prefetch reduced depth-1 from 36,702 to 7,388 origin requests and
  depth-10 from 41,493 to 7,502 in focused runs. Depth-100/1,000 currently use
  the dense selected/complete repack path and pass correctness, but their
  differential and sustained SLO evidence is still required;
- the complete Phase 5 interruption and 10,000-push maintenance matrix;
- concurrent fetch/push, cache-server fanout, throttling, and owner-failover
  scenarios from Phase 6;
- supported-provider compatibility, sustained canary, and default-on rollout.
- the owner report's remaining `repair_required` state must be explained and
  cleared for repository-local acceleration: generation receipts, repository
  registry coverage, and derived-index coverage still need valid evidence.
  Bucket-wide registry discovery is now a separate diagnostic state and does
  not by itself require repo-scoped repair; destructive bucket GC remains
  disabled until its independent completeness gate is proven.
- the post-`cbe848f4` Kubernetes qualification proves that normal protocol-v2
  and legacy helper admission emits no `catalog_materialization` event. The
  owner publication path now reads the immutable `.kinds` sidecar instead of
  downloading new pack bodies when only locator rows are needed, but
  intentional O(N) owner repair/rebuild, legacy/invalid sidecar repair,
  migration/compaction, graph, and repack paths still have latency and memory
  budgets open;
- the post-lazy depth-1/10 planner measured 11,659/15,553 ms before the
  large-batch scan change. The current full run shows depth-1/10 planner
  visibility in 9/13 ms and zero locator ordinal scans for those depth clones;
  rerun the differential with the current source and require lookup-mode
  telemetry to prove the change without increasing full-clone or
  incremental-fetch latency;
- catalog-filter planning now reads the additive ordinal-keyed metadata
  sidecar, filters ordinals, and resolves only retained OIDs. Existing or
  incomplete sidecars still use the bounded canonical fallback; the current
  full run supplies fresh current-binary evidence, but the large-closure
  request/latency SLO still needs differential and sustained proof;
- cold and warm full-clone response-pack SLOs remain open: the Kubernetes
  repository still generates a roughly 1.2 GB response pack, so cache hits and
  pack-count bounds alone do not prove large-team clone fanout is affordable;
- provider-specific range-request, interruption, retry, cache-server fanout,
  owner failover, and sustained canary evidence remain required for every
  supported object-store backend.
- a live capability-to-admission handoff while a long-running generation owner
  holds the lease still needs explicit latency/failover evidence; the current
  path fails closed rather than serving a mixed-generation snapshot.

## Outcome

Crab will support repositories with Kubernetes-scale history and sustained
large-team activity without clone, fetch, push, or maintenance cost growing
linearly with total commits or accumulated packs. Correctness remains exact:
authorization is fail-closed, reconstructed Git objects are byte-identical,
ref updates remain lock-then-push serialized, and GC never removes a referenced
or grace-period object.

The target architecture follows the public Git/GitHub large-repository model:

1. Geometrically sized immutable packs keep physical pack count logarithmic.
2. One logical object catalog maps OIDs to pack locations across all packs.
3. Reachability bitmaps answer authorization and `wants - haves` as set
   operations instead of full object walks.
4. A split commit graph accelerates ancestry and shallow-boundary queries.
5. Delta-preserving response packs avoid inflating and recompressing complete
   repositories for every clone.
6. Lease-owned background maintenance compacts recent data and metadata.
7. Generated artifacts and hot object ranges are cached for team fanout.

Crab borrows the algorithms and ownership boundaries, not GitHub's deployment
topology. GitHub can serve from managed repository hosts and replicas; Crab
keeps immutable Git data in object storage and uses the existing repository
generation owner plus cache service. Git's MIDX becomes Crab's generation-bound
object catalog, multi-pack bitmaps become catalog-ordinal visibility closures,
split commit graphs remain immutable metadata layers, geometric repack becomes
selected-suffix object-store compaction, and cruft/limbo safety becomes retained
manifest history plus grace-period GC classification.

Public technical references:

- GitHub geometric repack, MIDX, and multi-pack bitmap deployment:
  https://github.blog/open-source/git/scaling-monorepo-maintenance/
- Git reachability bitmap negotiation:
  https://github.blog/open-source/git/gits-database-internals-iv-distributed-synchronization/
- Git 2.55 geometric incremental MIDX maintenance:
  https://github.blog/open-source/git/highlights-from-git-2-55/
- Git commit-graph contract:
  https://git-scm.com/docs/git-commit-graph
- GitHub cruft-pack GC model:
  https://github.blog/engineering/architecture-optimization/scaling-gits-garbage-collection/

## Repository rules and invariants

Every executor must read `AGENTS.md`. Before modifying any shared crate, also
read `crates/AGENTS.md`. Preserve these repository-wide invariants:

- All SlateDB instances close on every exit path.
- Per-ref locking precedes ref publication; every acquired lock is released.
- GC keeps every referenced object and every object in the grace period.
- Object reconstruction is byte-identical or returns an error.
- Staged xorbs flush before bundle publication.
- Public errors use typed `CrabError`/crate error enums; do not stringify and
  discard sources.
- Production Rust has no `unwrap()`, `expect()`, `panic!`, `todo!`, or
  `unimplemented!`.
- Do not add compatibility fallbacks unless a tagged public contract is named,
  tested, documented, and given a removal or migration plan.
- Do not add configuration or environment variables until existing defaults,
  maintenance policy, and `doctor` cannot solve the requirement.
- Never run `crab gc --scope=bucket` during qualification.

Crab supports Git 2.30 through 2.50+ according to
`crab/docs/design/technical-design.md:2254`. New behavior must not silently
require Git 2.55.

## Baseline state at planning commit `aa150868`

### Geometric pack maintenance exists

`crates/crab-git/src/repack.rs:125` invokes Git's geometric policy:

```rust
Command::new("git")
    .arg(format!("--git-dir={}", source_git.display()))
    .arg("repack")
    .arg("-q")
    .arg("-d")
    .arg("-g")
    .arg("2")
```

`crab/src/cmd/repack.rs:139` downloads the pinned inventory, validates it,
uploads only newly generated packs, commits one CAS generation, rebinds the
visibility proof, and publishes exact locators. Repack is manual;
`crab/src/git/remote_helper.rs:2547` only emits an advisory threshold warning.

### The locator is Crab's object-store-native MIDX analogue

`crates/crab-metadata/src/git_object_locator/mod.rs:14` stores exact pack
identity, offset, packed entry length, and CRC for an OID. Pack slots are
durable and bound to immutable packs. `crab/src/git/push.rs:5690` now avoids
rescanning retained packs whose locator rows are already covered.

### Visibility is compressed only at rest

`crates/crab-metadata/src/git_visibility.rs:177` exposes complete per-ref
closures as `BTreeMap<String, Vec<String>>`. Persistence deduplicates OIDs into
one dictionary and chooses sparse positions or a bitmap per ref at
`crates/crab-metadata/src/git_visibility.rs:438`. Deserialization expands the
closures back into duplicated OID strings at
`crates/crab-metadata/src/git_visibility.rs:508`.

`crates/crab-read/src/upload_pack.rs:623` materializes a `BTreeSet<String>` for
a full-ref closure. Constrained fetches traverse objects in batches at
`crates/crab-read/src/upload_pack.rs:398`. The Kubernetes qualification showed
approximately 1.6 million proof objects, above the 100,000-object synchronous
profile at `crates/crab-metadata/src/git_visibility.rs:26`.

### Response packs discard stored deltas

`crates/crab-remote-git/src/reader.rs:330` already batches locator lookups and
`crates/crab-remote-git/src/reader.rs:1044` coalesces adjacent pack ranges.
However, `crates/crab-remote-git/src/pack.rs:321` writes each selected object as
a complete commit, tree, blob, or tag and zlib-compresses it. It emits no
OFS_DELTA or REF_DELTA entries, so protocol-v2 response construction repeats
inflation and compression and loses stored delta efficiency.

### The commit graph is deliberately bounded

`crates/crab-metadata/src/commit_graph.rs:24` caps the current summary at 10,000
commits and retains a 1,000-generation window. It is an acceleration structure,
not complete Kubernetes-scale history.

### A background ownership boundary already exists

`crab/src/cmd/metadb.rs:569` runs a repository-scoped generation owner. It
advances locator coverage and repairs visibility at
`crab/src/cmd/metadb.rs:620`. This is the correct owner boundary for additional
repository maintenance; do not create a second daemon or scheduler.

Cold visibility repair bulk-materializes each unique immutable pack into a
temporary local ODB and uses the same bounded reachability walk as push,
recovery, and repack. This avoids object-store request amplification during a
full rebuild. Incremental pushes continue to publish visibility edits instead
of repeating the cold path.

## Common commands

Use a unique target directory for this roadmap's checkout. Never fall back to
a local `target/` directory.

| Purpose | Command | Expected result |
|---|---|---|
| Volume check | `test -d /Volumes/Workspace && test -w /Volumes/Workspace` | exit 0 |
| Format | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap cargo fmt --all -- --check` | exit 0, no diff |
| Shared-crate tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap cargo test -p crab-metadata -p crab-read -p crab-remote-git --locked` | all pass |
| Crab tests | `cd crab && CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap make test` | all pass |
| Clippy | `cd crab && CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap make clippy` | exit 0 |
| Full workspace tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap cargo test --workspace --locked` | all pass |

If `/Volumes/Workspace` is unavailable, stop. Do not compile locally.

## Global scope boundaries

**In scope across the roadmap:**

- `crates/crab-metadata/src/git_visibility.rs`
- `crates/crab-metadata/src/git_object_locator/`
- `crates/crab-metadata/src/commit_graph.rs`
- `crates/crab-read/src/upload_pack.rs`
- `crates/crab-remote-git/src/reader.rs`
- `crates/crab-remote-git/src/pack.rs`
- `crates/crab-remote-git/src/commit_graph.rs`
- `crab/src/git/upload_pack_wire.rs`
- `crab/src/git/push.rs`
- `crab/src/lfs/publication.rs`
- `crab/src/cmd/lfs/push.rs`
- `crab/src/cmd/metadb.rs`
- `crab/src/cmd/repack.rs`
- repository-scoped GC code under `crab/src/cmd/gc/` when Phase 5 begins
- focused qualification scripts under `crab/scripts/e2e/`
- dedicated manual/scheduled qualification workflow under `.github/workflows/`
- docs directly describing changed behavior

**Out of scope:**

- Xorb chunking, deduplication, hydration, or VFS redesign.
- Bucket-wide garbage collection.
- Changes to ref authorization or hidden-ref semantics.
- Changing the Git wire protocol's observable correctness.
- Replacing S3/RustFS with a Git data server.
- Copying Git/GitHub source code into Crab.
- Raising the minimum Git version.
- Adding UI or marketing features.
- Adding broad runtime fallbacks to hide stale or corrupt metadata.

## Git workflow

- Use one branch per phase: `codex/large-repo-phase-0-baseline`, then
  `codex/large-repo-phase-1-bitmaps`, and so on.
- Rebase each branch on current `origin/main` before its final push.
- Use concise conventional commits such as
  `perf(fetch): plan object closures with bitmaps`.
- Each PR must include current behavior, changed owner boundary, tests, the
  Kubernetes/RustFS report, and explicit consideration of whether the change
  is the best fix rather than merely a plausible fix.
- Do not push or open a PR unless the operator requests it.

## Phase 0: Establish the qualification and SLO contract

### Background

The prior Kubernetes replay proved that active pack accumulation matters:
geometric compaction reduced raw clone time from approximately 459.6 seconds
to 101.1 seconds. It also exposed visibility work over roughly 1.6 million
objects and substantial temporary/storage amplification. These observations
must become reproducible evidence, not one-off terminal measurements.

No later phase may claim success without comparing the same operation mix on
the same source revision and RustFS topology.

### Deliverables

1. Add an opt-in qualification driver under
   `crab/scripts/e2e/run_large_repo_rustfs.py`. It must:
   - find Kubernetes first at
     `/Volumes/Workspace/Github/kubernetes/kubernetes`;
   - treat that checkout as read-only;
   - place mirrors, replay worktrees, reports, and temporary files under a
     task-specific `/Volumes/Workspace/CrabBuild/crabbuild-qualification/`
     directory;
   - use an isolated RustFS bucket/prefix and delete only the prefix created by
     the run;
   - never print credentials or environment values;
   - record source revision and all Crab/Git versions.
2. Seed the remote at `HEAD~1000`, replay the final 1,000 first-parent commits
   as individual pushes, and preserve per-push latency. If Kubernetes has fewer
   than 1,000 first-parent commits at the pinned revision, stop and report.
3. Measure at minimum:
   - initial import;
   - cold and warm full clone;
   - `--filter=blob:none` clone through protocol v2;
   - `--depth=1` and `--depth=100` clone;
   - incremental fetch after 1, 10, 100, and 1,000 pushes;
   - one-commit push latency;
   - active pack count/bytes and physical bucket bytes;
   - visibility build/read/plan time and peak RSS;
   - locator lookup time, object-store request count/bytes, pack-generation
     CPU time, response bytes, and cache hits.
4. Add a versioned JSON report schema and a read-only verifier script. Follow
   the structured verification style in
   `crab/scripts/verify-cache-service-smoke-report.py`.
5. Add a manual/scheduled workflow that runs only on a suitably provisioned
   runner. Do not put the full Kubernetes replay on ordinary pull-request CI.
6. Document the command, prerequisites, isolation rules, and report fields.

### Phase 0 acceptance criteria

- [ ] The source and final clone agree on every advertised ref.
- [ ] `git fsck --full` exits 0 in the full clone and the incremental-fetch
      checkout.
- [ ] A deterministic sample of at least 1,000 source objects has identical
      `git cat-file` type, size, and SHA-1 in source and clone.
- [ ] The report contains all required timings, counts, byte totals, versions,
      source revision, Crab manifest generation, and pack inventory hash.
- [ ] The report verifier rejects missing fields, negative durations,
      inconsistent ref/object counts, and failed correctness checks.
- [ ] Two consecutive runs on the same host complete with identical
      correctness output; their median operation times differ by no more than
      20%, or the report identifies host-level contention and is marked
      invalid rather than silently accepted.
- [ ] Normal PR CI does not download Kubernetes or require cloud credentials.
- [ ] No production behavior changes in this phase.

### Phase 0 verification

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap \
  cargo test -p crab-remote-git --example qualify_remote --locked
python3 crab/scripts/verify-large-repo-rustfs-report.py <report.json>
git diff --check
```

Expected: all commands exit 0; the verifier prints a one-line success summary
without credential or environment contents.

### Phase 0 STOP conditions

- RustFS cannot provide isolated repository prefixes or deterministic cleanup.
- The existing Kubernetes checkout would need modification, reset, or clean.
- Correctness differs before any performance change; diagnose that separately.
- The harness requires a bucket-wide destructive operation.

## Phase 1: Make reachability bitmaps the runtime model

### Background

Crab's version-3 visibility proof is already dictionary/bitmap-compressed at
rest, but `into_index` expands every membership back to a cloned hexadecimal
OID string. Full clone planning merges these lists into another tree set.
GitHub avoids this cost by assigning objects dense positions and evaluating
reachability unions and differences as compressed bitmaps.

This phase changes representation and algorithms only. Authorization remains
generation-bound and fail-closed. A stale or absent exact proof must not admit
arbitrary OIDs.

### Deliverables

1. Replace the runtime `BTreeMap<String, Vec<String>>` closure representation
   with:
   - one validated binary OID dictionary;
   - one lookup from OID to ordinal;
   - one sparse-or-bitmap closure per ref;
   - zero duplicated OID ownership per ref.
2. Preserve the current version-3 persisted identity fields: generation,
   pack-index hash, and Git validation digest. Existing version-1 reads are a
   tagged upgrade contract; keep them only at the storage/migration boundary
   and normalize immediately into the canonical runtime representation.
3. Provide narrow APIs for:
   - membership across a supplied visible-ref set;
   - union of visible-ref closures;
   - union/difference of want and have reachability;
   - deterministic iteration of selected OIDs;
   - object counts without materializing strings.
4. Update `crates/crab-read/src/upload_pack.rs` to use these APIs. Remove the
   `BTreeSet<String>` full-ref planning path.
5. Apply `GitVisibilityEdit` directly to the dictionary/bitmap representation.
   One small fast-forward push must not rebuild unrelated ref closures.
6. Add allocation and timing measurements to the Phase 0 report. Metrics must
   expose counts and durations, never individual private OIDs or ref names.
7. Update visibility docs and serialized-format documentation together with
   code.

### Phase 1 acceptance criteria

- [ ] For generated DAGs, bitmap authorization results exactly match the old
      sorted-closure model for every ref subset and OID.
- [ ] Hidden refs remain hidden; wants reachable only from hidden refs are
      rejected before object-store reads.
- [ ] Corrupt dictionary order, ordinal overflow, invalid bitmap length,
      generation mismatch, and pack-index mismatch fail closed.
- [ ] Full-ref planning for the Kubernetes proof performs no per-ref OID string
      cloning and constructs no `BTreeSet<String>`.
- [ ] On the Phase 0 host, median full-ref planning time is at most 25% of the
      baseline and peak visibility RSS is at most 50% of baseline.
- [ ] After a one-commit fast-forward push, synchronous visibility work scales
      with the changed closure/edit rather than total repository objects.
- [ ] If exact proof publication is deferred to the owner, protocol-v2 exact
      reads remain unavailable until the proof is current; the documented
      complete-pack path remains the only existing recovery behavior.
- [ ] Stored version-1 and version-3 fixtures still read successfully; newly
      written proofs use only the canonical current format.

### Phase 1 tests and verification

Add table-driven unit tests beside existing tests in
`crates/crab-metadata/src/git_visibility.rs` and planner tests in
`crates/crab-read/src/upload_pack.rs`. Add property tests for random ref DAGs,
edits, and visible-ref subsets.

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap \
  cargo test -p crab-metadata --features storage,remote-index --locked git_visibility
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap \
  cargo test -p crab-read --locked upload_pack
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap \
  cargo test -p crab --locked git_visibility
python3 crab/scripts/verify-large-repo-rustfs-report.py <phase-1-report.json>
```

Expected: all tests pass; the report proves the relative planning-time and RSS
targets and all correctness checks remain true.

### Phase 1 STOP conditions

- The new representation requires weakening per-ref authorization.
- A stable ordinal cannot be bound to an immutable generation without an
  ambiguous cross-generation read.
- Compatibility requires retaining two runtime authorization paths instead of
  one storage migration boundary.
- Performance improves only by skipping validation or object limits.

## Phase 2: Preserve deltas and cache clone response packs

### Background

The remote reader already finds exact packed entries and coalesces nearby
object-store ranges. The response writer then inflates and independently
compresses every object, discarding the repository's delta representation.
For a large clone this consumes CPU, increases egress, and repeats identical
work for every developer or CI worker.

Git's on-disk and network formats are both packfiles. Crab should independently
implement safe packed-entry reuse rather than port GPLv2 source.

### Deliverables

1. Extend the verified packed-entry metadata needed by one canonical response
   `PackAssembler` to decide whether an entry can be copied, rewritten from an
   offset delta to a reference delta, or must be materialized.
2. Preserve REF_DELTA payloads when the base is present in the response or is a
   client-proven thin-pack base. Rewrite OFS_DELTA headers to REF_DELTA when
   necessary; never emit a dangling base.
3. Keep one assembler path with an explicit per-entry strategy. Do not leave a
   second broad legacy pack generator behind.
4. Preserve cancellation, response-byte budgets, object-count budgets,
   checksum verification, deterministic object selection, and sideband
   behavior.
5. Add immutable generated-pack caching for no-have requests. The cache key
   must include at least repository identity, manifest Git validation digest,
   visible-ref authorization digest, canonical filter, depth/shallow state,
   tag policy, and thin/self-contained policy.
6. Cache only verified complete artifacts. Concurrent identical misses must
   coalesce to one producer. Cancellation by one waiter must not cancel a
   producer still needed by another waiter.
7. Never include signed URLs, credentials, user identity tokens, or mutable
   ref names in reusable artifact contents.
8. Add metrics for copied entries, converted deltas, materialized entries,
   source bytes, response bytes, assembler CPU, cache hit/miss, and coalesced
   waiters.
9. For dense `blob:none` and `object:type` selections, permit a selected
   object-set repack only when the catalog proves the complete set and the
   generated pack is verified against that exact set. Keep shallow and
   path-context filters on a correctness-preserving path until their own
   reachability index exists.

### Phase 2 acceptance criteria

- [ ] Every generated response passes `git index-pack --strict` and a clone
      using it passes `git fsck --full`.
- [ ] Unpacked object OIDs and bytes exactly match the planner's selected set.
- [ ] Tests cover base before delta, base after delta, deep delta chain,
      missing base, corrupt compressed payload, corrupt CRC, cancellation, and
      response limit exhaustion.
- [ ] `blob:none`, shallow, sparse, include-tag, and have/want requests contain
      no unauthorized or filtered-out object.
- [ ] The Kubernetes full-clone response is no larger than 125% of a reference
      pack produced by Git from the same selected objects.
- [ ] On the Phase 0 host, median response-pack CPU is at most 50% of baseline.
- [ ] A repeated identical clone causes at least 90% fewer origin range reads
      and at least 70% less response-pack CPU than the cold clone.
- [ ] A manifest or authorization-digest change cannot hit an older cached
      artifact.
- [ ] On the Kubernetes two-pack snapshot, a dense `blob:none` request uses
      the catalog-selected path, omits every cataloged blob, and its response
      pack passes exact object-set verification plus the client-side clone
      integrity gate.

### Phase 2 tests and verification

Model unit tests on existing pack checksum and coalescing tests in
`crates/crab-remote-git/src/pack.rs` and
`crates/crab-remote-git/src/reader.rs`. Add protocol tests beside
`crab/src/git/upload_pack_wire.rs` tests.

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap \
  cargo test -p crab-remote-git --locked pack
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap \
  cargo test -p crab --locked upload_pack_wire
python3 crab/scripts/verify-large-repo-rustfs-report.py <phase-2-report.json>
```

Expected: tests pass; strict pack verification and all relative performance
targets are recorded as passing.

### Phase 2 STOP conditions

- Correct delta-base identity cannot be proven from the pinned generation.
- Reuse would permit cross-authorization artifact sharing.
- The only implementation path requires copying GPLv2 implementation source.
- Cache correctness depends on mutable ref names rather than immutable state.

## Phase 3: Publish an incremental generation-bound object catalog

### Background

The locator supplies OID-to-pack location and the visibility proof supplies a
separate OID dictionary. Keeping those universes separate forces conversion,
duplicates validation, and makes every generation's metadata cost harder to
bound. Git's MIDX and bitmap formats solve this by sharing one pseudo-pack
object order.

Crab should borrow the ownership model, not necessarily Git's exact local-file
format. The result must remain efficient for immutable object storage and
SlateDB range/checkpoint reads.

### Deliverables

1. Introduce one generation-bound object catalog contract containing:
   - binary OID and dense ordinal;
   - object type and logical size when known;
   - immutable pack slot and exact packed-entry location;
   - optional proven delta-base ordinal/OID;
   - catalog generation, pack-index hash, and validation digest.
2. Make visibility closures reference catalog ordinals. Remove the independent
   visibility OID dictionary after migration so there is one canonical object
   identity universe.
3. Store the catalog as immutable base and delta layers:
   - a push appends metadata only for new/rebound objects;
   - readers search newest to oldest with deterministic duplicate precedence;
   - adjacent layers compact geometrically by object count;
   - the manifest/CAS names the exact layer chain.
4. Keep locator writer lifecycle and checkpoint safety inside
   `crates/crab-metadata/src/git_object_locator/`; do not leak SlateDB ownership
   into command code.
5. Repack may rebind physical locations without changing ref reachability.
   Publication must atomically bind the replacement catalog and visibility
   closures to the committed manifest generation.
6. Add `verify` coverage for layer ordering, duplicate OIDs, missing pack slots,
   invalid offsets, bitmap ordinals outside the catalog, interrupted writes,
   stale CAS, and compaction equivalence.
7. Provide a repository-scoped doctor/rebuild migration for tagged historical
   metadata. Do not add indefinite runtime aliases or dual-write formats.

### Phase 3 acceptance criteria

- [ ] There is one canonical runtime OID/ordinal universe used by locator,
      visibility, and response-pack assembly.
- [ ] A one-commit push writes metadata proportional to new/rebound objects,
      not total repository objects.
- [ ] Replaying 1,000 pushes keeps catalog layer count logarithmic under the
      documented geometric invariant.
- [ ] Lookup results before and after every layer compaction are identical for
      all OIDs in generated property tests.
- [ ] The Kubernetes report shows no more than 20% upward drift in median
      metadata publication time from push 1 to push 1,000 after excluding the
      explicitly recorded compaction runs.
- [ ] Repack does not rescan retained immutable pack indexes.
- [ ] A reader can pin one manifest/catalog/visibility tuple; mixed-generation
      tuples fail closed.
- [ ] Migration is explicit, idempotent, interruption-safe, and documented.

### Phase 3 tests and verification

Add storage/property tests under
`crates/crab-metadata/src/git_object_locator/` and cross-contract tests in
`crates/crab-metadata/tests/` if a single module cannot exercise publication.

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap \
  cargo test -p crab-metadata --features storage,remote-index --locked git_object
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap \
  cargo test -p crab --locked locator
python3 crab/scripts/verify-large-repo-rustfs-report.py <phase-3-report.json>
```

Expected: all tests and migration checks pass; report layer and publication
growth checks pass.

### Phase 3 STOP conditions

- The design introduces two long-lived locator databases or runtime readers.
- Repack can change an ordinal without rebuilding every bitmap bound to it.
- Layer compaction is not cancel-safe or can publish a partial chain.
- An existing tagged format cannot be migrated without a product decision.

## Phase 4: Replace the bounded summary with a complete split commit graph

### Background

The 10,000-commit summary is sufficient as a recent-history accelerator but
cannot answer deep ancestry or shallow-boundary queries for Kubernetes-scale
history. Git's split commit graph stores parent positions and generation
numbers in appendable layers, allowing history walks to skip large regions.

This phase does not change ref-update semantics. When the graph cannot prove
an answer, callers remain conservative rather than treating incomplete history
as authoritative.

### Deliverables

1. Define a versioned binary graph record with commit OID, parent ordinals,
   root tree OID, commit date, and corrected generation number.
2. Store a complete graph as immutable split layers. Push appends new commits;
   the owner geometrically compacts layers without rewriting stable history on
   every push.
   The owner now resolves a missing current graph from the validated previous
   generation through bounded remote-Git commit batches and appends the new
   layer; it falls back to complete pack materialization when that predecessor
   proof is unavailable.
3. Replace string parent maps in hot ancestry and shallow-boundary paths with
   positional reads.
4. Validate graph closure, parent ordering, generation monotonicity, duplicate
   commits, layer checksums, and manifest identity.
5. Add optional changed-path Bloom filters only after path-history benchmarks
   show value. Building these filters must be bounded per maintenance run and
   must not block push acknowledgement.
6. Keep a documented conservative fallback for incomplete/corrupt graph
   acceleration only where the existing public correctness contract requires
   it; corruption itself must be surfaced for repair.
7. Add doctor/fsck diagnostics and an idempotent graph rebuild operation.
8. Add the missing shallow object-closure acceleration as an explicit graph
   exit gate. A commit graph alone identifies commit boundaries; it does not
   identify every tree/blob reachable from the selected commits. The chosen
   implementation must publish a generation-bound reachability bitmap or
   equivalent tree-closure index that upload-pack can use without issuing one
   remote object traversal for every shallow request. It must remain
   conservative when the index is absent, stale, incomplete, or corrupt.

### Phase 4 acceptance criteria

- [ ] The graph contains all commits reachable from every committed manifest
      ref in the Kubernetes repository, not only the newest 10,000.
- [ ] For at least 10,000 deterministic ancestor pairs, graph answers equal
      `git merge-base --is-ancestor`.
- [ ] Shallow boundaries for depths 1, 10, 100, and 1,000 match Git for linear,
      merge-heavy, octopus-merge, and Kubernetes histories.
- [ ] A one-commit push appends a bounded graph layer and does not rewrite the
      complete graph.
- [ ] Layer compaction produces identical ancestry and boundary answers.
- [ ] Median Kubernetes ancestry query is at least 5x faster than baseline and
      does not download pack bodies when the graph is current.
- [ ] Graph corruption or generation mismatch cannot authorize a ref update.
- [ ] For the Kubernetes snapshot, depth-1/10/100/1,000 planning is exact and
      emits the same object set as Git's shallow clone differential. Depth-1
      and depth-100 planner p95 is at most 10 seconds, and storage requests
      are no more than twice the number of selected objects; otherwise the
      phase remains incomplete.

### Phase 4 tests and verification

Use existing tests in `crates/crab-metadata/src/commit_graph.rs` as the unit
style. Add generated DAG property tests and real-Git differential tests.

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap \
  cargo test -p crab-metadata --features storage --locked commit_graph
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap \
  cargo test -p crab-remote-git --locked commit_graph
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap \
  cargo test -p crab --locked shallow
python3 crab/scripts/verify-large-repo-rustfs-report.py <phase-4-report.json>
```

Expected: differential tests and performance thresholds pass.

### Phase 4 STOP conditions

- Generation-number computation is approximate rather than satisfying the
  documented reachability property.
- Split layers permit parent references to ambiguous or mutable positions.
- A partial graph is used as proof of non-ancestry without a conservative
  boundary signal.
- Changed-path Bloom filters would delay push acknowledgement.

## Phase 5: Automate bounded maintenance and stale-pack retention

### Background

Geometric repack is currently explicit and the post-fetch threshold is only a
warning. Large teams cannot rely on every developer to run maintenance, but
maintenance must not enter the synchronous push path. The existing generation
owner already has a repository-scoped lease, locator writer, retry loop, and
visibility repair responsibility.

GitHub also separates reachable packs from recently unreachable cruft. Crab's
object-store equivalent must respect immutable recovery roots and the existing
GC grace period rather than copying local `.mtimes` files literally.

### Deliverables

1. Extend the existing generation owner with one maintenance decision loop; do
   not create another service. It evaluates immutable repository state and
   chooses at most one bounded action per cycle.
2. Add read-only maintenance planning and structured output for:
   - violation of the geometric pack invariant;
   - small-pack object/byte ratio;
   - object-catalog layer count and bytes;
   - commit-graph layer count and bytes;
   - missing/stale generated clone artifacts;
   - active, retained-history, grace-period, and collectible pack bytes.
3. Reuse existing repository maintenance admission, push-lock, heartbeat,
   cancellation, and manifest CAS contracts. Never hold a ref lock while doing
   unbounded downloads or CPU work.
4. Run geometric pack compaction, catalog/bitmap compaction, graph compaction,
   and clone-artifact generation asynchronously under explicit time, byte, and
   request budgets.
5. Add an object-store-native stale-pack inventory recording last reachable
   generation/time. Historical recovery roots and grace-period objects remain
   protected. Repository GC consumes this evidence only after independently
   proving current unreachability.
6. Keep policy defaults internal for the first release. Add configuration only
   if qualification proves one default cannot safely cover supported backends.
7. Expose maintenance reason, work selected, bytes read/written, elapsed time,
   cancellation, next eligibility, and failures through structured metrics.

### Phase 5 acceptance criteria

- [ ] After 10,000 simulated one-commit pushes with the owner running, active
      pack and metadata-layer counts remain logarithmic in repository growth.
- [ ] Stable large packs are not downloaded or rewritten by routine geometric
      maintenance.
- [ ] Push p95 latency during maintenance is no more than 10% above the same
      workload without maintenance, excluding legitimate same-ref lock
      contention reported separately.
- [ ] Killing the owner during every maintenance stage leaves either the old
      manifest or one complete new manifest; restarting converges without
      manual cleanup.
- [ ] Every acquired lock/lease is released on success, error, cancellation,
      timeout, and stale CAS.
- [ ] Repository GC dry-run classifies active, retained-history, grace-period,
      and collectible bytes separately.
- [ ] GC deletes no object referenced by current state, any retained recovery
      root, workflow artifacts, or the grace period.
- [ ] No test or qualification command invokes bucket-wide GC.
- [ ] `crab metadb owner --once --jsonl` reports the selected maintenance action
      deterministically for a fixed snapshot.

### Phase 5 tests and verification

Add lifecycle tests beside `crab/src/cmd/metadb.rs`, repack integration tests
beside `crab/src/cmd/repack.rs`, and GC safety tests beside the repository-scope
GC implementation.

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap \
  cargo test -p crab --locked metadb
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap \
  cargo test -p crab --locked repack
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap \
  cargo test -p crab --locked gc
python3 crab/scripts/verify-large-repo-rustfs-report.py <phase-5-report.json>
```

Expected: all lifecycle, fault-injection, GC safety, and growth-bound checks
pass.

### Phase 5 STOP conditions

- Maintenance requires a second owner or competing lease hierarchy.
- A policy needs new environment/configuration surface before default behavior
  has been tested across S3, RustFS/MinIO, GCS, Azure, and R2.
- Repack or compaction can hold a ref lock during repository-sized I/O.
- GC relies only on stale-pack metadata without independently proving retained
  reachability.

## Phase 6: Qualify large-team concurrency and stage the rollout

### Background

Single-client Kubernetes replay proves repository scale but not team scale.
Large teams add identical clone fanout, concurrent fetches, branch-parallel
pushes, same-ref contention, cache stampedes, object-store throttling, owner
failover, and maintenance overlap. GitHub handles this with routing, local
replicas, caching, precomputation, and staged deployment. Crab must implement
the equivalent properties through its object-store and cache boundaries.

This phase is a release gate, not a new architecture rewrite.

### Deliverables

1. Extend the Phase 0 driver with controlled scenarios:
   - 50 simultaneous identical cold clones;
   - 100 warm clones through `crab-cache-server`;
   - 100 concurrent incremental fetches;
   - 20 pushes to independent refs;
   - 20 pushes contending on one ref;
   - fetch/clone overlap with each maintenance action;
   - injected latency, throttling, transient failures, and owner termination.
2. Add request coalescing for identical generated artifacts and hot immutable
   range reads where metrics prove stampede amplification. Reuse the existing
   cache service; do not create a repository server.
3. Keep LFS dependency publication proportional to new history: scan pushed
   tips while excluding every ref tip from the pinned base manifest, and ask
   Git to enumerate only blobs small enough to be valid LFS pointers. Prove a
   one-commit push from a `blob:none` clone does not hydrate ordinary blobs
   already reachable from the remote.
4. Add bounded admission/backpressure at repository operation boundaries.
   Report retryable throttling explicitly; do not copy GitHub's numeric limits
   without Crab workload evidence.
5. Define and publish operational SLOs for:
   - clone/fetch/push p50, p95, and p99;
   - error and retry rate;
   - origin request and egress amplification;
   - generated-pack cache hit rate;
   - lock wait and contention;
   - maintenance backlog and duration;
   - active/retained/collectible storage ratio.
6. Roll out in four gates:
   - shadow: compute new plans and compare with canonical results, never serve;
   - opt-in canary repositories;
   - percentage canary by immutable repository identity;
   - default-on after sustained correctness and SLO evidence.
7. Keep rollback generation-based: stop selecting new artifacts, pin the prior
   known-good manifest/index generation, and preserve immutable evidence for
   diagnosis. Never mutate already published pack objects.
8. Update operator docs, diagnostics, upgrade notes, and incident runbooks.

### Phase 6 acceptance criteria

- [ ] All load scenarios preserve exact refs and pass `git fsck --full` on
      every completed clone/fetch checkout.
- [ ] Independent-ref pushes complete without lost updates; same-ref pushes are
      serialized or rejected with the documented retryable stale/lock outcome.
- [ ] A one-commit push from a `blob:none` clone leaves already-remote ordinary
      blobs absent locally, and the LFS scan reads only newly introduced blobs
      no larger than the maximum pointer size.
- [ ] Fifty identical cold clones produce no more than two generated-pack
      producers; all other callers coalesce or consume the verified artifact.
- [ ] Warm clone fanout achieves at least 90% generated-artifact cache hits and
      at least 80% fewer origin requests than cold fanout.
- [ ] Under injected transient failures, every operation either succeeds with
      exact output or returns a typed retryable/cancelled error; no optimistic
      success is reported.
- [ ] Owner termination and maintenance overlap produce no corrupt manifest,
      leaked lock, unauthorized read, or referenced-object deletion.
- [ ] Phase 6 p95 full-clone time is at least 5x faster than the original
      pre-repack Kubernetes baseline and no slower than Phase 2 by more than
      10% under single-client conditions.
- [ ] One week of canary evidence, or an equivalent sustained test window
      approved by maintainers, has zero correctness mismatches before
      default-on rollout.
- [ ] All supported Git versions and object-store providers pass their existing
      compatibility gates.

### Phase 6 verification

```bash
cd crab && CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap make test
cd crab && CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap make clippy
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap cargo test --workspace --locked
python3 crab/scripts/verify-large-repo-rustfs-report.py <phase-6-report.json>
git diff --check
```

Expected: all gates pass; the final report includes every concurrency scenario,
correctness result, SLO comparison, and rollback exercise.

### Phase 6 STOP conditions

- Load targets can only be met by weakening authorization, integrity checks,
  object limits, or GC grace.
- Backpressure silently drops, acknowledges, or reorders ref updates.
- Cache sharing cannot bind artifacts to the exact authorization scope.
- Rollback requires deleting or rewriting immutable data.
- Canary correctness differs from the canonical path even once; investigate
  before increasing exposure.

## Phase evidence record

Update this table in planning/evidence commits. Do not paste secrets, complete
environment dumps, or credentials.

| Phase | Status | Implementation PR | Report artifact | Verification commit | Notes |
|---|---|---|---|---|---|
| 0 | POST-LAZY SINGLE-RUN PASS; DIFFERENTIAL/REPEATABILITY PENDING | PR #75 | `lazy-cbe848f4-1000-20260825` (prefix cleaned); pre-lazy baseline `local-k8s-final-04655f3b-1000-20260825` | `7ff92545` / binary `git_sha=7ff92545` | The current full profile passed 1,001 pushes and all 22 checks with exact refs/fsck/sample/source/cleanup evidence. The standalone baseline comparison is invalid because push and clone medians drifted by roughly 41% on the shared host; repeatability, differential, fault, provider, concurrency, and rollout evidence remain open |
| 1 | IMPLEMENTED; POST-LAZY NORMAL-PATH PROOF PASS; SLO PENDING | PR #75, follow-up PR #87 | `lazy-cbe848f4-1000-20260825`; `crabbuild-f2a941ce-k8s-20260827-smoke`; [released-shape workflow](https://github.com/crabbuild/crab-oss/actions/runs/32917566230) | `7ff92545`; `01d588ea`; `cbe848f4`; `c57ee1f4`; `f2a941ce` | Normal read/helper paths remain lazy, and the exact current release smoke passes the large-batch path. Owner repair intentionally may materialize the catalog; full-profile repeatability and SLO evidence remain open |
| 2 | IMPLEMENTED; CURRENT SINGLE-CLIENT EVIDENCE; SLO PENDING | PR #75, follow-up PR #87, PR #96 | `lazy-cbe848f4-1000-20260825`; `crabbuild-f2a941ce-k8s-20260827-smoke`; `e01fdf56-k8s-1000-20260828`; `b8c51985-locator-ref-tip-only-concurrent-20260828` | `7ff92545`; `c57ee1f4`; `f2a941ce`; `8fb0ca86`; `e01fdf56`; `b8c51985` | The current full run ends with 2 active packs and the e01 pack-source qualification retains 1,003 immutable physical history objects while serving converges to 2. The b8 locator startup smoke passes its 500-request regression budget. Response-pack egress, fanout, retention, and provider SLOs remain open |
| 3 | IMPLEMENTED; CURRENT OWNER EVIDENCE; SLO PENDING | PR #75, follow-up PR #87, PR #96 | `lazy-cbe848f4-1000-20260825`; `crabbuild-f2a941ce-k8s-20260827-smoke`; `e01fdf56-k8s-1000-20260828` | `7ff92545`; `c57ee1f4`; `ad2554fa`; `b9859f28`; `a55c89b3`; `4a8fc34e`; `14f30438`; `f2a941ce`; `88deb4e0`; `e01fdf56`; `b8c51985` | The e01 owner processed an inventory of 992 packs, swept 991 stale membership rows, and left 2 active serving packs, but took 471.1 s across ten passes; source-index elimination is correctness/IO hardening, not a measured repack wall-time win. The 10,000-push latency, memory, and interruption budgets remain open |
| 4 | IMPLEMENTED; POST-LAZY FETCH PASS; SHALLOW/DIFFERENTIAL SLO PENDING | PR #75, follow-up PR #87, PR #96 | `lazy-cbe848f4-1000-20260825`; `crabbuild-f2a941ce-k8s-20260827-smoke`; `e01fdf56-k8s-1000-20260828`; `b8c51985-locator-ref-tip-only-concurrent-20260828` | `7ff92545`; `cbe848f4`; `c57ee1f4`; `f2a941ce`; `e01fdf56`; `b8c51985` | The e01 full run passes incremental and depth-1/10/100/1,000 correctness; its valid comparison against 0bcd2f41 stayed within 20% for clone/fetch/push medians. The b8 same-ref run passes protocol-v2 visibility and the locator request budget. The 10,000-push shallow differential, response-pack SLO, concurrency, and rollout evidence remain open |
| 5 | IMPLEMENTED; CURRENT EVIDENCE; OPERATIONAL GAPS PENDING | PR #75, follow-up PR #87, PR #96 | `lazy-cbe848f4-1000-20260825`; `e01fdf56-k8s-1000-20260828` | `7ff92545`; `c57ee1f4`; `ad2554fa`; `e01fdf56` | Current-manifest GC roots retain pending catalog handoffs, and repo-local `repair_required` no longer conflates incomplete bucket-wide discovery with repair. The e01 run completed cleanup but retained 1,003 immutable pack objects for recovery history; grace-aware retention, interruption, receipt/registry completeness, 10,000-push, and full GC matrix remain pending; bucket-wide destructive GC stays disabled |
| 6 | PARTIAL | PR #75, follow-up PR #87, PR #96 | `lazy-cbe848f4-1000-20260825`; `crabbuild-f2a941ce-k8s-20260827-smoke`; `e01fdf56-k8s-1000-20260828`; `b8c51985-locator-ref-tip-only-concurrent-20260828` | `7ff92545`; `c57ee1f4`; `f2a941ce`; `d9c93263`; `88deb4e0`; `e01fdf56`; `b8c51985` | Current single-client correctness, the e01 1,000-replay pack-source qualification, and the b8 same-ref startup budget pass. Full-profile repeatability, 100-client fanout, fault, cache-server, provider, owner-failover, retention, and canary gates remain pending |

### Current branch verification evidence

The earlier broad proof was run with the roadmap target directory:

- `cargo test -p crab --lib --locked -- --test-threads=1`: 3,695 passed,
  2 ignored, 0 failed;
- `cargo test -p crab-metadata --locked`: 232 passed, 1 ignored;
- `cargo test -p crab-read --locked`: 55 passed;
- `cargo test -p crab-remote-git --locked`: 138 passed;
- focused split-graph, shallow, fetch-admission, generation-owner, repack, GC,
  schema, and report-verifier suites passed;
- the RustFS protocol-v2 smoke at `b7749b2e` passed 251 real Git commands
  and 76 checks; its incomplete-ODB push completed without hydrating any
  remote object (`0` read requests and `0` fetched bytes), and all canonical
  repository object counts and bytes remained unchanged across the filter
  matrix while generated cache artifacts increased;
- `RUSTUP_TOOLCHAIN=1.98.0 make split-crate-clippy-check` passed with warnings
  denied across all 17 split packages; production-library clippy for
  `crab-metadata`, `crab-git`, `crab-remote-git`, and `crab-read` also passed.

The repository-wide `make clippy` gate is not recorded as passing: it reaches
pre-existing warnings in untouched `crab-vfs` code. The post-lazy full
Kubernetes/RustFS qualification is green for correctness, but its performance
comparison is invalid and the Phase 6 rollout evidence plus the remaining
SLO/differential/fault/provider gates are not complete.

The current committed tree additionally passes the focused remote-helper
transcript suite (`42` tests with `RUST_MIN_STACK=33554432`), architecture
gates, split-crate behavior gates, strict split-crate clippy checks, metadata
tests, and remote-Git tests. The full workspace test/clippy gates are not
claimed green here; unrelated baseline failures/warnings outside the touched
surfaces remain separate cleanup work.

The post-`c5797d8f` focused proof additionally passes `cargo fmt --all
-- --check`, strict `crab-git` clippy, all 8 pack-locator tests, all 40
protected-receive tests, all 271 runnable Crab push tests (one unrelated
integration test remains ignored), and the generated-pack cache unit tests.
The full workspace and real-repository qualification gates remain open as
recorded in the evidence table.

The locator-startup follow-up (`b8c51985`) additionally passes
`cargo fmt --all -- --check`, the metadata locator module (`54` passed, `1`
ignored), all Crab push tests (`269` passed, `1` ignored), and the complete
remote-Git suite (`92` unit tests plus `61` repository tests). Its release
binary, whose embedded source revision is `b8c51985`, passed the isolated
four-agent same-ref RustFS smoke with 194.5 catalog requests per successful
push, zero integration retries, and a protocol-v2 content check. The full
workspace and long-duration rollout gates remain separate requirements.

The latest qualification-contract follow-up (`0a8e4aa8`) additionally passes
the Python E2E harness suite (`30` tests). Its post-push terminal fetch proves
that the generation-owner admission path completes the expected locator and
visibility repair before the filter matrix, so later filtered reads are
measured against a stable canonical remote rather than conflating repair with
steady-state read behavior.

The current owner JSONL/report contract also carries per-pass locator sweep
counters. Future full-profile qualifications now fail if this evidence is
missing, and can distinguish bounded stale-pack membership cleanup from a
repository-sized canonical catalog rebuild.

The focused proof for committed `04655f3b` is:

- `cargo fmt --all -- --check`;
- `cargo check -p crab --locked` and metadata feature checks;
- `cargo test -p crab-metadata --features remote-index,storage --locked -- --test-threads=1`:
  238 passed, 1 ignored;
- `cargo test -p crab --test remote_helper_transcript --locked -- --test-threads=1`:
  42 passed;
- `python3 -m unittest crab/scripts/e2e/test_verify_large_repo_rustfs_report.py crab/scripts/e2e/test_run_concurrent_push_smoke.py`:
  30 passed; and
- `git diff --check`.

All Cargo commands used the required isolated target directory on the
workspace volume. This is focused source proof, not a substitute for the
open full-suite, latest-binary, provider, fault, or team-concurrency gates.

The lazy-catalog follow-up (`cbe848f4`) additionally passes focused source
proof with the isolated target directory:

- `cargo fmt --all -- --check`;
- `cargo check -p crab --locked` with no warnings from the touched production
  crate;
- `cargo test -p crab-metadata --features remote-index,storage --locked git_visibility -- --nocapture`:
  27 passed;
- `cargo test -p crab-metadata --features remote-index,storage --locked git_object_locator -- --nocapture`:
  39 passed, 1 ignored;
- `cargo test -p crab-read --locked upload_pack -- --nocapture`: 23 passed;
- `cargo test -p crab --locked upload_pack_wire -- --nocapture`: 24 passed;
- `RUST_MIN_STACK=33554432 cargo test -p crab --locked --test
  remote_helper_transcript -- --nocapture`: 42 passed.

The large-batch locator follow-up additionally passes the same metadata
locator suite (`39` passed, `1` ignored). It keeps exact point reads for small
or sparse requests and selects a bounded 16 MiB read-ahead scan when a large
request covers at least one sixty-fourth of the pinned catalog. The K8s run
below predates this follow-up, so its depth-1/10 planning numbers remain the
before-change measurement.

The stale-pack follow-up (`ad2554fa`) additionally passes strict metadata
clippy, the complete metadata suite (`256` passed, `1` ignored), and locator
regressions proving that pure repacks remove old membership rows without
scanning the object catalog, while a marker-less catalog rebuilds the derived
index once. The remote-Git test follow-up (`b9859f28`) passes the complete
`crab-remote-git` suite (`84` unit tests and `61` repository tests) and makes
the shared delta-base coalescing assertion independent of scheduler timing.

The last command needs the explicit larger test stack for
`protocol_edges::eof_mid_fetch_batch_finalizes_without_blank_line`; the exact
test also fails on the pre-change `68ddf2fc` baseline with the default macOS
stack, so this is tracked as baseline harness debt rather than attributed to
the lazy-catalog change.

### Pre-lazy Kubernetes/RustFS baseline from committed `04655f3b`

Run profile: `local-k8s-final-04655f3b-1000-20260825`, Kubernetes revision
`b3bc2ac58fa173967f27ade80f28cc5015b8c1c3`, external local RustFS, and the
release binary whose provenance reports `git_sha=04655f3b`. The standalone
verifier passed with `status=ok` and `profile=full`; all 22 harness checks
passed and cleanup removed the isolated remote prefix.

- Pushes: 1,000 replay pushes plus the seed; median 1,808 ms, p95 3,211 ms.
- Owner: 1,993,181 ms at the 1,000 checkpoint across six passes; peak child
  RSS 1,846,001,664 bytes. Final state: generation 8, 2 active packs,
  1,263,723,813 active bytes, 9 catalog layers, 115,230,529 catalog bytes,
  current locator/visibility indexes, and one graph layer for 140,383 commits.
- Fetch: the 1,000-checkpoint incremental fetch completed in 8,927 ms with
  2,048 ms server operation time, 38,745 logical objects, 40,495,429 response
  bytes, and 226 storage requests.
- Clones: cold full 184,248 ms with 1,241,537,452 response bytes and two
  storage requests; warm full 56,788 ms with zero storage requests; blobless
  78,162 ms; depth-1/10/100/1,000 32,842/43,552/145,423/201,211 ms.
- Correctness: advertised refs and clone tips matched the source, full and
  incremental fsck passed, 1,000 sampled objects were byte-identical, and the
  source checkout was unchanged. Fingerprint:
  `7d97627cf1f4de8b87679dea53d99916df42c3152dc765399d4494c43af09624`.

This closes pre-lazy single-client correctness and qualification evidence. It
does not prove the post-`cbe848f4` lazy-admission behavior and does not close
the owner-latency/memory SLO, roughly 1.2 GB cold response-pack egress,
blobless kind-lookup cost, differential, fault, provider, concurrency, or
rollout gates listed above.

### Post-lazy Kubernetes/RustFS qualification

Run profile: `lazy-cbe848f4-1000-20260825`, the same Kubernetes revision
`b3bc2ac58fa173967f27ade80f28cc5015b8c1c3`, isolated local RustFS, 1,000
first-parent replay pushes, and the release binary whose provenance reports
`git_sha=7ff92545`. The standalone verifier reports `status=ok`,
`profile=full`, and 22/22 checks passed. Advertised refs and full/incremental
clone tips matched the source, both fsck checks passed, 1,000 sampled objects
were byte-identical, the source checkout was unchanged, and the run-owned
remote prefix was cleaned. The correctness fingerprint remained
`7d97627cf1f4de8b87679dea53d99916df42c3152dc765399d4494c43af09624`.

- Normal seed, full, warm, blobless, and depth-1/10/100/1,000 helper logs
  emitted no `catalog_materialization` event. The owner repair/rebuild path
  intentionally emitted that event, so the result proves lazy admission but
  not an O(N)-free maintenance cycle.
- Replay-only pushes had 671/2,543/8,193/19,731/57,809 ms for
  min/median/p95/p99/max. Active packs stayed at 2 after the seed and through
  checkpoint 1,000, with 1,263,705,690 active bytes; the store retained 1,004
  immutable historical pack objects, which is the physical-retention versus
  active-inventory distinction the GC/repack work must continue to enforce.
- Incremental fetch server planning/response remained bounded: visibility
  planning at pushes 1/10/100/1,000 was 51/60/74/662 ms, with
  37/420/4,810/38,745 logical objects and 2/6/33/226 storage requests.
- Before the large-batch locator follow-up, the indexed shallow planner took
  11,659/15,553/2,405/2,634 ms for depth 1/10/100/1,000 and selected
  30,031/44,026/1,001,853/1,643,211 objects. The depth-1/10 cost was traced to
  tens of thousands of exact OID authorization reads; the follow-up adds a
  bounded full scan for large requests and retains exact reads for sparse
  requests. A fresh release run must prove the improvement.
- Clone wall times were 199,311 ms cold full, 72,101 ms warm full, 110,384 ms
  blobless, and 45,570/58,702/174,452/191,898 ms for depth 1/10/100/1,000.
  The cold and deep responses still reach roughly 1.2 GB, so response-pack
  egress and fanout remain open SLOs.
- Owner convergence took 146,947/244,227/373,872/2,729,094 ms at checkpoints
  seed/1/10/100/1,000. Checkpoint 1,000 used six passes, peaked at
  1,837,449,216 bytes of child RSS, and read 1,478,153,950 bytes. Every
  acceleration receipt was current and valid, but `repair_required=true`
  remained because bucket-registry discovery is incomplete and destructive
  bucket GC remains disabled.
- The verifier comparison against the pre-lazy baseline is intentionally
  invalid: push median drifted +40.7% and clone median +41.2%, exceeding the
  20% limit, while fetch median improved 12.0%. Repeat the comparison on an
  idle isolated host before treating any latency change as a regression or
  improvement.

This run closes post-lazy single-client correctness and proves that normal
read admission no longer materializes the complete OID dictionary. It does not
close the large-batch shallow SLO, owner latency/memory, catalog-filter kind
lookup, response-pack, 10,000-push, differential, interruption/GC, provider,
concurrency, owner-failover, or rollout gates.

### Historical pre-handoff diagnostic

Run profile: `local-k8s-final-20260824`, Kubernetes revision
`b3bc2ac58fa173967f27ade80f28cc5015b8c1c3`, isolated local RustFS, one replay
push, and the release binary built from this working tree. The run-owned
remote prefix was cleaned successfully. Its report is intentionally not a
passing qualification report: the run was interrupted after depth-100
planning produced no result for 11 minutes.

- Seed import created one 1,643,202-object, 1,263,633,295-byte pack. The
  synchronous push closed in 233 s, ran no repository-wide maintenance before
  acknowledgement, and owner convergence published seven catalog layers
  (110,087,330 bytes), visibility, and a one-layer graph for 140,381 commits.
  The visibility owner operation took 137,657 ms; graph publication took
  89,880 ms.
- The replay push added two commits and nine objects in an 11,018-byte pack,
  passed connectivity with zero missing objects, and advanced the manifest to
  generation 2 with two active packs. Its incremental fetch planned nine
  objects in 0 ms, generated a 11,018-byte response in 5 ms, and completed the
  server operation in 60 ms (local Git fetch wall time was 98 s).
- Post-replay owner convergence rebuilt a one-layer graph for 140,383 commits
  in 135,675 ms and left ten catalog layers totalling 110,088,407 bytes. No
  SlateDB cancellation panic or temporary-bare-repository BrokenPipe appeared
  in this fresh run; the branch pins SlateDB to 0.15.0 and initializes every
  temporary Git directory before pack/index use.
- The cold full clone planned 1,643,211 objects in 66 ms, consolidated the two
  packs in 113,969 ms, and transferred a verified 1,244,177,064-byte response.
  The full clone passed `git fsck --full`. The warm clone hit the generated-pack
  cache, reused the same response, and generated it in 17,859 ms; its fsck also
  passed.
- The catalog-aware `blob:none --no-checkout` clone selected 1,102,159
  objects, omitted 541,052 blobs, planned in 1,987 ms, and used the guarded
  `selected_object_repack` path. Exact-set validation succeeded; pack
  generation took 78,564 ms and transferred 198,850,600 bytes. The clone
  completed and the run advanced to shallow probes.
- Depth-1 remained on the context-aware traversal path. It selected 30,031
  objects, took 40,291 ms to plan, issued 47,137 storage requests, and spent
  53,923 ms generating a 49,662,271-byte response. This is correct but not a
  large-repository SLO pass.
- Depth-100 was still in planning after 11 minutes and emitted no visibility
  plan or response telemetry. The OID deduplication regression test passes, but
  this live result proves that queue deduplication alone is not the required
  shallow optimization. A generation-bound commit/tree reachability bitmap or
  equivalent exact closure index is now an explicit Phase 4 exit gate.

### Follow-up indexed shallow run

Run profile: `local-k8s-shallow-index-v2-20260824`, the same Kubernetes
revision and RustFS topology, release binary from this branch, one replay push,
and closure profiles for depths 1, 10, 100, and 1,000. The run stopped at the
depth-100 clone timeout and cleaned its remote prefix.

- The owner published generation 1 with four content-addressed closure entries
  and a 1,438-byte descriptor for the 140,381-commit snapshot. After the
  replay push, generation 2 retained the closure publication contract.
- Depth-1 completed in 226.291 s, selected 30,031 objects, transferred
  49,662,271 bytes, and passed the clone integrity checks. The server recorded
  90,307 storage requests and 103.750 s of operation time, so exact indexing
  removed the unbounded history walk but did not yet meet the request-amplification
  SLO.
- Depth-10 completed in 301.611 s, transferred 106,665,346 bytes, and passed
  the clone integrity checks. It recorded 149,584 storage requests and
  172.045 s of operation time. The response was generated from the exact
  selection and was cached as a verified artifact.
- Depth-100 still timed out after 1,803.672 s before publishing response
  telemetry. The current dense selected-object repack downloads and repacks
  the canonical pack inventory for this profile; that is the remaining
  bottleneck. A cache descriptor fix now records both the requested selection
  count and the larger self-contained pack count, so delta-base expansion will
  not invalidate a reusable artifact once a bounded producer completes.

This run narrows the Phase 4 gap: object selection and shallow boundaries are
indexed and correct for the completed profiles, while response-pack production
still needs a pre-generated or range-native path before the large-repository
SLO can be accepted.

### Completed indexed shallow run

Run profile: `local-k8s-shallow-index-v3-20260824`, the same Kubernetes
revision and RustFS topology, one replay push, and the release binary from
the generation-bound implementation. The run completed and cleaned its
remote prefix. It is a smoke profile because it replays one commit, but it
covered every clone shape required to isolate the shallow response bottleneck.

- The seed imported approximately 1,643,202 objects into one canonical pack;
  the replay added two commits and nine objects. Full, incremental, filtered,
  and all four shallow clone tips matched the source; full and incremental
  checkouts passed `git fsck --full`, 1,000 sampled objects were byte-identical,
  advertised refs matched, the source checkout stayed unchanged, and cleanup
  left no remote-prefix leaks.
- Exact indexed planning selected 30,031/44,026/1,001,853/1,643,211 objects
  for depth 1/10/100/1,000. Planner times were 11/13/227/357 ms and planning
  used one storage request for depth 1/10 and three for depth 100/1,000.
- Depth-100 used selected repack: two source packs, 1,263,644,313 source
  bytes, 813,950,951 response bytes, 86,808 ms pack generation, and three
  storage requests. Depth-1 and depth-10 were exact but exposed the sparse
  delta-base response bottleneck addressed by the follow-up below.
- Depth-1, depth-10, depth-100, depth-1,000, and `blob:none` all completed
  through protocol v2. The filtered clone selected 1,102,159 objects and
  transferred 198,850,600 bytes after exact catalog filtering.

This report closes the correctness and exact-selection portion of the Phase 4
shallow gate for the measured snapshot. It does not close the full-profile
replay, differential ancestry, provider, concurrency, or canary gates.

### Sparse response-pack follow-up

Run profile: `local-k8s-shallow-prefetch-v1-20260824`, the same Kubernetes
revision and RustFS topology, one replay push, and focused depth-1/depth-10
clones. The run used a working-tree release binary containing the changes
later committed as `1d69fe79` and cleaned its remote prefix. It is a focused
optimization report, not a full-profile qualification report.

- The exact closure planner issued one storage request for each shallow plan;
  depth-1 selected 30,031 objects and depth-10 selected 44,026 objects.
- Depth-1 generated a 49,424,332-byte response in 27,180 ms with 7,388
  storage requests, 243,207,219 fetched bytes, and 381,891,113 inflated
  bytes. Depth-10 generated a 98,313,035-byte response in 27,944 ms with
  7,502 storage requests, 286,410,139 fetched bytes, and 778,311,780
  inflated bytes.
- The earlier batch-only baseline recorded 36,702/41,493 storage requests and
  40,684/71,125 ms of pack generation for the same depth-1/depth-10 profiles.
  The new path therefore removes the request amplification and materializes
  cross-batch delta bases locally while retaining strict pack/fsck checks.
- The new resolver keeps raw entries generation-bound, verifies CRCs, pack
  headers, delta results, and object IDs, charges aggregate inflated-byte
  budgets, checks cancellation, and uses the shared decode-admission limit.
  Its higher fetched-byte total is intentional bounded range amplification;
  provider-specific range economics still need the Phase 6 matrix.

The focused run passed the incremental tip, source-unchanged, remote-cleanup,
and artifact-redaction checks. It does not close the full 1,000-push report,
10,000-pair differential, provider, concurrency, or sustained-canary gates.

This evidence proves the push-acknowledgement boundary, generation binding,
multi-pack response reuse, exact filtered packing, and current owner lifecycle.
The v2 run additionally proves that the legacy remote-helper depth-1 and
depth-10 clones consume the exact indexed object closures and pass their Git
integrity checks. It does not satisfy the 1,000-push, depth-100/1,000 shallow
differential/performance, owner-fault, concurrency, cache-server, provider, or
canary gates.

### Locator-preserving repack qualification

Run profile: `local-k8s-locator-order-36444de0-20260825`, Kubernetes revision
`b3bc2ac58fa173967f27ade80f28cc5015b8c1c3`, isolated local RustFS, ten replay
pushes, and the release binary built from `36444de0`. The report is a smoke
profile, not the 1,000-push gate. The verifier passes with `--allow-smoke`, all
report checks are green, and the run-owned RustFS prefix has zero remaining
keys.

- The seed imported a 1,643,111-object, 1,263,576,922-byte pack. The seed
  owner converged in five passes with 1,643,111 logical objects, 125,903 ms of
  visibility work, and a 1,679,458,304-byte peak RSS. The ten-push checkpoint
  owner converged in six passes and selected `geometric_repack`; active packs
  fell from 11 to 2 in 36,043 ms.
- The locator publication fix writes evidence for current pack bindings before
  stale-slot sweep. A pure repack therefore retains the dense object catalog
  while only old pack rows are removed; a full dense rebuild is reserved for an
  actual object-row deletion. The checkpoint-10 owner passes completed in
  80.1 s and 72.0 s after repack without opening the old source pack/index,
  which proves the closure-rebind and catalog-publication ordering on this
  snapshot. The focused metadata regression also proves a repack that rewrites
  every object preserves the catalog object count.
- Full cold clone consolidated two packs into 1,244,191,708 response bytes in
  111,615 ms with two storage reads; the warm clone reused the verified
  generated artifact and recorded a 160 ms server operation. `blob:none`
  selected 1,102,159 objects and transferred 198,833,510 bytes in 77,017 ms.
- Exact shallow planning selected 30,031/44,026/1,001,853/1,643,211 objects
  for depth 1/10/100/1,000 in 10/16/722/404 ms. Response generation produced
  49,424,305/98,335,064/813,835,444/1,244,191,708 bytes in
  23,697/28,758/109,878/111,615 ms; depth-1/10 used 7,386/7,487 storage
  requests, while depth-100/1,000 used three each from the consolidated packs.
  Every shallow clone completed with exact tip/ref checks and the run passed
  the full/incremental fsck gates.
- Incremental fetch after pushes 1 and 10 transferred 7,342 and 120,128
  response bytes in 138 and 172 ms of server operation time. The source
  checkout remained unchanged, a deterministic ten-object sample matched, and
  all generated artifacts were cleaned or redacted as required.

This run closes a material post-repack regression and strengthens the
single-client evidence for Phases 2–5. It does not close the 1,000-push growth
curve, 10,000-pair shallow differential, interruption/GC matrix, supported
provider, large-team concurrency, or sustained canary gates.

### Long fast-forward history qualification

Run profile: `local-k8s-history-chain-100b-20260825`, Kubernetes revision
`b3bc2ac58fa173967f27ade80f28cc5015b8c1c3`, isolated local RustFS, 100 replay
pushes, and the release binary built from pre-rebase commit `1e9ac756`.
The report status is `ok`, the standalone verifier passes with `--allow-smoke`,
and the run-owned RustFS prefix is empty after cleanup. Because the binary is
not current `HEAD`, this is evidence for the exercised behavior, not final
acceptance of `793f6b43`.

- The source revision was `b3bc2ac58fa173967f27ade80f28cc5015b8c1c3`, with
  source base `67ea564b777bafee705a8a4ada1037015fdb96a6`. Advertised refs,
  full and incremental clone tips, Git fsck, a deterministic byte-identical
  object sample, source immutability, and cleanup all passed.
- The seed contained about 1.6M objects. At checkpoint 100, owner convergence
  completed six passes with `ref_journal_compaction`, two catalog advances,
  `commit_graph_rebuild`, `geometric_repack`, and a final no-op; active packs
  fell from 11 to 2 and occupied 1,264,812,786 bytes.
- Incremental fetch after pushes 1/10/100 planned 37/1,304/4,719 logical
  objects, omitted 1,637,163/1,637,200/1,638,492 objects, and generated
  40,695/3,355,559/8,549,205 response bytes in 158/302/485 ms of server
  operation time. Full cold clone generated 1,241,522,108 response bytes in
  181,352 ms of pack generation; warm clone was a 208 ms cache hit.
- Blobless selection planned 1,102,159 objects in 2,139 ms and transferred
  198,715,382 bytes. Depth-1/10/100/1,000 selected 60,062/88,052/1,001,853/
  1,643,211 logical objects, planned in 125/16/344/448 ms, and transferred
  49,318,431/94,912,761/814,844,514/1,241,871,926 response bytes. Depth-1/10
  used 6,966/7,453 storage requests; depth-100/1,000 used three each.
- The owner checkpoint took 676,232 ms and reached 1,871,806,464 bytes peak
  child RSS. This is a measured maintenance bottleneck, not an acceptance
  threshold: the implementation has the intended bounded artifact shape, but
  owner latency and memory need optimization before large-team rollout.
- The report records `generation_receipt_valid: false` and
  `repair_required: true`, with notes that the generation-index receipt is
  missing and bucket registry discovery is incomplete. The harness correctly
  keeps destructive bucket GC disabled, but production readiness requires this
  state to be explained, repaired, or explicitly made a non-error invariant
  with tests.

The long-history failure mode addressed by the current branch was cumulative
transition eviction: after enough fast-forward edits, retaining only the last
64 transition records dropped old haves and made exact incremental planning
fail. The current `incremental_history` path retains a bounded long-history
window, persists it through the schema migration, and uses sorted/deduplicated
visibility positions. Regression tests cover the 100-edit history and exact
planning beyond the former 64-transition boundary.

### 1,000-push qualification

Run profile: `local-k8s-full-history-a926-1000-20260825`, same Kubernetes
revision and isolated RustFS, 1,000 replay pushes, release binary built from
pre-rebase commit `a926fe9d`. The report status is `ok`; the standalone
verifier passes with `--allow-smoke`, all 1,001 pushes (seed plus 1,000
replays) completed, the full/filtered/shallow/incremental correctness checks
passed, and cleanup removed the local worktrees and run-owned remote prefix.
The correctness fingerprint is
`7d97627cf1f4de8b87679dea53d99916df42c3152dc765399d4494c43af09624`.

- The final inventory had 2 active packs and 1,263,069,674 active bytes.
  Owner actions at checkpoints 10/100/1,000 were
  `ref_journal_compaction`, `catalog_advance`, `commit_graph_rebuild`,
  `geometric_repack`, `catalog_advance`, and `none`; the checkpoint-1 owner
  omitted the repack because it was not due.
- Owner convergence took 192,870/445,348/723,410/2,580,374 ms at
  checkpoints 1/10/100/1,000 and peaked at 944,160,768/959,594,496/
  1,014,546,432/1,974,976,512 bytes of child RSS. The 43-minute,
  approximately 1.975-GB checkpoint-1,000 owner is the dominant measured
  large-team bottleneck; it is evidence for optimization, not an accepted
  production SLO.
- Cold full clone generated 1,240,981,438 response bytes in 134,561 ms of
  pack generation with two storage requests; the warm clone was a 307 ms
  verified cache hit. `blob:none` generated 198,746,774 response bytes in
  89,404 ms. Depth-1/10/100/1,000 generated
  49,108,396/91,467,386/812,827,443/1,240,938,129 response bytes in
  20,544/30,377/139,482/119,581 ms, with 6,187/7,276/3/3 storage requests.
- Incremental fetch after pushes 1/10/100/1,000 planned
  37/420/4,810/38,745 logical objects and generated
  22,589/454,861/5,805,513/39,942,282 response bytes using 2/6/33/225
  storage requests. Server operation time was 166/422/695/2,504 ms.
- Every acceleration checkpoint reported current locator, visibility, and
  commit-graph artifacts, but also `generation_receipt_valid: false` and
  `repair_required: true`; the notes identify a missing generation-index
  receipt and incomplete bucket-registry discovery. This remains an explicit
  production-readiness gap and destructive bucket GC stayed disabled.

This section is retained as a pre-rebase historical comparison only. The
current committed full-profile result is recorded above; independent
repeatability and the remaining differential, fault, concurrency, provider,
and rollout gates are still open.

## Final done criteria

All conditions must hold before this roadmap is marked DONE:

- [ ] Every phase acceptance checklist is complete with linked evidence.
- [ ] Kubernetes full clone, partial clone, shallow clone, incremental fetch,
      and 1,000-commit replay pass exact correctness checks.
- [ ] Pack count, object-catalog layers, and commit-graph layers remain
      logarithmic under sustained pushes.
- [ ] Push acknowledgement does not perform repository-wide visibility,
      catalog, graph, or pack maintenance.
- [ ] Fetch negotiation uses runtime bitmap algebra when current indexes exist.
- [ ] Response packs preserve reusable deltas and repeated clones use a
      generation/authorization-bound cache.
- [ ] Repository owner automatically maintains all derived artifacts within
      bounded budgets and is interruption-safe.
- [ ] GC retains current state, recovery history, workflow roots, and grace
      objects under all tested interleavings.
- [ ] Full workspace tests, formatting, clippy, and supported compatibility
      gates pass.
- [ ] Operator and user docs describe defaults, diagnostics, maintenance,
      recovery, and performance expectations.
- [ ] No source files outside the active phase's declared scope changed.
- [ ] `plans/README.md` and the phase evidence table are updated.

## Maintenance notes

- Reviewers should scrutinize generation binding and authorization before
  performance numbers. A faster stale or cross-scope read is a correctness
  failure.
- Keep the object catalog, visibility proof, graph, and generated clone packs
  as derived artifacts. The manifest, refs, immutable pack objects, and
  recovery roots remain authoritative.
- Revisit geometric factors and maintenance budgets only from report evidence.
  Avoid turning benchmark-specific thresholds into public configuration.
- Changed-path Bloom filters, packfile URI protocol support, and multi-region
  cache placement are deliberate follow-ups unless Phase 0/6 evidence makes
  one necessary for the stated acceptance criteria.
- If Git's newer incremental MIDX becomes attractive as a subprocess boundary,
  first propose a separate minimum-Git compatibility decision and CI matrix;
  do not add a hidden version-dependent behavior.
