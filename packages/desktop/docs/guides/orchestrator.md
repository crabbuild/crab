# Orchestrator View (Agent Tasks)

The Orchestrator view lets you delegate coding tasks to AI agents that
work in isolated git worktrees. Each task gets its own branch and
workspace — the agent makes changes without affecting your working
tree.

## Accessing the Orchestrator

- Click the **Bot** icon in the activity bar
- The Orchestrator is gated behind the `orchestratorEnabled` setting
  (enabled by default)

## Concepts

### Tasks

A task is a unit of work assigned to an agent:
- Has a natural-language prompt describing what to do
- Runs in an isolated git worktree (separate checkout)
- Produces commits on a dedicated branch
- Can be reviewed, iterated on, or discarded

### Task Lifecycle

```
created → queued → planning → running → reviewing → applied
                                    │              → discarded
                                    └─→ iterating → running
                                    └─→ failed
                                    └─→ stopped
```

### Agent Types

The orchestrator supports multiple agent backends:
- **Claude Code** — Anthropic's coding agent
- **Codex** — OpenAI's coding agent
- **Custom** — any CLI-based agent

## Layout

The view uses a three-column layout:

```
┌──────────┬───────────────────────────────────────────┐
│          │  ┌─────┬──────┬───────┬──────────┐        │
│  Task    │  │Chat │ Diff │ Files │ Terminal │        │
│  List    │  └─────┴──────┴───────┴──────────┘        │
│          │                                           │
│  • Task1 │  Chat / Diff / Files / Terminal content   │
│  • Task2 │                                           │
│  • Task3 │                                           │
│          │                                           │
└──────────┴───────────────────────────────────────────┘
```

### Task List (Left Panel)

- Shows all tasks with status indicators
- Color-coded by status (running=blue, reviewing=yellow, applied=green)
- Pinnable tasks stay at the top
- Click to select and view details

### Workspace Tabs (Right Panel)

#### Chat Tab
- Shows the agent's reasoning and tool calls
- Displays tool cards for each operation (file edits, bash commands,
  web fetches, search results)
- Supports follow-up messages for iteration
- Markdown rendering with syntax highlighting

#### Diff Tab
- Shows all changes made by the agent as a unified diff
- Hunk-level review (approve/reject individual changes)
- Side-by-side or inline diff modes

#### Files Tab
- Live file tree of the agent's worktree
- Shows which files were created, modified, or deleted
- Click to view file contents

#### Terminal Tab
- Direct terminal access to the agent's worktree
- Useful for running tests or inspecting the workspace

## Creating a Task

1. Click the **+** button or use `Cmd+N` in the Orchestrator view
2. Enter a prompt describing the task
3. Optionally select:
   - **Agent type** — which AI agent to use
   - **Base branch** — which branch to fork from
   - **Context pins** — files to include as context
   - **Issue link** — associate with a GitHub/GitLab issue
4. Click **Create** — the orchestrator:
   - Creates a new git worktree
   - Checks out a task branch
   - Spawns the agent with your prompt

## Reviewing Changes

When a task reaches the **reviewing** state:

1. Switch to the **Diff** tab to see all changes
2. Review each file's modifications
3. Choose an action:
   - **Apply** — merge the task branch into your working branch
   - **Iterate** — send feedback and let the agent continue
   - **Discard** — delete the worktree and branch

## Checkpoints

The orchestrator creates git checkpoints during task execution:
- Each significant agent action creates a checkpoint
- Use the **Checkpoint dropdown** to roll back to any point
- Checkpoints are lightweight (git commits in the worktree)

## Context Pins

Pin files or directories to include as context for the agent:
- Pinned files are passed to the agent at task creation
- Useful for providing examples, specs, or related code
- Managed via the **Context Pin Panel**

## Multi-Agent Support

Tasks can spawn additional agents:
- View all agents in the **Agents** tab
- Each agent has its own role and tool access
- Agents coordinate through the shared worktree

## Settings

Orchestrator settings are available in Settings → Orchestrator:
- **Default agent type** — which agent to use by default
- **Auto-apply** — automatically apply changes without review
- **Budget limits** — token/cost limits per task
- **Worktree location** — where to create task worktrees

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Cmd+N` | New task |
| `Cmd+Enter` | Send follow-up message |
| `Cmd+Shift+A` | Apply current task |
| `Escape` | Stop running task |

## Tool Cards

The Chat tab renders specialized cards for each agent tool use:

| Tool | Card | Shows |
|------|------|-------|
| File Edit | EditCard | Before/after diff with syntax highlighting |
| Bash Command | BashCard | Command + output with exit code |
| File Read | ReadCard | File path + content excerpt |
| Search | SearchCard | Query + matching results |
| Web Fetch | WebFetchCard | URL + fetched content summary |
| Sub-Agent | SubAgentCard | Delegated task + result |

Each card is collapsible and shows a status indicator (success/failure).

## Budget Management

The orchestrator tracks resource usage per task:
- **Token count** — total tokens consumed by the agent
- **Cost estimate** — approximate dollar cost
- **Turn count** — number of agent iterations

When a task approaches its budget limit, a warning dialog appears
with options to:
- Increase the budget and continue
- Stop the task and review current changes
- Discard the task

## Suggested Actions

After a task completes, the UI suggests contextual next steps:
- "Run tests" — if the agent modified test files
- "Create PR" — if changes are on a feature branch
- "Apply and push" — for simple, reviewed changes
- "Iterate" — if the changes need refinement

## Error Recovery

If an agent task fails:
- The error message is displayed in the Chat tab
- The worktree is preserved (changes are not lost)
- Options: retry with modified prompt, iterate with feedback, or discard
- The terminal tab shows the agent's stderr output for debugging
