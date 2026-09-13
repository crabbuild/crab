# crab-lfs

`crab-lfs` stores and verifies Git LFS object bytes in Crab object storage. It
owns the LFS object layout and SHA-256 integrity boundary, while leaving Git
LFS pointer parsing to [`crab-git`](../crab-git/README.md).

## Why it exists

An LFS pointer is only a small Git blob; the actual object can be many
gigabytes. Uploads and downloads need a provider-neutral address, idempotent
creation, bounded multipart memory, and verification that the bytes match the
pointer’s OID. Those guarantees belong beside the storage adapter rather than
inside Git parsing or CLI protocol code.

## Architecture

```text
Git LFS pointer (crab-git)
          │  SHA-256 OID
          ▼
LfsObjectStore(prefix)
          │
          ▼
{prefix}/lfs/objects/{aa}/{bb}/{oid}
          │
          ▼
verified bytes through crab-storage
```

`LfsObjectStore` provides idempotent `put`, bounded-memory `put_stream`,
`get`, `exists`, and `verify` operations. Its stream APIs let the direct Crab
CLI transfer path verify immutable objects while keeping file-sized payloads
out of memory. Uploads and explicit `verify_size` record a validator-bound
receipt when the provider exposes an ETag or version, allowing later presence
checks to avoid re-reading the object body. `get_stream` never writes receipts.
A configured primary fallback can serve reads when a selected replica is stale
or unavailable; selected replicas are verified before exposing their stream so
corruption can trigger fallback without replaying emitted bytes.

Full `get_stream` reads without a replica fallback use one body request and
verify delivered SHA-256 and size. Range streams first establish whole-object
proof, then require matching strong ETag/version metadata for delivery. A
provider without a strong validator cannot serve a verified partial stream.
All streams check exact response framing and withhold their final bytes until
EOF verification succeeds, including for HTTP Content-Length consumers.
Dropping a stream is cancellation, not evidence that verification succeeded.

Streamed uploads admit at most four part futures, including the final partial
part. Read/assembly buffers and provider allocations sit outside that queue
bound. Uploads verify size and SHA-256 before completion. Read, part, hash,
and completion failures attempt multipart abort; a cleanup failure preserves
the original upload error. Await the operation to finish cleanup: dropping its
future or terminating the process cannot guarantee remote part reclamation.

Verified HTTP/range streams require a strong ETag or object version. The
response must match the version that passed verification; a same-size
replacement is rejected before its stream is returned. Without such a validator,
use `download_to_file`, which hashes the bytes from one read before succeeding.

Receipts use verifier `crab-lfs/2`. Older receipts trigger fresh hashing because
the previous writer could attach an unrelated HEAD response to an upload's
verified bytes. New uploads no longer write HEAD-based receipts: the first
receipt-aware verifier hashes the stored body and records that response's
metadata. This adds one full verification read before later receipt hits.

`verify_origin(oid, expected_size)` performs a fresh SHA-256 and exact-size check
without reading/writing verification receipts or using the configured fallback.
Supply an origin-only store and bound the expected size and request deadline at
the caller. It checks response metadata before consuming the body and rejects
streams that exceed it. Four body verifications may run per process; hashing
runs on blocking workers that retain admission after caller cancellation. The
ordinary receipt-aware path uses the same body verifier when a receipt misses.

`LfsLockManager` provides the shared CAS-backed LFS lock record format at
`{prefix}/lfs/locks/{blake3(path)}`. `crab-http-server` uses this manager for
the Git LFS File Locking API. The CLI uses the same namespace through a
separate lock manager, so changes here do not automatically reach that CLI
surface.

Malformed shared lock records retain their object key and typed
`serde_json::Error` source, including its category and location. Shared
`unlock_with_id` and `force_unlock` return an exact existing tombstone unchanged
so a caller can recover from a lost success response. Both report `NotFound`
when the record is absent; the CLI's separate force-unlock treats absence as
success.

## Usage

```rust
use bytes::Bytes;
use crab_lfs::LfsObjectStore;
use crab_storage::{StorageProviderKind, build_static_env_store};
use sha2::{Digest, Sha256};

async fn example() -> Result<(), Box<dyn std::error::Error>> {
    let store = build_static_env_store("models", StorageProviderKind::S3)?;
    let lfs = LfsObjectStore::new(store, "team/repository");
    let data = Bytes::from_static(b"large-object-content");
    let oid: [u8; 32] = Sha256::digest(&data).into();

    lfs.put(&oid, data.clone()).await?;
    assert_eq!(lfs.verify(&oid).await?, data);
    Ok(())
}
```

For large local files, use `put_stream(&oid, path)` so the upload uses bounded
multipart buffers and aborts an incomplete upload on hash failure. Use
`object_path_for` when a higher-level Crab read path needs the canonical object
key.

Local upload failures retain the filename, I/O error kind, and original OS
error through `std::error::Error::source()`. Callers can report file context
and inspect the underlying cause without parsing the message.

## Boundaries

- [`crab-git`](../crab-git/README.md) parses and classifies the pointer blob.
- [`crab-storage`](../crab-storage/README.md) builds the object store and maps provider
  errors.
- The CLI and transfer-agent protocol remain in higher-level product crates.

The only content identity used here is the Git LFS SHA-256 OID. Crab-native
file hashes, shards, and Xorbs are owned by [`crab-xet`](../crab-xet/README.md).

Callers with explicit shutdown obligations use `get_stream_with_session` and an
`LfsReadSession`. Drop outstanding read futures and streams before awaiting
session close. Both receipt-miss preverification and full delivery register
blocking SHA-256 jobs with this session; close drains started jobs after a
cancelled join and rejects new verification work. The ordinary `get_stream`
entry point retains its existing global verification admission contract.
