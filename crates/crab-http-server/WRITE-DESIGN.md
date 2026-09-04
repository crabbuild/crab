# Native Git writes: implementation boundary

Status: native HTTP push is not yet available. Shared incoming-pack quarantine
is implemented and qualified; publication and HTTP receive wiring remain pending.
The passing intake tests do not prove an accepted push or an updated ref.

## Implemented intake boundary

`crab-git::incoming_pack::quarantine` privately spools one complete incoming pack,
checks its SHA-1 trailer and exact entry count, and validates zlib termination,
size declarations, canonical headers and OFS entry boundaries. It enforces caller
limits for wire bytes, object count, individual object size, total inflation and
delta depth, with cancellation checks between chunks and delta instructions.
Inflated programs and decoded objects use disk spools rather than retaining the
whole pack in memory. Dropping the quarantine removes only its private directory.

Forward REF deltas resolve within the pack before external lookups. An injected
base reader supplies unresolved thin bases; its errors retain their sources and
its returned bytes must match the requested Git OID. The caller must enforce the
base reader's authorization, allocation limits and I/O deadline. The generic Git
layer has no storage, Tokio or server dependency. Incoming packs and remote reads
share `crab-git::delta`, including overflow-checked size headers and bounded copies.

Native Git fixture packs and a remote-reader integration test cover full/thin
reconstruction. Live qualification used a 4,081-byte Kubernetes thin pack at
`160bd16d98b7f688ce4f3b5ab0c5e4c045f36233`: 30 incoming objects plus 23 bases read
through `crab-remote-git` from local RustFS. All 53 objects matched native Git bytes.
The release run took 452 ms including repository open, without cache isolation.
This measures intake, not receive-pack publication or production push latency.

Remaining intake work: Git object syntax and graph connectivity, pointer dependency
proof, normalized self-contained pack/index publication, and HTTP streaming with
a request-bound deadline. No Git binary, clone or local Git object database is
used by quarantine. The test oracle uses Git independently.

## Existing code and the semantic mismatch

`crates/crab-auth-server/src/receive/workflow.rs` exposes `prepare_receive`,
`verify_receive` and `commit_receive`. These consume a `ProtectedPushPlan` with
staged objects, a candidate manifest and a base-bound dependency receipt. Native
`git push` sends ref commands and a Git pack instead of this prepared plan.

The protected-view workflow also has different ref semantics. In
`receive/git_workspace.rs`, `materialize_source_push_in` can synthesize a source
commit when the view's old tip differs or its update is not a fast-forward.
`is_fast_forward` requires commit objects. That behavior serves protected view
translation; routing arbitrary branch/tag/delete commands through it would not
preserve the exact object IDs submitted by native Git.

Therefore, the HTTP server must not simply call that workflow with a fabricated
plan or report a rewritten commit as the client's requested update. The existing
protected-view behavior remains independently owned and unchanged.

## Required ownership

- HTTP composition owns receive-pack advertisement/framing, authentication,
  write permissions, request limits and Git report-status responses.
- Shared Git/write mechanics own pack quarantine, index verification, dependency
  validation and an exact ref-update plan. Reusable publication belongs below
  both server crates; the HTTP server must not depend on another server crate or
  invoke the CLI as its publisher.
- `crab-storage` owns object locations and conditional storage operations.
  `crab-metadata` owns immutable indexes, manifest validation, receipts and
  publication formats. `crab-coordination` owns ref serialization and GC fences.
- Existing read tokens remain read-only. Write permission must be explicit in
  both repository authorization and the token requested by the user. Adding
  writes must not silently upgrade credentials already issued for reads.

## Publication sequence to implement

1. Authenticate and authorize the repository before receiving a pack. Parse a
   bounded ref-command batch. Validate ref names and capabilities, preserve the
   submitted old/new object IDs and distinguish deletion from an empty repo.
2. Stream the pack into bounded quarantine on the configured temporary volume.
   Verify its checksum, indexes, object identities, thin-pack bases and graph
   connectivity. Inspect pointer dependencies before exposing new references.
   A temporary incoming-pack workspace is not a clone of the repository.
3. Build an immutable candidate from the verified incoming objects and a pinned
   source generation. Apply branch/tag policy and exact old-tip comparisons.
   Never synthesize a replacement commit to make a rejected update fit.
4. Acquire the existing per-ref serialization and global/repository GC writer
   fences. Recheck the authoritative base. Publish immutable objects and required
   metadata before the manifest/ref authority changes; preserve source errors and
   release every acquired lease on success, rejection, cancellation or failure.
5. Commit through the canonical conditional manifest/coordinator boundary.
   Publish/read back the required locator and visibility evidence so successful
   pushes can immediately be consumed by `crab-remote-git` and HTTP fetch.
   A lost response after commit must remain distinguishable from a failed commit.
6. Return Git status for every requested ref. Cancel/reject paths must leave no
   changed refs; atomic batches must have no partially updated subset. Cleanup
   may remove this request's quarantine, never committed or grace-period data.

The relevant existing contracts include `PushLockAcquireContext::acquire_ref`,
`GcFenceLease::acquire_writer`, `GcFenceHeartbeat`, the manifest store's conditional
publication, the segmented pack index, `GitObjectLocatorWriter`, and the generation
index receipts. The protected receive workflow demonstrates fencing and metadata
ordering; its view translation is not part of native Git publication.

## Evidence required before exposing push

- Native Git round trips: empty-repo initial push, update, branch creation,
  lightweight/annotated tags, deletion, policy-controlled force updates and
  atomic multi-ref batches. Every accepted ref must have the exact client OID.
- Rejection without publication: read-only token/member, stale old tip, protected
  branch, malformed commands, corrupt/truncated/thin pack, missing dependencies,
  size/time limits and revoked credentials.
- Concurrency/recovery: competing writers, GC fencing, cancellation at each
  publication boundary, restart and response loss after an actual commit.
- Real RustFS proof: push from one native Git client, read the resulting generation
  through the browser APIs without a server clone, then fetch from an independent
  client and compare commits, trees and blob bytes. Include the existing protected
  receive/view tests as sibling-surface proof when shared code is moved.
