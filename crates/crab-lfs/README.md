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
`get`, `exists`, and `verify` operations. Its stream APIs let an HTTP
composition boundary verify an immutable object before serving a bounded
range. Successful streamed verification records a validator-bound receipt when
the provider exposes an ETag or version, allowing later presence checks to
avoid re-reading the object body. A configured primary fallback can serve reads
when a selected replica is stale or unavailable; receipts are written to the
source that passed verification.

`LfsLockManager` provides the shared CAS-backed LFS lock record format at
`{prefix}/lfs/locks/{blake3(path)}`. The CLI and the standard HTTP gateway
must use this namespace so locks acquired by either client are visible to the
other.

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
`object_path_for` when a higher-level transfer agent needs range requests.

## Boundaries

- [`crab-git`](../crab-git/README.md) parses and classifies the pointer blob.
- [`crab-storage`](../crab-storage/README.md) builds the object store and maps provider
  errors.
- The CLI and transfer-agent protocol remain in higher-level product crates.
- `crab-lfs-server` owns standard HTTP protocol, authentication, and policy.

The only content identity used here is the Git LFS SHA-256 OID. Crab-native
file hashes, shards, and Xorbs are owned by [`crab-xet`](../crab-xet/README.md).
