# crab-write

Shared Git publication mechanics for Crab. This crate owns catalog publication,
ref-journal commit and compaction extracted from CLI push, reader repair and generation-owner
paths. Those CLI callers use these implementations; HTTP receive can compose it without depending
on the CLI or another server.

## Publication flow

```text
commit_edits                 durable journal refs; catalog may still lag
     ↓
make_readable                elected generation owner, with caller GC fences
     ├── compact_for_owner   fold journal into the manifest
     ├── maintain_catalog    publish the generation-bound object catalog
     └── visibility check    recheck the manifest and active journal
     ↓
Some(manifest): ready        None: capture fresh state and try another pass
```

A commit error may be uncertain; resolve that outcome before proceeding or
reporting rejection. Read readiness is a separate result from ref durability.

## Initialization

`initialize::initialize_repository` owns canonical empty-repository creation for
the CLI and HTTP server. It creates the layout only for an empty repository prefix,
conditionally publishes the generation-zero manifest, and adopts concurrent or
previous initialization after validating the persisted canonical roots.

## Catalog publication

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

## Generation maintenance

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
Current ref values alone cannot establish the historical transaction outcome;
use its bound receipt or retained commit evidence. An unresolved outcome does
not authorize replay, even when the refs match the attempted update.

This crate does not yet own the complete generation service: receive-to-commit,
index receipts and restart repair still need a shared composing path before HTTP
push can acknowledge a fully readable generation.

## Verification

| Contract | Executable evidence |
| --- | --- |
| Invalid local evidence and remote-only byte reconstruction | [Catalog tests](tests/catalog.rs) |
| Catalog close/release, cancellation, superseded generations, read readiness | [Generation tests](tests/generation.rs) |
| Atomic batches, namespace conflicts, holder-safe cleanup, failed compaction and retry | [Journal tests](tests/journal.rs) |
| Canonical initialization and adoption | [Initialization tests](src/initialize.rs) |
| Original error retained when cleanup also fails | [Error precedence test](src/lib.rs) |

Run these focused tests from the repository root. Replace `crab-write-dev` with
a unique checkout name and verify the workspace volume is mounted and writable,
following root `AGENTS.md`:

```sh
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-write-dev \
  cargo test -p crab-write --locked --lib --test catalog --test generation --test journal
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-write-dev \
  cargo clippy -p crab-write --locked --all-targets -- -D warnings
```

The tests use local fixtures and in-memory storage; native pack fixtures require
Git. They establish component contracts, not a complete HTTP receive path or
provider qualification. Use the repository's dedicated CI and RustFS qualification
for live storage, race/crash behavior, and cross-platform proof.
