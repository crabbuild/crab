---
name: crab-cli-core
description: Own Crab CLI dispatch, command contracts, structured output, errors, cross-cutting behavior, and skill routing. Use when a Crab CLI change spans commands or changes the public CLI surface.
---

# Crab CLI core

Keep the CLI boring and explicit: one canonical path, narrow typed APIs,
stable machine output, and evidence for every externally visible claim.

## Route by command family

- Repository setup: `configure`, `init`, `setup`, `clone`, `mirror`,
  `worktree`, `config`, `track`, `untrack`, `install`, `uninstall`, and
  `completions`.
- Native large files: `add`, `reset`, `status`, `why`, `hydrate`, `dehydrate`,
  `diff`, `ls-files`, `fetch`, `prune`, `du`, `stat`, `cache`, `staging`,
  `adopt`, `unadopt`, `undo`, `migrate`, and selective `download`.
- Git synchronization: `push`, `pull`, `ship`, `import`, `export`, locks, and
  the `crab://` remote helper.
- Workflows: `run`, `repro`, `stage`, `freeze`, `unfreeze`, `exp`, `queue`,
  `workflow`, `data`, `artifacts`, `params`, `metrics`, `plots`, and workflow
  cache operations.
- LFS compatibility: `lfs`, the transfer agent, and `optimize lfs`.
- Storage operations: `gc`, `fsck`, `compact`, `repack`, `optimize`, `metadb`,
  cache/staging maintenance, and storage-focused `du` or `stat`.
- Tiers and replication: `tier`, `replica`, replica optimization, and
  coordinator lifecycle.
- Mounts: `mount`, `unmount`, `daemon`, and virtual filesystem coordination.
- Managed operations: authentication, organizations, repositories, members,
  service accounts, audit, and release manifests.
- Recovery: `doctor`, `env`, `errors`, `logs`, `version`, `upgrade`, and
  `recover`.
- Skill management: `skills list` and `skills install`.

When a change crosses families, keep this skill active and give each stateful
operation one clear owner. Do not build a second wrapper path merely to avoid
moving policy to the correct command.

## Public contracts

1. Parse command arguments before opening repositories, credentials, or remote
   clients.
2. Resolve output mode once. `--json` is one terminal envelope; `--jsonl`,
   where supported, is an event stream ending in a result. Machine output owns
   stdout, progress and logs use stderr, and Git-invoked protocols keep stdout
   reserved for their wire contract.
3. Preserve typed errors and their source chain. Use stable error codes when a
   failure is user-visible; never stringify an error and discard its cause.
4. Keep cancellation safe. Release locks, close metadata databases, flush
   staged data, and remove temporary resources on success, failure, and
   cancellation.
5. Keep compatibility only for an explicit shipped API, serialized format,
   migration boundary, or dependency contract. Otherwise delete the stale
   branch and leave one canonical path.
6. Treat config keys, pointer formats, storage layouts, schema names, and
   command flags as cross-component contracts. Search all consumers before
   changing any of them.

## Agent execution contract

- Discover the installed surface with `crab version`, command-specific
  `--help`, and `crab skills list`; do not infer flags from a different release.
- Prefer read-only commands and `--json` for decisions. Use `--jsonl` only
  when progress events are useful, and require its terminal `result` event.
- Treat stable error code, category, `retryable`, and exit code as the retry
  contract. Never retry from matching a human log phrase.
- Before mutation, name the repository, worktree, ref, remote, and intended
  side effect. Re-read state after mutation instead of trusting optimistic
  progress.
- Give concurrent agents separate worktrees and refs by default. Same-ref
  publication still follows the Git-sync skill's lock and rebase contract.

## Implementation loop

1. State the user-visible contract and the success side effect.
2. Read the complete command definition, implementation, callers, callees,
   siblings, tests, and upstream dependency types that define behavior.
3. Identify the owner boundary; gather inputs, normalize them, decide once,
   then act.
4. Implement the smallest bounded refactor that removes stale paths instead
   of adding aliases, shims, or speculative fallbacks.
5. Add one regression test for the behavior that matters. Test the public
   result, not an incidental helper branch.
6. Run focused validation, then the broadest relevant gate. For a remote or
   filesystem claim, perform an end-to-end smoke with a disposable fixture.

## Skill installation contract

`crab install` configures Crab's Git filter/diff drivers, hooks, completions,
and optional aliases. It does not install agent skills. Use the `skills`
namespace for the embedded Agent Skills catalog.

List the embedded catalog with:

```text
crab skills list
```

Install one self-contained skill with:

```text
crab skills install codex crab-large-files --skill=crab-large-files
```

The provider may be any supported Agent Skills host, including `codex`,
`claude-code`, `gemini-cli`, `cursor`, `windsurf`, `cline`, `roo`,
`github-copilot`, `opencode`, `goose`, `qwen-code`, `trae`, and
`zed`. The default destination follows that provider's documented global
skills directory. `CODEX_HOME`, `CLAUDE_HOME`, and `GEMINI_HOME` override the
corresponding provider home; `--root PATH` selects a final directory for
isolated or project-scoped installs. Existing destination directories require
`--force`.

## Validation

Use a volume-backed, checkout-specific Cargo target directory:

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/<checkout> cargo check --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/<checkout> cargo test --locked
```

Run the command’s real side effect in a disposable repository or local S3
fixture when compilation and unit tests do not prove the claim. Report what
ran and what remains unverified.
