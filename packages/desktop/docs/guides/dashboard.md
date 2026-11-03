# Dashboard View

The Dashboard is the landing page for each repository. It provides a
GitHub-style overview of repository health, activity, and key metrics
at a glance — designed to answer "what's happening in this repo?"
within seconds of opening it.

## Accessing the Dashboard

- Click the **LayoutDashboard** icon in the activity bar (first item)
- The Dashboard is shown by default when opening a repository

## Layout

The Dashboard uses a responsive card-based layout that adapts to
window width:

```
┌────────────────────────────────────────────────────────────────┐
│  Repository Name                                               │
│  crab://bucket/my-ml-project                                 │
├────────────────────────────────────────────────────────────────┤
│                                                                │
│  ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐   │
│  │  Files  │ │  Size   │ │Pointers │ │Hydration│ │This Week│   │
│  │ 12,450  │ │ 48.2 GB │ │  1,203  │ │   67%   │ │5 commits│   │
│  └─────────┘ └─────────┘ └─────────┘ └─────────┘ └─────────┘   │
│                                                                │
│  ┌─────────────────────────────────┐ ┌─────────────────────┐   │
│  │  Commit Activity (52 weeks)     │ │  Top Contributors   │   │
│  │  ░░▓▓░░▓▓▓░░░▓▓▓▓░░▓▓▓▓▓▓░░░    │ │  Alice: 234 commits │   │
│  │  ░░▓░░░▓▓░░░░▓▓▓░░░▓▓▓▓▓░░░░    │ │  Bob:   189 commits │   │
│  │  ░░░░░░▓░░░░░▓▓░░░░▓▓▓▓░░░░░    │ │  Carol: 156 commits │   │
│  └─────────────────────────────────┘ └─────────────────────┘   │
│                                                                │
│  ┌─────────────────────────────────┐ ┌─────────────────────┐   │
│  │  Commit Frequency               │ │  Branch Health      │   │
│  │  ▁▂▃▅▇█▇▅▃▂▁▂▃▅▇█▇▅▃▂▁          │ │  main: healthy      │   │
│  │  [Day] [Week] [Month]           │ │  feat/x: 12 behind  │   │
│  └─────────────────────────────────┘ └─────────────────────┘   │
│                                                                │
│  ┌─────────────────────────────────┐ ┌─────────────────────┐   │
│  │  Dedup Savings                  │ │  Release Timeline   │   │
│  │  Saved 12.4 GB (38% dedup)      │ │  v2.1.0 — 3 days    │   │
│  │  ████████████░░░░░░░░░░░░░░░░   │ │  v2.0.0 — 2 weeks   │   │
│  └─────────────────────────────────┘ └─────────────────────┘   │
│                                                                │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │  Recent Commits                                         │   │
│  │  a1b2c3d  feat: add dashboard metrics    Alice  2h ago  │   │
│  │  d4e5f6g  fix: memory leak in watcher    Bob    5h ago  │   │
│  │  h7i8j9k  chore: update dependencies     Carol  1d ago  │   │
│  └─────────────────────────────────────────────────────────┘   │
│                                                                │
└────────────────────────────────────────────────────────────────┘
```

## Metric Cards

### Key Stats Row

Five stat cards across the top provide instant orientation:

| Card | Data Source | Description |
|------|-------------|-------------|
| Files | `repo_summary` | Total file count in the repository |
| Logical Size | `repo_summary` | Sum of all file sizes |
| Pointer Files | `repo_summary` | Crab + LFS tracked files |
| Hydration | `hydration_status` | Percentage of pointers hydrated |
| This Week | `git_pulse` | Commits, authors, files changed |

### Commit Activity Heatmap

A GitHub-style contribution heatmap showing the last 52 weeks of
commit activity. Each cell represents one day, colored by commit
count (darker = more commits).

Hovering a cell shows: date, commit count, and top author for that
day.

### Commit Frequency Chart

A bar chart showing commit frequency over time with selectable
granularity:
- **Day** — daily commit counts
- **Week** — weekly aggregates (default)
- **Month** — monthly aggregates

### Top Contributors

Ranked list of contributors by commit count. Shows name and total
commits. Clicking a contributor filters the Timeline view to their
commits.

### Branch Health

Shows the status of active branches:
- How far ahead/behind each branch is relative to main
- Stale branches (no commits in 30+ days) highlighted
- Click a branch to navigate to the Branches view

### Dedup Savings

Crab-specific metric showing storage efficiency:
- Total logical size vs actual stored size
- Deduplication ratio as a percentage
- Visual progress bar

### Release Timeline

Shows recent tags/releases with:
- Tag name and date
- Time since release
- Click to view the tag in the Timeline

### Recent Commits

The last 20 commits with:
- Short SHA (clickable → Timeline)
- Commit message subject
- Author name
- Relative timestamp

## Data Loading

The Dashboard fetches multiple metrics in parallel on mount:

```typescript
// All fetches fire simultaneously
Promise.all([
  rpc.repoSummary(repoId),
  rpc.gitLog(repoId, { max: 20 }),
  rpc.gitActivity(repoId),
  rpc.gitContributors(repoId),
  rpc.listRefs(repoId),
  rpc.gitPulse(repoId, { days: 7 }),
  rpc.gitCommitFrequency(repoId, { granularity: "week" }),
  rpc.dedupSavings(repoId),
  rpc.gitBranchHealth(repoId),
  rpc.gitReleaseTimeline(repoId),
]);
```

Each metric renders independently as it arrives — the Dashboard never
shows a single loading spinner for all data.

### Stale-While-Revalidate

Expensive metrics (code frequency, contributor growth) use cached
values on subsequent visits, refreshing in the background without
showing loading states.

## Deferred Repositories

If a repository is marked as deferred (restored from localStorage but
not yet opened via the agent), the Dashboard shows a gentle
"Opening repository…" state instead of attempting RPC calls that would
fail.

## Navigation

Clicking elements in the Dashboard navigates to detailed views:
- Commit SHA → Timeline view (commit selected)
- Branch name → Branches view (branch selected)
- Contributor → Timeline view (filtered by author)
- Hydration percentage → Large Files view
