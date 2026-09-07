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
out of memory. Successful streamed verification records a validator-bound
receipt when the provider exposes an ETag or version, allowing later presence
checks to avoid re-reading the object body. A configured primary fallback can
serve reads when a selected replica is stale or unavailable; receipts are
written to the source that passed verification.

`verify_origin(oid, expected_size)` performs a fresh SHA-256 and exact-size check
without reading/writing verification receipts or using the configured fallback.
Supply an origin-only store and bound the expected size and request deadline at
the caller. It checks response metadata before consuming the body and rejects
streams that exceed it. Four body verifications may run per process; hashing
runs on blocking workers that retain admission after caller cancellation. The
ordinary receipt-aware path uses the same body verifier when a receipt misses.

`LfsLockManager` provides the shared CAS-backed LFS lock record format at
`{prefix}/lfs/locks/{blake3(path)}`. Crab's CLI uses this namespace so locks
remain visible across local clients and worktrees.

## Usage

```rust
use bytes::Bytes;
use crab_lfs::LfsObjectStore;
use crab_storage::{StorageProviderKind, build_static_env_store};
use sha2::{Digest, Sha256};

let store = build_static_env_store("models", StorageProviderKind::S3)?;
let lfs = LfsObjectStore::new(store, "team/repository");
let data = Bytes::from_static(b"large-object-content");
let oid: [u8; 32] = Sha256::digest(&data).into();

lfs.put(&oid, data.clone()).await?;
assert_eq!(lfs.verify(&oid).await?, data);
# Ok::<(), Box<dyn std::error::Error>>(())
```

For large local files, use `put_stream(&oid, path)` so the upload uses bounded
multipart buffers and aborts an incomplete upload on hash failure. Use
`object_path_for` when a higher-level Crab read path needs the canonical object
key.

## Boundaries

- [`crab-git`](../crab-git/README.md) parses and classifies the pointer blob.
- [`crab-storage`](../crab-storage/README.md) builds the object store and maps provider
  errors.
- The CLI and transfer-agent protocol remain in higher-level product crates.

The only content identity used here is the Git LFS SHA-256 OID. Crab-native
file hashes, shards, and Xorbs are owned by [`crab-xet`](../crab-xet/README.md).
