# crab worktree

Create, list, move, repair, and remove Git worktrees with Crab-aware
hydration state.

## Synopsis

```bash
crab worktree <subcommand> [OPTIONS]
```

`crab worktree` delegates Git-compatible worktree registration and checkout
behavior to `git worktree`, then manages Crab state that Git does not know
about: per-worktree hydrated-pointer caches, hydration policy, and cleanup of
Crab state when worktrees are removed or pruned.

## Subcommands

| Subcommand | Description |
|------------|-------------|
| `add` | Create a linked worktree |
| `list` | List worktrees |
| `lock` | Lock a worktree |
| `unlock` | Unlock a worktree |
| `move` | Move a worktree |
| `remove` | Remove a worktree and its unlocked Crab state |
| `prune` | Prune stale Git worktree records and unlocked Crab state |
| `repair` | Repair Git worktree administrative links |

Git-compatible flags are passed through where the installed Git version
supports them. Newer Git options are rejected before mutation when the local
Git does not support them.

## Add a Worktree

```bash
crab worktree add ../wt feature/data-cleanup
```

The path is the user-supplied Git worktree path. `../wt` is only an example;
Crab does not hard-code a default linked-worktree location.

Use the same branch and detached-HEAD flags you would use with Git:

```bash
crab worktree add -b experiment ../experiment main
crab worktree add --detach ../scratch HEAD
crab worktree add --no-checkout ../empty HEAD
```

## Hydration Policies

`crab worktree add` can record or apply a Crab hydration policy for the new
worktree:

| Policy | Behavior |
|--------|----------|
| `--hydrate=lazy` | Create a pointer-only worktree and hydrate later on demand |
| `--hydrate=pointer-only` | Keep Crab pointer files materialized as pointer bytes |
| `--hydrate=full` | Run `crab hydrate --all` in the new worktree after Git creates it |
| `--hydrate-include <glob>` | Hydrate selected Crab pointer files after creation |
| `--hydrate-manifest <path>` | Hydrate paths selected by a manifest file |
| `--hydrate-manifest-ref <ref:path>` | Hydrate paths selected by a manifest stored in Git |
| `--hydrate-profile <name>` | Hydrate paths selected by `crab.toml` profile |

When no explicit hydration policy is passed, Crab reuses the same project
defaults that `crab clone` would resolve from `crab.toml`. For example,
`[hydrate] default = "eager"` becomes full hydration, and
`hydrate.auto_patterns` becomes selective hydration.

If Git creates the worktree successfully but post-create hydration fails,
`crab worktree add` returns an error and preserves the new worktree. Retry
from inside that worktree:

```bash
cd ../wt
crab hydrate
```

## No-Checkout Worktrees

`--no-checkout` preserves Git behavior: Git creates the worktree metadata but
does not populate working-tree files.

If a materializing hydration policy is selected with `--no-checkout`, Crab
stores it as a pending per-worktree policy instead of trying to hydrate
immediately:

```bash
crab worktree add --no-checkout --hydrate=full ../empty HEAD
cd ../empty
crab hydrate
```

The later `crab hydrate` consumes the pending policy when no explicit selector
is provided. Explicit hydrate selectors still win for that invocation and do
not erase a separate pending full or selective policy:

```bash
crab hydrate --include 'models/*.bin'
```

In a no-checkout worktree, `crab hydrate` does not perform a general Git
checkout. It materializes only selected Crab pointer files by reading pointer
blobs from the current worktree's Git index or `HEAD` tree. Non-Crab tracked
files, and unselected Crab pointer files, remain absent until the user runs Git
checkout commands or selects them in a later `crab hydrate`.

Lazy, pointer-only, and clone-default lazy no-checkout policies do not leave a
pending hydration marker, because there is no default `crab hydrate` action to
consume. You can still hydrate selected Crab pointer files later by passing an
explicit selector such as `crab hydrate --include 'models/*.bin'`.

## Prefetch

`crab worktree add --prefetch` warms selected content in the Crab cache without
claiming files are materialized in the working tree. The selection must be
bounded by clone defaults, `--hydrate=full`, `--hydrate-include`,
`--hydrate-manifest`, `--hydrate-manifest-ref`, or `--hydrate-profile`.

With `--no-checkout`, prefetch remains cache-only. It must not create working
tree files. Run `crab hydrate` later when you want selected Crab pointer files
materialized.

## Git LFS Boundary

`crab hydrate` only materializes Crab pointer files. It does not run `git lfs
fetch`, `git lfs checkout`, or `git lfs pull`.

Git LFS is compatible with Git worktrees through Git's normal filter and LFS
commands. Linked worktrees share the repository's common LFS object cache, but
each worktree has its own checked-out files. Use Git LFS commands from inside
the target worktree when you want LFS files materialized:

```bash
git lfs fetch
git lfs checkout
# or:
git lfs pull
```

`git worktree add --no-checkout` suppresses Git checkout, so LFS smudge does
not run during creation. Later Git checkout/reset/sparse-checkout commands and
Git LFS commands control LFS materialization.

## Per-Worktree State

Crab stores worktree-local state under the main worktree's shared `.crab`
directory:

```text
.crab/worktrees/main/
.crab/worktrees/<linked-worktree-id>/
```

Each worktree has its own `hydrated-pointers-v1.sqlite` cache and hydration
policy. Cache rows use the exact no-follow stat captured from a verified
published file descriptor. WAL transactions preserve unrelated rows from
concurrent hydrators and update only changed paths. A linked worktree never
treats another worktree's cache as authoritative state.

## Copy-on-Write Hydration

When `crab hydrate` (including post-create hydration from `crab worktree add`)
finds the same Crab pointer already hydrated in a sibling worktree, it first
tries a filesystem copy-on-write clone. Candidate lookup is content-addressed,
so the paths and branches do not need to match. Crab accepts a clone only after
checking its size and full BLAKE3 hash against the pointer, then preserves the
destination mode and publishes it with an atomic rename.

Concurrent hydrators may publish the same destination. Each keeps a proof for
its own verified file descriptor; a later rename does not turn the earlier
publication into a failure. Git index updates and validation-cache reuse still
require that proof to match the current path and indexed pointer. A content
edit detected on the published inode is an error, not a new verification proof.

CoW cloning is automatic on supported macOS and Linux filesystems. It has no
flag or separate hydration mode. Unsupported filesystems, cross-filesystem
worktrees, stale or corrupt cache entries, source mutation, and clone failures
fall through to the normal chunk/Xorb hydration path. This optimization does
not change Crab's CDC, chunk deduplication, Xorb storage, push, or remote
reconstruction format.

The cache is never used to bypass clean-filter input verification. Git permits
the pathname supplied to a clean filter to name a worktree file whose bytes
differ from the filter's standard input. Crab therefore hashes the supplied
bytes whenever Git invokes the filter; exact Git index-stat refresh after
hydrate prevents unchanged files from invoking it during ordinary status and
diff operations.

After a CoW clone, worktrees are independent: editing one file does not alter
the sibling file. Both files initially share physical extents only where the
filesystem supports that behavior. This is distinct from writable VFS overlay
CoW, which keeps changes in a mount overlay instead of publishing a normal
working-tree file.

Legacy JSON hydrated-pointer caches, including the shared
`.crab/hydrated-pointers.json` path, are not read, migrated, or rewritten.

## JSON Listing

Use `--json` for structured worktree identity output:

```bash
crab worktree list --json
```

By default this stays fast and reports Git worktree fields plus Crab identity.
Add `--with-crab-state` when you need hydration policy, cache, pointer, and
speculation state summaries:

```bash
crab worktree list --json --with-crab-state
```

## Related Commands

- [`crab hydrate`](crab-hydrate.md) - materialize Crab pointer files
- [`crab dehydrate`](crab-dehydrate.md) - replace hydrated files with pointers
- [`crab status`](crab-status.md) - inspect current worktree hydration state
- [`crab clone`](crab-clone.md) - clone with Crab filter setup and hydration defaults
