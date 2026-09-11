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
in place.

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

Catalog-exact dense filters (`blob:none` and `object:type`) can assemble a
large selected response directly from verified packed entries, preserving
delta payloads and materializing only bases omitted from the selection. The
assembler uses OID-based REF_DELTA links across read batches; shallow,
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
and directories share a name prefix. Comparison prunes equal tree IDs. History,
diff, blame, archive, storage, inflation, and response work have independent
aggregate limits.

History remains authoritative over verified raw commit objects. When the
manifest names an immutable split commit graph, open bounds the complete graph
to 128 MiB, verifies every descriptor and layer Blake3 identity, validates
stable ordinals, parent closure, corrected generations, and the exact manifest
generation/pack/digest tuple. A snapshot uses it only while each positional
parent list exactly matches the corresponding raw commit; missing or corrupt
acceleration falls back to raw parent order and can never hide a reachable
commit. First-parent path cursors carry the next verified raw parent, so later
pages do not replay newer commits. A matching complete graph groups bounded raw
commit and tree reads for range coalescing without becoming the history authority.

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

## Live qualification

### Command-line qualification

`qualify_remote` exercises repository open, snapshot/commit reads, cold and
warm directory/blob reads, history, path history, compare, diff, blame, and a
complete archive through one shared runtime:

```console
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-remote-qualification \
  cargo run -p crab-remote-git --release --example qualify_remote -- \
  <bucket> <repository-prefix> <path-changed-by-head>
```

Use a unique target directory for your checkout on the mounted workspace volume.

The example reports elapsed time plus canonical `crab-storage` read attempts
and bytes for each operation. Those counters include manifest, inventory,
pack-index, and pack-body reads but exclude SlateDB locator-internal reads, so
they are useful for regression comparison rather than complete provider
billing. The example uses an explicit larger archive qualification budget; it
does not change library or service defaults.

## Browsing correctness against a real repository

`qualify_browse` emits JSONL evidence using only the remote storage API. It
paginates 1,000 first-parent commits and the complete HEAD tree, checks 128
spread-out blob paths twice, and streams every HEAD blob through an independent
Git SHA-1 calculation. Paths and commit messages are emitted as byte arrays.
The run succeeds only when a final `complete` record is emitted.

A fresh push can precede object-catalog publication.
Run `crab metadb owner --once` from the uploader repository to advance the
catalog; repeat owner passes until `action=none` to finish all derived maintenance. The direct reader
returns `RepositoryIndexing` while locator coverage is absent or stale and does
not perform this write-side work. See [metadata ownership](../../crab/docs/guides/metadb.md).

Run the built example from an empty directory with the local RustFS environment
configured as described in the [local development guide](../../crab/docs/guides/local-dev-rustfs.md):

```console
/path/to/qualify_browse <bucket> <repository-prefix> > browse.jsonl
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
