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
│  │ Registry     │ │ Adapters     │ │ (SQLite)               │  │
│  └──────────────┘ └──────────────┘ └────────────────────────┘  │
└────────────────────────────┬───────────────────────────────────┘
                             │ Spawns CLI processes
┌────────────────────────────▼───────────────────────────────────┐
│  Agent Processes (Claude Code, Codex, Custom)                  │
│  Each runs in its own git worktree with NDJSON output          │
└────────────────────────────────────────────────────────────────┘
```

## Task Lifecycle

A task follows provider turn boundaries; persistent processes can serve several
turns without exiting:

```text
created → queued (if capacity is full) → running
running → reviewing (completed) | planning (plan completed)
running → failed | interrupted
reviewing / planning / stopped → iterating → running
running / queued → stopped
reviewing → applied
any open task → discarded
```

### State Descriptions

| State | Description |
|-------|-------------|
| `created` | Task and workspace exist; provider has not started |
| `queued` | Waiting for a free execution slot |
| `planning` | Native plan turn completed; awaiting review |
| `running` | Provider turn active, including native planning |
| `reviewing` | Agent finished; user is reviewing changes |
| `iterating` | User sent feedback; agent is making revisions |
| `applied` | Changes merged into the user's branch |
| `discarded` | Changes rejected; worktree cleaned up |
| `failed` | Provider turn failed or process exited unexpectedly |
| `interrupted` | Provider reports an interrupted turn |
| `stopped` | User manually stopped the agent |
| `archived` | Task moved to archive (hidden from active list) |

### Codex goals

Goals use the native app-server goal API and project the complete native state
into task metadata and the chat goal bar. Activation is a durable user input,
sharing ordinary submission, capacity admission, checkpoints, recovery, and
Retry. An active goal can start or continue turns automatically, so its owned
provider retains a slot between turns. Git snapshots wait until continuation
stops. Pause disables continuation after the current turn; Stop interrupts the
provider and pauses any remaining durable active goal before shutdown completes.

Cold metadata reads, pause, and clear initialize an owned app-server client
without resuming the thread or starting inference. These clients close on every
exit path and during workspace shutdown. Ordinary provider replacement lets
in-flight metadata reads finish; newly requested reads join pending startup.
Cold activation updates the saved goal
before resume, preventing an old active objective from running first. Goal
notifications update the persisted projection and visible accounting; native
status and usage are never inferred from a local objective string.
Only completions held for goal continuation can be finalized by a later goal
notification. Resume snapshots cannot finish previous work. An unchanged
exhausted budget can acknowledge activation without creating a turn; that
metadata acknowledgement settles task activity without fabricating completion.

## Worktree Isolation

By default, root tasks receive dedicated Git worktrees. Primary/existing
workspace modes use the selected checkout; child conversations share their
parent's workspace:

```
Repository (.git)
├── Main worktree (user's checkout)
│   └── Working tree files...
├── .crab/worktrees/swift-falcon/     ← Task 1
│   ├── .git                         ← Pointer to private Git directory
│   └── Working tree files...
└── .crab/worktrees/bold-cedar/       ← Task 2
    ├── .git
    └── Working tree files...
```

`crab-desktop-task.json` lives in each worktree's private Git directory so
snapshots and apply cannot stage it as user source. Matching source metadata
from `v2.0.0-alpha.1` is explicitly migrated before checkpoints or cleanup.

### Child conversations

Adding an agent creates a child task, then starts it through the ordinary
orchestrator adapter owner. Configured binaries/environment, native model and
mode controls, approvals, questions, deadlines, and provider session persistence
follow the same path as a root conversation. Child events and usage remain in
that child's transcript; the Agents view reads task status and opens each chat.

Children borrow their parent's workspace and branch (`worktreeMode: existing`).
They do not own cleanup or independently apply shared changes. Applying through
the parent requires all workspace conversations to be idle. Discarding an owned
workspace retires queued children, blocks new starts during removal, and awaits
native provider `close` before deleting files. Child deletion preserves borrowed
branches. Automatic turn commits skip another chat's in-progress changes;
workspace snapshots represent shared files, not isolated per-agent edits.

Native startup is awaitable, including queue admission and the initial provider
submission. Task creation returns before this wait so native questions remain
answerable. A retry closes the previous process before admission; shutdown keeps
its capacity slot until `close`. Pending follow-ups are cancelled before workspace
removal or restoration. Restore never resends a prompt: it reuses an existing
workspace or checks out the saved local branch, then updates borrowing chats to
the restored path. A missing branch is an error. Archive awaits native closure,
pending inputs and automatic snapshots, commits remaining edits, persists its
intent, then removes the clean owned worktree while preserving its branch.
Borrowed workspaces retain their files and peers. `WorkspaceScripts` owns local
setup, Run/dev-server, and archive-hook processes under this same boundary.
New owned local worktrees run configured setup before their first provider
submission and checkpoint. Borrowing conversations await the same preparation
job; stopping one waiter preserves peers, while stopping the final waiter awaits
physical script closure. Setup state is saved in task SQLite. Completed or
skipped setup survives process restart; failed/interrupted setup requires explicit
Retry. Existing and primary workspaces run setup only through the explicit IPC
action. Restore marks a physically recreated owned worktree for setup again.
Setup output enters the durable chat journal. A failure prevents provider writes,
keeps Retry visible, and retains an unsent follow-up for Retry after restart.
Startup never replaces a missing workspace from its original base. Explicit
Retry uses the same saved-branch restoration as Restore, after closing all peer
writers, and rebinds borrowing chats. Restoring through a child reopens an
archived owner; discarded owned workspaces cannot be revived. Primary/existing
directories without a Crab owner must be restored externally. Stale registration
cleanup targets the absent checkout and saved branch, preserving Git's locked
and live-worktree safeguards and unrelated registrations.
Before removing an owned worktree, Archive/Discard stop its script writers,
execute the configured archive hook, and await completion. Archive then captures
the hook's changes in the saved branch. A failed hook leaves the worktree intact.
Archiving a borrowed conversation stops only its own scripts and skips the
removal hook, preserving peers that still use the workspace.
Borrowed discards retain an admission barrier until their own writers close and
discarded status is saved; a late follow-up cannot restart the closing chat.

Apply now owns closure and the Git merge/selected-file operation together;
IPC cannot merge first and retire writers afterward. It stops all script writers
in the workspace, including peers in a borrowed directory, before reading files.
Restore awaits script closure before moving paths. Existing settings and
repository script configuration feed one owner; the task supplies repository
identity. Run replacement/nonconcurrent admission is serialized, ports stay
reserved while in use, and exhausted ranges fail without reusing another port.

On POSIX, scripts run in a dedicated process group. Stop signals that group,
escalates after two seconds, and awaits both stream closure and group exit;
background descendants with ignored stdio remain owned. Node's `killed` flag
only confirms signal delivery, not exit. Windows uses `taskkill /T /F`; Windows
orphan/job ownership and supported-OS qualification remain open.

Normal app quit places a synchronous admission barrier on the orchestrator,
withdraws pending inputs, and joins existing Stop operations. It awaits local
provider closure, goal readers/controls, pending startups, input recovery,
accepted workspace creation/apply/archive/restore/discard, script groups, and
automatic snapshots. Cancelling an archive hook prevents removal without being
reported as a failure to quit; a physical-close failure still fails shutdown.
Active local work becomes stopped; idle completed conversations keep their
outcomes. Undispatched initial jobs stay queued for the existing restart path;
cancelled follow-ups are never turned into automatic initial starts. Remaining
active native goals are paused through metadata-only clients before quit finishes.
`app-shutdown.ts` prevents every repeated Electron `will-quit` event and settles
all registered owners before `app.exit`. A failing sibling cannot bypass other
closures. This covers ordinary quit, not abrupt OS termination or orphan cleanup.

### Naming Convention

Worktrees use Docker-style friendly names (`adjective-noun`):
- Directory: `.crab/worktrees/<friendly-name>/`
- Branch: `agent/<friendly-name>`
- Collision handling: appends numeric suffix (`swift-falcon-2`)

### Worktree Creation Flow

1. Resolve the base ref (current HEAD or specified branch)
2. Generate a unique friendly name
3. Create the git worktree: `git worktree add -b agent/<name> <path> <base>`
4. Write `crab-desktop-task.json` in the private Git directory
5. Return the worktree path to the orchestrator

### Worktree Cleanup

When an owned task is discarded:
1. Remove the worktree: `git worktree remove --force <path>`
2. Delete the branch: `git branch -D agent/<name>`
3. Clean up checkpoint refs: `git update-ref -d refs/crab/checkpoints/<id>/*`

Applying changes retains the worktree and branch. Reopen returns an applied
conversation to review without reverting Git changes or sending a prompt.
Archive preserves the branch and checkpoints for later restoration. Closed
conversations retain their transcript; only explicit deletion removes the row.

## Checkpoint System

The checkpoint manager creates per-turn snapshots of the workspace
state, enabling rollback to any previous agent turn.

### Storage

Checkpoints are stored as git refs:
```
refs/crab/checkpoints/<task-id>/<turn-number>
refs/crab/checkpoint-intents/<task-id>/<turn-number>
refs/crab/bases/<base-sha>
refs/crab/restore-backups/<task-id>/<timestamp>-<uuid>
```

Local task baselines are resolved to commit SHAs and pinned before provider
launch. Primary-workspace creation snapshots dirty files without changing the
user's index or HEAD. Shared parent/child conversations retain their base until
the last non-discarded conversation releases it. Permanent deletion discards
first, then detaches children and commits metadata before deleting chat history.

### Snapshot Strategy

Each checkpoint captures the FULL working tree state (tracked +
untracked files) by:

1. Creating a temporary index file
2. Copying the real index, then adding all working files (including untracked)
3. Writing a tree object from the temp index
4. Creating a commit object pointing to that tree
5. Storing the commit SHA as the checkpoint ref

This ensures per-turn diffs are meaningful even when the agent doesn't
commit its changes between turns.

Local capture and ref publication are mandatory preconditions for provider
submission. Staging/filter failures propagate; no partial tree or HEAD fallback
is accepted. The clean-tree shortcut explicitly includes untracked files even
when the user's status configuration hides them. Retry validates that an
existing checkpoint resolves to a commit. Failure retains the exact unsent input
and settings for Retry, including across restart. A rejected queued follow-up
cannot release the preceding native turn's capacity or timeout ownership.

Post-turn automatic commit failures use the visible error channel while keeping
the provider's completed outcome. These local guarantees do not qualify remote
checkpoints: SSH agent dispatch and remote baseline ownership remain incomplete.

Follow-ups separate capture from publication. The orchestrator captures the
snapshot before provider dispatch, then publishes its ref only for accepted or
unconfirmed delivery. Definitively rejected input creates no checkpoint turn.
Bookkeeping is serialized per conversation; rejection leaves the accepted counter
and feedback unchanged. Counters persist in SQLite, including an upgrade from
the released schema's saved feedback. Retry reuses the prior
checkpoint and latest accepted/unconfirmed input without rewriting task identity.
Before capacity admission, SQLite stores the queued input text/ID and native
settings. After admission it stores the prepared candidate number and captured
SHA before native dispatch. A private `refs/crab/checkpoint-intents/` ref protects
the object until result bookkeeping, canonical ref publication, and journal
acknowledgement finish. Restart replays only that bookkeeping, once; it never
resends the input. Known rejection removes it, known acceptance remains accepted,
and missing delivery confirmation remains visible. Live local processes and the
SSH owner retain their liveness authority during metadata recovery.

A queued follow-up restored after owner loss remains unsent with a visible
Retry notice. Explicit Retry sends its saved text and settings; unrelated task
completion never sends it or replays the original prompt. Persisted initial
queued starts retain the automatic admission behavior shipped in
`v2.0.0-alpha.1`. Coordinator rows use the primary workspace and consume no
provider slot; the released coordinator rows migrate to that ownership mode.

### File comparisons

`registerDiffIpc` routes local, saved-turn, and remote requests. Local live and
saved comparisons share `computeDiff`; a missing baseline or Git failure is an
error, with no retry against a different ref or directory. Saved comparisons
read repository objects even when an archived worktree is absent. `HEAD` marks
the live working state, including uncommitted files, rather than claiming that
the result is the branch's committed tree.

Live previews copy the actual index into a private temporary directory, add
intent entries there, and compare the requested baseline against the working
tree. This includes tracked, staged, and untracked files together, preserves
staged ignored additions and split indexes, and compares previously untracked
baseline files accurately. The real index, files, branch, and checkpoint refs
are untouched. The temporary directory is removed on success or failure. Intent
entries record no file content; Git's comparison still honors required clean
filters, whose errors are surfaced.

A single bounded Git invocation emits NUL-delimited raw paths and patches.
Pairing them avoids separate path/patch reads disagreeing, preserves unusual
filenames and rename identity, and includes empty, binary, and symbolic-link
changes. Git text conversion and external diff programs are disabled for this
structured patch contract. The renderer ignores old replies, shows read
failures distinctly from an empty result, and offers Reload after repair.

Remote comparison now uses the same private-index strategy in the sidecar's
`task_diff` owner. It preserves the `diff`/`name_status`/`head_ref` response from
`v2.0.0-alpha.1`, deriving paths and patches from one bounded Git process. An
unavailable baseline returns a contextual error with its original source.
The alternate remote task caller also supplies its required worktree path.
Remote saved-turn requests reject before attempting local filesystem access.

Git C-quoted paths share one UTF-8 byte decoder across remote file rows, hunk
anchors, and staging-state grouping. Octal bytes are decoded together; trimming
cannot remove a filename's trailing spaces. Built-sidecar task-view proof uses
the real RPC router over owned stdio and loopback SSH channels, with a process
sandbox blocking access to the user's indexer database. Closing SSH during an
outstanding comparison produces a visible failure, rather than waiting for the
RPC deadline. Pending replies and UTF-8 decoding belong to the exact channel;
replacement cannot accept old replies or erase the new channel's buffer. Batch
consumers submit independent NDJSON requests through ordinary routing, matching
the sidecar's one-request-per-line parser and preserving remote errors.
Both repo routing and direct profile calls share reconnect admission. Only the
six inspected cacheable reads plus `task_compute_diff`, `git_stash_list` and
`git_blame` can queue; mutations and
unclassified operations fail immediately with `SSH_RECONNECTING`. Replaying
reads never schedules an earlier rejected write.
The session manager owns initial, explicit and reconnect attachment. Each
connection has a cancellation signal and jump chain; readiness follows a
validated capability handshake. Reconnect sends queued reads through the new
channel, and permanent disconnect rejects them. Remote tasks become interrupted
on reconnecting before orphan checks run against a ready channel.
Main supplies a fresh deployer per attempt. The sidecar's capability handshake
reports unsupported extended SSH services as false. Agentless profiles advertise
no repository operations; failed automatic deployment returns an error.
Four built-sidecar browser cases exercise real manager reconnect through an
injected startup-only deployer. Packaged binary lookup/upload, host-key trust,
remote native chat dispatch and descendant-process ownership remain gates.

### File restoration

`restoreFiles` accepts an exact checkpoint or backup ref owned by the
conversation. Full restoration requires a Crab-owned worktree, including a
borrowing chat whose owner is still present. Primary and unowned existing
folders reject it. Source validation precedes provider cancellation and is
repeated after every shared workspace writer has drained.
The saved owner must not be discarded, and the private Git metadata must match
that owner. Git's actual working-tree root and common repository must match the
saved paths; a separate worktree must have a separate Git index. Recreated
folders, replaced Git pointers, and unrelated repositories cannot redirect a
restore into the primary checkout. Text-hunk editing still supports primary
folders through physical path and exact-preimage checks; it never replaces a
whole Git index or tree.

Before modifying files or the real index, the manager saves the current working
state under a new backup ref. Ignored paths are outside snapshot ownership;
file/directory collisions reject restoration before any index mutation. Git
`read-tree` loads the captured state into the index, then restores the selected
tree with `--reset -u`. This removes later non-ignored additions as well as
restoring edits. The branch HEAD, checkpoint refs, native identity, transcript,
feedback, and input counters remain available. The index matches the selected
file state. Backup refs survive pruning and workspace Archive/Restore, and are
released on Discard alongside the task's checkpoints.

The saved-file menu exposes backups for recovery, surfaces errors, and can reload
failed reads. Snapshot reads follow the selected conversation's lifecycle. A
dedicated files-changed notification refreshes Diff and invalidates applied-file
markers for all sharing chats; it does not publish a native completion. Obsolete
Diff replies cannot replace the refreshed result.

File restoration does not implement native conversation rewind. Provider-specific
history revert and the mapping between native turns and accepted inputs remain
separate compatibility gates. Restore is also not an atomic multi-file
transaction: failures after backup creation leave a retained recovery state.

### Text hunk edits

Apply/revert IPC delegates to the orchestrator's workspace owner. It blocks new
submissions, closes every local provider sharing the directory (including
borrowed chats and independent primary-folder conversations), drains setup and
post-turn Git jobs, and closes Run descendants before mutating files. A closure
failure leaves file bytes untouched. Remote or closed conversations reject edits.

The hunk engine resolves the physical workspace and destination, rejects parent
symlink/path escapes and symbolic-link leaves, and requires a unique text
preimage. Creation cannot overwrite different contents; its inverse cannot
delete a changed file. An empty replacement can be inverted only for an empty
file, because string edits carry no insertion position. Whole provider diffs
still require their own contract or checkpoint restoration.

Turn-summary Undo stops at the first rejection and reports how many prior edits
were undone. Its success label describes file edits; it does not rewind native
conversation history, counters, or checkpoints. These per-file edits are not an
atomic multi-file transaction.

## Agent Adapters

The orchestrator supports multiple agent backends through a common
adapter interface:

```typescript
interface AgentAdapter {
  spawn(prompt: string, cwd: string, env?: Record<string, string>, options?: AgentTurnOptions): void | Promise<void>;
  iterate?(prompt: string, options?: AgentTurnOptions): void | Promise<void>;
  cancel(): void | Promise<void>;
  onEvent(listener: (event: AgentEvent) => void): () => void;
  onExit(listener: (code: number) => void): () => void;
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
| Mid-turn steering | Not possible | `turn/steer` with `expectedTurnId` |

The adapter is implemented across three modules:

- **`codex-rpc.ts`** — `CodexRpcClient`: JSON-RPC framing, request/response
  correlation, backpressure retry (`-32001`), server-request dispatch.
- **`codex-app-server-adapter.ts`** — `CodexAppServerAdapter`: lifecycle
  (initialize → thread → turns → shutdown), notification routing, approval
  handling.
- **`codex-shared.ts`** — Shared parsing (`parseCodexEvents`, `parseCodexItem`,
  `extractUsageEvent`). The adapter retains the server's offered approval
  decisions and returns the selected value unchanged.

Thread start and resume pass the task's current workspace path. A saved thread
may refer to a worktree that has since been recreated elsewhere; the native
resume request must override that saved path before starting another turn.

### Bidirectional Adapter (Claude Code)

`claude-code-adapter.ts` starts Claude with stream-json input/output, partial
messages, and the official SDK's stdio permission transport. It initializes the
connection before sending user messages and preserves it across turns. Saved
session IDs are stored in the task database and passed to `--resume` after a
restart. User/project permission settings remain authoritative; the adapter does
not force permission bypass.

`claude-code-events.ts` translates complete messages and partial events. It
deduplicates streamed text/thinking, pairs tools/results by tool-use ID, and
emits whole-session per-model usage snapshots from result messages. Question answers and tool approvals
return to their original control requests instead of starting a new chat turn.

### Durable transcripts

`orchestrator-chat.ts` registers chat IPC and owns `ChatJournal` in the main
process. `agent-chat-messages.ts` projects native events into ordered messages.
Every changed row is committed to SQLite before a renderer update is sent.
Text, thinking, tool inputs/results, requests, command/watch events, and actual
terminal outcomes survive an unmounted view. Metadata preserves provider block
and turn IDs. Row UPSERT updates the existing full-text search index; the schema
migration preserves earlier history.

The renderer's `useAgentChat` hook loads an authoritative snapshot and merges
newer revisioned updates that arrived during that load, including removals. It
never replaces the database with its local message array. `Orchestrator.iterate`
publishes pending intent before dispatch for every caller, including internal
follow-ups. Acceptance clears the pending marker; definitive rejection removes
that row. A new process restores unfinished sends as unconfirmed. Workflow
invocations and application notes also enter the journal. An unconfirmed
delivery retains the user's text and its warning,
including after a completed provider turn. A lost Electron response preserves
the renderer's intent until snapshot reconciliation. No automatic retry follows
an uncertain submission. Task feedback commits on accepted/unconfirmed delivery;
definitive rejection leaves its accepted counter unchanged. Native startup completion
is awaited for restarted processes. Initial starts and idle follow-ups share a
FIFO admission queue, reserving capacity through preparation and submission.
Cancellation removes queued input before snapshots or native writes. Provider
review/autonomous turns, active queued-turn accounting, and packaged crash
recovery remain qualification gates; pending-input recovery has real native
process-exit proof in the dedicated POSIX fixture.

Native terminal publication first saves its exact event, stable receipt ID, and
task outcome in the same task-database transaction. The chat journal commits and
publishes that row before acknowledging the receipt. Restart replays pending
receipts without provider execution; an existing row with the same ID is not
appended again. Earlier receipts drain before buffered later events, user input,
or notes. A metadata acknowledgement failure preserves both the receipt and an
already committed renderer patch. Startup errors share this terminal publisher.

A restored local Codex/Claude process that has no saved completion is interrupted,
not running. Initial queued starts keep their existing admission contract; queued
follow-ups remain explicit unsent Retry input. SSH liveness and workflow ownership
are separate. A completed autonomous cycle does not imply that an active goal
finished: losing that goal's owner interrupts it without starting inference.

Live requests remain answerable across selection and reload. Requests restored
after connection loss display `Connection closed`. A durable-write failure
stops the provider once and emits an error; later chat loads also expose that
failure. Renderer delivery failures do not invalidate a committed transcript.
Codex resolves per-thread requests when native turn transitions cancel them;
Crab follows those resolution notifications without inventing request lifetimes.

### Shared native controls

Text updates carry provider item IDs; completed blocks replace their own
partial streams. Completion uses the provider's terminal outcome. Rejected
steering leaves the active turn intact. Model, effort, and plan-mode changes use
native controls; active-turn changes are rejected. Codex supplies collaboration
settings with `turn/start`. Claude acknowledges permission mode, model, and
session effort controls before sending the next message. Separately acknowledged
settings persist even if a later control rejects the message. Cold resumes carry
requested settings through startup without changing the saved task first.
Explicit null resets reach the resumed session unchanged; definite startup
rejection retains the previously acknowledged model and effort.

Both review entry points submit through the task's input owner. Review intent
uses the same journal, capacity queue, checkpoint publication, cancellation, and
restart bookkeeping as ordinary input; its kind persists for explicit Retry.
Codex uses native `review/start` with instructions covering the task's saved
baseline, including committed and dirty changes. Claude receives those
instructions on its own conversation. Cold Codex review resumes the thread
without sending the original prompt or an extra ordinary turn. Removed the
separate one-shot review subprocess and its truncated diff prompt.

Inline Codex review forwards delegate starts/items while completing the parent
review turn. The adapter groups delegate activity under that parent, preserves
output before acknowledgement, and targets the live delegate when interrupted.
Reviews reject same-turn steering. Flat and virtualized transcripts expose the
same named chat log to assistive technology.

The composer passes the original prompt unchanged. New-chat, template, and
workflow plan choices use the same native adapter boundary without a synthetic
plan prefix. Selecting the agent default clears
a model override; selecting default effort clears the effort override using the
provider's reset semantics. A late Codex acknowledgement is correlated to its
turn ID so fast completion receives the correct planning/reviewing status.

Model pickers in new chats, active chats, and workflow steps query the configured
CLI. Codex uses paginated `model/list`; Claude supplies its model metadata in
the initialize response. Discovery creates no user turn. Native effort values
and supported levels come from that metadata; discovery errors remain visible.
`orchestrator-models.ts` owns each discovery process by renderer/request ID.
Cancel, picker unmount, full-page navigation, renderer crash, and window closure
retire it. Cancelled replies cannot replace a new catalog; Retry creates a new
request. New chats, active chats, and workflow steps share these controls.
Request IDs can be reused after cancellation, while a separate closure owner
retains the old process until it exits. App quit rejects new discovery and awaits
both pending and already-closing native processes.

Codex startup/submission and Claude initialization/session controls wait for
native replies. Claude user messages use the SDK's stdio input; a successful
pipe write alone does not prove that the provider consumed the message.
The task deadline and explicit cancellation own those waits; transport deadlines
do not interrupt legitimate startup hooks. Discovery has its own renderer owner,
and cancellation still bounds process shutdown.

`agent-elicitation.ts` owns standard MCP form/URL requests for both transports.
Forms preserve primitive types, defaults, and enum values, with JSON Schema
validation before a response is sent. URL cards show the full URL and server;
only an explicit user action opens the OS browser. Decline and cancel are
distinct responses. Request resolution means the response was delivered, not
that external authentication succeeded. Opaque Claude dialog kinds and Codex
elicitation extensions are not advertised. Claude initialization snapshots and
live prompt redelivery share request identity and render once.

`orchestrator-cost.ts` uses provider estimates and durable session/model
baselines. Claude's `modelUsage` includes subagents; its deltas become per-turn
records. Codex supplies tokens without a dollar estimate, so its cost displays
as unavailable. No unrelated model price is substituted.

#### Optional methods on `AgentAdapter`

Both persistent adapters implement these optional `AgentAdapter` methods:

```typescript
/**
 * Send a follow-up to a long-lived adapter without re-spawning.
 * The orchestrator calls this from iterate() when the method is present.
 */
iterate?(prompt: string, options?: AgentTurnOptions): void | Promise<void>;

/**
 * Deliver the user's approval decision live over the wire.
 * The orchestrator calls this from recordApproval() when present.
 * The option id must be one of the choices offered with that request.
 */
respondApproval?(requestId: string, decision: string): boolean;
respondInput?(requestId: string, response: AgentInputResponse): boolean;
listModels?(cwd: string, env?: Record<string, string>): Promise<AgentModel[]>;
```

`AgentInputResponse` distinguishes question answers from MCP accept/decline/
cancel responses and typed form content. Validation errors retain the pending
request so the user can correct and resubmit it.

`AgentTurnOptions` contains optional model, mode, and reasoning level. Explicit
`null` resets model or effort; omission leaves the current setting unchanged.
`configuration_changed` events report acknowledged settings, including Claude's
native permission-mode changes.

Custom adapters without these methods continue through their configured
per-turn process path. A false response means the request is no longer pending;
the UI must not display a successful submission.

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
- `steerCount` — mid-turn steer attempts
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

Task metadata is persisted in SQLite at
`~/.crab/desktop/agent-tasks.sqlite`. The logical record includes:

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

The raw live event stream remains ephemeral; terminal receipts are retained
until journal acknowledgement. The separate chat SQLite journal persists
the projected conversation, requests, submission state, and terminal outcomes.
On restart, the orchestrator loads task metadata and native session identities;
the renderer loads the journal through the main-process chat owner.

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
