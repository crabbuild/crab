# crab-write

Shared Git publication mechanics for Crab. This crate currently owns catalog
publication extracted from the CLI push and generation-owner paths. Both CLI
callers use this implementation; HTTP receive can compose it without depending
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

## Caller responsibilities

The caller must supply the complete inventory for its anchor and hold the
locator writer lease and required GC fences. It owns cancellation, lease renewal,
writer close on every outcome, and any checkpoint needed after publication. The
shared function neither changes refs nor closes the supplied writer. Keep the
writer alive while awaiting the operation; cancellation must still close it.

This crate does not yet own the complete generation service: journal commit and
compaction, lease orchestration, visibility readiness, index receipts and restart
repair still need a shared composing path before HTTP push can acknowledge a
fully readable generation.

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
