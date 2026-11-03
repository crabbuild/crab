# Keyboard Shortcuts Reference

Crab Desktop is designed for keyboard-first workflows. Every action
is accessible via keyboard, and shortcuts are customizable in
Settings → Shortcuts.

## Global Shortcuts

These shortcuts work regardless of which view is active:

| Shortcut | Action |
|----------|--------|
| `Cmd+Shift+P` | Open Command Palette |
| `Cmd+K` | Quick Open (fuzzy file search) |
| `Cmd+,` | Open Settings |
| `Cmd+Shift+T` | New Terminal |
| `` Ctrl+` `` | Toggle Terminal |
| `Cmd+W` | Close current tab/pane |
| `Cmd+Shift+W` | Close window |
| `Cmd+Q` | Quit application |
| `Cmd+R` | Reload window |
| `Cmd+Shift+I` | Toggle Developer Tools |

## View Navigation

| Shortcut | View |
|----------|------|
| `Cmd+1` | Dashboard |
| `Cmd+2` | Orchestrator |
| `Cmd+3` | Explorer |
| `Cmd+4` | Search |
| `Cmd+5` | Changes |
| `Cmd+6` | Large Files |
| `Cmd+7` | Branches |
| `Cmd+8` | Worktrees |
| `Cmd+9` | Timeline |
| `Cmd+Shift+E` | Focus Explorer |
| `Cmd+Shift+F` | Focus Search |
| `Cmd+Shift+G` | Focus Changes |
| `Cmd+Shift+B` | Focus Branches |

## Git Operations

| Shortcut | Action | Context |
|----------|--------|---------|
| `Cmd+Enter` | Commit | Changes view, message focused |
| `Cmd+Shift+K` | Push | Any view |
| `Cmd+Shift+L` | Pull | Any view |
| `Cmd+Shift+S` | Stash | Any view |

## Changes View

| Shortcut | Action |
|----------|--------|
| `Space` | Toggle stage/unstage selected file |
| `Cmd+A` | Select all files |
| `Delete` | Discard selected changes |
| `Cmd+Z` | Undo last stage/unstage |
| `]c` | Next hunk |
| `[c` | Previous hunk |

## Explorer View

| Shortcut | Action |
|----------|--------|
| `↑` / `↓` | Navigate entries |
| `Enter` | Open file / expand folder |
| `←` | Collapse folder |
| `→` | Expand folder |
| `/` | Focus filter |
| `Cmd+N` | New file |
| `Cmd+Shift+N` | New folder |

## Branches View

| Shortcut | Action |
|----------|--------|
| `Enter` | Checkout selected branch |
| `Cmd+N` | Create new branch |
| `Delete` | Delete selected branch |
| `F2` | Rename selected branch |
| `/` | Focus filter |

## Timeline View

| Shortcut | Action |
|----------|--------|
| `↑` / `↓` | Navigate commits |
| `Enter` | Open commit detail |
| `Cmd+C` | Copy commit SHA |
| `/` | Focus search |

## Terminal View

| Shortcut | Action |
|----------|--------|
| `Cmd+\` | Split horizontal |
| `Cmd+Shift+\` | Split vertical |
| `Cmd+W` | Close pane |
| `Cmd+F` | Search in scrollback |
| `Cmd+K` | Clear terminal |
| `Alt+←` / `Alt+→` | Switch panes |
| `Cmd+↑` / `Cmd+↓` | Scroll by page |

## Orchestrator View

| Shortcut | Action |
|----------|--------|
| `Cmd+N` | New task |
| `Cmd+Enter` | Send follow-up message |
| `Cmd+Shift+A` | Apply current task |
| `Escape` | Stop running task |

## Diff Viewer

| Shortcut | Action |
|----------|--------|
| `]c` | Next change |
| `[c` | Previous change |
| `Cmd+D` | Toggle unified/split |
| `Space` | Stage/unstage hunk |

## Search View

| Shortcut | Action |
|----------|--------|
| `Enter` | Execute search |
| `F4` | Next match |
| `Shift+F4` | Previous match |
| `Cmd+Shift+H` | Toggle replace |
| `Cmd+Alt+Enter` | Replace all |

## Dialog Shortcuts

| Shortcut | Action |
|----------|--------|
| `Enter` | Confirm / Submit |
| `Escape` | Cancel / Close |
| `Tab` | Next field |
| `Shift+Tab` | Previous field |

## Customization

All shortcuts can be customized:

1. Open Settings → Shortcuts (or `Cmd+K Cmd+S`)
2. Search for the command
3. Click the shortcut cell
4. Press the new key combination
5. Conflicts are highlighted automatically

### Keybinding File

Custom keybindings are stored in localStorage and can be
exported/imported as JSON for sharing across machines.

### Conflict Resolution

When two commands share the same shortcut:
- The more specific context wins (view-specific > global)
- Conflicts are highlighted in yellow in the shortcuts editor
- The user is prompted to resolve on detection

## Platform Notes

| macOS | Windows/Linux | Action |
|-------|---------------|--------|
| `Cmd` | `Ctrl` | Primary modifier |
| `Option` | `Alt` | Secondary modifier |
| `Cmd+Shift` | `Ctrl+Shift` | Tertiary modifier |
| `Ctrl` | (varies) | Terminal passthrough |
