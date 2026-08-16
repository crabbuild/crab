---
name: crab-storage-ops
description: Inspect, compact, verify, repair, and optimize Crab storage, metadata indexes, staging, caches, shards, packs, xorbs, and garbage collection.
---

# Crab storage operations

Operate on durable storage only after identifying references, grace periods,
metadata ownership, and the exact scope of the command. Read-only inventory
and mutation must be visibly different.

## Command scope

- `gc` enumerates reachable refs, manifests, packs, xorbs, shards, indexes, and
  grace-period candidates; it deletes only the requested repository scope.
- `fsck` verifies Git objects, pointers, file indexes, chunk coverage, shard
  terms, packs, checksums, and metadata consistency. Repair is opt-in.
- `compact` merges small metadata shards while preserving every live row.
- `repack` consolidates Git packs without changing reachable history.
- `optimize plan/apply` coordinates cache, index, xorb, pack, shard, tier, and
  replica actions. Plan is read-only; apply consumes the reviewed plan.
- `metadb diagnose/rebuild/compact/cache-*` operates on metadata databases and
  their local chunk-index cache.
- `staging stats/verify/clean`, `cache stats/verify/clean/prune`, `du`, and
  `stat` report or maintain local storage. Do not conflate staging with cache.

## Safety invariants

1. Never delete a referenced xorb, shard, pack, file index entry, or object in
   the configured grace period.
2. Never use bucket-wide deletion when a repository prefix is sufficient.
3. Close every metadata database on success, error, and cancellation.
4. Rebuild indexes from durable authoritative objects; do not make a cache the
   source of truth.
5. Compaction and repacking preserve content identity and ref reachability.
6. A repair report must identify what was changed, what was skipped, and what
   remains corrupt or unavailable.

## Operating loop

1. Capture the repository or prefix, backend, ref roots, storage classes,
   retention policy, locks, and current usage.
2. Run a dry run or read-only diagnose and save the structured report.
3. Review candidates against live references and recent activity.
4. Apply one bounded operation, with cancellation and lock release intact.
5. Re-run fsck or verification, compare object counts and byte totals, and
   prove a fresh read or hydration for content affected by the operation.

## Verification

Use a disposable prefix containing shared chunks, unreachable objects, small
and large shards, multiple refs, and a recent object. Prove that compaction,
repack, repair, cache eviction, and scoped GC preserve live content while only
the intended candidates change. Include a failed write and interrupted run in
tests for any new mutation path.
