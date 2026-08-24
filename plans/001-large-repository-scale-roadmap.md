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
- **Current draft PR**: https://github.com/crabbuild/crab-oss/pull/75

### Current execution state

The implementation work for Phases 1 through 5 is assembled on one draft
integration branch so reviewers can inspect the complete generation-binding
contract across push, read, maintenance, and GC. This intentionally does not
mark those phases accepted: their Kubernetes/RustFS performance thresholds,
long-running fault matrix, and rollout gates still require dedicated evidence.
The branch must remain a draft until those gates are either recorded here or
split into explicitly tracked follow-up work approved by maintainers.

Implemented on the current branch:

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

Still required before the roadmap is DONE:

- a fresh, publishable Kubernetes/RustFS report from the current branch;
- the 1,000-push growth and latency comparison for catalog and graph layers;
- 10,000 deterministic Kubernetes ancestry pairs and depth-1/10/100/1,000
  shallow differential proof;
- the complete Phase 5 interruption and 10,000-push maintenance matrix;
- concurrent fetch/push, cache-server fanout, throttling, and owner-failover
  scenarios from Phase 6;
- supported-provider compatibility, sustained canary, and default-on rollout.

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
| 0 | PARTIAL | PR #75 | `local-k8s-multipack-fixed2-20260824` (interrupted) | `fa779130` | Current working-tree run proves seed/incremental/full-clone correctness and cleans its prefix; repeatability, 1,000 replay, and partial-clone gates remain pending |
| 1 | IMPLEMENTED; QUALIFICATION PENDING | PR #75 | `local-k8s-multipack-fixed2-20260824` | `4c771baa`, `ac74dad1`, `acbe1da8` | Bitmap runtime and bounded incremental visibility publication pass unit/integration and Rust 1.98 strict-lint proof; full filtered-planning RSS/latency gate pending |
| 2 | IMPLEMENTED; PARTIAL EVIDENCE | PR #75 | `local-k8s-multipack-fixed2-20260824` | `d2a4c97d` through `0bddea98` | Two-pack consolidation produced a verified 1.244 GB response; warm clone hit the cache; CPU/egress and filtered/shallow gates pending |
| 3 | IMPLEMENTED; QUALIFICATION PENDING | PR #75 | `local-k8s-multipack-fixed2-20260824` | `d5090649` | Catalog advance was 6.176 s at seed and 802 ms after one incremental pack; 1,000-push layer/publication drift gate pending |
| 4 | IMPLEMENTED; QUALIFICATION PENDING | PR #75 | `local-k8s-multipack-fixed2-20260824` | `9bb558a6` | Complete graph rebuilds covered 140,381/140,383 commits; 10,000-pair and shallow differential/performance gate pending |
| 5 | IMPLEMENTED; OPEN DEPENDENCY BLOCKER | PR #75 | `local-k8s-multipack-fixed2-20260824` | `9bb558a6` | Owner sequencing works, but SlateDB 0.14.1 `sst_iter` cancellation panics recur during scans; interruption/10,000-push matrix pending |
| 6 | PARTIAL | PR #75 | `local-k8s-multipack-fixed2-20260824` | `0a8b5c8f`, `b7749b2e` | Full cold/warm single-client clones pass; `blob:none` planning exceeds six minutes; concurrency, fault, cache-server, provider, and canary gates pending |

### Current branch verification evidence

The following proof was run with
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-roadmap`:

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
pre-existing warnings in untouched `crab-vfs` code. The full Kubernetes/RustFS
qualification and Phase 6 rollout evidence are also not yet complete, so this
table is an implementation checkpoint rather than phase acceptance.

### Fresh Kubernetes/RustFS evidence from the current implementation

Run profile: `local-k8s-multipack-fixed2-20260824`, source Kubernetes `b3bc2ac5`,
isolated RustFS, one replay push, current working tree release binary. The run
was intentionally stopped at the partial-clone probe after the probe spent
more than six minutes in the current traversal planner without producing a
response; its isolated remote prefix was cleaned successfully.

- Seed import created a 1,643,202-object, 1,263,633,295-byte pack. The seed
  generation owner advanced the catalog in 6,176 ms, published visibility in
  143,481 ms, and rebuilt the 140,381-commit graph in 72,010 ms.
- The first incremental push committed two commits and nine new objects in
  2,950 ms in the report (2,511 ms in the structured push result); the
  ref-journal phase was 237 ms and no catalog, visibility, graph, or pack
  maintenance ran before acknowledgement.
- The next owner cycle compacted the ref journal first; catalog advance for the
  two-pack generation completed in 802 ms and graph rebuild completed in
  109,410 ms for 140,383 commits. SlateDB 0.14.1 emitted repeated
  `sst_iter.rs:453` `JoinError::Cancelled` panics during owner scans while the
  command still exited zero. This is an open production blocker, not accepted
  behavior.
- The two-pack cold full clone selected
  `complete_pack_consolidation`, verified 1,643,211 objects, produced a
  1,244,177,064-byte response in 89,078 ms of pack generation, and passed
  `git fsck --full`. The warm clone was a generated-pack cache hit, with
  14,636 ms server-side response generation, and also passed `git fsck --full`.
- The `blob:none --no-checkout` clone remains unaccepted: the current
  filter-aware planner falls back to per-object commit/tree traversal, stayed
  CPU-bound for more than six minutes without response bytes, and was
  interrupted. A type-aware catalog or equivalent filtered-pack planning path
  is required before partial-clone SLOs can pass.

This evidence proves the push-acknowledgement boundary and complete multi-pack
response path, but it does not satisfy the 1,000-push, shallow differential,
partial-clone, owner-fault, fanout, provider, or canary gates.

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
