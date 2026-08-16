---
name: crab-large-files
description: Manage Crab-native large files end to end, including tracking, parallel add, staging, pointers, lazy hydration, dehydration, prefetch, selective download, chunk diffs, cache maintenance, and migration. Use whenever a user asks about large files, model or dataset files, pointers, hydration, disk pressure, `crab add`, `crab hydrate`, `crab dehydrate`, or `crab migrate`.
compatibility: Crab CLI with a Git worktree and Crab-native large-file tracking.
---

# Crab large files

Use the native Crab path for files whose bytes should live in content-addressed
object storage while Git stores small pointer blobs. Keep the content identity
and the local staging durability boundary explicit in every explanation.

## Command scope

`add`, `reset`, default `status`, `why`, `hydrate`, `dehydrate`, `diff`,
`diff-driver`, `ls-files`, hidden native `FilterProcess`, `fetch`, `prune`,
`du`, `stat`, `cache`, `staging`, `adopt`, `unadopt`, `undo`, `migrate`, and
selective `download`.

Route `crab lfs` and `optimize lfs` to `crab-lfs`. Route remote ref/object
transfer, `push`, `pull`, and `ship` orchestration to `crab-git-sync`.

## Mental model

```text
full file -> hash + CDC chunks -> .crab/staging -> pointer blob in Git
           -> commit -> Crab push -> remote xorbs/shards/metadata
pointer   -> local cache/staging or remote reconstruction -> full file
full file -> dehydrate -> verified pointer, disk bytes released
```

## File lifecycle

1. Before adding, inspect tracking patterns, pointer state, staged rows, and
   the file's current content hash. Use `--dry-run` for broad globs.
2. `crab add` is the high-throughput staging path: it hashes and chunks files,
   writes staged chunks and the file push plan, emits pointers, and optionally
   runs the final Git staging step. Do not bypass the staging contract by
   inventing pointer files or copying chunks into the global cache.
3. After a commit, use `crab-git-sync` for the push. A staged xorb must be
   durable before the bundle push can be considered valid.
4. For `hydrate`, resolve patterns, manifests, profiles, sparse-checkout, local
   recovery sources, and archived-object restore options before downloading.
   Verify the reconstructed file hash and size, not only its existence.
5. For `dehydrate`, protect files selected by an active prefetch profile unless
   the user explicitly chooses `--ignore-profiles`. Confirm that replacing full
   bytes with a pointer does not discard uncommitted content.
6. Use `fetch` or cache warm operations when the user wants bytes available
   locally without replacing pointers. Use selective `download` when they want
   files under a destination without cloning a repository.

## Diagnostics and maintenance

- `status`, `why`, `ls-files`, `diff`, and `diff-driver` explain pointer and
  hydration state. Read their source output schema before scripting against it.
- `staging stats/verify/clean` concerns the repository-local staging area;
  `cache stats/verify/clean/prune` concerns reusable local cache objects. Do
  not conflate them or recommend deleting staging to solve cache pressure.
- `stat push-plan --verify` checks add-time prepared xorb metadata. A plan is
  useful acceleration, not a replacement for authoritative staged bytes.
- `migrate` rewrites history or converts formats. Require a dry-run, identify
  refs affected, and explain rollback/backup expectations before mutation.
- `adopt`, `unadopt`, and `undo` are working-tree transitions. Check Git staged
  changes and content hashes before acting.

## Proof

For any content-changing claim, compare original and hydrated bytes or the
documented Blake3 identity. For remote paths, use the RustFS end-to-end fixture
from `crab-cli-verification`. For implementation work, also read
`crab/docs/design/add.md`, `crab/docs/design/cache.md`, and
`.codex/skills/crab-cli-core/references/contracts.md`.

## Read first

- `crab/docs/guides/large-file-versioning.md`
- `crab/docs/guides/add.md`
- `crab/docs/guides/hydrate.md`
- `crab/docs/guides/dehydrate.md`
- `crab/docs/guides/status.md`
- `crab/docs/guides/staging.md`
- `crab/docs/guides/cache.md`
- `crab/docs/guides/migrate.md`
- `crab/docs/design/add.md`
- `crab/docs/design/cache.md`
- `crab/src/{engine,hydrate,cache}/`
- `crab/src/cmd/{add,reset,status,hydrate,dehydrate,diff,download,migrate,staging}.rs`
