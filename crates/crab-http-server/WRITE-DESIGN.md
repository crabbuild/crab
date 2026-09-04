# Native Git writes: implementation boundary

Status: design evidence for the next implementation stage. The HTTP application
currently supports fetch only. None of the requirements below is implemented by
this document or implied by the passing read tests.

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
