# Agent chat compatibility

Status: in progress; [PR #670](https://github.com/crabbuild/crab/pull/670). Base: `origin/crab-v2` at `e60de177853`.
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

The PR was subsequently rebased onto `54e5120f134` (Changes sidebar reflow).
That update touches only its guide, StagingZones and CommitComposer. The two
published audit commits rebased without conflicts; all 14 in-flight SSH-owner
files matched their preserved hashes after restoring the patch. Responsive
Changes-page edits enter through the base branch, not the audit commits.

The audit is now rebased onto `e60de177853`, integrating terminal persistence
and desktop CI fixes (#672 and #669). Three conflicts were resolved by keeping
shared settings read/write helpers, the terminal resume/detach bridge and CI
failure artifacts alongside native conversation ownership, host trust and the
remote runtime job. Of the 182 preserved audit file hashes, 174 remain exact;
the eight changed files incorporate those inspected upstream paths. Native
runtime and frontend builds were repeated because the base changes Rust storage,
repository initialization and terminal composition. All 372 scoped checks across
17 files pass, including settings and terminal ownership. The terminal lifecycle
fixture now owns its shell home: personal startup files produced an NVM warning
and prevented its foreground marker; the controlled-home rerun passes. The
rebuilt pair passes ten real SSH/SFTP cases and four SSH task-view cases, and
the focused Rust cache ownership test passes on the integrated source. Production
types, Electron/renderer/native builds, workflow parsing, Rust formatting and
diff checks pass. Evidence: `e60-scoped-final.json`, `e60-native-ssh.json`,
`e60-diff-live.json`, `e60-indexer-rust-test.log`, `e60-native-build.log` and
`e60-terminal-owned-home.json`. Broad/cross-OS CI is still required.

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
- [OpenSSH known-hosts format](https://man.openbsd.org/sshd#SSH_KNOWN_HOSTS_FILE_FORMAT),
  [hashed-host lookup](https://man.openbsd.org/ssh-keygen#F),
  [default trust databases](https://man.openbsd.org/ssh_config#GlobalKnownHostsFile),
  [Electron dialog ownership](https://www.electronjs.org/docs/latest/api/dialog#dialogshowmessageboxwindow-options), installed SSH2
  `1.17.0` `client.js`/`protocol/kex.js`, and Electron `33.2.0`
  `MessageBoxOptions`: asynchronous host verification must be supplied explicitly.
  No verifier means acceptance. The SSH2 handshake timeout includes the user
  decision unless the caller owns that deadline. Electron's abortable native
  dialog needs a parent window on macOS. Crab checks its app pins plus default
  OpenSSH user/system files; it does not implement OpenSSH certificate-authority
  authentication or custom ssh_config trust-file options.
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
| Remote file comparisons | Same IPC → `RpcRouter` → sidecar dispatcher → `agent/src/task_diff.rs` | Private index, one bounded Git read, tagged response shape, shared path decoding; compiled sidecar and task-view checks over stdio and loopback SSH; actual CLI/sidecar deployment proof below |
| SSH session readiness | Main dependency factory → session manager → deployer/channel → capability negotiator → router | Initial/manual/reconnect share attachment; real sidecar capability response, interrupted tasks, bounded cancellation, profile-owned jump chains and four SSH/browser cases |
| RPC channel ownership | `RpcRouter.attachChannel` → channel-local pending calls/decoder → sidecar NDJSON requests | Cache/poller batches use ordinary routing; closure/replacement, profile isolation, split UTF-8, error fidelity, timeout and queue-admission regressions; real SSH browser failure during an outstanding read |
| Extended Codex actions | `electron/orchestrator-ipc.ts` → orchestrator → adapter | Feedback, preload API and operation contracts; remaining command/watch ownership and live controls are gates |
| Live behavior | `tests/agent-chat-live.spec.ts` | Installed CLI → real file write → renderer turn derivation |

Paths in the table are relative to `packages/desktop/`. Current `origin/main`
does not contain the desktop adapter; sibling comparisons use `crab-v2`.

## Corrections implemented

- SSH initial connect, manual deployment and reconnect now share one channel
  attachment and capability handshake. Main supplies the dependencies, with a
  fresh deployer per attempt so progress cannot cross profiles. Both manual IPC
  implementations use the session owner. Readiness is published only after
  negotiation; auto-deployment failure rejects instead of publishing a partial
  agent. Explicit agentless profiles remain available for SSH/SFTP and advertise
  no repository RPC capabilities until manual deployment succeeds.
- Each connection owns its cancellation signal and jump chain. Replacement,
  disconnect and the total 60-second reconnect deadline retire in-flight work;
  a late client or deployment cannot resurrect the profile or replace its
  channel. Opening EOF rejects immediately. Router queues are the only read
  queue; the unused fabricated replay path is removed. Successful reconnect
  actually sends queued reads, and permanent disconnect rejects them.
- Capabilities validate the response and reject retired-generation replies.
  The sidecar now implements the capability handshake, truthfully reporting its
  unimplemented extended SSH service flags as false. This does not advertise
  native chat dispatch. Reconnecting interrupts remote tasks before recovery;
  all affected siblings change state before persistence yields, and obsolete
  interruption notifications are not published after recovery advances.
- Remote reconnect admission now defers only the six inspected cacheable reads
  and the inspected `task_compute_diff`, `git_stash_list` and `git_blame` reads.
  Mutating and unclassified methods reject immediately
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

- The remaining seven-file CI scope reproduced 13 failing assertions and a
  process mock that lacked Node's close lifecycle. The real routing and
  readiness source fixes are retained. Fixtures now check typed reconnect
  errors, capabilities only after negotiation, one NDJSON request per batch
  member, queued startup acknowledgement after capacity is released, and
  cancellation closure. Removed mock-only `crab_status` coverage and local-Git
  checkpoint expectations for remote paths; neither establishes a sidecar
  implementation. The corrected scope passes 170 tests, with no unhandled
  rejections, and its strict fixture type probe passes after removing the stale
  `isWriteMethod` import. Remote native dispatch and remote snapshot ownership
  remain implementation gates. Evidence: `ssh-remaining-ci-before.json`,
  `ssh-remaining-ci-final.json` and `ssh-ci-fixture-types-final.log`.
- Linux ARM64 CI job `129658665861` failed at the first build-script link:
  repository Cargo configuration names a cross linker that is absent on the
  native runner. The desktop ARM64 job now uses the native compiler overrides
  already used by the release job. [Cargo's configuration contract](https://doc.rust-lang.org/cargo/reference/config.html#environment-variables)
  gives the linker environment override precedence over TOML. YAML parsing passes;
  the native ARM64 build and bootstrap rerun is still required.
- A two-process native probe reproduced SlateDB writer fencing in the shared
  account-wide indexer cache. Both processes reported the same database path;
  closing the first emitted a `Fenced` background-task error. The actual SSH
  fixture also fails its new independent-cache assertion on the prior binary.
  The indexer now owns a private temporary derived cache, closes its database
  before removing the directory, and retires the unused shared-path/default
  configuration. Cache contents rebuild per sidecar lifetime. Its status wire
  fields remain the same; the Health card displays the actual current path.
  The focused Rust ownership/cleanup test passes. The rebuilt release pair
  passes all ten deployment checks, including two native sidecars, retirement
  of one while the other remains usable, and a replacement after reconnect.
  Both recorded cache paths are removed after shutdown, all owned processes
  exit cleanly, and the captured Rust diagnostics contain no errors. The four
  real SSH task-view cases also pass with the new default cache owner.
  Evidence: `indexer-concurrent-before.json`,
  `indexer-concurrent-ssh-before.json`, `indexer-owner-rust-test.log`,
  `indexer-concurrent-ssh-final.json`, `indexer-concurrent-ssh.json` and
  `indexer-owner-diff-live.json`. The release build, strict fixture types,
  workflow YAML parsing, Rust formatting and diff checks pass. Cross-platform
  CI remains required.
- SSH deployment now resolves a verified Crab CLI/sidecar pair for the exact app
  release and remote target. Development uses the real `resources/native` layout;
  packaged applications download dedicated release assets over approved HTTPS
  hosts into a private cache. Concurrent profiles share a download, with
  cancellation retained by remaining borrowers. Corrupt completed caches fail
  closed with an explicit removal/reconnect instruction. No provider credential
  or new product configuration is used for artifact download.
  The pair and its Git helper install under
  `~/.crab/desktop/runtimes/<version>/<target>/<content-hash>/`. Uploads wait for
  the SFTP CLOSE acknowledgement, verify regular executable files and their
  remote checksums, then publish the directory. A remote stdin-owned guard
  removes only its own upload directory on interruption. Existing running
  sessions keep their executable; version guessing and global process checks
  are removed. The remote sidecar uses its matching uploaded CLI explicitly.
  One session-owned capability handshake completes deployment; progress is
  typed and scoped to the selected profile and includes runtime preparation.
  Real SSH/SFTP checks cover install, reuse, corruption, cancellation and safe
  quoting. OPEN, WRITE and CLOSE stalls fail after 30 seconds without progress;
  the fixture confirms that the owned staging directory is removed. The server
  explicitly acknowledges SSH channel closure, matching the SSH2 subsystem
  owner contract; writable finish alone is not an SFTP CLOSE acknowledgement. The just-built release pair also passes through the real session
  manager, capability negotiator and RPC router: it creates a Git worktree with
  the uploaded CLI, returns native Diff bytes and exits cleanly. Its download
  transport serves owned build assets; it does not claim a published GitHub
  endpoint exists before release. A browser case checks profile isolation and
  retained-state filtering in the actual progress component.
  Release staging emits Linux/macOS x64/arm64 manifests and both binaries.
  Assembly rejects missing, corrupt or mismatched runtime assets. CI now builds
  Linux ARM64 and exercises native SSH bootstrap on each advertised remote
  target. Cross-platform CI, Linux ABI/minimum-distro qualification, packaged
  main-process bootstrap and real published-asset checks remain release gates.
  Evidence: `ssh-runtime-layout-before.json`, `ssh-runtime-sharing-before.json`,
  `ssh-runtime-final-scoped.json`, `ssh-upload-stall-final2.json`, `ssh-native-runtime-live.json`,
  `ssh-native-runtime.json`, `ssh-runtime-final-live3.json`,
  `ssh-runtime-progress-final.json` and `ssh-runtime-release-suite-final.log` in the owned qualification output.
  The JSON reporter's totals include skipped opt-in cases; validation counts
  use individual assertion statuses. The preceding connection-test scope was
  167 passes plus one skipped browser case; its separate seven-case live run
  passes in full. The runtime scope passes 187 assertions with three opt-in
  cases skipped. A later native deployment run passes ten deployment cases;
  its progress browser case hit the 30-second fixture deadline. Restricting
  Vite dependency discovery to that fixture page removes unrelated HTML
  entry scans, and the separate browser rerun passes. Four SSH task-view
  cases, 27 release/build checks, production/fixture types, Electron/renderer
  builds, workflow YAML parsing and Rust formatting also pass. Broad CI run
  `42752026672` builds and smokes the Linux package but reports 21 regression
  failures (routing, capability defaults and remote task lifecycle); these
  remain under investigation and must not be reported as a green gate.
- Broad CI exposed two source regressions: the reconnect allowlist omitted
  supported worktree/staging/pointer/forge reads, and remote startup repeated
  its running transition and reset the timeout already owned by `launchTask`.
  Fifteen new repo/profile/batch read cases fail on the prior source, then pass
  after inspecting the Rust handlers, read-only staging CLI, worktree list path
  and all four forge providers' HTTP GET implementations. Mutation and unknown
  methods retain immediate rejection; unsupported `crab_status`, cancellation
  replay and array-batch transport are not restored to satisfy older mocks.
  Five existing routing/lifecycle suites pass after the source corrections.
  The native reconnect case then reproduced `repository not open` on a new
  sidecar. The router now retains only acknowledged local workspace opens, drops
  acknowledged closes, and restores that session state before readiness and
  queued reads. Endpoint changes forget old identities; late restores cannot
  write into a replacement channel. The production open handler delegates to
  the existing file browser, supplies Rust's required `repo_id`, and returns
  an SSH handle so renderer worktree opens stay remote. The real fixture uses
  this same open path, creates a worktree, drops the transport, reuses verified
  binaries and receives the queued worktree list from a replacement sidecar.
  This remains session-state restoration, not replay of uncertain mutations.
  Evidence: `ssh-read-siblings-before.json`, `ssh-read-siblings-final.json`,
  `ssh-repository-reopen-before.json`, `ssh-repository-reopen-native-final.json`
  `ssh-reopen-exact-source-native.json` and `ssh-reopen-final.json`. The final
  seven-file scope passes 178 tests;
  ten real SSH/SFTP native cases pass, including reconnect. Production and
  fixture types, the Electron build, Rust formatting and diff checks pass. Broader CI is still a release gate; older
  mocks asserting optimistic capabilities, unsupported remote checkpointing,
  cancellation replay, array-batch transport and pre-owner admission timing
  need contract review alongside the remaining source gaps.
- Test Connection now owns its authenticated command until channel closure.
  Its five-second deadline includes opening the exec channel; timeout, transport
  loss, stream errors, failed exit/signal and excessive output fail the test and
  close its transport. A late channel is retired. Success requires an exact
  `ok` output line; SSH2's optional exit status remains optional, while a reported
  failure cannot be overridden by stdout. Five regressions fail before the fix.
  The form/preload/type bridge now consumes the backend's single `{ ok, error }`
  result contract. The only reachable renderer bridge caller is ConnectionForm;
  the pre-existing QuickStartWizard has no callers and remains outside this path.
  Real SSH and registered IPC feed the actual form: a hung remote command renders
  timeout, closes its connection, re-enables Test Connection, then a second test
  renders success and closes its new transport. Scoped command/SSH tests and the
  separate live form case pass. The bounded command owner adds source lines to
  replace the previously unbounded callback; no helper, setting or fallback is
  introduced. Evidence: `ssh-test-command-before.json`,
  `ssh-test-command-final.json`, `ssh-test-command-live.json`, and the timeout/
  recovery screenshots in the owned qualification output.
- SSH host verification now runs before authentication for direct connections,
  every ProxyJump hop, reconnect and Test Connection. A connection-owned native
  Electron dialog rejects by default; replacing a changed key also requires
  explicit acknowledgement. Approval holds the exact presented key and saves it
  before authentication; cancellation or changed trust records cannot grant a
  late approval. Revocations take precedence across application, user and system
  records. OpenSSH hashed endpoints, wildcard negation and nondefault-port
  identities are checked; both default `known_hosts2` siblings participate in
  revocation checks too. Malformed/unreadable trust records fail closed.
  The old unconnected store's events, randomart and positive-only fallback are
  removed. Application approvals are listed and removed through typed settings
  IPC. User/system OpenSSH files remain read-only.
  The SSH2 handshake deadline pauses during the decision and resumes with its
  remaining network budget. Trust failures stop automatic reconnect, including
  rekey failures. Scoped owner/policy/router/provider tests pass; five real SSH
  cases cover approval, rejection before authentication, changed/revoked keys,
  Test Connection and a two-hop chain. A separate parented native-dialog fixture
  verifies rejection, approval and acknowledged key replacement through real
  authentication, a remote marker write and a visible result. An OpenSSH
  `ssh-keygen` oracle agrees on hashed endpoints and SHA256 fingerprints.
  Four compiled-sidecar/browser cases still pass with the real trust policy.
  Evidence: `ssh-host-final-tests.json`, `ssh-host-ipc-tests.json`,
  `ssh-host-native-result.json`, `ssh-host-openssh-oracle.log` and
  `ssh-host-sidecar-live.json` in the owned qualification output. Packaged and
  cross-OS native dialog qualification still require CI/dedicated environments.
- SSH readiness qualification adds seven regressions that failed against the
  previous manager, then passes 202 tests across ten manager/router/capability/
  recovery and adjacent files.
  Four task-view cases run the real session manager, router, capability
  negotiator, compiled sidecar and Git over authenticated loopback SSH. Breaking
  the channel causes a second sidecar and a fresh handshake; the queued file
  refresh renders the changed bytes. Cancellation, late clients/channels,
  credential lookup, manual deployment, permanent queue failure, concurrent
  jump chains and stale capability replies have scoped proof. Eight Rust
  comparison tests still pass after the real capability handler is added.
  Production types, strict fixture types, both JavaScript builds, Rust format
  and diff checks pass after the rebase. Production code shrinks by 145 lines.
  The live fixture supplies its already-built sidecar through a startup-only
  deployer; it does not qualify binary lookup, upload, host trust or remote
  native dispatch. Ephemeral SSH keys stay outside the Git fixture and are
  removed. The indexer sandbox remains enabled. Evidence:
  `ssh-manager-owner-before.json`, `ssh-owner-final.json`,
  `ssh-owner-live-final.json`, and `ssh-owner-rust-tests.log`.
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

- Single-file Revert now uses the same workspace mutation owner as hunk edits
  and saved-file restoration. It drains shared native owners, post-turn writers,
  and Run descendants; checks the live comparison and workspace identity; and
  retains a pre-edit backup before changing bytes. Rename sources come from
  Git's live comparison. Modified/deleted files, tracked/untracked additions,
  and literal Unicode paths restore without moving HEAD or changing unrelated
  staging. Replaced directories, symbolic-link parents, ignored collisions,
  and failed backup publication reject before mutation. The renderer checks
  IPC success, shows failures without dropping the row, prevents duplicate
  submissions, and reloads the actual comparison after success. Historical
  snapshot views do not offer a live-baseline Revert action.
  The browser first reproduced the silent resolved-failure response; a real Git
  probe also reproduced the old checkout failure for untracked additions.
  Qualification: 267 scoped owner/checkpoint/comparison tests, seven renderer
  cases, and six real Run-descendant cases pass on macOS; typecheck and both
  JavaScript builds and strict touched-fixture types pass. Owned-worktree backup recovery is exercised through
  the renderer and real files. This does not qualify provider-native rewind or
  full restoration of a user's primary folder. Evidence: `revert-before.json`,
  `revert-git-contract.json`, `revert-owner-tests.json`,
  `revert-renderer-final.json`, and `revert-real-writers.json` in the owned output.
  The path behavior follows [Git restore](https://git-scm.com/docs/git-restore)
  with [literal pathspecs](https://git-scm.com/docs/git#Documentation/git.txt---literal-pathspecs).

- Remote task creation qualification reproduced deletion of unrelated bytes in
  the old eight-character task directory through the real SSH/runtime owner.
  Creation now validates UUIDs, resolves the requested commit strictly, reserves
  a full-UUID path atomically, and never removes an existing path. It releases
  only an empty reservation after failure; partially created checkouts remain
  available for recovery. Missing HOME fails rather than using a shared temp
  directory. Both Electron callers send full identities and retain the returned
  branch/SHA; the original branch selection stays the Apply target. The branch
  picker supplies local branch names, so removed origin/HEAD fallbacks are not
  needed to resolve its selections. The tagged alpha's returned paths are opaque:
  new paths can change without relocating old saved workspaces or adding readers.
  All 234 scoped orchestration checks and ten real SSH/SFTP cases pass. The native
  checks cover shared-prefix IDs, existing bytes, simultaneous duplicate requests,
  invalid UUIDs/refs, failed-creation cleanup, and a real Orchestrator task whose
  persisted baseline stays fixed after HEAD moves. Native/Electron builds,
  production and touched-fixture types, Rust formatting and diff checks pass.
  The fixture owns its settings path. Its log assertion reads the sidecar's
  actual severity rather than treating a CLI ERROR serialized inside a WARN as
  an indexer failure; root sidecar ERRORs still fail. Evidence:
  `task-workspace-before.json`, `task-workspace-scoped.json`,
  `task-workspace-native-final.json`, and `task-workspace-creation.json` in the
  owned output. The creation handler exists in `v2.0.0-alpha.1`; missing native
  dispatch is specifically `task_spawn`/`task_iterate`. Creation proof does not
  qualify `worktree_destroy`, `task_discard`, Apply cleanup or process-name orphan
  checks. Those sibling lifecycle paths require ownership before release.
  Strict commit resolution follows [Git rev-parse](https://git-scm.com/docs/git-rev-parse).

- Native process transport now reuses the existing Codex app-server and Claude
  Code adapters over raw SSH stdio. A private Unix sidecar service owns UUIDs,
  independent 16 KiB output credit, acknowledged chunked stdin, process groups,
  physical stop receipts and release. Every process captures its exact channel;
  channel replacement rejects unknown closure and cannot inherit old writes.
  Server login supplies credentials and PATH; local environment overrides never
  cross the transport. Local native adapters keep the existing spawn defaults.
  EOF and TERM/HUP/INT retire supervisors before sidecar exit. SIGKILL/crashes,
  session-detached descendants and persisted orphan ownership remain unqualified.
  A live Codex turn first exposed macOS EPERM during group retirement. Apple's
  [XNU signal implementation](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_sig.c#L1612-L1621)
  excludes zombies before checking permissions. A real zombie regression proves
  bounded waiting for ESRCH; persistent permission errors still reject closure.
  This follows Tokio 1.53.3's inspected `process_group`, `wait` and `kill_on_drop`
  contracts; dropping a child alone cannot prove group closure. Transport frames
  are never canceled halfway through the shared NDJSON writer.
  All 182 scoped adapter/router/process tests, six focused Rust process tests,
  ten real SSH/SFTP runtime cases, and two real SSH terminal cases pass. All
  four opt-in native SSH cases pass with Codex 0.163.0 and Claude Code 2.1.319:
  model catalogs, exact file-writing turns, native approval responses and
  graceful owner termination. The existing Claude CLI login supplies its credit;
  no credentials changed. The SFTP probe also round-trips Unicode/NUL stdin
  through the actual deployed runtime. Native/Electron builds, production and
  touched-fixture types, formatting and diff checks pass. The terminal fixture
  forwards non-PTY stdin and publishes exit after stdio closure. Its regression
  run uses the owned tmux bottle's libraries and terminfo database.
  Evidence: `native-process-scoped-final.json`, `native-process-zombie-rust.log`,
  `native-process-ssh-zombie-final.json`, `native-process-terminal-owned.json`,
  `native-process-live-zombie-final.json`, and provider native-transport artifacts
  in the owned output. This foundation is not wired into remote task UI yet:
  `task_spawn`/`task_iterate` remain absent. Shared startup, snapshots, resume,
  cancellation and Apply/Discard are the next integration gate. The unrelated
  extended `processManagement` service capability remains disabled.
  ARM64 CI on the preceding head passed native build/tests but timed out in
  two SSH cases; that gate remains open. Its plain Node qualification no longer
  imports Electron SQLite: the actual persisted-task probe remains on Electron,
  while every remote runtime/workspace check runs on both runners.

- The remote cleanup follow-up replaces path guessing and filesystem deletion
  after Git failure with one disposable-workspace owner. Creation writes a
  private Git-administration record containing the task UUID, branch and
  canonical root/common/private Git directories. Apply, Discard and Destroy
  verify that identity against the opened repository. Native processes acquire
  shared file leases; cleanup takes an exclusive lease. A factory carrying a
  task UUID requires its owner even after `.git` disappears, preventing an
  unowned spawn in a recreated directory. Confirmed group closure releases the
  lease before its receipt; uncertain closure retains it while the sidecar lives.
  The implementation uses [Rust's standard file locks](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock),
  available before the workspace's Rust 1.97 minimum. No dependency changes.
  The core CLI's existing worktree removal contract remains authoritative;
  [Git's locked-worktree refusal](https://git-scm.com/docs/git-worktree#Documentation/git-worktree.txt---force)
  cannot trigger recursive deletion or account-wide pruning. Apply now propagates
  status/staging/commit failure before merging. Confirmed merges and removals
  retain their result when later cleanup fails, with explicit flags and a
  `cleanup_error` consumed by both Electron callers. Failed, missing-owner or
  unacknowledged Discard does not become a discarded task. The existing IPC,
  task-operation handler and agent error subscription display those failures.
  The secondary remote Apply caller now supplies the required worktree path.
  The first real SSH regression resolved Discard successfully for an unrelated,
  unregistered directory. The rebuilt runtime rejects that request and preserves
  its bytes. All ten real SSH/SFTP cases pass, including wrong task/branch/root,
  cross-sidecar native writer exclusion, confirmed closure then cleanup,
  recreated-directory spawn rejection, successful Discard/Destroy and a real
  merge whose locked worktree remains intact. All 307 scoped checks and eight
  focused Rust checks pass, including retained bytes and unchanged target HEAD
  after a pre-commit rejection. Production/fixture types, native/Electron builds,
  formatting and diff checks pass. Evidence: `remote-cleanup-before.json`,
  `remote-cleanup-native-final.json`, `remote-cleanup-native.json`,
  `remote-cleanup-tests-commit.json`, `remote-cleanup-rust-final.log`, and
  `remote-cleanup-native-process-tests.log` in the owned output.
  Full remote task startup/snapshots/cancellation remain open. Before release,
  capture the Apply target durably, persist mutation recovery receipts, provide
  explicit recovery for older/moved ownership records, and own mutating Git
  subprocesses through canceled RPCs and backend exit. Cooperative leases do
  not establish ownership of external editors or detached/crashed descendants.

- Local Discard now uses the saved task path, UUID, recorded branch and live Git
  identity through one worktree owner. Removed friendly-name lookup, path scans,
  error-phrase success handling and opportunistic Git pruning from removal.
  The existing root/common/private-index validation moved from checkpoints to
  the worktree boundary and remains shared by snapshot restore and file Revert.
  The tagged source-metadata migration remains explicit. Git refusal leaves the
  task stopped with its bytes and recovery refs intact. Snapshot cleanup follows
  confirmed directory removal. Retained branches or refs produce a warning while
  the task records the completed discard. An absent checkout cannot authorize
  deleting a branch from an old name; surviving refs stay available for recovery.
  Archive uses the same identity check, captures the live branch after writer
  closure and preserves it. A real changed-branch archive/restore/discard cycle
  proves that intentional caller difference. Retention pruning does not report
  success while branch cleanup needs attention; its caller supplies each task's
  saved path and identity.
  Private checkpoint/base/intent/backup writes now use `--no-deref`; cleanup also
  supplies each captured old object ID. This follows
  [Git update-ref's direct-ref and old-value contract](https://git-scm.com/docs/git-update-ref),
  preventing a symbolic private ref from changing or deleting a user branch and
  preserving concurrent ref updates. Five real-Git cases protect that invariant.
  All four new Discard cases failed before the fix. The current source passes
  307 scoped checks and all 27 owned-workspace live cases, including real branch
  and snapshot ref locks, a locked checkout, recreated-directory rejection,
  shared native/script closure, recovery and changed-branch Archive. Production
  and touched-fixture types, Electron build and diff checks pass. Non-test source
  shrank as removal and identity ownership replaced the duplicate paths.
  Evidence: `local-discard-before.json`, `local-discard-scoped-second.json`,
  `local-discard-live-archive-final.json`, and the matching type/build logs in the
  owned output. These checks do not establish durable mutation receipts across
  process death or task-database persistence failure; that remains release work.

- Local native providers now start in an owned POSIX process group. Both adapters
  signal that group and publish physical closure only after the group disappears
  and stdio closes. Provider exit retires remaining members, including children
  that ignore TERM or retain inherited stdio. This differs intentionally from
  managed Run scripts: a background Run remains usable after its shell exits;
  a conversation cannot use a dead provider. Unknown closure rejects without an
  adapter exit event, retaining the writer at destructive workspace boundaries.
  Denied Darwin probes remain uncertain until ESRCH; bounded failure preserves
  its source error. Released groups cannot receive later signals from this owner.
  [Node's exit/close distinction](https://nodejs.org/docs/latest-v22.x/api/child_process.html#event-close),
  [POSIX detached sessions](https://nodejs.org/docs/latest-v22.x/api/child_process.html#optionsdetached)
  and [leader-only kill semantics](https://nodejs.org/docs/latest-v22.x/api/child_process.html#subprocesskillsignal)
  define this boundary. Protocol-only mocks no longer invent OS process IDs;
  real fixtures qualify the physical owner separately.
  Both new real-process cases failed before the fix. Current macOS proof passes
  108 scoped checks, 29 process/workspace cases, six native provider cases and
  two native shell-writer cancellations. Both installed providers stop actual
  TERM-resistant parent/child writers before cancellation resolves. Codex review
  cancellation, Claude startup-hook cancellation, model catalogs and exact file
  writes remain verified with the existing CLI logins. Production/fixture types
  and Electron build pass. Evidence: `local-native-owner-before.json`,
  `local-native-owner-scoped-final.json`, `local-native-owner-live-final.json`,
  `local-native-owner-providers.json`, and `local-native-tool-cancel.json` in the
  owned output. CI runs the two inference-free owner cases on Linux and macOS.
  A process group does not contain every provider tool:
  [Codex tools can start their own groups/sessions](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/utils/pty/src/process_group.rs).
  Normal native interruption has writer proof; forced provider death, escaped
  descendants, packaged crash recovery and Windows job ownership remain gates.

- The main orchestrator now runs remote Codex and Claude conversations through
  the same native adapters, approvals, input receipts, journal and turn pipeline.
  Electron startup supplies the SSH router. Initial turns, warm follow-ups and
  cold session resume no longer call unsupported task-spawn/iterate/cancel RPCs;
  the old task-event bridge and its mock-only lifecycle tests were removed.
  Local binary paths, extra arguments and credential configuration stay local.
  The server inherits its process environment; snapshot Git receives only its
  private-index path and product-owned author assignments.
  Checkpoint refs share one policy across hosts. Remote capture copies the real
  index on the server, retains staged ignored additions and writes through a
  private index. Each operation captures its channel; replacement cannot move
  later commands or cleanup onto another owner. Bounded command capture awaits
  physical closure after timeout/output failure. Unknown closure retains the
  private index, and simultaneous snapshot/cleanup failures retain both errors.
  Post-turn Git commits execute on the workspace host. Remote Apply now awaits
  native closure; Discard cleans refs only after confirmed workspace removal.
  Parent workspace UUIDs, admission, settings controls, failed closure and
  snapshot refusal have scoped proof. All 395 scoped checks pass, including
  two resource-failure cases, as do 27 actual local workspace cases.
  Three real SSH cases exercise the built sidecar with existing CLI logins:
  private-index capture/publication/ref-lock refusal, and both providers' initial,
  warm and cold turns. They preserve native session IDs, publish checkpoints
  0/1/2, retain visible accepted input and exclude client-only configuration from
  native spawn requests. Codex Apply merges the expected remote file; Claude
  Discard removes the workspace and its refs after closure. Production/fixture
  types and Electron build pass. CI adds the inference-free checkpoint case.
  The [811924de411 process-owner gate](https://github.com/crabbuild/crab/actions/runs/42801653371)
  passes on Linux and macOS ARM64/Intel. Its Linux ARM64 job passed all 271 native
  tests but timed out in two SSH cases; full jobs and this new head remain gates.
  Evidence: `remote-native-scoped-final.json`, `remote-checkpoint-resources.json`,
  `remote-native-local-workspaces.json`, `remote-native-task-live-final.json`,
  `remote-native-checkpoint-live-final.json` and matching type/build logs.
  This is macOS loopback qualification. Remote availability/catalogs and server
  PATH discovery, scripts, goals, file restore/Revert, historical comparisons,
  durable endpoint/Apply-target recovery and packaged/cross-OS behavior remain
  release gates. The unused auxiliary remote facade is outside the production
  main-orchestrator path and still needs retirement of its unsupported calls.

- SSH connections now enable TCP_NODELAY at the shared ready boundary, including
  Test Connection, reconnect and jump clients. This follows OpenSSH 9.6's
  [socket policy](https://github.com/openssh/openssh-portable/blob/V_9_6_P1/packet.c#L2121-L2136),
  used for exec/SFTP as well as interactive sessions. ssh2 1.17 forwards the
  client setting to its underlying socket; loopback servers model the same peer
  policy. Linux ARM64 reproduced the five-second runtime-reuse timeout. With
  both peers configured it passes in 252 ms. A new 4 MiB transfer through the real
  session manager fails in 5204 ms when only the production setting is removed,
  then passes in 587 ms with the setting restored. The ordinary test budget is
  unchanged. All ten inference-free Linux SFTP cases pass; macOS also passes the
  built native-runtime case, 72 scoped checks and both real terminal cases.
  Production/fixture types, Electron build, Rust formatting and workflow parse
  pass. Evidence: `ssh-linux-control.json`, `ssh-linux-managed-large-before.json`,
  `ssh-linux-managed-large-fixed.json`, `ssh-linux-nodelay-final.json`,
  `ssh-nodelay-native-final.json`, `ssh-nodelay-scoped-first.json` and
  `ssh-nodelay-terminal-final.json` in the owned output. This reproduces and fixes
  the short ARM64 timeout; the 120-second built-runtime case still requires CI
  proof. That case now persists its last phase and bounded process diagnostics,
  and ARM64 CI retains diagnostics and built assets even after qualification
  failure. No timeout was increased and no failing check was bypassed.

- Model discovery now resolves saved task ownership before reading client
  configuration. New SSH chats use their qualified profile/repository handle;
  existing conversations supply the saved task ID, so a raw server path cannot
  start a local provider. Discovery shares chat/checkpoint process factories,
  including disposable workspace UUIDs and channel replacement fences. Remote
  catalogs inherit only server settings. Renderer cancellation, late-result
  fencing and app-quit closure remain shared across hosts.
  Four regressions fail before the fix. All 407 scoped checks pass, with 14 final
  discovery checks covering local task context, unsupported providers and
  request ownership. Seven browser cases pass, including the restored workspace's
  task ID reaching discovery. Real SSH IPC returns Codex's eight and Claude's five
  models in both repository and saved task contexts, with deliberately invalid
  client launch settings excluded and no user turn emitted. Production and
  touched native-fixture types, Electron/renderer builds, Rust formatting and
  diff checks pass. Evidence: `remote-model-before.json`, `remote-model-broad.json`,
  `remote-model-final.json`, `remote-model-browser.json`,
  `remote-model-native-live.json` and `catalogs-remote-native-task.json` in the
  owned output. Server PATH resolution and the remote agent-availability picker
  remain separate release gates; these catalogs qualify the native IPC path on
  macOS loopback with the existing server login.

- Agent availability now probes the selected SSH account through the same
  captured native process factory as chat. `command -v` checks the inherited
  server PATH, then the resolved executable supplies `--version`. Missing
  binaries are distinct from executable/transport failures. Version commands
  have bounded output/time and retain physical ownership through app quit;
  unknown closure refuses a successful shutdown. The unused detector's
  unsupported RPC/direct-exec fallback stack is removed.
  The renderer binds availability to the repository host, ignores old replies,
  clears it on disconnect and retries on reconnect. Local cached detection stays
  in the main process; the duplicate localStorage cache and retired command-name
  alias are removed. An SSH empty state names the server, surfaces errors and
  retries without directing users into local installation settings.
  The browser regression fails against the preceding view and passes after
  restoration. All 417 scoped checks and eight selected browser checks pass.
  Two real SSH/built-sidecar cases cover present/missing/broken server binaries,
  actual Codex 0.163.0 and Claude Code 2.1.319 availability, and both native
  catalogs in repository/task contexts. No unsupported detection RPC or client
  launch settings enter the server probe. Production/native-fixture/renderer-
  fixture types, Electron/renderer builds, Rust formatting and workflow parse
  pass. CI adds the inference-free availability case alongside checkpoints.
  Evidence: `remote-availability-scoped.json`, `remote-availability-broad.json`,
  `remote-availability-browser-before.json`, `remote-availability-browser-final.json`,
  `remote-availability-native-live.json` and matching native task artifacts.
  This qualifies the inherited server environment on macOS loopback. Minimal
  account/login-shell PATH setup and cross-platform deployment remain gates.

- Native Clippy's three fatal unwrap findings are resolved without suppressions.
  Review comments group into typed nested maps, preserving their JSON path/line
  keys and comment order. Repository size skips failed entry-type reads during
  its existing best-effort scan instead of retrying metadata with a panic.
  It continues to exclude symlinks, unlike the separate cache-size helper;
  [DirEntry's contract](https://doc.rust-lang.org/std/fs/struct.DirEntry.html#method.file_type)
  explains the platform-dependent metadata lookup and non-following behavior.
  Current-source Clippy reproduces all three errors before the fix and passes
  afterward; 164 existing sidecar warnings remain. Six forge tests and the new
  symlink-boundary test pass. The rebuilt sidecar's actual RPCs return an exact
  37-byte size delta while excluding a loop symlink, then preserve grouped
  Unicode-path comments across two files and multiple lines through an owned
  HTTP forge fixture. It closes with exit zero. Rust formatting and diff checks
  pass; no dependency or lockfile changed. Evidence: `native-clippy-before-current.log`,
  `native-clippy-fixed.log`, `native-size-test.log`, `native-forge-test.log`,
  `native-clippy-agent-build.log` and `native-clippy-wire-smoke.json` in the owned
  output. This removes the fatal Clippy blocker, not the remaining warning debt
  or the desktop ESLint configuration gate.

- The [d444f4061f2 desktop run](https://github.com/crabbuild/crab/actions/runs/42818674675)
  passes Linux ARM64's 271 native tests and all eleven SSH runtime cases,
  including both former timeouts. Linux x64 also passes 271 native tests, the
  remote checkpoint case, packaged smoke and all eleven SSH cases. Its broad
  run reaches 4042 passing checks but fails nine assertions in two old test
  files. Eight exercise the deleted task-spawn/notification path; the sidecar
  never implemented those task/script RPCs. Those obsolete integration tests
  are removed, while the manual archive request-mapping checks remain. Resume
  coverage now verifies persisted admission, saved workspace/session identity
  and delegation to the shared lifecycle. Actual native initial/warm/cold turns
  retain their separate SSH proof. All 47 remaining checks in those two files
  pass. Evidence: `retired-remote-rpc-before.json` and
  `retired-remote-rpc-final.json` in the owned output. Remote setup/Run/archive
  execution remains a production gate; mock request mapping does not prove it.
  No runtime shim, skipped gate, baseline or suppression was added.

- Stop now joins checkpoint preparation, follow-up publication, post-turn Git and
  private-index cleanup. Local Git uses the same owned-command boundary as SSH;
  cancellation waits for physical closure. Accepted workspace mutations still
  finish during quit, while read-only ref queries cancel and drain. Unknown
  writer closure retains its private index and fences later workspace mutations;
  losing a read-only query does not claim ownership of writes. Four lifecycle
  regressions fail before this change. Real Git also exposed status refreshing
  the user index despite unchanged contents: three index-byte checks fail before
  setting `GIT_OPTIONAL_LOCKS=0` on both hosts. The
  [Git environment contract](https://git-scm.com/docs/git#Documentation/git.txt-GITOPTIONALLOCKS)
  disables that optional refresh while preserving required write locks. The
  final scoped run passes 475 checks. The rebuilt native pair passes 53 checks,
  including local and SSH TERM-resistant Git-filter parent/child cancellation,
  exact index preservation and private-index/lock cleanup. Existing-login Codex
  and Claude initial/warm/cold turns, checkpoint refs and Apply/Discard pass.
  Production/fixture types, Electron/native builds, workflow parsing and Rust
  formatting pass. Evidence: `checkpoint-stop-before.json`,
  `checkpoint-index-refresh-before.json`, `checkpoint-stop-broad-final.json`,
  `checkpoint-stop-native-final.json` and `checkpoint-stop-native-build.log` in
  the owned output. Channel loss, backend death, durable orphan/index recovery,
  escaped sessions and Windows descendant ownership remain separate gates.

- The actual Linux ARM64 CI artifact from `d444f4061f2` fails to load on Debian
  12 (glibc 2.36), even after its OpenSSL 3 dependency is installed: linked symbols
  require `GLIBC_2.38` and `GLIBC_2.39`. The downloaded archive matches CI's SHA256;
  qualification used an isolated Debian 12 container and recorded
  `linux-d444-debian12-with-openssl.log`. Linux needs an older build ABI and
  qualification against supported distributions. Windows CI also fails packaged
  smoke cleanup with `EBUSY` / `WinError 32` while terminal processes retain
  fixture directories. Durable terminal ownership and fixture teardown need
  diagnosis before assigning that failure to application shutdown.

- Packaged startup exposed an unconditional workflow webhook bind on port 17789.
  Another local instance already owned it, so the real package could not finish
  launching. The address is shipped in `v2.0.0-alpha.1`; it remains unchanged.
  Startup now acquires it only for enabled webhook triggers. Save/enable waits
  for listener readiness before persisting activation, bind failure reaches IPC
  and the workflow status text, and a later request can retry. The
  [Node listener contract](https://nodejs.org/docs/latest-v22.x/api/net.html#serverlisten)
  requires awaiting asynchronous bind success/error. A pre-fix admission
  regression fails; six webhook checks cover refusal/cause/retry, real HTTP
  dispatch, admission and error delivery. Full workflow execution/shutdown
  remains separately qualified, not established by its stubbed runner.

- The real macOS package then exposed node-pty's helper resolving to
  `app.asar.unpacked.unpacked` when the daemon starts from a physical unpacked
  entry. Launch the canonical logical `app.asar/dist-electron/daemon` entry;
  [Electron's ASAR contract](https://www.electronjs.org/docs/latest/tutorial/asar-archives)
  resolves native files through the archive. A built-package probe reproduces
  the missing helper and spawn failure from the old origin, then executes an
  actual shell and exits zero from the logical origin. No dependency was edited.
  The 95 workflow/terminal checks pass, including 49 repeated terminal checks
  after building. Production/fixture types, Electron/renderer builds and the
  unsigned directory package pass. The packaged repository/chat/terminal smoke
  and profile/repository restart smoke each pass on current source in separate
  runs. Fixtures now explicitly kill only their canonical repository's durable
  PTYs and wait for PID disappearance before deletion; app quit intentionally
  detaches those sessions. Windows confirmation still requires CI. A combined
  rerun fails the worktree-detail-panel assertion; earlier profile-quit timeouts
  also remain unexplained despite a later uninstrumented pass. Stable full
  packaged qualification is still open. Evidence: `webhook-start-before.json`,
  `webhook-start-final.json`, `packaged-startup-scoped.json`,
  `packaged-startup-terminal-final.json`, `packaged-node-pty-origin.json`,
  `packaged-node-pty-origin-validation.log`, `packaged-startup-final.json` and
  `packaged-startup-final-clean.json` in the owned output.

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
- Qualify checkpoint-command cancellation through channel loss and backend
  death, including durable retained private-index and process recovery. Direct
  Stop now has real local/SSH Git-filter descendant proof.
- Qualify server login/PATH setup and complete remote scripts,
  goals, file restore/Revert and historical comparisons. The main native task
  pipeline now has real SSH initial/follow-up/cold-resume and Apply/Discard proof.
  Remote comparison has compiled-sidecar, RPC and task-view proof over stdio;
  deployment and complete baseline/endpoint ownership still need qualification.
  Session ownership, capability
  negotiation and actual queued-read replay now have real SSH/browser proof.
  Production deployment now uses exact-release CLI/sidecar assets and immutable
  remote installs, with real native bootstrap proof. Cross-platform CI, Linux
  ABI/minimum-distro qualification, packaged bootstrap and published-asset checks
  remain open. Concurrent native sidecars now have private-cache ownership
  and shutdown proof on macOS; cross-OS evidence is still required. Manual deployment
  sets the deploying state before its first await, so a concurrent call is
  rejected rather than starting a second channel. Host verification now has
  policy, real SSH and native-dialog proof; packaged/cross-OS qualification is
  still open. Test Connection now has bounded remote-command ownership and
  live form failure/retry proof. Do not claim production SSH readiness from the
  owned loopback fixture. Remote file editing/status consumers and their sidecar
  parameter/result contracts, remaining remote Apply/Discard recovery/target
  safety, and Git-filter descendant cleanup remain open.
- Finish live watch/command-control checks and the lifecycle/owner contract for
  extended controls that can execute or write outside ordinary chat submission.
- Run appropriate scoped verification, build changed JavaScript bundles, inspect
  the final diff and non-test size, and record broad/packaged CI evidence or the
  exact remaining qualification gaps. [PR #670](https://github.com/crabbuild/crab/pull/670)
  is open against `crab-v2`, with continued release-gap work. No merge or release
  publication has been requested.

Run live checks only in an owned scratch directory:

```sh
CRAB_AGENT_LIVE_ROOT=<absolute-scratch-directory> npm test -- tests/agent-chat-live.spec.ts --maxWorkers 1
```

The test uses the installed agents and their existing authentication, performs
real inference, writes only inside its temporary fixture, and removes the
fixture afterward. It skips unless the scratch-directory variable is provided.

The normal-quit fixture launches a real Electron main process against the built
owners. Build them first so this check exercises current source:

```sh
npm run build:electron
CRAB_AGENT_LIVE_ROOT=<absolute-scratch-directory> npm test -- tests/agent-shutdown-live.spec.ts --maxWorkers 1
```

This fixture currently qualifies macOS/POSIX shutdown; it does not replace the
packaged Crab restart and supported-OS gates above.

The script-owner checks use real shell groups and the installed Crab binary:

```sh
CRAB_BIN=<installed-crab-binary> CRAB_AGENT_LIVE_ROOT=<absolute-scratch-directory> npm test -- tests/workspace-scripts-live.spec.ts --maxWorkers 1
```

The request-card browser checks run separately from packaged smoke:

```sh
npm exec playwright -- test --project agent-requests --workers 1
```

They use the existing Vite test server and an isolated installed-Chrome profile.

## Command cancellation follow-up (2026-11-10)

- An actual Node command leader that installs a TERM handler reproduces a
  cancellation hang: the local process owner starts descendant retirement only
  after leader exit, while command cancellation previously sent only TERM.
  The new live regression fails before the fix and proves PID disappearance
  after it. Its cleanup waits for the owned command even on assertion failure.
  Node documents that [sending a signal does not prove termination](https://nodejs.org/download/release/v22.20.0/docs/api/child_process.html#subprocesskillsignal).
- `runAgentCommand` escalates cancelled, timed-out, or over-limit commands to
  KILL after five seconds. Repeated failure notifications do not restart that
  deadline. Completion still requires physical closure; uncertain closure keeps
  the existing checkpoint writer fence. Provider adapters retain their own
  shutdown ladders; SSH commands keep the captured channel and native stop
  receipt, without transmitting local credentials.
- All 81 scoped command/checkpoint/SSH-detection checks and all three real local
  process cases pass. The two timeout/output-limit cases also prove that sending
  KILL does not itself release the writer. Production and touched-fixture types,
  Electron build, and diff checks pass. Evidence: `command-leader-before.json`,
  `command-leader-after.json`, and `command-leader-scoped.json`.
- The packaged worktree-detail failure remains open. Two additional current
  package probes pass, including compact-to-wide resizing and branch/worktree
  navigation; neither reproduces or explains the earlier combined-suite failure.
  Windows descendant ownership and packaged restart stability remain release
  gates.
- CI run `42843352759` passes Linux ARM64's native/SSH job. Linux x64 passes
  native integration, checkpoint ownership, packaged smoke, packaged storage,
  and SSH qualification, then reports 4,046 passes, 99 skips, and eleven goal
  failures in the broad suite. The goal fixture intercepted every spawned
  process, including checkpoint Git, and returned an empty HEAD result.
  Restricting its protocol mock to Codex preserves real Git snapshots. All
  eleven failures reproduce locally before this fixture correction; all eleven
  cases and the fixture type check pass afterward. Assertions remain intact.
  Evidence: `goal-owner-before.json` and `goal-owner-after.json`. New-head broad
  CI is still required; macOS and Windows had not completed at this check.
- Further packaged qualification uses an explicitly owned child home: the
  complete repository/chat/settings/Git/worktree/cold-and-warm-terminal case
  passes against the unchanged `1d687cd81ab` package. Its fixture sessions close,
  and the owned daemon's birth time/entry match before graceful shutdown and
  confirmed PID disappearance. Evidence: `packaged-isolated-current.json` and
  `packaged-isolated-home.json`. An earlier default-home cold-terminal failure
  used a pre-existing daemon from another checkout with an unpacked entry;
  that run does not qualify a fresh current daemon. Existing-daemon upgrade and
  packaged restart stability still need proof. A separate evaluator inspection
  failed before module inspection and was discarded; its isolated process tree
  was stopped and confirmed absent.
- Windows job `129924483362` passes native tests and four packaged cases, but
  two terminal cases fail when killing a daemon session with
  `Signals not supported on windows.` Cleanup then fails with `WinError 32`.
  The held directory belongs to the terminal-isolation fixture.
  Daemon transport is home-scoped while saved layouts are profile-scoped; the
  profile switch alone does not establish which process retains the directory.
  Source inspection and Linux success do not replace Windows owner proof.

### Windows terminal termination correction

The terminal Kill IPC reaches `Session.kill()`, which supplies `SIGTERM` to
`NodePtySubprocessHandle`. Installed node-pty 1.1.0's
`lib/windowsTerminal.js` rejects any signal argument before invoking its
ConPTY termination path. The wrapper now omits the signal on Windows;
POSIX termination retains the supplied signal. Explicit signal requests
remain a separate operation. Adopted descriptor handles use POSIX process
signals directly and do not call node-pty's Windows API.

The three-platform contract regression fails on the former Windows branch
and passes with the correction. All 70 scoped terminal checks pass, including
an authenticated real-daemon command followed by Kill and confirmed shell
PID disappearance. Production and touched-fixture types and the Electron
build pass. Evidence: `pty-kill-contract-before.json`,
`pty-kill-contract-after.json`, `pty-kill-types.log`, and
`pty-kill-fixture-types.log`. Native Windows packaged cleanup and descendant
ownership still require CI; this local result does not establish either.

### Long-home terminal startup correction

Both macOS jobs in run `42843352759` fail the terminal profile-isolation case
while waiting for a terminal input. The same case reproduces against the
rebased package. A separate real-package probe establishes the boundary:
an 88-byte socket path starts a working daemon terminal; a 135-byte path
reports `Socket path exceeds 104-byte limit` and leaves the terminal empty.
Node's [IPC contract](https://nodejs.org/docs/latest-v22.x/api/net.html#ipc-support)
limits Unix socket pathnames to 103 bytes on macOS and 107 on Linux before
truncation. The old validator also allowed one byte beyond those limits.

The shared address resolver now hashes unrepresentable paths into a bounded
endpoint under `/tmp`, with a private 0700 directory owned by the current
user. Tokens and PID files retain the original home identity. Startup rejects
public or symlinked socket directories; clients, stale cleanup and shutdown
all resolve the same endpoint. Representable Unix addresses retain the
v2.0.0-alpha.1 daemon reuse boundary until a protocol migration explicitly
retires it. Windows retains its named-pipe transport. Obsolete long-path
rejection tests are replaced by bounded, distinct UTF-8 address and actual
long-path startup/cleanup checks; Windows fixtures no longer invent socket
files for named pipes.

All 132 scoped checks pass, including real short/long-path daemon commands,
Kill with confirmed shell disappearance, private-directory rejection, stale
cleanup, client, upgrade and sibling adapter cases. Production and touched
fixture types and the Electron build pass. The rebuilt unsigned macOS
package passes the previously failing profile-isolation case in an owned
home, and its owned daemon is stopped with matching birth time and confirmed
PID disappearance. Evidence: `long-socket-address-before.json`,
`long-socket-final.json`, `long-home-packaged-before.json`,
`profile-packaged-current.json`, and `profile-packaged-home.json`.
The combined packaged rerun passes updater, workflow output, repository/chat/
terminal and profile-isolation cases, skips the storage case without an owned
bucket, and fails the separate workspace-sidebar scroll-restoration assertion
at `core-v2.spec.ts:524`. Its three owned daemons are stopped with verified
identity and confirmed disappearance. Evidence: `full-packaged-current.json`
and `full-packaged-home.json`. Sidebar persistence and new-head cross-platform
qualification remain required; the combined suite is not reported as passing.

### Sidebar quit persistence correction

The combined packaged failure exposed a separate save boundary. A controlled
real-package probe scrolls the mounted list and dispatches quit before its
queued scroll event runs: the visible position is 651, but the persisted
workspace position is 0. The packaged regression reproduces that mismatch
before the source correction. Earlier diagnostic runs that restored normally
do not disprove this scheduling window.

Quit now captures the mounted, unfiltered list's visible offset directly and
updates the tracked position before persisting. Filtering retains the prior
unfiltered position. The listener follows filter/key commits through
`useLayoutEffect`, whose [React contract](https://react.dev/reference/react/useLayoutEffect)
installs the committed effect before repaint. No timer, assertion threshold
or restart timeout changes. Key-change/unmount persistence still uses the
tracked previous-list offset; its pending-scroll boundary needs separate
qualification rather than assuming this quit proof covers it.

The rebuilt unsigned package passes the complete sidebar case, including
immediate quit flush, actual restart restoration and filtered quit preserving
the unfiltered position. Production/fixture types, renderer build and directory
packaging pass. Evidence: `sidebar-scroll-flush-before.json`,
`sidebar-regression-before.json`, `sidebar-regression-final.json`, and
`sidebar-flush-package-hashes.json`. The owned daemon is stopped with matching
birth time and confirmed disappearance. One interim extended check used a
locator from its closed pre-restart window; that fixture mistake is corrected
without changing assertions. The combined packaged rerun passes all five
enabled cases: updater, workflow output, repository/chat/terminal, profile
isolation and sidebar persistence. Storage is skipped without an isolated
bucket. All three owned daemons close with matched identity and confirmed
disappearance. Evidence: `full-packaged-after.json` and
`full-packaged-after-home.json`. This does not qualify storage, Windows or
other release gates.

## Electron runtime release gate (2026-11-10)

`package.json` selects Electron `^33.2.0`; the lockfile and installed package
resolve to 33.4.11. Electron's [support policy](https://www.electronjs.org/docs/latest/tutorial/electron-timelines)
covers its three latest stable majors. The [current release index](https://releases.electronjs.org/release?channel=stable)
lists 43, 44, and 45, with 45.2.3 the newest stable version at this check.
The current application runtime is outside that support window. Production
release requires a supported-runtime upgrade with native module rebuilding,
PTY daemon, preload/IPC, updater, packaged storage, and cross-OS qualification.
The GNU CLI/sidecar ABI work does not satisfy this application-runtime gate.

A read-only `npm audit --json` of the current lockfile reports 47 vulnerable
package entries: three critical, 27 high, 15 moderate, and two low. These include
build/test tools as well as Electron and the updater; the counts do not establish
application exploitability. Evidence: `desktop-dependency-audit.json`. Upgrade
and lockfile review remain required; no automatic fix, override, or audit
suppression was applied.

## GNU runtime portability qualification (2026-11-10)

- Linux native builds and tests use an AlmaLinux 8 toolchain container unless
  their host already has glibc 2.28 and GCC 14+. GCC Toolset 14 supports the current DuckDB C++
  sources while staging still enforces glibc 2.28, GLIBCXX 3.4.25, and CXXABI
  1.3.11. The compiler and runtime ABI are separate contracts: GCC 8 fails four
  actual DuckDB translation units; Toolset 14 passes their syntax checks. Its
  linked C++17 filesystem/reference-wrapper fixture passes the ABI gate and
  executes on stock AlmaLinux 8. Full CLI/sidecar rebuild and distro RPC proof
  remain pending. Evidence: `linux-native-baseline-build-trusted.log`,
  `linux-native-toolset14-preflight.log`, and `linux-native-toolset14-runtime.log`.
- Azure and Google SDKs use Rustls without changing SDK versions or removing
  their other default authentication, API-tag, and signing features. Reqwest
  also loads native roots so system/private CAs remain available. The lockfile
  removes eight platform-TLS/OpenSSL packages, with no additions or version
  changes. Four exact release feature graphs cover Linux x64/arm64, macOS
  arm64, and Windows x64: OpenSSL is absent and native roots remain enabled.
- Actual Google Storage and Azure Core clients reject an unknown CA, accept
  a trusted private CA, and reject a trusted certificate with the wrong hostname.
  The owned HTTPS server records the two accepted requests without authorization
  headers. Evidence: `linux-cloud-tls-contract.json`; no cloud credentials or
  live provider operations are used by this TLS probe.
- The real ELF policy accepts a baseline C++ fixture, rejects both downloaded
  `d444f4061f2` Linux binaries for their newer GLIBC requirements, and rejects a
  deliberately OpenSSL-linked fixture. Evidence: `linux-abi-policy-real-elf.log`.
- After rebasing on `fe527105459`, the macOS CLI/NFS helper/sidecar build passes
  with the new SDK transport. Native integration reports 273 passes and one
  existing ignored case. All 114 scoped desktop checks, 27 build-tool checks,
  23 architecture regression tests, production types, and Electron build pass.
  Evidence: `linux-portability-mac-native-rebased.log`,
  `linux-portability-mac-native-tests.log`, and `rebased-scoped.json`.
- The rebuilt unsigned macOS directory package passes the complete repository,
  chat/settings, Git/worktree, and cold/warm-terminal smoke case in a fresh
  owned child home. Fixture terminal closure and the owned daemon's confirmed
  disappearance precede cleanup. Evidence: `rebased-packaged-current.json`,
  `rebased-packaged-home.json`, and `linux-portability-verification.json`.
  This proves the current packaged path; profile/restart stability and the
  pre-existing foreign-daemon upgrade path remain separate gates.
- GNU release qualification still requires the full rebuilt pair and real
  repository RPCs on AlmaLinux 8, Debian 12, and Amazon Linux 2023. Alpine/musl,
  Electron/native Node module minimum distributions, Windows owner cleanup,
  published assets, and new-head cross-platform CI remain separate gates.


## Qualified GNU ARM64 runtime and supported Electron candidate (2026-11-10)

The complete GNU ARM64 release build now passes using the owned 10 GiB test VM.
All three staged binaries pass the linked glibc/C++ ABI limits and external
OpenSSL rejection, and their manifest hashes match. Actual AlmaLinux 8, Debian
12 and Amazon Linux 2023 containers each pass CLI/sidecar versions plus ping,
open, write and exact read-back RPCs. The exact runtime passes 272 native tests;
one existing S3 case remains ignored without an isolated bucket. All containers
close and the owned VM is stopped. Evidence: `linux-native-toolset14-retry-build.log`,
`linux-qualified-distros.log`, `linux-qualified-test.log`, and
`linux-portability-verification.json`. GNU x64, Alpine/musl and GUI distribution
minimums remain separate gates.

The supported-runtime candidate selects Electron 45.2.3, with embedded Node
24.22.2, and uses the public Electron rebuild API. Electron's macOS bundle now
requires macOS 13. Better-sqlite3 13.0.3 uses its maintained N-API implementation;
NAN 2.30.0 supplies the supported V8 callback contract needed by cpu-features.
There are no dependency patches, overrides or vendored replacements. Builder
26's updater configuration and icon conversion calls use its current APIs.
The directory package builds with the actual native SQLite, PTY and SSH addons.
The existing logical ASAR daemon entry remains canonical.

The [Electron breaking-change contract](https://www.electronjs.org/docs/latest/breaking-changes)
requires asynchronous clipboard writes and deprecates synchronous safeStorage.
Screenshot copying now awaits the PNG ClipboardItem write and returns failure
on native rejection. Saved connections use the asynchronous key provider,
reject Linux basic_text/unknown backends, and atomically rotate ciphertext when
requested. Reads and mutations share one queue so a delayed rotation cannot
overwrite a concurrent save. Existing vault schemas and ciphertext are retained.
All 31 storage and two clipboard cases pass, including rotation failure and
overlapping save/read behavior. These fake-key-provider tests establish queue
and error behavior, not operating-system keyring migration.

A synthetic old-runtime keyring migration probe was blocked in the macOS
Keychain authorization UI before creating a vault. The owned process and daemon
were closed with verified identity. No existing saved credentials were read,
reset or replaced, and no authorization was bypassed. A native old-ciphertext
migration and native clipboard copy still need a dedicated authorized GUI
session. Evidence: `keyring-migration-blocker.json` and `keyring-main-sample.txt`.

The packaged SQLite probe opens an actual previous-schema database through the
new native binding, preserves its old row and full-text index, adds the metadata
column, writes a new reply with item metadata, then closes/reopens and verifies
both rows, metadata and full-text search. Evidence:
`electron45-packaged-sqlite.json`. Production/fixture types, renderer/Electron
builds, 27 build-tool checks and 73 scoped persistence/storage/native-PTY checks
pass. Cross-OS qualification of this runtime remains required.

The packaged runtime exposed an existing navigation feedback loop: ordinary
history observations were delivered back to the repository view as restore
commands and could replace a newer terminal selection. History now issues
restore snapshots only for back/forward and explicit session restoration.
The unchanged packaged core case passes terminal selection, back navigation,
command-terminal switching and reload. Cross-repository restore identity and
restore-on-mount remain separate qualification work.

A subsequent full run reached every enabled feature assertion but failed while
deleting the profile fixture's HOME: authenticated daemon shutdown closed only
the server, leaving process-owned durability timers alive. RPC, idle and
successful replacement now delegate to daemon-main's shutdown owner, with the
requested terminal-kill policy. The checkpoint timer stops before the final
checkpoint; history, quota and memory resources close before process exit.
Standalone server instances retain their own transport/terminal cleanup.
The RPC acknowledgement precedes shutdown, so clients can distinguish acceptance
from process closure. Ninety scoped daemon/control tests pass. Fixtures that
create a HOME authenticate to that HOME's daemon, await its actual PID disappearance,
then delete it; fixtures inheriting a HOME never stop its durable daemon.

The post-runtime-upgrade audit still reports 31 vulnerable package entries,
including two critical entries in the old test toolchain. These include build
and test dependencies, so the count does not establish application exploitability.
The next dependency priority is the supported Vite/Vitest toolchain plus remaining
updater/transitive fixes. Evidence: `electron45-dependency-audit.json`.


The rebuilt Electron 45 package passes all five enabled combined smoke cases:
updater metadata/failures, workflow output/failures, repository/chat/terminal,
profile isolation and sidebar quit/restart persistence. Storage alone is skipped
without an isolated bucket. Both fixture-owned daemons exit through authenticated
shutdown before fixture deletion; the inherited qualification daemon is stopped
separately with matched birth identity. All three are confirmed gone. Evidence:
`electron45-shutdown-full.json` and `electron45-shutdown-full-home.json`.
This is unsigned macOS qualification, not cross-platform or production release
approval. The PR stays open and unmerged.


## Desktop dependency security candidate (2026-11-10)

Vite 8.3.7, React plugin 6.3.0 and Vitest 5.1.0 replace the old build/test
toolchain. Node typings follow the supported Node 22 host. The existing native
crash/recovery runner now declares vite-node 6 directly and uses its exported
CLI; it no longer relies on Vitest's transitive private executable. The real
Electron-node loader reaches the fixture's argument validation in an owned
HOME without invoking either provider. Vite's removed object-form manual chunks
becomes a Rolldown code-splitting group for Monaco. Removed minWorkers options
are deleted from desktop CI and runnable documentation.

Vitest's supported mock signature takes a function type, and constructor mocks
must be constructible. Three touched fixture types use that signature; ssh2's
Client fixture now uses a function constructor. Assertions and test inclusion
remain intact. The initial upgraded run passes 111 checks and exposes 52 SSH
fixture failures; the constructor correction passes all 54 SSH cases. The
combined rerun passes all 163 selected storage, clipboard, terminal, SSH,
workflow, updater, chat database and Codex resume cases. Production and touched
fixture types pass. The exported CLI probe is `toolchain-fixture-loader.json`;
regression evidence is `toolchain-scoped.json`, `toolchain-ssh-fixed.json`, and
`security-scoped.json`.

Normal updates within existing ranges select electron-updater 6.8.11 and remove
its vulnerable private builder-util-runtime copy, retaining 9.7.0. They also
update DOMPurify, Browserslist, brace expansion, YAML, compression and CSS parsing.
The application's two glob callers use maintained minimatch 11.0.6's named
minimatch function, including dot-file matching in SSH sync. Both workspace
discovery surfaces plus SSH, storage and updater pass 123 focused checks after
the final dependency update. Production types, renderer/Electron builds and all
27 build-tool checks pass. No patch, override, vendor edit, audit suppression
or unexpected registry is introduced.

The final read-only audit reports **zero critical and zero high** entries,
with **11 moderate and three low** entries still open. Most moderate entries
are parent-package reports of minimatch 9/10 inside build/lint dependencies:
app-builder-lib pins @electron/universal 2.0.3, and the latest TypeScript ESLint
parser still requires minimatch 10. npm's suggested major fixes include builder
and ESLint downgrades; these are not suitable substitutes for a maintained
upstream fix or separately approved dependency override. The three low entries
are the existing Streamdown/KaTeX/Mermaid tree; a renderer upgrade needs its
own API and behavior qualification. Evidence: `security-final-audit.json`,
`toolchain-lock-review.json` and `security-lock-review.json`. Audit package
counts include build tools and transitive parent entries; they do not establish
application exploitability or production readiness.


The final security candidate's rebuilt unsigned macOS package passes all five
enabled combined smoke cases with zero unexpected or flaky results. Storage
remains skipped without its isolated bucket. Both explicitly owned-home daemon
processes are gone before fixture removal, and the qualification parent daemon
is closed separately with verified birth identity. Evidence:
`security-final-packaged.json` and `security-final-packaged-home.json`.
The final fixture type probe also passes. Broader and cross-platform checks
remain assigned to CI; these results do not qualify signing or publication.

Dependency contracts reviewed for this batch: [Vite 8 migration](https://vite.dev/guide/migration),
[Vitest 5 migration](https://vitest.dev/guide/migration/),
[Vitest 4 removed APIs and constructor mocks](https://v4.vitest.dev/guide/migration.html),
[minimatch 11 matching contract](https://github.com/isaacs/minimatch/blob/v11.0.6/README.md),
and the [updater redirect advisory](https://github.com/advisories/GHSA-p2f4-r6v6-j797).

## Durable SSH task destination (2026-11-10)

A task previously saved only its editable SSH profile ID. Four warm/cold
Apply/Discard regressions demonstrate that reconnecting that profile to a
different destination redirects the task. Two additional regressions replace
the channel while workspace writers are closing. A real two-server regression
uses different server keys and ports with the same synthetic repository visible
to both: the old controller merges through the replacement server and removes
the task worktree. Backend UUID checks alone cannot establish host ownership
when the metadata is accessible on both servers.

New tasks persist the verified host, port, account and public-key fingerprint.
The session manager snapshots profiles before connection and automatic reconnect;
the router accepts an endpoint only from the connection owning its attached
agent channel. Start, checkpoint queries/writes, diff, Apply and disposable
Discard verify that saved destination. Apply/Discard capture the channel before
awaiting writer closure. The dependency contract is ssh2 1.17.0's hostVerifier:
without hostHash it receives the raw public-key Buffer, and an asynchronous
true callback accepts the handshake. Fingerprints retain the existing SHA256
public-key format; no credential is stored in the task binding.

Workspace conversations share ownership by authenticated endpoint and path.
Different profile aliases for the same endpoint share the owning task UUID;
different endpoints with the same path cannot borrow that UUID or retire each
other's writers. The real SQLite migration preserves old paths, branches and
native session IDs but leaves missing endpoint bindings unset. The tagged
v2.0.0-alpha.1 task schema stored no verified endpoint, so inferring one from
today's editable profile would redirect old work. Explicit recovery of those
bindings remains a production release gate.

The touched task family passes 268 cases; the session manager passes 55,
including profile mutation during connection/reconnect. Production and touched
fixture types, renderer/Electron builds and Rust formatting pass. Real SSH
checkpoint and availability siblings pass without provider inference. The fixed
two-server test refuses warm and restarted operations with zero task RPCs to
the replacement, then reconnects the original endpoint and applies the exact
file bytes. All fixture sidecars exit before successful fixture deletion.
The same real test is included in Unix desktop CI. Evidence:
`remote-endpoint-before.json`, `remote-endpoint-inflight-before.json`,
`remote-endpoint-shared-before.json`, `remote-endpoint-native-before.json`,
`remote-endpoint-native-before-effect.json`, `remote-endpoint-task-regression.json`,
`remote-endpoint-profile-owner.json`, `remote-endpoint-native-final.json` and
`remote-endpoint-native-siblings.json`.

The rebuilt unsigned macOS package passes all five enabled combined smoke cases;
storage is skipped without an isolated bucket. All three owned daemons are
confirmed gone. Evidence: `remote-endpoint-packaged.json` and
`remote-endpoint-packaged-home.json`. Production code grows to store and enforce
the previously absent destination contract; the existing router, SSH types and
task database own it without a new production adapter or file.

At the preceding ecd117bd7c4 head, Linux ARM64 CI passes all runtime/ABI/distro
steps, 272 native tests (one isolated-storage case ignored), and 11 real SSH
upload/startup cases. Windows passes its application build, native integration,
installer packaging and packaged storage qualification, but its broad suite
reports 62 failures across seven files. Those failures remain release blockers;
this endpoint batch does not claim cross-platform completion. Captured Git
Apply-target identity, explicit old-task recovery and the remaining lifecycle
and workflow gates above are still open. The PR remains unmerged.

## Windows ownership and fixture candidate (2026-11-10)

The preceding Windows broad-suite failure separates into two production
ownership defects and platform-specific fixture setup. Orchestrator shutdown
drains writers and persists tasks but never closes its task database. Three
new regressions leave real SQLite WAL/shared-memory files open after success,
writer failure and final-save failure. Shutdown now closes that owned store
after settling accepted operations, even when the final save fails. The goals
fixture also shuts down its complete service before removing its owned HOME.

The synchronous JavaScript realpath implementation preserves Windows short
names, whereas Git reports expanded paths. Workspace root/index validation and
missing-workspace restoration now use Node's native realpath implementation
consistently, including metadata-pointer resolution. Async diff/hunk path
resolution already uses the native binding; it requires no second path or
case-folding rule. Existing wrong-root, foreign-repository and shared-index
rejections remain covered. Worktree registration fixtures compare native paths
with separator normalization and rewrite Git's hidden pointer through an open
read/write descriptor. Node documents that truncating hidden Windows files
with the w flag fails with EPERM.

The daemon resolver unit fixtures explicitly disable ASAR interpretation while
they model logical package paths as ordinary directories, restoring the prior
process flag afterward. Actual ASAR loading remains covered by packaged smoke.
The terminal lifecycle fixture explicitly waits for Electron's GUI executable
in Windows cmd so the shell cannot compete with its foreground child for input.
No product fallback, signal change, dependency, override or lockfile change is
introduced by these fixture corrections.

All 361 touched-family cases and the real two-server endpoint test pass on
macOS; production types and the Electron build pass. The semantic fixture type
probe passes; its default unused-symbol check records four pre-existing
diagnostics in the three older fixture modules. Those symbols and repository
compiler settings are unchanged. Evidence: `task-db-shutdown-before.json`,
`task-db-shutdown-after.json`, `windows-owner-family.json`,
`windows-owner-ssh-live.json`, `windows-owner-fixture-types.log` and
`windows-owner-fixture-semantic-types.log`. Actual Windows execution of this
candidate remains assigned to CI; local macOS proof does not close that gate.

The rebuilt unsigned macOS package also passes all five enabled combined smoke
cases with zero unexpected or flaky results. Storage is skipped without an
isolated bucket; all three owned daemons are confirmed gone. Evidence:
`windows-owner-packaged.json` and `windows-owner-packaged-home.json`.

Dependency contracts: [Node native realpath and Windows file flags](https://nodejs.org/docs/latest-v22.x/api/fs.html),
[libuv Windows path resolution](https://github.com/libuv/libuv/blob/v1.51.0/src/win/fs.c),
[SQLite connection closure](https://github.com/WiseLibs/better-sqlite3/blob/v13.0.3/docs/api.md#close---this),
[Electron ASAR controls](https://www.electronjs.org/docs/latest/tutorial/asar-archives#treating-an-asar-archive-as-a-normal-file),
and [Windows start/wait behavior](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/start).

The next Apply-target probe also demonstrates a real mutation through the
built sidecar: create a task on intended, switch the repository to other, then
Apply with the task's returned baseline SHA. The sidecar reports target other,
merges the file there, leaves intended unchanged and removes the task worktree.
The owned process exits cleanly. Evidence: `apply-target-native-before.json`.
This remains unfixed. Local default creation captures its branch, but local
Apply still infers a missing target and selective Apply swallows checkout
failure; both siblings need qualification before choosing a canonical fix.
