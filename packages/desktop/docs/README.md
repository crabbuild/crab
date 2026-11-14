# Crab Desktop Documentation

Welcome to the Crab Desktop documentation. This guide covers the
architecture, design decisions, and user-facing features of the desktop
client for Crab — serverless git for large files.

## Documentation Structure

```
docs/
├── architecture/
│   ├── overview.md              Three-tier architecture
│   ├── ipc-protocol.md          JSON-RPC communication
│   ├── state-management.md      Renderer state & persistence
│   ├── agent-sidecar.md         Rust agent internals
│   ├── event-system.md          File watching & event flow
│   ├── orchestrator-system.md   Agent task orchestration
│   ├── agent-teams.md           Sub-agent transcripts and mixed provider teams
│   ├── lsp-integration.md       Language server protocol
│   ├── terminal-subsystem.md    PTY management & shell integration
│   └── build-and-packaging.md   Build pipeline & distribution
├── design/
│   ├── design-principles.md     UX & engineering principles
│   ├── security-model.md        Sandboxing, IPC whitelist, credentials
│   ├── performance.md           Lazy loading, streaming, virtual scroll
│   ├── error-handling.md        Error strategy across all tiers
│   ├── theming-and-accessibility.md  Themes, WCAG, keyboard nav
│   └── data-flow-patterns.md    Common data flow patterns
├── pty-daemon.md                PTY daemon: per-session model, env vars, quotas, diagnostics
└── guides/
    ├── file-explorer.md         Explorer view
    ├── orchestrator.md          Agent tasks / orchestrator view
    ├── changes.md               Changes & staging view
    ├── large-files.md           Large files view
    ├── branches.md              Branches view
    ├── worktrees.md             Worktrees view
    ├── commit-timeline.md       Commit timeline view
    ├── workflows.md             Workflow editor view
    ├── terminal.md              Terminal view
    ├── settings.md              Settings view
    ├── dashboard.md             Dashboard / overview view
    ├── search.md                Full-text search & replace
    ├── conflicts.md             Merge conflict resolution
    ├── stashes.md               Git stash management
    ├── pull-requests.md         PR integration (GitHub/GitLab)
    ├── health-diagnostics.md    System health & troubleshooting
    ├── command-palette.md       Command palette usage
    ├── diff-viewer.md           Diff rendering & hunk controls
    ├── cloning-repositories.md  Clone workflow & authentication
    └── keyboard-shortcuts.md    Complete shortcut reference
```

## Quick Start

```bash
cd crab-desktop
npm install
npm run build:agent   # cargo build --release -p crab-desktop-agent
npm run dev           # vite + electron with hot reload
```

## Architecture at a Glance

```
┌─────────────────────────────────────────────────────────┐
│  Renderer (React 18 + Tailwind + Vite + TypeScript)     │
│  Views: Explorer, Changes, Branches, Worktrees,         │
│         Timeline, Terminal, Orchestrator, Workflows, ...│
└────────────────────────┬────────────────────────────────┘
                         │ contextBridge IPC
┌────────────────────────▼────────────────────────────────┐
│  Main Process (Electron 45, Node 24)                    │
│  Agent supervisor, Orchestrator, Terminal pool, LSP     │
└────────────────────────┬────────────────────────────────┘
                         │ NDJSON-RPC over stdio
┌────────────────────────▼────────────────────────────────┐
│  crab-desktop-agent (Rust, tokio)                     │
│  100+ RPC verbs, crab-sdk reads, CLI shell-outs       │
└─────────────────────────────────────────────────────────┘
```
