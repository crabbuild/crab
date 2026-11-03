# Data Flow Patterns

This document describes the common data flow patterns used throughout
Crab Desktop. Understanding these patterns helps developers predict
how data moves through the application and where to look when
debugging issues.

## Pattern 1: Request-Response (Synchronous RPC)

The most common pattern. The renderer makes a request and waits for
a single response.

```
Renderer                    Main                    Agent
   │                         │                       │
   │  rpc.listRefs(repoId)   │                       │
   │ ───────────────────────►│                       │
   │                         │ {"method":"list_refs"}│
   │                         │ ─────────────────────►│
   │                         │                       │ query refs
   │                         │ {"result":{refs:[...]}}
   │                         │ ◄─────────────────────│
   │  Promise<{refs: [...]}> │                       │
   │ ◄───────────────────────│                       │
```

**Used by**: stat, list_refs, git_status_porcelain, pointer_info,
read_text, preview_*, git_log, auth_status

**Characteristics**:
- Single request, single response
- Renderer awaits the promise
- Errors reject the promise
- Typically completes in < 100ms

## Pattern 2: Streaming with Progress (Long Operations)

For operations that take seconds to minutes, the agent streams
progress notifications alongside the final response.

```
Renderer                    Main                    Agent
   │                         │                       │
   │  rpc.hydrate(params)    │                       │
   │ ───────────────────────►│                       │
   │                         │  {"method":"hydrate"} │
   │                         │ ─────────────────────►│
   │                         │                       │
   │                         │  notification:        │
   │  onProgress(line)       │  progress/line        │
   │ ◄───────────────────────│ ◄─────────────────────│
   │                         │                       │
   │  onProgress(line)       │  notification:        │
   │ ◄───────────────────────│  progress/line        │
   │                         │ ◄─────────────────────│
   │                         │                       │
   │                         │ {"result":{"ok":true}}│
   │  Promise<{ok: true}>    │ ◄─────────────────────│
   │ ◄───────────────────────│                       │
```

**Used by**: clone, push, hydrate, dehydrate, add, status

**Characteristics**:
- Single request, multiple notifications, one final response
- Notifications update the ProgressDrawer and job state
- The promise resolves only when the operation completes
- Cancellable via `rpc.cancel(opId)`

## Pattern 3: Streaming List (Partial Results)

For large directory listings, the agent sends partial results as
they become available.

```
Renderer                    Main                    Agent
   │                         │                       │
   │  rpc.list(repoId,path)  │                       │
   │ ───────────────────────►│                       │
   │                         │  {"method":"list"}    │
   │                         │ ─────────────────────►│
   │                         │                       │
   │  onPartial(entries[])   │  progress/partial     │
   │ ◄───────────────────────│ ◄─────────────────────│
   │  (render first batch)   │                       │
   │                         │                       │
   │  onPartial(entries[])   │  progress/partial     │
   │ ◄───────────────────────│ ◄─────────────────────│
   │  (append to tree)       │                       │
   │                         │                       │
   │  Promise<{entries:all}> │  {"result":{...}}     │
   │ ◄───────────────────────│ ◄─────────────────────│
```

**Used by**: list (large directories), repo_summary

**Characteristics**:
- Renderer can display partial results immediately
- Final response contains the complete dataset
- Improves perceived performance for large repos

## Pattern 4: Fire-and-Forget (Notifications)

Some operations don't need a response — the renderer fires and
moves on.

```
Renderer                    Main                    Agent
   │                         │                       │
   │  rpc.cacheWarm(repoId)  │                       │
   │ ───────────────────────►│                       │
   │  (no await)             │  {"method":"cache_warm","id":null}
   │                         │ ─────────────────────►│
   │                         │                       │ (background work)
```

**Used by**: cache warming, telemetry events, prefetch hints

**Characteristics**:
- No `id` field in the JSON-RPC request (notification)
- No response expected
- Errors are logged but not surfaced to the user

## Pattern 5: Event-Driven Refresh

External changes trigger automatic UI updates without explicit user
action.

```
Filesystem                  Main                    Renderer
   │                         │                       │
   │  file modified          │                       │
   │ ───────────────────────►│                       │
   │                         │  debounce (300ms)     │
   │                         │                       │
   │                         │  IPC: "fs/changed"    │
   │                         │ ─────────────────────►│
   │                         │                       │ useFileWatcher
   │                         │                       │ triggers refresh
   │                         │                       │
   │                         │  rpc.gitStatus(...)   │
   │                         │ ◄─────────────────────│
   │                         │                       │
```

**Used by**: Changes view, Explorer, Branches, Timeline

**Characteristics**:
- Triggered by filesystem events, not user actions
- Debounced to prevent refresh storms
- Each view independently decides what to refresh

## Pattern 6: Optimistic Update with Rollback

For fast-feeling interactions, the UI updates immediately and rolls
back on failure.

```
User clicks "Stage file"
    │
    ├─ Immediately move file to "Staged" section (optimistic)
    │
    ├─ Call rpc.gitAddPaths(repoId, [path])
    │
    ├─ On success: confirm (no visual change needed)
    │
    └─ On failure: move file back to "Unstaged" + show error toast
```

**Used by**: Stage/unstage files, toggle pin, reorder repos

**Characteristics**:
- UI updates before the RPC completes
- Provides instant feedback (< 16ms)
- Rollback on error maintains consistency

## Pattern 7: Stale-While-Revalidate

For expensive metrics that don't need to be real-time, show cached
data immediately and refresh in the background.

```
View mounts
    │
    ├─ Read cached value from memory/localStorage
    ├─ Render immediately with cached data
    │
    ├─ Fire background RPC to fetch fresh data
    │
    └─ On response: update state (smooth transition, no loading spinner)
```

**Used by**: Dashboard metrics, code frequency, branch health

**Characteristics**:
- No loading state on subsequent visits
- Fresh data replaces stale data seamlessly
- Cache key includes repo ID for multi-repo support

## Pattern 8: Cancellable Operation

Long-running operations support cooperative cancellation.

```
Renderer                    Main                    Agent
   │                         │                       │
   │  rpc.hydrate(params)    │                       │
   │ ───────────────────────►│                       │
   │                         │ ─────────────────────►│
   │                         │                       │ working...
   │                         │                       │
   │  rpc.cancel(opId)       │                       │
   │ ───────────────────────►│                       │
   │                         │  cancel signal        │
   │                         │ ─────────────────────►│
   │                         │                       │ check token
   │                         │                       │
   │                         │  error: OpCancelled   │
   │  Promise rejects        │ ◄─────────────────────│
   │ ◄───────────────────────│                       │
```

**Used by**: hydrate, push, clone, git_log (large repos)

**Characteristics**:
- Cooperative: agent checks cancellation token at `.await` points
- Partial work may have been done (e.g., some chunks uploaded)
- Error code 1010 distinguishes cancellation from real failures

## Anti-Patterns

### Polling (Avoided)

The app never polls for state changes. All updates are event-driven
(filesystem watcher, agent notifications, or user actions).

### Shared Mutable State (Avoided)

The renderer and main process never share memory. All communication
is message-passing via IPC, ensuring clear ownership boundaries.

### Blocking the Main Process (Avoided)

The main process never performs synchronous I/O on the hot path.
Agent communication is fully async. File dialogs use Electron's
async APIs.
