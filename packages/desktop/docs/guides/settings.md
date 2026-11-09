# Settings View

The Settings view provides centralized configuration for all aspects
of Crab Desktop — appearance, editor behavior, git preferences,
integrations, and system diagnostics.

## Accessing Settings

- Click the **Settings** (gear) icon in the activity bar
- Keyboard shortcut: `Cmd+,`

## Layout

```
┌──────────────────┬──────────────────────────────────────────────┐
│  Sections        │  Section Content                             │
│                  │                                              │
│  • Appearance    │  ┌────────────────────────────────────────┐  │
│  • Editor        │  │  Theme                                 │  │
│  • Previews      │  │  [Dark ▾]                              │  │
│  • Workspace     │  │                                        │  │
│  • Diff          │  │  Editor Theme                          │  │
│  • Commit        │  │  [One Dark Pro ▾]                      │  │
│  • Notifications │  │                                        │  │
│  • Prefetch      │  │  Font Size                             │  │
│  • Experiments   │  │  [14] px                               │  │
│  • Integrations  │  │                                        │  │
│  • Storage       │  │  Icon Theme                            │  │
│  • Cache         │  │  [Material Icons ▾]                    │  │
│  • Memory        │  └────────────────────────────────────────┘  │
│  • Orchestrator  │                                              │
│  • Shortcuts     │                                              │
│  • Reset         │                                              │
│  • About         │                                              │
└──────────────────┴──────────────────────────────────────────────┘
```

## Sections

### Appearance

- **Theme** — Dark, Light, High Contrast Dark, High Contrast Light,
  System (follows OS preference)
- **Editor theme** — syntax highlighting theme (One Dark Pro,
  Solarized, Monokai, Nord, etc.)
- **Font size** — global UI font size
- **Workspace theme** — per-repo theme override (useful for
  distinguishing multiple open repos)

### Editor

- **Font family** — editor and diff font
- **Font size** — editor-specific font size
- **Tab size** — spaces per tab
- **Word wrap** — on/off/bounded
- **Minimap** — show/hide code minimap
- **Line numbers** — on/off/relative
- **Bracket matching** — highlight matching brackets

### Previews

- **Max preview size** — skip previews for files above this size
- **Parquet row limit** — max rows to show in Parquet preview
- **Image preview** — enable/disable inline image rendering
- **Notebook rendering** — how to display Jupyter notebooks

### Workspace

- **Reopen repos on startup** — restore previously open repos
- **Default clone directory** — where `git clone` puts repos
- **File watcher** — enable/disable live filesystem monitoring
- **Ignored patterns** — glob patterns to exclude from the tree

### Diff

- **Diff style** — Unified or Split (side-by-side)
- **Context lines** — lines of context around changes (default 3)
- **Whitespace** — show/ignore whitespace changes
- **Word diff** — highlight word-level changes within lines
- **Inline blame** — show git blame annotations in diffs

### Commit

- **Commit template** — default commit message template
- **Conventional commits** — enable type/scope prefix UI
- **Sign commits** — GPG/SSH signing (auto-detect key)
- **Auto-stage on commit** — stage all modified files before commit
- **Spell check** — check commit messages for typos

### Notifications

- **Desktop notifications** — enable/disable OS notifications
- **Terminal completion** — notify when long commands finish
- **Push/pull completion** — notify on sync operations
- **Agent task completion** — notify when orchestrator tasks finish
- **Quiet hours** — suppress notifications during specified times

### Prefetch Profiles

Configure which files to pre-download on clone/pull:
- Define glob patterns for auto-hydration
- Multiple profiles for different workflows
- Per-branch profile overrides

### Experiment Tracking

- **Provider** — MLflow, Weights & Biases, or custom
- **Endpoint URL** — tracking server address
- **API key** — authentication (stored in keychain)
- **Auto-log** — automatically log metrics from workflow runs

### Integrations (Forge)

- **GitHub** — OAuth token, default organization
- **GitLab** — access token, instance URL
- **Bitbucket** — app password, workspace
- **Gitea** — token, instance URL
- Status shows connection health and token expiry

### Remote Storage

- **Default remote** — S3/GCS/Azure endpoint
- **Region** — cloud region
- **Bucket** — storage bucket name
- **Credentials** — AWS profile or explicit keys
- **Encryption** — client-side encryption settings

### Cache

- **Cache directory** — where chunk cache lives
- **Max cache size** — eviction threshold
- **Cache stats** — current usage, hit rate
- **Clear cache** — button to purge all cached data

### Memory

- **Max log lines** — ring buffer size for progress logs
- **Terminal scrollback** — lines per terminal session
- **Heap limit** — when to start evicting cached data
- **Memory usage** — current heap size display

### Orchestrator

- **Enabled** — toggle orchestrator feature
- **Default agent** — which AI agent to use
- **Worktree location** — where to create task worktrees
- **Auto-apply** — apply changes without review
- **Budget limits** — max tokens/cost per task
- **Agent log level** — verbosity of agent output

### Diagnostics

- **Health & Diagnostics** — agent and CLI versions, cache and staging usage,
  indexer status, repository information, and diagnostic reports
- **Notifications** — notification preferences and Do Not Disturb
- **Logs** — activity buffer size, agent log level, and developer tools
- **PTY Daemon** — terminal daemon status and diagnostics
- **Telemetry** — anonymous usage preferences and payload preview

Health & Diagnostics is part of Settings and has no separate activity-bar button.

### Shortcuts

- **Keybinding editor** — view and customize all keyboard shortcuts
- **Conflict detection** — highlights conflicting bindings
- **Reset to defaults** — restore original shortcuts
- **Import/Export** — share keybinding configurations

### Reset

- **Reset settings** — restore all settings to defaults
- **Clear state** — purge localStorage and cached data
- **Reset layout** — restore default panel sizes and positions

### About

- **Version** — app version, Electron version, Node version
- **Agent version** — Rust sidecar version
- **Crab CLI version** — installed CLI version
- **System info** — OS, architecture, memory
- **Telemetry** — opt-in/out of anonymous usage data
- **Licenses** — open-source license information

## Scope System

Settings can be scoped:
- **User** (global) — applies to all repositories
- **Workspace** — applies only to the current repository

A scope chip next to each setting shows its current scope. Workspace
settings override user settings.

## Persistence

- User settings → `localStorage` (renderer) + `main-settings.json`
  (main process subset)
- Workspace settings → `.crab/desktop/settings.json` in the repo
- Secrets (tokens, keys) → OS keychain via `safeStorage`

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Cmd+,` | Open Settings |
| `Cmd+K Cmd+S` | Open Keyboard Shortcuts |
| `/` | Focus search within settings |
| `Escape` | Close settings |
