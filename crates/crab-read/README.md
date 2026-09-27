# crab-read

`crab-read` is Crab's canonical read and hydration orchestration layer. It
turns a Git/Xet pointer into verified bytes by combining manifest metadata,
replica selection, cache-aware storage, shard coverage checks, and Xet file
reconstruction.

## Why it exists

Several product surfaces need the same read guarantees: fetch, hydrate, path
views, and the virtual filesystem. They must all reject unauthorized fetch
wants, select a readable replica, fetch every shard term, and verify the final
file hash. Centralizing that path prevents a surface-specific shortcut from
returning incomplete or unverified content.

## Architecture

```text
Git fetch wants
      │ manifest + admission policy
      ▼
fetch admission and hidden-ref filtering
      │
      ▼
replica readiness / routing policy
      │
      ▼
StoreClient + CachingStore
      │ manifest → file index → shard/xorb objects
      ▼
ShardHydrator → Xet reconstruction → whole-file BLAKE3 verification
```

`FetchAdmissionPolicy` defaults to allowing ref tips while rejecting arbitrary
object wants. It can also admit reachable objects and hide configured refs.
Replica selection reports readiness, generations, fallbacks, and routing
choices so callers can distinguish an unavailable replica from a corrupt
object.

`ShardHydrator` provides memory, file, and half-open byte-range reconstruction.
Before full-file reconstruction it checks that the pointer's shard terms are
covered; after reconstruction it verifies the requested file hash. A shared
adaptive concurrency controller limits parallel downloads. In-memory outputs
use checked, fallible reservation and cannot grow beyond the declared size;
short or overlong results fail. This is not configured memory admission:
large representable outputs, caller-retained results, and transient decode
still need resource bounds. Cache capacity is not a whole-read memory bound.
`reconstruct_range_to_writer_with_cancel` streams an exact range into a
caller-provided writer and accepts the caller's cancellation token and lookup
session. It checks range bounds before I/O and verifies chunk integrity and
the exact output length without allocating a range-sized result. The caller
must bound the writer and unblock it on cancellation. The existing in-memory
range API retains its clamping behavior and delegates to this writer path.
`reconstruct_range_stream` owns a one-chunk asynchronous backpressure channel
for protocol adapters that do not need a file-index lookup session. Dropping
the stream cancels pending source work; successful completion still requires
consuming it through EOF, where late source and integrity errors surface.
Range verification does not establish the whole-file BLAKE3 hash.
The operation token also reaches download admission, availability checks and
chunk transport. Xet waits for source futures during its final writer join;
these reads must observe cancellation for destination cleanup to finish.
`ReadRuntimeBuilder` attaches
the decoded-range cache using the object cache's
resolved root and budget. Callers cannot accidentally omit range reuse;
unavailable or unsafe cache storage degrades to verified origin reads.

StoreClient reads and fills the decoded-range cache while holding the term's
download permit and reconstruction buffer admission. It caches each requested
range independently and finishes fills before returning decoded bytes. Xet is
not given a cache, so it cannot retain data in detached cache-write tasks.
Cache write errors remain best-effort and cannot replace valid origin output.
Cancellation/drop stops pending write attempts. This is not a persistence
promise when caching is unavailable, over budget, or concurrently evicted,
nor an aggregate filesystem-latency bound.

`ReadError::Reconstruction` retains Xet's failure and the operation's first
terminal read and writer failures. Crab records typed adapter errors before
passing them to Xet, preserving their sources even when Xet reports a secondary
channel error. Recovered hint/cache failures do not become the operation's cause.
Consumers can walk the standard source chain to distinguish origin integrity,
availability hooks, and writer I/O.

A completed writer failure takes precedence over read failure or cancellation.
Otherwise, caller-token and source-reported cancellation return
`ReadError::Cancelled`. The output owner closes the writer before taking the
failure snapshot, preventing late writes from changing it. Runtime initialization
errors also retain their source.

CLI/server adapters own user-facing classification. They must preserve this
chain; converting only its display text loses recovery information. The CLI
atomic-output adapter, not the shared writer API, owns publishing verified
temporary files and leaving an existing destination untouched on failure.

## Usage

Wrap the origin in the cache adapter, create the repository layout, and reuse
one hydrator for a read session:

```rust
use crab_auth::CloudCredentials;
use crab_auth_store::build_store_from_credentials;
use crab_cache_store::{CacheConfig, CachingStore};
use crab_read::{ReadRuntimeBuilder, ReadStoreLayout};
use crab_types::storage::StorageProviderKind;

async fn example(pointer_bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let origin = build_store_from_credentials(
        "bucket",
        CloudCredentials::StaticEnv {
            provider: StorageProviderKind::S3,
        },
    )?;
    let cached = CachingStore::new(origin.clone(), CacheConfig::default())?;
    let layout = ReadStoreLayout::new(origin, "repositories/team/project".to_owned());
    let hydrator = ReadRuntimeBuilder::new(cached, layout, 16).build()?;
    let bytes = hydrator.reconstruct_from_pointer(pointer_bytes).await?;
    println!("read {} bytes", bytes.len());
    Ok(())
}
```

Use `reconstruct_range_from_pointer` for bounded in-memory partial reads,
`reconstruct_range_to_path` for large partial reads, and `reconstruct_to_path`
for complete large files. Range reconstruction limits the recipe to the Xet
chunks overlapping the selected byte interval. The path API makes cold,
low-coverage reads with bounded xorb requests; high-coverage reads may cache a
complete verified xorb. The in-memory API retains its non-installing whole-xorb
source read so VFS windows preserve one-request fetch and decoded-range reuse.
The pointer and metadata remain the source of truth; caches only change where
immutable bytes are fetched from.
Use `reconstruct_to_writer` with a sink for verification or cache warming that
does not need to retain the file. Success verifies actual whole-file hash and
size, but a writer can receive bytes before final verification; consumers must
keep output private until success. Streaming the output does not by itself
bound decoding, downstream retention, or total process memory.

Dropping reconstruction signals child cancellation and closes its owned
buffer/destination even if upstream writer handles remain. This is not a join
of all background work or a latency guarantee for an arbitrary blocking writer.
Size violations are integrity errors; other source failures are preserved
rather than relabeled as short output. Partial-range success checks the exact
clamped length and underlying xorb/chunk integrity, not the whole-file hash.

## Diff term resolution

`TermResolver` serves diff callers that need reconstruction terms or ordered
chunk sequences rather than file output. `TermResolver::new` returns a
configuration error for zero concurrency or values above Tokio's maximum permit
count. One semaphore limits admitted metadata work across this resolver's
concurrent batches. Each batch also retains at most that many worker tasks,
reaping whichever finishes first before scheduling another input. Input and
result collections still scale with batch size; this is not total memory admission.

| API | Per-file resolution failure |
| --- | --- |
| `resolve_batch` | Log and omit the unresolved file. |
| `resolve_sequences_batch` | Log and omit the unresolved file. |
| `resolve_sequences_batch_strict` | Return the first worker error after draining the batch. |

Term batches reuse metadata's `SharedFileIndexLookup`, the same session
owner used by hydration. Each batch binds the handle to its origin and
repository prefix, retaining scoped read behavior. Close works even if unused
clones remain.

Cancellation stops admission and drains workers before closing the shared
file-index lookup session. Workers waiting for a concurrency permit observe the
cancellation token; already admitted metadata reads finish before cleanup.
Dropping the batch cancels its own admission waiters without cancelling the
caller's token or sibling batches. A per-batch cleanup task waits for admission
to end and tracked workers to release their state, then closes the session.
Normal completion awaits that same task. Dropping the batch during cleanup
does not interrupt it, but shutting down the runtime can: await the batch
through cancellation before stopping Tokio. Admitted origin reads still need
their own transport deadlines.

Strict batches retain worker join failures as `ReadError::ResolutionTask`.
Its error source is Tokio's `JoinError`, so diagnostic consumers can distinguish
worker panic from task cancellation without parsing log text. The CLI preserves
that source while retaining its internal-error diagnostic classification.

Replica readiness compares the exact authenticated capsule view before reading
and validating every cataloged shard and xorb body. Large-body hashing and
parsing run outside the async executor; worker failures remain typed as
`ReadError::ReadinessTask`. Product caches may skip repeated immutable-body
validation, but must recheck the replica's authenticated view digest before
selection.

## Boundaries

Dependency preflight consumes `crab-git`'s validated pointer contracts and
delegates LFS integrity to `crab-lfs`; both are lower-level dependencies. Server
authentication, authorization, writer coordination and publication remain with
their composing owners.

`dependency_proof::verify_dependencies` consumes the pointer list produced by
`crab-git::receive_plan::validate`. It binds Crab shard selection to the same
captured repository snapshot, then verifies Crab and LFS payloads at origin.
It checks count, individual size, conflicting declarations and total unique
file bytes before I/O; duplicate content is verified once. The batch deadline
covers selection, admission waits and content verification. Its successful body
traffic is bounded by the lookup budget plus pointer count times the per-content
read limit, excluding transport retries.

Pass an origin-only layout. LFS verification ignores receipts and replica
fallback; extension transforms stay with the client, as the primary OID/size
identify the stored bytes. Verification writes no durable evidence and is not
publication authority. A publisher must hold GC fences and recheck the exact
base before exposing refs. Native HTTP receive/publication remains unfinished.

`capsule_protocol::open_view` loads the v2 checkpoint root, double-collects
complete per-ref-head object metadata around concurrent head reads, and retries
a changing snapshot. It resolves each activation record still referenced by a
prepared head exactly once; committed selects all prepared states for that
activation, while preparing or aborted selects every predecessor. It then loads
the checkpoint and reachable capsule runs with caller-supplied individual and
aggregate byte limits. Exact size, provider version, BLAKE3 identity,
transaction identity, base-root binding, and materialized refs are verified
before the view is returned. Git clone/fetch, pointer catalog lookup, checkout,
and hydration consume this same view.

Checkpoints use the layered source directory exclusively. Ordinary fetch uses
`open_view_from_root_with_layered_control` to keep catalog and visibility bodies
cold; `open_view_from_root_with_control` loads those bodies for consumers that
need full authorization or pointer catalogs. These entry points differ in read
requirements, not storage-format compatibility.

`CapsuleRepositoryView::git_snapshot` captures the same canonical pack inventory
and Git identity used by both capsule Git readers. It performs no storage I/O
and does not publish a v1 manifest. Its synthetic ETag covers the root and every
visible per-ref transaction; unchanged root generation is not sufficient for
snapshot equality. The token is not a provider CAS token. Identical packs in
multiple runs appear once; conflicting metadata for one pack fails closed.
`with_browse_indexes` opt-in attaches only an exact-state derived record without
I/O. Such views require the origin-backed reader; the explicit private-memory
reader cannot resolve external index objects. Ordinary Git readers do not load
the record. Fetch transition hints accept the same authenticated cross-ref
closure reuse as visibility application when creating a new branch; existing
refs still require an exact expected-old match.
For ordinary tip-bound Git negotiation, an exact chain from every advertised
want to a client have is a sufficient cut point. This proof does not establish
that a historical have remains visible, so the wire owner may send `ready`
without ACKing it. The ordinary authorization, object budget, and complete
pack plan still run before any pack bytes are sent. Unknown, ambiguous, or
incomplete chains continue negotiation.
Publication must recheck capsule activity against a freshly loaded root;
`RemoteGitRepository::is_current` checks the v1 manifest and is not a v2
freshness check.
`git_repository_from_store` retains the supplied origin before and after the
first checkpoint, so placement checks and derived-index readers share the
real repository store. Uncheckpointed pack-byte admission still precedes opening;
verified bodies already present in complete capsules are reused without another
origin read. Only the explicit `git_repository` embedded-pack helper uses a
private in-memory store.

`capsule_protocol::open_ref_view_from_root_for_refs` is the explicit-push
variant. It double-reads only the requested deterministic head keys without
loading checkpoint or capsule payloads, avoiding repository-wide LIST,
unrelated-head GET, and immutable-history GET requests. Its non-selected ref
values are not authoritative; complete advertisement uses
`open_ref_view_from_root`, while Git transfer and cross-ref pointer catalogs
must continue to use `open_view`. Protected-push admission uses the narrower
`read_visible_refs_from_root_for_refs`, which retains the same stable-head and
atomic-activation checks but returns only requested refs and fetches no capsule
or checkpoint payloads.

Layered pack inventory includes both checkpoint sources and newer frontier
sources, counting a repeated physical source only once. Pack counts, bytes,
declared object totals, and visibility identity use the same member inventory.
Concurrent source-range reads own their request descriptors before suspension,
so HTTP and background-maintenance tasks retain Tokio's `Send` contract.

Captured `CRBRUN06` frontier controls supply contiguous lookup-index ranges for
compacted runs. The shared Git reader still validates each original index hash,
checksum and inventory under its existing request/byte limits. Canonical pack
and sidecar ranges remain authoritative for installation and maintenance; the
lookup pool neither changes visibility nor adds eager stable-source reads.
Exact run-member admission is verified in the same control-suffix read, rather
than fetched separately. The caller's frontier byte admission still bounds the
whole source; combining these already-required bytes does not skip admission.
The layered reader also uses the complete authenticated run-member OID map as
physical placement hints for delta bases absent from visibility additions.
These hints avoid unrelated index scans after cache eviction; they do not
authorize fetch wants or establish client ownership of thin-pack bases.

Cold layered installation stages and authenticates every pack and sidecar
before publishing pack files. Body and sidecar ranges share one pre-I/O byte
budget. Its result reports complete visibility only after the downloaded index
OID union exactly matches the authenticated closure and includes every captured
ref/peeled tip; metadata alone is not installation proof. Duplicate pack bodies
are installed once, including repeated members in one compacted run. Native
installation and remote object reads use metadata's content comparison to reject
conflicting commitments while retaining authenticated physical member positions.
A newer per-ref frontier disqualifies checkpoint-only
installation; physical packs with extra objects still require caller-owned
connectivity checks. Hidden-ref/filter/shallow selection remains caller policy
and must use the authorized selected-object path rather than copying all packs.

Cold installation can retain native pack bodies through an optional
`CachingStore`. A hit is length/BLAKE3-verified into a private file; the selected
origin still supplies sidecars, and index/locator checks and the complete
visibility proof still precede publication. Cache corruption uses the canonical
origin path; destination I/O failures remain terminal. The same aggregate byte
admission applies before cache or origin I/O. Strict administrative verification
passes no cache. Filtered, shallow, and incremental selected-object paths do not
inherit complete-pack cache admission.
The selected `StoreLayout` owns both paths and origin for these installers;
there is no independent store argument that can disagree with its authority.
Native and incremental installers take the operation token. They cancel source
waits and await local/blocking work before returning, so the caller can release
reader admission and remove staging afterwards. Do not implement cancellation
by dropping the enclosing installer future. This does not add cancellation to
older administrative entry points that do not accept an operation token.

Native installation and maintenance have different pack contracts. Maintenance
preserves authenticated thin source bytes and their identities. Complete native
installation orders source members by their declared base dependencies, rejects
missing or cyclic dependencies, and repairs only thin packs with Git. Repair
must produce exactly the source OIDs plus declared bases before its files are
installed under their repaired content hash. Self-contained sources keep their
original bytes. Repeated installation verifies existing bodies and sidecars;
corrupt local artifacts are errors, not cache hits. Thin-source installation
does not claim the self-contained cold path's complete-visibility proof.

Historical verification uses that native installer directly from its retained
layered checkpoint, without constructing a synthetic current-root snapshot.
Physical maintenance can bind a retained complete checkpoint to its exact
root with `compacted_view_from_checkpoint`. It checks the same pointer identity,
size ceiling, visibility and transition metadata as the stored compacted reader,
rejects footer-only input, and excludes newer ref heads. The stored compacted
reader uses this same constructor after loading its checkpoint.
Strict fsck and recovery share full immutable-source validation, including the
retained capsule-run transaction/base binding. Source/member hashes remain
mandatory. Recovery currently reads complete sources for this strict proof and
reads member ranges again for native installation; this is not a request-minimal
history-verification claim.

Current-view and historical integrity checks share the installed-database
dependency verifier. It validates external catalog bodies, looks up Crab file
identities in canonical Xet MerkleHash encoding, and verifies whole-file bytes
at origin for both Crab and LFS pointers. Distinct pointer blobs sharing a file
identity reuse its content proof only after every declared size is validated.
Its scan worker drains on cooperative cancellation before the caller may
release the temporary Git database.
Deep metadata diagnosis, rebuild, HTTP adoption and background integrity consume
this same proof after layered installation, without repacking the repository.
Current-view administrative verification also authenticates complete immutable
sources, including framing outside member ranges. It admits the deduplicated
source inventory before the first source read, and bounds source verification
and pack installation separately under the caller's Git byte ceiling. These
strict checks read source bodies and then member ranges; ordinary push/fetch
does not inherit those extra reads. Token cancellation drains native installation
before releasing its temporary database, as it already does for the Git scan.
Rebuild verifies the reachable Git/Xet/LFS closure before publishing a new
checkpoint or claiming a no-op.
Catalog-read statistics count logical shard/xorb verification reads, separately
from file reconstruction and transport retries.

`verify_catalog_file_recipe` shares the CLI's catalog-selected shard and
origin-reconstruction proof. It ignores pointer shard hints, bounds each shard
by the existing 512 MiB format limit, authenticates the selected recipe, and
uses `verify_origin_recipe` to stream its ordered chunks and prove the final
file hash/size. Reconstruction retains at most one bounded xorb and one decoded
chunk. This deep administrative check rereads selected shard/xorb bodies after
catalog verification; it is not a foreground-request optimization. Live
historical Xet restoration and the complete qualification matrix remain required.

Incremental installation revalidates existing pack, index, and reverse-index
hashes before treating a member as local. An entirely local selection returns
an empty installed-path list with a successful admission proof and makes no
origin reads; it does not attempt to consume absent download windows. Local
corruption remains an error, not a reason to skip verification or silently
replace files.

Cold installation resolves source/member iterator closures before awaiting I/O,
keeping its future usable in spawned server integrity tasks. Checkpoint fixtures
exercise that same task boundary as well as pack bytes and visibility proofs.

- [`crab-metadata`](../crab-metadata/README.md) defines manifests, file
  indexes, and shard metadata; this crate consumes them.
- [`crab-cache-store`](../crab-cache-store/README.md) supplies cache-aware
  object access and origin fallback.
- [`crab-xet`](../crab-xet/README.md) owns pointer/chunk/shard mechanics; this
  crate owns the end-to-end read order and verification.
- [`crab-vfs`](../crab-vfs/README.md) and
  [`crab-auth-server`](../crab-auth-server/README.md) are callers, not
  alternate reconstruction implementations.

`ShardHydrator::with_read_admission` scopes GET/HEAD origin admission to a cloned
hydrator while retaining its shared cache, download concurrency and decoded
buffer controls. Shard-hint and bloom-prefilter failures caused by admission
are terminal, so index fallback cannot replace a budget error with not-found.

`ShardHydrator::file_index_lookup` composes a pinned, write-free lazy lookup with
its existing metadata cache. Callers pass that handle to reconstruction and
close it afterward. Unknown reconstruction recipes retain `ReadError::NotFound`
through Xet; a shard missing its indexed file retains a typed corruption error.

The shared runtime defaults to 128 MiB of decoded-output admission. Compressed
fetch buffers and cache I/O coexist with that output, so this is not a total RSS
cap. Explicit caller buffer budgets remain supported. Large-file qualification
tracks physical memory separately from this admission limit.
