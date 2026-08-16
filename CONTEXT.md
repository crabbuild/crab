# Crab

Crab is a serverless Git remote helper for repositories whose large content lives in cloud object storage. This context defines the domain language used when discussing Crab's Git and large-file behavior.

## Language

**Worktree**:
A Git checkout attached to a repository, consisting of checked-out files plus repository metadata that is partly shared and partly worktree-specific.
_Avoid_: Checkout, workspace

**Working tree**:
The checked-out files on disk inside a worktree.
_Avoid_: Worktree, when referring only to files

**Main worktree**:
The original non-bare worktree created by `git init` or `git clone`.
_Avoid_: Primary checkout, root checkout

**Linked worktree**:
An additional worktree attached to the same repository with Git worktree machinery.
_Avoid_: Clone, branch copy

**Pointer file**:
A small file that represents large content stored outside Git's ordinary blob payload.
_Avoid_: Placeholder

**Hydration**:
Materializing a pointer file into its full file content in a working tree.
_Avoid_: Download, restore

**Dehydration**:
Replacing materialized file content with a pointer file in a working tree.
_Avoid_: Delete, unload

**Hydration policy**:
A user-visible choice that determines which pointer files a worktree materializes and when.
_Avoid_: Hydration mode

**Prefetch**:
Warming large-file content ahead of direct access without necessarily materializing every pointer file in a working tree.
_Avoid_: Hydration

**Worktree identity**:
The stable Crab identity for a worktree, derived from Git's worktree metadata rather than branch name or filesystem path.
_Avoid_: Branch, path

**No-checkout worktree**:
A worktree whose Git metadata exists but whose working-tree files were intentionally not materialized during creation.
_Avoid_: Empty clone, broken worktree

**Per-worktree state**:
Crab-local metadata scoped to one worktree identity rather than the whole repository.
_Avoid_: Shared state
