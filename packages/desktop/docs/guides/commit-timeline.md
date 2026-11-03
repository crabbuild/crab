# Commit Timeline View

The Commit Timeline view displays the repository's commit history as
a scrollable graph with branch lanes, merge points, and rich metadata
including CI status and experiment tracking.

## Accessing the Commit Timeline

- Click the **GitGraph** icon in the activity bar
- Also accessible from the Dashboard by clicking a commit

## Layout

```
┌─────────────────────────────────────────────────────────────────┐
│  Branch: [main ▾]  [Refresh]  Search: ________                  │
├─────────────────────────────────────────────────────────────────┤
│  Graph │ SHA     │ Message              │ Author  │ Date        │
│────────┼─────────┼──────────────────────┼─────────┼─────────────│
│  ●     │ a1b2c3d │ feat: add dashboard  │ Alice   │ 2 hours ago │
│  │     │         │                      │         │             │
│  ●─┐   │ d4e5f6g │ Merge branch 'fix'   │ Bob     │ 5 hours ago │
│  │ │   │         │                      │         │             │
│  │ ●   │ h7i8j9k │ fix: memory leak     │ Bob     │ 6 hours ago │
│  │ │   │         │                      │         │             │
│  ●─┘   │ l0m1n2o │ refactor: cleanup    │ Alice   │ 1 day ago   │
│  │     │         │                      │         │             │
│  ●     │ p3q4r5s │ chore: update deps   │ Carol   │ 2 days ago  │
└─────────────────────────────────────────────────────────────────┘
```

## Features

### Commit Graph

The left column renders an SVG commit graph showing:
- **Commit nodes** — circles on branch lanes
- **Branch lanes** — colored vertical lines for each branch
- **Merge lines** — diagonal connections showing merge points
- **Trunk detection** — the main branch is always the leftmost lane

The graph uses a topological sort algorithm to assign lanes and
minimize crossings. Lane colors are deterministic based on branch name.

### Virtual Scrolling

The commit log uses `@tanstack/react-virtual` with a fixed row height
(28px) for smooth scrolling through thousands of commits. Only visible
rows are rendered — scrolling through 100k commits stays at 60fps.

### Commit Details

Click a commit to see:
- Full commit message (subject + body)
- Author and committer with timestamps
- Parent commit(s)
- Changed files with diff stats (+/- lines)
- Associated tags and branches

### Branch Filter

The branch dropdown at the top filters the log:
- **All branches** — shows the full graph
- **Specific branch** — shows only commits reachable from that branch
- **HEAD** — shows the current branch's history

### Search

Search commits by:
- Message text (fuzzy match)
- Author name
- SHA prefix
- File path (commits that touched a specific file)

### CI Status

When a forge provider is configured, commits show CI status chips:
- 🟢 Passed
- 🔴 Failed
- 🟡 Pending
- Click to view check details

### Experiment Tracking

For ML repositories with experiment tracking configured:
- Commits show associated experiment runs
- Run status (running, completed, failed, killed)
- Click to view experiment metrics
- Color-coded by status

### Commit Actions

Right-click a commit for:
- **Cherry-pick** — apply this commit to the current branch
- **Revert** — create a revert commit
- **Reset to here** — move HEAD to this commit (soft/mixed/hard)
- **Create branch here** — start a new branch from this commit
- **Create tag** — tag this commit
- **Copy SHA** — copy the full commit hash
- **Browse files at this commit** — open Explorer at this revision

### Commit Popover

Hovering over a commit shows a popover with:
- Full message preview
- File change summary
- Quick action buttons (cherry-pick, revert, copy SHA)

## Navigation

### Keyboard

| Shortcut | Action |
|----------|--------|
| `↑` / `↓` | Navigate commits |
| `Enter` | Open commit detail |
| `Cmd+C` | Copy selected commit SHA |
| `/` | Focus search |
| `Escape` | Clear selection |

### Integration

- Clicking a commit SHA in other views navigates to the Timeline
- The Dashboard's "Recent Commits" section links here
- Branch operations (merge, rebase) update the Timeline on completion

## Performance

The Timeline is optimized for large histories:
- Commits are loaded in pages (default 500 per page)
- Scrolling near the bottom triggers the next page load
- Graph layout is computed incrementally as pages load
- Cancellable fetch — switching branches cancels the in-flight load
