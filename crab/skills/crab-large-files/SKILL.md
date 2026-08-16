---
name: crab-large-files
description: Manage Crab-native large files end to end, including tracking, parallel add, staging, pointers, lazy hydration, dehydration, prefetch, selective download, chunk diffs, cache maintenance, and migration.
---

# Crab large files

Use the native Crab path when Git should store small pointer blobs while the
file bytes live in content-addressed object storage. Keep identity, staging,
cache, and remote durability separate in every decision.

## Lifecycle

```text
full file -> Blake3 identity + CDC chunks -> local staging -> Git pointer
           -> commit -> remote push -> durable xorbs/shards/metadata
pointer   -> cache/staging or remote reconstruction -> full file
full file -> verified pointer -> dehydrate -> disk bytes released
```

1. Inspect tracking patterns, Git state, pointer status, staged rows, and the
   current file hash. Use `--dry-run` for broad globs.
2. `crab add` is the parallel path: read and hash each file, chunk it, write
   staged chunks and the push plan, write the pointer, and optionally stage
   the Git path. Never hand-author pointer blobs or copy chunks into the cache.
3. Before a push, flush staged xorbs and confirm the file-to-chunk index covers
   every chunk for the file version.
4. `crab hydrate` resolves patterns, manifests, profiles, sparse checkout,
   local recovery sources, and archived-object restore options before fetching.
   Verify size and Blake3 identity after reconstruction.
5. `crab dehydrate` replaces hydrated bytes only after checking that content is
   unchanged and that an active prefetch profile is not being violated. Use
   `--ignore-profiles` only when that eviction is intentional.
6. Use `fetch` or cache warming when bytes should be available locally without
   replacing pointers. Use selective `download` for files outside a checkout.

## Command map

- `add`, `reset`, `adopt`, `unadopt`, and `undo` change working-tree or index
  state; inspect staged changes before and after each operation.
- `status`, `why`, `ls-files`, `diff`, and `diff-driver` explain pointer,
  hydration, tracking, and chunk differences. Script against JSON schemas,
  not human wording.
- `staging stats`, `staging verify`, and `staging clean` operate on the durable
  add-time staging area. `cache` commands operate on reusable local objects.
- `stat push-plan --verify` checks prepared xorb metadata; it does not replace
  authoritative staged bytes.
- `migrate` rewrites history or converts representations. Require a dry run,
  list affected refs, and preserve a recovery point before mutation.
- `du` distinguishes local staging/cache usage from remote object usage.

## Invariants

- Reconstructed bytes are byte-identical or hydration fails.
- Staged xorbs flush before any bundle or ref push.
- `chunks_for_file(file_hash)` returns every chunk for that file version.
- Shard reconstruction terms cover all chunks; an incomplete result is an
  error, never a partial success.
- Dehydration never discards uncommitted bytes or silently overwrites a file.
- GC and cache eviction never remove data still referenced by a live pointer or
  protected by the configured grace period.

## Verification

Create deterministic files, record their hashes, run add/commit/push, then
clone or dehydrate/hydrate and compare hashes. Check pointer text, staged
metadata, remote object presence, and cache/staging statistics. For a failure,
capture the structured error and confirm no ref, pointer, or file was advanced
partially.
