---
name: crab-git-sync
description: Operate and change Crab Git synchronization and remote-transfer paths. Use for push, pull, ship, import/export, remote helpers, refs, locks, non-fast-forward handling, and remote side-effect proof.
---

# Crab Git synchronization

Own the boundary between local Git history and a Crab remote. Separate Git
objects and refs from Crab-native file content, and preserve the lock/CAS
contracts that serialize remote mutations.

## Choose the path

- `crab push` is the explicit concurrent Crab-aware upload path.
- `crab pull` wraps Git pull and applies the configured post-pull hydration
  policy; advancing a ref does not mean full file bytes are present.
- `crab ship` intentionally combines add, commit, and push. Treat message,
  branch, remote, dry-run, no-push, and integration retry flags as a single
  user contract.
- The `crab://` remote helper is Git-invoked and has a machine protocol. Keep
  protocol output clean and do not assume direct CLI behavior.
- `import` brings a raw object-storage prefix into a new Crab-backed history;
  `export` materializes a selected snapshot into raw storage. Preserve their
  versioning, collision, journal, and dry-run boundaries.
- `lock`, `unlock`, and `locks` are advisory file operations. `--force` is an
  explicit ownership override, not a generic retry.

## Push reasoning

1. Inspect the configured remote, current branch, worktree cleanliness, staged
   Crab data, and existing locks or journals.
2. Flush staged chunks and prepared xorbs before publishing remote metadata or
   refs. A ref must never point at an incomplete object set.
3. Acquire the per-ref lock before the CAS decision. Release it on every
   success, error, cancellation, and early-return path.
4. Verify connectivity from the new ref tip through the local Git object store
   before committing a remote ref.
5. Handle non-fast-forward and lock contention through the documented
   integration/retry path. Never add an ad-hoc force-push fallback.

## Pull and transfer reasoning

Distinguish fetched pointer blobs, Git packs, staged content, cache objects,
and hydrated working-tree files. After a pull, report exactly which of those
states changed. For import/export, keep generated manifests and journals in a
disposable location and make resumability explicit.

## Proof

Verify remote ref state, expected pack/xorb/shard/manifest keys, lock release,
and any audit event. Then use a fresh clone or hydrate to prove that the
published content reconstructs byte-identically. For negative tests, use a
stale lock, missing staged chunk, divergent ref, or malformed Git object and
assert the stable error plus the absence of a partial ref advance.

Never print credentials, signed URLs, or full private object-store endpoints.
