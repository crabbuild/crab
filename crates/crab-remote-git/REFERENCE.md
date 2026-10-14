# crab-remote-git reference

[Start with the crate README](README.md) for the usage path and ownership map.

`crab-remote-git` is the canonical filesystem-free read API for Git data stored
by Crab. It reads committed manifests, immutable pack inventories, exact object
locations, pack ranges, and typed Git objects directly from `crab-storage`. It
does not clone a repository or create a local object database.

## Consistency model

A repository handle is pinned to one validated tuple:

- manifest generation;
- immutable pack-inventory hash;
- exact object-locator coverage.

Opening retries that complete handshake once when publication races a reader.
An older or absent locator returns `RepositoryIndexing`; inconsistent newer
metadata fails closed. A snapshot then pins a reachable commit and root tree.

The caller supplies `RepositoryIdentity`, including the current physical
placement generation. This identity scopes every shared cache and single-flight
key. A managed service must authorize and resolve the active placement before
constructing it.

## Public API

The supported entry points are:

- `RemoteGitRuntime`: process-wide bounded caches, origin/decode admission, and
  metrics;
- `RemoteGitRepository::open`: generation-consistent repository open;
- `RemoteGitRepository::from_snapshot`: immutable committed-journal repository
  view without locator publication, for callers that own retention and freshness;
- `RemoteGitRepository::from_snapshot_with_catalog_tail`: the same immutable
  view with opportunistic exact base-catalog lookup and journal-pack-first misses;
- `RemoteGitRepository::is_current`: metadata-only manifest identity check for
  safely reusing a pinned immutable handle;
- `RemoteGitRepository::operation`: one typed operation kind,
  cancellation-aware locator session, protected correlation ID, and aggregate
  work budget;
- `RemoteGitRepository::{refs,resolve,snapshot}`: ref and reachable-revision
  selection;
- `RemoteGitRepository::{generate_pack,generate_pack_cached,generate_pack_request_cached}`:
  verified response packs, with immutable reuse after selection or before an
  exact request is planned;
- `RemoteGitSnapshot::{entry,list_directory,list_tree_blobs,list_tree_recursive,blob_metadata,read_blob}`:
  browser navigation, bounded metadata-only tree traversal, seekable recursive
  blob pages, and Git-representation content;
- `RemoteGitSnapshot::{history,path_history,compare,diff,blame}`: bounded Git
  semantics without a checkout;
- `RemoteGitSnapshot::{archive,archive_stream}`: bounded traversal, with the
  stream owning operation cleanup.

Paths and cursor payloads are opaque bytes. Callers must preserve `GitPath`
bytes at transport boundaries and must sign `PageCursor` values before exposing
them to untrusted clients.

Every operation must be finalized with `OperationContext::finish`. Streaming
archive traversal owns and finalizes the context itself. Dropping either uses a
tracked best-effort cleanup fallback, while explicit completion preserves close
errors. `OperationLimits::max_duration` bounds locator open and semantic work;
expiration cancels the operation and returns a typed timeout.

## Migrating from 0.1 to 0.2

Version 0.2 marks the asynchronous graph-membership API. Tagged Crab releases
v1.0.1 and v1.2.0 through v1.2.4 expose the synchronous 0.1 method; consumers
must update their exact dependency requirement to `=0.2.0` and await the result:

```rust
use crab_remote_git::{RemoteGitRepository, Result};
use gix_hash::ObjectId;
use tokio_util::sync::CancellationToken;

async fn membership(
    repository: &RemoteGitRepository,
    candidates: &[ObjectId],
    roots: &[ObjectId],
    cancel: &CancellationToken,
) -> Result<Option<Vec<bool>>> {
    repository.commits_reachable_from(candidates, roots, cancel).await
}
```

`None` still means no complete pinned graph. `Some` preserves candidate order
and duplicates. The call admits metadata reads and completes its operation
before returning, including cancellation and close-error handling. Retain the
runtime until active operations finish, then await shutdown. Do not block a
Tokio thread to imitate the old synchronous call.

The crate-version boundary does not change stored graph formats or require a
repository rewrite. Released version-1 graphs retain full validation; paged
capsule graphs retain authenticated dependency reads. Existing CLI callers
already await this method. `crab-read`, `crab-write` and `crab-remote` also move
to 0.2 because tagged public error variants expose these dependency types.
Update their exact requirements together; passing a 0.1 remote Git error into
a 0.2 wrapper is not a compatible conversion. The SDK's public values and error
categories hide those concrete types and remain version 0.1.
The workspace pins the affected libraries to 0.2 exactly, while SDK package
qualification discovers each library's declared version from Cargo metadata.
Cargo treats a change in the leftmost nonzero version component as incompatible;
see the [Cargo SemVer contract](https://doc.rust-lang.org/cargo/reference/semver.html#change-categories).

## Completing an operation

Keep the semantic result until `finish` has closed the locator session:

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

| Semantic result | Locator close | Returned result |
| --- | --- | --- |
| Success | Success | Value |
| Error | Success | Semantic error |
| Success | Error | Metadata close error |
| Error | Error | `CloseAfterFailure`, retaining both typed errors |

Deadline expiration converts success or cancellation into `Timeout` before
close-result selection. Other semantic errors retain their identity.

At service shutdown, stop admission and finish or drop live contexts before
awaiting `RemoteGitRuntime::shutdown`. Shutdown cancels work and waits for
tracked tasks and contexts; it cannot complete while its own caller holds an
unfinished context. Keep the Tokio runtime alive until cleanup finishes.

## Performance model

### Caches and aggregate budgets

Object storage is the correctness authority. Runtime memory is disposable and
bounded. Exact locators avoid pack scans, range reads avoid complete pack
downloads, immutable reads are single-flight, and object, parsed-object,
manifest, inventory, negative, blame-result, and pack-index caches are byte
bounded. Cached blame results remain subject to the current operation's
logical, traversal, history, blame, and response limits; a warm result cannot
bypass a stricter caller budget.

Blocking Git decode admission belongs to the queued or running worker. Dropping
its waiting read detaches the task; it keeps its slot until completion or unwind.
Point and batch materialization, entry inspection/inflation, and individual and
coalesced index parsing share that owner. Cancellation can stop admission and
remains cooperative inside kernels; started blocking work is not force-aborted.
Runtime shutdown retains the join obligation. General tracked comparison/sorting
work and response-pack producer admission retain their separate policies.

Timed process caches use Tokio's monotonic clock. Expiry is checked after cache
lock acquisition, and an entry is expired at its exact deadline. Waiting for a
negative-cache lock cannot prolong an exact miss. Manifest cache hits still
require authoritative ETag revalidation during repository opening. Controlled
clock support is a development dependency only; production TTLs and cache keys
are unchanged.

Shared base/index reads recheck their caches after admission: a caller that
missed before a previous producer finished must reuse its verified result.
Index-size producers publish their cache entry before retiring the shared task.
Object checksum/size checks and operation budgets still apply to late cache hits;
parsed indexes are reused only within the caller's source-byte limit.

Batch scheduling is lazy and its concurrency is the minimum of origin,
blocking-decode, object-flight, logical-object, storage-request, fetched-byte,
and inflated-byte limits. Batched object reads fetch selected entries and their
delta dependencies together, then retain verified bases in the bounded object
cache so later history waves do not repeat the same locator and range reads.
Archive traversal produces one entry at a time; its pending tree work is bounded
by the verified tree-object limit.

Services may keep a bounded cache of cloned immutable repository handles.
`is_current` detects manifest changes only. Services that must observe
uncompacted commits capture a validated repository snapshot and open it through
`from_snapshot`; its snapshot digest isolates journal-specific cache entries. A
changed snapshot requires a new immutable handle; cached state is never refreshed
in place. High-throughput services may use `from_snapshot_with_catalog_tail`. It
reads the immutable pack inventory named by the latest catalog checkpoint and
proves that inventory is a subset of the pinned snapshot. Object lookups use that
catalog and search only the remaining pack tail on a miss. If no checkpoint can
be proven as a subset or the derived catalog cannot open, the canonical complete
pack-index path remains; a miss in both the proven catalog and complete tail is
definitive.

Capsule readers use `from_snapshot_with_lookup_sources` with one owned
`SnapshotLookupSources`. Inline locators, preferred frontier indexes,
authenticated object-to-member admission, and non-canonical pack ranges share
this path. The caller authenticates them against its snapshot; opening validates
preferred inventory membership and source pack sizes. An explicitly empty
preferred index set remains distinct from an unspecified set. The canonical
snapshot and catalog-tail constructors retain their existing behavior.

### Generated response packs

Response packs can be persisted beneath the repository's immutable
`generated-packs/v1` namespace. Selection-bound keys cover physical repository
identity, manifest Git state, the visible authorization union, canonical
request semantics, output policy, and canonicalized object selection.

Request-bound keys let identical non-deepening shallow fetches acquire the
renewable cross-process producer lease before reachability planning; the
producer must return a verified self-contained pack. Both key forms include
the generated-pack descriptor format version, so stale derived descriptors
naturally miss after a format change. Complete pack bodies and descriptors are
verified on every read. Runtime single-flight and the renewable internal-lock
contract coalesce concurrent producers; cancelling one waiter does not cancel
work still needed by another process.

The runtime owner must still await shutdown before process exit: returning from
a cancelled waiter does not imply its shared producer has stopped. Lease-bound
work receives a child cancellation token. Renewal failure cancels that token,
awaits cleanup, then releases the lease and returns the lease error. Caller
cancellation also drains the work before release without cancelling the parent
token or replacing a real producer failure. Request-bound producer closures
must use the supplied token, not a separately captured caller token.

Catalog-exact dense filters (`blob:none` and `object:type`) can assemble a
large selected response directly from verified packed entries, preserving
delta payloads and materializing only bases omitted from the selection. The
assembler resolves locators once and orders proven selections by pack identity
and offset before bounded read batches. This prevents OID-ordered batches from
repeatedly fetching overlapping coalesced ranges. OID-based REF_DELTA links
preserve bases across batch boundaries; per-entry CRCs and aggregate byte limits
remain mandatory. Negotiated thin selections share this ordering; conservative
responses without a proven selected-base set retain dependency ordering. Shallow,
path-context, and other filters retain the conservative selected-repack path
until their reachability proofs can bound the same optimization. Repository
GC treats these objects as a soft acceleration cache: recent descriptors
retain their referenced artifacts through the configured grace period, after
which stale descriptor/artifact pairs become collectible.

GC resolves recent descriptors with bounded list-concurrency and streams
validated pairs, keeping response-cache cleanup from turning into an
unbounded read or memory wave as request history grows.

### Source pack reuse

Large response producers download committed source `.pack`, `.idx`, and `.rev`
artifacts. The pack body and both sidecars are validated against the pinned
inventory, then staged with hard links when the workspace permits it, avoiding
the CPU and I/O cost of rebuilding a source index. Shallow selection also keeps
the source installation bounded by skipping an OID enumeration that the
selection planner does not consume; exact response-set validation remains in
place.

Complete-inventory response generation compares the unique source OID union
with the authorized selection. Small overlap between self-contained packs
uses the shared structural assembler instead of native delta recompression;
unproven closure or a different selected set retains exact selected-object
generation. Duplicate removal does not authorize extra objects or bypass pack,
index, entry-CRC, response-size, or receiver integrity checks.

### Trees and history

Directory listing reads only the selected tree. `list_tree_blobs` binary-seeks
each visited Git tree from an exact byte prefix and exclusive continuation,
emits at most one lookahead beyond the requested page, and can collapse a
delimiter subtree into one common prefix. Its canonical depth-first order is
the bytewise order of complete paths, including the implicit `/` after a tree
name. It never reads blob bodies. `list_tree_recursive` remains the exhaustive
metadata traversal for callers that need the complete tree. Child sizes are
absent unless the caller requests bounded page-only metadata. Directory cursors
resume after an exact entry in the pinned tree, preserving Git order when files
and directories share a name prefix. Comparison prunes equal tree IDs and
normalizes its private entry vectors by exact raw path before matching them.
This preserves one type change when a name moves in Git order and avoids false
changes to unchanged prefix siblings. Sorting runs in the tracked blocking
worker; listing and prefix seeking retain canonical Git order. History,
diff, blame, archive, storage, inflation, and response work have independent
aggregate limits.

History remains authoritative over verified raw commit objects. When the
released manifest names a version-1 split commit graph, open bounds the complete graph
to 128 MiB, verifies every descriptor and layer Blake3 identity, validates
stable ordinals, parent closure, corrected generations, and the exact manifest
generation/pack/digest tuple. A snapshot uses it only while each positional
parent list exactly matches the corresponding raw commit; missing or corrupt
acceleration falls back to raw parent order and can never hide a reachable
commit. First-parent path cursors carry the next verified raw parent, so later
pages do not replay newer commits. A matching complete graph groups bounded raw
commit and tree reads for range coalescing without becoming the history authority.

Capsule browse graph version 3 uses descriptor and dependency-page reads instead
of loading the complete graph. Its captured ref roots authorize exact ancestor-set
queries; physically retained records do not grant visibility. OID, parent and
generation access is batched and asynchronous. Required page corruption fails
explicitly. Path-state binds the immutable descriptor's verified positional digest
and validates consumed records through the graph reader. Complete recurrence and
physical coverage validation remain the publication/maintenance owner's obligation.
This removes the graph bitmap and full positional hashing from capsule foreground
reads; outer capsule control capture is a separate remaining opening cost.

All snapshot constructors use this same graph validation when their supplied
metadata names an index. Attribution captures the path-state descriptor and
graph identity, batches path prefixes, and fetches only required authenticated
pages. Verified page caching is bounded and private to that captured index;
every invocation wraps the same origin store with its current operation's
read admission. The cache retains no operation admission or cancellation token.
Descriptor/page reads obey aggregate request/byte admission, while entry, depth,
logical-object and response limits also apply to warm queries. Required missing
pages fail as corruption rather than becoming indexing readiness or path misses.
A cancelled waiter can abandon shared descriptor initialization; a successful
initializer retains only immutable metadata, so later operations remain usable.
A corrupt declared path index, or a missing graph needed to interpret it,
returns `Corrupt(PathState)` instead of fabricating attribution. A journal
overlay that changes the Git validation digest discards the base index pointers.
Absent or superseded indexes cause no index-origin reads. Graph opening obeys
request/byte budgets, the operation deadline, caller cancellation and runtime
shutdown. This reader contract does not publish or discover capsule browse
indexes; the maintenance owner must first bind them to the captured v2 state.

`with_commit_graph` attaches a newly durable graph to an immutable copy of a
pinned handle. It keeps authenticated pack lookup sources and runtime identity;
it clears old path attribution because graph ordinals may differ. Graph loading
selects known commit roots from the validated graph and supplied snapshot,
without requiring pack reads during opening. Unresolved tags use verified raw
peeling, so tags to trees or blobs do not disable commit acceleration. Root validation, graph
origin reads and reachability allocation share one operation budget. Standard
opening retains its aggregate handshake budget.

### Diagnostics and deployment

Each operation emits one structured span with only its bounded operation kind,
process-local correlation ID, outcome, and safe error category. Raw OIDs,
paths, content, provider endpoints, storage prefixes, and credentials are not
trace or metric fields. Integrity incidents use the same safe correlation ID
without formatting the source error into normal logs.

Deploy latency-sensitive services in the same region as the object store. A
local RustFS run proves protocol behavior and correctness, not production cloud
latency; cold/warm request counts, bytes, CPU, memory, and tail latency still
need measurement against a representative large repository.

Point reads and bulk traversal have different cost shapes. A deep path has one
dependent tree lookup per component on a cold runtime. History and blame can
perform many small random reads, while an uncached archive may read every tree
and blob. Services should reserve separate admission for these expensive
operations and should not infer archive or blame latency from root-listing
latency.

Resolving a full commit ID uses the validated complete split graph to prove
reachability without object-store reads. When that acceleration is unavailable,
the reader walks verified raw commits breadth-first from the pinned refs, checking
nearby merge parents before older ancestry on either branch. That fallback remains
bounded by the operation's history and object budgets.

The graph computes one reachability map for the handle's pinned commit roots.
Full-OID resolution then performs an ordinal lookup, including exclusion of
retained commits outside those roots. Construction visits each graph edge once
on an admitted blocking worker and charges one byte per graph ordinal before
allocation. Queries against another explicit root set retain their own traversal;
the pinned proof cannot confer visibility from unrelated refs.

## Live qualification

### Command-line qualification

`qualify_remote` measures isolated capsule-v2 opening, ref resolution, commit,
directory, blob, point/batch object, history, attribution and blame queries. Each
workload gets a fresh runtime and runs cold/warm with opening reported separately:

```console
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-remote-qualification \
  cargo run -p crab-remote --features publication --release --example qualify_remote -- \
  <bucket> <repository-prefix> <text-path> [operation|all|build-indexes]
```

Use a unique target directory for your checkout on the mounted workspace volume.
`build-indexes` explicitly runs state-fenced background maintenance; normal
queries only consume published indexes and require attribution readiness.
The JSONL measurements include backend operation outcomes/consumed bytes,
locator-internal reads and graph/path-state decode counts. Backend invocations
are distinct from provider HTTP retries; the scaling harness meters HTTP and
process CPU/peak RSS separately. Oracle records verify Git content hashes and
complete supported blame origins. Archive qualification remains in
`qualify_browse`. See the [delivery and measurement contract](../../crab/docs/architecture/indexed-remote-reads.md).

## Browsing correctness against a real repository

`qualify_browse` emits JSONL evidence using only the remote storage API. It
paginates 1,000 first-parent commits and the complete HEAD tree, checks every directory entry attribution and selected file history/blame, reads 128
spread-out blob paths twice, and streams every HEAD blob through an independent
Git SHA-1 calculation. Paths and commit messages are emitted as byte arrays.
The run succeeds only when a final `complete` record is emitted.

A fresh push can precede object-catalog publication.
Run the explicit `qualify_remote ... build-indexes` operation after publishing
the capsule fixture to construct complete browse indexes. The direct reader
does not perform this write-side work; incomplete attribution remains indexing. See [metadata ownership](../../crab/docs/guides/metadb.md).

Run the built example from an empty directory with the local RustFS environment
configured as described in the [local development guide](../../crab/docs/guides/local-dev-rustfs.md):

```console
/path/to/qualify_browse <bucket> <repository-prefix> <text-path> > browse.jsonl
python3 crab/scripts/e2e/verify_remote_browse.py \
  /path/to/read-only/source-repository browse.jsonl --output report.json
```

The verifier expects the fixture to publish the source revision as `refs/heads/main`
with no other refs (`--revision` selects the source revision; default `HEAD`).
It compares every tree path, mode, and object ID, commit metadata and parent
order, sampled blob sizes, and all streamed content hashes
against native Git. Only the separate verifier accesses the source checkout;
the reader neither runs Git nor creates an object database. The explicit larger
archive budgets belong to this qualification workload, not service defaults.
This proves the uploaded HEAD snapshot and sampled history, not every historical
blob, every API, or production performance.

## Content representations

`read_blob` returns the exact Git blob representation. It classifies ordinary
Git blobs, Crab pointers, and Git LFS pointers but never materializes pointer
targets. Logical Crab content belongs to `crab-read`; verified LFS content
belongs to `crab-lfs`. Service composition decides whether those representations
are enabled.

An unborn default branch does not imply an empty repository: `RepositoryRefs.head`
is `None` and `unborn_head` carries the symbolic branch name even when tags or other
branches exist. Explicit ref reads still resolve normally; a missing HEAD does not
silently resolve to another ref. Call `RepositoryRefs::is_empty()` to test whether
there are any refs.
