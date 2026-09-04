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

`ReadError::Reconstruction` retains the typed failure returned by Xet. Its
source wrapper exposes the nested client/writer errors that Xet 1.6 keeps
behind `Arc` without `Error::source` annotations. The store adapter passes
typed errors into Xet instead of formatting them; consumers can walk the
standard source chain to distinguish origin integrity, availability hooks,
and writer I/O. Caller-token and source-reported cancellation return
`ReadError::Cancelled`. Runtime initialization errors also retain their source.
An intermittent protocol CI failure still loses the availability source through
actual reconstruction. The typed-source contract is not fully qualified; see
Plan 017's direct read-through checkpoint for the failing job and investigation.

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

# async fn example(pointer_bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
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
# let _ = bytes;
# Ok(())
# }
```

Use `reconstruct_range_from_pointer` for partial reads and
`reconstruct_to_path` for large files. The pointer and metadata remain the
source of truth; caches only change where immutable bytes are fetched from.
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

## Boundaries

- [`crab-metadata`](../crab-metadata/README.md) defines manifests, file
  indexes, and shard metadata; this crate consumes them.
- [`crab-cache-store`](../crab-cache-store/README.md) supplies cache-aware
  object access and origin fallback.
- [`crab-xet`](../crab-xet/README.md) owns pointer/chunk/shard mechanics; this
  crate owns the end-to-end read order and verification.
- [`crab-vfs`](../crab-vfs/README.md) and
  [`crab-auth-server`](../crab-auth-server/README.md) are callers, not
  alternate reconstruction implementations.
