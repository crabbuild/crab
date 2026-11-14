# Agent teams

Crab supports native provider sub-agents and mixed Codex/Claude teams in the
same Chat surface. A roster above the lead transcript shows each agent's
provider, ancestry and current state. Open an agent to read its instructions,
transcript, complete plain text and team messages. Copy and Download include
all saved text and tool output, including command-session chunks. Crab-managed
children also have direct message and Stop controls.

The surface extends the existing desktop design system: Geist, semantic theme
tokens, shared shadcn controls, compact rows and status colors. Desktop uses
agent navigation beside the transcript; narrow windows use a horizontally
scrolling agent list. Ancestry stays explicit at every depth. The viewer keeps
the lead conversation in place and traps focus while reading a child.

## Native sub-agents

`ClaudeCodeAdapter` requests `forwardSubagentText` during initialization.
`ClaudeEventParser` correlates `Agent`/`Task` tool calls, stable task/agent IDs,
`parent_tool_use_id`, `task_started` and `task_notification`. The initial
tool-call identity is replaced by the stable task identity when it arrives;
resuming the agent keeps its existing transcript. Background launch results do
not mark the child complete. Sub-agent assistant/tool events are wrapped before
entering the orchestrator, so a child completion cannot finish the lead turn.

`CodexSubagents` discovers descendants through `thread/started`,
`collabAgentToolCall` and `subAgentActivity`. It rejoins known child threads
with `thread/resume`, without configuration overrides. It pages
`thread/turns/list` with full items to fill history before subscription, then
routes live items and turn completions into that child's transcript. Completed
history items supersede buffered copies by provider identity. An active text
item has no stream offset; its authoritative `item/completed` snapshot supplies
any prefix missed before subscription. Subscription/read failures remain
visible inside the child transcript.

Native spawning, messaging and tool permissions stay with the provider.
Codex's collaboration calls and Claude's `SendMessage` results are projected
into the message history after the provider reports their result. Native
sub-agents use the lead's permission controls; their transcript viewer is
read-only. Guide those agents through the lead conversation and native tools.

## Mixed-provider coordination

Every orchestrated Codex/Claude adapter receives the same task-bound tools:

| Tool | Behavior |
| --- | --- |
| `crab_spawn_agent` | Create a named Codex or Claude child in the parent's workspace; return its saved identity before startup or approval. |
| `crab_list_agents` | List this task tree's identities, providers, parents and current statuses. |
| `crab_send_message` | Send to a parent, child or sibling through the recipient's normal native input path. Idle recipients resume; active recipients receive steering/input. |
| `crab_wait_agent` | Wait at most 30 seconds for the current turn; return the actual state if still queued or running. |
| `crab_read_agent` | Return the complete durable chat transcript. |
| `crab_stop_agent` | Stop the caller's descendants and join their owned processes. |

Codex receives dynamic tools on `thread/start`; calls return over
`item/tool/call`. Claude receives a `crab_team` SDK MCP server and calls it
through its existing `mcp_message` control transport. No listening socket,
temporary credential or additional runtime dependency is introduced. The same
stdio mechanism is available to local and SSH-owned provider processes; live
qualification below covers local providers.

`agent-team.ts` owns tool validation and team membership. Callers cannot read,
message or stop another tree. An agent cannot stop its parent or siblings.
Reparenting rejects cycles. Message journals distinguish acknowledged,
unconfirmed and failed delivery; failed submissions never appear delivered.
Pending waits remove their listeners and timers when the owner cancels.

The orchestrator owns child admission, provider startup, workspace permissions,
checkpoints and closure. Children borrow the parent's workspace; only its owner
may dispose of it. Stopping a lead also stops descendants. Children count
toward existing task capacity; a queued child remains queued when all slots
are occupied. A waiting parent must yield/finish if it occupies the only slot.
Agents should assign separate files or explicitly coordinate edits.

## Persistence and rendering

`applyAgentEvent` stores each child as a separate nested transcript entry in
the existing chat journal. Text/tool identities are scoped to that child.
Child status and output also propagate to the parent journal, so the roster
survives renderer unmounts and reloads. Mixed children retain their own normal
chat records and independent provider session IDs.

`AgentTeamPanel` reads that projection; it does not infer completion from the
presence of output. The rich transcript reuses existing turn/tool rendering.
Full text and download expose complete saved output even when a rich tool
preview or transcript virtualization shows a smaller view. Agent operation
cards share `SubAgentCard`; neither instructions nor result text is sliced.

## Contracts and proof

Verified with Codex CLI `0.163.0` generated experimental TypeScript protocol
bindings and Claude Code `2.1.319`, plus the official
[Codex app-server events](https://developers.openai.com/codex/app-server#events),
[Claude SDK types](https://code.claude.com/docs/en/agent-sdk/typescript),
[sub-agent attribution](https://code.claude.com/docs/en/agent-sdk/subagents),
and [SDK control bridge source](https://github.com/anthropics/claude-agent-sdk-python/blob/main/src/claude_agent_sdk/_internal/query.py).

`tests/agent-team.spec.ts` covers ownership, delivery uncertainty, cancellable
waits, concurrent item identities, resumed/nested Claude agents and paginated
Codex child history. `tests/browser/agent-team.pw.ts` covers complete output,
three-level ancestry, narrow layouts, export bytes and failed-send drafts.

`tests/agent-team-live.spec.ts` runs only with `CRAB_AGENT_LIVE_ROOT` pointing
to an owned qualification directory. Installed authenticated agents delegate
across providers, write a real file, read the saved child result, then stop and
resume to exchange another instruction with a visible file change. Separate
native sub-agent cases prove child output stays separate from the lead. The
browser suite can render these saved real-provider transcripts using that same
directory. Packaged, cross-platform and SSH team qualification remain separate
release gates.
