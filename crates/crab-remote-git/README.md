# crab-remote-git

Read Git objects directly from Crab object storage, without cloning a repository
or creating a local object database. This internal workspace crate owns verified
Git reads; services own authorization and representation selection.

## Read path

```text
Caller: authorize and resolve physical placement
  -> RepositoryIdentity + RemoteGitRuntime
  -> RemoteGitRepository::open
       manifest generation + pack inventory + locator coverage
     or RemoteGitRepository::from_snapshot
       validated manifest + committed journal overlay
  -> OperationContext: shared budgets and cancellation
       snapshot -> tree / blob / history / diff
  -> OperationContext::finish: result + locator close
```

A repository handle pins validated metadata. A snapshot pins a reachable commit
and its root tree. Locator publication lag returns `RepositoryIndexing`;
opening never performs write-side catalog maintenance. Empty repositories can
open, but selecting a snapshot returns `EmptyRepository`.

## Choose an entry point

| Need | API | Contract |
| --- | --- | --- |
| Shared admission and caches | `RemoteGitRuntime` | Process-wide; shut down after active contexts finish or drop |
| Open a repository | `RemoteGitRepository::open` | Caller supplies authorized physical placement identity |
| Open a committed journal view | `RemoteGitRepository::from_snapshot` | Caller supplies a validated snapshot, retention, and freshness policy; no catalog required |
| Reuse a handle | `is_current` | Checks manifest identity; journal freshness can require reopening |
| Select a revision | `refs`, `resolve`, `snapshot` | Selection stays within pinned visible refs |
| Browse content | `entry`, `list_directory`, `list_tree_recursive`, `read_blob` | Paths are opaque `GitPath` bytes; blob reads return Git representation |
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

Operation-owned coalesced ranges and packed-entry metadata reads use the same
admission boundary: failed headers charge a request but no advertised body,
and each facade retry reserves its own request and response bytes. An early
range-size check rejects work that already exceeds the remaining byte budget.

Packed-entry, pack-index body and index-size producers share one immutable-read
flight implementation with independent participant budgets. Request attempts and
advertised responses are admitted for each participating operation; packed-entry
allocations are additionally admitted before decode. Rejection reaches only the
affected participant. Late
joiners reserve prior work once per operation. When no participant can admit
further work, the producer retires and fresh callers can start new work without
inheriting a retired caller's budget failure. Successful callers still share
one origin read and one decode; cancellation of one operation does not cancel
another participant's work. Each waiter holds a lease on the producer. Releasing
its last lease atomically closes admission and cancels a child runtime token;
explicit cancellation joins producer cleanup before returning. Dropped waiters
signal cancellation through the same lease, with runtime shutdown retaining the
join obligation. Departed participants stop accumulating charges; rejoining the
same operation reserves work performed while it was absent without charging
previously admitted work twice.
