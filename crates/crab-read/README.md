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
`ReadRuntimeBuilder` attaches
the decoded-range cache using the object cache's
resolved root and budget. Callers cannot accidentally omit range reuse;
unavailable or unsafe cache storage degrades to verified origin reads.

Reconstruction success waits for that operation's decoded-range cache-write
attempts. Xet 1.6.0 starts those writes in detached tasks; an operation-local
cache owner tracks even tasks not yet polled, so immediate runtime shutdown
does not discard a healthy admitted fill after successful prefetch. Cache
write errors remain best-effort and cannot replace valid origin output.
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
chunks overlapping the selected byte interval. Cold, low-coverage reads fetch
bounded xorb ranges; high-coverage reads may cache a complete verified xorb.
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

- [`crab-metadata`](../crab-metadata/README.md) defines manifests, file
  indexes, and shard metadata; this crate consumes them.
- [`crab-cache-store`](../crab-cache-store/README.md) supplies cache-aware
  object access and origin fallback.
- [`crab-xet`](../crab-xet/README.md) owns pointer/chunk/shard mechanics; this
  crate owns the end-to-end read order and verification.
- [`crab-vfs`](../crab-vfs/README.md) and
  [`crab-auth-server`](../crab-auth-server/README.md) are callers, not
  alternate reconstruction implementations.
