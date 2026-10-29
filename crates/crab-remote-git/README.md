# crab-remote-git

Read Git objects directly from Crab object storage, without cloning a repository
or creating a local object database. This internal workspace crate owns verified
Git reads; services own authorization and representation selection.

## Read path

```text
Caller: authorize and resolve physical placement
  -> RepositoryIdentity + RemoteGitRuntime
  -> crab-read Capsule capture: authenticated root, refs and immutable sources
  -> RemoteGitRepository::from_snapshot
       caller-validated materialized snapshot
     or RemoteGitRepository::from_snapshot_with_catalog_tail
       compacted-base catalog + bounded journal-pack overlay
  -> OperationContext: shared budgets and cancellation
       snapshot -> tree / blob / history / diff
  -> OperationContext::finish: result + locator close
```

A repository handle pins caller-validated metadata. A snapshot pins a reachable
commit and its root tree. This crate does not open a live stored manifest/ref
journal or infer current authority from a locator. Product callers capture the
Capsule view through crab-read and own freshness and remote retention. Empty
captures open, but selecting a snapshot returns `EmptyRepository`.

Snapshot constructors retain explicitly supplied commit-graph/path-state indexes
only for their exact materialized Git state. Loading a graph shares the normal
identity, integrity and admission checks; a journal ref change discards stale
base indexes even when its generation is unchanged. Snapshots without indexes
make no additional index requests. Publication of capsule-bound browse indexes
is a separate maintenance responsibility, not part of opening or pushing.

Captured physical indexes resolve OIDs and both offset and reference delta
bases through authenticated pages. Reference membership searches stay within
the source pack's pages. Snapshot opening checks their state, origin and prefixes;
queries retain storage admission, encoded-work limits and cancellation. Complete
index misses are definitive. Physical placement never assigns catalog ordinals
or authorizes ref visibility; ordinary unindexed snapshots retain their existing
pack-index inspection path until background readiness is published.

Delta reads retain the selected physical representation. OFS bases and in-pack
REF bases resolve inside their source pack. External REF lookup starts with
the snapshot's preferred representation. If that physical walk cycles or exceeds
the depth bound, a bounded graph search selects an acyclic path through admitted
aliases without changing the requested entry. Point reads, metadata inspection
and batch materialization share this plan; CRC, hash and byte-admission failures
remain terminal. Captured offset pages retain every physical alias, including
page-only snapshots without pack sidecars. Repacking materializes displaced
dependencies before combining representations, preventing valid source chains
from becoming response-pack cycles.

Capsule graphs retain an immutable paged reader and captured commit roots;
membership reads exact ancestor sets rather than allocating a full reachability
bitmap. A full commit ID matching a verified captured root reuses that visibility
proof without rereading ancestry pages; other IDs still require exact membership.
Graph/OID/parent reads share operation admission and cancellation, including
warm queries. Released manifest graph version 1 retains complete graph validation.
`commits_reachable_from` is asynchronous and uses a metadata-admitted operation
without opening the object-locator database.

Without a commit graph, full commit-ID resolution checks matching ref tips
before traversing other roots. Matching annotated tags still require verified
peeling; non-tip commits retain bounded breadth-first reachability traversal.

## Choose an entry point

| Need | API | Contract |
| --- | --- | --- |
| Shared admission and caches | `RemoteGitRuntime` | Process-wide; shut down after active contexts finish or drop |
| Open a captured Git view | `RemoteGitRepository::from_snapshot` | Caller supplies a validated snapshot, retention, and freshness policy; no catalog required |
| Open authenticated capsule sources | `RemoteGitRepository::from_snapshot_with_lookup_sources` | `SnapshotLookupSources` carries validated locators, source ranges, preferred indexes and object-to-member admission; snapshot identity and inventory checks remain mandatory |
| Accelerate a committed snapshot | `RemoteGitRepository::from_snapshot_with_catalog_tail` | Any available catalog whose immutable pack inventory is a subset of the snapshot combines with the remaining pack tail; unavailable or unproven catalogs fall back to all pinned pack indexes |
| Reuse a handle | Caller-held Capsule capture | Revalidate root and ref-head authority in the capture owner; never infer freshness from a legacy manifest ETag |
| Select a revision | `refs`, `resolve`, `snapshot` | Selection stays within pinned visible refs |
| Browse content | `entry`, `list_directory`, `list_tree_blobs`, `list_tree_recursive`, `read_blob` | Paths are opaque `GitPath` bytes; bounded blob pages seek by prefix/continuation without reading bodies |
| Inspect changes | `history`, `path_history`, `compare`, `diff`, `blame` | Aggregate work limits apply, including cache hits |
| Stream an archive | `archive_stream` | Transfers operation cleanup ownership to the stream |
| Generate a response pack | `generate_pack` and cached variants | Verified output; reuse requires matching identity and request policy |

See [public API contracts](REFERENCE.md#public-api) and the
[exported types](src/lib.rs). Sign page cursors before exposing them to
untrusted clients; preserve path bytes at transport boundaries.

## Complete every operation

For an already opened repository, retain the semantic result until `finish`
closes the locator session. Propagating the snapshot error first would skip
explicit cleanup and its close-error reporting.

```rust
use crab_remote_git::{OperationKind, RemoteGitRepository, RemoteGitSnapshot, Result, Revision};
use tokio_util::sync::CancellationToken;

async fn snapshot(
    repository: &RemoteGitRepository,
    revision: &Revision,
    cancel: &CancellationToken,
) -> Result<RemoteGitSnapshot> {
    let operation = repository.operation(OperationKind::Snapshot, cancel).await?;
    let result = repository.snapshot(revision, &operation).await;
    operation.finish(result).await
}
```

Archive streams own this completion step. Drop cleanup is best effort; explicit
completion preserves close errors. At service shutdown, stop admission, finish
or drop live contexts, then await `RemoteGitRuntime::shutdown` while Tokio is
still running. See [result and close-error precedence](REFERENCE.md#completing-an-operation).

Generated-pack producers may outlive an individual cache waiter. Their runtime
owner must therefore await shutdown even after a cancelled request has returned.
Lease cancellation and renewal failure signal a work-owned child token and
drain that work before releasing its lease; request-bound producer closures
must observe the supplied token and complete their cleanup before returning.

Pack-inventory downloads cancel only their operation's child token on a source
failure, drain started body/sidecar writers, and skip queued work. Canonical,
embedded and inline pack bodies share the same length/hash verifier and flush
pending file writes before returning, including on cancellation or stream
failure.

Large canonical, capsule-backed and cached response packs use the storage
owner's ordered concurrent range stream. Four range requests are shared across
store clones, including concurrent source packs; complete pack SHA-1/BLAKE3
verification and request/byte admission remain mandatory. Small artifacts keep
a single GET. Pack/sidecar writes and response-pack file reads use 1 MiB buffers.

Cached and source packs share the authenticated writer and drain pending writes on every returned
exit. Pack generation passes that semantic result to explicit session
closure. Callers must await cancellation cleanup before deleting destinations;
abandoning the future is not a synchronous drain guarantee.

## Choose the content representation

| Returned blob | Content owner |
| --- | --- |
| Ordinary Git blob | This crate returns its bytes |
| Crab pointer | `crab-read` hydrates logical content |
| Git LFS pointer | `crab-lfs` verifies and reads the target |

`read_blob` classifies pointers but does not materialize their targets.
An unborn HEAD can coexist with other refs; use `RepositoryRefs::is_empty()`
to test whether the repository has any refs.

## Performance and qualification

Point and batch reads reuse verified decoded delta bases from the same bounded
runtime cache. Batch dependency discovery stops at a cached base after checking
its object identity and size, avoiding repeated origin reads across batches.

Point reads, history, blame, and archives have different costs. A successful
local fixture does not establish production cloud latency or full API coverage.

| Topic | Read next |
| --- | --- |
| Cache identity, single-flight, and aggregate budgets | [Performance model](REFERENCE.md#performance-model) |
| Generated packs and source pack reuse | [Response packs](REFERENCE.md#generated-response-packs) |
| Tree traversal and verified commit-graph acceleration | [Trees and history](REFERENCE.md#trees-and-history) |
| Safe telemetry and deployment measurements | [Diagnostics and deployment](REFERENCE.md#diagnostics-and-deployment) |
| Cold/warm API measurement | [Run `qualify_remote`](REFERENCE.md#command-line-qualification) |
| Compare remote browsing with native Git | [Run `qualify_browse`](REFERENCE.md#browsing-correctness-against-a-real-repository) |

## Working on this crate

- [AGENTS.md](AGENTS.md): source map, invariants, and focused verification routes.
- [Cargo.toml](Cargo.toml): internal workspace package; no declared Cargo features.
- [Repository tests](tests/remote_repository.rs): real Git fixture behavior.
- [Reference](REFERENCE.md): complete consistency, lifecycle, performance, and qualification details.

## SDK read composition

`archive_reader` exposes the same incremental traversal as `archive_stream`,
with explicit `close().await` for consumers stopping before EOF. Closing does
not read remaining entries or claim their integrity. EOF and traversal errors
finalize the operation; drop retains the operation's tracked cleanup behavior.

Callers whose own read policy fails after session acquisition pass
`Error::Consumer` to `OperationContext::finish`. The boxed source remains typed;
the canonical finalizer records failure and preserves a simultaneous locator
close failure. Do not finalize such a read as successful and report policy
failure afterward.

`OperationContext::read_admission` shares cancellation and the aggregate budget
with additional object-body owners such as hydration. Do not attach it around
Git reads that already charge the context, which would double-count those
requests. It admits storage requests and reserves fetched bytes before bodies
are consumed; the storage hook documents its transport boundary.

Repository opening uses a separate aggregate budget for metadata GET/HEAD,
listing invocations and locator acquisition, including Store retries. Optional
commit-graph and shallow-closure acceleration cannot suppress admission errors.
The admitted store is temporary: returned handles retain the original store,
and later operations allocate their own budgets. Git-reader charge sites and
provider-internal pagination/retries still require complete transport accounting.

Each semantic operation opens its locator through that operation's admission,
covering checkpoint acquisition and subsequent catalog page reads. Catalog
lookups no longer charge a synthetic storage request. Shallow-closure entry
reads also use admission so Store retries consume the same aggregate budget.

The opening scope also enforces the configured operation duration across the
whole handshake, including pending listings. Caller cancellation, runtime
shutdown and timeout cancel the handshake and await its completion; no separate
timer task is left behind. Wrapped cancellation becomes the scope's terminal
reason while real operation failures and typed close errors remain available.

Repository handles retain their immutable shard-index root alongside the Git
catalog identity. `shard_index_hash` exposes that captured root to content
owners without loading shard metadata into metadata-only reads. Refresh
captures a separate root; it does not mutate existing handles.

Pack and sidecar downloads attach the operation admission policy to the Store
stream. Response headers reserve actual advertised bytes, and facade retries
consume new request admission. Pack inventory size is a preflight bound and an
integrity check, not the byte-accounting authority. These stream paths retain
backpressure and verification before returning the completed artifact.

Complete-inventory clone transfer starts after the catalog visibility bitmap
proves every catalog ordinal is visible. Pack bodies use a locator-free transfer
operation while a separate operation scans the pinned catalog concurrently;
the downloaded pack indexes must still cover that exact OID set before the
inventory is returned. Partial visibility keeps the normal planned-pack path.

Generated response packs retain Git's sideband packet limits while batching
wire writes in at most 1 MiB. The same writer serves CLI and HTTP callers;
the HTTP duplex still applies backpressure. Success explicitly flushes the
batch before response-end framing. Write/flush errors remain typed and pending
writes or flushes remain cancellable, so dropping a batch cannot silently
complete a truncated response.

Operation-owned coalesced ranges and packed-entry metadata reads use the same
admission boundary: failed headers charge a request but no advertised body,
and each facade retry reserves its own request and response bytes. An early
range-size check rejects work that already exceeds the remaining byte budget.

### Shared immutable-read work

Packed-entry, pack-index body and index-size producers share one immutable-read
flight implementation with independent participant budgets. Request attempts and
advertised responses are admitted for each participating operation; packed-entry
allocations are additionally admitted before decode.

Rejection reaches only the affected participant. Late
joiners reserve prior work once per operation.

When no participant can admit
further work, the producer retires and fresh callers can start new work without
inheriting a retired caller's budget failure. Successful callers still share
one origin read and one decode; cancellation of one operation does not cancel
another participant's work.

Each waiter holds a lease on the producer. Releasing
its last lease atomically closes admission and cancels a child runtime token;
explicit cancellation joins producer cleanup before returning. Dropped waiters
signal cancellation through the same lease, with runtime shutdown retaining the
join obligation.

Departed participants stop accumulating charges; rejoining the
same operation reserves work performed while it was absent without charging
previously admitted work twice.

Batch locator reads coalesce nearby lazy pack indexes within one immutable
source. Windows retain the source-range and overread bounds and at most 256
indexes; each index still passes its descriptor hash, Git checksum, and inventory
checks before the batch enters the existing parsed-index cache.

Identical windows share a budget-participating producer. Standalone, inline, and single-index reads
retain their existing path. This reduces index requests, not the number of
physical capsule objects or the payload requests needed to fetch them.

Batch index matching sorts requested OIDs once while retaining caller order and
duplicates. Each verified index probes the smaller side, avoiding a complete
request-set scan per small frontier member or an index scan per point read.
Self-contained matches remain preferred over external deltas. This bounds CPU
lookup work; it does not change source-range admission or reduce origin requests.

### Indexed history and blame

First-parent path history and blame use captured path-state change points when
an index is available. History verifies each candidate against raw first-parent
commit/tree entries, including deletion, mode changes and covering type resets.
Continuations resume from the candidate's parent without rewalking unrelated
commits.

Blame preserves line comparison and deletion boundaries while jumping
between relevant versions. Root-path and all-parent history retain their existing
raw traversal; index absence retains the ordinary Git read path. Missing or
corrupt advertised pages are errors, not a reason to switch traversal.

Snapshot constructors treat supplied graph readiness as required metadata: a
missing, corrupt or retired advertised graph fails opening. Index absence still
permits ordinary raw reads. The manifest-based `open` retains its release-tagged
optional-graph policy; cancellation and admission failures are never suppressed.

Operation finalization normalizes wrapped cancellation only when its own scope
was interrupted, preserving real failures and typed locator-close errors.

## Blame comparison reuse

First-parent blame retains verified line mappings by repository identity, ordered
parent/current blob IDs and a fixed algorithm/semantics version. Complete blame
results retain their commit, raw-path and generation identity. Both entry kinds
share the existing blame cache's entry and byte limits; no additional cache
configuration is introduced. Blob verification, comparison-budget charging and
cancellation checks happen before consuming a retained line map. A cached map
never authorizes a commit or substitutes for fetching an unverified blob.
