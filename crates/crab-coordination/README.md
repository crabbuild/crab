# crab-coordination

`crab-coordination` owns the mutable authority that makes concurrent Crab
writes safe. It provides short-lived object-store leases for simple
serialization and a linearizable active-active write-coordinator contract for
multi-region ref commits.

## Why it exists

Crab stores immutable objects, but refs, manifests, and transaction state are
mutable. Uploading objects is not enough: two writers must not publish
conflicting ref updates, and garbage collection must not delete objects still
owned by an in-flight transaction. This crate centralizes those rules so push,
replication, repair, and GC share one authority model.

## Architecture

There are two complementary coordination paths:

```text
Single repository mutation                 Active-active mutation
        │                                          │
        ▼                                          ▼
PushLock: short-TTL CAS lease          WriteCoordinator: durable state machine
        │                                          │
        └── object-store lock                      ├─ Pending
                                                   ├─ ObjectsUploaded
                                                   ├─ Committed
                                                   ├─ Materialized
                                                   └─ Aborted
```

`PushLock` protects a Git ref or internal resource under a repository prefix.
It has a default five-minute TTL, holder-checked release, renewal, and
expired-lease reclamation. Enable it with `object-store-lock`.

`WriteCoordinator` exposes health, begin/upload/commit/materialize/abort,
ref lookup, GC safety snapshots, repair snapshots, and write fencing. The
provider-specific DynamoDB, Spanner, and Cosmos DB implementations share the
same CAS-backed state contract. `commit_uploaded_push` is the canonical
helper for the monotonic begin → upload-confirmation → commit → regional
materialization path.

## Usage

Use the in-memory coordinator to exercise the transaction contract in a test
or local integration:

```rust
use crab_coordination::{
    commit_uploaded_push, CommitRequest, InMemoryWriteCoordinator,
};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let coordinator = InMemoryWriteCoordinator::new();
let outcome = commit_uploaded_push(
    &coordinator,
    CommitRequest {
        operation_id: "push-123".into(),
        writer: "writer-a".into(),
        region: "west".into(),
        manifest_generation: 7,
        refs: vec![],
        uploaded_objects: vec!["objects/manifest-7".into()],
        target_regions: vec!["west".into()],
    },
).await?;

assert_eq!(outcome.operation_id, "push-123");
# Ok(())
# }
```

For a lock-backed critical section, compile with `object-store-lock` and
acquire a lock from an `Arc<dyn object_store::ObjectStore>` using
`PushLock::acquire_ref_default`. Always release the returned lock, including
on error paths; its release operation is holder-checked.

Provider features are independent:

```toml
[dependencies]
crab-coordination = { version = "1", features = ["coordinator-dynamodb"] }
```

## Boundaries

- [`crab-storage`](../crab-storage/README.md) owns object access and CAS
  primitives; this crate owns the mutation protocol built on them.
- [`crab-metadata`](../crab-metadata/README.md) owns manifests and indexes;
  coordination decides when a new manifest generation becomes authoritative.
- [`crab-auth-store`](../crab-auth-store/README.md) supplies scoped stores;
  credential validity is separate from write serialization.
