# Agent chat compatibility

Status: in progress; [draft PR #670](https://github.com/crabbuild/crab/pull/670). Base: `origin/crab-v2` at `2f6d36ec12a`.
Compatibility is not yet fully qualified.

Refreshed from `64b33b64cba`; the workflow-output update integrated without
conflicts. The earlier sidebar/navigation update and terminal-launch handoff
remain consistent across preload, types, and the task view.
The later push-discovery/startup update fast-forwarded from `e1bae932369`
without overlapping the audit. Tracked patch bytes and every untracked audit
file matched the preserved backup after integration. This update changes Rust
storage/cache dependencies and indexer composition, with no desktop TypeScript
or test changes; it does not supply new packaged sidecar qualification.
The subsequent forced-push history fix fast-forwarded to `2f6d36ec12a` without
overlap. It changes only CLI push code, its tests, and protocol documentation.
The tracked audit patch and all 50 untracked audit files again matched their
preserved copies after integration; comparison proof source is unchanged.

## Contracts checked

- Installed Codex CLI: `0.163.0`. Generated the experimental JSON schemas with
  `codex app-server generate-json-schema --experimental --out <scratch>`.
- [Official Codex app-server documentation](https://learn.chatgpt.com/docs/app-server).
- Claude Code was initially `2.1.239`; it auto-updated to `2.1.319` during
  qualification. Current native settings/local-command proof uses `2.1.319`.
- [Claude headless operation](https://code.claude.com/docs/en/headless),
  [Claude result types](https://code.claude.com/docs/en/agent-sdk/typescript#sdkresultmessage),
  [streaming](https://code.claude.com/docs/en/agent-sdk/streaming-output),
  [user input](https://code.claude.com/docs/en/agent-sdk/user-input), and
  [usage scopes](https://code.claude.com/docs/en/agent-sdk/cost-tracking).
- Inspected the official `@anthropic-ai/claude-agent-sdk` `0.3.319` transport
  source/types without adding a runtime dependency. Verified its initialize
  envelope and stdio permission transport against installed Claude `2.1.239`.
- [MCP elicitation specification](https://modelcontextprotocol.io/specification/2025-11-25/client/elicitation),
  installed Codex elicitation schemas, and the SDK's elicitation callback/control
  types. Standard forms and URL mode are supported; provider extensions and
  opaque Claude dialog kinds are not advertised.
- [Node child-process lifecycle](https://nodejs.org/api/child_process.html#event-close):
  process ownership ends on `close`, after stdio drains, including failed spawn.
  Native cancellation now returns that closure promise; transport or control
  failure completes the turn without pretending the process already exited.
- [SSH2 channel lifecycle](https://github.com/mscdex/ssh2/tree/v1.17.0#channel),
  installed `ssh2` 1.17.0 `Channel.js`, and
  [Node UTF-8 decoding](https://nodejs.org/api/string_decoder.html): EOF and
  close retire a channel's RPC owner. Half-closing stdin alone does not prove
  that remote stdout ended. Split multibyte packets need a decoder per channel.
- [JSON-RPC response identity and errors](https://www.jsonrpc.org/specification):
  replies echo the original ID and preserve the remote error code/data. The
  sidecar's `agent/src/main.rs` parses one `rpc::Request` per NDJSON line; it
  rejects a JSON array. Batched consumers now submit independent calls through
  the same routing and request lifecycle as single calls.
- [Node filesystem path resolution and exclusive creation](https://nodejs.org/docs/latest-v22.x/api/fs.html):
  hunk writes resolve actual parent paths, reject symbolic-link leaves, and
  create new files with `wx`. Lexical worktree prefixes cannot establish the
  destination of a path containing traversal or a symbolic-link parent.
- [Node 22 event ordering](https://nodejs.org/docs/latest-v22.x/api/events.html#asynchronous-vs-synchronous)
  makes journal listeners synchronous. Terminal task state and its replay receipt
  now commit before event publication. [SQLite WAL](https://www.sqlite.org/wal.html)
  does not make transactions across separate databases atomic; an acknowledged,
  replayable receipt bridges the task and chat commits instead.
- [SQLite UPSERT](https://www.sqlite.org/lang_upsert.html) and
  [conflict resolution](https://www.sqlite.org/lang_conflict.html): updating a
  streamed row must run UPDATE triggers so full-text search drops old text.
  REPLACE's delete triggers depend on `recursive_triggers` and do not provide
  that contract with this database's settings.
- [Node 22 writable streams](https://nodejs.org/docs/latest-v22.x/api/stream.html#writablewritechunk-encoding-callback):
  accepting bytes into a pipe buffer does not prove that the provider consumed
  them. Claude's SDK-style user-message write has no turn-start RPC response;
  initialization and session controls have separate acknowledgements.
- [Git ref updates](https://git-scm.com/docs/git-update-ref) and
  [object retention](https://git-scm.com/docs/git-gc#_notes): a private ref pins
  each prepared snapshot while delivery/bookkeeping is unsettled. A real Git
  pruning check verifies retention before checkpoint publication.
- [Git ref filtering](https://git-scm.com/docs/git-for-each-ref#_options): Retry
  checks the exact checkpoint name after a successful ref query. Repository
  errors cannot become an absent baseline and overwrite an accepted turn's
  checkpoint; failed initial setup captures its missing baseline on Retry.
- [Git staging errors](https://git-scm.com/docs/git-add),
  [commit creation](https://git-scm.com/docs/git-commit-tree), and
  [object verification](https://git-scm.com/docs/git-rev-parse): failed staging
  cannot become a partial snapshot or HEAD fallback. Retry verifies
  `^{commit}`; a raw SHA alone does not prove the object exists. Local baseline
  refs preserve primary-workspace snapshots across actual Git pruning.
- [Git index/tree restoration](https://git-scm.com/docs/git-read-tree) and
  [working-tree restoration](https://git-scm.com/docs/git-restore): restoring the
  index and files can preserve HEAD. Git can overwrite ignored files during
  restoration; an owned real-Git probe reproduced that behavior. The manager
  now rejects ignored-path collisions and pins the current state before writes.
  Snapshot capture starts from the actual index so staged additions remain
  tracked even after a new ignore rule matches them. A real split-index probe
  also preserves modified bytes.
- [Git repository identity](https://git-scm.com/docs/git-rev-parse): the actual
  working-tree root, shared repository directory, and per-worktree Git directory
  establish the restore destination. A real regression first reproduced Git
  walking from a recreated task folder into its parent and overwriting primary
  files. Restore now rejects that folder, a pointer sharing the primary index,
  and an unrelated repository before mutation. Core also verifies the saved
  owner's metadata and repeats validation after writer closure.
- [Git intent entries](https://git-scm.com/docs/git-add#Documentation/git-add.txt---intent-to-add)
  record paths without staging content. Local Diff uses a copied index to
  compare the full working state against its saved baseline. A real Git probe
  shows ordinary comparison falsely deleting a still-present untracked
  baseline file; intent entries correct that without changing the real index.
  [Git raw patch output](https://git-scm.com/docs/git-diff) supports NUL-delimited
  paths and patches in one invocation. Required filter failures reject the read;
  external diff and text-conversion programs are disabled for structured patches.
- [Git C quoting](https://github.com/git/git/blob/v2.50.1/quote.c) represents
  non-ASCII filename bytes with octal escapes. A shared decoder now preserves
  UTF-8 identity across remote rows, hunk anchors, and staging-state grouping.
  A real Git probe also reproduces trailing spaces before a header's tab;
  trimming that header used to address a different filename.
- [Git worktree restoration and safeguards](https://git-scm.com/docs/git-worktree#_options):
  saved-branch recovery supplies the existing branch without `-b`, `-B`, or
  force. It removes only a matching absent checkout's registration through the
  Crab CLI; `crab/src/cmd/worktree.rs` delegates removal to Git and scopes Crab
  state cleanup to that worktree. Real Git tests preserve locked, live, and
  unrelated stale registrations.

Pinned usage semantics to the installed [Codex `rust-v0.163.0` source](https://github.com/openai/codex/tree/rust-v0.163.0).
`TokenUsageInfo.append_last_usage` accumulates individual API responses.
Responses output includes reasoning; gross input includes cache categories.
Claude emits a completed assistant envelope per non-empty content block.
Its session/model totals include subagents and restored spend.

## Evidence map

| Surface | Entry and owner | Consumers and proof |
| --- | --- | --- |
| Agent launch and native controls | `electron/orchestrator.ts`, provider adapters | Registry factories → adapters → installed CLIs; `tests/agent-turn-controls.spec.ts`, orchestrator tests, live control changes |
| Codex transport and lifecycle | `electron/codex-app-server-adapter.ts`, `electron/codex-rpc.ts` | Orchestrator events → turn derivation; lifecycle, resume, approval, transport tests |
| Claude transport/output | `electron/claude-code-adapter.ts`, `electron/claude-code-events.ts` | Registry → orchestrator → renderer; transport and parser tests; live session continuity |
| Questions, MCP forms/URLs, permission profiles, and approvals | Adapter pending requests → `recordInput`/`recordApproval` IPC | `AskUserCard`, `ElicitationCard`, `ApprovalCard`, `AgentTasksView`; wire tests, browser checks, real questions/approvals and both providers' MCP form round trips |
| Model discovery | `electron/orchestrator-models.ts` → configured registry adapter | New chat, active chat, workflow pickers; renderer ownership/cancellation, browser retry/unmount checks, both installed CLIs |
| Durable chat | `electron/orchestrator-chat.ts` → `ChatDatabase`, shared `agent-chat-messages.ts` projection | Registered IPC → `useAgentChat` → `ChatTranscript`; real SQLite, late-snapshot browser checks, both native providers' reopened child requests |
| Review | Diff view and summary card → `startReview` → shared task submission → native adapter | Both providers' full task-view review/Archive/Restore checks, Codex cancellation, native delegate correlation and early-output tests |
| Codex goals | `GoalBar` → goal IPC → shared input/admission → native goal API | Native UI file/completion/cold-clear smoke, `tests/orchestrator-goals.spec.ts`, notification/accounting and early-resume contracts |
| Terminal crash recovery | Native/startup terminal publisher → task SQLite receipt/outcome transaction → chat journal → acknowledgement | Both provider crash windows around the real journal commit, replay/deduplication, storage-fault ordering, and lost goal-owner tests |
| Normal app quit | Electron `will-quit` → `app-shutdown.ts` → orchestrator/model-discovery owners | Repeated quit, sibling cleanup failure, physical closure, queued starts/follow-ups, checkpoint writers, and metadata-only native goal pause |
| Local workspace scripts | First provider launch, Run/setup IPC, and task lifecycle → durable workspace preparation → `WorkspaceScripts` → owned process groups | Real writers across Archive, Apply, shared borrowers, nonconcurrent replacement, failed hooks, quit, and final-subscriber setup cancellation; both native providers read setup files before writing |
| Text hunk editing | Apply/revert IPC → orchestrator workspace transition → physical path and preimage validation | Edit card and turn-summary Undo; both native owner boundaries, actual Run descendants, real file regressions, and renderer-to-filesystem success/stale-error checks |
| Saved file restoration | `restoreFiles` IPC → shared file-operation owner → checkpoint manager → retained Git backup and tree restoration | Saved-file menu, backup recovery, ignored-file rejection, real shared writers, native session/history continuity, and task-view Diff refresh |
| Local file comparisons | `registerDiffIpc` → `computeDiff` → private intent index and one Git raw/patch stream | Default and saved-turn Diff, PR description reads, archived object reads, unusual paths, actual IPC errors and renderer Reload; remote wire parser remains separate |
| Remote file comparisons | Same IPC → `RpcRouter` → sidecar dispatcher → `agent/src/task_diff.rs` | Private index, one bounded Git read, tagged response shape, shared path decoding; compiled sidecar and task-view checks over stdio and loopback SSH, with deployment still unqualified |
| RPC channel ownership | `RpcRouter.attachChannel` → channel-local pending calls/decoder → sidecar NDJSON requests | Cache/poller batches use ordinary routing; closure/replacement, profile isolation, split UTF-8, error fidelity, timeout and queue-admission regressions; real SSH browser failure during an outstanding read |
| Extended Codex actions | `electron/orchestrator-ipc.ts` → orchestrator → adapter | Feedback, preload API and operation contracts; remaining command/watch ownership and live controls are gates |
| Live behavior | `tests/agent-chat-live.spec.ts` | Installed CLI → real file write → renderer turn derivation |

Paths in the table are relative to `packages/desktop/`. Current `origin/main`
does not contain the desktop adapter; sibling comparisons use `crab-v2`.

## Corrections implemented

- Remote reconnect admission now defers only the six inspected cacheable reads
  and `task_compute_diff`. Mutating and unclassified methods reject immediately
  with `SSH_RECONNECTING`, including calls made directly by the orchestrator.
  Repo routing, profile RPC and batch consumers share this owner boundary;
  the mutation-prefix denylist and its classification-only tests are removed.
  Replaying reads cannot execute an earlier rejected write.
- Remote RPC pending calls now belong to their attached channel. EOF, close,
  error, or replacement immediately settles only that owner's calls and removes
  their deadlines. Old channel events cannot erase a replacement's partial
  frame; duplicate attachment is idempotent, and split UTF-8 survives packets.
  Batch consumers use independent NDJSON requests through ordinary routing,
  preserving registered remote transports, existing reconnect admission,
  per-profile deadlines, result order, successful peers, and remote error data.
  The old array transport and duplicate batch timers are removed. Session
  deployment/reconnect ownership remains a separate unfinished boundary.
- Remote file comparison no longer stages or resets the real index, substitutes
  a different baseline, or turns Git failures into empty results. The sidecar
  owns a private index, bounded output/child execution, contextual baseline
  errors, and cleanup. The tagged response fields remain intact; paths and
  patches come from one Git read. Client response validation rejects missing
  metadata, and the alternate remote task caller supplies its saved path.
  One UTF-8 decoder replaces the remote/header/staging quote decoders, and hunk
  headers preserve trailing spaces in names. Remote native dispatch and process
  lifecycle qualification remain open.
- Local live and saved-turn comparisons now share one read path. A private
  intent index includes mixed tracked/untracked changes and saved untracked
  baselines without modifying staged bytes. Removed HEAD/staged/directory
  fallback chains and the latest-turn content-staging snapshot. One Git process
  supplies structured file identity and patches, including unusual names,
  renames, binary/empty files, and symbolic links. Errors reach a visible Diff
  failure with Reload; a failed read cannot masquerade as no changes. Historical
  comparisons read repository objects, and remote requests cannot read local
  paths. Physical SSH deployment and remote native dispatch remain gates below.
- Replaced checkpoint branch resets and event-index trimming with file-only
  restoration. All sharing writers drain before capture; each restore saves a
  retained backup, validates an owned source ref, preserves native history and
  counters, and keeps the Git branch HEAD. Full restoration stays within
  Crab-owned worktrees with a live matching owner and verified Git identity.
  Discarded owners and replaced directories reject without changing files.
  Staged files matching a new ignore rule are captured;
  ignored-path collisions reject before real-index changes. Backups remain
  selectable after reload and are cleaned up on Discard. Both snapshot menus
  share one read/error/restore owner. File-change events refresh Diff and clear
  obsolete applied-file markers across sharing chats without claiming another
  native completion; late Diff replies are ignored.
- Text hunk writes now use the workspace owner, including all providers sharing
  a primary folder or borrowed worktree. Native cancellation, setup, Git jobs,
  and Run descendants settle before mutation. Physical path checks reject
  traversal and symlink escapes. Unique preimages prevent ambiguous replacement,
  deletion of changed additions, and overwriting an existing file without its
  preimage. Turn Undo stops at rejection and shows partial progress instead of
  claiming success. Its label now describes file edits, without implying that
  native conversation history was rewound.
- Local baseline capture, pinning, and per-turn checkpoints now block provider
  submission on failure. Required clean-filter failures retain the exact unsent
  input across reload and Retry. Hidden untracked files remain in snapshots;
  unborn branches create root snapshots without changing the user's HEAD/index.
  A failed queued capture retains the preceding native turn's capacity and
  timeout. Post-turn automatic commit errors reach the visible error channel
  without replacing the completed native outcome. Remote startup no longer
  attempts to manufacture a local baseline for an SSH path.
- Content-addressed base refs survive Git pruning until the last non-discarded
  parent/child conversation releases them. Permanent deletion detaches children
  before metadata persistence; failure restores the in-memory graph and retains
  chat history. Real SQLite and Git checks cover parent deletion and shared
  baseline retention.
- Removed launch-time recreation from the original base ref. Missing local
  workspaces fail before setup, checkpoint capture, or provider construction.
  Explicit Retry restores a Crab owner's saved branch through the same physical
  boundary as Restore, closes peer writers, updates borrowed paths, and sends
  the retained input. An archived owner reopens when restored through a child;
  discarded owned workspaces and unowned primary/existing directories are not
  replaced. Targeted stale-registration cleanup handles deleted checkouts and
  canonical path aliases without broad pruning or force.
- New owned local worktrees await configured setup before provider submission
  and checkpoint capture. Shared chats join one preparation job; Stop detaches
  one waiter, and the final waiter awaits the real script group. SQLite records
  completion or interruption, so ordinary cold resumes do not repeat setup.
  Physical Restore marks preparation pending again. Setup failures block native
  writes and expose Retry; a failed follow-up remains durable and retries its
  exact input after restart. The renderer keeps this known-unsent message,
  distinct from an uncertain native delivery. Existing/primary workspaces use
  explicit setup instead of receiving automatic scripts retroactively.
- Codex steering sends `expectedTurnId`. Native model/effort values reach
  `turn/start`, and collaboration settings select plan/default mode. Claude
  uses acknowledged session controls for permission mode, model, and effort.
  Per-task native settings no longer get duplicated into CLI flags.
- Codex handlers register before thread creation. Both start/resume responses
  publish the durable thread identity; early notifications are not lost or
  duplicated. Fork notifications do not select a different conversation.
  Both requests pass the current workspace explicitly; resume must not restore
  an obsolete worktree path from the provider's saved history.
- Approval responses preserve numeric/string JSON-RPC IDs. Concurrent requests
  cannot collide on a timestamp. File approvals read the official grant root.
- Closed transports reject pending and backoff requests. Process exits clear
  turn timers and approvals; initialization failure initiates shutdown.
- Claude result failures, including error subtypes and `is_error` results,
  produce a visible error and close the renderer turn.
- Codex goals retain the complete native object: objective, status, token budget,
  token/time usage, and timestamps. Updated/cleared notifications project into
  the task, SQLite, and the visible goal bar. First-goal creation, editing,
  pause/resume, clear, inline failures, and accounting are reachable from chat.
  Activation uses the shared input journal, admission, recovery, checkpoint,
  and exact Retry path. Native autonomous turns retain their capacity slot;
  automatic Git commits wait until the goal stops. Pause/clear can withdraw
  queued activation without submitting it. Closed workspaces reject activation.
  Metadata queries and inactive mutations use owned, initialized-only clients
  when no conversation process exists; they never resume inference. Stop closes
  the provider and automatically pauses any surviving durable active goal before
  releasing shutdown. A failed pause is surfaced instead of claiming closure
  succeeded. Native goal reads/controls use the ordinary bounded RPC deadline.
  Cold activation updates metadata before resume can continue an old objective;
  it sends no ordinary turn or original prompt. Early resume snapshots are
  accepted before the response continuation sets the thread identity.
  Only a completed turn held for an active goal can later be finalized by an
  inactive-goal notification; a no-goal resume snapshot cannot complete an old
  turn. Activation that remains budget-limited settles without inventing a new
  provider turn, even when second-resolution timestamps make its snapshot
  identical. Failed cold resume preserves uncertain delivery, marks failure,
  retires the provider, and pauses the accepted goal. Unexpected process loss
  between autonomous turns is a failure. Metadata readers survive ordinary
  provider replacement, join startup when necessary, and close on Stop or
  workspace shutdown; a resumed chat must not cancel its own concurrent read.
  These ownership decisions follow the [official goal API](https://learn.chatgpt.com/docs/app-server#manage-a-thread-goal)
  and [pinned goal processor](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/app-server/src/request_processors/thread_goal_processor.rs):
  active goals can start work immediately; materialized-thread metadata reads
  and mutations do not require loading the thread, and mutation acknowledgement
  precedes runtime effects. Individual autonomous-turn checkpoint/accounting
  reconciliation remains part of the broader provider-turn gate below.
- Reviews specify their target. Filesystem methods, base64 content, copy fields,
  client-generated watch IDs, and changed-path notifications match the schema.
  Extended experimental methods are enabled during initialization.
- Both review buttons now use task-owned submission. Review shares durable
  intent, admission, checkpoint publication, cancellation, and Retry; its kind
  survives queued recovery and accepted-input reload. Codex uses native custom
  review instructions against the saved task baseline, including committed and
  dirty changes; Claude uses its existing conversation. Cold Codex review
  resumes without an extra ordinary turn. Removed the unowned one-shot review
  subprocess, invalid sandbox flag, and truncated diff prompt.
- The [pinned upstream review implementation](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/core/src/tasks/review.rs) forwards its delegate's start/items
  while completing the parent turn. Delegate IDs no longer replace the parent
  lifecycle. Early review entry can establish identity before acknowledgement;
  completed output survives a late acknowledgement. Cancellation targets the
  native delegate; reviews reject same-turn steering. A review entry renders
  once across its item lifecycle. Flat and virtualized chats share the named
  transcript log.
- Goal and feedback controls check IPC success before updating UI state.
- Command sessions use argv, client process IDs, streamed base64, and the final
  execution response. UTF-8 split across chunks is preserved. Stdin, resize,
  and terminate are acknowledged requests; execution has no local RPC timeout.
- Removed the unused rollback IPC/preload/adapter route; the official server
  has retired that method. No renderer caller used it.
- Claude now uses persistent bidirectional stream-json with initialization,
  partial output, and the SDK's stdio permission protocol. It inherits configured
  permission policy and answers prompts live. Session IDs persist in SQLite and
  restore via explicit `--resume`; follow-ups contain only the new user message.
- Claude partial text/thinking and completed messages do not repeat output.
  Concurrent tool calls/results share tool-use IDs. Result usage is counted once
  per turn, without repeatedly adding assistant block totals.
- Both agents route question answers to the original pending request. The card
  supports multiple questions, multiple selections, free text, secret masking,
  cancellation, and submission errors. Removed the old global event that sent
  answers as unrelated iteration feedback.
- Approval cards display the provider's offered choices. Codex command/network
  amendments round-trip unchanged; unsupported decisions and stale answers are
  rejected. Removed the fake policy menu and the action to change an already
  resolved answer. Provider cancellation/connection closure disables old cards.
- Turn completion handles both persistent agents. Starting a follow-up clears
  stale completion metadata and restores its timeout. A late process exit cannot
  replace a user-stopped task with a failure.
- Native startup has an explicit completion promise: Codex waits for the first
  `turn/start` acknowledgement; Claude waits for initialization, initial session
  controls, and its SDK-style user-message write. Queued starts remain pending
  until admitted or cancelled. Creation returns the task ID without awaiting
  startup, so the chat needed to answer native prompts remains reachable.
  Retry retires the previous CLI before queue admission; old outcomes cannot
  overwrite the next attempt. Timeouts reserve capacity until native closure.
- Restore is distinct from Retry and does not resend the saved prompt. Existing
  workspaces are reused; missing owned workspaces check out the exact saved local
  branch and rebind borrowing conversations. A missing branch fails rather than
  creating from HEAD. Native processes and pending submissions close before a
  workspace changes. Primary/existing workspaces retain their ownership.
- Archive now uses the same workspace shutdown owner as Restore/Discard. It
  awaits provider closure, pending inputs, queued starts, and background Git
  snapshots, saves remaining edits, persists archive intent, and removes the
  clean owned worktree while retaining its branch. Persistence/removal failures
  remain visible; borrowed workspaces retain their files and other conversations.
  Applied tasks expose Reopen, without claiming to undo applied Git changes.
- Worktree control metadata now lives in the linked worktree's private Git
  directory, outside `git add -A`. An explicit migration moves matching
  `.crab-task.json` files shipped in `v2.0.0-alpha.1` before checkpoints or cleanup;
  ordinary readers use only the canonical administrative file. Native model and
  effort overrides also survive SQLite task reloads.
- Archived/applied/discarded conversations reject programmatic sends and disable
  the composer while retaining its draft and history. Archive/Discard failures
  remain visible in the task view without optimistically removing the chat.
- The task view now consumes preload's single-payload status, budget-warning,
  and nonconcurrent-stop subscriptions. Restore/Retry refresh backend metadata,
  including a restored workspace path; failures are visible. Renderer and IPC
  share the backend task-status type. Interrupted tasks expose Retry/Discard
  and terminal-row controls; all non-running statuses clear streaming state.
  Removed the separate `spawning` hint outside the task-status contract.

- Provider terminal outcomes determine completed/failed/interrupted status.
  Removed forced-success and failure-suppression renderer workarounds. Unexpected
  stdio closure fails unfinished turns promptly. Lifecycle waits for process
  `close` so buffered stdout can deliver its final result.
- Rejected Codex steering leaves the running turn intact. Submission
  acknowledgements serialize new-turn decisions. Only a definitive rejection
  after observed completion can start a new turn; timeouts/transport errors are
  not resubmitted. Interrupt rejection is handled and its terminal waiter is
  registered before sending the request.
- Lost Codex submission responses are distinct from provider rejections.
  Unconfirmed sends remain in durable history with a visible warning, including
  when the native turn has already completed. A Claude write failure after a
  dispatch attempt is also unconfirmed. An Electron response lost at the
  renderer preserves the local intent until a snapshot reconciles it. These
  paths do not automatically resend input or restore an old task status over
  potentially accepted work. Attention/error turns expose their actual expanded
  state to assistive technology.
- Text follows provider item/block IDs. Completed snapshots replace their own
  partial stream; repeated deltas and identical separate messages survive chat,
  raw-event turn rendering, and load. Removed the unused exec-format parser path
  and substring/time deduplication that discarded legitimate output.
- Codex counts every distinct response snapshot for the active turn, ignoring
  restored/foreign usage. Cache categories and reasoning are not counted twice.
  Claude tracks whole-session model totals with durable baselines across restart
  and new session identities after reset.
- Removed conflicting hardcoded dollar-price estimates. Costs use provider
  estimates; token-only providers display unavailable cost. Per-turn totals
  group model records. Old price-table aggregates without per-record provider
  estimates are not reused as evidence.
- Follow-up model choices reach Codex `turn/start` or Claude's acknowledged
  `set_model` control, then persist on the task. Mid-turn model changes are
  rejected before dispatch. Active conversations stay on their owning agent.
- Cold resumes carry requested settings separately from saved task state.
  Explicit model/effort resets survive into native startup; definite rejection
  retains previous settings, while separately acknowledged Claude controls
  remain authoritative even if a later control fails.
- New chats, follow-ups, and templates select native plan mode instead of
  prefixing the prompt. Workflow nodes now use the same native plan control
  without a synthetic prefix. Follow-up effort controls use provider-native values;
  default choices explicitly reset model/effort. Claude's individual control
  acknowledgements update task state even if a later control rejects the
  message. Codex lifecycle events carry turn IDs so a settings acknowledgement
  after fast completion reclassifies only its own turn.
- Codex permission-profile requests offer exact turn/session grants or denial.
  Cancellation grants no permissions; closed transports cannot acknowledge
  stale user decisions.
- Model pickers query installed-provider metadata, including pagination and
  native effort capabilities. Discovery does not create a thread or user turn.
  New chats, active chats, and workflow steps show discovery errors; explicitly
  configured model IDs remain visible. Launch and discovery both honor the
  configured executable path.
- Removed fixed startup/chat-submission cutoffs and Claude control cutoffs.
  Native responses, task deadlines, and caller cancellation own their waits.
  Discovery processes belong to a renderer/request ID and stop on cancellation,
  picker unmount, navigation, renderer loss, or window closure. Cancel/Retry is
  available in all three picker surfaces; late replies cannot replace a new
  request. Provider shutdown retains its bounded signal escalation.
- Child conversations use the same configured `startTask` path as root chats:
  native settings, request routing, task deadlines, session persistence, and
  terminal outcomes have one owner. Output and usage belong only to the child.
  The Agents view derives status from child tasks and opens their own chats.
  Shared changes are reviewed/applied from the parent; active conversations
  prevent applying. Children borrow the workspace and branch. Discarding the
  owner retires queued children, awaits native process closure, and blocks new
  starts/follow-ups during cleanup. Permanent deletion does not delete a
  borrowed branch. Automatic commits skip another conversation's active turn.
- Standard MCP forms preserve boolean/number/string/enum-array values, defaults,
  required fields, and provider field order. Validation uses the requested field
  schemas through Ajv/formats; arbitrary property names are safe. Invalid
  submissions remain answerable. URL requests identify the server and full
  destination, require a click to open the OS browser, and distinguish decline
  from cancel. Pending requests follow provider resolution notifications.
  Claude restores initialization prompt snapshots through
  the live handler and suppresses duplicate redelivery, including after an answer.
- Main-process chat ownership journals text, tools/results, typed requests,
  command/watch events, and terminal outcomes before renderer delivery. Removed
  renderer whole-history saves and selection-dependent projection. User sends
  enter durable history as pending intent before dispatch. Acceptance clears
  that marker; a definitive rejection removes only that pending row. A send
  interrupted by process loss restores with an unconfirmed-delivery warning.
  Workflow messages and application notes use the same owner. Reopening a live
  chat preserves answerable requests; reconnecting after process loss marks old
  requests closed. Failure to save stops the provider and is shown immediately
  or when its chat next opens. Renderer delivery failure leaves durable output
  intact. Revisioned updates reconcile with late snapshots without losing rows.
- Existing chat databases gain nullable metadata without replacing history.
  Row UPSERT keeps the full-text index current during streamed replacements.
  Turn rendering preserves separate provider blocks and actual terminal
  outcomes; pending requests, interrupted turns, and failures stay visible.
- `Orchestrator.iterate` owns submission events for every caller. Chat IPC now
  delegates instead of maintaining its own send recorder. Auto-fix, rebase
  resolution, workflow node feedback, and reject-plan follow-ups reach the same
  journal. Workflow invocations on a source task remain application commands,
  separate from the actual provider prompts sent to their node tasks.
  Explicit removal patches reconcile rejected sends even when history loads
  late. A pending intent that cannot be saved never reaches provider dispatch.
- Follow-up bookkeeping is serialized through native acceptance. Definitive
  rejection/cancellation leaves the accepted counter unchanged without adding
  feedback; uncertain delivery retains the input. Snapshots capture files before
  dispatch and publish refs only after accepted/unconfirmed delivery, avoiding
  phantom checkpoint turns for rejected messages. Retry preserves the existing
  checkpoint and sends the latest accepted/unconfirmed feedback, while the
  original task prompt remains unchanged. SQLite persists checkpoint numbering;
  the released schema's missing counter is migrated from saved feedback.
- Starts and idle-provider follow-ups share FIFO capacity admission. Preparation
  reserves a slot before snapshots or native writes; cancellation withdraws queued
  input. Active local providers, closing processes, and SSH-owned work consume
  capacity. Persisted local status without a process and workflow coordinators
  do not. Coordinator workspaces explicitly use `primary`; an SQL migration
  assigns that ownership to coordinator rows shipped in `v2.0.0-alpha.1`.
- Queued follow-ups persist their text and native settings before admission.
  After owner loss they remain unsent with an explicit Retry notice. Retry sends
  that exact input and settings, without replaying the original prompt. Early
  validation or native rejection retains any earlier unsent input. Persisted
  initial queued starts retain the automatic admission contract from the tag.
- Before dispatch, SQLite stores the exact pending input, candidate checkpoint
  number, and captured SHA; `refs/crab/checkpoint-intents/` pins the snapshot.
  Result/feedback/counter updates precede checkpoint publication. The recovery
  record and pin remain until the journal acknowledges the result. Restart
  restores accepted or conservatively unconfirmed bookkeeping exactly once and
  never sends native work. Known rejection removes the input. Live local
  adapters and the SSH owner retain their liveness decisions during recovery.
- Task-metadata failure cannot revoke known native acceptance. Failed rejection
  bookkeeping retains recoverable text until its result can be saved. A journal
  acknowledgement failure retains the recovery record, and an accepted journal
  row is not downgraded by older unknown task metadata. Checkpoint publication
  errors use the existing visible error channel while preserving accepted work.
- Native terminal outcomes and startup failures use one publisher. A task SQLite
  transaction saves the outcome plus an identified terminal receipt before chat
  publication. The journal commits and sends its patch, then clears that receipt.
  Restart replays the exact event; an existing stable ID prevents another row.
  Earlier receipts drain before later buffered frames, input, or notes. Failed
  acknowledgement preserves the receipt without dropping a committed UI patch.
  Restored local provider work without a known terminal state is interrupted;
  queued initial starts and SSH liveness keep their existing owner contracts.
  A completed cycle of an active goal is not recovered as goal completion.
- Removed the startup task-to-TTY reattach loop, its unowned module, and tests
  for that retired path. A direct repro changed `reviewing` to `failed` when
  the daemon correctly reported no matching session. Both tagged and current
  native adapter factories use stdio; terminal-only launches return before
  creating an orchestrator row. Native chat recovery therefore belongs to its
  own task/journal owner. Interactive terminals retain their existing daemon
  routing and renderer restoration; ADR 0003 now states that boundary explicitly.
- Normal app quit now stops accepting provider work before releasing slots. It
  joins an existing Stop, withdraws queued follow-ups, preserves undispatched
  initial jobs, and awaits native processes, goal metadata clients/controls,
  input recovery, checkpoints, automatic snapshots, and accepted workspace
  creation/archive/restore/discard. Active local tasks stop; idle completed
  tasks retain their outcomes. Durable active goals pause without thread resume.
  Model discovery keeps physical-close ownership after releasing a request ID.
  Main composes these owners with sidecar/LSP cleanup. Every repeated quit stays
  prevented, and all owners settle before exit, including when a sibling fails.
  Workflow scheduler ownership remains a separate gate; this does not claim
  abrupt-termination or packaged-restart cleanup.
- Local workspace scripts now have one process/port owner under the
  orchestrator. Removed the separate IPC maps, duplicate stop timers, and
  nonconcurrent policy split. Both repository configuration and saved settings
  use the task's repository identity. POSIX children form an owned process group;
  closing the shell or receiving a signal does not release descendants.
  This follows the [Node child-process contract](https://nodejs.org/api/child_process.html#subprocesskilled).
  Windows uses [taskkill tree termination](https://learn.microsoft.com/windows-server/administration/windows-commands/taskkill),
  but orphan/job ownership still needs Windows qualification.
- Archive/Discard retire local script writers before executing their configured
  removal hook. A hook failure preserves the workspace; Archive saves successful
  hook output on its branch. Borrowed conversations stop their own scripts and
  leave peers/removal hooks alone. Restore and normal quit await script ownership.
  Quit can cancel a hook without removing its worktree or reporting a false
  shutdown failure. Physical-close failures still prevent successful cleanup.
- Apply and selected-file application now run through one orchestrator operation
  after writer closure, with workspace admission blocked until the operation
  settles. The old IPC chain demonstrably merged and marked applied while both
  real writer processes remained alive. Run replacement and saved nonconcurrent
  mode await closure before spawning the next group. Port exhaustion is explicit;
  an occupied port is never reused as a fallback.

## Verification so far

- Draft PR #670 contains the accumulated audit changes; responsive Changes-page
  files remain outside it. The follow-up replay-admission run passes 104 tests
  across six scoped files. Four regression cases first reproduced deferred
  mutations and a direct-call bypass; current coverage adds the real file-write
  method across all three RPC entry points. All three SSH/browser cases pass.
  The built sidecar actually writes a fixture while connected; the next write
  rejects during reconnect, and replaying a queued comparison leaves its bytes
  unchanged. Types, strict fixture types, and the Electron build pass. Evidence:
  `ssh-replay-admission-before.json`, `ssh-replay-admission-final.json`,
  `ssh-replay-admission-live.json`, and `sidecar-ssh-replay-admission.json`.
- Claude credit-restoration recheck uses installed Claude Code 2.1.319 and the
  existing CLI login: both selected live cases pass. One writes an exact file
  and derives a completed renderer turn; the other retains the same session
  and exact inference context across follow-up, process closure, and resume.
  The JSON reporter includes 18 skipped cases in its aggregate passed count;
  the individual assertion records establish **two actual passes**. Evidence:
  `claude-credit-restored-recheck.json`.
- RPC channel qualification passes 102 tests across six scoped files. Eleven
  lifecycle regressions first failed against the old router. Three built-sidecar
  task-view cases now run over a real loopback SSH connection: normal tracked
  and Unicode Diff rendering, missing-baseline repair/Reload, and channel closure
  while a read is actually outstanding. The last case stops the owned sidecar,
  observes request delivery on SSH, closes that channel, and checks the visible
  failure before the RPC deadline. All three preserve the real index; sidecars
  exit successfully. A real batch combines ping, comparison, and an unknown
  method, preserving success order and error code -32601. The sandbox still
  blocks the user's indexer database. This qualifies transport, not deployment,
  normal indexer startup, reconnect, or remote native CLI launch. Evidence:
  `ssh-lifecycle-before.json`, `ssh-lifecycle-final.json`, `ssh-sidecar-final.json`,
  and `sidecar-diff-ssh-closure.json` in the owned qualification output.
  Production types, the strict test/fixture probe, and the Electron build pass;
  the router deletes 114 net production lines. Scoped ESLint cannot start:
  installed ESLint 9.39.4 requires `eslint.config.*`, absent from the desktop
  checkout and its unchanged base. `ssh-router-lint.log` records that blocker;
  no lint configuration or suppression was added to this transport change.
- Remote comparison qualification passes eight Rust tests and 45 client/live
  tests across three files. The real task view calls the registered IPC, RPC
  router, built sidecar, and Git over a stdio-backed channel: mixed tracked and
  Unicode files display, a missing baseline shows its identity, repair/Reload
  succeeds, and the real index stays byte-identical. The macOS fixture denies
  sidecar reads/writes to the user's indexer database; it does not exercise SSH
  deployment or claim a normal indexer startup. Five local renderer/file cases,
  both existing standalone parser checks, and all 31 browser checks also pass.
  Two Rust regressions first failed for omitted new files and a missing baseline
  becoming success. UTF-8/header tests first failed for quoted names and trailing
  spaces in mutation anchors. Evidence: `remote-diff-before.log`,
  `remote-path-before.json`, `remote-trailing-before.json`,
  `remote-rust-tests-last.log`, `remote-diff-proof-final.json`, and
  `remote-sidecar-last.json` in the owned qualification output.
  The first backup-recovery fixture waited on an old toast and a one-second file
  poll; it now awaits the actual restore API response before checking bytes.
  A combined run also omitted its installed `CRAB_BIN`; the configured rerun
  passes. Production/strict test types, both JavaScript builds, the sidecar build,
  Rust format, and `git diff --check` pass. Cargo uses this checkout's external
  target on the mounted workspace volume. `tempfile` moved from test-only to
  runtime use for owned preview indexes; the lockfile is unchanged.
  Clippy remains blocked by three pre-existing unwraps in
  `agent/src/forge/handlers.rs:233`, `agent/src/forge/handlers.rs:238`, and
  `agent/src/handlers.rs:8144` (the last is at line 8248 in the base). The same
  expressions exist at `0aadf7c4afb`; no diagnostic remains in the new reader.
  Production files, including embedded Rust tests, are 316 lines smaller than
  the base. `remote-comparison-verification.json` records scope and artifacts.
- Local Diff qualification passes 243 tests across four scoped files on the
  refreshed base, including 49 comparison, checkpoint, and remote-wire tests.
  Six regression cases failed first: mixed tracked/untracked changes, staged
  edits undone on disk, unchanged/modified untracked baselines, missing refs,
  and unusual paths/file kinds. Additional real Git checks cover staged ignored
  additions, split indexes with staged-byte preservation, unborn repositories,
  required clean-filter failures, quoted renames, and archived object reads.
  Checkpoint tests now invoke the production reader instead of reproducing its
  Git commands. Registered IPC tests prove saved/latest routing and error relay.
  Five real renderer/file cases pass through the production comparison IPC,
  including a missing-baseline error that recovers through Reload after repair.
  All 31 browser checks and both native task-view lifecycle scenarios pass.
  The native run preceded the unrelated upstream fast-forward; the TypeScript
  and test source were unchanged by that update. The final strict probe fixes
  two fixture type mistakes and includes both fixture global declarations;
  production typecheck, strict test types (excluding test no-unused policy),
  both JavaScript builds, and `git diff --check` pass. Production TypeScript is
  580 lines smaller than the base. Evidence: `diff-before.json`,
  `diff-latest-scoped.json`, `diff-renderer-ipc.json`, `diff-browser.log`,
  `diff-native-ui.json`, and `diff-verification.json` in the owned output.
  Remote sidecar computation and the remaining compatibility gates stay open.
- Saved-file restoration passes 284 tests across eight scoped files. Four real
  renderer/file checks cover successful Undo, partial stale-edit rejection,
  snapshot/backup recovery with Diff refresh, and visible ignored-file failure.
  A real shared Run-process check proves descendants close before restoration.
  Both installed providers pass three completed native turns through file
  restore, backup recovery, Apply, Archive/Restore, a blocked checkpoint capture,
  and exact Retry on the original native identity. Transcript and input counters
  remain intact. Claude uses the existing CLI login with restored credit.
  Evidence: `file-restore-scoped-final.json`, `file-restore-boundary-real.json`
  (five selected passed, eighteen unselected), and
  `file-restore-boundary-native.json` (two selected passed, four unselected).
  Regression evidence first reproduced ignored-file overwrites, missing staged
  ignored files, stale Diff results, and a recreated folder modifying its parent
  repository. Git identity and ownership guards now reject replacements before
  mutation, including an owner changed while native cancellation settles.
  Production typecheck, the strict test type probe (excluding test no-unused
  policy), both JavaScript builds, and `git diff --check` pass. The 31 browser
  checks also pass after fixing the real-file fixture's accidental use of the
  shared default Vite port. Production TypeScript is 411 lines smaller than the
  base, including new files. `file-restore-verification.json` records artifacts
  and proof scope; the remaining gates below still apply.
- Text-hunk follow-up passes 278 tests across nine scoped files and all 31
  request/workspace browser checks. Five real-file regressions failed before
  preimage validation; the actual Undo button failed its rejection check before
  error handling changed. Independent primary chats and borrowed chats await
  both provider cancellation promises, and a delayed post-turn Git writer blocks
  mutation. Three real Run-owner checks pass for owned, borrowed, and primary
  workspaces. Two renderer-to-Core checks verify actual file changes and visible
  stale-edit/partial-progress errors. These manual file checks do not run native
  inference or qualify provider-specific rewind formats.
  Evidence: `hunk-scoped-final.json`, `hunk-real-writers.json`,
  `hunk-renderer-final.json`, and `hunk-browser-final.log` in the owned output.
  Production typecheck, a strict test type probe (excluding the fixtures'
  no-unused policy), both JavaScript builds, and `git diff --check` pass.
  Removed the old Undo tests that reimplemented the component and asserted
  continuing after rejection; the real button and file effects replace that
  duplicate logic. Production TypeScript is 442 lines smaller than the base.
- Required-checkpoint qualification passes 285 tests across eight scoped files.
  Real Git/Core checks also prove primary and shared parent/child baselines
  survive pruning until their final owner discards them, including actual
  SQLite parent deletion. Production typecheck, the Electron build, and
  `git diff --check` pass. Production TypeScript is 470 lines smaller than the
  base, including new files. Evidence: `checkpoint-required-scoped-complete.json`
  and `checkpoint-base-retention-final.json` in the owned qualification output.
- Both providers now pass the required-clean-filter failure through the task
  view: native dispatch is blocked, reload retains the unsent input, and Retry
  completes it on the original session after repair. Claude passes in
  `checkpoint-required-native-ui-final.json`; Codex passes the traced rerun in
  `codex-rollout-diagnosis.json`. That earlier combined run failed Codex resume
  with a missing rollout. The rerun observes the same non-ephemeral rollout
  surviving both cold resumes; it does not explain the previous loss. Native
  review can use the user's configured review agents, so this fixture's task
  lifetime is five minutes rather than its former two; production stays at its
  existing thirty-minute default. No authentication or native configuration
  was changed to obtain a pass.
- Missing-workspace recovery passes with both installed providers through the
  real task view. External deletion leaves a stale Git registration; the failed
  follow-up remains visible through reload, and explicit Retry restores saved
  files, prepares the recreated workspace, and completes that exact follow-up
  on the original native identity. Real writers close before restoration.
  Evidence: `missing-workspace-native-ui-final.json` (two passed, two unselected),
  `missing-workspace-writers-final.json` (one selected passed), and
  `missing-workspace-verification.json` in the owned qualification output.
  The first Claude run stopped during its configured review agents at a
  premature 90-second observation deadline, before testing external deletion.
  The final unchanged provider configuration passes with observation aligned
  to the fixture's task-owned lifetime.
- The recovery follow-up passes 232 tests across five scoped files, including
  real Git/Crab checks for saved branches, symlink aliases, locked/live worktrees,
  and unrelated stale registrations. Borrowed Retry reopens archived owners;
  discarded owners and unowned missing directories remain protected. Production
  typecheck, the scoped strict probe, Electron build, and `git diff --check`
  pass. The branch still matches live `origin/crab-v2` at `e1bae932369`.
- Automatic local setup qualification passes on macOS: both installed providers
  read a setup-created file and produce the expected file, with setup output
  visible in the task view and included in the first checkpoint. Archive,
  physical Restore, review, and another restored follow-up preserve native
  identity. Deliberately failing setup before that follow-up exposes Retry,
  retains the unsent text through page reload, then sends that exact text after
  setup is repaired. Each recreated workspace prepares once. Evidence:
  `setup-native-ui-retry.json` (two passed, two unselected) and
  `setup-verification.json` in the owned qualification output.
- Setup follow-up passes 224 tests across six scoped files, twelve script-owner
  checks (including two deliberate syscall faults), thirty browser checks, and
  both real Electron/native-provider quit checks. Production typecheck, the
  scoped strict test probe, both JavaScript builds, and `git diff --check` pass.
  The current CLI login also passes a fresh real Claude question/answer/file
  check; no credential switch was used. Source hashes and report names are in
  `setup-verification.json`. Packaged restart and supported-OS gates below remain.
- Ten dedicated real-script checks pass using the installed Crab/Git and real
  shell descendants. They cover group retirement, preserved shared peers,
  saved archive-hook output, Apply's merge result, saved nonconcurrent mode,
  failed-hook preservation, quit during setup/archive, and admission during a
  borrowed conversation's discard, permission-denied existence probes, and
  denied termination signals. Targeted regressions
  were observed failing before their corresponding owner changes.
  Stress qualification exposed macOS `kill(group, 0)` returning `EPERM` during
  group retirement. The owner now keeps waiting for `ESRCH`; actual termination
  errors still fail and preserve the workspace. The distinction is covered by
  separate regressions and follows [Apple's process-group implementation](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_sig.c#L1579).
  Evidence: `scripts-owner-complete.json` and `scripts-verification.json` in the
  owned qualification output. Two permission cases deliberately inject syscall
  failures around real child processes; the other ownership checks are unfaked.
- Script-owner follow-up passes 288 tests across eleven scoped files, including
  local/remote apply callers, the shared script type contract, checkpoints,
  journals, goals, and workflow callers. The first diff-helper run lacked the
  packaged Crab binary; retrying with the installed `CRAB_BIN` and rerunning the
  complete scoped set passes. No native binary was copied into the checkout.
  The 30 renderer request/workspace checks, strict test type probe, production
  typecheck, and both JavaScript builds pass. Both real Electron quit checks
  also pass with the new owner integrated. Remaining source changes reduce
  non-test TypeScript by 609 lines versus the current base, including new files.
- Both native task-view checks pass with the real Run shortcut and Apply button,
  followed by Archive, Restore, and resumed inference on the same provider
  session. Actual writer PIDs are gone before Apply completes; the configured
  archive hook's file survives Restore. Each final transcript retains three
  completed native turns. Evidence: `scripts-native-ui-final.json` and each
  provider's workspace-lifecycle report/screenshot in the owned output.
  The first extended fixture run checked the composer while Diff was selected;
  it now explicitly opens Chat before asserting archived read-only state.
  This fixture error is distinct from the older unresolved Restore visibility
  failure below. A background Codex goal read cancelled by archive may return
  transport-closed; its disposed renderer owner ignores that obsolete reply.
- Normal-quit follow-up passes 240 tests across ten scoped files, covering core,
  journal/IPC/workflow callers, native adapter closure, model discovery, and
  Electron quit ordering. New regressions were observed failing before the
  shutdown owner was added. Typecheck and the Electron bundle build pass.
- Real Electron 33.4.11 quit checks pass for both installed providers. Each
  fixture writes an exact file, holds an actual MCP form, queues a follow-up and
  an initial task, then requests quit twice. Both quit events stay prevented;
  native exit notifications arrive before Electron exits, pending requests
  resolve, the follow-up is removed without a file effect, and SQLite preserves
  stopped/queued states. Codex's active goal is paused durably. This exercises
  built production owners in a dedicated Electron backend fixture, not the
  complete packaged Crab bootstrap or abrupt owner termination.
  Evidence: `shutdown-native-final.json`, `shutdown-scoped-synced.json`, and
  each provider's `*-electron-quit.json` in the owned qualification output.
- Node 22: renderer and Electron TypeScript checks pass.
- The review-stage qualification passed 506 tests across 39 targeted files,
  before the subsequent goal work. The final goal-stage verification passes
  240 tests across 15 relevant files, including goal ownership, no-goal resume,
  exhausted-budget acknowledgement, failed resume, metadata-reader replacement,
  and provider death between autonomous turns. The earlier coverage includes
  orchestrator lifecycle,
  transport, approvals, parsers, model controls, and extended operations.
  Real installed Crab/Git checks prove saved-branch restoration and rejection
  of a missing branch. Queue admission, startup acknowledgement, retry cleanup,
  timeout, restoration ownership, and cancellation while a pre-turn checkpoint
  is pending have targeted lifecycle coverage.
- Electron and renderer production builds pass.
- Terminal-recovery follow-up passes 235 tests across eight targeted files,
  including task/journal state, workflow consumers, active goals, TermRouter,
  and renderer terminal restoration. Renderer/Electron typechecks and a strict
  probe of the touched test fixtures pass. Both production builds pass after
  removal of the task-to-TTY startup path.
- Ten native owner-crash cases pass across the installed Codex and Claude Code
  CLIs: delivery, checkpoint publication, queued input, pre-journal terminal
  commit, and post-journal/pre-acknowledgement commit. Real files, pre-input
  checkpoint contents, counters, session identities, terminal rows, and receipt
  cleanup are checked after reopening both SQLite stores. Completed work is not
  resent; unsent queued input is sent only by explicit Retry. The full native
  goal UI/pause/resume/cold-follow-up regression also passes with this publisher.
  Evidence: `terminal-scoped-final.json`, `terminal-native-final.json`, and
  `terminal-native-goal-final.json` in the owned qualification output directory.
  The initial terminal fixture returned immediately after submission acceptance;
  it now waits for its selected native completion boundary before exit injection.
- Deterministic storage faults reproduced and now protect ordering of an older
  terminal receipt before buffered output, and delivery of a committed patch
  when the separate task acknowledgement fails. Repeated restart replay leaves
  one identified terminal row. Failed/interrupted/plan outcomes retain their own
  task status. These are process-owner crash checks; packaged shutdown and
  cross-OS coverage remain gates below.
- `git diff --check` passes. Including new files, production TypeScript remains
  smaller than the base; shared request validation and provider
  transports replace duplicate or unused paths.
- Thirty browser interaction tests pass in the `agent-requests` Playwright
  project. These exercise cards, model discovery, the text reducer, and the cost panel with controlled IPC;
  they do not constitute packaged/provider E2E proof. Visually checked the fixture,
  including child-chat controls at a narrow viewport.
  The complete task view also proves Restore does not invoke Retry, refreshes
  model discovery with the restored path, and handles single-payload notices.
- Four native renderer scenarios pass: both providers save, archive, restore,
  and resume an owned workspace on the same native conversation, and both answer
  a background child MCP request after opening/reloading its chat. The lifecycle
  cases use the full task view, real Git/Crab, SQLite, and native CLIs; a thin
  fixture HTTP bridge invokes production chat IPC and lifecycle methods. They
  do not qualify the packaged Electron main process. Claude also starts from
  the tagged source-metadata format and proves that control metadata does not
  enter the applied user branch. Native fixtures use independent Vite ports and
  caches to avoid interference with browser checks.
- Both providers' lifecycle scenarios now include a committed arithmetic
  regression and provider restart. Clicking Diff → Review with AI starts an
  owned review, finds the incorrect subtraction in `sum.ts`, leaves the file
  unchanged, and visibly completes on the saved conversation. Archive/Restore
  then preserves the finding and a further real file-changing turn succeeds.
  Codex's separate live cancellation check receives a successful native
  interrupt acknowledgement, closes its process, preserves the fixture file,
  and records an interrupted outcome.
- Six abrupt-owner-exit scenarios pass with real Codex/Claude inference: each
  provider executes a file change, then its owner exits before receiving delivery
  completion or before publishing the checkpoint. A new owner restores the exact
  pre-input SHA, one feedback entry, delivery status, and the resulting file,
  without replaying the input. Each provider also restores an undispatched
  follow-up as unsent, ignores unrelated task completion, and sends it exactly
  once only after explicit Retry. Owned detached process groups are stopped
  before recovery; this fault-injection fixture is POSIX-only.
- Both providers pass live global-capacity admission: a real MCP form holds the
  active slot while an idle conversation's input stays durable and undispatched.
  Answering the form admits the exact input on its existing session. Cancelling
  a second queued input prevents its file write and removes its pending row.
- Both providers pass the actual workflow caller's native plan controls. The
  workflow pauses for plan review without creating the requested implementation
  file. An explicit mode-changing follow-up creates that exact file on the same
  session and the workflow completes. The fixture isolates workflow records in
  memory; it does not qualify workflow run-file persistence.
- Both providers pass real cold-resume settings resets through the task owner:
  a discovered model/effort runs in native plan mode, its process closes, then
  a resumed turn resets mode/model/effort and creates the exact requested file.
  The native conversation identity is unchanged and saved settings reflect the
  acknowledgements. Regression tests also reject cold startup before or after
  a separately acknowledged Claude mode change without saving rejected settings.
- The refreshed task-view run had one Codex Archive → Restore visibility timeout
  with the backend still applied in the saved failure snapshot. The isolated
  rerun and both providers' lifecycle reruns under concurrent browser/build load
  pass; traced Codex native closure took about 0.3 seconds in the isolated run.
  The original failure has no established cause and remains a stability finding.
- A later lifecycle rerun completed Codex's visible Archive/Restore/resume proof
  but timed out during final cleanup. Claude's complete case passed. Incremental
  cleanup phases are now retained to distinguish browser, provider, Vite, and
  bridge shutdown before diagnosing the blocked owner.
  The instrumented isolated rerun passed through full cleanup: provider closure
  took about 0.7 seconds and browser closure took 17 seconds. This does not
  establish the cause of the earlier cleanup timeout.
- Both native providers pass the background-request browser smoke through the
  production registered chat IPC, SQLite journal, renderer hook, and transcript.
  No chat is mounted when the child request arrives. Opening and reloading the
  chat preserves the original request; a user submits typed values through the
  card, the real MCP server receives them, and the completed reply is visible.
  Codex's native empty-form tool consent is answered before the server form.
  A stricter Claude check also confirms the final reply is in the viewport
  after completion; the fixture scrolls back after filling the expanded turn.
- Rechecked the current Claude CLI login after credit restoration: real inference
  returned `CREDIT_OK`. Both providers' native background-request browser smokes
  pass with awaited startup. The initial Claude retry reported an MCP connection
  timeout; the next complete run passed. Failed runs now retain fixture transport
  traces alongside the conversation evidence.
- The restored login also passes a real question/selected-file write and four
  further live contracts: inference context across follow-up/restart/resume,
  model/effort selection and reset, and MCP form/URL response delivery.
  No credentials were changed. Evidence: `claude-credit-restored.json` and
  `claude-credit-contracts.json` in the owned qualification output directory.
- Native Codex goal UI proof passes through the full task view, production
  journal, SQLite, real Git/Crab, and installed app-server: New goal → bounded
  objective → file containing `native-goal` → visible native Complete status
  and accounting → Stop → clear through an initialized-only cold client.
  The passing run used 2,383 of 6,000 tokens. Goal control component proof also
  covers a rejected draft, pause/resume, budget-limit notification, clear,
  and a narrow viewport. An initial live fixture lacked its task-view diff route;
  native work/completion/clear succeeded, but the fixture failed its all-routes
  assertion. Adding the real diff route produced the passing complete run.
- Extended native goal proof also passes pause and resume while a real MCP
  form is pending. The current turn retains its slot while paused; another
  task stays queued until the goal/turn completes. The form response reaches
  the server, the file is written, Complete is visible, and the queued task
  then runs. After Stop and cold clear, an ordinary composer follow-up resumes
  the same native thread, appends to the file, and shows its completed reply.
  Evidence: `goal-native-ui-final2.json` and `native-goal-ui-controls.json` in
  the owned qualification output. The final fixture budget is 100,000 tokens
  to leave room for MCP discovery/context; the earlier 12,000-token run reached
  the native budget limit before the held form. The plain file-only goal above
  remains separately qualified with 6,000 tokens.
  Intermediate checks exposed an ambiguous unnamed status locator and a
  metadata read cancelled by cold startup. The status labels are now distinct,
  and provider replacement preserves the reader. Both complete reruns and
  targeted regressions pass. Cleanup phases are retained on failure.
- Live Codex lost-acknowledgement proof passes through an owned stdio proxy
  around the installed CLI. It drops the second `turn/start` response but lets
  the real native turn finish. A renderer action produces the exact file,
  retains one user message with an unconfirmed-delivery marker, displays the
  warning alongside the completed response, and preserves reviewing status.
  This fault-injection fixture is POSIX-only; the provider and file effect are
  real, while the transport fault is deliberate.
- The pinned [Codex event handler](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/app-server/src/bespoke_event_handling.rs)
  aborts pending per-thread requests at turn start and completion. The human
  interaction fixture requests input during MCP tool execution. Immediate
  startup transport checks do not prove that a pre-turn request can wait for a
  human; Crab honors native cancellation rather than delaying turn transitions.
- Live MCP URL consent round trips pass through both native providers: a real
  fixture server requests a URL, receives acceptance without form values, and
  the model turn completes. This proves transport/user-decision handling, not
  an external OAuth callback. Browser tests separately verify full destination
  disclosure, explicit opening, and distinct decline/cancel actions. Codex's
  [pinned core handler](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/core/src/session/handlers.rs#L149)
  turns accepted absent content into `{}`; Claude preserves omission. The
  fixture asserts each native contract without changing production responses.
- Live child-task proof passes for both installed providers through the real
  orchestrator: exact file creation, child-only event ownership, completed task
  status, durable provider identity, and awaited native shutdown. The parent
  stays unchanged. Renderer interaction proof separately checks child approvals,
  completion/failure status, and the shared-workspace diff action.
- Live Codex context-resume proof passes: three real inference turns, native
  settings reset on follow-up, then process restart and recall of a generated
  token absent from the new prompt, on one thread identity. The resumed thread
  now runs in a different owned workspace and writes the remembered token there;
  the original workspace receives no file. This qualifies explicit resume cwd.
- Live Claude context-resume proof passes: a real inference turn stores a
  generated fixture token, a local `/mcp` turn exercises native control resets,
  then a restarted process recalls that token through `--resume` without seeing
  it in the new prompt. All three turns complete on the same session identity.
- Live Codex proof passes: real file creation, one completed renderer turn,
  UTF-8 filesystem write/read/copy, metadata, listing, removal, and a streamed
  PTY command with a visible successful exit. The test answers requested fixture
  approvals through the same adapter API as the UI. A prior run without that
  simulated user response timed out; subsequent runs with a response pass.
- Live Codex native-control proof passes: native plan mode makes no file change,
  then switching to default mode and resetting model/effort produces the exact
  requested file on the same thread. Both turns visibly complete. Earlier
  qualification attempts hit premature initialization/thread-start deadlines;
  these fixed transport cutoffs have been removed. Delayed-reply regression
  cases now verify that native startup can finish under its caller's lifetime.
- Both installed CLIs return model catalogs without starting a user turn.
- Live Codex MCP proof passes: a real stdio server requests a form, the adapter
  emits the pending input, and the server receives `{count: 0, enabled: false}`.
  The fixture uses `approval_policy="on-request"`. With the inherited `never`
  policy the CLI declines the form before forwarding it, matching the pinned
  [upstream policy implementation](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/codex-mcp/src/elicitation.rs).
  Claude cancels startup/discovery-time requests locally; its real tool-call
  elicitation smoke now passes, including typed form delivery and turn completion.
- Restored credits were verified using the existing Claude CLI login. Real
  inference passes for exact file creation, streamed output, usage, a Write
  approval, and an actual AskUserQuestion answer that determines file contents.
  Native plan mode holds implementation until the next mode-changing turn;
  both turns complete on one session. A discovered model and supported effort
  run real inference, then explicit resets produce a second successful response
  on that same session. No credential switch was used.
- Real Claude startup-hook cancellation passes on macOS: the CLI and its hook
  process exit without opening a user turn. Renderer ownership tests cover
  cancellation, navigation, crash, closure, duplicate IDs, and stale cleanup.
- Desktop lint is blocked before source inspection: its ESLint 9 command has
  no desktop flat config. `origin/crab-v2` has the same missing config; the
  tracked ESLint config belongs only to `packages/web/`.
- The mounted workspace volume now passes a write probe in this worktree's
  external Cargo target directory. Sidecar builds and focused Rust tests use
  that target; no local target was created. Small non-repository live fixtures
  use an owned temporary directory.

## Remaining completion gates

- Extend startup/cancellation qualification to the remaining supported OSes;
  the current real-hook cancellation fixture uses POSIX command quoting.
- Expose live steering in the composer and qualify provider-turn
  accounting for queued input. Rejection,
  concurrent-send ordering, cancellation, latest accepted-input Retry, and
  interrupted input/checkpoint bookkeeping now have targeted proof. Saved-file
  restoration now preserves history and owns writer closure. Native conversation
  rewind still needs each provider's current API and native-turn/input mapping;
  file restoration does not stand in for that contract.
- Qualify restart through the packaged Electron main process, including orphaned
  provider cleanup. The POSIX crash fixture owns and closes its detached process
  group before reopening metadata; that does not prove packaged orphan cleanup.
  Normal quit now owns the orchestrator and discovery processes. Full packaged
  main-process lifecycle qualification remains required.
- Resolve the isolated Codex archive visibility failure before claiming stable
  lifecycle qualification. A later checkpoint-failure task-view run also lost a
  native Codex rollout after successful cold review: the saved parent ID stayed
  unchanged, but native Retry returned `no rollout found`. Claude's equivalent
  checkpoint-failure run passes. Rollout existence, follow-up cancellation, and
  request traces are retained alongside browser traces for diagnosis.
- Finish native recovery semantics. Saved-file restoration and text-hunk ownership
  now have real file/browser proof. Turn-summary Undo still
  needs native provider-shape qualification: Codex unified diffs and Claude
  writes without an old-file preimage cannot be treated as exact string edits.
  Replace that reconstruction with canonical recovery semantics; text-hunk
  checks do not establish full native rewind or atomic turn restoration.
- Extend local script ownership to workflow-runner shell nodes and qualify
  Windows descendant/orphan ownership. Local setup/Run/archive processes now
  share the orchestrator's Archive/Discard/Restore/Apply/quit boundary on POSIX.
  Workflow scheduling and independent shell executors still require their own
  shutdown integration; local process-group proof does not establish that.
- Resolve historical cost files lacking provider session/model baselines before
  claiming accurate resumed accounting for pre-fix histories.
- Resolve remote-agent availability. Electron sends `task_spawn`/`task_iterate`,
  but the Rust sidecar dispatcher has no implementations. Local-provider proof
  does not qualify SSH agent chats. Remote comparison now has compiled-sidecar,
  RPC, and task-view proof over stdio; physical SSH deployment and remote
  baseline ownership still need qualification. Channel-local RPC settlement and
  request transport now have real SSH/browser proof. Production main creates the
  session manager without its deployment dependencies, and no later wiring sets
  them. The manager's reconnect path does not redeploy or replay the router's
  read queue; its separate unused queue resolves fabricated replay metadata.
  Manual deployment also discards the returned channel. Capabilities negotiation
  uses a missing sidecar method and routes while status is `deploying`, which the
  router currently rejects. Fix those owner/contract boundaries, cancellation
  and late-client events, and Git-filter descendant cleanup before claiming
  remote support. Router replay admission now rejects unclassified/mutating
  calls; manager readiness and actual queue replay remain unfinished.
- Finish live watch/command-control checks and the lifecycle/owner contract for
  extended controls that can execute or write outside ordinary chat submission.
- Run appropriate scoped verification, build changed JavaScript bundles, inspect
  the final diff and non-test size, and record broad/packaged CI evidence or the
  exact remaining qualification gaps. A draft PR has been requested for the
  accumulated fixes, with continued release-gap work. No merge or release
  publication has been requested.

Run live checks only in an owned scratch directory:

```sh
CRAB_AGENT_LIVE_ROOT=<absolute-scratch-directory> npm test -- tests/agent-chat-live.spec.ts --maxWorkers 1 --minWorkers 1
```

The test uses the installed agents and their existing authentication, performs
real inference, writes only inside its temporary fixture, and removes the
fixture afterward. It skips unless the scratch-directory variable is provided.

The normal-quit fixture launches a real Electron main process against the built
owners. Build them first so this check exercises current source:

```sh
npm run build:electron
CRAB_AGENT_LIVE_ROOT=<absolute-scratch-directory> npm test -- tests/agent-shutdown-live.spec.ts --maxWorkers 1 --minWorkers 1
```

This fixture currently qualifies macOS/POSIX shutdown; it does not replace the
packaged Crab restart and supported-OS gates above.

The script-owner checks use real shell groups and the installed Crab binary:

```sh
CRAB_BIN=<installed-crab-binary> CRAB_AGENT_LIVE_ROOT=<absolute-scratch-directory> npm test -- tests/workspace-scripts-live.spec.ts --maxWorkers 1 --minWorkers 1
```

The request-card browser checks run separately from packaged smoke:

```sh
npm exec playwright -- test --project agent-requests --workers 1
```

They use the existing Vite test server and an isolated installed-Chrome profile.
