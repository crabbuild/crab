# Plan 003: Stop downloading every referenced shard during current bucket GC

> **Executor instructions**: Execute in order and run each verification gate.
> Stop on any STOP condition. Update this plan's status in `plans/gc/README.md`
> when complete.
>
> **Drift check (run first)**:
> `git diff --stat ff7792a4..HEAD -- crab/src/cmd/gc/bucket.rs crab/src/git/push.rs crab/src/metadata/shard_sync.rs crates/crab-auth-server/src/receive crates/crab-metadata/src/ref_registry.rs crates/crab-xet/src crates/crab-storage/src/layout.rs crab/src/cmd/doctor.rs crab/src/main.rs`

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/gc/001-writer-admission.md`,
  `plans/gc/002-bounded-resumable-engine.md`
- **Category**: perf, migration
- **Planned at**: commit `ff7792a4`, 2026-08-22

## Why this matters

Bucket GC currently downloads and materializes every referenced shard to learn
which xorbs and file-index rows are live. That makes recurring GC read cost
proportional to all retained reconstruction metadata, not garbage. This plan
publishes a compact, content-addressed closure beside each shard and requires an
explicit verified backfill for existing repositories before destructive GC can
use it.

## Current state

- `crab/src/cmd/gc/bucket.rs:741-835` calls `get(...).bytes()` for every
  referenced shard, verifies the shard hash, parses all xorb blocks and file
  sections, then `try_collect`s every result.
- `crab/src/cmd/gc/bucket.rs:837-891` needs the resulting file-hash sets to
  tombstone unreferenced file-index rows.
- `crates/crab-metadata/src/ref_registry.rs:44-79` stores only
  `repo -> Vec<shard_hash>` plus completeness metadata. It has no durable proof
  that a closure object exists for each registered shard.
- `crab/src/git/push.rs:7165-7193` union-registers candidate shard hashes before
  manifest publication. Protected receive performs equivalent promotion and
  registration in `crates/crab-auth-server/src/receive/`.
- `crab/docs/architecture/object-storage-layout.md` declares shards immutable,
  content-addressed global objects and the ref-registry the GC root. Keep the
  closure derived and immutable; it must not become a second reconstruction
  authority.

Target contract for Unified layout:

- A `ShardGcClosureV1` is generated from exactly the bytes used to create one
  shard. It contains shard hash, sorted/deduplicated xorb hashes, sorted/
  deduplicated file hashes, counts, format version, and body digest.
- The closure body is immutable and addressed by its BLAKE3 digest under a
  canonical global GC namespace.
- Ref-registry entries bind `shard_hash -> closure_digest`; union registration
  happens before manifest publication alongside the shard root.
- Destructive bucket GC accepts a closure only when the registry is schema
  current, coverage complete, the body digest matches its key, its embedded
  shard hash matches the registry entry, and every retained shard has exactly
  one binding.
- Existing data is backfilled by an explicit doctor/migration action that reads
  and verifies the old shard once. There is no destructive runtime fallback to
  downloading missing shards.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Codec tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-metadata --locked shard_gc_closure` | all matching tests pass |
| Writer tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked shard_gc_closure` | all matching tests pass |
| Protected writer tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-auth-server --locked shard_gc_closure` | all matching tests pass |
| Bucket GC tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib cmd::gc::bucket --locked` | all bucket GC tests pass |
| Format | `cargo fmt --all -- --check` | exit 0 |

## Scope

**In scope**:

- `crates/crab-metadata/src/shard_gc_closure.rs` (create), `ref_registry.rs`,
  `lib.rs`, and errors
- `crates/crab-storage/src/layout.rs`
- the canonical shard creation/parser boundary in `crates/crab-xet/`
- direct push shard publication in `crab/src/git/push.rs` and
  `crab/src/metadata/shard_sync.rs`
- protected receive shard promotion under `crates/crab-auth-server/src/receive/`
- `crab/src/cmd/gc/bucket.rs` and standalone GC Plan 002 root adapters
- `crab/src/cmd/doctor.rs`, `crab/src/main.rs`
- object-layout, doctor, and GC docs

**Out of scope**:

- Making closures authoritative for file reconstruction.
- Keeping a permanent read-the-shard fallback in destructive GC.
- Repairing a corrupt shard from a closure.
- Any unimplemented remote layout, recipe-tree format, or layout migration. This
  plan covers the current shard/xorb/file-index layout only.
- Deleting old shards or xorbs during backfill.

## Git workflow

- Branch: `gc/003-shard-closures`
- Commits: closure codec/layout, writers/registry, verified backfill, collector.
- Do not run a live bucket GC.

## Steps

### Step 1: Define and prove one streaming closure codec

Add a versioned streaming codec in `crab-metadata`; keep shard parsing in
`crab-xet`. Use the repository's existing hash and serialization dependencies;
do not add an alternate metadata database. Encoding must be deterministic for
equivalent input order, bounded by configured bytes, and stream-decodable into
standalone GC Plan 002 partitions. Reject unsorted/duplicate rows if the format promises
canonical order, invalid hashes, count overflow, trailing bytes, digest
mismatch, and unknown versions.

Add a differential property test: for generated valid shards, the closure's
xorb and file sets exactly equal the existing full shard parser's sets. Corrupt
or truncated shards cannot produce a closure.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-metadata --locked shard_gc_closure_codec`
→ round-trip, canonicalization, corruption, and differential properties pass.

### Step 2: Bind closures into the ref-registry root contract

Extend the registry schema so a repo's registered shard roots carry their
closure digest. Preserve union semantics under concurrent pushes. Completeness
now means bucket discovery is complete, every registered repo entry is exact or
conservative as documented, and every shard root has a valid closure binding.
An older or partially backfilled registry remains readable for diagnostics but
fails destructive GC.

Add canonical fan-out key construction in `StoreLayout`; callers do not build
the global prefix. Document mutability, ownership, reachability, and cleanup in
the object-layout contract. Closure objects are themselves reachable while any
registry/history root binds them.

**Verify**:
registry tests cover concurrent union, schema upgrade, missing/duplicate
binding, conservative extra roots, deregistration, and coverage completion.

### Step 3: Publish closures on every shard writer

At the common shard-finalization boundary, derive the closure once, upload it
create-only, verify an existing same-key body on retry, then upload/verify the
shard and union-register the binding before manifest/ref publication. Use Plan
011 writer admission for the global domain. Direct push, protected receive,
import/recovery shard restoration, and replication/restripe shard writers must
all use the same helper or explicitly prove why they cannot create a newly
referenced shard.

Do not make each caller independently parse and encode. One shared publication
helper must own ordering and idempotency. A closure conflict or missing binding
fails before refs move.

**Verify**: run both writer test commands → each production shard writer either
publishes a verified closure binding or is proven read-only; fault injection at
closure put, shard put, registry CAS, and manifest CAS is retry-safe.

### Step 4: Add explicit verified backfill

Add a dry-run-first doctor action that enumerates registered current and
historical shard roots, reports missing closure bindings, and estimates read/
write bytes. Apply mode obtains writer admission, downloads each missing shard
once with bounded byte and concurrency budgets, verifies its content hash,
derives/uploads the closure, and CAS-unions the binding. Persist partition
progress so cancellation resumes. Only a complete final reconciliation may
advance registry schema/coverage.

Do not modify or delete shard/xorb bodies. A missing or corrupt shard is an fsck
failure and blocks completion; it is not papered over with an empty closure.

**Verify**:
tests cover dry-run zero writes, crash/resume, concurrent push union, corrupt or
missing shard, closure conflict, final count/digest reconciliation, and
idempotent rerun.

### Step 5: Switch bucket GC to closure streams and delete the old path

Replace `extract_hashes_from_shards` with a standalone GC Plan 002 root source that streams
closure rows directly into external partitions. Verify every binding and body
while reading. File-index GC consumes the file-hash stream through a bounded
partitioned interface; it must not rebuild per-repo complete `HashSet`s.

If any retained shard lacks a valid closure, dry-run reports the exact backfill
requirement and destructive execution fails before journal seal. Delete the
full-shard-download collector after the backfill command exists; do not keep it
as a hidden fallback.

**Verify**:
bucket tests prove destructive GC performs zero shard-body GETs when closure
coverage is complete, refuses incomplete coverage, preserves all referenced
xorbs/file rows, and remains bounded above RAM. Run the full bucket test and
format commands.

## Test plan

- Codec differential/property tests against real shard parsing.
- Registry schema/completeness/concurrent-union tests.
- Every shard writer and every durable boundary.
- Backfill dry-run, resume, corruption, concurrency, and final reconciliation.
- Bucket GC request-count assertion: zero referenced-shard body GETs.
- Synthetic closures above RAM with bounded root streaming.

## Done criteria

- [ ] New shard publication cannot move a visible root without a valid closure
  binding.
- [ ] Existing repositories have a dry-run-first resumable backfill.
- [ ] Destructive GC fails closed on any missing/corrupt/ambiguous binding.
- [ ] Complete-coverage bucket GC performs zero referenced-shard body GETs.
- [ ] File-index reachability is streamed, not collected per repository.
- [ ] The old destructive full-shard fallback is deleted.
- [ ] Targeted tests, bucket tests, format, and `git diff --check` pass.

## STOP conditions

- Closure generation cannot be tied to the exact verified shard bytes.
- A writer surface would publish a shard/ref before its closure binding.
- Backfill would mark coverage complete despite a missing/corrupt shard.
- The proposed format requires loading one entire large closure in memory.
- A runtime compatibility fallback is proposed instead of explicit backfill.
- Registry CAS size/QPS misses the retained current-layout target; address the
  measured limit before adding more unbounded state.

## Maintenance notes

Closure under-reporting is data-loss risk. Review the differential property,
publication order, registry completeness transition, and the absence of old
fallbacks more aggressively than compression or minor throughput details.
