# Conflicts View

The Conflicts view appears when a merge, rebase, or cherry-pick
encounters conflicting changes. It provides a structured interface
for understanding and resolving each conflict, then continuing or
aborting the operation.

## When It Appears

The Conflicts view activates automatically when:
- A `git merge` produces conflicts
- A `git rebase` step has conflicts
- A `git cherry-pick` encounters conflicts
- The app detects `MERGE_HEAD`, `REBASE_HEAD`, or `CHERRY_PICK_HEAD`
  in the `.git` directory

## Layout

```
┌─────────────────────────────────────────────────────────────────┐
│  ⚠️  Merge in progress — 3 conflicts remaining                  │
│  [Continue]  [Abort]                                            │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  Conflict Files:                                                │
│                                                                 │
│  ⚠️  src/handlers.rs          both modified                     │
│  ⚠️  src/config.rs            both modified                     │
│  ⚠️  README.md                deleted by them                   │
│                                                                 │
│  ─── Resolved ───                                               │
│  ✅  src/main.rs              both modified (resolved)          │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

## Features

### Operation Banner

The top banner shows:
- **Operation type** — merge, rebase, or cherry-pick
- **Conflict count** — how many files still need resolution
- **Continue button** — proceed after all conflicts are resolved
- **Abort button** — cancel the operation entirely

### Conflict File List

Each conflicting file shows:
- **Status icon** — ⚠️ unresolved, ✅ resolved
- **File path** — relative to repository root
- **Conflict type** — human-readable description:
  - "both modified" (UU) — both sides changed the file
  - "both added" (AA) — both sides added a new file
  - "both deleted" (DD) — both sides deleted the file
  - "deleted by us" (DU) — we deleted, they modified
  - "deleted by them" (UD) — they deleted, we modified
  - "added by us" (AU) — we added, they have no version
  - "added by them" (UA) — they added, we have no version

### Merge Editor

Clicking a conflicting file opens the three-way merge editor:

```
┌──────────────────┬──────────────────┬──────────────────┐
│  Ours (Current)  │  Result          │  Theirs (Incoming)│
│                  │                  │                  │
│  fn process() {  │  fn process() {  │  fn process() {  │
│    let x = 1;    │    ???           │    let x = 2;    │
│  }               │  }               │    let y = 3;    │
│                  │                  │  }               │
└──────────────────┴──────────────────┴──────────────────┘
│  [Accept Ours]  [Accept Theirs]  [Accept Both]  [Edit] │
└─────────────────────────────────────────────────────────┘
```

Resolution options per conflict region:
- **Accept Ours** — keep the current branch's version
- **Accept Theirs** — take the incoming branch's version
- **Accept Both** — include both changes (order: ours then theirs)
- **Edit manually** — open the result pane for free-form editing

### Pointer Conflict Card

When Crab pointer files conflict (both sides modified a large file),
a specialized card shows:
- Both pointer hashes
- File sizes for each version
- Option to choose which version to keep
- No three-way merge (binary content can't be merged)

### Resolution Flow

1. Open each conflicting file
2. Choose a resolution strategy for each conflict region
3. Save the resolution (marks the file as resolved)
4. When all files are resolved, click **Continue**
5. The operation completes (merge commit created, or rebase continues
   to next step)

### Abort

Clicking **Abort** runs the appropriate abort command:
- Merge: `git merge --abort`
- Rebase: `git rebase --abort`
- Cherry-pick: `git cherry-pick --abort`

This restores the repository to its pre-operation state.

## Context Menu

Right-click a conflict file for:
- Accept ours (entire file)
- Accept theirs (entire file)
- Open in merge editor
- Mark as resolved
- Open in external editor

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Enter` | Open selected file in merge editor |
| `Cmd+Shift+C` | Continue operation |
| `Cmd+Shift+A` | Abort operation |
| `Space` | Mark selected file as resolved |

## Integration

- The Changes view shows a banner when conflicts exist
- The Branches view's merge/rebase actions navigate here on conflict
- The StatusBar shows "MERGING" / "REBASING" indicator
- The Command Palette includes conflict resolution commands
