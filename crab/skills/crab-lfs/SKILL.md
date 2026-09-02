---
name: crab-lfs
description: Implement and operate Crab's Git LFS compatibility layer, including native and transfer-agent modes, LFS hooks, pointers, locks, fetch/push/pull, conversion, deduplication, pruning, and protocol diagnostics.
---

# Crab Git LFS compatibility

Keep Git LFS compatibility separate from Crab-native pointers. They may share
storage or conversion tools, but pointer syntax, hooks, transfer protocols,
locks, and verification rules remain distinct.

## Operating modes

1. Determine whether the checkout uses Crab-native pointers, Crab's Git LFS
   filter/storage implementation, the custom transfer agent, or a conversion
   path before changing behavior.
2. Inspect `.gitattributes`, Git filters, LFS configuration, pointer format,
   object location, and the hook or protocol entry point.
3. For fetch, push, pull, and checkout, prove both remote object identity and
   local materialization. A valid pointer alone is not proof of content.
4. For conversion between LFS and Crab formats, require a dry run, enumerate
   affected paths and refs, and retain the documented rollback boundary.
5. For deduplication or pruning, verify the destination object or Crab cache
   before deleting an LFS object. Respect recent-object and remote-verification
   policies.

## Command scope

- `crab lfs install/uninstall/update` owns LFS Git config and hooks. It is
  separate from top-level `crab install` and from `crab skills install`.
- `track/untrack`, `pointer`, `clean`, `smudge`, `filter-process`,
  `merge-driver`, hooks, and `ext` own Git LFS compatibility surfaces.
- `fetch/pull/push/checkout`, `ls-files`, `status`, `fsck`, `prune`, locks,
  environment, version, and logs own transfer and local-object lifecycle.
- `crab lfs clone` is a deprecated compatibility wrapper; new automation
  should use normal clone plus explicit fetch/pull behavior.
- `crab lfs-transfer-agent` is a Git-invoked machine protocol, not a human
  command.
- `crab lfs convert` changes the current representation with dry-run and
  rollback boundaries. `crab lfs migrate import/export/info` is the explicit
  history-analysis and rewrite surface.
- `crab lfs dedup` and `crab optimize lfs dedup` remove only verified duplicate
  storage. `crab optimize lfs prune` and `crab lfs prune` require explicit
  reachability, recent-object, remote-verification, and confirmation policy.

## Protocol discipline

- Keep transfer-agent stdin/stdout strictly machine-readable; send diagnostics
  to the correct log channel.
- A pre-push hook must upload required LFS objects before the Git ref becomes
  visible. Clean/smudge/filter-process transform bytes locally and do not by
  themselves prove remote publication.
- Preserve lock ownership, force semantics, retry boundaries, and request IDs.
- Never place credentials, signed URLs, or raw authorization headers in logs.
- Distinguish an LFS pointer parse, an object download, and a verified checkout
  in status and test assertions.

## Verification

Create a deterministic LFS pointer, upload it, fetch into a clean checkout,
and compare content hashes. Exercise a missing object, an expired lock, a
conversion dry run, and a prune candidate that fails remote verification. Each
failure must leave the pointer, object index, and lock state consistent.
