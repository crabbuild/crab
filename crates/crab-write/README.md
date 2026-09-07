# crab-write

Shared Git publication mechanics for Crab. This crate owns catalog publication,
ref-journal commit and compaction extracted from CLI push, reader repair and generation-owner
paths. Those CLI callers use these implementations; HTTP receive can compose it without depending
on the CLI or another server.

`initialize::initialize_repository` owns canonical empty-repository creation for
the CLI and HTTP server. It creates the layout only for an empty repository prefix,
conditionally publishes the generation-zero manifest, and adopts concurrent or
previous initialization after validating the persisted canonical roots.

`catalog::publish_inventory` accepts a caller-owned locator writer, a committed
inventory/coverage anchor and optional validated local index evidence. Missing
evidence is read from storage: the pack trailer, bounded Git index/reverse index,
and optional kind sidecar. The public publication path does not download full
packs, run Git, create an object database or clone a repository.

Current-pack rows are written before obsolete slots are swept. A sweep that
changes the object universe requires rebuilding and replaying the dense ordinal
catalog; repacks retaining every object avoid that rebuild. A caller may defer
rebuild work to the generation owner. Coverage advances only after the current
manifest still matches the supplied generation and pack-index hash.

`LocatorPackEvidence::from_local` validates immutable local index sidecars before
they enter publication. Their files must remain unchanged until the call ends.
The remote path validates the same checksums/counts and uses the same writer.
Storage, metadata, Git, worker and file errors retain their sources.

`generation::maintain_catalog` owns the catalog lease, renewal, planning reader,
writer, checkpoint and close lifecycle. Supply a captured manifest and its complete
pack inventory. The function checks the generation/index/visibility identity before
planning and again after closing the writer. A superseded sample returns `None`;
a current sample returns advancement and catalog/sweep statistics. The CLI owner
reports superseded samples before writing a generation receipt or running later
maintenance. Its continuous loop retries immediately; a one-shot run reports the
superseded sample and exits. The shared anchor parser also serves native push, repack and history
recovery; malformed index hashes retain their source errors.

`generation::maintain_commit_graph` derives a missing generation-bound split
commit graph through bounded `crab-remote-git` batches after catalog readiness.
It reuses the preceding generation's validated graph when available, uploads
immutable graph objects first and attaches the descriptor only if the manifest's
Git identity is still current. It does not materialize packs or create a checkout.

The lifecycle returns a `Send` future suitable for an owned Tokio task. Metadata
point lookups capture owned keys before constructing their concurrent batch, so
borrowed iterator entries do not make publication unspawnable. This retains the
existing concurrency bound and request ordering; the extra key storage is linear
in the caller's bounded batch, with 21 bytes per key.

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

`journal::commit_edits` is the CLI's shared journal commit path. It validates a
complete batch and checks each expected old OID against a caller-supplied snapshot
before writing anything. It then reads per-ref causal parents and uses the metadata
journal's prepared heads and single atomic active marker. The supplied snapshot
must be captured while holding every edited ref lease, with those leases retained
and renewed through completion; passing an earlier snapshot is not a concurrency
check. Existing committed journal edits count toward the old-value comparison,
even before generation compaction. The function preserves exact new OIDs, tag
peeling, HEAD changes, uploaded pack/shard references and visibility evidence.

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
commit-outcome recovery once publication is attempted. The journal checks before
each prepared head and before its active marker: cancellation there rolls back
prepared heads. After the marker attempt it finishes recovery and promotion.
Late renewal/release errors cannot replace a known successful commit result.
Callers must retain and renew their edited-ref leases throughout this work.

## Caller responsibilities

For the lower-level `catalog::publish_inventory`, the caller must supply the
complete inventory for its anchor and hold the locator writer lease and required
GC fences. It owns cancellation, lease renewal,
writer close on every outcome, and any checkpoint needed after publication. The
shared function neither changes refs nor closes the supplied writer. Keep the
writer alive while awaiting the operation; cancellation must still close it.

Await journal and catalog lifecycle operations to completion; do not abort their task or drop their
future to enforce a deadline. They own stateful writes and lease cleanup. The
caller still owns the generation-owner election and any required GC fences.

For journal commit, callers also own write authorization, individual ref-name/policy and
graph/dependency validation, immutable uploads, visibility proof, and ref leases.
After a failed marker write, the metadata journal attempts bounded exact readback.
Matching marker bytes confirm commit and allow head cleanup to continue. If the
marker is absent, different, oversized or unreadable, `RefJournalCommitUncertain`
retains the transaction ID, original write error and any readback error. Absence
does not prove rejection: a compactor may already have published the generation
and removed the active marker. No prepared-head rollback follows a marker attempt.
Callers must reconcile an uncertain outcome before reporting failure. Successful journal commit
means refs are durable, not that the derived catalog is ready for reads.

This crate does not yet own the complete generation service: receive-to-commit,
index receipts and restart repair still need a shared composing path before HTTP
push can acknowledge a fully readable generation.

## Verification

CLI tests exercise generation advancement without new packs, stale-slot repair,
concurrent local/remote evidence and kind metadata through the extracted code.
The direct integration test rejects cancellation, mismatched local evidence and
truncated indexes without publishing coverage, closes/reopens the writer, then
verifies exact commit/tree/blob bytes
through `crab-remote-git` after discarding the local Git repository.

A separate local RustFS qualification also removed its local repository before
catalog publication and read all three objects byte-identically. Publication took
21 ms for that small fixture. This is catalog qualification, not an accepted
HTTP push or a production latency guarantee.

Journal tests cover owner and reader handoff, preservation of a replacement ref
lease, cancellation while waiting, manifest failure and retry, and repeat calls
with no active transactions. Shared renewal tests prove that lease loss drains
the operation and preserves its primary error; existing CLI tests also cover a
completed operation racing a stalled backend renewal.

Commit tests cover a second atomic batch over un-compacted journal state, including
causal parents, exact creation/update/deletion, peeled tag removal and HEAD changes.
Stale and malformed batches leave storage unchanged, including no orphan journal
artifacts. Existing compaction tests also enter through the shared commit function.

Namespace tests cover conflicting concurrent creates, atomic parent replacement,
existing-ref progress behind a busy namespace lease, cancellable admission and
preservation of a committed result after lease loss. A CLI test interleaves a
journal create with an initial import whose manifest ETag is still unchanged.
Metadata tests cancel immediately before and after the marker boundary.
A RustFS race accepts exactly one conflicting create, releases the namespace
lease, and publishes the winning generation. Its commit/tree/blob bytes match
through `crab-remote-git` after removal of the local fixture. The two contending
operations complete in 229 ms and read readiness takes 163 ms for this small
fixture; these shared-cache samples do not measure HTTP push latency.

Metadata fault tests cover lost marker replies, unavailable readback, wrong or
oversized marker bodies and compaction before readback. CLI/protected-service
error boundaries preserve the uncertain transaction's identity and typed sources.
A RustFS proxy qualification drops all 55 marker-write responses across transport
and storage retries. RustFS accepts the marker once; exact readback confirms it,
then compaction/catalog publication and byte-identical remote reads succeed.
The injected failure adds 22.6 seconds to commit; it is not a normal latency sample.

A RustFS fixture removes its local repository before shared journal commit, rejects
a stale retry without another active transaction, then compacts and publishes the
catalog. Exact commit/tree/blob bytes match through `crab-remote-git`. The shared
commit took 7 ms, compaction 22 ms and catalog lifecycle 22 ms for this small fixture;
these are component timings with shared caches, not HTTP push benchmarks.

A separate RustFS round trip commits a native Git fixture through the ref journal,
removes its local repository, verifies that a busy reader skips compaction, then
compacts the journal and publishes the catalog. The compactor publishes exact
visibility from the transaction's evidence; all three Git objects subsequently
match the native oracle through `crab-remote-git`. Journal compaction took 28 ms
and catalog publication 12 ms for that small fixture. This does not qualify
HTTP receive or production latency.

Catalog lifecycle tests cover cancellation behind a writer, failed-publication
cleanup/retry, stale samples before planning and a manifest change after the
captured read. The latter runs the lifecycle in a spawned Tokio task. Metadata
point and scan lookup tests preserve request order and missing rows. Eleven
focused CLI owner/planning/commit-graph tests pass through the shared code.

A separate RustFS fixture removes its local repository before the shared
lifecycle publishes the catalog. It binds visibility to the published checkpoint,
verifies exact commit/tree/blob bytes through `crab-remote-git`, and repeats the
lifecycle without a second advance. Full catalog lifecycle took 23 ms and journal
compaction 29 ms for that small fixture. These shared-cache observations do not
qualify native HTTP receive or production latency.
