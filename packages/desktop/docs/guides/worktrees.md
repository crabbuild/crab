# Worktrees View

The Worktrees view manages git worktrees — separate checkouts of the
same repository that share a single `.git` directory. Worktrees let
you work on multiple branches simultaneously without stashing or
switching.

## Accessing the Worktrees View

- Click the **FolderTree** icon in the activity bar
- Worktrees are a power-user feature for parallel development

## Concepts

### What is a Worktree?

A git worktree is an additional working directory linked to the same
repository. Each worktree:
- Has its own checked-out branch
- Has its own working tree and index
- Shares the object database and refs with the main worktree
- Can be on any branch (no two worktrees can share a branch)

### Use Cases

- Work on a hotfix while a feature branch has uncommitted changes
- Run tests on one branch while developing on another
- Agent tasks use worktrees for isolated execution
- Compare implementations across branches side-by-side

## Layout

```
┌─────────────────────────────────────────────────────────┐
│  [+ Add Worktree]  [Refresh]  Search: ________          │
├─────────────────────────────────────────────────────────┤
│                                                         │
│  ┌─────────────────────────────────────────────────┐    │
│  │ 🟢 main (primary)                    ↑0 ↓2      │    │
│  │    /Users/dev/project                           │    │
│  │    3 dirty files                                │    │
│  └─────────────────────────────────────────────────┘    │
│                                                         │
│  ┌─────────────────────────────────────────────────┐    │
│  │ 🔵 feature/auth                      ↑3 ↓0      │    │
│  │    /tmp/project-auth                            │    │
│  │    Clean                                        │    │
│  └─────────────────────────────────────────────────┘    │
│                                                         │
│  ┌─────────────────────────────────────────────────┐    │
│  │ 🟣 fix/memory-leak                   🔒 Locked  │    │
│  │    /tmp/project-memleak                         │    │
│  │    1 dirty file                                 │    │
│  └─────────────────────────────────────────────────┘    │
│                                                         │
└─────────────────────────────────────────────────────────┘
```

## Features

### Worktree Cards

Each worktree is displayed as a card showing:
- **Branch name** — the checked-out branch
- **Path** — filesystem location
- **Status** — dirty file count, ahead/behind upstream
- **Lock state** — locked worktrees show a lock icon
- **Color accent** — user-assignable color for visual grouping
- **Pin indicator** — pinned worktrees stay at the top

### Adding a Worktree

Click **+ Add Worktree** to open the creation dialog:

1. **Existing branch** — check out an existing branch in a new worktree
2. **New branch** — create a new branch and check it out
3. **Detached HEAD** — check out a specific commit

Options:
- **Location** — where to create the worktree directory
- **Hydration policy** — how to handle Crab pointer files:
  - `all` — hydrate everything immediately
  - `on_access` — hydrate on first read
  - `pointers_only` — keep as pointers

### Worktree Detail Panel

Selecting a worktree opens a detail panel with:
- File tree of the worktree's contents
- Diff view for modified files
- Merge-back actions (merge worktree branch into main)
- Push estimate (how much data would be uploaded)

### Worktree Operations

#### Lock / Unlock
- Lock a worktree to prevent accidental removal
- Locked worktrees show a lock icon and reason
- Right-click → Lock / Unlock

#### Move
- Relocate a worktree to a different directory
- Right-click → Move

#### Remove
- Delete the worktree directory and unlink from the repo
- Confirmation dialog shows:
  - Whether the worktree has uncommitted changes
  - Whether the branch will be deleted
  - Option to force-remove dirty worktrees

#### Color & Label
- Assign a color from preset swatches (red, orange, yellow, green,
  blue, purple, pink)
- Colors appear as a left-border accent on the card
- Useful for visual grouping (e.g., red = hotfix, blue = feature)

#### Pin
- Pin worktrees to keep them at the top of the list
- Pinned worktrees persist across sessions

### Worktree Compare

Compare two worktrees side-by-side:
- Select two worktrees → right-click → Compare
- Shows file-level differences between the two checkouts
- Useful for reviewing divergent implementations

### Push Estimate

Each worktree card can show a push estimate:
- Total bytes that would be uploaded
- Number of xorbs and pointers
- Helps decide whether to push from this worktree

## Context Menu

Right-click a worktree card for:
- Open in Finder/Explorer
- Open terminal here
- Lock / Unlock
- Set color
- Pin / Unpin
- Rename (display name)
- Move
- Compare with...
- Remove

## Metadata Persistence

Worktree UI metadata (colors, pins, display names, hydration policy)
is stored in `.crab/desktop/worktrees.json` within the repository.
This metadata is separate from git's worktree tracking.

## Integration with Orchestrator

The Orchestrator creates worktrees for agent tasks:
- Each task gets a temporary worktree in the system temp directory
- These worktrees appear in the Worktrees view with an "agent" badge
- When a task is applied, its branch is merged and the worktree removed
- When discarded, the worktree is cleaned up automatically

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Cmd+Shift+W` | Focus Worktrees view |
| `Enter` | Open selected worktree detail |
| `Cmd+N` | Add new worktree |
| `Delete` | Remove selected worktree |
