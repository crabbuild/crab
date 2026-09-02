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
- Direct object-store remotes and managed logical repositories are distinct
  publication paths. A managed push prepares a protected session and scoped
  staging grant, uploads immutable data, then asks the service to verify and
  finalize. Never fall back to direct bucket credentials when that path fails.

## Push reasoning

1. Inspect the configured remote, current branch, worktree cleanliness, staged
   Crab data, and existing locks or journals.
2. Flush staged chunks and prepared xorbs before publishing remote metadata or
   refs. A ref must never point at an incomplete object set.
3. Use repository admission to bound expensive direct push work, then acquire
   canonical per-ref locks before the expected-old decision. Admission is
   capacity control; ref locks and journal CAS provide correctness.
4. Verify connectivity from the new ref tip through the local Git object store
   and publish ref-scoped visibility evidence before committing the journal.
5. Treat the ref-journal active marker as the durable atomic boundary. Release
   ref locks after it; manifest, locator, commit-graph, frontier, and other
   derived maintenance belong to the generation owner.
6. Handle non-fast-forward and lock contention through the documented
   integration/retry path. Never add an ad-hoc force-push fallback.

## Concurrent agents

- Prefer one linked worktree and branch per agent. Different refs can publish
  concurrently; same-ref writers are serialized and still need Git history
  integration.
- `crab push --rebase-on-non-fast-forward` is for one current local branch,
  without force or delete refspecs. It waits up to the agent integration lock
  window, fetches and rebases after a competing advance, retries the push, and
  stops for a local conflict instead of resolving code automatically.
- Preserve per-ref outcomes for multi-ref pushes. A partial outcome is not a
  repository-wide success, and retry must target only work that remains valid.

## Pull and transfer reasoning

Distinguish fetched pointer blobs, Git packs, staged content, cache objects,
and hydrated working-tree files. After a pull, report exactly which of those
states changed. For import/export, keep generated manifests and journals in a
disposable location and make resumability explicit.

## Proof

Verify remote ref state, ref-journal transaction and active marker, visibility
evidence, expected pack/xorb/shard objects, lock release, and any audit event.
Then use a fresh clone or hydrate to prove that the published content
reconstructs byte-identically. For negative tests, use a stale lock, missing
staged chunk, divergent ref, or malformed Git object and assert the stable
error plus the absence of a partial ref advance.

Never print credentials, signed URLs, or full private object-store endpoints.
