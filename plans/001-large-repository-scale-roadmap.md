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
now complete for one K8s/RustFS run and remains a single-run result; repeatability,
the 10,000-push differential, fault, provider, concurrency, and rollout gates
are still open.

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

The 2026-08-26 follow-up is committed as `c57ee1f4`:

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
- Owner publication verifies pack indexes and scans only new or rebound pack
  bodies once to populate optional kind metadata; covered stable pack bodies
  are not downloaded. Filtered reads retain the canonical bounded traversal
  path when kinds are absent.
- `repair_required` no longer treats incomplete bucket-wide discovery as
  repository-local repair failure. The bucket-wide state remains visible in
  diagnostics and destructive bucket GC remains disabled until its separate
  completeness gate is proven.

Focused source proof for this follow-up passes formatting, `cargo check -p
crab`, 30 catalog-visibility tests, 39 locator tests with one intentional
stress test ignored, five repack tests, and 23 upload-pack tests. The fresh
release-binary Kubernetes/RustFS run, 10,000-push differential, and team-scale
fault/provider/concurrency evidence are intentionally not claimed by this
commit.

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
transition-bitmap fix at `01d588ea`; lazy catalog follow-up at `cbe848f4`):

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
- selected-object response repacking for dense type-only filters, with exact
  generated-object-set verification;
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
- a large-batch locator scan policy that replaces tens of thousands of exact
  OID point reads with one bounded, read-ahead scan while retaining the exact
  lookup path for sparse or small requests.
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

Still required before the roadmap is DONE:

- an independent repeatability full-profile report from the current lazy binary
  after the large-batch locator scan change;
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
  owner publication path now avoids a full stable-pack body scan when only
  locator rows are needed, but intentional O(N) owner repair/rebuild, missing
  kind fallback, migration/compaction, graph, and repack paths still have
  latency and memory budgets open;
- the post-lazy depth-1/10 planner measured 11,659/15,553 ms before the
  large-batch scan change. Re-run those depths with `c57ee1f4` and require
  locator lookup-mode telemetry to prove the bounded scan removes the
  point-read wave without increasing full-clone or incremental-fetch latency;
- catalog-filter planning now reads the additive ordinal-keyed metadata
  sidecar, filters ordinals, and resolves only retained OIDs. Existing or
  incomplete sidecars still use the bounded canonical fallback, so the
  implementation gap is closed but the large-closure request/latency SLO still
  needs fresh current-binary evidence;
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
| 1 | IMPLEMENTED; POST-LAZY NORMAL-PATH PROOF PASS; SLO PENDING | PR #75, follow-up PR #87 | `lazy-cbe848f4-1000-20260825`; [released-shape workflow](https://github.com/crabbuild/crab-oss/actions/runs/32917566230) | `7ff92545`; `01d588ea`; `cbe848f4`; `c57ee1f4` | Normal seed/full/warm/blobless/shallow helper logs contain no `catalog_materialization`; owner maintenance intentionally does. Dense ordinal resolution has bounded read-ahead, and the large-batch locator scan is source-verified but needs a fresh release run |
| 2 | IMPLEMENTED; CURRENT SINGLE-CLIENT EVIDENCE; SLO PENDING | PR #75, follow-up PR #87 | `lazy-cbe848f4-1000-20260825` | `7ff92545`; `c57ee1f4` | Final inventory is 2 active packs with 1,263,705,690 bytes; cold/warm full clones are 199,311/72,101 ms, blobless is 110,384 ms, and depth-1/10/100/1,000 are 45,570/58,702/174,452/191,898 ms. Physical history has 1,004 immutable pack objects, while the active inventory remains 2; suffix reuse is covered by focused repack tests, but response-pack egress, fanout, and provider SLOs remain open |
| 3 | IMPLEMENTED; CURRENT OWNER EVIDENCE; SLO PENDING | PR #75, follow-up PR #87 | `lazy-cbe848f4-1000-20260825` | `7ff92545`; `c57ee1f4` | Owner convergence at checkpoints 1/10/100/1,000 was 146.947/244.227/373.872/2,729.094 s; checkpoint 1,000 used six passes, ended at 2 active packs, peaked at 1.837 GB RSS, and read 1,478,153,950 bytes. Owner publication now skips stable pack-body downloads for optional kinds, but maintenance latency, memory, and full rebuild paths remain large-team bottlenecks |
| 4 | IMPLEMENTED; POST-LAZY FETCH PASS; SHALLOW/DIFFERENTIAL SLO PENDING | PR #75, follow-up PR #87 | `lazy-cbe848f4-1000-20260825` | `7ff92545`; `cbe848f4`; `c57ee1f4` | Incremental fetch at pushes 1/10/100/1,000 used 51/60/74/662 ms of visibility planning and 2/6/33/226 requests; shallow planner times were 11,659/15,553/2,405/2,634 ms for depth 1/10/100/1,000 before the large-batch scan policy. The ordinal handoff covers sequential same-ref edits in focused tests; 10,000-pair differential, response-pack SLO, concurrency, and rollout evidence remain open |
| 5 | IMPLEMENTED; CURRENT EVIDENCE; OPERATIONAL GAPS PENDING | PR #75, follow-up PR #87 | `lazy-cbe848f4-1000-20260825` | `7ff92545`; `c57ee1f4` | Current-manifest GC roots retain pending catalog handoffs, and repo-local `repair_required` no longer conflates incomplete bucket-wide discovery with repair. Repository GC now includes grace-aware generated response-pack cache retention and force cleanup; interruption, receipt/registry completeness, 10,000-push, and full GC matrix remain pending; bucket-wide destructive GC stays disabled |
| 6 | PARTIAL | PR #75, follow-up PR #87 | `lazy-cbe848f4-1000-20260825` | `7ff92545`; `c57ee1f4` | Current single-client correctness and warm-cache checks pass; fresh post-follow-up K8s evidence, shallow planner optimization, large-team concurrency, fault, cache-server, provider, owner-failover, and canary gates remain pending |

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

The latest qualification-contract follow-up (`0a8e4aa8`) additionally passes
the Python E2E harness suite (`30` tests). Its post-push terminal fetch proves
that the generation-owner admission path completes the expected locator and
visibility repair before the filter matrix, so later filtered reads are
measured against a stable canonical remote rather than conflating repair with
steady-state read behavior.

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
