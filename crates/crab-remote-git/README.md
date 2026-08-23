# crab-remote-git

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
- `RemoteGitRepository::is_current`: metadata-only manifest identity check for
  safely reusing a pinned immutable handle;
- `RemoteGitRepository::operation`: one typed operation kind,
  cancellation-aware locator session, protected correlation ID, and aggregate
  work budget;
- `RemoteGitRepository::{refs,resolve,snapshot}`: ref and reachable-revision
  selection;
- `RemoteGitRepository::{generate_pack,generate_pack_cached}`: verified
  delta-preserving response packs, with immutable reuse for no-have requests;
- `RemoteGitSnapshot::{entry,list_directory,blob_metadata,read_blob}`: browser
  navigation and Git-representation content;
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

## Performance model

Object storage is the correctness authority. Runtime memory is disposable and
bounded. Exact locators avoid pack scans, range reads avoid complete pack
downloads, immutable reads are single-flight, and object, parsed-object,
manifest, inventory, negative, blame-result, and pack-index caches are byte
bounded. Cached blame results remain subject to the current operation's
logical, traversal, history, blame, and response limits; a warm result cannot
bypass a stricter caller budget.
Batch scheduling is lazy and its concurrency is the minimum of origin,
blocking-decode, object-flight, logical-object, storage-request, fetched-byte,
and inflated-byte limits. Archive traversal produces one entry at a time; its
pending tree work is bounded by the verified tree-object limit.
Services may keep a bounded cache of cloned immutable repository handles and
use `is_current` after a short freshness interval. A changed manifest always
requires a new complete open handshake; cached state is never refreshed in
place.

No-have response packs can be persisted beneath the repository's immutable
`generated-packs/v1` namespace. Keys bind physical repository identity,
manifest Git state, the visible authorization union, canonical request
semantics, output policy, and ordered object selection. Complete pack bodies
and descriptors are verified on every read. Runtime single-flight and the
existing renewable internal-lock contract coalesce concurrent producers;
cancelling one waiter does not cancel work still needed by another process.

Directory listing reads only the selected tree. Child sizes are absent unless
the caller requests bounded page-only metadata. Comparison prunes equal tree
IDs. History, diff, blame, archive, storage, inflation, and response work have
independent aggregate limits.

History remains authoritative over verified raw commit objects. When the
manifest names an immutable `CommitGraphSummary`, open bounds it to 16 MiB,
verifies its Blake3 identity, validates its OIDs, parent lists, and topological
generations, and retains it only after manifest/inventory/locator coverage has
matched. A snapshot uses it only while each summary parent list exactly matches
the corresponding raw commit; missing summary entries fall back to raw parent
order and can never hide a reachable commit.

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

## Live qualification example

`qualify_remote` exercises repository open, snapshot/commit reads, cold and
warm directory/blob reads, history, path history, compare, diff, blame, and a
complete archive through one shared runtime:

```console
cargo run -p crab-remote-git --release --example qualify_remote -- \
  <bucket> <repository-prefix> <path-changed-by-head>
```

The example reports elapsed time plus canonical `crab-storage` read attempts
and bytes for each operation. Those counters include manifest, inventory,
pack-index, and pack-body reads but exclude SlateDB locator-internal reads, so
they are useful for regression comparison rather than complete provider
billing. The example uses an explicit larger archive qualification budget; it
does not change library or service defaults.

## Content representations

`read_blob` returns the exact Git blob representation. It classifies ordinary
Git blobs, Crab pointers, and Git LFS pointers but never materializes pointer
targets. Logical Crab content belongs to `crab-read`; verified LFS content
belongs to `crab-lfs`. Service composition decides whether those representations
are enabled.
