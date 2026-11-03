# Command Palette

The Command Palette is a keyboard-driven interface for executing any
action in Crab Desktop. It provides fuzzy search across 200+
commands, context-aware filtering, and a secondary prompt system for
commands that need additional input.

## Opening the Command Palette

- Keyboard shortcut: `Cmd+Shift+P`
- Also accessible from the TopBar menu

## How It Works

### Fuzzy Search

Type any part of a command name to filter results:
- "branch" → shows all branch-related commands
- "push" → shows push commands
- "theme" → shows appearance commands

The search is fuzzy — "crbr" matches "Create Branch" because it
contains the characters in order.

### Context-Aware Filtering

Commands are filtered based on the current context:
- **No repo open** — only global commands shown
- **Repo with forge** — PR commands available
- **Active merge** — conflict resolution commands prioritized
- **Dirty working tree** — commit/stash commands highlighted

### Command Categories

Commands are organized by category:

| Category | Examples |
|----------|---------|
| Git | Commit, Push, Pull, Fetch, Merge, Rebase |
| Branch | Create, Delete, Rename, Checkout, Set Upstream |
| Stash | Save, Pop, Apply, Drop |
| View | Switch to Explorer, Open Terminal, Show Dashboard |
| File | New File, New Folder, Reveal in Finder |
| Crab | Hydrate, Dehydrate, Add, Push Large Files |
| Settings | Open Settings, Change Theme, Toggle Feature |
| Help | Show Shortcuts, Open Documentation, Copy Diagnostics |

### Secondary Prompts

Some commands need additional input. When selected, the palette
transitions to a prompt mode:

**Example: "Create Branch"**
1. User types "create branch" → selects the command
2. Palette shows: "Branch name:" with a text input
3. User types "feature/auth" → presses Enter
4. Branch is created

**Example: "Checkout Branch"**
1. User types "checkout" → selects the command
2. Palette shows a list of available branches
3. User filters and selects → branch is checked out

### Prompt Types

| Type | UI | Example |
|------|-----|---------|
| `text` | Free-form input | Branch name, commit message |
| `choice` | Filterable list | Branch selection, remote selection |

## Keyboard Navigation

| Key | Action |
|-----|--------|
| `↑` / `↓` | Navigate results |
| `Enter` | Execute selected command |
| `Escape` | Close palette |
| `Backspace` (empty) | Go back to command list (from prompt) |
| `Tab` | Accept autocomplete suggestion |

## Command Registration

Commands are registered at startup from multiple sources:

### Static Commands (lib/git-commands.ts)

```typescript
registerCommand({
  id: "git.commit",
  label: "Git: Commit",
  category: "Git",
  when: (ctx) => ctx.hasStagedChanges,
  execute: async (ctx) => {
    // Navigate to Changes view and focus commit composer
  },
});
```

### View Commands (lib/view-commands.ts)

```typescript
registerCommand({
  id: "view.explorer",
  label: "View: Show Explorer",
  category: "View",
  execute: () => {
    window.dispatchEvent(new CustomEvent("crab:navigate", { detail: "explorer" }));
  },
});
```

### Dynamic Commands

Some commands are generated dynamically based on context:
- Recent branches (for quick checkout)
- Open repositories (for quick switch)
- Registered keyboard shortcuts (for discoverability)

## Integration with Views

The Command Palette can trigger actions in any view:
- Execute git operations without navigating to the relevant view
- Open specific settings sections directly
- Trigger terminal commands
- Navigate to specific commits or branches

## Customization

Commands can be rebound in Settings → Shortcuts:
- View all registered commands and their shortcuts
- Assign custom key bindings
- Detect and resolve conflicts
- Export/import keybinding configurations
