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
