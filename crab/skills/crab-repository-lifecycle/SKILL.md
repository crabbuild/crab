---
name: crab-repository-lifecycle
description: Set up, adopt, clone, configure, and maintain the Git-facing lifecycle of a Crab repository. Use for init, setup, clone, mirror, tracking, worktrees, config, filter installation, and repository onboarding.
---

# Crab repository lifecycle

Own the transition from an ordinary Git checkout to a Crab-aware checkout and
the local configuration around it. Preserve user edits and make the selected
scope explicit.

## Command scope

`configure`, `init`, `setup`, `clone`, `mirror`, `worktree`, `config`, `track`,
`untrack`, `install`, `uninstall`, and `completions`.

## Onboarding sequence

1. Inspect Git remotes, branch and worktree state, Crab configuration,
   `.gitattributes`, existing tracking patterns, and any uncommitted changes.
2. Choose one entry path:
   - `crab configure` is the guided default: it selects the provider, discovers
     credentials, initializes the Git/remote state, installs integration, and
     proposes or applies tracking. Use `--dry-run` for a non-mutating plan.
   - `crab init` creates or adopts a Crab remote configuration.
   - `crab setup` installs local large-file integration after initialization.
   - `crab clone` creates a new checkout; choose lazy, eager, include, exclude,
     and optional cache warming deliberately.
   - `crab adopt` converts already-present full files into staged pointers.
3. Use `crab track` for explicit patterns. Treat auto-detection as a proposal;
   preview broad changes before modifying attributes or configuration.
4. Confirm the remote URL, filter driver, tracked patterns, and selected Git
   config scope. Do not silently switch between local, global, and system
   installation.
5. For worktrees, inspect shared Git metadata and Crab worktree state before
   creating, switching, hydrating, pruning, or removing a worktree. Give each
   concurrent agent its own linked worktree and branch unless they must
   serialize on one ref.
6. Prove one real path: add/commit/push for an existing checkout, or
   clone/hydrate for a new one. Check both Git state and reconstructed bytes.

## Command decisions

- Use `init` for remote and project configuration; do not use `setup` as a
  substitute for remote initialization.
- Use `configure` when both are wanted. With no remote it may reuse discovered
  `crab.toml`; in non-interactive automation supply the remote explicitly.
- Use `clone` when Git history and a new working tree are required. Use
  `download` when selected files are needed without a checkout.
- Use mirror mode only when a named Git remote should be synchronized into a
  Crab remote; make the direction and ref policy visible.
- Use `install` or `uninstall` at the requested Git config scope. Global and
  system changes affect other repositories and require explicit authorization.
- Top-level `install` owns Git filter/diff drivers, repository hooks,
  completions, and optional aliases. Agent skills are installed only through
  `crab skills install`.
- Use `config get` to inspect value origin before changing a key. Preserve
  arrays, unknown sections, comments where supported, and unrelated values.
- Linked worktrees share Git common state and Crab staging, but each has its
  own hydrated-pointer database and hydration policy under the shared `.crab`
  tree. A sibling's state is never authoritative. Hydration may use a verified
  content-addressed filesystem CoW clone, then falls back to normal remote
  reconstruction.

## Safety

- Never overwrite `.gitattributes` or Crab configuration without reading and
  preserving existing entries.
- Never report setup complete because the command parsed. Check the installed
  filter, remote URL, tracked pattern, and a representative file.
- Treat history rewrite, `--force`, worktree deletion, and system-wide install
  as destructive boundaries. Preview first unless already authorized.
- Keep credentials, tokens, signed URLs, and private endpoints out of output.

## Proof checklist

For a new or adopted checkout, record the final remote, filter configuration,
tracked patterns, pointer count, and one byte-identity check after hydration.
For clone behavior, test both the default lazy state and the requested eager or
selective state. For a worktree change, verify the source and destination
worktree remain consistent after switching or cleanup.
