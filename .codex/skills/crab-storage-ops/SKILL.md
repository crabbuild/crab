---
name: crab-storage-ops
description: Inspect, compact, verify, repair, and optimize Crab storage, metadata indexes, staging, caches, shards, packs, xorbs, and garbage collection. Use whenever a user mentions `crab gc`, `fsck`, `du`, `stat`, `cache`, `staging`, `metadb`, `compact`, `repack`, `restripe`, or storage-related `optimize` commands.
compatibility: Crab CLI with a configured object-storage remote; some operations require provider access.
---

# Crab storage operations

Use this skill for operator-facing maintenance of the Crab data plane. Start
with inventory and a dry-run, identify the exact repository and object scope,
then perform the smallest operation that restores the documented invariant.

## Command scope

`gc`, `fsck`, `compact`, `repack`, `du`, `stat`, `cache`, `staging`, `metadb`,
`optimize plan/apply/xorbs/packs/shards/cache/indexes`, and storage-side
`prune`. Route `tier`, `replica`, and workflow-cache operations to their
specialized skills.

## Operation selection

- `du`, `stat`, cache, and staging inspection establish local and remote cost
  before mutation.
- `fsck` checks integrity and may offer safe repairs; capture structured output
  and distinguish missing, corrupt, and merely uncached objects.
- `gc` removes unreachable remote objects only after reference and grace-period
  analysis. Use repository scope. Never run bucket-wide GC for a repository
  task, and never bypass confirmation without explicit authorization.
- `compact` consolidates metadata shards; `repack` consolidates Git packs;
  xorb optimization/restripe changes content-addressed layout. Verify that
  canonical metadata and reads remain valid after each operation.
- `metadb diagnose/rebuild/compact` operates on derived indexes. Rebuild from
  durable shard metadata and prove the rebuilt index agrees with its source.
- Local cache and staging cleanup have different ownership and locks. Check
  active writers before clean, force-breaking a stale lock only with evidence.
- `optimize plan/apply` is an orchestrator. Inspect the plan, cost inputs,
  selected operations, and audit record before applying it.

## Safe maintenance loop

1. Read the relevant guide and source command module.
2. Run the read-only inventory or dry-run in the intended repository scope.
3. Save JSON/JSONL output and identify the exact objects, refs, indexes, or
   segments affected.
4. Apply only the authorized operation.
5. Re-run fsck or the command's verify path, inspect audit events, and perform
   a fresh clone/hydrate when content or metadata was changed.

## Read first

- `crab/docs/guides/gc.md`
- `crab/docs/guides/fsck.md`
- `crab/docs/guides/du.md`
- `crab/docs/guides/stat.md`
- `crab/docs/guides/cache.md`
- `crab/docs/guides/staging.md`
- `crab/docs/guides/metadb.md`
- `crab/docs/guides/repack.md`
- `crab/docs/guides/optimize-xorbs.md`
- `crab/docs/design/cache.md`
- `crab/src/{metadata,storage,restripe}/`
- `crab/src/cmd/{gc/,fsck,compact,repack,optimize,metadb,staging,stat,du}.rs`
- `.codex/skills/crab-cli-core/references/contracts.md`
