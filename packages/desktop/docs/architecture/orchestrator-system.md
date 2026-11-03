# Orchestrator System Architecture

The Orchestrator is the subsystem that manages AI agent tasks within
Crab Desktop. It provides isolated execution environments, lifecycle
management, checkpoint/rollback capabilities, and multi-agent
coordination.

## System Overview

```
┌────────────────────────────────────────────────────────────────┐
│  Renderer (AgentTasksView)                                     │
│  Task list · Chat tab · Diff tab · Files tab · Terminal tab    │
└────────────────────────────┬───────────────────────────────────┘
                             │ IPC: orchestrator:* channels
┌────────────────────────────▼───────────────────────────────────┐
│  Orchestrator (electron/orchestrator.ts)                       │
│  ┌──────────────┐ ┌──────────────┐ ┌────────────────────────┐  │
│  │ Task Manager │ │ Checkpoint   │ │ Worktree Isolation     │  │
│  │ (lifecycle)  │ │ Manager      │ │ (git worktrees)        │  │
│  └──────────────┘ └──────────────┘ └────────────────────────┘  │
│  ┌──────────────┐ ┌──────────────┐ ┌────────────────────────┐  │
│  │ Agent        │ │ Agent        │ │ Persistence            │  │
│  │ Registry     │ │ Adapters     │ │ (JSON on disk)         │  │
│  └──────────────┘ └──────────────┘ └────────────────────────┘  │
└────────────────────────────┬───────────────────────────────────┘
                             │ Spawns CLI processes
┌────────────────────────────▼───────────────────────────────────┐
│  Agent Processes (Claude Code, Codex, Custom)                  │
│  Each runs in its own git worktree with NDJSON output          │
└────────────────────────────────────────────────────────────────┘
```

## Task Lifecycle

A task progresses through a well-defined state machine:

```
                    ┌─────────┐
                    │ created │
                    └────┬────┘
                         │ enqueue
                    ┌────▼────┐
                    │ queued  │
                    └────┬────┘
                         │ start
                    ┌────▼─────┐
                    │ planning │  (agent reads context, forms plan)
                    └────┬─────┘
                         │
                    ┌────▼─────┐
              ┌────►│ running  │◄────┐
              │     └────┬─────┘     │
              │          │           │
              │     ┌────▼──────┐    │
              │     │ reviewing │    │ iterate
              │     └──┬──┬──┬──┘    │
              │        │  │  │       │
              │        │  │  └───────┘
              │        │  │
     ┌────────┘   ┌────▼──▼────┐
     │ stop       │  applied   │
     │            │  discarded │
┌────▼────┐       │  failed    │
│ stopped │       └────────────┘
└─────────┘
```

### State Descriptions

| State | Description |
|-------|-------------|
| `created` | Task object exists, no resources allocated |
| `queued` | Waiting for a free execution slot |
| `planning` | Agent is reading context and forming a plan |
| `running` | Agent is actively making changes |
| `reviewing` | Agent finished; user is reviewing changes |
| `iterating` | User sent feedback; agent is making revisions |
| `applied` | Changes merged into the user's branch |
| `discarded` | Changes rejected; worktree cleaned up |
| `failed` | Agent exited with an error |
| `stopped` | User manually stopped the agent |
| `archived` | Task moved to archive (hidden from active list) |

## Worktree Isolation

Each task runs in a dedicated git worktree to prevent interference
with the user's working tree:

```
Repository (.git)
├── Main worktree (user's checkout)
│   └── Working tree files...
├── .crab/worktrees/swift-falcon/     ← Task 1
│   ├── .crab-task.json               ← Task metadata
│   └── Working tree files...
└── .crab/worktrees/bold-cedar/       ← Task 2
    ├── .crab-task.json
    └── Working tree files...
```

### Naming Convention

Worktrees use Docker-style friendly names (`adjective-noun`):
- Directory: `.crab/worktrees/<friendly-name>/`
- Branch: `agent/<friendly-name>`
- Collision handling: appends numeric suffix (`swift-falcon-2`)

### Worktree Creation Flow

1. Resolve the base ref (current HEAD or specified branch)
2. Generate a unique friendly name
3. Create the git worktree: `git worktree add -b agent/<name> <path> <base>`
4. Write `.crab-task.json` metadata file
5. Return the worktree path to the orchestrator

### Worktree Cleanup

When a task is discarded or applied:
1. Remove the worktree: `git worktree remove --force <path>`
2. Delete the branch: `git branch -D agent/<name>`
3. Clean up checkpoint refs: `git update-ref -d refs/crab/checkpoints/<id>/*`

## Checkpoint System

The checkpoint manager creates per-turn snapshots of the workspace
state, enabling rollback to any previous agent turn.

### Storage

Checkpoints are stored as git refs:
```
refs/crab/checkpoints/<task-id>/<turn-number>
```

### Snapshot Strategy

Each checkpoint captures the FULL working tree state (tracked +
untracked files) by:

1. Creating a temporary index file
2. Adding all files (including untracked) to the temp index
3. Writing a tree object from the temp index
4. Creating a commit object pointing to that tree
5. Storing the commit SHA as the checkpoint ref

This ensures per-turn diffs are meaningful even when the agent doesn't
commit its changes between turns.

### Rollback

Rolling back to a checkpoint:
1. `git checkout <checkpoint-sha> -- .` (restore all files)
2. Clean untracked files that don't exist in the checkpoint tree
3. Update the task's turn counter

## Agent Adapters

The orchestrator supports multiple agent backends through a common
adapter interface:

```typescript
interface AgentAdapter {
  spawn(config: SpawnConfig): ChildProcess;
  parseEvent(line: string): AgentEvent | null;
  formatPrompt(prompt: string, context: string[]): string[];
}
```

### Supported Agents

| Agent | CLI Command | Output Format |
|-------|-------------|---------------|
| Claude Code | `claude` | NDJSON events |
| Codex | `codex` | NDJSON events |
| Custom | User-defined | Configurable parser |

### App-Server Adapter (Codex)

The Codex integration uses a persistent `codex app-server --listen stdio://`
process per task, driven over JSON-RPC 2.0 (newline-delimited stdio). This
replaces the earlier per-turn `codex exec --json` subprocess model.

Key differences from the old exec adapter:

| Aspect | Exec adapter (deleted) | App-server adapter |
|--------|------------------------|--------------------|
| Process lifetime | One process per turn | One process per task |
| Communication | NDJSON on stdout | Bidirectional JSON-RPC over stdio |
| Approvals | Queued, delivered next turn | Answered live on the wire |
| Follow-ups | Re-spawn with preamble | `turn/start` on existing connection |
| Mid-turn steering | Not possible | `turn/steer` (falls back to interrupt + start) |

The adapter is implemented across three modules:

- **`codex-rpc.ts`** — `CodexRpcClient`: JSON-RPC framing, request/response
  correlation, backpressure retry (`-32001`), server-request dispatch.
- **`codex-app-server-adapter.ts`** — `CodexAppServerAdapter`: lifecycle
  (initialize → thread → turns → shutdown), notification routing, approval
  handling.
- **`codex-shared.ts`** — Shared parsing (`parseCodexEvents`, `parseCodexItem`,
  `extractUsageEvent`) and decision mapping (`mapDecisionToCodex`).

#### Optional methods on `AgentAdapter`

The base `AgentAdapter` class exposes two optional methods that only the
app-server adapter implements:

```typescript
/**
 * Send a follow-up to a long-lived adapter without re-spawning.
 * The orchestrator calls this from iterate() when the method is present.
 */
iterate?(prompt: string): void;

/**
 * Deliver the user's approval decision live over the wire.
 * The orchestrator calls this from recordApproval() when present.
 * `decision` uses Crab's vocabulary; the adapter maps it to the
 * protocol's vocabulary internally.
 */
respondApproval?(requestId: string, decision: "allow" | "allow_once" | "deny"): void;
```

Other adapters (Claude Code, Custom) do not implement these and continue
using the existing re-spawn and preamble-injection paths.

#### Approval round-trip sequence

```
┌──────────┐       ┌─────┐       ┌──────────────┐       ┌─────────────────────┐       ┌──────────────────┐
│ Renderer │       │ IPC │       │ Orchestrator │       │ CodexAppServer      │       │ codex app-server │
│          │       │     │       │              │       │ Adapter             │       │ (child process)  │
└────┬─────┘       └──┬──┘       └──────┬───────┘       └──────────┬──────────┘       └────────┬─────────┘
     │                │                 │                           │                           │
     │                │                 │                           │  serverRequest (rpcId=N)  │
     │                │                 │                           │◄──────────────────────────│
     │                │                 │                           │                           │
     │                │                 │   approval_request event  │                           │
     │◄───────────────┼─────────────────┼───────────────────────────│                           │
     │                │                 │                           │                           │
     │  User clicks   │                 │                           │                           │
     │  "Allow"       │                 │                           │                           │
     │────────────────►                 │                           │                           │
     │                │  respondApproval│                           │                           │
     │                │────────────────►│                           │                           │
     │                │                 │                           │                           │
     │                │                 │  adapter.respondApproval()│                           │
     │                │                 │──────────────────────────►│                           │
     │                │                 │                           │                           │
     │                │                 │                           │  respond(N, {decision})   │
     │                │                 │                           │──────────────────────────►│
     │                │                 │                           │                           │
     │                │                 │                           │  serverRequest/resolved   │
     │                │                 │                           │◄──────────────────────────│
     │                │                 │                           │                           │
     │                │                 │                           │  (adapter clears pending) │
     │                │                 │                           │                           │
     │                │                 │                           │  item/completed (turn     │
     │                │                 │                           │  resumes normally)        │
     │                │                 │                           │◄──────────────────────────│
     │                │                 │                           │                           │
```

Decision mapping (Crab → Codex protocol):

| Crab decision | Codex decision       | Semantics                         |
|---------------|----------------------|-----------------------------------|
| `allow`       | `acceptForSession`   | Standing rule — Codex caches it.  |
| `allow_once`  | `accept`             | This invocation only.             |
| `deny`        | `decline`            | Reject; turn continues if able.   |
| (shutdown)    | `cancel`             | Reject and interrupt the turn.    |

#### Observability

Per-connection metrics tracked by the adapter (surfaced via IPC, never
sent off-machine):

- `initMs` — time to complete the initialize handshake
- `turnCount` — total turns started on this connection
- `interruptCount` — turns interrupted via `turn/interrupt`
- `steerCount` / `steerFallbackCount` — mid-turn steer attempts and fallbacks
- `approvalRoundTripMs[]` — per-approval latency
- `reconnectCount` — times the orchestrator re-created the adapter
- `backpressureRejects` — `-32001` responses received

### PATH Resolution

On macOS, GUI apps don't inherit the user's shell PATH. The adapter
layer resolves tool locations by:
1. Checking the login shell PATH (cached on first call)
2. Scanning common directories: `~/.cargo/bin`, `/usr/local/bin`,
   `/opt/homebrew/bin`, `~/.local/bin`
3. Detecting nvm/mise-installed Node versions
4. Building an expanded PATH for the spawned process

## Agent Events

Agents emit a stream of events that the orchestrator normalizes:

```typescript
type AgentEvent =
  | { type: "thinking"; content: string }
  | { type: "tool_use"; tool: string; input: object }
  | { type: "tool_result"; tool: string; output: string }
  | { type: "text"; content: string }
  | { type: "error"; message: string }
  | { type: "done"; exit_code: number };
```

These events drive the Chat tab's real-time display of agent activity.

## Persistence

Task metadata is persisted to disk at
`~/.crab/desktop/agent-tasks.json`:

```json
{
  "tasks": [
    {
      "id": "c2f4fe17-64bf-4339-8622-3d935b3c7522",
      "prompt": "Add pagination to the /users endpoint",
      "agentType": "claude-code",
      "status": "reviewing",
      "branch": "agent/swift-falcon",
      "worktreePath": "/repo/.crab/worktrees/swift-falcon",
      "baseRef": "abc123",
      "baseBranch": "main",
      "repoPath": "/Users/dev/project",
      "createdAt": 1715000000000,
      "startedAt": 1715000001000,
      "feedback": [],
      "todos": []
    }
  ]
}
```

Events are NOT persisted (they can be large). On restart, the
orchestrator reads task metadata and reconciles with actual worktree
state on disk.

## Multi-Agent Coordination

Tasks can spawn additional agents that share the same worktree:

- Each agent has a role (e.g., "coder", "reviewer", "tester")
- Agents coordinate through the shared filesystem
- The orchestrator tracks all agents per task
- Stopping a task terminates all associated agents

## Resource Limits

The orchestrator enforces configurable limits:

| Limit | Default | Purpose |
|-------|---------|---------|
| Max concurrent tasks | 3 | Prevent resource exhaustion |
| Task timeout | 30 min | Kill runaway agents |
| Max turns per task | 50 | Prevent infinite loops |
| Budget (tokens) | Configurable | Cost control |
