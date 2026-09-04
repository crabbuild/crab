# crab-write

Shared Git publication mechanics for Crab. This crate owns catalog publication and
ref-journal compaction extracted from CLI push, reader repair and generation-owner
paths. Those CLI callers use these implementations; HTTP receive can compose it without depending
on the CLI or another server.

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

## Caller responsibilities

The caller must supply the complete inventory for its anchor and hold the
locator writer lease and required GC fences. It owns cancellation, lease renewal,
writer close on every outcome, and any checkpoint needed after publication. The
shared function neither changes refs nor closes the supplied writer. Keep the
writer alive while awaiting the operation; cancellation must still close it.

Await journal operations to completion; do not abort their task or drop their
future to enforce a deadline. They own stateful writes and lease cleanup. The
caller still owns the generation-owner election and any required GC fences.

This crate does not yet own the complete generation service: journal commit,
catalog/visibility readiness, index receipts and restart repair still need a
shared composing path before HTTP push can acknowledge a fully readable generation.

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

A separate RustFS round trip commits a native Git fixture through the ref journal,
removes its local repository, verifies that a busy reader skips compaction, then
compacts the journal and publishes the catalog. The compactor publishes exact
visibility from the transaction's evidence; all three Git objects subsequently
match the native oracle through `crab-remote-git`. Journal compaction took 28 ms
and catalog publication 12 ms for that small fixture. This does not qualify
HTTP receive or production latency.
