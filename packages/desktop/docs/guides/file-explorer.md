# File Explorer View

The Explorer view is the primary file browsing interface for Crab
repositories. It displays the repository's file tree with rich
metadata including hydration state, pointer indicators, and git status
decorations.

## Accessing the Explorer

- Click the **Files** icon in the activity bar (or press the
  configured shortcut)
- The Explorer is typically the default view when opening a repository

## Features

### File Tree

The file tree renders the repository contents as a collapsible
hierarchy. It uses the `@pierre/trees` library for efficient rendering
with virtual scrolling — repositories with hundreds of thousands of
files remain responsive.

Each entry shows:
- **File/folder icon** — type-specific icons (code, image, data, etc.)
- **Name** — filename with syntax-aware coloring
- **Size** — human-readable file size
- **Git status** — modified (yellow), added (green), deleted (red),
  untracked (grey) decorations
- **Hydration indicator** — whether the file is hydrated (full
  content) or dehydrated (pointer only)

### Rev Selector

A dropdown at the top of the Explorer lets you browse any ref:
- Branches (local and remote)
- Tags (annotated tags are peeled to their commit)
- Arbitrary commit SHAs

Changing the rev re-fetches the tree listing for that point in time.

### Filtering

- **Text filter** — type to filter the tree by filename (fuzzy match)
- **Track filter** — toggle between All / Tracked / Untracked files
- **Collapse all** — collapse the entire tree to top-level entries

### File Operations

Right-click a file or folder for context menu actions:
- **Open** — open the file in the editor pane
- **Hydrate** — download the full content for a pointer file
- **Dehydrate** — replace hydrated content with a pointer
- **Copy path** — copy the relative path to clipboard
- **Reveal in Finder/Explorer** — open the containing folder
- **New file** — create a new file in the selected directory
- **New folder** — create a new folder

### Hydration Decorations

Files tracked by Crab show their hydration state:
- 🟢 **Hydrated** — full content available locally
- 🔵 **Pointer** — only the pointer blob is present (content in cloud)
- 🟡 **Partial** — some chunks cached, not fully hydrated

### Live Refresh

The Explorer watches the filesystem for changes. When files are
created, modified, or deleted outside the app (e.g., from a terminal
or another editor), the tree updates automatically.

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `↑` / `↓` | Navigate entries |
| `Enter` | Open file / expand folder |
| `←` | Collapse folder |
| `→` | Expand folder |
| `Cmd+Shift+E` | Focus Explorer view |
| `/` | Focus filter input |

## File Previews

Opening a file in the editor pane triggers format-specific rendering:

| File Type | Preview |
|-----------|---------|
| Text/Code | Syntax-highlighted editor (Monaco) |
| Parquet | Schema, sample rows and SQL |
| SafeTensors | Tensor names, shapes, dtypes |
| Jupyter (.ipynb) | Rendered notebook cells |
| Images | Inline image preview |
| SQLite | Table list, sample rows and SQL |
| ZIP archives | File listing with sizes |
| NumPy (.npy/.npz) | Array shape and dtype |

Previews are generated server-side by the Rust agent, keeping the
renderer lightweight even for multi-gigabyte files.

Choose **SQL** in the preview to query the file at the selected revision. In
Parquet SQL, `data` is the selected file. In SQLite SQL, use the database's table
names. Drafts, results and recent queries stay with the file and revision while
the repository remains open. **Cancel query** stops native work and waits for
cleanup. Results indicate when they reach the row or byte limit.

SQLite Browse and SQL include committed write-ahead log (WAL) changes. An active
working-tree database uses a read-only SQLite transaction. SQLite may maintain
its shared-memory index while the reader is open. For an index or committed
selection, keep the database and its `-wal` companion in that same selection to
include commits that have yet to be checkpointed.

Parquet and captured SQLite copies use private temporary storage. The combined
input snapshot is limited to 8 GiB. Source capture and execution share a
60-second budget.

## Drag and Drop

- Drag files between folders to move them
- Drag files to the Changes view to stage them
- Drag external files into the tree to copy them into the repo

## Integration with Other Views

- Clicking a file opens it in the **editor pane** (right side)
- Modified files show git status decorations linking to the
  **Changes view** for staging
- Pointer files show hydration state linking to the **Large Files
  view** for hydration management
- The Explorer shares the `FileTree` component with the Changes view
  for consistent tree rendering across the application
- Right-clicking a file offers "Open in Terminal" which launches a
  terminal pre-cd'd to the file's directory
- The breadcrumb bar above the editor shows the file's path and
  allows navigation back up the tree
