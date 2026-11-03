# Diff Viewer

The Diff Viewer renders file-level differences with syntax
highlighting, inline annotations, and interactive controls. It
appears in multiple contexts throughout the application — the Changes
view, commit detail, PR review, and standalone file comparison.

## Where Diffs Appear

| Context | Trigger | Comparison |
|---------|---------|-----------|
| Changes view | Click a modified file | Working tree vs index |
| Commit detail | Click a file in commit | Commit vs parent |
| PR review | Click a file in PR diff | PR head vs base |
| Explorer | Right-click → Compare | Any two revisions |
| Worktree compare | Compare two worktrees | Worktree A vs B |

## Features

### Unified Diff

The default view shows additions and deletions inline:

```
  fn main() {
-     println!("Hello, world!");
+     println!("Hello, Crab!");
+     init_logging();
  }
```

- Deletions highlighted in red
- Additions highlighted in green
- Unchanged context lines in default color
- Line numbers for both old and new versions

### Split (Side-by-Side) Diff

Toggle to split view for easier comparison of large changes:

```
┌─────────────────────────┬─────────────────────────┐
│  Old (left)             │  New (right)            │
│                         │                         │
│  fn main() {            │  fn main() {            │
│    println!("Hello");   │    println!("Crab");  │
│  }                      │    init_logging();      │
│                         │  }                      │
└─────────────────────────┴─────────────────────────┘
```

Configurable in Settings → Diff → Diff Style.

### Syntax Highlighting

Diffs are syntax-highlighted using Shiki grammars:
- Language detected from file extension
- Highlighting applied to both old and new content
- Themes match the editor theme setting
- Grammars loaded lazily and cached

### Hunk Navigation

Navigate between change regions:
- **Next hunk**: `]c` or toolbar button
- **Previous hunk**: `[c` or toolbar button
- Hunk boundaries are visually marked with a separator line

### Expand Context

Click "Show N more lines" between hunks to reveal additional context
without loading the entire file.

### Word-Level Diff

Within changed lines, individual word changes are highlighted:
- Deleted words: red background
- Added words: green background
- Helps identify small changes within long lines

Configurable in Settings → Diff → Word Diff.

### Whitespace Handling

Options for whitespace display:
- **Show all** — spaces, tabs, and line endings visible
- **Ignore whitespace** — hide whitespace-only changes
- **Trim trailing** — ignore trailing whitespace differences

### Patch Minimap

A minimap on the right edge shows the overall shape of the diff:
- Red marks for deletions
- Green marks for additions
- Click to jump to a specific region
- Useful for navigating large diffs

### Inline Comments (PR Review)

When viewing PR diffs, inline comments appear in the gutter:
- Click the gutter to add a new comment
- Existing comments show as expandable annotations
- Reply to threads inline
- Resolve conversations

### Notebook Diffs

For Jupyter notebook files (`.ipynb`), a specialized diff view shows:
- Cell-level changes (not raw JSON)
- Markdown cells rendered
- Code cells with syntax highlighting
- Output changes (images, tables)

### Crab Pointer Diffs

When a Crab pointer file changes, the diff shows:
- Pointer metadata comparison (hash, size)
- Chunk-level diff (which chunks changed)
- Option to view the full content diff (requires hydration)

## Hunk Staging Controls

In the Changes view, each hunk has interactive controls:

| Button | Action |
|--------|--------|
| Stage Hunk | Add this hunk to the index |
| Unstage Hunk | Remove from index (staged files) |
| Discard Hunk | Revert this change |
| Stage Lines | Select specific lines to stage |

These controls appear on hover over the hunk header.

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `]c` | Next hunk |
| `[c` | Previous hunk |
| `Cmd+D` | Toggle unified/split |
| `Space` | Stage/unstage hunk (Changes view) |
| `Cmd+Z` | Undo last stage/unstage |

## Performance

- Diffs are fetched on demand (not pre-loaded for all files)
- Large diffs (> 10,000 lines) show a warning with option to load
- Syntax highlighting is async (doesn't block initial render)
- Monaco editor handles scrolling for very large files
- Cancellable: switching files cancels the in-flight diff fetch
