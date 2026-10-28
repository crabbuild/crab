# crab-coordination

`crab-coordination` owns the mutable authority that makes concurrent Crab
writes safe. It provides short-lived object-store leases for simple
serialization and a linearizable active-active write-coordinator contract for
multi-region ref commits.

## Why it exists

Crab stores immutable objects, but ref heads and transaction state are
mutable. Uploading objects is not enough: two writers must not publish
conflicting ref updates, and garbage collection must not delete objects still
owned by an in-flight transaction. This crate centralizes those rules so push,
replication, repair, and GC share one authority model.

## Architecture

There are two complementary coordination paths:

```text
Single repository mutation                 Active-active mutation
        │                                          │
        ├─ PushLock: short-TTL CAS lease           ▼
        │                              WriteCoordinator: durable state machine
        └─ PushAdmissionTicket: fixed CAS slots     ├─ Pending
                   │                                ├─ ObjectsUploaded
                   └─ object-store CAS              ├─ Committed
                                                   ├─ Materialized
                                                   └─ Aborted
```

`PushLock` protects a Git ref or internal resource under a repository prefix.
It has a default five-minute TTL, holder-checked release, renewal, and
expired-lease reclamation. Enable it with `object-store-lock`.

Unrepresentable expiry or renewal deadlines return a configuration error;
deadline arithmetic must not panic or wrap into an expired lease.

Nonblocking slot acquisition reuses the inspected lease's CAS token: reclaiming
a released slot needs one GET and one conditional PUT (plus a failed create for
a new acquisition context). A diagnostic expiry can avoid a backend-clock probe
only when declining admission; reclaiming a live claim still checks backend age.

GC writer fences have a distinct post-commit release: once the authoritative
publication record durably roots every uploaded object, the writer can remove
its holder with a direct CAS and does not need the backend-clock probes that
protect abandoned uploads with quarantine. Error and cancellation paths must
continue using ordinary release.

GC writer acquisition retries refresh an already-installed active claim without
adding another holder or advancing writer history. Expired and quarantined claims
remain lost. Generated holder IDs retain a diagnostic PID and use UUID entropy
across hosts and container namespaces; PIDs and wall clocks alone are insufficient.
Malformed stored fence state remains an error and is never silently repaired.

### Lease lifecycle

`while_renewing` borrows a `PushLock` and polls work alongside lease renewal.
CLI maintenance and shared journal compaction use this path.

| Event | Result and cleanup contract |
| --- | --- |
| Work finishes before renewal | Return the work result without waiting for a pending backend retry. |
| Renewal fails | Signal the supplied cancellation token, stop renewing, and await the work to completion. |
| Work and renewal both fail | Return the work error. |
| Work succeeds after renewal failed | Return the renewal error. |
| Caller drops the wrapper future | Work is not drained; the borrowed lock is not released. |

Await the wrapper to completion, then explicitly release the lease on both
success and error paths. The operation must cooperate with the cancellation
token and close its own resources before returning. Without a token, renewal
failure still waits for the operation; the wrapper cannot stop it for the caller.

CLI repair, diagnostics publication and experiment writes use the product GC
admission owner to await work before releasing repository and global fences.
Cancellation stops repair before regional publication begins; an entered
publication drains through commit resolution. A cancelled repair can require an
idempotent retry to acknowledge already-visible regional state. Storage failures
remain the primary error even when cancellation also reaches cleanup.

### Admission and active-active writes

`PushAdmissionTicket` bounds expensive single-repository push pipelines with a
fixed number of reusable CAS lease slots. Waiting writers own no object and
probe slots in rotating order, so admission work and stored coordination state
remain bounded independently of queue depth. Slot expiry uses backend-authored
object time from one reusable clock key; admission is best-effort fair rather
than FIFO.

A direct CLI publication already owns the repository and global GC
writer fences, so its capacity ticket reuses those claims instead of acquiring
a duplicate fence pair. Standalone admission tickets continue to acquire and
release their own GC fences.

`WriteCoordinator` exposes health, begin/upload/commit/materialize/abort,
ref lookup, GC safety snapshots, repair snapshots, and write fencing. The
provider-specific DynamoDB, Spanner, and Cosmos DB implementations share the
same CAS-backed state contract.

Repo GC uses `acquire_gc_fence` to capture protected objects and close write
admission in one atomic state mutation. A pre-existing fence is rejected
unchanged. After deletion work drains, `release_gc_fence` releases only the
acquired epoch. Every operator fence advances the epoch, even when already
fenced, so GC cleanup cannot resume a later operator fence. Epoch exhaustion
fails without mutation. Advisory GC snapshots do not grant deletion authority.

A process lost after GC admission leaves writes fenced; there is no automatic
expiry or reset. Operator resume remains an explicit recovery override under
the existing `--repair-verified` contract. Before using it, stop and drain all
GC runs for the repository as well as verifying provider failover, object
replication and coordinator-backed repair. Resume during an active sweep
invalidates its exclusion guarantee. Automatic GC cleanup never uses this
administrative override.

For production publication, use `commit_uploaded_push_refs` after uploading
immutable objects, then materialize the exact v2 Capsule transaction before
calling `mark_region_materialized`. Requests and repair gaps require an exact
Capsule descriptor; no manifest-generation planner or projection path remains:

```text
upload objects → commit_uploaded_push_refs → persist regional authority
                                                 → mark_region_materialized
```

Protocol-v2 requests carry the exact base-root, transaction, activation, capsule-run
hash, and run size. Coordinator outcomes assign a monotonic commit sequence so repair
replays regional gaps in consensus order rather than operation-ID order.

Stored coordinator state declares `format: "capsule_v2"`. Unversioned state,
missing Capsule descriptors, retired manifest-generation fields and force-CAS
bypasses are rejected. DynamoDB, Spanner and Cosmos DB decode the whole state
before returning refs, repair gaps or GC protection; invalid state is an error,
never an empty protection set. This includes unversioned records containing
refs after their transaction history has been compacted.

There is no automatic conversion or compatibility reader. Clearing rejected
state would discard in-flight protection and is not a repair. New native
repositories use fresh coordinator authority. The CAS record version remains
separate from this format discriminator.

The coordinator always compares exact old refs; Git force policy is evaluated
before the native transaction is prepared. `CommitOutcome` and CLI audit metadata
report `commit_sequence` when available. The shipped managed API v1 has no
sequence field, so its client metadata leaves the sequence absent instead of
inventing one; its retained diagnostic wire field does not authorize v1 data.
Direct commit replay
validates the exact request before returning an existing outcome. Only the caller
that has completed regional publication may mark that region materialized;
the old combined commit/materialize helper is removed.

## Usage

Use the in-memory coordinator to exercise the transaction contract in a test
or local integration:

```rust
use crab_coordination::{
    CommitRequest, CoordinatedCapsulePublication, InMemoryWriteCoordinator,
    commit_uploaded_push_refs,
};

async fn example() -> Result<(), Box<dyn std::error::Error>> {
    let coordinator = InMemoryWriteCoordinator::new();
    let run_hash = "4".repeat(64);
    let outcome = commit_uploaded_push_refs(
        &coordinator,
        CommitRequest {
            operation_id: "push-123".into(),
            writer: "writer-a".into(),
            region: "west".into(),
            capsule_publication: CoordinatedCapsulePublication {
                base_root_digest: "1".repeat(64),
                transaction_id: "2".repeat(64),
                activation_id: "3".repeat(64),
                run_hash: run_hash.clone(),
                run_size: 1024,
            },
            refs: vec![],
            uploaded_objects: vec![format!("repo/v2/capsules/44/{run_hash}")],
            target_regions: vec!["west".into()],
        },
    )
    .await?;

    assert_eq!(outcome.commit_sequence, 1);
    // This state-machine example does not publish a regional Capsule.
    // A product caller must do that before marking its region materialized.
    Ok(())
}
```

For a lock-backed critical section, compile with `object-store-lock` and
acquire a lock from an `Arc<dyn object_store::ObjectStore>` using
`PushLock::acquire_ref_default`. Always release the returned lock, including
on error paths; its release operation is holder-checked.

Provider features are independent. Enable the required provider in a consuming
Crab workspace member; this crate is not published to the registry:

```toml
[dependencies]
crab-coordination = { workspace = true, features = ["coordinator-dynamodb"] }
```

## Boundaries

- [`crab-storage`](../crab-storage/README.md) owns object access and CAS
  primitives; this crate owns the mutation protocol built on them.
- [`crab-metadata`](../crab-metadata/README.md) owns Capsule records and indexes;
  coordination orders exact native transactions before regional materialization.
- [`crab-auth-store`](../crab-auth-store/README.md) supplies scoped stores;
  credential validity is separate from write serialization.
