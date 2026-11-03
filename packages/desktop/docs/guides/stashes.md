# Stashes View

The Stashes view manages git stashes — temporary snapshots of your
working tree that let you save uncommitted changes and restore them
later. Useful when you need to switch branches without committing
work in progress.

## Accessing the Stashes View

- Click the **Layers** icon in the activity bar
- Stashes are only available for local repositories

## Layout

```
┌─────────────────────────────────────────────────────────────────┐
│  [+ Stash]  [Refresh]                                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  stash@{0}  WIP on main: add dashboard metrics                  │
│  stash@{1}  fix: partial memory leak work                       │
│  stash@{2}  WIP on feature/auth: token refresh                  │
│                                                                 │
│  (empty state: "No stashes. Use + Stash to save your            │
│   current changes for later.")                                  │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

## Features

### Stash List

Each stash entry shows:
- **Ref** — `stash@{0}`, `stash@{1}`, etc. (most recent first)
- **Message** — the stash description (auto-generated or custom)

### Creating a Stash

Click **+ Stash** in the toolbar:
- Saves all uncommitted changes (staged + unstaged)
- Optionally include untracked files
- Optionally provide a custom message
- Working tree is restored to a clean state

### Stash Operations

#### Apply

Restores the stash's changes to the working tree without removing
the stash from the list. Useful when you want to apply the same
changes to multiple branches.

#### Pop

Restores the stash's changes and removes the stash from the list.
This is the most common operation — "I'm done with the detour, give
me my changes back."

If applying the stash causes conflicts, the stash is NOT removed
(you'll need to resolve conflicts first, then manually drop it).

#### Drop

Permanently deletes a stash without applying it. A confirmation
dialog prevents accidental deletion.

### Crab Pointer Compatibility

Crab pointer-tracked files survive stash/pop correctly because git
stash operates on blob content (the pointer blob), not on hydrated
file bytes. After popping a stash that includes pointer files, the
hydration state is preserved.

## Context Menu

Right-click a stash entry for:
- **Apply** — restore without removing
- **Pop** — restore and remove
- **Drop** — delete permanently
- **Copy ref** — copy `stash@{N}` to clipboard

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Cmd+Shift+S` | Create new stash |
| `Enter` | Apply selected stash |
| `Delete` | Drop selected stash (with confirmation) |
| `Cmd+Shift+P` | Pop selected stash |

## Auto-Refresh

The stash list refreshes automatically when:
- The file watcher detects changes to `.git/refs/stash`
- A stash operation completes
- The view gains focus

## Error Handling

- **Apply/pop with conflicts**: Shows a warning toast and navigates
  to the Conflicts view
- **Drop confirmation**: Requires explicit confirmation since the
  operation is irreversible
- **Empty stash**: The "Stash" button is disabled when the working
  tree is clean (nothing to stash)
