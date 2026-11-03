# Changes View

The Changes view is the staging and commit interface. It shows your
working tree modifications, lets you stage/unstage at the file or hunk
level, and compose commits — all without leaving the app.

## Accessing the Changes View

- Click the **Changes** icon (dot-in-square) in the activity bar
- Keyboard shortcut: `Cmd+Shift+G`

## Layout

```
┌─────────────────────────┬────────────────────────────────┐
│  File List / Tree       │  Inline Patch Diff             │
│                         │                                │
│  Staged:                │  @@ -10,7 +10,8 @@             │
│    ✓ src/main.rs        │   fn main() {                  │
│    ✓ src/lib.rs         │  -    old_line();              │
│                         │  +    new_line();              │
│  Unstaged:              │  +    another_line();          │
│    ○ README.md          │   }                            │
│    ○ tests/test.rs      │                                │
│                         │  [Stage Hunk] [Discard Hunk]   │
├─────────────────────────┤                                │
│  Commit Composer        │                                │
│  ┌───────────────────┐  │                                │
│  │ Commit message... │  │                                │
│  └───────────────────┘  │                                │
│  [Commit]               │                                │
└─────────────────────────┴────────────────────────────────┘
```

## Features

### File Status List

Files are grouped into sections:
- **Staged** — changes ready to commit (green checkmark)
- **Unstaged** — modified but not staged (hollow circle)
- **Untracked** — new files not yet added to git

Each file shows:
- Status code (Modified, Added, Deleted, Renamed, Untracked)
- Color-coded indicator
- File path (relative to repo root)

### View Modes

Toggle between:
- **List view** — flat list sorted by path
- **Tree view** — hierarchical folder structure
- **Staging zones** — visual drag-and-drop staging areas

### Hunk-Level Staging

Click a file to see its diff in the right panel. Each hunk has:
- **Stage hunk** — add this specific change to the index
- **Unstage hunk** — remove from the index (for staged files)
- **Discard hunk** — revert this change in the working tree
- **Line-level staging** — select individual lines to stage

The hunk toolbar appears on hover over each diff section.

### Batch Actions

Select multiple files (Shift+click or Cmd+click) for batch operations:
- **Stage all selected**
- **Unstage all selected**
- **Discard all selected** (with confirmation dialog)

### Commit Composer

The bottom panel contains the commit message editor:
- **Subject line** — first line, shown in log
- **Body** — extended description (optional)
- **Conventional commit** — dropdown for type prefix (feat, fix, etc.)
- **Scope** — optional scope chip
- **Issue references** — auto-suggest from linked forge
- **Amend** — toggle to amend the previous commit
- **Sign** — GPG/SSH sign the commit

### Commit Readiness

A readiness indicator shows whether you can commit:
- ✅ Staged changes present + message entered
- ⚠️ No staged changes
- ⚠️ Empty commit message
- ❌ Merge conflict unresolved

### Discard Confirmation

Discarding changes is destructive. The app shows a confirmation dialog
when:
- Discarding more than 50 lines
- Discarding an entire file
- Batch discarding multiple files

The dialog shows exactly what will be lost.

## Crab Large File Integration

When large files (tracked by Crab) are modified:
- The **Crab Staged Section** shows files that need `crab add`
- The add flow handles chunking and staging to the Crab index
- After `crab add`, pointer blobs appear in the git staged section

## Context Menu

Right-click a file for:
- Stage / Unstage
- Discard changes
- Open in editor
- Open diff in new tab
- Copy path
- Reveal in Finder

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Cmd+Shift+G` | Focus Changes view |
| `Cmd+Enter` | Commit (when message is focused) |
| `Space` | Toggle stage/unstage for selected file |
| `Cmd+A` | Select all files |
| `Delete` | Discard selected (with confirmation) |
| `Cmd+Z` | Undo last stage/unstage |

## Refresh Behavior

The Changes view auto-refreshes when:
- The file watcher detects changes
- A git operation completes (commit, merge, rebase)
- The view gains focus after being in the background
- Manual refresh via the toolbar button

## Tips and Best Practices

- **Stage by hunk, not by file** — for cleaner commits, stage only
  the relevant hunks from each file rather than entire files
- **Use the commit readiness indicator** — it tells you at a glance
  whether you're ready to commit
- **Review before discard** — the discard confirmation shows exactly
  what will be lost; read it carefully
- **Conventional commits** — enable the type/scope prefix for
  consistent commit messages across your team
- **Amend carefully** — amending rewrites history; only amend commits
  that haven't been pushed

## Troubleshooting

| Issue | Solution |
|-------|----------|
| "Stale hunk" error | The file changed since the diff was loaded; click Refresh |
| Patch apply failed | Another process modified the file; refresh and retry |
| Empty staged section | Ensure you've staged changes (not just modified files) |
| Commit button disabled | Check the readiness indicator for the reason |
