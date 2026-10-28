# crab-write

Shared Capsule publication and captured browse-index mechanics for Crab.
Product callers own authorization, ref leases, dependency proof and GC fencing;
this crate owns conditional publication, recovery and immutable index mechanics.

## Publication flow

```text
caller: authenticate, capture Capsule authority, retain leases and GC fences
  -> capsule_protocol::prepare_publication
  -> capsule_protocol::publish_prepared: resolve conditional ref commit
  -> separate captured browse/checkpoint maintenance under renewed ownership
```

A commit error may be uncertain; resolve that outcome before proceeding or
reporting rejection. Derived read readiness is separate from ref durability.
Stored manifest/ref-journal generation maintenance and live remote-Git opening
have been removed. Retained journal mechanics are separate retirement
work; they do not provide a v1 fallback to Capsule product callers.

Callers with cancellation-sensitive immutable preparation can use
`prepare_publication` then `publish_prepared`, checking their lease token between
them. Publication must be awaited through outcome resolution, even if renewal
fails after that boundary.

`PreparedCapsulePublication::ref_frontier_bytes` reports the exact physical run
bytes in a prepared ref successor, after native compaction. Product callers can
enforce their read budget before publication without estimating append sizes.

A single-ref push commits by conditionally replacing only that ref head.

A multi-ref push creates a unique preparing transaction record, conditionally
prepares each edited head, wins a commit-vs-abort CAS on that record, and
creates an immutable committed marker. A competing writer may abort a still
preparing attempt, but cannot abort a committed one; a committed record with a
missing marker is repaired before its prepared state is used.

The repository root changes only for checkpoint and maintenance work, so distinct existing
refs share no foreground mutable object.

After a failed per-ref conditional write, v2 reads back the exact candidate.
Matching bytes confirm commit even if a lost create reply retried into an
already-exists error. An unchanged original head permits bounded retry of a
transient failure with the original conditional token; this also protects a
late first request from overwriting another writer. Readback allows the larger
of the original and candidate bodies because compaction can shrink the head.

A conflict plus a different readable head confirms a competing writer;
an unreadable result remains uncertain rather than being reported as stale.

Single- and multi-ref publication share this ref-head recovery. Transaction
commit-vs-abort records, root maintenance and rollback keep their own protocols.

Ref-frontier compaction gathers one selected contiguous window, then
performs one final `CapsuleRun::compact` on a blocking worker. Ordinary push and
coordinated repair share this path. The 32-run batching policy bounds interim
frontiers; the normal count target rolls each newest 500-capsule window into
one CRBRUN01 run.

A 64 MiB input-byte budget also triggers prefix compaction;
large historical runs remain stable so appending small commits does not rewrite
repository-sized payloads. The smallest useful merge may contain two inputs
above that soft budget.

If a new leaf would exceed the persisted 64-segment
ceiling, the writer merges the eligible adjacent pair with the fewest bytes.
Every resulting run still contains at most 512 capsules; a frontier with no
eligible pair requires a checkpoint.

Source authentication, immutable upload verification and conditional publication are
preserved. The worker owns immutable data and cannot publish if its caller is
cancelled.

Historical restore publishes a layered checkpoint against the exact fenced
root. Visibility must authenticate every restored ref and peeled tip. Source
validation and protection remain caller responsibilities; the writer retains
the ref-authority epoch, conditional publication and uncertain-outcome checks.
It does not rewrite source packs or publish an embedded checkpoint.

## Captured browse maintenance

`generation::browse::build_graph` consumes a pinned, origin-backed capsule Git
reader and exact snapshot. It persists a reusable attribution prefix before the
orchestrator publishes graph readiness. `build_path_state` then attaches that
verified graph, resumes attribution from its checkpoint and persists progress
every 32 commits. Completion replaces the partial record through the returned
CAS token; cached readers observe both phases without a ref change.

Graph construction writes paged descriptor version 3, reuses unchanged paged layers
and performs bounded-directory carries. The Capsule owner rebuilds an eager browse graph
even when its state digest is otherwise current, preserving reusable attribution
through the existing checkpoint boundary.

Full readback validates graph/page
closure, including recomputed ancestor-set recurrence, before the returned
descriptor can become readiness. Graph discovery
drains a separate bounded operation per batch.

`generation::with_generation_owner` shares renewed owner election and
global/repository GC fences for the Capsule browse orchestrator;
all acquired leases are released on returned failure or cancellation. The
orchestrator rechecks capsule activity and conditionally publishes the small
derived record. None of this is a foreground ref-publication prerequisite.
Generation maintenance never reads a stored v1 manifest or writes a legacy
generation CAS.

## Initialization

`capsule_protocol::initialize` creates the unborn Capsule root for CLI, SDK,
gateway and view-service callers. Existing valid roots are adopted without
changing HEAD. A nonempty prefix without a Capsule root is rejected unchanged;
initialization does not upgrade stored pack-v1 repositories in place. The old
manifest-only repository initializer is removed. The unused initial-manifest and mutable inventory publishers are removed.
Retained journal mechanics remain separate retirement work.

## Journal compaction

`journal::compact_for_owner` folds already committed ref transactions into the
manifest under its renewable lease. It waits up to two lease lifetimes for
handoff, then drains at most five waves before releasing ownership.

`journal::compact_for_reader` skips a busy lease and checks a half-TTL scheduling
budget between batches. These are scheduling bounds, not hard I/O deadlines.

Both use the metadata crate's conditional manifest publication, visibility
compaction and active-marker cleanup. Ref locks are released only when their
holder matches the committed transaction; a successor's lock survives cleanup.
Compacted generations use the shared RFC 3339 timestamp formatter.

Cancellation is checked before admission and between complete waves. Once a
wave starts, it finishes its CAS and cleanup before cancellation is observed.
Lease renewal failure signals cancellation and drains that operation. Both entry
points await lease release on success and error; an operation error remains the
primary error when release also fails. A cancellation result does not roll back
transactions that were already committed.

## Journal commit

`journal::commit_edits` is a retained journal commit primitive. It validates a
complete batch and checks each expected old OID against a caller-supplied snapshot
before writing anything. It then reads per-ref causal parents and uses the metadata
journal's prepared heads and single atomic active marker.

The supplied snapshot
must be captured while holding every edited ref lease, with those leases retained
and renewed through completion; passing an earlier snapshot is not a concurrency
check. Existing committed journal edits count toward the old-value comparison,
even before generation compaction. The function preserves exact new OIDs, tag
peeling, HEAD changes, uploaded pack/shard references and visibility evidence.

`journal::commit_existing_ref_edit` is the bounded fast path for a caller that
retains one journal-backed existing ref's exact visible transaction together
with its old OID. While holding that ref's lease, it verifies the small mutable
head and immutable parent transaction before publishing the update, so its work
is independent of repository pack count. A mismatch returns `RefChanged`. Ref
creation, deletion, multi-ref publication and namespace changes must use
`commit_edits` with a full coherent snapshot.

Latency-sensitive callers can instead retain the opaque state returned by
`capture_existing_ref_commit_base` and pass it to
`commit_captured_existing_ref_edit`. The caller-owned ref lease allows the ref
head and compacted manifest to be captured concurrently, so both manifest-only
and journal-backed existing refs use the bounded path. The final conditional
head write validates that capture without rereading it; the same ref lease must
span both calls.

Creations and deletions additionally hold the renewable `git-ref-namespace`
internal lease, reread the coherent repository snapshot, and validate the final
ref set. Independently locked `feature` and `feature/sub` cannot both commit.

An atomic delete of the parent and creation of its child remains valid. Updates
to existing refs bypass this gate. The CLI's initial-manifest fast path uses the
same gate and rechecks that no journal writer has published during its uploads.

Namespace contention retries stop after two lease lifetimes and observe
cancellation; in-flight storage calls still drain.

`with_ref_namespace` exposes that gate for the initial-manifest publisher. Its
callback must check the supplied cancellation token before publication and finish
commit-outcome recovery once publication is attempted.

The journal checks before
each prepared head and before its active marker: cancellation there rolls back
prepared heads. After the marker attempt it finishes recovery and promotion.

Late renewal/release errors cannot replace a known successful commit result.
Callers must retain and renew their edited-ref leases throughout this work.

## Caller responsibilities

Await journal lifecycle operations to completion; do not abort their task or drop their
future to enforce a deadline. They own stateful writes and lease cleanup. The
caller still owns the generation-owner election and any required GC fences.

For journal commit, callers also own write authorization, individual ref-name/policy and
graph/dependency validation, immutable uploads, visibility proof, and ref leases.

After a failed marker write, the metadata journal attempts bounded exact readback.
Matching marker bytes confirm commit and allow head cleanup to continue. An
absent marker is confirmed only when every edited ref's immutable compaction
frontier descends from the exact transaction ID. If neither proof succeeds, or
the marker is different, oversized or unreadable, `RefJournalCommitUncertain`
retains the transaction ID, original write error and any readback error. No
prepared-head rollback follows a marker attempt.

Callers must reconcile an uncertain outcome before reporting failure. Successful journal commit
means refs are durable, not that the derived catalog is ready for reads.

Current ref values alone cannot establish the historical transaction outcome;
use its bound receipt or retained commit evidence. An unresolved outcome does
not authorize replay, even when the refs match the attempted update.

This crate does not yet own the complete generation service: receive-to-commit,
index receipts and restart repair still need a shared composing path before HTTP
push can acknowledge a fully readable generation.

## Verification

| Contract | Executable evidence |
| --- | --- |
| Renewed owner election and GC admission release after error/cancellation | [Generation tests](tests/generation.rs) |
| Atomic batches, namespace conflicts, holder-safe cleanup, failed compaction and retry | [Journal tests](tests/journal.rs) |
| Capsule initialization, publication and recovery | [Capsule protocol tests](src/capsule_protocol.rs) |
| Original error retained when cleanup also fails | [Error precedence test](src/lib.rs) |

Run these focused tests from the repository root. Replace `crab-write-dev` with
a unique checkout name and verify the workspace volume is mounted and writable,
following root `AGENTS.md`:

```sh
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-write-dev \
  cargo test -p crab-write --locked --lib --test generation --test journal
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-write-dev \
  cargo clippy -p crab-write --locked --all-targets -- -D warnings
```

The tests use local fixtures and in-memory storage; native pack fixtures require
Git. They establish component contracts, not a complete HTTP receive path or
provider qualification. Use the repository's dedicated CI and RustFS qualification
for live storage, race/crash behavior, and cross-platform proof.
