# Terminal View

The Terminal view provides a full-featured terminal emulator built on
xterm.js with GPU-accelerated rendering, shell integration, split
panes, and deep integration with the Crab workflow.

## Accessing the Terminal

- Click the **Terminal** icon in the activity bar
- Keyboard shortcut: `` Ctrl+` `` (backtick)
- Quick terminal: `Cmd+Shift+T` opens a floating terminal panel

## Features

### Terminal Emulator

Built on `@xterm/xterm` with addons:
- **WebGL renderer** — GPU-accelerated for smooth scrolling
- **Unicode 11** — full emoji and CJK character support
- **Web links** — clickable URLs (Cmd+click)
- **Search** — find text in scrollback (`Cmd+F`)
- **Clipboard** — system clipboard integration
- **Serialization** — save/restore terminal state

### Split Panes

Create multiple terminal panes:
- **Horizontal split** — side-by-side terminals
- **Vertical split** — stacked terminals
- Resizable via drag handles
- Each pane runs an independent shell session

### Shell Detection

The terminal detects the user's login shell:
- Reads `$SHELL` environment variable
- Falls back to `/bin/zsh` (macOS) or `/bin/bash` (Linux)
- Inherits the user's shell configuration (`.zshrc`, `.bashrc`)

### Pre-Warmed Pool

Terminals spawn instantly because the main process pre-warms 2 PTY
shells on startup. When you open a terminal, it connects to an
already-running shell rather than waiting for shell initialization.

### Shell Integration

When supported by the shell, the terminal tracks:
- **Command boundaries** — knows where each command starts/ends
- **Exit codes** — shows success/failure per command
- **Command duration** — timing for long-running commands
- **Working directory** — tracks `cd` for breadcrumb display

### Color Schemes

Multiple built-in color schemes:
- Dark (default)
- Light
- Solarized Dark/Light
- Monokai
- Nord
- Dracula
- Custom (user-defined)

Schemes are selectable per-terminal or globally in Settings.

### Scrollback

Configurable scrollback buffer:
- Default: 10,000 lines
- Configurable: 1,000 – 100,000 lines
- Memory estimate shown in settings
- Scrollback persists across view switches (not app restarts)

### Paste Guard

Protects against dangerous paste operations:
- Detects multi-line pastes (potential command injection)
- Detects `sudo` commands in pasted content
- Shows confirmation dialog with paste analysis
- Configurable sensitivity

### Notifications

Desktop notifications for long-running commands:
- Notifies when a command completes after the terminal loses focus
- Configurable minimum duration threshold
- Shows command name and exit code in notification

### Bookmarks

Save frequently-used commands as bookmarks:
- Click the bookmark icon next to a completed command
- Access bookmarks from the toolbar dropdown
- Execute bookmarked commands with one click

### Terminal Presets

Quick-launch terminals with pre-configured commands:
- "Crab status" — opens terminal and runs `crab status`
- "Git log" — opens terminal with `git log --oneline`
- Custom presets definable in settings

## Layout

```
┌─────────────────────────────────────────────────────────────────┐
│  [+] [Split H] [Split V] [Scheme ▾] [Search] [Settings]         │
├─────────────────────────────────────────────────────────────────┤
│  Breadcrumb: ~/project (main) $                                 │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  $ crab status                                                │
│  On branch main                                                 │
│  Hydrated: 42/100 files (1.2 GB / 8.4 GB)                       │
│                                                                 │
│  $ git log --oneline -5                                         │
│  a1b2c3d feat: add dashboard view                               │
│  d4e5f6g fix: memory leak in file watcher                       │
│  h7i8j9k refactor: extract preview module                       │
│  l0m1n2o chore: update dependencies                             │
│  p3q4r5s docs: add architecture overview                        │
│                                                                 │
│  $                                                              │
│                                                                 │
├─────────────────────────────────────────────────────────────────┤
│  Status: zsh │ 10,000 lines │ UTF-8 │ Exit: 0 │ 0.3s            │
└─────────────────────────────────────────────────────────────────┘
```

### Breadcrumb Bar

Shows:
- Current working directory
- Current branch (if in a git repo)
- Shell type indicator

### Status Bar

Shows:
- Shell name and PID
- Scrollback usage
- Character encoding
- Last command exit code
- Last command duration

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `` Ctrl+` `` | Toggle terminal view |
| `Cmd+Shift+T` | New terminal |
| `Cmd+\` | Split horizontal |
| `Cmd+Shift+\` | Split vertical |
| `Cmd+W` | Close current pane |
| `Cmd+F` | Search in scrollback |
| `Cmd+K` | Clear terminal |
| `Cmd+↑` / `Cmd+↓` | Scroll by page |
| `Cmd+Home` / `Cmd+End` | Scroll to top/bottom |
| `Alt+←` / `Alt+→` | Switch between panes |

## Font Settings

Terminal font is configurable separately from the editor:
- Font family (monospace fonts recommended)
- Font size (8–24px)
- Line height
- Font weight
- Ligatures (on/off)

## Crash Recovery

If a terminal session crashes:
- A banner appears with the error message
- Option to restart the shell in the same pane
- Scrollback from before the crash is preserved
- Other terminal panes are unaffected

## Integration

- The Orchestrator's Terminal tab uses the same terminal component
- Worktree context menu → "Open terminal here" opens a terminal
  pre-cd'd to the worktree path
- Command palette commands can execute in the terminal
- Terminal output can be sent to the agent for analysis
