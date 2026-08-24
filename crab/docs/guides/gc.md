# crab gc

Run garbage collection on the remote store.

## Synopsis

```
crab gc [OPTIONS]
```

## Description

`crab gc` identifies and removes unreachable objects from the remote object
store. Unreachable objects are xorbs, shards, and other data that are no longer
referenced by any git ref (branch, tag, or HEAD). This reclaims storage space
in your cloud bucket.

Garbage collection operates on the remote store, not the local cache. Use
`crab prune` for local cache cleanup.

## Options

| Option | Default | Description |
|--------|---------|-------------|
| `--dry-run` | `false` | List unreachable objects without deleting anything |
| `--force` | `false` | Bypass the grace period — delete all unreachable objects immediately |
| `--yes` | `false` | Skip interactive confirmation when `--force` is used |
| `--grace-period <duration>` | configured value (24h by default) | Override the minimum age, such as `1h` or `7d` |
| `--resume <run-id>` | — | Resume an interrupted destructive run |
| `--scope <repo\|bucket>` | `repo` | Select repository-local or bucket-global GC |
| `--list-profile <profile>` | configured value | Override bucket listing with `adaptive`, `cost`, or `latency` |

Bucket administrators can rebuild the shared GC root registry with
`crab gc --repair-registry --bucket <bucket>`. The repair enumerates repository
manifests, validates each current shard index, CAS-replaces the discovered
entries, and only then marks bucket coverage complete. Destructive bucket GC
fails closed while the registry schema or coverage marker is incomplete.
The registry stores a small record per repository and spreads each repo's
shard roots across deterministic four-hex CAS partitions. Push contention and
write size therefore do not grow with the number of repositories or require a
whole-repository shard-set rewrite. This is a hard cutover: stop older writers,
upgrade every writer, then repair the registry before destructive bucket GC.

Bucket-global xorb and shard listing defaults to `adaptive`. Small namespaces
complete through one recursive stream per kind. Large namespaces cross a
bounded provider-aware probe threshold and restart as concurrent scans of only
the populated two-hex hash partitions. The crossover waits until recursive
pagination costs as many calls as the complete 256-way fan-out, bounding the
restart overhead to about 2x at the threshold. Use `cost` to minimize LIST
calls or `latency` to prefer parallel wall time. The logged `list_requests`
value counts logical streams; provider pagination and retries can issue
additional API requests.

Destructive bucket runs persist candidate batches and partitioned reachability
marks under the run journal. Mark membership is loaded one hash partition at a
time, and an interrupted planning phase is replayed from its original snapshot
before any delete is allowed. Referenced shard closures are authoritative: a
missing or corrupt closure fails closed instead of downloading an unverified
shard body. Closure hashes are read from bounded immutable segments; orphaned
segments from an interrupted publication are collected after the grace period.
File-index reconciliation is committed before the run advances to object
deletion and is retried idempotently after a crash.

Root walking and LIST run without blocking writers. Crab then takes a short
exclusive fence to reconcile the exact root snapshot and a separate short
fence for each 512-object delete batch or file-index partition. If a writer
crosses between batches, its fence epoch invalidates the plan before the next
delete. Each resumed candidate is HEADed under the fence and must still match
its planned ETag/version and size; a recreated or newly fresh key is retained.
After success, candidate batches, outcomes, and mark chunks are retired and
only the small completed-run state remains.

Repository runs use the same durable plan, but walk current, historical,
journal, workflow, and pack roots directly into key-partitioned marks.
Pack-list segments and delete outcomes are consumed in bounded batches;
store-only deletion does not build a process-wide deleted-key list. Bucket
previews use a non-executable journal and remove its temporary batches and
marks after reporting. Repair commands intentionally retain their
collection-oriented behavior.

## How It Works

1. Takes a snapshot of all current git refs (branches, tags).
2. Streams all repository roots (and, for bucket scope, all registered
   repositories' roots) into durable partitioned marks. Dry runs may build a
   preview set for reporting.
3. Lists all objects in the remote store under the repository prefix.
4. Computes the set of unreachable objects (present in store but not reachable
   from any ref).
5. For shared bucket objects, requires a complete ref-registry and preserves
   the union of every registered repository's shard roots. Ordinary push
   registers its base-plus-candidate shard set before manifest CAS.
6. Applies a grace period filter: recently-created objects are retained even if
   unreachable, to avoid deleting objects from in-progress pushes.
7. Deletes unreachable objects that are older than the grace period, or every
   unreachable object when `--force` is explicitly confirmed.

### Grace Period

When `--grace-period` is omitted, Crab uses the resolved
`gc_grace_period` config value (24 hours by default). This prevents race
conditions where a concurrent push creates objects that have not yet been
linked to a ref. Non-force runs also clamp the effective value to the one-hour
minimum.

The `--force` flag bypasses object-age checks after an explicit confirmation.
It does not bypass writer/sweep fencing, bucket ref-registry completeness,
active-active coordinator proof, closure coverage, or reachability.

### Object Categories

GC tracks deletions across several categories:

- Xorbs — chunk data blobs
- Shards — metadata shards
- Packs — Git objects in repo-scope GC

## Examples

### Dry run to see what would be collected

```bash
crab gc --dry-run
```

```
GC dry run:
  Unreachable xorbs:       42 (1.2 GB)
  Unreachable shards:      8 (45 MB)
  Total reclaimable:       1.24 GB
  (no objects deleted — dry run)
```

### Run garbage collection

```bash
crab gc
```

### Force-delete all unreachable objects

```bash
crab gc --force
```

You will be prompted for confirmation:

```
WARNING: --force bypasses the grace period. Objects from in-progress
pushes may be deleted. Continue? [y/N]
```

### Force without confirmation

```bash
crab gc --force --yes
```

## Safety

- The grace period protects against deleting objects from concurrent pushes.
- `--force` requires explicit confirmation (or `--yes`) to prevent accidents.
- GC never deletes objects that are reachable from any ref.
- Without `--force`, GC never deletes objects inside the configured grace
  period (or the one-hour minimum, if the configured value is shorter).
- With `--force`, age protection is disabled, but writer fencing,
  reachability, closure coverage, and bucket registry/coordinator safety checks
  still apply.

## Prerequisites

- The repository must be initialized with `crab init`.
- AWS credentials must be configured with write/delete permissions on the
  bucket.

## Related Commands

- [`crab prune`](crab-prune.md) — remove unreferenced objects from the local cache.
- [`crab fsck`](crab-fsck.md) — check repository integrity.
- [`crab repack`](crab-repack.md) — consolidate remote Git pack files.
- [`crab du`](crab-du.md) — see storage usage breakdown.

## JSON Output

Supports `--json` and `--jsonl`.

- `--json` runs to completion and emits a single result envelope.
- `--jsonl` emits any available progress/warnings followed by a terminal
  `result` event.

### crab gc --json

```json
{
  "schema": "gc",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:20.400Z",
  "data": {
    "packs_deleted": 0,
    "xorbs_deleted": 42,
    "shards_deleted": 8,
    "bytes_reclaimed": 1342177280,
    "dry_run": false,
    "cancelled": false,
    "partial_enumeration": false
  }
}
```

### crab gc --jsonl

```
{"schema":"gc.event","version":"1.0","timestamp":"2026-04-24T18:32:20.400Z","type":"result","data":{"packs_deleted":0,"xorbs_deleted":42,"shards_deleted":8,"bytes_reclaimed":1342177280,"dry_run":false,"cancelled":false,"partial_enumeration":false}}
```

See [Structured Output](structured-output.md) for envelope details, event types,
and error handling.
