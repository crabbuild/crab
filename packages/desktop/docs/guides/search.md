# Search View

The Search view provides VS Code-style full-text search across the
repository with regex support, search-and-replace, file filtering,
and result grouping.

## Accessing the Search View

- Click the **Search** icon in the activity bar
- Keyboard shortcut: `Cmd+Shift+F`

## Features

### Search Input

The search bar supports three toggle modes:

| Toggle | Icon | Description |
|--------|------|-------------|
| Case Sensitive | `Aa` | Match exact case |
| Whole Word | `Ab|` | Match word boundaries only |
| Regex | `.*` | Interpret query as a regular expression |

### Search Execution

Search is powered by `git grep` via the agent's `git_grep` RPC verb:
- Searches the working tree (includes uncommitted changes)
- Respects `.gitignore` (ignored files are excluded)
- Supports regex patterns when the regex toggle is active
- Results are cancellable (switching queries cancels the previous search)

### Result Display

Results are grouped by file with collapsible sections:

```
┌─────────────────────────────────────────────────────────────────┐
│  🔍 "handleRequest"  [Aa] [Ab|] [.*]                            │
│  Files to include: ________  Files to exclude: ________         │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ▾ src/handlers.rs (4 matches)                                  │
│    12: pub async fn handleRequest(state: &State) -> Result<..>  │
│    45:     // handleRequest delegates to typed handlers         │
│    89: fn handleRequest_inner(req: Request) -> Result<Value> {  │
│   134:     handleRequest(state, req, out).await                 │
│                                                                 │
│  ▾ src/main.rs (2 matches)                                      │
│    67:     let result = handleRequest(&state, request).await;   │
│    92: // Each handleRequest runs in its own task               │
│                                                                 │
│  ─── 6 results in 2 files ───                                   │
└─────────────────────────────────────────────────────────────────┘
```

### View Modes

- **List view** — flat list of all matches
- **Tree view** — matches grouped by directory hierarchy

### File Filters

Narrow search scope with glob patterns:
- **Files to include**: e.g., `*.rs`, `src/**/*.ts`
- **Files to exclude**: e.g., `*.test.ts`, `node_modules/**`

Multiple patterns are comma-separated.

### Search and Replace

Expand the replace section to perform bulk replacements:

1. Enter the replacement text
2. Preview replacements inline (strikethrough old, highlight new)
3. Replace options:
   - **Replace** — replace one occurrence
   - **Replace All in File** — replace all in a specific file
   - **Replace All** — replace across all files

Replace uses the agent's `search_replace` RPC verb which performs
atomic file writes with proper encoding handling.

### Result Navigation

- Click a match to open the file at that line in the editor pane
- Arrow keys navigate between matches
- `F4` / `Shift+F4` cycle through matches

### Virtual Scrolling

For searches with thousands of matches, results are virtualized:
- Files with fewer than 50 matches render directly
- Files with 50+ matches use `@tanstack/react-virtual`
- Smooth scrolling regardless of result count

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Cmd+Shift+F` | Focus Search view |
| `Enter` | Search / next match |
| `Cmd+Shift+H` | Toggle replace mode |
| `Cmd+Alt+Enter` | Replace all |
| `F4` | Next match |
| `Shift+F4` | Previous match |
| `Escape` | Clear search |

## Performance

- Search is debounced (300ms after last keystroke)
- Previous search is cancelled when a new query starts
- Results stream incrementally for large repos
- File groups are collapsible to reduce DOM size
