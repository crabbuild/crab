import { useCallback, useEffect, useRef, useState } from "react";
import clsx from "clsx";
import {
  AlertCircle,
  BarChart3,
  CheckCircle2,
  ChevronDown,
  Clock,
  Loader2,
  Play,
  RefreshCw,
  Settings,
  Square,
  Terminal,
} from "lucide-react";
import { KanbanViewOrchestrator } from "../components/kanban/KanbanView";
import { BoardSettings } from "../components/kanban/BoardSettings";
import { AnalyticsPanel } from "../components/kanban/AnalyticsPanel";
import { BoardCreateDialog } from "../components/kanban/BoardCreateDialog";
import type {
  AgentWorkflowDefinition,
  KanbanBoardConfig,
  KanbanBoardState,
  KanbanCardInfo,
  KanbanColumnInfo,
  KanbanRunTaskOptions,
  SymphonyRuntimeSnapshot,
} from "../ipc";

interface KanbanViewProps {
  repoPath?: string;
}

type SyncStatus = "idle" | "syncing" | "error";

/**
 * Top-level Kanban view with toolbar (board selector dropdown,
 * sync status, settings gear, analytics button) + Board component.
 */
export function KanbanView({ repoPath }: KanbanViewProps) {
  const [boards, setBoards] = useState<KanbanBoardConfig[]>([]);
  const [activeBoardId, setActiveBoardId] = useState<string | null>(null);
  const [boardState, setBoardState] = useState<KanbanBoardState | null>(null);
  const [syncStatus, setSyncStatus] = useState<SyncStatus>("idle");
  const [loading, setLoading] = useState(true);
  const [dropdownOpen, setDropdownOpen] = useState(false);
  const [analyticsOpen, setAnalyticsOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [createBoardOpen, setCreateBoardOpen] = useState(false);
  const [lastSyncTime, setLastSyncTime] = useState<number | null>(null);
  const [syncError, setSyncError] = useState<string | null>(null);
  const [agentWorkflowDefinitions, setAgentWorkflowDefinitions] = useState<AgentWorkflowDefinition[]>([]);
  const [symphonyState, setSymphonyState] = useState<SymphonyRuntimeSnapshot | null>(null);
  const [symphonyBusy, setSymphonyBusy] = useState(false);

  // Load boards on mount
  useEffect(() => {
    let cancelled = false;
    async function loadBoards() {
      try {
        const allBoards = await window.crab.kanban.getBoards();
        if (cancelled) return;
        setBoards(allBoards);
        // Auto-select board matching current repo, or first board
        const match = allBoards.find((b) => b.repoPath === repoPath);
        const selected = match ?? allBoards[0] ?? null;
        setActiveBoardId(selected?.id ?? null);
      } catch {
        // IPC not ready yet — will retry on reconnect
      } finally {
        if (!cancelled) setLoading(false);
      }
    }
    loadBoards();
    return () => { cancelled = true; };
  }, [repoPath]);

  useEffect(() => {
    if (!repoPath) {
      setAgentWorkflowDefinitions([]);
      return;
    }
    let cancelled = false;
    window.crab.agentWorkflows.listDefinitions(repoPath).then((result) => {
      if (!cancelled && result.success) {
        setAgentWorkflowDefinitions(result.definitions);
      }
    }).catch(() => {
      if (!cancelled) setAgentWorkflowDefinitions([]);
    });
    return () => { cancelled = true; };
  }, [repoPath]);

  // Load board state when active board changes
  useEffect(() => {
    if (!activeBoardId) {
      setBoardState(null);
      return;
    }
    let cancelled = false;
    async function loadState() {
      try {
        const state = await window.crab.kanban.getBoard(activeBoardId!);
        if (!cancelled) setBoardState(state);
      } catch {
        // Board may not exist yet
      }
    }
    loadState();
    return () => { cancelled = true; };
  }, [activeBoardId]);

  // Subscribe to board updates from main process
  useEffect(() => {
    const unsub = window.crab.kanban.onBoardUpdated((payload) => {
      if (payload.boardId === activeBoardId) {
        setBoardState((prev) =>
          prev
            ? { ...prev, cards: payload.cards, columns: payload.columns }
            : null,
        );
        setLastSyncTime(Date.now());
        setSyncStatus("idle");
        setSyncError(null);
      }
    });
    return unsub;
  }, [activeBoardId]);

  // Load and subscribe to Symphony runtime state for the active board.
  useEffect(() => {
    if (!activeBoardId) {
      setSymphonyState(null);
      return;
    }
    let cancelled = false;
    window.crab.kanban.symphony.getState(activeBoardId).then((result) => {
      if (!cancelled && result.success) {
        setSymphonyState(result.data ?? null);
      }
    }).catch(() => {
      if (!cancelled) setSymphonyState(null);
    });
    return () => { cancelled = true; };
  }, [activeBoardId]);

  useEffect(() => {
    const unsubscribeState = window.crab.kanban.symphony.onState((payload) => {
      if (payload.boardId === activeBoardId) setSymphonyState(payload);
    });
    const unsubscribeConfig = window.crab.kanban.symphony.onConfigError((payload) => {
      if (payload.boardId === activeBoardId) {
        setSyncStatus("error");
        setSyncError(payload.message);
      }
    });
    return () => {
      unsubscribeState();
      unsubscribeConfig();
    };
  }, [activeBoardId]);

  // Handle card move via IPC
  const handleCardMove = useCallback(
    async (cardId: string, targetColumnId: string, positionIndex: number) => {
      const card = boardState?.cards.find((candidate) => candidate.id === cardId);
      if (card?.workflowProjection?.managed) return;
      if (card && isActiveSymphonyRuntime(card)) return;
      try {
        await window.crab.kanban.moveCard(cardId, targetColumnId, positionIndex);
        // Optimistic update
        setBoardState((prev) => {
          if (!prev) return prev;
          const updatedCards = prev.cards.map((c) =>
            c.id === cardId
              ? { ...c, columnId: targetColumnId, positionIndex }
              : c,
          );
          return { ...prev, cards: updatedCards };
        });
      } catch {
        // Revert will happen on next sync
      }
    },
    [boardState?.cards],
  );

  const handleForceSync = useCallback(async () => {
    if (!activeBoardId) return;
    setSyncStatus("syncing");
    setSyncError(null);
    try {
      await window.crab.kanban.forceSync(activeBoardId);
      setSyncStatus("idle");
      setLastSyncTime(Date.now());
    } catch (err) {
      setSyncStatus("error");
      setSyncError(err instanceof Error ? err.message : "Sync failed");
    }
  }, [activeBoardId]);

  const handleCreateBoard = useCallback(async (config: Partial<KanbanBoardConfig>) => {
    try {
      const created = await window.crab.kanban.createBoard({
        repoPath: repoPath ?? "",
        ...config,
      });
      if (created) {
        setBoards((prev) => [...prev, created]);
        setActiveBoardId(created.id);
        setBoardState({
          config: created,
          cards: [],
          columns: created.columns,
        });
        setCreateBoardOpen(false);
        setSettingsOpen(true);
      }
    } catch (err) {
      setSyncError(err instanceof Error ? err.message : "Board creation failed");
    }
  }, [repoPath]);

  // Handle board settings save via IPC (debounced persist within 1s)
  const settingsSaveRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const handleSettingsSave = useCallback(
    (config: Partial<KanbanBoardConfig>) => {
      if (settingsSaveRef.current) clearTimeout(settingsSaveRef.current);
      settingsSaveRef.current = setTimeout(async () => {
        if (!activeBoardId) return;
        try {
          const updated = await window.crab.kanban.updateBoard(activeBoardId, config);
          if (updated) {
            setBoards((prev) =>
              prev.map((b) => (b.id === activeBoardId ? updated : b)),
            );
            setBoardState((prev) =>
              prev ? { ...prev, config: updated, columns: updated.columns } : prev,
            );
          }
        } catch {
          // Save failed — will retry on next change
        }
      }, 800);
    },
    [activeBoardId],
  );

  // Clean up settings save timer
  useEffect(() => {
    return () => {
      if (settingsSaveRef.current) clearTimeout(settingsSaveRef.current);
    };
  }, []);

  const handleCreateCard = useCallback(
    async (columnId: string, title: string, description?: string) => {
      if (!activeBoardId) return;
      try {
        // The backend's create-card handler emits a board-updated event
        // which will update our state via the onBoardUpdated subscription.
        // No optimistic update needed — avoids duplicate cards.
        await window.crab.kanban.createCard(activeBoardId, columnId, title, description);
      } catch {
        // Card creation failed
      }
    },
    [activeBoardId],
  );

  const handleRunAgent = useCallback(
    async (card: KanbanCardInfo, options?: KanbanRunTaskOptions) => {
      if (!activeBoardId) return;
      try {
        const result = await window.crab.kanban.runTask(card.id, options);
        if (!result.success) {
          setSyncError(result.error ?? "Agent spawn failed");
          return;
        }
        const task = result.task as { id?: string; worktreePath?: string } | undefined;
        if (task?.id && options?.openChat !== false) {
          localStorage.setItem(
            "crab.orchestrator.pendingOpenTask",
            JSON.stringify({ taskId: task.id, repoPath }),
          );
          window.dispatchEvent(
            new CustomEvent("crab:open-agent-task", {
              detail: { taskId: task.id, repoPath },
            }),
          );
        }
      } catch (err) {
        setSyncError(err instanceof Error ? err.message : "Agent spawn failed");
        // Agent spawn failed
      }
    },
    [activeBoardId, repoPath],
  );

  const handleRunWorkflow = useCallback(async (definitionId: string, targetCards: KanbanCardInfo[]) => {
    if (!activeBoardId || !repoPath) return;
    for (const card of targetCards) {
      try {
        await window.crab.agentWorkflows.startRun(repoPath, definitionId, {
          sourceBoardId: activeBoardId,
          sourceCardId: card.id,
          projectToKanban: true,
        });
      } catch {
        // Workflow start failed
      }
    }
  }, [activeBoardId, repoPath]);

  const handleSymphonyStart = useCallback(async () => {
    if (!activeBoardId) return;
    setSymphonyBusy(true);
    try {
      const result = await window.crab.kanban.symphony.start(activeBoardId);
      if (result.success) setSymphonyState(result.data ?? null);
      else setSyncError(result.error ?? "Symphony start failed");
    } finally {
      setSymphonyBusy(false);
    }
  }, [activeBoardId]);

  const handleSymphonyStop = useCallback(async () => {
    if (!activeBoardId) return;
    setSymphonyBusy(true);
    try {
      const result = await window.crab.kanban.symphony.stop(activeBoardId);
      if (result.success) setSymphonyState(result.data ?? null);
      else setSyncError(result.error ?? "Symphony stop failed");
    } finally {
      setSymphonyBusy(false);
    }
  }, [activeBoardId]);

  const handleSymphonyRefresh = useCallback(async () => {
    if (!activeBoardId) return;
    setSymphonyBusy(true);
    try {
      const result = await window.crab.kanban.symphony.refresh(activeBoardId);
      if (result.success) setSymphonyState(result.data ?? null);
      else setSyncError(result.error ?? "Symphony refresh failed");
    } finally {
      setSymphonyBusy(false);
    }
  }, [activeBoardId]);

  const handleRetrySymphonyIssue = useCallback(async (issueId: string) => {
    if (!activeBoardId) return;
    const result = await window.crab.kanban.symphony.retryIssue(activeBoardId, issueId);
    if (result.success) setSymphonyState(result.data ?? null);
    else setSyncError(result.error ?? "Symphony retry failed");
  }, [activeBoardId]);

  const handleCancelSymphonyIssue = useCallback(async (issueId: string) => {
    if (!activeBoardId) return;
    const result = await window.crab.kanban.symphony.cancelIssue(activeBoardId, issueId);
    if (result.success) setSymphonyState(result.data ?? null);
    else setSyncError(result.error ?? "Symphony cancel failed");
  }, [activeBoardId]);

  const handleReleaseSymphonyIssue = useCallback(async (issueId: string) => {
    if (!activeBoardId) return;
    const result = await window.crab.kanban.symphony.releaseBlockedIssue(activeBoardId, issueId);
    if (result.success) setSymphonyState(result.data ?? null);
    else setSyncError(result.error ?? "Symphony release failed");
  }, [activeBoardId]);

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center">
        <Loader2 size={20} className="animate-spin text-gx-muted" />
      </div>
    );
  }

  const columns: KanbanColumnInfo[] = boardState?.columns ?? [];
  const cards: KanbanCardInfo[] = boardState?.cards ?? [];
  const activeBoardConfig = boardState?.config ?? boards.find((board) => board.id === activeBoardId) ?? null;

  return (
    <div className="flex h-full flex-col bg-gx-bg">
      {/* Toolbar */}
      <div className="flex items-center gap-2 border-b border-gx-border px-3 py-2">
        {/* Board selector dropdown */}
        <div className="relative">
          <button
            type="button"
            className="btn flex items-center gap-1 text-xs"
            onClick={() => setDropdownOpen((v) => !v)}
          >
            <span className="truncate max-w-[160px]">
              {boards.find((b) => b.id === activeBoardId)?.name ?? "Select Board"}
            </span>
            <ChevronDown size={12} />
          </button>
          {dropdownOpen && (
            <div className="absolute left-0 top-full z-50 mt-1 w-56 rounded border border-gx-border bg-gx-surface shadow-xl py-1">
              {boards.length === 0 && (
                <div className="px-3 py-2 text-xs text-gx-muted">No boards configured</div>
              )}
              {boards.map((board) => (
                <div
                  key={board.id}
                  className={clsx(
                    "flex w-full items-center px-3 py-1.5 text-xs text-gx-text hover:bg-gx-accent/10 transition group",
                    board.id === activeBoardId && "bg-gx-accent/10 font-medium",
                  )}
                >
                  <button
                    type="button"
                    className="flex-1 text-left truncate"
                    onClick={() => {
                      setActiveBoardId(board.id);
                      setDropdownOpen(false);
                    }}
                  >
                    {board.name || board.repoPath?.split("/").pop() || board.id}
                  </button>
                  <button
                    type="button"
                    className="hidden group-hover:block btn-icon h-4 w-4 text-gx-muted hover:text-gx-error"
                    title="Delete board"
                    onClick={async (e) => {
                      e.stopPropagation();
                      try {
                        await window.crab.kanban.deleteBoard(board.id);
                        setBoards((prev) => prev.filter((b) => b.id !== board.id));
                        if (activeBoardId === board.id) {
                          setActiveBoardId(null);
                          setBoardState(null);
                        }
                      } catch {}
                    }}
                  >
                    ×
                  </button>
                </div>
              ))}
              {/* New Board option */}
              <div className="border-t border-gx-border mt-1 pt-1">
                <button
                  type="button"
                  className="flex w-full items-center gap-1.5 px-3 py-1.5 text-xs text-gx-accent hover:bg-gx-accent/10 transition"
                  onClick={() => {
                    setDropdownOpen(false);
                    setCreateBoardOpen(true);
                  }}
                >
                  <span>+</span>
                  <span>New Board</span>
                </button>
              </div>
            </div>
          )}
        </div>

        {/* Sync status */}
        <button
          type="button"
          className={clsx(
            "btn-icon",
            syncStatus === "syncing" && "animate-spin",
            syncStatus === "error" && "text-gx-error",
          )}
          onClick={handleForceSync}
          title={syncError ?? "Sync board"}
          disabled={syncStatus === "syncing"}
        >
          <RefreshCw size={14} />
        </button>

        {/* Sync status indicator */}
        <SyncStatusIndicator
          status={syncStatus}
          lastSyncTime={lastSyncTime}
          error={syncError}
        />

        <div className="flex-1" />

        {/* Analytics button */}
        <button
          type="button"
          className={clsx("btn-icon", analyticsOpen && "text-gx-accent")}
          title="Analytics"
          onClick={() => setAnalyticsOpen((v) => !v)}
        >
          <BarChart3 size={14} />
        </button>

        {/* Settings gear */}
        <button
          type="button"
          className={clsx("btn-icon", settingsOpen && "text-gx-accent")}
          title="Board settings"
          onClick={() => setSettingsOpen(true)}
        >
          <Settings size={14} />
        </button>
      </div>

      {activeBoardConfig?.symphony?.enabled && (
        <SymphonyRuntimeHeader
          state={symphonyState}
          busy={symphonyBusy}
          onStart={handleSymphonyStart}
          onStop={handleSymphonyStop}
          onRefresh={handleSymphonyRefresh}
        />
      )}

      {/* Board content */}
      {boardState ? (
        <div className="flex flex-1 overflow-hidden">
          <div className="flex-1 overflow-hidden">
            <KanbanViewOrchestrator
              boardId={activeBoardId ?? boardState.config.id}
              columns={columns}
              cards={cards}
              boardConfig={activeBoardConfig}
              initialConfig={{
                densityMode: boardState.config.densityMode,
                savedViews: boardState.config.savedViews,
                columnStates: boardState.config.columnStates,
              }}
              onCardMove={handleCardMove}
              onCreateCard={handleCreateCard}
              onRunAgent={handleRunAgent}
              agentWorkflows={agentWorkflowDefinitions}
              onRunWorkflow={handleRunWorkflow}
              onRetrySymphonyIssue={handleRetrySymphonyIssue}
              onCancelSymphonyIssue={handleCancelSymphonyIssue}
              onReleaseSymphonyIssue={handleReleaseSymphonyIssue}
            />
          </div>
          {analyticsOpen && activeBoardId && (
            <AnalyticsPanel
              boardId={activeBoardId}
              onClose={() => setAnalyticsOpen(false)}
            />
          )}
        </div>
      ) : (
        <div className="flex flex-1 items-center justify-center">
          <div className="text-center">
            <p className="text-sm text-gx-muted">No board configured for this repository.</p>
            <button
              type="button"
              className="btn btn-primary mt-3 text-xs px-4 py-1.5"
              onClick={() => setCreateBoardOpen(true)}
            >
              Create Board
            </button>
          </div>
        </div>
      )}

      {/* Board Settings Dialog */}
      {settingsOpen && boardState && (
        <BoardSettings
          board={boardState.config}
          onClose={() => setSettingsOpen(false)}
          onSave={handleSettingsSave}
        />
      )}

      {createBoardOpen && (
        <BoardCreateDialog
          repoPath={repoPath}
          onClose={() => setCreateBoardOpen(false)}
          onCreate={handleCreateBoard}
        />
      )}

    </div>
  );
}

// ── Sync Status Indicator ─────────────────────────────────────────

function formatRelativeTime(timestamp: number): string {
  const diff = Date.now() - timestamp;
  if (diff < 60_000) return "just now";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h ago`;
  return `${Math.floor(diff / 86_400_000)}d ago`;
}

function SyncStatusIndicator({
  status,
  lastSyncTime,
  error,
}: {
  status: SyncStatus;
  lastSyncTime: number | null;
  error: string | null;
}) {
  if (status === "error") {
    return (
      <span
        className="flex items-center gap-1 text-[10px] text-gx-error"
        title={error ?? "Sync error"}
      >
        <AlertCircle size={10} />
        Error
      </span>
    );
  }

  if (status === "syncing") {
    return (
      <span className="flex items-center gap-1 text-[10px] text-gx-muted">
        <Loader2 size={10} className="animate-spin" />
        Syncing…
      </span>
    );
  }

  if (lastSyncTime) {
    return (
      <span
        className="flex items-center gap-1 text-[10px] text-gx-muted"
        title={`Last synced: ${new Date(lastSyncTime).toLocaleTimeString()}`}
      >
        <CheckCircle2 size={10} className="text-gx-success" />
        {formatRelativeTime(lastSyncTime)}
      </span>
    );
  }

  return (
    <span className="flex items-center gap-1 text-[10px] text-gx-muted">
      <Clock size={10} />
      Not synced
    </span>
  );
}

function SymphonyRuntimeHeader({
  state,
  busy,
  onStart,
  onStop,
  onRefresh,
}: {
  state: SymphonyRuntimeSnapshot | null;
  busy: boolean;
  onStart: () => void;
  onStop: () => void;
  onRefresh: () => void;
}) {
  const counts = state?.counts ?? { running: 0, retrying: 0, blocked: 0, completed: 0, claimed: 0 };
  const status = state?.status ?? "stopped";
  const tokenTotal = state?.codexTotals.totalTokens ?? 0;
  const workflowPath = state?.workflowPath ?? "WORKFLOW.md";

  return (
    <div className="flex min-h-[40px] flex-wrap items-center gap-x-3 gap-y-1 border-b border-gx-border bg-gx-surface px-3 py-1.5 text-[10px]">
      <div className="flex min-w-0 items-center gap-1.5 text-gx-text">
        <Terminal size={13} className="shrink-0 text-gx-accent" />
        <span className="font-medium">Symphony</span>
        <span
          className={clsx(
            "chip px-1.5 py-0",
            status === "error" && "border-gx-error/50 text-gx-error",
            status === "polling" && "border-gx-thinking/50 text-gx-thinking",
          )}
        >
          {status}
        </span>
      </div>

      <div className="flex min-w-0 items-center gap-2 text-gx-muted">
        <span title="Running issues">run {counts.running}</span>
        <span title="Retrying issues">retry {counts.retrying}</span>
        <span title="Blocked issues">blocked {counts.blocked}</span>
        <span title="Claimed issues">claimed {counts.claimed}</span>
        <span title="Completed in this runtime">done {counts.completed}</span>
      </div>

      <div className="min-w-0 flex-1 truncate text-gx-muted" title={workflowPath}>
        {state?.configError ? (
          <span className="text-gx-error">{state.configError.message}</span>
        ) : (
          <span>
            workflow {workflowPath} {state?.lastReloadAtMs ? `reloaded ${formatRelativeTime(state.lastReloadAtMs)}` : ""}
          </span>
        )}
      </div>

      <div className="flex items-center gap-2 text-gx-muted">
        <span title="Total Codex tokens">{formatCompactNumber(tokenTotal)} tokens</span>
        {state?.nextPollAtMs && <span>poll {formatFutureTime(state.nextPollAtMs)}</span>}
        {state?.rateLimits ? <span title={formatRateLimit(state.rateLimits)}>rate limit</span> : null}
      </div>

      <div className="flex items-center gap-1">
        {state?.started ? (
          <button
            type="button"
            className="btn-icon h-6 w-6"
            title="Stop Symphony"
            aria-label="Stop Symphony"
            disabled={busy}
            onClick={onStop}
          >
            {busy ? <Loader2 size={12} className="animate-spin" /> : <Square size={12} />}
          </button>
        ) : (
          <button
            type="button"
            className="btn-icon h-6 w-6"
            title="Start Symphony"
            aria-label="Start Symphony"
            disabled={busy}
            onClick={onStart}
          >
            {busy ? <Loader2 size={12} className="animate-spin" /> : <Play size={12} />}
          </button>
        )}
        <button
          type="button"
          className="btn-icon h-6 w-6"
          title="Refresh Symphony"
          aria-label="Refresh Symphony"
          disabled={busy}
          onClick={onRefresh}
        >
          <RefreshCw size={12} className={busy ? "animate-spin" : undefined} />
        </button>
      </div>
    </div>
  );
}

function formatCompactNumber(value: number): string {
  return new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 }).format(value);
}

function formatFutureTime(timestamp: number): string {
  const diff = timestamp - Date.now();
  if (diff <= 0) return "now";
  if (diff < 60_000) return `${Math.ceil(diff / 1_000)}s`;
  if (diff < 3_600_000) return `${Math.ceil(diff / 60_000)}m`;
  return `${Math.ceil(diff / 3_600_000)}h`;
}

function formatRateLimit(value: unknown): string {
  if (!value || typeof value !== "object") return "No rate-limit details";
  try {
    return JSON.stringify(value);
  } catch {
    return "Rate-limit snapshot unavailable";
  }
}

function isActiveSymphonyRuntime(card: KanbanCardInfo): boolean {
  const status = card.symphonyRuntime?.status;
  return status === "running" || status === "retrying" || status === "blocked" || status === "claimed";
}
