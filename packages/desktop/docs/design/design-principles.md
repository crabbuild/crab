# Design Principles

Crab Desktop is designed around a set of core principles that guide
both UX decisions and engineering trade-offs.

## UX Principles

### 1. Git-Native, Not Git-Replacement

The app enhances git workflows rather than hiding them. Users see
branches, commits, diffs, and refs — the same mental model they use
on the command line. Crab-specific features (large files, hydration,
xorbs) are surfaced as natural extensions of the git workflow.

### 2. Progressive Disclosure

Simple operations (commit, push, pull) are one-click. Advanced
operations (interactive rebase, hunk staging, worktree management)
are accessible but don't clutter the primary interface.

The activity bar organizes views by frequency of use:
- Dashboard, Explorer, Changes — daily use
- Branches, Timeline — frequent use
- Worktrees, Workflows, Orchestrator — power-user features

### 3. Non-Blocking UI

Every operation that might take more than 100ms is async with visible
progress. The user can continue browsing files while a push runs in
the background. Multiple operations can run concurrently.

### 4. Instant Feedback

- File tree renders immediately with cached data, then refreshes
- Terminal spawns from a pre-warmed pool (no shell startup delay)
- Deferred repos show cached metadata before the agent opens them
- Virtual scrolling keeps frame rate stable with 100k+ entries

### 5. Keyboard-First

Every action is accessible via keyboard:
- `Cmd+Shift+P` — Command palette (200+ commands)
- `Cmd+K` — Quick open (fuzzy file search)
- `Cmd+Enter` — Commit (when in Changes view)
- `Cmd+Shift+G` — Focus Changes view
- View-specific shortcuts documented in each view's toolbar

## Engineering Principles

### 1. Crash Isolation

The three-tier architecture ensures that:
- A renderer crash shows an error boundary, not a blank window
- An agent crash triggers auto-restart (max 1 per 30s)
- A terminal crash doesn't affect other terminals or the main UI

### 2. Correctness Over Performance

- Hunk staging validates anchors before applying patches
- Push operations verify byte-identical reconstruction
- Worktree operations check for dirty state before destructive actions
- The agent never returns partial/incorrect data — it errors instead

### 3. Minimal Global State

Only truly global concerns live in the store:
- Open repos and active selection
- Job progress (cross-view visibility)
- Log ring buffer

Everything else is view-local state, reducing coupling and preventing
cascade re-renders.

### 4. Type Safety End-to-End

- Renderer: TypeScript with strict mode
- IPC: Typed wrapper (`ipc.ts`) with 200+ method signatures
- Agent: Rust with `#![deny(clippy::unwrap_used)]`
- Preload: Explicit whitelist — no dynamic method dispatch

### 5. Graceful Degradation

- Forge features hide when no provider is detected
- Experiment tracking silently skips when unconfigured
- Preview falls back to hex dump for unknown formats
- Offline mode shows cached data with staleness indicators

## Visual Design

### Color System

The app uses a semantic color palette via CSS custom properties:

| Token | Purpose |
|-------|---------|
| `--gx-bg` | Background |
| `--gx-surface` | Elevated surfaces (cards, panels) |
| `--gx-border` | Borders and dividers |
| `--gx-text` | Primary text |
| `--gx-muted` | Secondary/disabled text |
| `--gx-accent` | Interactive elements |
| `--gx-success` | Added/positive states |
| `--gx-warn` | Modified/warning states |
| `--gx-error` | Deleted/error states |
| `--gx-info` | Informational highlights |

Themes (dark, light, high-contrast dark, high-contrast light) swap
these tokens. Workspace-level theme overrides are supported.

### Layout

```
┌──────────────────────────────────────────────────────────┐
│  TopBar (repo name, sync status, branch indicator)       │
├────┬─────────────────────────────────────────────────────┤
│    │                                                     │
│ A  │  Main Content Area                                  │
│ c  │  (view-dependent: tree, diff, terminal, etc.)       │
│ t  │                                                     │
│ i  │                                                     │
│ v  │                                                     │
│ i  │                                                     │
│ t  │                                                     │
│ y  │                                                     │
│    │                                                     │
│ B  │                                                     │
│ a  │                                                     │
│ r  │                                                     │
│    │                                                     │
├────┴─────────────────────────────────────────────────────┤
│  StatusBar (agent health, job count, branch, sync)       │
└──────────────────────────────────────────────────────────┘
```

The activity bar is 48px wide with icon-only buttons. Hovering shows
a tooltip with the view name. The active view is indicated by a text
color change and a left border accent.

### Resizable Panels

Views that show a list + detail (Changes, Branches, Worktrees) use
`react-resizable-panels` for user-adjustable split ratios. Panel
sizes persist across sessions.
