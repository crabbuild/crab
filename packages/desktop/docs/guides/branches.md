# Branches View

The Branches view provides full branch lifecycle management — create,
switch, merge, rebase, push, pull, and delete branches with visual
ahead/behind indicators and upstream tracking.

## Accessing the Branches View

- Click the **GitBranch** icon in the activity bar
- The current branch is also shown in the TopBar and StatusBar

## Layout

```
┌─────────────────────────────────────────────────────────┐
│  [+ New Branch]  [Refresh]  Filter: ________            │
├─────────────────────────────────────────────────────────┤
│                                                         │
│  ● main                          ↑2 ↓0   origin/main    │
│                                                         │
│  ▸ feature/                                             │
│    ○ feature/auth-flow           ↑3 ↓1   origin/...     │
│    ○ feature/dashboard           ↑0 ↓5   origin/...     │
│                                                         │
│  ▸ fix/                                                 │
│    ○ fix/memory-leak             ↑1 ↓0   —              │
│                                                         │
│  ─── Remote Only ───                                    │
│    ○ origin/feature/old-branch                          │
│                                                         │
└─────────────────────────────────────────────────────────┘
```

## Features

### Branch Tree

Branches are displayed as a tree grouped by path prefix:
- `feature/auth` and `feature/dashboard` nest under `feature/`
- The tree is collapsible for repos with many branches
- The current branch (HEAD) is highlighted with a filled circle

### Ahead/Behind Indicators

Each branch with an upstream shows:
- **↑N** — commits ahead of upstream (ready to push)
- **↓N** — commits behind upstream (needs pull)
- Color-coded: green (ahead), orange (behind), grey (in sync)

### Pull Mode

Per-branch pull strategy:
- **Merge** — `git pull --no-rebase` (default)
- **Rebase** — `git pull --rebase`
- **Fast-forward only** — `git pull --ff-only`

The pull mode is stored per-branch in localStorage and shown as a
dropdown on the branch card.

### Branch Operations

#### Create Branch
1. Click **+ New Branch** in the toolbar
2. Enter branch name
3. Choose base (current HEAD, another branch, or a commit)
4. Optionally check out immediately

#### Switch Branch (Checkout)
- Double-click a branch to check it out
- Or right-click → Checkout
- Warns if working tree has uncommitted changes

#### Merge
- Right-click a branch → **Merge into current**
- Opens the Merge Dialog with strategy options:
  - Merge commit (default)
  - Squash
  - Fast-forward (if possible)
- If conflicts arise, switches to the Conflicts view

#### Rebase
- Right-click → **Rebase current onto this**
- Opens the Rebase Dialog
- Interactive rebase shows the todo list editor
- Supports edit, squash, fixup, drop operations

#### Push
- Right-click → **Push** (or use the ↑ button)
- Sets upstream if not configured
- Force push requires confirmation dialog

#### Pull
- Right-click → **Pull** (or use the ↓ button)
- Uses the configured pull mode for that branch
- Shows progress during fetch + merge/rebase

#### Delete
- Right-click → **Delete**
- Local-only delete by default
- Force delete (unmerged branches) requires confirmation
- Option to also delete the remote tracking branch

#### Rename
- Right-click → **Rename**
- Updates the local branch name
- Optionally updates the upstream tracking reference

### Set Upstream

- Right-click → **Set upstream**
- Choose from available remote branches
- Or create a new remote tracking branch

### Remote Management

The **Remote Manager Popover** (accessible from the toolbar) lets you:
- View configured remotes
- Add new remotes
- Remove remotes
- Fetch from specific remotes

## Context Menu

Right-click any branch for the full action menu:
- Checkout
- Merge into current
- Rebase current onto
- Push / Pull / Fetch
- Create PR (if forge configured)
- Rename
- Set upstream
- Delete (with force option)
- Copy branch name

## Forge Integration

When a forge provider (GitHub, GitLab) is detected:
- **Create PR** — opens the PR creation dialog pre-filled with the
  branch name and recent commits
- **PR status** — shows if a PR already exists for the branch
- **CI status** — shows pipeline/check status on the branch

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Cmd+Shift+B` | Focus Branches view |
| `Enter` | Checkout selected branch |
| `Cmd+N` | New branch |
| `Delete` | Delete selected branch |
| `/` | Focus filter input |

## Refresh Behavior

The branch list refreshes when:
- A git operation completes (commit, merge, push, pull)
- The file watcher detects `.git/refs` changes
- Manual refresh via toolbar button
- The view gains focus
