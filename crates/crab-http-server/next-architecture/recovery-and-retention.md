# Recovery, compaction, and backups

[Design index](README.md) · Proposed architecture; not implemented.

Restore begins at the [authoritative control record](storage-protocol.md#authoritative-control-record)
and runs under a [recovering activation](ownership-and-load-balancing.md#acquisition-and-restore).
The [validation plan](validation-and-delivery.md) defines the fault evidence
required before enabling writes or destructive collection.

## Recovery and failure behavior

### Exact restore

Read the control record from origin and acquire a recovering activation before
opening a writable database. Fetch the named recovery root and every dependency
by explicit key. Validate path scope, digest, length, encoding, page size,
transaction continuity and checksum chain. Apply through the exact advertised
end position; reject gaps, overlaps that do not form an allowed compaction
cover, and unexpected database identity.

Run `PRAGMA integrity_check` and `PRAGMA foreign_key_check` before initial
serving activation. Verify the repository UUID and supported migration ledger.
An expensive integrity check contributes to RTO and must be measured; skipping
it is not a hidden fast path.

A failed restore never triggers creation of an empty database. Distinguish
brand-new catalog entries from repositories whose published graph is missing
or corrupt. A later operator restore can deliberately choose a retained backup
as a new generation under a quiesced transition.

### Failure matrix

| Failure point | Durable state | Required behavior |
| --- | --- | --- |
| Before SQLite transaction | Prior head | Reject/retry without effects |
| During local transaction | Prior head | Roll back or discard local file on restart |
| After local commit, before capture | Prior head | No success; recover prior head if disk lost |
| After capture, before upload | Prior head | Retain staging while resolving; no success |
| After LTX upload, before manifest | Prior head plus orphan bytes | Ignore orphan during restore |
| After manifest upload, before head CAS | Prior head plus orphan graph | Ignore unpublished graph |
| Head CAS accepted, response lost | New published head | Reconcile request identity; never allocate duplicate |
| Head CAS loses to takeover | Successor-owned prior published head | Fence old actor; its tail stays unreferenced |
| Process killed after HTTP success | Published graph includes result | Recreated Pod restores result |
| All Crab Pods lose local disks | Object-store graph only | Restore all demanded cells from storage |
| Object store unavailable | No new publication proof | Block cells and return bounded retryable errors |
| Peer unreachable but control renews | Owner still authoritative | Diagnose network; do not steal on TCP failure alone |
| Owner pauses past lease interval | Possible local tentative state | Successor CAS fences publication; old actor stops on resume |
| Disk full/WAL capture error | Prior or indeterminate head | Stop admission, resolve publication, preserve evidence |
| Bad LTX checksum or missing published segment | Unusable recovery graph | Fail closed for cell; report exact dependency |
| Git commits before SQL completion | Durable outbox plus Git effect | Reconcile and publish final SQL state |
| Shutdown grace expires | Last published head survives | New owner resolves all requests and outbox states |

### Worked takeover race

```text
Initial: owner A, epoch 19, control revision 84, head H42.

A locally commits issue 51 and uploads segment S43 and manifest H43.
B qualifies takeover and CASes revision 84:
    owner B, epoch 20, recovering, published head H42.
A attempts to publish H43 using revision 84: rejected.
B restores H42, publishes an epoch-20 snapshot, then serves.

S43 exists in object storage but is not part of authoritative state.
A must not return issue 51 as committed.
The original request ID can be retried on B and committed once there.
```

Reverse the two CAS operations and B must inherit H43 instead. Both orders are
covered by the same single-key ordering; no scan of old epoch tails is needed.

## Compaction, retention, and backups

### Compaction authority

Compaction creates a different physical representation of the same published
application revision and end checksum. It does not create a new user mutation.

1. Pin a published input manifest.
2. Read and verify its selected complete LTX ranges.
3. Write immutable compacted objects and a candidate recovery graph.
4. Reconstruct or validate the candidate's exact endpoint against the original.
5. Publish the replacement graph with a control CAS that preserves logical state.
6. Retain old inputs until no current restore or retained backup depends on them.

The first scheduler permits bounded compaction per node and per cell. It can
build outside the application actor, but final publication goes through the same
control coordinator. If head advancement invalidates the plan, rebase only with
verified unchanged coverage or retry later.

LTX file compatibility needs an explicit capability gate. The inspected Celld
README describes both frame and block layouts, including a reader-first rollout
requirement even where the nominal file version alone does not distinguish
support. Crab must advertise actual decoding capability and qualify golden files
before allowing a new writer/compactor format.
[Pinned compatibility notes](https://github.com/denoland/celld/blob/10cb1303dac710dcb3b557e318e08c855261f68b/crates/ltx/README.md#file-compatibility)

### Retention and collection

First delivery keeps remote LTX and manifests; automatic destructive collection
remains disabled until retention and concurrent restore tests pass. Bound growth
operationally and measure it; this is a delivery stage, not a permanent policy.

Before enabling collection, implement explicit recovery roots for active control
state, retained backups, migration evidence and active restore/compaction pins.
A collector marks reachable immutable objects, waits a configured grace period,
then revalidates roots before deleting exact scoped candidates. A minimum age
alone is insufficient if a restore can outlive that age; use renewable pins or a
documented bounded operation lifetime.

Never reuse a deleted content key as a new dependency without racing collectors
being excluded. Never apply a broad bucket lifecycle expiration to authoritative
LTX or manifest prefixes. Stale-owner uploads are collected only when unreferenced
and outside protected in-flight windows.

Application collection is separate from Git xorb GC. It may not delete Git
dependencies or bypass the repository's existing GC rules. Material deletions
must remain scoped, observable and recoverable where the provider permits it.

### Backups and point-in-time restore

A backup record pins a verified published manifest and its application revision,
timestamp, generation, schema and required decoder capabilities. Retaining an
object-store version of `control.json` alone is not sufficient unless all named
immutable dependencies also remain retained.

Ordinary LTX compaction can discard intermediate transaction states. Initial
restore supports retained checkpoint positions, not an arbitrary timestamp
inside a merged segment. Fine-grained PITR requires retaining the appropriate
uncompacted coverage and a verified time-to-position index. Wall-clock timestamps
alone do not order concurrent system events.

A complete repository backup needs an independently defined Git checkpoint and
asset retention set in addition to SQL. Quiesce cross-domain workflows and record
both sides if the backup promises a consistent PR/Git view. Independent backups
at different times are not a transactionally consistent repository snapshot.

Restore into a new generation, preserve the old graph as evidence, fence prior
writers through the current control record, and validate cross-domain references
before reopening writes. Returning to an old SQL snapshot can intentionally lose
later operations; that is an explicit operator recovery action, not ordinary
failover.
