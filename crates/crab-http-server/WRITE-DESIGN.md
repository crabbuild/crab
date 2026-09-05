# Native Git writes: implementation boundary

Status: native HTTP receive is composed and has an isolated RustFS integration
proof for standard repository publication. Exact branch/tag creates, fast-forward
updates and deletions work; forced rewrites are rejected atomically. See
[Native Git push](README.md#native-git-push) for limits and remaining qualification.
Shared intake/graph/dependency proofs below are historical component evidence;
only the HTTP integration tests establish an accepted native push.

## Implemented receive framing and authorization

`crab-git::receive_wire` parses bounded command sections without consuming pack
bytes, advertises supported capabilities and encodes known atomic outcomes.
Native Git tests cover branch/tag creation, deletion without a pack, empty packs
and visible rejection/unpack failures. HTTP receive admission and publication
now compose this framing with the intake and commit boundaries below.

Repository members now have explicit `read`/`write` grants. Git tokens bind one
owner/repository and requested permission to a browser session; effective access
intersects the grant and token scope. Revocation invalidates retained principals
on their next authorization check. The browser defaults to scoped read tokens
and offers write scope only for a configured writer; the token API enforces the
same grant. Receive checks `Principal::can_write` before intake and publication.

The RustFS browser smoke signs in, creates a token, reads the exact Kubernetes tip
through Git HTTP, rejects another repository mapping and observes 401 after
revocation. Write-token issuance succeeds for the writer's repository and returns
403 for a read-only grant. Config, HTTP tests and mobile browser checks cover
explicit scope, revocation, selection and failure/retry behavior.

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

HTTP composition now supplies receive admission/deadlines, canonical pack/index
publication, and streaming with a request-bound deadline. No Git binary,
clone or local Git object database is used by quarantine or preparation. The
test oracle uses Git independently.

## Implemented graph and ref boundary

`crab-git::receive_plan::validate` applies exact old/new OID comparisons across an
atomic candidate ref map. It rejects duplicate commands and final ref namespace
collisions, requires commits for branch tips, enforces caller-supplied deletion
and non-fast-forward policy, and returns peeled tag targets without rewriting
commits. Git provides no separate force flag on the receive wire.

Every quarantined object is parsed, including unreachable objects. Typed links
must exist and match commit/tree/blob/tag requirements. Gitlinks remain references
to another repository. Tree names retain raw bytes, while malformed modes,
duplicate/unsorted entries, traversal components and protected `.git` aliases are
rejected. Valid Crab/LFS pointers are returned as dependencies for storage proof.

The injected `GraphSource` may stop traversal at an object only using a verified,
generation-bound closure proof. A locator hit alone is not such a proof. Unknown
objects are read and traversed with object-size, aggregate-byte, traversal-step
and ref-count bounds. The caller must use the same pinned generation throughout,
recheck the base under writer locks and prove pointer payloads before committing.

Live qualification created a new Kubernetes-derived commit and annotated tag in
an isolated client object database. Native `git fsck --strict` passed. Their
498-byte thin pack contained four objects; quarantine read one base from RustFS.
Validation used 38 committed proof frontiers and read no additional old object
bodies. The exact new commit/tag OIDs and peeled target were preserved. The release
run took 1,033 ms including repository open, proof loading, intake and validation;
caches were not isolated. No refs were published by this check.

## Implemented self-contained pack preparation

`IncomingPack::prepare` streams every unique reconstructed object, including thin
bases, into full zlib entries in OID order. This deliberately trades delta
compression for independent pack readability; the output byte limit is separate
from the wire limit. Empty inputs need no pack. Private artifact ownership removes
partial output on failure and all prepared files on drop.

Only normalized, bounded full entries reach Gitoxide's v2 index writer, with one
worker and checksum verification. Indexed OIDs must exactly match quarantine.
Crab's existing reverse-index and kind-sidecar encoders produce the same formats
used by locator publication. The result exposes the Git pack SHA-1, full-pack
Blake3 identity, byte size and object count. Preparation needs no Git binary or
object database. Peak additional disk is two output packs plus index sidecars;
memory is bounded by the quarantine object limits and index cardinality.

Nine intake/preparation tests pass, including native Git full/thin fixtures,
independent index generation, exact object reconstruction, deterministic output,
empty input, byte limits, corrupted/truncated spools and cleanup. The Kubernetes
candidate above produced a 2,714-byte self-contained pack from the 498-byte thin
input: four incoming objects plus one RustFS base. A separate native Git client
with no alternates verified the pack, generated a byte-identical index and read
all five objects identically to the source. Local preparation took 2 ms; repository
open, intake, validation and preparation took 904 ms, without cache isolation.
These artifacts are private and are not evidence of published refs or HTTP push.

## Implemented per-ref visibility planning

`receive_plan::plan_visibility` builds exact object sets for the existing
`GitVisibilityEdit` publication format. A `VisibilitySource` pins one prior ref
tip and its complete committed closure to the same generation as the object
reader. Membership in a union of other refs is not a valid prior closure.

The planner initially prunes objects in that prior closure. It emits additive
evidence only if traversal actually reaches the prior tip, which proves the
entire prior closure remains reachable. Otherwise it expands the pruned graph
and emits a complete replacement closure. That distinction prevents force
updates from retaining authorization for old, unreachable objects. Trusted
commits/trees/tags still require their outgoing edges when expanding a closure;
proven blobs are leaves and Gitlinks remain external references. Incoming objects
not reachable from the selected ref do not enter its visibility evidence.

The planner shares object parsing, identity checks, kind validation, cancellation
and byte/step limits with ref validation. It does not enforce ref update policy,
prove pointer payloads or write storage. The publisher must bind its output to the
same pinned base and pass it through the canonical metadata transaction boundary.

Five focused tests cover additive commit/tag evidence, shared-subtree rewrites,
trusted objects outside a reusable ref, Gitlinks, malformed/missing objects,
limits, cancellation and invalid prior-tip binding. The seven existing ref/graph
tests still pass. Kubernetes/RustFS qualification matches native `git rev-list`
exactly: the new commit adds three objects and its tag adds four, with no old
object-body reads. Planning both proofs took 1,628 ms locally, including individual
remote catalog lookups; this is not a cache-isolated or HTTP push benchmark.

A separate orphan commit retains only the Kubernetes `.github` subtree under a
new root. Native `git fsck --strict` passes. RustFS-backed replacement planning
matches native Git's complete reachable sets: 12 objects for the orphan commit
and 13 for its annotated tag, excluding the former main tip. Its 4,733-byte input
carries all 13 objects; neither intake nor planning reads old object bodies.
The two replacement proofs took 124 ms locally. This fixture and cache state
differ from the additive measurement, so their times are not directly comparable.

## Implemented Crab pointer content proof

`crab-read::pointer_proof::verify_crab_pointer` verifies one pointer from an
explicit shard through an origin-only store. It checks the shard identity,
selects the exact ordered recipe, validates each xorb's identity and serialized
payload digest, then decompresses and hash-checks each selected chunk with the
existing `XorbParser`. The reconstructed size and whole-file Blake3 must match
the pointer. Repeated chunk occurrences retain their ordering. Empty files work
without xorbs. No ref, receipt, cache or local Git object is written.

The caller bounds file size, shard/xorb reads, aggregate successful response
bodies, expanded chunk occurrences and duration. Transport retries retain the
store's separate bounds. One xorb body is retained at a time; nonconsecutive
reuse is charged again. CPU work runs on blocking workers with cancellation
checkpoints. The shared materializing shard parsers now cap expanded occurrences,
so repeated ranges cannot multiply a small metadata input into unbounded output.
All synchronous shard replay paths share record readers that reject records over
the canonical shard byte limit and grow buffers only as bytes arrive. They reuse
the upstream header codecs and record views; the upstream streaming helpers
reserve attacker-declared sizes before reading. The verifier replays bounded
records instead of following lookup offsets into another deserializer.
Streaming visitors still support aggregate inventories above the materialization
limit while applying the same per-record byte bound.

`FileIndexLookupSession::for_snapshot` selects dependencies from an explicit
`read_repository_snapshot` result. It reuses the canonical shard scan, including
captured journal shards, without reading a newer manifest or opening SlateDB.
Later manifests, journal commits and acceleration rows cannot widen the captured
inventory. Cancellation leaves no reader checkpoints or reader handle to close.
Ordinary current-state lookups retain the same shared anchor derivation and
acceleration behavior.

Snapshot-bound callers supply `FileIndexLookupLimits`: file queries/cache entries,
cumulative shard visits, individual shard-body bytes and recipe expansion. The
inventory must fit before session allocation. Each scan reserves its complete
visit count before dispatch; failure or cancellation does not refund the count
or cache incomplete absence results. Successful cached results need no new I/O.
Four scans may overlap across all sessions in a process. Full-body traffic is
bounded by visits times the body cap, plus each visit's HEAD and at most 4,108
bloom/trailer bytes, excluding the
transport's separately bounded retries. Current-state readers retain their
existing query and inventory limits. No config or environment setting is added.

Scan admission is acquired before origin reads and retained by the blocking
hash/recipe parser. Caller cancellation or timeout does not release capacity
while a detached parser still owns buffers and CPU work. Hashing and extraction
run off async workers. Pointer content proofs independently admit four operations
per process; their deadline includes queue time, and every blocking job shares
the request's permit until it exits. A caller can return promptly on timeout
while the remaining bounded job stays accounted for. These stage bounds do not
replace admission and an overall deadline for the complete receive operation.

Selection and content evidence are not publication authority. The publisher must
acquire writer/GC fences and recheck its exact base before publication. Pointer
hints and file-index hits alone are insufficient. Admission/deadline integration
for the remaining receive stages, LFS HTTP transfer and fenced publication remain
pending; HTTP push is still disabled.

Six focused tests cover repeated compressed content, empty files, missing or
corrupt origin objects, forged file identity, resource bounds and cancellation
of pending reads. All 36 relevant shard tests pass, including oversized/truncated
records, optional record flags, repeated ranges across three materializing
readers and streaming above the materialization limit. A separate RustFS fixture reconstructs
16,384 bytes from three ordered chunk occurrences in two xorbs, reading 1,218
stored bytes per proof. Ten local release proofs took 1.816–2.769 ms each; these
are small synthetic, cache-sharing observations, not Kubernetes or production
latency claims. Corrupting and deleting an isolated fixture xorb both caused
verification to fail; restoring the content made it pass again.

Ten file-index tests pass, including captured-state lookup after a later manifest
and acceleration row are published, no checkpoint writes, existing scoped-reader
behavior and committed journal candidates. A separate RustFS fixture captures
generation 1, then publishes generation 2 before opening the snapshot-bound
lookup. It selects only generation 1's dependency; content proof reconstructs its
exact 16,384 bytes. The current snapshot selects only generation 2's dependency.
All six repository objects retain identical sizes and ETags during the reads.
Captured lookup plus content proof took 3.293 ms locally. This is an isolated
metadata/content fixture, not a native Git push or production latency result.

The bounded lookup version passes 16 file-index tests, including oversized
inventory/query rejection before storage reads, cached-result reuse, cumulative
cache/visit caps, shard/recipe limits, and retained reservations after errors and
cancellation. A fresh RustFS fixture repeats the pinned lookup/content proof with
one allowed shard visit, then verifies exhausted-budget and oversized-body
rejections. The failed body read keeps its visit charged; all six repository
objects remain unchanged. Lookup, cached/budget checks and content proof took
3.168 ms locally. Process-wide admission and CPU deadline behavior are not proven
by this small fixture.

The admitted CPU version passes 18 file-index tests and eight pointer-proof
tests. New cases prove admission precedes origin reads, proof deadlines include
queue time, and detached blocking jobs retain capacity until exit. An isolated
RustFS fixture repeats pinned selection and resource rejection checks, then
verifies 32 concurrent content proofs against the exact file identity. Those
proofs complete in 14.780 ms total; selection and one content proof take 3.430 ms.
All six repository objects retain their sizes and ETags. This synthetic,
cache-sharing fixture does not qualify native push or production throughput.

The CLI and protected-service error boundaries handle lookup limits and preserve
the typed source of admission/worker failures. This also fixes the missing CLI
match arm exposed by CI after the lookup-limit variant was introduced. Two CLI
error tests and one protected-service error test pass; CLI compilation and the
architecture gates also pass.

## Combined dependency preflight

`crab-read::dependency_proof::verify_dependencies` connects the validated Git
pointer list to captured-snapshot shard selection and origin content proof.
Before I/O it rejects excessive pointer counts, individual/aggregate file sizes,
unrecognized dependencies and conflicting sizes for one content identity.
Repeated content is verified once, with Crab and LFS identities kept distinct.
One deadline covers the complete selection/content batch, including admission
waits. Storage traffic has the lookup budget plus at most pointer count times
the per-content read budget; transport retries remain separately bounded.

`LfsObjectStore::verify_origin` hashes the exact stored bytes without trusting
or writing receipts and without replica fallback. It rejects mismatched response
size before consuming the stream, then checks actual length and SHA-256. The
canonical receipt-miss path shares its body verifier. Four admitted LFS bodies
may overlap per process, and blocking hash jobs retain capacity through caller
cancellation. This preserves the existing receipt/fallback behavior for ordinary
LFS operations while giving publication preflight an explicit origin contract.

The primary LFS OID/size identify the stored bytes after extension processing;
extension hashes describe client transform inputs, not additional server
objects ([upstream specification](https://github.com/git-lfs/git-lfs/blob/main/docs/extensions.md)).

Five combined preflight tests and 32 LFS storage tests pass, including deduped
mixed batches, no storage mutations, early limits, cancellation/deadlines,
missing/corrupt content, receipt bypass, fallback isolation and existing
multipart/receipt/read paths. A native Git commit containing one Crab pointer
and two distinct LFS pointer blobs (one with an extension) passes strict fsck,
quarantine and graph/ref validation. RustFS verification reconstructs the Crab
file's 16,384 bytes and hashes the shared five-byte LFS payload once. It rejects
corruption and deletion of the isolated LFS fixture and passes after restoration.
All four listed repository objects retain their sizes and ETags during the
successful preflight; refs remain unchanged. The combined proof took 3.139 ms
locally, a small synthetic observation rather than production push latency.

No HTTP endpoint accepts these candidates yet. Writer fences, canonical
publication/recovery and authorized receive-pack wiring still need
implementation and native client round-trip qualification.

## Existing code and the semantic mismatch

`crab-write::catalog::publish_inventory` now owns the catalog engine formerly
inside the CLI push module. CLI push/reader repair and the generation owner both
use it. It validates local/remote index evidence, writes current rows before
sweeping stale slots, rebuilds/replays changed ordinal catalogs when permitted,
and rechecks the manifest before advancing coverage. The caller still owns its
writer lease, GC fences, close and checkpoint lifecycle. The public publication
path reads bounded index sidecars and pack trailers without a local repository.

Three existing CLI tests pass through the shared code, covering generation
advances, stale rows, mixed sibling evidence and kind metadata. A direct test
rejects cancellation, mismatched local evidence and a truncated index without
coverage, closes/reopens its writer, and then
reads the exact commit/tree/blob through `crab-remote-git` after the source Git
repository has been discarded. A separate RustFS qualification also verifies
all three objects after discarding the local repository; catalog publication
took 21 ms for that small fixture. This proves the extracted catalog component,
not journal publication or an accepted HTTP push.

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

## Publication sequence and remaining recovery requirements

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
6. Return Git status for every requested ref. Cancellation/rejection before the
   marker attempt must leave refs unchanged; atomic batches must have no partially
   updated subset. After a marker attempt, reconcile uncertain outcomes instead
   of reporting cancellation as rejection. Cleanup may remove this request's
   quarantine, never committed or grace-period data.

The relevant existing contracts include `PushLockAcquireContext::acquire_ref`,
`GcFenceLease::acquire_writer`, `GcFenceHeartbeat`, the manifest store's conditional
publication, the segmented pack index, `GitObjectLocatorWriter`, and the generation
index receipts. The protected receive workflow demonstrates fencing and metadata
ordering; its view translation is not part of native Git publication.

The native CLI's `commit_ref_journal` makes an immutable transaction visible by
its active marker; `compact_ref_journal_for_owner` subsequently folds it into a
generation under the manifest owner lease. `crab-remote-git` deliberately returns
`RepositoryIndexing` while committed journal transactions remain uncompacted.
The CLI now uses `crab-write::journal::commit_edits`, which checks the whole batch
against a snapshot captured under retained ref leases, obtains causal parents
and commits through the same metadata marker. A failed marker write is confirmed
only by bounded readback of the exact expected bytes. Otherwise the typed
`RefJournalCommitUncertain` preserves the transaction ID and write/readback errors.
An absent marker is not a rejected push: generation compaction removes committed
markers. HTTP receive must reconcile this outcome or fail the transport without
inventing an unchanged-ref result; Git's [report-status contract](https://git-scm.com/docs/pack-protocol#_report_status)
reports the actual outcome for each requested ref. Prepared heads are never
rolled back after a marker write is attempted.

Ref creation/deletion now acquires the shared `git-ref-namespace` lease after
edited-ref leases, rereads a coherent snapshot and checks the complete candidate
namespace. This closes the race where separately locked `feature` and
`feature/sub` both passed validation. The plain CLI initial-manifest path uses the
same gate; ordinary existing-ref updates still need only their per-ref locks.
Cancellation before the active marker rolls back prepared heads. After its
attempt, recovery drains to a known or uncertain outcome; a late namespace lease
failure cannot turn an accepted commit into rejection. A RustFS race accepts one
create and rejects its conflicting sibling, then publishes byte-identical remote
commit/tree/blob reads after removing the local fixture.

This guarantee covers cooperating journal writers and the plain CLI initial
publisher. Protected receive still publishes a complete manifest through
`crab-auth-server::receive::finalize::commit_receive_manifest`; active-active
writers publish through the versioned coordinator and materialize a projection.
Neither enters this gate. Their authority/coexistence and namespace validation
need separate integration and qualification before HTTP push supports those
repository modes. Payload validation alone does not enforce ref-name conflicts.

`crab-write::journal` now owns both generation-owner and reader compaction.
The CLI delegates to it, preserving bounded handoff waits, non-waiting reader
admission, metadata CAS/visibility publication and holder-checked ref cleanup.
It checks cancellation between complete waves and releases the manifest lease
on success and failure. Shared `crab-coordination::while_renewing` drains stateful
operations after renewal failure; all existing CLI internal-owner callers use it.
Callers must await this work to completion, including cancellation cleanup.
Three shared journal tests, a shared lease-draining test and six existing CLI
journal/renewal/handoff tests pass. A separate RustFS fixture publishes a native
commit through the journal, discards the local repository, compacts its exact
visibility and publishes the catalog. `crab-remote-git` reads all three objects
byte-identically. Journal compaction takes 28 ms and catalog publication 12 ms
for that small fixture, without cache isolation. It does not exercise HTTP push.

`crab-write::generation::maintain_catalog` now owns the catalog lease, planning
reader, writer, checkpoint and close lifecycle. It rechecks the captured manifest
before planning and after writer close. Superseded work has an explicit result;
the CLI owner skips receipt publication and later maintenance, reporting the
superseded sample. Its continuous loop retries immediately; one-shot runs exit. Shared
index anchors and row planning also serve native push, repack and history recovery.
The returned future is `Send`, covered by spawning the lifecycle in Tokio. Metadata point
lookups construct owned keys before their asynchronous batch; this avoids the
borrowed-iterator lifetime failure exposed by spawning the lifecycle in Tokio.
Four lifecycle tests cover contention/cancellation, failed-publication cleanup,
stale planning and a generation change after the captured read. Metadata point
and scan tests preserve lookup order/missing rows; eleven focused CLI owner and
commit-graph tests pass. RustFS verifies exact commit/tree/blob reads after
removing the fixture's local repository, completing the catalog lifecycle and
binding visibility to its checkpoint. Repeating maintenance reports no advance.
The small fixture takes 23 ms for catalog lifecycle and 29 ms for journal
compaction; cache state is shared and native HTTP push remains unqualified.

`crab-write::generation::make_readable` now composes journal compaction, catalog
lifecycle and binding of existing verified visibility to the exact catalog.
It reads coherent snapshots before and after publication: a newer manifest or
active journal returns a superseded result requiring another pass. Missing
visibility returns `VisibilityUnavailable`; neither this error nor cancellation
rolls back committed refs. The caller retains generation-owner election and both
GC writer fences and awaits internal lease/handle cleanup. This pass does not
reconstruct absent proofs from unverified objects or publish index receipts.

Four readiness tests cover an empty remotely opened repository, cancellation
without writes, missing proof without ref rollback, and an independent ref commit
while catalog admission is blocked. The existing four catalog lifecycle tests
also pass. A RustFS fixture holds owner/GC leases, removes its local Git repository
before committing, and uses one readiness pass to compact the journal and make
all three exact Git objects remotely readable. Repeating the pass keeps the same
generation. Observed readiness time was 69 ms for that small fixture, with shared
caches; this is not an HTTP push or production latency benchmark.

The HTTP server now owns an on-demand read-readiness job under generation-owner
election and global/repository GC writer fences. API refresh and Git upload-pack
retry their authoritative open after this job. Per-repository retained tasks
coalesce requests, global admission bounds publication to two repositories, and
request cancellation cannot drop a stateful publisher. Server shutdown cancels
and drains jobs before the read runtime closes. Existing owners and GC sweeps
remain authoritative; missing visibility remains an explicit failure.

Five server tests cover journal-aware cache refresh, missing-proof failure without
rollback, competing owners, both GC domains, cancellation/retry/shutdown cleanup,
and exclusion of non-members from API/Git-triggered publication. All 24 server
tests pass. A sandboxed RustFS run verifies initial API publication and native Git
fetch after a second journal commit, with exact commit/tree/blob bytes and no
server clone. Publication uses temporary index sidecars. A denied temporary write
returns 503 and closes the writer; a restarted server with writable temporary
space repairs that generation. Owner and GC leases are reacquirable afterward.

Native receive-to-commit wiring now has HTTP and RustFS qualification. Four
injected storage faults cover lost marker replies, rejected marker writes, and
cancellation before and after the commit boundary. A fresh server instance reads
the recorded outcome and exact blob bytes; explicit retry replaces uncommitted
prepared evidence under a new lease. An inconclusive marker attempt retains that
evidence instead of inventing a rollback. These are cooperative shutdown tests,
not abrupt process-crash qualification.

GC heartbeats continue renewing until their owner finishes draining and explicitly
stops or drops them. Operation cancellation alone cannot expire the fence and
leave a crash quarantine after successful cleanup. Memory and RustFS tests keep a
cancelled writer alive past its original expiry, admit another writer, release
both, and acquire a sweep. Renewal failure still cancels the owning operation.

Index receipts and restart reconstruction of missing evidence remain unfinished.
A raw manifest PUT or journal-only endpoint cannot
meet immediate read/fetch visibility or coexist correctly with native CLI writers.

## Evidence required before exposing push

Tag-only HTTP publication now preserves an unborn default branch through journal
replay, catalog publication and reopen. Native Git protocol-v2 clone retains its
symbolic name and fetches tag content; later branch publication establishes a
default normally. The API distinguishes unborn HEAD from an empty ref set, and
the browser can browse the available tags. RustFS qualification repeats this
flow and verifies exact blob bytes after client removal and server restart.
CLI/protected candidate builders and read-side advertisements share this rule.
Older tagged readers reject the new state; deploy the updated components together.

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
