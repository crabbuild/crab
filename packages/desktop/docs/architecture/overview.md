# Architecture Overview

Crab Desktop is a cross-platform desktop application for managing
Crab repositories backed by cloud object storage (S3, GCS, Azure).
It follows a three-tier architecture that separates concerns between
the UI, process management, and data operations.

## Three-Tier Architecture

```
┌────────────────────────────────────────────────────────────────┐
│                     RENDERER (Sandboxed)                       │
│  React 18 · Tailwind CSS · Vite · TypeScript                   │
│                                                                │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────────────┐   │
│  │ Explorer │ │ Changes  │ │ Timeline │ │ Orchestrator     │   │
│  │ View     │ │ View     │ │ View     │ │ (Agent Tasks)    │   │
│  └──────────┘ └──────────┘ └──────────┘ └──────────────────┘   │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────────────┐   │
│  │ Branches │ │ Worktrees│ │ Terminal │ │ Workflows        │   │
│  └──────────┘ └──────────┘ └──────────┘ └──────────────────┘   │
│                                                                │
│  State: useReducer + Context · Persistence: localStorage       │
└────────────────────────────┬───────────────────────────────────┘
                             │ contextBridge (preload.ts)
                             │ IPC whitelist: 200+ methods
┌────────────────────────────▼───────────────────────────────────┐
│                     MAIN PROCESS (Privileged)                  │
│  Electron 45 · Node 24                                         │
│                                                                │
│  ┌─────────────┐ ┌──────────────┐ ┌────────────────────────┐   │
│  │ Agent       │ │ Orchestrator │ │ Terminal Pool          │   │
│  │ Supervisor  │ │ (Tasks)      │ │ (node-pty, pre-warmed) │   │
│  └─────────────┘ └──────────────┘ └────────────────────────┘   │
│  ┌─────────────┐ ┌──────────────┐ ┌────────────────────────┐   │
│  │ Sidecar Mgr │ │ LSP Manager  │ │ File Watcher           │   │
│  └─────────────┘ └──────────────┘ └────────────────────────┘   │
│  ┌─────────────┐ ┌──────────────┐                              │
│  │ Keychain    │ │ Forge Auth   │                              │
│  └─────────────┘ └──────────────┘                              │
└────────────────────────────┬───────────────────────────────────┘
                             │ NDJSON over stdio (JSON-RPC 2.0)
┌────────────────────────────▼───────────────────────────────────┐
│                     AGENT SIDECAR (Rust)                       │
│  tokio async runtime · crab-sdk · crab CLI                 │
│                                                                │
│  ┌─────────────┐ ┌──────────────┐ ┌────────────────────────┐   │
│  │ Read Verbs  │ │ Write Verbs  │ │ Git Operations         │   │
│  │ (SDK)       │ │ (CLI shell)  │ │ (libgit2 / shell)      │   │
│  └─────────────┘ └──────────────┘ └────────────────────────┘   │
│  ┌─────────────┐ ┌──────────────┐ ┌────────────────────────┐   │
│  │ Previews    │ │ Forge APIs   │ │ Workflow Engine        │   │
│  │ (Parquet,   │ │ (GitHub,     │ │ (YAML DAG, experiment  │   │
│  │  SafeTensor)│ │  GitLab)     │ │  tracking)             │   │
│  └─────────────┘ └──────────────┘ └────────────────────────┘   │
└────────────────────────────────────────────────────────────────┘
```

## Layer Responsibilities

### Renderer (React)

The renderer is sandboxed with `contextIsolation: true` and
`nodeIntegration: false`. It has no direct access to the filesystem,
network, or native APIs. All operations go through the IPC bridge.

Key responsibilities:
- UI rendering (30+ views, 80+ components)
- Local state management (repos, jobs, logs)
- User interaction handling (keyboard shortcuts, drag-and-drop)
- Theme management (dark/light/high-contrast)
- Virtual scrolling for large datasets (file trees, commit logs)

### Main Process (Electron)

The main process is the privileged coordinator. It owns:
- **Agent supervisor** — spawns the Rust sidecar, handles crashes with
  debounce (max 1 auto-restart per 30s window), writes crash dumps
- **Orchestrator** — manages agent task lifecycle (create → run →
  review → apply/discard), worktree isolation, checkpoints
- **Terminal pool** — pre-warms 2 PTY shells for instant spawn,
  manages split panes and shell environment detection
- **LSP manager** — TypeScript language server for code intelligence
- **File watcher** — `chokidar`-based watcher for live refresh
- **Keychain** — OS credential storage for forge tokens
- **Settings** — reads/writes `main-settings.json` for agent config

### Agent Sidecar (Rust)

A single-binary async server (`crab-desktop-agent`) that processes
RPC requests concurrently via tokio tasks. Design choices:

- **Read verbs** (list, stat, read_text, pointer_info) call directly
  into `crab-sdk` for zero-overhead access to the object store
- **Write verbs** (clone, push, hydrate, dehydrate) shell out to the
  installed `crab` CLI so the desktop app inherits identical
  behavior to the command line
- **Git verbs** (commit, branch, merge, rebase) use direct git
  commands for full compatibility
- **Streaming** — long-running operations emit progress notifications
  as JSON-RPC notifications (no id field)
- **Cancellation** — cooperative via `tokio_util::CancellationToken`

## Process Lifecycle

```
App Launch
    │
    ├─ Main process starts
    │   ├─ Reads main-settings.json
    │   ├─ Spawns agent sidecar (with CRAB_DESKTOP_LOG env)
    │   ├─ Pre-warms terminal pool (2 shells)
    │   ├─ Restores persisted orchestrator tasks
    │   └─ Creates BrowserWindow
    │
    ├─ Renderer mounts
    │   ├─ Restores repos from localStorage
    │   ├─ Opens deferred repos lazily (no RPC until selected)
    │   └─ Registers command palette shortcuts
    │
    └─ User interaction loop
        ├─ View navigation via ActivityBar
        ├─ RPC calls: Renderer → Preload → Main → Agent
        └─ Notifications: Agent → Main → Renderer (event emitter)

App Shutdown
    │
    ├─ Renderer persists state to localStorage
    ├─ Main process stops file watchers
    ├─ Orchestrator persists task metadata
    ├─ Agent receives stdin EOF → graceful shutdown
    └─ Terminal pool kills all PTY processes
```

## Technology Stack

| Layer | Technology | Purpose |
|-------|-----------|---------|
| Renderer | React 18 | UI framework |
| Renderer | Tailwind CSS 3 | Styling |
| Renderer | Vite 5 | Build tool + HMR |
| Renderer | TypeScript 5.9 | Type safety |
| Renderer | @tanstack/react-virtual | Virtual scrolling |
| Renderer | @xyflow/react | Workflow DAG editor |
| Renderer | @xterm/xterm | Terminal emulator |
| Renderer | Monaco Editor | Code editing + diffs |
| Renderer | @pierre/trees | File tree rendering |
| Main | Electron 45 / Node 24 | Desktop shell |
| Main | node-pty | Terminal PTY |
| Main | better-sqlite3 | Local persistence |
| Main | zeromq | Inter-process messaging |
| Agent | Rust 2024 | Systems language |
| Agent | tokio | Async runtime |
| Agent | crab-sdk | Object store access |
| Agent | parquet/arrow | Data file previews |
| Agent | safetensors | ML model previews |
| Agent | slatedb | Background indexer cache |

## Directory Layout

```
crab-desktop/
├── agent/                 Rust sidecar binary
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs        Entry point, RPC dispatch loop
│       ├── rpc.rs         JSON-RPC 2.0 framing
│       ├── handlers.rs    100+ verb implementations
│       ├── shellout.rs    Streaming CLI wrapper
│       ├── git_branches.rs  Branch operations
│       ├── git_commit.rs    Commit logic
│       ├── git_hunks.rs     Hunk staging/unstaging
│       ├── git_worktree.rs  Worktree management
│       ├── git_remote.rs    Remote operations
│       ├── forge/           GitHub/GitLab/Bitbucket APIs
│       ├── workflow.rs      YAML workflow engine
│       ├── preview.rs       File format previews
│       ├── indexer.rs       Background file indexer
│       └── watcher.rs       Filesystem change detection
├── electron/              Main process
│   ├── main.ts            App entry, window creation
│   ├── preload.ts         Context bridge (IPC whitelist)
│   ├── agent.ts           Sidecar supervisor
│   ├── orchestrator.ts    Task lifecycle manager
│   ├── orchestrator-adapters.ts  Agent type adapters
│   ├── orchestrator-worktree.ts  Git worktree isolation
│   ├── orchestrator-checkpoints.ts  Checkpoint/rollback
│   ├── lsp-manager.ts     Language server management
│   ├── file-watcher.ts    Live filesystem monitoring
│   └── keychain.ts        OS credential storage
├── src/                   Renderer
│   ├── main.tsx           React entry + error boundary
│   ├── App.tsx            Shell layout + routing
│   ├── state.tsx          Global state (useReducer + Context)
│   ├── settings.ts        User preferences
│   ├── ipc.ts             Typed RPC wrapper (200+ methods)
│   ├── views/             30+ view components
│   ├── components/        80+ reusable UI components
│   ├── hooks/             Custom React hooks
│   ├── lib/               Utilities, parsers, registries
│   └── pages/             Legacy page components
├── tests/                 Playwright + Vitest tests
├── scripts/               Build & packaging scripts
├── package.json           Dependencies & scripts
├── electron-builder.yml   Packaging configuration
└── vite.config.ts         Vite build configuration
```
