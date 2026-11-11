# Event System and File Watching

Crab Desktop uses a multi-layered event system to keep the UI
synchronized with filesystem changes, git state transitions, and
background operations. This document describes how events flow through
the application and how each layer participates in change detection.

## Overview

```
┌─────────────────────────────────────────────────────────────────┐
│  Filesystem                                                     │
│  (user edits, external tools, git operations)                   │
└────────────────────────────┬────────────────────────────────────┘
                             │ Metadata inventory in a worker
┌────────────────────────────▼────────────────────────────────────┐
│  Local metadata monitor (worker per physical root)              │
│  • Paths, inode, mode, size, mtime and ctime; no file payloads   │
│  • One scan at a time, followed by a 1 second idle interval     │
│  • Ignore: node_modules, .git/objects, target, __pycache__      │
│  • Pass-through: .git/index, .git/HEAD, .git/refs/*             │
└────────────────────────────┬────────────────────────────────────┘
                             │ IPC: "fs/changed" event
┌────────────────────────────▼────────────────────────────────────┐
│  Renderer Event Handlers                                        │
│  • useFileWatcher hook subscribes per-view                      │
│  • Triggers refresh callbacks (git status, file tree, refs)     │
│  • Debounced at the view level to prevent render storms         │
└─────────────────────────────────────────────────────────────────┘
```

## File Watcher Architecture

`electron/file-watcher.ts` routes IPC; `electron/local-file-watcher.ts` owns
shared subscriptions and worker lifetimes. Both IPC and `LocalProvider` use the
same monitor for a physical directory. SSH continues through its admitted
remote provider and native/polling owner.

A worker inventories paths and file metadata. It reads no file payloads and
keeps a fixed-size, order-independent fingerprint rather than a file-sized
cache. It skips artifact subtrees, reports an initial invalidation, then emits
`fs/changed` when a completed inventory differs. Only one scan runs at a time;
the next begins after a one-second idle interval. Notifications remain advisory:
file reads use their captured native source and integrity/version checks.

The prior local paths duplicated `fs.watch` and synchronously closed its handles
on Electron's main thread. On the qualification host this blocked repository
switches in libuv's FSEvents deinitialization semaphore, while native directory
notifications were also absent in direct dependency probes. The single metadata
path avoids that dependency. See the upstream
[libuv close implementation](https://github.com/libuv/libuv/blob/v1.46.0/src/unix/fsevents.c#L811).

Stopping a subscription removes its callback immediately. The last subscriber
requests worker shutdown and waits for the current metadata scan to release
its directory handles. A new subscriber cannot join an owner already draining;
application shutdown joins active and draining workers together. There is no
forced worker termination, polling fallback mode or backend-selection setting.
Local monitoring remains independent of native agent construction/readiness.

### Ignored Paths

The watcher skips directories that produce high-frequency noise
without affecting git status:

| Directory | Reason |
|-----------|--------|
| `node_modules/` | Package installs produce thousands of events |
| `.git/objects/` | Object writes are internal to git |
| `.git/logs/` | Reflog writes don't affect working tree |
| `.crab/staging/` | Staging area managed by the agent |
| `.crab/cache/` | Cache writes are internal |
| `target/` | Rust build artifacts |
| `__pycache__/` | Python bytecode cache |
| `.venv/` | Python virtual environments |

### Interesting Git Files

Certain files inside `.git/` signal meaningful state changes that
views need to react to:

| File | Signals |
|------|---------|
| `index` | Staging area changed (external `git add`) |
| `HEAD` | Branch switch or commit |
| `MERGE_HEAD` | Merge in progress |
| `REBASE_HEAD` | Rebase in progress |
| `CHERRY_PICK_HEAD` | Cherry-pick in progress |
| `refs/heads/*` | Branch created/deleted/updated |
| `refs/tags/*` | Tag created/deleted |

### Coalescing and Bounds

A snapshot folds each eligible path's identity and timestamps into a fixed-size
fingerprint; it does not retain an entry map or read large-file contents. Changes
during a scan are rechecked by subsequent scans, with no overlapping inventories
or retry queue. Subscriber callbacks are also debounced in the renderer. Very
large file-count and network-filesystem scan costs still require profile-specific
qualification; file payload size does not determine scan allocation.

## Renderer-Side Event Handling

### useFileWatcher Hook

Views subscribe to filesystem changes via the `useFileWatcher` hook:

```typescript
// The repository owner starts/stops one subscription.
useFileWatcherRoot(getWatchPath(repo));
// Views subscribe without managing worker lifetimes.
useFileWatcher(getWatchPath(repo), reload);
```

### View Refresh Patterns

Each view handles filesystem events differently:

| View | Refresh Action |
|------|---------------|
| Explorer | Re-fetch directory listing |
| Changes | Re-run `git status --porcelain` |
| Branches | Re-fetch refs list |
| Timeline | Re-fetch recent commits |
| Worktrees | Re-fetch worktree status |
| Dashboard | Re-fetch summary metrics |

## Agent Notifications

The Rust agent also emits events for operations it initiates:

### Progress Notifications

```json
{ "jsonrpc": "2.0", "method": "progress/line", "params": { "op": "hydrate", "line": "..." } }
{ "jsonrpc": "2.0", "method": "progress/partial", "params": { "entries": [...] } }
{ "jsonrpc": "2.0", "method": "progress/complete", "params": { "op": "push", "ok": true } }
```

### Watcher Notifications

The agent's internal filesystem watcher (using the `notify` crate)
emits change events for the background indexer:

```json
{ "jsonrpc": "2.0", "method": "watcher/changed", "params": { "paths": ["src/main.rs"] } }
```

## Custom Events (Renderer)

The renderer uses DOM custom events for cross-component communication
that doesn't warrant global state:

| Event | Purpose |
|-------|---------|
| `crab:active-view-changed` | Notify when the active view changes |
| `crab:open-command-palette` | Programmatically open the palette |
| `crab:repo-opened` | A repo was opened via command |
| `crab:toast` | Display a toast notification |

## Orchestrator Events

The orchestrator emits task lifecycle events:

| Event | Payload |
|-------|---------|
| `task-status` | `{ taskId, status, ... }` |
| `task-event` | `{ taskId, event: AgentEvent }` |
| `task-checkpoint` | `{ taskId, turnNumber, sha }` |

These are forwarded to the renderer via IPC and consumed by the
AgentTasksView to update the task list and chat tab in real time.

## Error Handling

Event handlers are wrapped in try-catch to prevent a single failed
refresh from breaking the event pipeline. Errors are logged via
`tracing` (agent) or `console.warn` (renderer) but never propagated
to crash the application.
