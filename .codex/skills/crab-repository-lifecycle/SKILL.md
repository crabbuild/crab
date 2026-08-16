---
name: crab-repository-lifecycle
description: Set up, adopt, clone, configure, and maintain the Git-facing lifecycle of a Crab repository. Use whenever a user mentions `crab init`, `crab setup`, `crab clone`, mirror mode, tracking patterns, adoption, worktrees, config, Git filter installation, or repository onboarding.
compatibility: Crab CLI repository with Git and a configured Crab or object-storage remote.
---

# Crab repository lifecycle

Own the transition from an ordinary Git repository to a Crab-aware repository
and the surrounding local configuration. Read the exact guide before giving a
command sequence; defaults such as lazy checkout, remote naming, and config
precedence are contracts, not guesses.

## Command scope

`init`, `setup`, `clone`, `mirror`, `worktree`, `config`, `track`, `untrack`,
`install`, `uninstall`, and `completions`.

Use `crab-large-files` for the mechanics of hashing, staging, hydration, and
dehydration. Use `crab-git-sync` for a push/pull/import/export transfer.

## Onboarding sequence

1. Inspect the repository root, existing Git remotes, `.crab.toml`,
   `.crab/config.toml`, `.gitattributes`, and worktree state. Preserve existing
   patterns and user edits.
2. Choose the canonical setup path:
   - `crab init` for a new Crab remote and repository config.
   - `crab setup` to install the local large-file integration after init.
   - `crab clone` for a new checkout; decide lazy, eager, include, exclude,
     and optional chunk-index warming from the user's intent.
   - `crab adopt` when full files already exist in a working tree and should
     become Crab pointers.
3. Configure tracking deliberately. Auto-detection is a proposal to review,
   not permission to rewrite unrelated `.gitattributes` entries. Use dry-run
   for uncertain patterns and explain the resulting filter behavior.
4. Confirm the Git filter driver and Crab remote are installed in the scope the
   user requested. Do not silently switch between repository, global, and
   system Git config.
5. For worktrees, inspect the shared Git metadata and the Crab-specific
   worktree state before creating, switching, cleaning, or hydrating one.
6. After setup, hand off content operations to `crab-large-files` and prove the
   first real add/commit/push or clone/hydrate path with
   `crab-cli-verification` when the user asks whether setup works.

## Safety boundaries

- Never overwrite `.gitattributes`, `.crab.toml`, or `.crab/config.toml`
  without reading and preserving existing entries.
- Never claim a repository is ready because the command parsed. Check the
  installed filter, remote URL, tracked patterns, and a representative file.
- Treat `--force`, history rewrite, worktree cleanup, and system-wide install
  as explicit state changes. Preview or confirm when the user did not already
  authorize them.
- Keep provider credentials and tokens out of command output and reports.

## Read first

- `crab/docs/guides/getting-started.md`
- `crab/docs/guides/init.md`
- `crab/docs/guides/clone.md`
- `crab/docs/guides/adopting-existing-repos.md`
- `crab/docs/guides/project-config.md`
- `crab/docs/guides/worktree.md`
- `crab/docs/guides/track.md`
- `crab/src/cmd/{init,setup,clone,mirror,worktree,config,track}.rs`
- `.codex/skills/crab-cli-core/references/contracts.md`
