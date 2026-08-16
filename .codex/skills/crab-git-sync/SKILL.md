---
name: crab-git-sync
description: Operate and change Crab's Git synchronization and remote-transfer paths. Use whenever a user mentions `crab push`, `crab pull`, `crab ship`, import/export, remote helpers, refs, locks, non-fast-forward handling, or proving remote object and ref side effects.
compatibility: Crab CLI with Git and a reachable Crab remote or object-storage backend.
---

# Crab Git synchronization

Own the boundary between a local Git repository and the Crab remote. Separate
Git refs and packs from Crab-native file content, and preserve the lock/CAS
contracts that serialize remote mutations.

## Command scope

`push`, `pull`, `ship`, `import`, `export`, `lock`, `unlock`, `locks`, and
remote-helper behavior for `crab://`.

Use `crab-large-files` for preparing or reconstructing file bytes, selective
`download`, and prefetch. Use `crab-workflow` for pushing workflow stage cache
entries.

## Choose the path

- Use native `crab push` when the goal is concurrent Crab-aware upload and the
  user wants explicit control of refs or retries.
- Use `crab pull` when the goal is Git pull plus Crab's configured post-pull
  hydration behavior.
- Use `crab ship` for the intentional one-shot add + commit + push path. Treat
  `--no-push`, dry-run, branch, remote, and integration retry options as part of
  the user's requested contract.
- Use the Git remote helper path when Git itself is invoking `git-remote-crab`.
  Do not assume it has the same process or output behavior as a direct CLI
  command.
- Use `import`/`export` for raw object-storage onboarding or snapshot export;
  read their URL and versioning rules before choosing flags.
- Use advisory file locks only for the intended paths and owners. `force` is a
  destructive exception, not a normal retry.

## Push/pull reasoning

1. Inspect the configured remote, current branch/ref, worktree cleanliness,
   staged Crab data, and any existing lock or push state.
2. For a push, ensure staged chunks and prepared xorbs are flushed before
   remote metadata or ref publication. Preserve per-ref lock release on every
   error or cancellation path.
3. For a pull, distinguish fetched pointer blobs from files that still need
   hydration. Do not report full content as available merely because Git refs
   advanced.
4. For non-fast-forward or lock contention, use the command's documented
   integration/retry behavior. Do not add an ad-hoc force-push fallback.
5. Verify the side effect: remote ref state, expected pack/xorb/shard objects,
   audit event where applicable, and a fresh clone or hydrate for content paths.

## Import and export

Treat import as a resumable, journaled migration and export as a snapshot
materialization contract. Read the source, destination, versioning, timestamp,
collision, and dry-run behavior in the guide before running a mutation. Keep
generated manifests and journals outside unrelated tracked changes.

## Read first

- `crab/docs/design/push.md`
- `crab/docs/architecture/git-integration.md`
- `crab/docs/guides/fetch.md`
- `crab/docs/guides/ship.md`
- `crab/docs/guides/import.md`
- `crab/docs/guides/export.md`
- `crab/docs/guides/lock.md`
- `crab/src/git/`
- `crab/src/cmd/{push,pull,ship,import,export,lock}.rs`
- `.codex/skills/crab-cli-core/references/contracts.md`
