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

### Terminal Modes

Use the mode menu beside **New terminal** to choose the lifetime of new tabs,
splits, shell selections, and ordinary presets. **Normal** is the default;
existing tabs keep their mode. `CRAB_PTY_MODE=daemon` explicitly selects a
daemon default.
Launch requests carrying task/agent metadata retain daemon ownership.

| Mode | Owner | Closing the tab | Closing Crab |
|------|-------|-----------------|--------------|
| Daemon | Background terminal service | Hides the tab; session stays available in **Sessions** | Process keeps running |
| Normal | Crab application | Terminates the process | Terminates the process |

Both modes support the same terminal emulator, search, splits, clipboard,
shell integration, bookmarks, profiles, and broadcast input. Each tab shows its
mode with an icon: a terminal glyph for **Normal**, the SendToBack glyph for
**Daemon**. Hover the icon for its lifetime description; the mode menu retains
both names. If the daemon is unavailable, select **Normal** explicitly; Crab does not
silently change a terminal's lifetime. Shells start on demand; prewarming is disabled.
Closing or hiding the last tab leaves its pane empty. Use **New terminal** or
**Sessions** to open another session; closing a tab does not spawn a replacement.

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
- Local daemon scrollback and the active agent screen survive app close and reopen while the daemon remains alive

### Session Recovery

Ordinary local terminals run in the PTY daemon. A command or agent started in
one of these shells continues when the app closes or updates. Reopening the
repository attaches to the same process. If an update removes the older app bundle,
existing terminals keep their daemon and new terminals use the current runtime.
Both Daemon and Normal remain available together. **Normal** in the toolbar mode menu or
**New normal terminal** in the context menu creates an ephemeral terminal.
Saved normal tabs reopen as fresh shells in their original mode, including when
the daemon is unavailable. Saved startup commands and credentials are not replayed.

Crab saves tab order, pane splits and proportions, each pane's active tab, and
the focused pane as they change. Repository paths own this state, so switching
repositories or reopening a workspace with new repository IDs restores its
layout. Evicting a hidden terminal renderer releases its UI resources and
leaves durable processes running.

SSH terminals require **tmux on the remote Unix host**, available in the SSH
command path. Crab uses a separate `crab-desktop` tmux socket and named sessions;
it does not use your default tmux server. App close, workspace switching and SSH
transport loss detach the client while the remote shell and agent continue.
Reopening reattaches to the saved profile and remote session. Closing an SSH
terminal tab explicitly terminates that remote session.

If the remote host is unavailable, the saved SSH tab stays visible with a retry
message. Press Enter after reconnecting to retry the same session. Crab does not
replace a missing remote session with a fresh shell or rerun an agent command.
SSH terminals created before durable tmux ownership cannot recover a process
that already ended with its original SSH channel.

An OS reboot, remote reboot, explicit process termination, or daemon failure
can end a process. Layout recovery does not recreate that process's memory or
resume its work automatically. If a saved local process is gone, reopening its tab starts a fresh shell; saved
agent commands are not rerun. Checkpoint recovery is a separate confirmation flow.

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

The agent menu shows CLIs detected on the local host or explicitly configured
with an executable override. Saved custom shell commands remain available.
Configured executable paths, extra arguments, and environment variables apply
when launching a preset. Disabled agents are hidden. Use **Refresh CLI list**
after installing a CLI or changing its configuration. Failed detection offers
a retry without advertising unverified agents.

**New terminal**, **Find**, and **Sessions** use icon-only toolbar buttons;
hover or focus a button for its tooltip. A dot on Sessions indicates hidden
sessions, with the count in its tooltip.

## Layout

```
┌─────────────────────────────────────────────────────────────────┐
│  [+] [Mode ▾]             [Split →] [Split ↓] [Find]          │
│                          [Sessions] [Broadcast] [Settings]    │
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

### Toolbar

The main toolbar keeps creation, pane layouts, search, and sessions visible.
**Terminal settings** expands labelled controls for profiles, colour, fonts,
scrollback, paste protection, scrollbars, breadcrumbs, and output export.
Controls wrap when the terminal area narrows. Each pane's **Terminal actions**
menu contains prompt navigation, copying output, retrying commands, and bookmarks.
Broadcast displays a visible notice while input is sent to every open terminal.

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

- Repository and task terminals use the same terminal component and controls
- Worktree context menu → "Open terminal here" opens a terminal
  pre-cd'd to the worktree path
- Command palette commands can execute in the terminal
- Terminal output can be sent to the agent for analysis
