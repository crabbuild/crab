# State Management

Crab Desktop uses a lightweight state management approach built on
React's `useReducer` + Context, avoiding external state libraries.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│  StoreProvider (Context)                            │
│                                                     │
│  ┌───────────────────────────────────────────────┐  │
│  │  AppState (useReducer)                        │  │
│  │                                               │  │
│  │  repos: OpenRepo[]        ← open repositories │  │
│  │  activeRepoId: string     ← selected repo     │  │
│  │  logs: ProgressLine[]     ← ring buffer       │  │
│  │  jobs: Record<id, Job>    ← running ops       │  │
│  │  pinnedPaths: Set<string> ← pinned repos      │  │
│  │  repoOrder: string[]      ← sidebar ordering  │  │
│  └───────────────────────────────────────────────┘  │
│                                                     │
│  Actions: addRepo, removeRepo, setActive,           │
│           appendLog, updateJob, pinRepo, reorder    │
└─────────────────────────────────────────────────────┘
```

## State Shape

### AppState

```typescript
interface AppState {
  repos: OpenRepo[];           // Currently open repositories
  activeRepoId: string | null; // Which repo is selected
  logs: ProgressLine[];        // Ring buffer of progress lines
  jobs: Record<string, JobState>; // Active operations
  pinnedPaths: Set<string>;    // Pinned repo paths
  repoOrder: string[];         // Custom sidebar ordering
}
```

### OpenRepo

```typescript
interface OpenRepo extends RepoHandle {
  label: string;        // Display name
  path?: string;        // Local filesystem path
  openedAt: number;     // Timestamp
  deferred?: boolean;   // Restored but not yet opened via RPC
  initialView?: string; // View to show on first render
}
```

### JobState

```typescript
interface JobState {
  op: string;                          // Operation name
  status: "running" | "done" | "failed";
  exitCode?: number;
  lines: number;                       // Progress line count
  startedAt: number;
  updatedAt: number;
}
```

## Persistence Strategy

State is persisted to `localStorage` for session restoration:

| Key | Content | Purpose |
|-----|---------|---------|
| `crab.recentRepos.v1` | `{path, label, openedAt}[]` | Reopen repos on startup |
| `crab.activeRepoPath.v1` | `string` | Restore active selection |
| `crab.pinnedRepoPaths.v1` | `string[]` | Pinned repos |
| `crab.repoOrder.v1` | `string[]` | Custom sidebar order |
| `crab.repoSidebarOpen` | `boolean` | Sidebar collapsed state |

Main-process settings that affect the agent (log level, devtools) are
stored in `~/.config/crab-desktop/main-settings.json` because the
main process cannot access `localStorage`.

## Data Flow

### Opening a Repository

```
User clicks "Open" → file dialog
    │
    ▼
rpc.openLocal(path)
    │
    ▼
Agent returns RepoHandle { repo_id, url, is_remote, cache_dir }
    │
    ▼
dispatch({ type: "ADD_REPO", payload: { ...handle, path, label } })
    │
    ▼
State updates → localStorage persists → UI re-renders
```

### Progress Notifications

```
Agent emits progress/line notification
    │
    ▼
Main process forwards via event emitter
    │
    ▼
onAgentEvent("progress/line", handler) in renderer
    │
    ▼
dispatch({ type: "APPEND_LOG", payload: line })
dispatch({ type: "UPDATE_JOB", payload: { id, status, lines } })
    │
    ▼
ProgressDrawer + StatusBar update
```

## View-Local State

Most view-specific state lives in component-local `useState` hooks
rather than the global store. This keeps the global state minimal and
avoids unnecessary re-renders across unrelated views.

Examples:
- File tree expansion state → `ExplorerView` local state
- Selected branch → `BranchesView` local state
- Terminal sessions → `TerminalView` local state + registry
- Diff hunks → `ChangesView` local state

## Settings

User preferences are managed by a separate `SettingsProvider`:

```typescript
interface Settings {
  theme: "dark" | "light" | "system";
  editorFontSize: number;
  maxLogLines: number;
  reopenReposOnStartup: boolean;
  diffStyle: "unified" | "split";
  commitTemplate: string;
  // ... 30+ settings
}
```

Settings persist to `localStorage` and are accessed via the
`useSetting(key)` hook. The subset that affects the main process
(agent log level, devtools) is mirrored to `main-settings.json`.

## Memory Management

- **Log ring buffer**: Capped at `maxLogLines` (default 5000) to
  prevent unbounded growth during long operations
- **Heap watcher**: Monitors `performance.memory` and evicts cached
  data when heap usage exceeds thresholds
- **Deferred repos**: Repos restored from localStorage are marked
  `deferred: true` — no RPC calls until the user selects them
