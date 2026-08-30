# Plan 003: Persist and consume complete shard closures

> **Hard-cutover rule**: consume only the canonical Crab-owned shard v1
> contract established by Plan 013. Delete v2 fixtures/readers rather than
> qualifying both formats.

> **Executor instructions**: Follow every step and verification gate. Stop and
> report if a closure cannot be proven complete; do not silently fall back to a
> full shard scan for destructive GC. Update `plans/README.md` when complete.
>
> **Drift check (run first)**:
> `git diff --stat b738f3b2..HEAD -- crab/src/cmd/gc/bucket.rs crab/src/cmd/gc/mod.rs crates/crab-xet/src/shard.rs crates/crab-metadata/src/ref_registry.rs crates/crab-storage/src/layout.rs crab/src/git/push.rs crates/crab-auth-server/src/receive crab/src/restripe crab/src/cmd/recover.rs`
> Reconcile every Current state excerpt before proceeding.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/001-close-writer-gc-fence.md`,
  `plans/002-durable-bounded-gc-engine.md`, and
  `plans/013-dependency-closed-shard-partitioning.md`
- **Category**: perf
- **Planned at**: commit `b738f3b2`, 2026-08-22

## Delivery status

Partial implementation in this branch: immutable strict closure sidecars are
published at current shard writers, consumed by destructive bucket GC, and
cleaned when their source shard is deleted. The repair/backfill path is obsolete
under the no-user hard cutover and must be deleted. Sidecar decode and concurrent
closure reads have explicit byte budgets. Coverage markers, segmented
closures, and durable repair progress are still pending; oversized closures
fail closed until segmentation is delivered.

## Why this matters

Bucket GC currently downloads every referenced shard body on every run and
parses it twice to discover live xorb and file hashes. That makes recurring GC
read I/O proportional to all retained data, not to changed roots, and keeps
large shard bodies in memory during the mark phase. The current shard writer and
reader already know the complete relationships; persist that verified closure
once from repository genesis and make closure coverage a
hard deletion precondition.

## Current state

- Bucket GC derives `referenced_shards` from current and historical manifests,
  then calls `extract_hashes_from_shards` for every retained shard
  (`crab/src/cmd/gc/bucket.rs:203-327`, `:748-842`). The function downloads the
  entire body, verifies the Merkle hash, reads xorb blocks, then reads file-info
  sections from the same bytes.
- `crates/crab-xet/src/shard.rs:168-390` currently contains v1/v2 reader
  branches. Plan 013 hard-cuts this to canonical v1 before closure work. A
  bloom is only a negative prefilter; it is not a complete xorb/file relationship.
- Current push sessions produce and upload shards in
  `crab/src/git/push.rs:1100-1188` and track uploaded hashes before manifest
  publication (`:7160-7260`). Protected receive and recovery/restripe have
  independent shard upload paths.
- `RefRegistry` records current shard sets, completeness, generation, workflow
  roots, and active-active registrations (`crates/crab-metadata/src/ref_registry.rs:44-188`),
  but has no closure coverage marker.
- Existing registry repair is explicit and fail-closed for incomplete coverage
  (`crab/src/cmd/gc/bucket.rs:968-1010`). Use the same administrative boundary;
  do not make a missing sidecar an implicit permissive fallback.

## Closure contract

Define a canonical-v1 immutable `ShardClosureManifest` keyed by the exact shard
content hash. It must contain:

- schema/parser version, shard hash, byte length, and verified content digest;
- total xorb entries and file entries;
- an ordered set of bounded closure segment objects containing every xorb hash
  and every file-hash → shard relationship needed by current file-index GC;
- segment count, per-segment digest, total Merkle/root digest, and creation
  generation; and
- no mutable ref, user credential, or provider-specific path.

Store closure manifests and segments in the current global `.crab` namespace
through `StoreLayout` helpers (for example a hash-partitioned
`shard-closures/` family; choose and freeze the exact path before writing).
Use create-if-absent/verified content-addressed writes so retries cannot change
an existing closure. Closure segments must have a fixed row/byte ceiling; GC
streams them through Plan 002 rather than building another complete set.

The closure is authoritative only when the source hash, segment digests,
counts, and schema are all validated. A closure that omits one relationship is
corrupt, not “best effort.”

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Shard tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-xet --locked shard` | canonical v1 reader/writer tests pass; non-v1 rejected |
| Metadata tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-metadata --locked ref_registry` | coverage/CAS tests pass |
| GC closure tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked shard_closure` | closure and no-body-GET tests pass |
| Bucket GC tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked cmd::gc::bucket` | bucket suite passes |
| Format | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo fmt --all -- --check` | exit 0 |

## Scope

**In scope**:

- `crates/crab-storage/src/layout.rs` (global closure paths)
- a current-layout closure contract in `crates/crab-metadata/` or
  `crab/src/cmd/gc/` with bounded segment I/O
- `crates/crab-metadata/src/ref_registry.rs` (coverage marker/identity)
- direct push, protected receive, recovery restore, restripe reconcile, and
  any other production shard writer from Plan 001's writer map
- `crab/src/cmd/gc/bucket.rs`, `crab/src/cmd/gc/mod.rs`, CLI wiring, and tests
- canonical-v1 initialization/registry coverage that begins complete at genesis
- current storage/recovery/web documentation

**Out of scope**:

- changing immutable xorb payload bytes or canonical shard-v1 semantics
- replacing the manifest or ref-registry as the root of truth
- using a bloom filter as a complete closure
- PB shard/recipe/partition layouts
- retaining an unbounded full-shard fallback for destructive GC

## Steps

### Step 1: Implement and verify the immutable closure format

Add typed manifest and segment records with strict validation: lowercase
content hashes, bounded counts, exact byte totals, no duplicate relationships,
and matching source/segment digests. Provide streaming encode/decode APIs that
never require all file entries in one `Vec`. Add canonical path builders and
content-addressed create-if-absent writes through the current `Store`.

Write canonical v1 fixtures from `crab-xet` and assert that the closure contains
every xorb block and every file-info entry. Include empty and multi-segment
shards, duplicate entries, malformed hashes, truncated segments, wrong source
hash, count mismatch, and future schema cases.

**Verify**: `cargo test -p crab-xet --locked shard` and
`cargo test -p crab-metadata --locked shard_closure_format` pass; corrupt or
incomplete closure data is rejected before any candidate is returned.

### Step 2: Produce closures at every canonical shard publication boundary

Generate the closure from the same verified shard bytes/entries used by each
writer, before the root publication that makes the shard reachable:

- direct push: after shard bytes are finalized and verified, before the
  ref-registry union/manifest CAS;
- protected receive: in the service-owned promotion path before service
  manifest publication;
- recovery restore and restripe reconcile: before their manifest/file-index CAS;
- replication/materialization and any writer-map path that publishes a shard:
  produce or copy the verified closure and verify its source hash at the
  destination.

Acquire the Plan 001 shared writer lease for global content and keep it through
closure + registry/manifest/coordinator publication. Do not write a closure
after the root is visible. If closure generation fails, fail the writer before
publication; an extra uploaded shard may remain grace-protected, but an
unproven root must not become current.

**Verify**: per-writer tests show a manifest/ref-registry publication is never
visible without a valid closure; retrying the same shard is idempotent;
closure-generation errors leave no new committed root. Run the coordination,
receive, GC, and writer tests from Plan 001.

### Step 3: Make closure coverage complete from genesis

Extend canonical `RefRegistry` v1 with a required closure coverage identity
(digest, generation, and covered shard-root frontier). Repository initialization
starts with complete empty coverage; every shard writer atomically advances it.
Delete repair/backfill commands, serde defaults, and missing-field compatibility
branches. A non-v1 or incomplete registry fails closed and the development
repository must be reinitialized.

**Verify**: tests cover empty genesis, current/history roots, multi-repo shared
shards, cancellation, process death, duplicate retry, missing/corrupt shard,
registry CAS conflict, and a concurrent writer. Incomplete/stale coverage makes
destructive GC fail before any deletion.

### Step 4: Replace bucket shard downloads with closure streaming

Change `run_bucket_gc_under_maintenance` so the mark phase loads and validates
closure manifests/segments for every referenced shard and streams xorb/file
relationships into the Plan 002 mark set. The normal destructive path must not
call `extract_hashes_from_shards` for a shard with complete coverage. A missing,
corrupt, schema-unknown, or coverage-mismatched closure returns a structured
configuration/corrupt-object error before any delete batch. Do not silently
download the shard as a fallback.

Retain full shard verification for `fsck`. Keep
closure objects live while their source shard or historical root is live; add
closure candidates to bucket GC only when no corresponding shard root remains,
using the same grace/fence/run journal as other global objects.

**Verify**: an instrumented object store asserts zero referenced-shard body GETs
for complete coverage and at least one GET for closure repair. Tests prove
missing/corrupt coverage aborts before deletion, while an orphan closure is
deleted only after its source shard/root is safely gone.

### Step 5: Maintain file-index correctness without unbounded maps

Feed closure file-hash segments into `gc_file_indexes` in bounded sorted
streams. Preserve the invariant that every file hash reachable from every
retained shard version is included. For a file appearing in multiple shards,
keep all valid placements until the current `file_index_db` CAS/tombstone
contract decides what is stale. Ensure every MetaDb guard closes on success,
cancellation, and error.

**Verify**: multi-shard files, repeated hashes, historical-only shards,
empty shards, and partial closure segments produce the same file-index result
as the current full-body reference implementation; the reference comparison
passes without changing the existing `chunks_for_file`/reconstruction
invariants.

## Test plan

- Strict closure format/property tests for canonical v1 shards and segmented streams.
- Writer publication tests for direct, protected, recovery, restripe,
  replication, and any newly discovered shard writer.
- Genesis coverage/failure/CAS tests with multiple repositories.
- Instrumented stores measuring referenced-shard body GETs and closure GETs.
- Differential GC tests comparing closure mark results with full `ShardReader`
  extraction on generated multi-shard repositories.
- File-index/MetaDb close and cancellation tests.

## Done criteria

- [ ] Every canonical shard publication writes one verified immutable closure
      before root publication; no writer-map row is exempt without a documented
      read-only reason.
- [ ] Canonical v1 repositories have complete closure coverage from genesis;
      non-v1 repositories are rejected and reinitialized.
- [ ] Destructive bucket GC fails closed on missing/corrupt/stale coverage and
      performs zero referenced-shard body GETs when coverage is complete.
- [ ] Closure/file-index streams obey explicit row/byte/memory budgets.
- [ ] Closure objects are retained and collected with the same root/grace/fence
      rules as their source shards.
- [ ] All focused GC/metadata/xet tests and format checks pass.
- [ ] No shard bytes, PB layout, or reconstruction authority changed.

## STOP conditions

- The closure cannot represent every canonical v1 xorb and file relationship
  without changing shard bytes or using an unbounded record.
- A writer can publish a shard root before its closure or cannot acquire the
  Plan 001 global fence.
- Genesis/writer publication cannot distinguish complete from partial coverage after interruption.
- A closure mismatch would be handled by a permissive full-shard fallback in a
  destructive run.
- A file-index differential test disagrees with full-shard extraction.
- Any test/format gate fails twice after a reasonable fix attempt.

## Maintenance notes

The closure schema, source-hash binding, segment limits, and coverage marker are
persistent contracts. New shard format versions must ship a closure decoder and
backfill fixture before GC can accept them. Reviewers should check that closure
generation is before publication, that global/repo fences cover it, and that
the zero-body-GET claim is measured rather than inferred.
