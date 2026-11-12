import { Button } from "@/components/ui/button";
import { DropdownMenuItem, PointMenu } from "@/components/ui/dropdown-menu";
import clsx from "clsx";
import {
  AlertCircle,
  Bot,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  ClipboardList,
  ListTodo,
  Loader2,
  Pin,
  User,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { Streamdown } from "streamdown";
import type { AgentWorkflowDefinition, AgentWorkflowRun } from "../../ipc";
import { publishToast } from "../../lib/toasts";
import {
  ChatAttachment,
  ChatInput,
  ChatInputAgent,
  ChatInputModel,
  ChatInputSubmission,
  ChatMode,
} from "./ChatInput";
import { CheckpointMarker } from "./CheckpointMarker";
import { CreateCommandDialog } from "./CreateCommandDialog";
import { GoalBar } from "./GoalBar";
import { ResumeBanner } from "./ResumeBanner";
import { RevertCheckpointDialog } from "./RevertCheckpointDialog";
import { SlashCommand } from "./SlashCommandDropdown";
import { OrchestratorTask } from "./TaskCard";
import { TaskProgressPanel } from "./TaskProgressPanel";
import { TodoItem, TodoList } from "./TodoList";
import { TodoProgressPanel } from "./TodoProgressPanel";
import { resolveToolCard } from "./tool-cards";
import { HunkActionsProvider } from "./tool-cards/HunkActionsContext";
import { ToolCard } from "./ToolCard";
import { TurnCostChip } from "./TurnCostChip";
import { WorkflowProgressInline } from "./WorkflowProgressInline";

// ── Types ───────────────────────────────────────────────────────────
import type {
  ChatMessage,
  Checkpoint,
  ToolEvent,
} from "../../../electron/agent-chat-messages";
import { ModelDiscoveryStatus, type ModelDiscoveryControls } from "./ModelDiscoveryStatus";

interface Props {
  task: OrchestratorTask;
  messages: ChatMessage[];
  isStreaming: boolean;
  onSendMessage: (text: string, extras?: { attachments?: ChatAttachment[]; agentType?: string; model?: string | null; mode?: ChatMode; intelligence?: string | null }) => void;
  onStop: () => void;
  /** Number of feedback iterations sent for this task. */
  iterationCount?: number;
  /** Todo items for this task. */
  todos?: TodoItem[];
  /** Callback to add a todo. */
  onAddTodo?: (text: string, source?: "user" | "agent" | "comment") => void;
  /** Callback to toggle a todo. */
  onToggleTodo?: (todoId: string) => void;
  /** Callback to remove a todo. */
  onRemoveTodo?: (todoId: string) => void;
  /** Checkpoints (per-turn snapshots) to render in the timeline. */
  checkpoints?: Checkpoint[];
  /** Called when the user confirms reverting to a checkpoint turn. */
  onRevertCheckpoint?: (turnNumber: number) => void;
  /** Agents available to select from the composer. */
  agents?: ChatInputAgent[];
  /** Currently selected agent type. Defaults to the task's agent. */
  selectedAgentType?: string;
  /** Called when the user picks a different agent for this conversation. */
  onAgentChange?: (agentType: string) => void;
  /** Models compatible with the selected agent. */
  models?: ChatInputModel[];
  modelDiscovery?: ModelDiscoveryControls;
  intelligenceLevels?: string[];
  /** Currently selected model (empty = agent default). */
  selectedModel?: string;
  /** Called when the user picks a different model. */
  onModelChange?: (model: string) => void;
  /** Current chat mode — "plan" or "instant". */
  mode?: ChatMode;
  /** Called when the user toggles between plan and instant modes. */
  onModeChange?: (mode: ChatMode) => void;
  /** Repository id used by the @ context picker for file/commit lookups. */
  repoId?: string | null;
  /** Agent workflows surfaced in the @ menu. */
  workflows?: AgentWorkflowDefinition[];
  /** Absolute path of the editor file currently in focus (drives the quick-attach chip). */
  activeEditorFile?: string | null;
  /** Display label for the active editor file. */
  activeEditorLabel?: string | null;
  /** Called when the user wants to open the Agent Manager view. */
  onOpenAgentManager?: () => void;
  /** Optional structured transcript renderer. When provided, replaces the
   *  flat messages.map() rendering with the turn-based ChatTranscript. */
  transcript?: React.ReactNode;
  /** Active workflow runs associated with this chat task. */
  workflowRuns?: AgentWorkflowRun[];
}

// ── Component ───────────────────────────────────────────────────────

export function ChatTab({
  task,
  messages,
  isStreaming,
  onSendMessage,
  onStop,
  iterationCount = 0,
  todos = [],
  onAddTodo,
  onToggleTodo,
  onRemoveTodo,
  checkpoints = [],
  onRevertCheckpoint,
  agents,
  selectedAgentType,
  onAgentChange,
  modelDiscovery,
  intelligenceLevels = [],
  models,
  selectedModel,
  onModelChange,
  mode,
  onModeChange,
  repoId,
  workflows = [],
  activeEditorFile,
  activeEditorLabel,
  onOpenAgentManager,
  transcript,
  workflowRuns = [],
}: Props) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [autoScroll, setAutoScroll] = useState(true);
  const [todosExpanded, setTodosExpanded] = useState(false);
  const [revertTurn, setRevertTurn] = useState<number | null>(null);

  // ── Awaiting response state ───────────────────────────────────────
  // Bridges the dead zone between user submit and first streaming token.
  const [isAwaitingResponse, setIsAwaitingResponse] = useState(false);
  const awaitingStartRef = useRef<number>(0);

  // Clear awaiting state when streaming begins or a new agent message arrives.
  useEffect(() => {
    if (isStreaming) {
      setIsAwaitingResponse(false);
    }
  }, [isStreaming]);

  useEffect(() => {
    if (!isAwaitingResponse) return;
    // If a new agent message appeared, clear the awaiting state.
    const lastMsg = messages[messages.length - 1];
    if (lastMsg && lastMsg.role === "agent") {
      setIsAwaitingResponse(false);
    }
  }, [messages, isAwaitingResponse]);

  // Local fallback state when the parent doesn't control agent/model/mode.
  const [localAgentType, setLocalAgentType] = useState(selectedAgentType ?? task.agentName);
  const [localModel, setLocalModel] = useState(selectedModel ?? "");
  const [localMode, setLocalMode] = useState<ChatMode>(mode ?? "instant");
  const [intelligence, setIntelligence] = useState<string | null>(task.reasoningLevel ?? null);
  useEffect(() => { setIntelligence(task.reasoningLevel ?? null); }, [task.id, task.reasoningLevel]);
  // Keep local state aligned with parent-controlled props.
  useEffect(() => { if (selectedAgentType !== undefined) setLocalAgentType(selectedAgentType); }, [selectedAgentType]);
  useEffect(() => { if (selectedModel !== undefined) setLocalModel(selectedModel); }, [selectedModel]);
  useEffect(() => { if (mode !== undefined) setLocalMode(mode); }, [mode]);

  const activeAgentType = selectedAgentType ?? localAgentType;
  const activeModel = selectedModel ?? localModel;
  const activeMode = mode ?? localMode;

  const handleAgentChange = useCallback((t: string) => {
    setLocalAgentType(t);
    onAgentChange?.(t);
  }, [onAgentChange]);

  const handleModelChange = useCallback((m: string) => {
    setIntelligence(null);
    setLocalModel(m);
    onModelChange?.(m);
  }, [onModelChange]);

  const handleModeChange = useCallback((m: ChatMode) => {
    setLocalMode(m);
    onModeChange?.(m);
  }, [onModeChange]);

  // Resolve the list of agents — falls back to the task's own agent.
  const resolvedAgents: ChatInputAgent[] = agents ?? [
    { type: task.agentName, name: task.agentName, available: true },
  ];

  // Slash command state
  const [slashCommands, setSlashCommands] = useState<SlashCommand[]>([]);
  const [slashPending, setSlashPending] = useState<string | null>(null);
  const [createCommandOpen, setCreateCommandOpen] = useState(false);
  const [createCommandInitialName, setCreateCommandInitialName] = useState("");

  // ── Per-turn cost tracking ────────────────────────────────────────
  //
  // Fetches the per-turn cost breakdown for this task and maps each
  // cost entry onto the corresponding agent message by timestamp. We
  // poll on `isStreaming` transitions (turn completed → costs updated)
  // rather than every token event, so the UI stays quiet while the
  // agent is working.
  const [turnCosts, setTurnCosts] = useState<Array<{ timestamp: number; cost: number }>>([]);

  useEffect(() => {
    let cancelled = false;
    const fetchCosts = () => {
      (window as any).crab?.orchestrator
        ?.getTurnCosts?.(task.id)
        .then((result: { success?: boolean; turns?: Array<{ timestamp: number; cost?: number }> }) => {
          if (cancelled) return;
          if (result?.success && Array.isArray(result.turns)) {
            setTurnCosts(result.turns.flatMap(turn => turn.cost === undefined ? [] : [{ ...turn, cost: turn.cost }]));
          }
        })
        .catch(() => {});
    };
    fetchCosts();
    // Re-fetch shortly after streaming stops so the final usage event
    // has a chance to land in the cost tracker.
    if (!isStreaming) {
      const handle = setTimeout(fetchCosts, 800);
      return () => {
        cancelled = true;
        clearTimeout(handle);
      };
    }
    return () => { cancelled = true; };
  }, [task.id, isStreaming]);

  /**
   * Maps each agent message id to the cost that best matches its
   * timestamp (the first turn at-or-after the message). Also exposes
   * the running average so we can flag costly turns.
   */
  const { costByMessageId, rollingAverages } = (() => {
    const byId = new Map<string, number>();
    const averages = new Map<string, number>();
    if (turnCosts.length === 0) return { costByMessageId: byId, rollingAverages: averages };

    const agentMessages = messages.filter((m) => m.role === "agent" && m.kind !== "thinking");
    let costIdx = 0;
    const seen: number[] = [];
    for (const m of agentMessages) {
      while (costIdx < turnCosts.length && turnCosts[costIdx].timestamp < m.timestamp - 2_000) {
        costIdx++;
      }
      const candidate = turnCosts[costIdx];
      if (candidate && candidate.timestamp <= m.timestamp + 30_000) {
        byId.set(m.id, candidate.cost);
        averages.set(m.id, seen.length > 0 ? seen.reduce((a, b) => a + b, 0) / seen.length : 0);
        seen.push(candidate.cost);
        costIdx++;
      }
    }
    return { costByMessageId: byId, rollingAverages: averages };
  })();

  // ── Pin-to-context ────────────────────────────────────────────────
  //
  // Right-click any agent message → "Pin to context" writes its content
  // to the workspace's `.context/` folder via the existing context-file
  // API. That file is then injected into every subsequent agent prompt
  // by the orchestrator, so investigations stay coherent even when the
  // chat history is truncated.
  const [pinMenu, setPinMenu] = useState<{ x: number; y: number; message: ChatMessage } | null>(null);

  useEffect(() => {
    if (!pinMenu) return;
    const close = () => setPinMenu(null);
    window.addEventListener("scroll", close, true);
    return () => {
      window.removeEventListener("scroll", close, true);
    };
  }, [pinMenu]);

  const pinMessageToContext = useCallback(async (message: ChatMessage) => {
    const title = deriveTitle(message.content);
    const fileName = `pin-${formatDate(message.timestamp)}-${slugify(title)}.md`;
    const body = [
      `# ${title}`,
      "",
      `> Pinned from chat on ${new Date(message.timestamp).toLocaleString()}.`,
      "",
      message.content.trim(),
    ].join("\n");
    try {
      const result = await (window as any).crab?.orchestrator?.addContextFile(task.id, fileName, body);
      if (result?.success === false) {
        publishToast({ kind: "error", title: "Pin failed", body: result.error });
      } else {
        publishToast({ kind: "success", title: "Pinned to context", body: fileName });
      }
    } catch (err: any) {
      publishToast({ kind: "error", title: "Pin failed", body: err?.message });
    }
  }, [task.id]);

  // Load slash commands on mount.
  useEffect(() => {
    (window as any).crab?.orchestrator
      ?.listCommands?.("")
      .then((result: { success?: boolean; commands?: SlashCommand[] }) => {
        if (result?.success && result.commands) {
          setSlashCommands(result.commands);
        }
      })
      .catch(() => {});
  }, []);

  // Auto-scroll to bottom when new messages arrive.
  useEffect(() => {
    if (autoScroll && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [messages, isStreaming, isAwaitingResponse, autoScroll]);

  // Detect user scroll-up to disable auto-scroll.
  const handleScroll = useCallback(() => {
    if (!scrollRef.current) return;
    const { scrollTop, scrollHeight, clientHeight } = scrollRef.current;
    setAutoScroll(scrollHeight - scrollTop - clientHeight < 40);
  }, []);

  const handleInputSubmit = useCallback(
    (submission: ChatInputSubmission) => {
      if (isStreaming) return;

      // Handle /todo shortcut — still honors the legacy inline syntax.
      const todoMatch = submission.text.match(/^\/todo\s+(.+)/i);
      if (todoMatch && onAddTodo) {
        onAddTodo(todoMatch[1].trim(), "user");
        return;
      }

      // Handle inline slash commands with arguments: `/command-name args here`
      const slashMatch = submission.text.match(/^\/([a-z0-9-]+)(?:\s+(.*))?$/i);
      if (slashMatch) {
        const cmdName = slashMatch[1].toLowerCase();
        const rawArgs = slashMatch[2]?.trim() ?? "";
        const cmd = slashCommands.find((c) => c.name === cmdName);
        if (cmd) {
          // Expand the command with arguments and send.
          setSlashPending(cmdName);
          (window as any).crab?.orchestrator
            ?.expandCommand?.(cmdName, { _raw: rawArgs })
            .then((result: { success?: boolean; expanded?: string }) => {
              setSlashPending(null);
              if (result?.success && result.expanded) {
                setIsAwaitingResponse(true);
                awaitingStartRef.current = Date.now();
                onSendMessage(result.expanded, {
                  attachments: submission.attachments,
                  agentType: submission.agentType,
                  model: submission.model,
                  mode: submission.mode,
                  intelligence,
                });
              }
            })
            .catch(() => {
              setSlashPending(null);
            });
          return;
        }
      }

      // Build the final text, inlining attachments as an "Attached" block
      // so agents that don't read structured metadata still see them.
      let text = submission.text;
      if (submission.attachments.length > 0) {
        const lines = submission.attachments
          .map((a) => `- [${a.kind}] ${a.label}${a.value && a.value !== a.label ? ` (${a.value})` : ""}`)
          .join("\n");
        text = `${text}\n\nAttached:\n${lines}`;
      }

      // Immediately show the awaiting response indicator.
      setIsAwaitingResponse(true);
      awaitingStartRef.current = Date.now();

      onSendMessage(text, {
        attachments: submission.attachments,
        agentType: submission.agentType,
        model: submission.model,
        mode: submission.mode,
        intelligence,
      });
    },
    [isStreaming, onAddTodo, onSendMessage, slashCommands, intelligence],
  );

  // Handle slash command selection by expanding it through the orchestrator
  // and sending directly. Supports $ARGUMENTS substitution: if the user typed
  // `/command-name some args here`, the "some args here" part is passed as
  // `_raw` for $ARGUMENTS expansion.
  const handleSlashSelect = useCallback(
    (name: string) => {
      if (!name) return;
      setSlashPending(name);

      // If the command expects arguments but none were provided via inline
      // text, we still expand — the agent will see $ARGUMENTS as empty and
      // can ask the user. This matches Claude Code's behavior.
      (window as any).crab?.orchestrator
        ?.expandCommand?.(name, { _raw: "" })
        .then((result: { success?: boolean; expanded?: string }) => {
          setSlashPending(null);
          if (result?.success && result.expanded) {
            setIsAwaitingResponse(true);
            awaitingStartRef.current = Date.now();
            onSendMessage(result.expanded, {
              agentType: activeAgentType,
              model: activeModel || null,
              mode: activeMode,
              intelligence,
            });
          }
        })
        .catch(() => {
          setSlashPending(null);
        });
    },
    [onSendMessage, activeAgentType, activeModel, activeMode, slashCommands, intelligence],
  );

  // Handle "Create command" from the dropdown.
  const handleCreateCommand = useCallback((initialName?: string) => {
    setCreateCommandInitialName(initialName ?? "");
    setCreateCommandOpen(true);
  }, []);

  // After a command is created, reload the list.
  const handleCommandCreated = useCallback(() => {
    (window as any).crab?.orchestrator
      ?.listCommands?.("")
      .then((result: { success?: boolean; commands?: SlashCommand[] }) => {
        if (result?.success && result.commands) {
          setSlashCommands(result.commands);
        }
      })
      .catch(() => {});
  }, []);

  const handlePickContextFiles = useCallback(async (): Promise<string[] | null> => {
    try {
      const picked = await (window as any).crab?.orchestrator?.pickContextFiles?.();
      return Array.isArray(picked) ? picked : null;
    } catch {
      return null;
    }
  }, []);

  void slashPending; // reserved for future "expanding…" state UI

  // ── Extract latest TodoWrite state from messages ──────────────────
  //
  // Walk all messages to find the most recent TodoWrite tool event.
  // This drives the fixed bottom panel instead of inline rendering.
  const latestTodoWrite = (() => {
    let latest: { input: unknown; previousInput?: unknown } | null = null;
    for (const msg of messages) {
      if (!msg.events) continue;
      for (const evt of msg.events) {
        if (evt.tool === "TodoWrite" && evt.input) {
          latest = { input: evt.input, previousInput: evt.previousInput };
        }
      }
    }
    return latest;
  })();

  // ── Extract TaskCreate/TaskUpdate events for the bottom panel ──────
  //
  // Collect all TaskCreate and TaskUpdate events from messages.
  // These drive the TaskProgressPanel (fixed bottom) instead of inline cards.
  const taskManagementEvents = (() => {
    const events: ToolEvent[] = [];
    for (const msg of messages) {
      if (!msg.events) continue;
      for (const evt of msg.events) {
        if (evt.tool === "TaskCreate" || evt.tool === "TaskUpdate") {
          events.push(evt);
        }
      }
    }
    return events;
  })();

  // Timestamp of the most recent chat activity — used to decide whether
  // to offer the resume banner on mount.
  const lastMessageTs = messages.length > 0 ? messages[messages.length - 1].timestamp : null;

  const focusComposer = useCallback(() => {
    // Composer mounts below; use a small delay so layout settles.
    setTimeout(() => {
      const ta = document.querySelector<HTMLTextAreaElement>(
        "textarea[aria-label='Chat message input']",
      );
      ta?.focus();
    }, 50);
  }, []);

  return (
    <HunkActionsProvider value={{ taskId: task.id, taskTitle: task.title, worktreePath: task.worktreePath }}>
      <div className="flex h-full flex-col">
        {/* Revert checkpoint confirmation dialog */}
        {revertTurn !== null && onRevertCheckpoint && (
        <RevertCheckpointDialog
          turnNumber={revertTurn}
          onConfirm={() => {
            onRevertCheckpoint(revertTurn);
            setRevertTurn(null);
          }}
          onCancel={() => setRevertTurn(null)}
        />
      )}

        {/* Revert checkpoint confirmation dialog */}
        {pinMenu && (
          <PointMenu position={pinMenu} onClose={() => setPinMenu(null)}>
            <DropdownMenuItem
              onSelect={() => {
              void pinMessageToContext(pinMenu.message);
              setPinMenu(null);
            }}
              className="justify-start flex w-full items-center gap-2 px-3 py-1.5 text-left text-[11px] text-foreground hover:bg-accent"
            >
              <Pin size={11} className="text-primary" />
              Pin to context for future turns
            </DropdownMenuItem>
          </PointMenu>
        )}

        {/* Pin context menu */}
        <ResumeBanner
        taskId={task.id}
        lastMessageTs={lastMessageTs}
        onResume={focusComposer}
      />

        {task.agentName === "codex" && (
          <GoalBar key={task.id} taskId={task.id} isStreaming={isStreaming} closed={["archived", "applied", "discarded"].includes(task.status)} />
        )}

        {/* Resume banner — offers "pick up where you left off" when a prior
          session left checkpoints behind but no in-memory messages. */}
        <div
        ref={scrollRef}
        onScroll={handleScroll}
        className="flex-1 overflow-auto px-4 py-3"
      >
          <div className="mx-auto max-w-4xl space-y-3">
            {/* Messages area */}
            {todos.length > 0 && onToggleTodo && onAddTodo && onRemoveTodo && (
              <div className="rounded-lg border border-border bg-card">
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  onClick={() => setTodosExpanded(!todosExpanded)}
                  className="flex w-full items-center gap-2 px-3 py-2 text-xs text-foreground hover:bg-accent transition rounded-lg"
                >
                  {todosExpanded ? (
                    <ChevronDown size={11} />
                  ) : (
                    <ChevronRight size={11} />
                  )}
                  <ListTodo size={11} className="text-primary" />
                  <span className="font-medium">Todos</span>
                  <span className="text-muted-foreground">
                    ({todos.filter((t) => !t.done).length}/{todos.length}{" "}
                    remaining)
                  </span>
                </Button>
                {todosExpanded && (
                  <div className="border-t border-border px-2 py-2">
                    <TodoList
                  taskId={task.id}
                  todos={todos}
                  onToggle={onToggleTodo}
                  onAdd={onAddTodo}
                  onRemove={onRemoveTodo}
                  compact
                />
                  </div>
                )}
              </div>
            )}

            {messages.length === 0 && !isStreaming && (
              <div className="flex h-full flex-col items-center justify-center gap-2 text-center">
                <Bot size={28} className="text-muted-foreground/50" />
                <p className="text-xs text-muted-foreground max-w-[280px]">
                  The agent will show its progress here as it works on your
                  task.
                </p>
              </div>
            )}

            {/* Structured transcript renderer (turn-based) or flat message list */}
            {transcript
              ? transcript
              : messages.map((msg, idx) => {
                  // Thinking messages: show as "Thought for Xs" after streaming ends,
                  // or as a live "Thinking…" indicator during streaming.
                  if (msg.kind === "thinking") {
              // Calculate duration: time between this thinking message and the
              // next non-thinking message (or now if still streaming).
              const nextMsg = messages.slice(idx + 1).find((m) => m.kind !== "thinking");
              const endTime = nextMsg?.timestamp ?? (isStreaming ? Date.now() : msg.timestamp);
              const durationSec = Math.max(1, Math.round((endTime - msg.timestamp) / 1000));

              // During streaming and this is the last thinking message, show live indicator.
              const isLastThinking = !messages.slice(idx + 1).some((m) => m.kind === "thinking" || (m.role === "agent" && m.content));
              const isLive = isStreaming && isLastThinking;

              return (
                <ThinkingBubble
                  key={msg.id}
                  message={msg}
                  durationSec={durationSec}
                  isLive={isLive}
                  agentName={task.agentName}
                />
              );
            }
                  return isPlanMessage(msg, task, messages) ? (
                    <PlanCard key={msg.id} message={msg} agentName={task.agentName} />
                  ) : (
                    <MessageBubble
                  key={msg.id}
                  message={msg}
                  agentName={task.agentName}
                  turnCost={costByMessageId.get(msg.id) ?? null}
                  turnAverage={rollingAverages.get(msg.id) ?? null}
                  onContextMenu={msg.role === "agent" ? (e) => {
                    e.preventDefault();
                    setPinMenu({ x: e.clientX, y: e.clientY, message: msg });
                  } : undefined}
                />
                  );
                })}

            {/* Checkpoint markers interspersed after their corresponding turns */}
            {checkpoints.map((cp) => (
          <CheckpointMarker
            key={`checkpoint-${cp.turnNumber}`}
            turnNumber={cp.turnNumber}
            timestamp={cp.timestamp}
            onRevert={onRevertCheckpoint ? (turn) => setRevertTurn(turn) : undefined}
          />
        ))}

            {isStreaming && (
              <div className="flex items-start gap-2">
                <div className="rounded-lg bg-card px-3 py-2 text-xs text-muted-foreground">
                  <Loader2 size={12} className="animate-spin" />
                </div>
              </div>
            )}

            {/* Awaiting response indicator — shown immediately after user
            submits, before isStreaming flips. Bridges the perceived dead zone. */}
            {isAwaitingResponse && !isStreaming && (
          <AwaitingResponseIndicator
            agentName={task.agentName}
            startTime={awaitingStartRef.current}
          />
        )}
          </div>
          {/* close max-w-4xl */}
        </div>

        {/* Jump to bottom */}
        {!autoScroll && (
          <div className="flex justify-center pb-1">
            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={() => {
              setAutoScroll(true);
              scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight, behavior: "smooth" });
            }}
              className="flex items-center gap-1 rounded-full bg-card border border-border px-2 py-0.5 text-[10px] text-muted-foreground hover:text-foreground transition"
            >
              <ChevronDown size={10} /> Jump to bottom
            </Button>
          </div>
        )}

        {/* Max iterations warning */}
        {iterationCount >= 5 && (
          <div className="shrink-0 mx-3 mb-1 rounded-md border border-warning/40 bg-warning/10 px-3 py-2 text-xs text-warning">
            {iterationCount} iterations — consider breaking this into smaller
            tasks.
          </div>
        )}

        {/* Task progress panel — aggregates TaskCreate/TaskUpdate into a checklist */}
        {latestTodoWrite && (
        <TodoProgressPanel
          input={latestTodoWrite.input}
          previousInput={latestTodoWrite.previousInput}
        />
      )}

        {/* Workflow progress panel — shows active workflow runs for this chat */}
        {taskManagementEvents.length > 0 && (
        <TaskProgressPanel taskEvents={taskManagementEvents} />
      )}

        {/* Input area */}
        {workflowRuns.length > 0 && (
        <WorkflowProgressInline runs={workflowRuns} />
      )}

        {/* Input area */}
        <div className="shrink-0 p-3">
          {(task.status === "archived" || task.status === "applied" || task.status === "discarded") && (
            <p role="status" className="mx-auto mb-2 max-w-4xl text-xs text-muted-foreground">
              {task.status === "discarded" ? "This conversation was discarded. Start a new chat to continue." : "Reopen this conversation to send messages."}
            </p>
          )}
          <fieldset disabled={task.status === "archived" || task.status === "applied" || task.status === "discarded"} className="mx-auto min-w-0 max-w-4xl border-0 p-0">
            <ModelDiscoveryStatus discovery={modelDiscovery} />
            <ChatInput
          placeholder={
            task.status === "reviewing"
              ? "Send feedback, / for commands, @ for context…"
              : "Type a message, / for commands, @ for context…"
          }
          isStreaming={isStreaming}
          onSubmit={handleInputSubmit}
          onStop={onStop}
          agents={resolvedAgents}
          agentType={activeAgentType}
          onAgentChange={handleAgentChange}
          models={models ?? []}
          model={activeModel}
          onModelChange={handleModelChange}
          intelligence={intelligence}
          intelligenceLevels={intelligenceLevels}
          onIntelligenceChange={setIntelligence}
          mode={activeMode}
          onModeChange={handleModeChange}
          slashCommands={slashCommands}
          onSlashSelect={handleSlashSelect}
          onCreateCommand={handleCreateCommand}
          onPickContextFiles={handlePickContextFiles}
          onOpenAgentManager={onOpenAgentManager}
          repoId={repoId ?? null}
          workflows={workflows}
          taskId={task.id}
          activeEditorFile={activeEditorFile ?? null}
          activeEditorLabel={activeEditorLabel ?? null}
        />
          </fieldset>
        </div>
      </div>
      <CreateCommandDialog
      open={createCommandOpen}
      onClose={() => setCreateCommandOpen(false)}
      onCreated={handleCommandCreated}
      initialName={createCommandInitialName}
    />
    </HunkActionsProvider>
  );
}

// ── Awaiting response indicator ─────────────────────────────────────
// Shows progressive status labels to bridge the gap between user submit
// and the first streaming token. Eliminates the perceived blank page.

function AwaitingResponseIndicator({ agentName: _agentName, startTime }: { agentName: string; startTime: number }) {
  const [elapsed, setElapsed] = useState(0);

  useEffect(() => {
    const interval = setInterval(() => {
      setElapsed(Date.now() - startTime);
    }, 400);
    return () => clearInterval(interval);
  }, [startTime]);

  // Progressive stage labels — keeps the UI moving so the user feels momentum.
  const stageLabel = elapsed < 500
    ? "Connecting…"
    : elapsed < 2000
      ? "Reading your message…"
      : elapsed < 5000
        ? "Thinking…"
        : `Thinking… ${Math.round(elapsed / 1000)}s`;

  return (
    <div className="flex items-start gap-2">
      <div className="rounded-lg bg-card px-3 py-2 text-xs text-muted-foreground flex items-center gap-2">
        <span className="inline-flex gap-[3px]">
          <span className="w-1.5 h-1.5 rounded-full bg-current animate-bounce [animation-delay:0ms]" />
          <span className="w-1.5 h-1.5 rounded-full bg-current animate-bounce [animation-delay:150ms]" />
          <span className="w-1.5 h-1.5 rounded-full bg-current animate-bounce [animation-delay:300ms]" />
        </span>
        <span className="text-[11px]">{stageLabel}</span>
      </div>
    </div>
  );
}

// ── Thinking bubble ─────────────────────────────────────────────────

function ThinkingBubble({ message, durationSec: _durationSec, isLive, agentName: _agentName }: { message: ChatMessage; durationSec: number; isLive: boolean; agentName: string }) {
  const [expanded, setExpanded] = useState(false);

  // Format duration label.
  const durationLabel = isLive
    ? "Thinking…"
    : "Thought for a moment";

  return (
    <div className="flex items-start gap-2">
      <div className="max-w-[85%] rounded-lg bg-thinking/5 border border-thinking/20 px-3 py-2 text-xs leading-relaxed text-muted-foreground">
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={() => setExpanded(!expanded)}
          className="justify-start flex items-center gap-1.5 text-[10px] font-medium text-thinking cursor-pointer hover:text-primary transition w-full text-left"
        >
          {isLive ? (
            <Loader2 size={10} className="animate-spin" />
          ) : expanded ? (
            <ChevronDown size={10} />
          ) : (
            <ChevronRight size={10} />
          )}
          {durationLabel}
        </Button>
        {expanded && message.content && (
          <div className="mt-1.5 pt-1.5 border-t border-thinking/15 italic">
            <MarkdownContent content={message.content} />
          </div>
        )}
      </div>
    </div>
  );
}

// ── Message bubble ──────────────────────────────────────────────────

function MessageBubble({
  message,
  agentName: _agentName,
  turnCost,
  turnAverage,
  onContextMenu,
}: {
  message: ChatMessage;
  agentName: string;
  turnCost?: number | null;
  turnAverage?: number | null;
  onContextMenu?: (e: React.MouseEvent) => void;
}) {
  const isUser = message.role === "user";
  const isError = message.kind === "error";
  const isDone = message.kind === "done";

  // ── Completion (done) event ──
  // Short messages get a compact green indicator; longer ones get full
  // markdown rendering so summaries like "Changes made" are readable.
  if (isDone) {
    const isShort = (message.content || "").length < 120 && !message.content.includes("\n");
    if (isShort) {
      return (
        <div className="flex items-center gap-2 px-1 py-1">
          <CheckCircle2 size={8} className="text-success shrink-0 agent-status-icon" />
          <span className="text-xs text-success font-medium">
            {message.content || "Task completed"}
          </span>
        </div>
      );
    }
    // Longer done messages → render with markdown like normal agent messages.
    return (
      <div className="flex items-start gap-2">
        <CheckCircle2 size={8} className="text-success shrink-0 agent-status-icon mt-1" />
        <div className="max-w-[85%] rounded-lg bg-card px-3 py-2 text-xs leading-relaxed text-foreground">
          <MarkdownContent content={message.content} />
        </div>
      </div>
    );
  }

  // ── Error event: red border/background card ──
  if (isError) {
    return (
      <div className="flex items-start gap-2">
        <AlertCircle size={8} className="text-destructive shrink-0 agent-status-icon mt-1" />
        <div className="max-w-[85%] rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-xs leading-relaxed text-destructive">
          <MarkdownContent content={message.content} />
        </div>
      </div>
    );
  }

  // ── Normal user/agent messages ──
  return (
    <div
      className={clsx("flex items-start gap-2", isUser && "flex-row-reverse")}
      onContextMenu={onContextMenu}
    >
      {/* User messages keep their avatar; agent messages use left-border accent instead */}
      {isUser && (
        <div className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-info/15">
          <User size={12} className="text-info" />
        </div>
      )}
      <div
        className={clsx(
          "max-w-[85%] rounded-lg px-3 py-2 text-xs leading-relaxed relative",
          isUser
            ? "bg-info/15 text-foreground"
            : "bg-card text-foreground",
        )}
      >
        {/* Cost chip — only shown on agent messages with known cost. */}
        {!isUser && turnCost != null && (
          <span className="absolute -top-1.5 right-1.5">
            <TurnCostChip cost={turnCost} average={turnAverage ?? null} />
          </span>
        )}

        {isUser ? (
          <pre className="whitespace-pre-wrap font-sans">{message.content}</pre>
        ) : (
          <MarkdownContent content={message.content} />
        )}

        {/* Inline tool-use events */}
        {message.events && message.events.length > 0 && (
          <div className="mt-2 space-y-1.5">
            {message.events.map((evt, i) => (
              <ToolEventCard key={i} event={evt} />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

// ── Tool event card (dispatches to specialized renderers) ───────────

function ToolEventCard({ event }: { event: ToolEvent }) {
  const tool = event.tool ?? "";

  // TodoWrite is rendered in the fixed bottom panel — skip inline rendering.
  if (tool === "TodoWrite") {
    return null;
  }

  // TaskCreate/TaskUpdate are rendered in the fixed TaskProgressPanel — skip inline.
  if (tool === "TaskCreate" || tool === "TaskUpdate") {
    return null;
  }

  // Check the registry for a specialized renderer.
  const SpecializedCard = resolveToolCard(tool);
  if (SpecializedCard) {
    return <SpecializedCard event={event} />;
  }

  // Fallback: generic tool card.
  return (
    <ToolCard tool={tool} summary={event.summary} input={event.input} detail={event.detail} />
  );
}

// ── Markdown renderer ───────────────────────────────────────────────

function MarkdownContent({ content }: { content: string }) {
  if (!content.trim()) return null;

  return (
    <div className="prose-agent">
      <Streamdown shikiTheme={["github-light", "github-dark"]}>
        {content}
      </Streamdown>
    </div>
  );
}

// ── Plan detection ──────────────────────────────────────────────

/** Detects whether a message looks like an implementation plan. */
function isPlanMessage(message: ChatMessage, task: OrchestratorTask, messages: ChatMessage[]): boolean {
  if (message.role !== "agent") return false;
  // Thinking messages are never plans themselves.
  if (message.kind === "thinking") return false;

  // If the task is in "planning" status and this is the first non-thinking agent message, treat it as a plan.
  if (task.status === "planning") {
    const agentTextMessages = messages.filter((m) => m.role === "agent" && m.kind !== "thinking");
    if (agentTextMessages.length > 0 && agentTextMessages[0].id === message.id) return true;
  }

  // Detect structured plan markers in the content.
  const content = message.content.trim();
  const planMarkers = [
    /^#+\s*(implementation\s+)?plan/i,
    /^plan:/i,
    /^implementation plan/i,
    /^here('s| is) (my |the )?(implementation )?plan/i,
  ];

  for (const marker of planMarkers) {
    if (marker.test(content)) return true;
  }

  // Detect numbered list that starts within the first 200 chars (likely a plan).
  const firstChunk = content.slice(0, 200);
  if (/^\d+\.\s/.test(firstChunk) && task.status === "planning") return true;

  return false;
}

// ── Plan card ───────────────────────────────────────────────────────

function PlanCard({ message, agentName: _agentName }: { message: ChatMessage; agentName: string }) {
  return (
    <div className="flex items-start gap-2">
      <div className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-warning/15">
        <ClipboardList size={12} className="text-warning" />
      </div>
      <div className="max-w-[90%] rounded-lg border border-warning/30 bg-warning/5 px-4 py-3">
        <div className="mb-2 flex items-center gap-1.5 text-[11px] font-semibold text-warning">
          <ClipboardList size={11} />
          Implementation Plan
        </div>
        <pre className="whitespace-pre-wrap font-sans text-xs leading-relaxed text-foreground">
          {message.content}
        </pre>

        {/* Inline tool-use events */}
        {message.events && message.events.length > 0 && (
          <div className="mt-2 space-y-1">
            {message.events.map((evt, i) => (
              <ToolEventCard key={i} event={evt} />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

// ── Helpers ─────────────────────────────────────────────────────────

/** Derive a short human title from a message body. Used when naming
 *  pinned context files. */
function deriveTitle(content: string): string {
  const firstLine = content
    .split("\n")
    .map((l) => l.replace(/^#+\s*/, "").trim())
    .find((l) => l.length > 0);
  if (!firstLine) return "Pinned note";
  return firstLine.length > 60 ? firstLine.slice(0, 57) + "…" : firstLine;
}

/** Slug-safe lowercased identifier derived from free-form text. */
function slugify(input: string): string {
  return (
    input
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 40) || "note"
  );
}

/** Compact YYYYMMDD-HHMMSS timestamp for file naming. */
function formatDate(ts: number): string {
  const d = new Date(ts);
  const pad = (n: number) => n.toString().padStart(2, "0");
  return `${d.getFullYear()}${pad(d.getMonth() + 1)}${pad(d.getDate())}-${pad(d.getHours())}${pad(d.getMinutes())}${pad(d.getSeconds())}`;
}
