import { useEffect, useState } from "react";
import clsx from "clsx";
import { CheckCircle2, ExternalLink, Trash2, Clock, XCircle, Pin, GitBranch, FolderPlus, Pencil } from "lucide-react";
import { AgentIcon, agentAccentColor, agentProviderLabel } from "./AgentIcon";
import type { TaskStatus } from "../../../electron/orchestrator";

// ── Types ───────────────────────────────────────────────────────────

export type { TaskStatus };

/** Statuses that represent terminal (completed) states where deletion is allowed. */
export const TERMINAL_STATUSES: Set<TaskStatus> = new Set([
  "reviewing",
  "applied",
  "failed",
  "stopped",
  "interrupted",
  "archived",
  "discarded",
]);

export interface OrchestratorTask {
  pendingCreation?: boolean;
  id: string;
  title: string;
  agentName: string;
  model?: string;
  reasoningLevel?: string;
  planFirst?: boolean;
  status: TaskStatus;
  createdAt: number;
  completedAt?: number;
  lastActivityAt?: number;
  branch?: string;
  baseBranch?: string;
  /** Progress: files completed / total files (while running). */
  progress?: { done: number; total: number };
  /** PR metadata if a PR has been created. */
  prUrl?: string;
  prNumber?: number;
  /** The filesystem path to the agent workspace. */
  worktreePath?: string;
  /** How the task workspace was chosen. */
  worktreeMode?: "auto" | "primary" | "existing";
  /** Agent Workflow run ID associated with this chat/session. */
  workflowRunId?: string;
  /** Agent Workflow node run ID associated with a workflow-spawned chat. */
  workflowNodeRunId?: string;
  /** Sidebar pin timestamp. */
  pinnedAt?: number;
  /** Parent task ID for sub-agent/child chats. */
  parentTaskId?: string;
  /** Root task ID for nested sub-agent trees. */
  rootTaskId?: string;
  /** Agent row ID that spawned this child chat. */
  spawnedByAgentId?: string;
  /** Role used when this chat was spawned as a sub-agent. */
  spawnedRole?: string;
  /** Optional label override for dense sidebar display/search. */
  sidebarLabel?: string;
  /** Derived child task IDs for sidebar tree rendering. */
  childTaskIds?: string[];
  /** Optional explicit sidebar grouping key. */
  sidebarGroup?: string;
}

// ── Status dot styling ──────────────────────────────────────────────

const STATUS_DOT: Record<TaskStatus, string> = {
  running: "bg-gx-success animate-pulse",
  planning: "bg-gx-warn",
  reviewing: "bg-gx-info",
  iterating: "bg-gx-thinking",
  queued: "bg-gx-warn animate-pulse",
  applied: "bg-gx-success",
  archived: "bg-gx-muted",
  failed: "bg-gx-error",
  stopped: "bg-gx-muted",
  interrupted: "bg-gx-muted",
  created: "bg-gx-muted/50",
  discarded: "bg-gx-muted/60",
};

// ── Elapsed time formatter ──────────────────────────────────────────

function formatElapsed(startMs: number, endMs?: number): string {
  const referenceMs = endMs ?? Date.now();
  const seconds = Math.floor((referenceMs - startMs) / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const secs = seconds % 60;
  if (minutes < 60) return `${minutes}m ${secs}s`;
  const hours = Math.floor(minutes / 60);
  return `${hours}h ${minutes % 60}m`;
}

/** Format a timestamp as relative time ("just now", "2m ago", "1h ago"). */
function formatTimeAgo(timestampMs: number): string {
  const seconds = Math.floor((Date.now() - timestampMs) / 1000);
  if (seconds < 10) return "just now";
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

/** Hook that ticks the elapsed timer every second while the task is active. */
function useElapsedTimer(task: OrchestratorTask): string {
  const isTerminal = TERMINAL_STATUSES.has(task.status);
  const [, setTick] = useState(0);

  useEffect(() => {
    // Tick every second for running tasks (shows elapsed),
    // every 30s for terminal tasks (shows "X ago" which changes slowly).
    const interval = setInterval(() => {
      if (!document.hidden) setTick((t) => t + 1);
    }, isTerminal ? 30_000 : 1000);
    return () => clearInterval(interval);
  }, [isTerminal]);

  if (isTerminal) {
    // For completed/failed tasks, show "time since completion" not duration.
    const completedAt = (task as any).completedAt ?? (task as any).updatedAt ?? task.createdAt;
    return formatTimeAgo(completedAt);
  }

  // For running tasks, show elapsed duration since creation.
  return formatElapsed(task.createdAt);
}

// ── Component ───────────────────────────────────────────────────────

interface Props {
  task: OrchestratorTask;
  isSelected: boolean;
  onClick: () => void;
  compact?: boolean;
  onDelete?: (taskId: string) => void;
  onTogglePin?: (taskId: string, pinned: boolean) => void;
  onSetGroup?: (taskId: string, currentGroup?: string) => void;
  onRename?: (taskId: string, currentTitle: string) => void;
  depth?: number;
  hasChildren?: boolean;
}

export function TaskCard({ task, isSelected, onClick, compact, onDelete, onTogglePin, onSetGroup, onRename, depth = 0, hasChildren }: Props) {
  const canDelete = TERMINAL_STATUSES.has(task.status);
  const elapsed = useElapsedTimer(task);
  const isPinned = Boolean(task.pinnedAt);

  if (compact) {
    return (
      <div className="group relative flex justify-center">
        <button
          type="button"
          onClick={onClick}
          className={clsx(
            "flex h-10 w-10 items-center justify-center rounded-md border transition",
            isSelected
              ? "border-gx-accent/50 bg-gx-accent/10"
              : "border-gx-border bg-gx-surface hover:bg-gx-surface-hover",
          )}
          aria-pressed={isSelected}
          aria-label={`Task: ${task.sidebarLabel ?? task.title}`}
          title={task.sidebarLabel ?? task.title}
        >
          <AgentIcon
            agentName={task.agentName}
            size={16}
            className={agentAccentColor(task.agentName)}
          />
          <span
            className={clsx("absolute bottom-1 right-1 h-2 w-2 rounded-full", STATUS_DOT[task.status])}
            aria-label={task.status}
          />
        </button>
      </div>
    );
  }

  return (
    <div className="group relative" style={{ paddingLeft: depth > 0 ? Math.min(depth * 12, 36) : 0 }}>
    <button
      type="button"
      onClick={onClick}
      className={clsx(
        "w-full rounded-md border px-3 py-2 text-left transition",
        isSelected
          ? "border-gx-accent/50 bg-gx-accent/10"
          : "border-gx-border bg-gx-surface hover:bg-gx-surface-hover",
      )}
      aria-pressed={isSelected}
      aria-label={`Task: ${task.title}`}
    >
      {/* Row 1: status indicator + title */}
      <div className="flex items-center gap-2">
        {hasChildren && <GitBranch size={10} className="shrink-0 text-gx-muted" />}
        {task.status === "applied" || task.status === "reviewing" ? (
          <CheckCircle2 size={10} className="shrink-0 text-gx-success" />
        ) : task.status === "failed" ? (
          <XCircle size={10} className="shrink-0 text-gx-error" />
        ) : (
          <span
            className={clsx("h-2 w-2 shrink-0 rounded-full", STATUS_DOT[task.status])}
            aria-label={task.status}
          />
        )}
        <span className={clsx(
          "min-w-0 flex-1 truncate text-xs font-medium",
          (task.status === "applied" || task.status === "reviewing")
            ? "text-gx-text/80"
            : "text-gx-text",
        )}>
            {task.sidebarLabel ?? task.title}
        </span>
      </div>

      {/* Row 2: agent name + elapsed time + completion status */}
      {!compact && (
        <div className="mt-1 flex items-center gap-1.5 pl-4 text-[10px] text-gx-muted">
          <AgentIcon
            agentName={task.agentName}
            size={10}
            className={agentAccentColor(task.agentName)}
          />
          <span className="truncate" title={task.agentName}>
            {task.spawnedRole ?? agentProviderLabel(task.agentName)}
          </span>
          {task.status === "queued" ? (
            <span className="shrink-0 text-gx-warn">· Waiting</span>
          ) : task.status === "applied" || task.status === "reviewing" ? (
            <span className="shrink-0 flex items-center gap-1 text-gx-success">
              <CheckCircle2 size={9} />
              <span>Complete · {elapsed}</span>
            </span>
          ) : task.status === "failed" ? (
            <span className="shrink-0 flex items-center gap-1 text-gx-error">
              <XCircle size={9} />
              <span>{elapsed}</span>
            </span>
          ) : task.status === "stopped" || task.status === "interrupted" || task.status === "discarded" ? (
            <span className="shrink-0 flex items-center gap-1 text-gx-muted">
              <Clock size={9} />
              <span>{elapsed}</span>
            </span>
          ) : (
            <span className="shrink-0 flex items-center gap-1">
              <Clock size={9} className="text-gx-muted/60" />
              <span>{elapsed}</span>
            </span>
          )}
          {task.prUrl && (
            <span
              className="shrink-0 flex items-center gap-0.5 text-gx-accent cursor-pointer hover:underline"
              onClick={(e) => {
                e.stopPropagation();
                (window as any).crab?.openUrl(task.prUrl);
              }}
              role="link"
              aria-label={`PR #${task.prNumber}`}
            >
              <ExternalLink size={8} />
              #{task.prNumber}
            </span>
          )}
          {isPinned && <Pin size={9} className="shrink-0 fill-gx-accent text-gx-accent" />}
          {task.sidebarGroup && (
            <span className="flex min-w-0 items-center gap-0.5 truncate text-gx-muted/80" title={`Collection: ${task.sidebarGroup}`}>
              <FolderPlus size={9} className="shrink-0" />
              <span className="truncate">{task.sidebarGroup}</span>
            </span>
          )}
          {task.workflowRunId && (
            <span className="shrink-0 text-gx-accent/90" title="Workflow session">
              · Workflow
            </span>
          )}
        </div>
      )}

      {/* Row 3: progress bar (only while running/iterating) */}
      {!compact && task.progress && (task.status === "running" || task.status === "iterating") && (
        <div className="mt-1.5 pl-4">
          <div className="flex items-center gap-2">
            <div className="h-1 flex-1 overflow-hidden rounded-full bg-gx-border">
              <div
                className="h-full rounded-full bg-gx-accent transition-all"
                style={{ width: `${Math.round((task.progress.done / task.progress.total) * 100)}%` }}
              />
            </div>
            <span className="shrink-0 text-[9px] text-gx-muted">
              {task.progress.done}/{task.progress.total}
            </span>
          </div>
        </div>
      )}
    </button>

      {/* Delete button (visible on hover for terminal-state tasks) */}
      {canDelete && onDelete && (
        <button
          type="button"
          onClick={(e) => {
            e.stopPropagation();
            onDelete(task.id);
          }}
          className="absolute right-2 top-2 hidden rounded p-1 text-gx-muted hover:bg-gx-error/20 hover:text-gx-error transition group-hover:block"
          aria-label={`Delete task: ${task.title}`}
          title="Delete task"
        >
          <Trash2 size={12} />
        </button>
      )}
      {onTogglePin && (
        <button
          type="button"
          onClick={(e) => {
            e.stopPropagation();
            onTogglePin(task.id, !isPinned);
          }}
          className={clsx(
            "absolute right-7 top-2 hidden rounded p-1 transition group-hover:block",
            isPinned
              ? "text-gx-accent hover:bg-gx-accent/10"
              : "text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text",
          )}
          aria-label={isPinned ? `Unpin task: ${task.title}` : `Pin task: ${task.title}`}
          title={isPinned ? "Unpin chat" : "Pin chat"}
        >
          <Pin size={12} className={isPinned ? "fill-current" : undefined} />
        </button>
      )}
      {onSetGroup && (
        <button
          type="button"
          onClick={(e) => {
            e.stopPropagation();
            onSetGroup(task.id, task.sidebarGroup);
          }}
          className="absolute right-12 top-2 hidden rounded p-1 text-gx-muted transition hover:bg-gx-surface-hover hover:text-gx-text group-hover:block"
          aria-label={task.sidebarGroup ? `Move task from collection: ${task.title}` : `Add task to collection: ${task.title}`}
          title={task.sidebarGroup ? "Change collection" : "Add to collection"}
        >
          <FolderPlus size={12} />
        </button>
      )}
      {onRename && (
        <button
          type="button"
          onClick={(e) => {
            e.stopPropagation();
            onRename(task.id, task.title);
          }}
          className="absolute right-[68px] top-2 hidden rounded p-1 text-gx-muted transition hover:bg-gx-surface-hover hover:text-gx-text group-hover:block"
          aria-label={`Rename task: ${task.title}`}
          title="Rename chat"
        >
          <Pencil size={12} />
        </button>
      )}
    </div>
  );
}
