---
name: crab-lfs
description: Implement and operate Crab's Git LFS compatibility layer, including native and transfer-agent modes, LFS hooks, pointers, locks, fetch/push/pull, conversion, deduplication, pruning, and protocol diagnostics.
---

# Crab Git LFS compatibility

Keep Git LFS compatibility separate from Crab-native pointers. They may share
storage or conversion tools, but pointer syntax, hooks, transfer protocols,
locks, and verification rules remain distinct.

## Operating modes

1. Determine whether the checkout uses native LFS, the transfer-agent mode, or
   a conversion path before changing behavior.
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

- `crab lfs` covers status, locks, fetch, push, pull, logs, and transfer-agent
  operations.
- `crab lfs-transfer-agent` is a Git-invoked machine protocol, not a human
  command.
- `crab optimize lfs dedup` removes verified duplicate storage.
- `crab optimize lfs convert` changes representation with a reversible plan.
- `crab optimize lfs prune` removes unreachable local objects under explicit
  verification, age, confirmation, and failure-mode flags.

## Protocol discipline

- Keep transfer-agent stdin/stdout strictly machine-readable; send diagnostics
  to the correct log channel.
- Preserve lock ownership, force semantics, retry boundaries, and request IDs.
- Never place credentials, signed URLs, or raw authorization headers in logs.
- Distinguish an LFS pointer parse, an object download, and a verified checkout
  in status and test assertions.

## Verification

Create a deterministic LFS pointer, upload it, fetch into a clean checkout,
and compare content hashes. Exercise a missing object, an expired lock, a
conversion dry run, and a prune candidate that fails remote verification. Each
failure must leave the pointer, object index, and lock state consistent.
