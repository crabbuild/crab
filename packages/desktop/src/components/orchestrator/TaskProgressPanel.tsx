/**
 * Task Progress Panel — fixed bottom panel for Claude Code's
 * TaskCreate/TaskUpdate protocol.
 *
 * Claude Code can use individual TaskCreate and TaskUpdate calls
 * (as opposed to the batch TodoWrite protocol). This panel aggregates
 * all TaskCreate/TaskUpdate events from the message history into a
 * unified progress checklist — rendered at the bottom of the chat,
 * above the input area.
 *
 * Layout matches TodoProgressPanel: collapsible header with progress
 * bar and individual task items with status icons.
 */

import { useState } from "react";
import clsx from "clsx";
import {
  Check,
  ChevronDown,
  ChevronUp,
  Circle,
  ClipboardList,
  Loader2,
} from "lucide-react";
import type { ToolEvent } from "./chat-types";

// ── Types ───────────────────────────────────────────────────────────

interface TaskItem {
  id: string;
  title: string;
  status: "completed" | "in_progress" | "pending";
}

export interface TaskProgressPanelProps {
  /** All TaskCreate/TaskUpdate events accumulated from messages. */
  taskEvents: ToolEvent[];
}

// ── Aggregation ─────────────────────────────────────────────────────

/**
 * Builds a task list from the sequence of TaskCreate/TaskUpdate events.
 * Each TaskCreate adds a new task; TaskUpdate modifies its status.
 *
 * ID matching strategy (handles multiple Claude Code versions):
 *   1. If TaskCreate input has `id`/`task_id`, use it as the canonical ID
 *   2. If the detail response contains "#N created successfully", extract N
 *   3. Fall back to sequential position (1, 2, 3...)
 *   4. TaskUpdate matches by `task_id` against any of the above
 *   5. Also try matching by position number (e.g., task_id="1" → first task)
 */
function aggregateTasks(events: ToolEvent[]): TaskItem[] {
  const tasksById = new Map<string, TaskItem>();
  const taskOrder: string[] = [];
  // Maps position numbers ("1", "2", ...) to canonical IDs for fallback matching.
  const positionToId = new Map<string, string>();
  let autoId = 0;

  for (const evt of events) {
    const input = evt.input as Record<string, unknown> | undefined;
    if (!input) continue;

    if (evt.tool === "TaskCreate") {
      autoId++;
      const posStr = String(autoId);

      // Determine canonical ID: prefer explicit id from input, then from detail response.
      const inputId = input.id ?? input.task_id;
      const detailMatch = evt.detail?.match(/#(\d+)\s+created successfully[:\s]*(.+)?$/i);
      const detailId = detailMatch?.[1];

      const canonicalId = String(inputId ?? detailId ?? posStr);
      const title = String(
        detailMatch?.[2]?.trim() ??
        input.title ?? input.description ?? input.prompt ?? `Task ${autoId}`
      );

      if (!tasksById.has(canonicalId)) {
        taskOrder.push(canonicalId);
      }
      tasksById.set(canonicalId, {
        id: canonicalId,
        title: title.slice(0, 120),
        status: "pending",
      });

      // Register position mapping so "1" resolves to the first task's canonical ID.
      positionToId.set(posStr, canonicalId);
      // Also register the detailId as an alias if different from canonical.
      if (detailId && detailId !== canonicalId) {
        positionToId.set(detailId, canonicalId);
      }

    } else if (evt.tool === "TaskUpdate") {
      let rawId = String(input.task_id ?? input.id ?? "");
      // Strip leading "#" if present (e.g., "#1" → "1").
      if (rawId.startsWith("#")) rawId = rawId.slice(1);
      const status = String(input.status ?? "");
      if (!rawId || !status) continue;

      const mappedStatus: TaskItem["status"] =
        status === "completed" || status === "done" ? "completed" :
        status === "in_progress" || status === "running" ? "in_progress" :
        "pending";

      // Try direct match first, then position-based fallback, then Nth-task index.
      let resolvedId: string | undefined;
      if (tasksById.has(rawId)) {
        resolvedId = rawId;
      } else if (positionToId.has(rawId)) {
        resolvedId = positionToId.get(rawId);
      } else {
        // Numeric ID → try as 1-based index into creation order.
        const numIdx = parseInt(rawId, 10);
        if (!isNaN(numIdx) && numIdx >= 1 && numIdx <= taskOrder.length) {
          resolvedId = taskOrder[numIdx - 1];
        }
      }

      if (resolvedId && tasksById.has(resolvedId)) {
        const existing = tasksById.get(resolvedId)!;
        tasksById.set(resolvedId, { ...existing, status: mappedStatus });
      } else {
        // Unknown task — create a placeholder entry.
        const placeholderId = resolvedId ?? rawId;
        if (!tasksById.has(placeholderId)) {
          taskOrder.push(placeholderId);
        }
        tasksById.set(placeholderId, {
          id: placeholderId,
          title: `Task #${rawId}`,
          status: mappedStatus,
        });
      }
    }
  }

  return taskOrder.map((id) => tasksById.get(id)!).filter(Boolean);
}

// ── Status rendering ────────────────────────────────────────────────

function StatusIcon({ status }: { status: TaskItem["status"] }) {
  switch (status) {
    case "completed":
      return (
        <div className="flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-gx-success/20 text-gx-success">
          <Check size={10} strokeWidth={3} />
        </div>
      );
    case "in_progress":
      return (
        <div className="flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-gx-accent/20 text-gx-accent">
          <Loader2 size={10} className="animate-spin" />
        </div>
      );
    default:
      return (
        <div className="flex h-4 w-4 shrink-0 items-center justify-center rounded-full border border-gx-border text-gx-muted">
          <Circle size={8} />
        </div>
      );
  }
}

function StatusBadge({ status }: { status: TaskItem["status"] }) {
  const styles = {
    completed: "bg-gx-success/15 text-gx-success border-gx-success/30",
    in_progress: "bg-gx-accent/15 text-gx-accent border-gx-accent/30",
    pending: "bg-gx-surface text-gx-muted border-gx-border",
  };
  const labels = {
    completed: "done",
    in_progress: "active",
    pending: "pending",
  };
  return (
    <span className={clsx("rounded px-1.5 py-0.5 text-[9px] font-medium border", styles[status])}>
      {labels[status]}
    </span>
  );
}

// ── Progress bar ────────────────────────────────────────────────────

function ProgressBar({ completed, total }: { completed: number; total: number }) {
  const pct = total > 0 ? Math.round((completed / total) * 100) : 0;
  return (
    <div className="flex items-center gap-2">
      <div className="flex-1 h-1.5 rounded-full bg-gx-surface overflow-hidden">
        <div
          className="h-full rounded-full bg-gx-success/70 transition-all duration-300"
          style={{ width: `${pct}%` }}
        />
      </div>
      <span className="text-[9px] text-gx-muted font-mono tabular-nums">
        {completed}/{total}
      </span>
    </div>
  );
}

// ── Main component ──────────────────────────────────────────────────

export function TaskProgressPanel({ taskEvents }: TaskProgressPanelProps) {
  const [expanded, setExpanded] = useState(true);

  const tasks = aggregateTasks(taskEvents);
  if (tasks.length === 0) return null;

  const completed = tasks.filter((t) => t.status === "completed").length;
  const inProgress = tasks.filter((t) => t.status === "in_progress").length;
  const total = tasks.length;

  const statusLabel = completed === total
    ? "Completed"
    : inProgress > 0
      ? "In progress"
      : "Pending";

  return (
    <div className="shrink-0 border-t border-gx-border bg-gx-bg-sub">
      <div className="mx-auto max-w-4xl">
      {/* Header — always visible */}
      <button
        type="button"
        onClick={() => setExpanded(!expanded)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left hover:bg-gx-surface-hover transition cursor-pointer"
      >
        <ClipboardList size={12} className="text-blue-400 shrink-0" />
        <span className="text-[11px] font-medium text-gx-text">Tasks</span>

        {/* Compact progress summary */}
        <span className="flex items-center gap-1.5 text-[10px] text-gx-text/80">
          {completed > 0 && (
            <span className="flex items-center gap-0.5 text-gx-success">
              <Check size={9} strokeWidth={3} />
              {completed}
            </span>
          )}
          {inProgress > 0 && (
            <span className="flex items-center gap-0.5 text-gx-accent">
              <Loader2 size={9} />
              {inProgress}
            </span>
          )}
        </span>

        {/* Status label */}
        <span className={clsx(
          "rounded px-1.5 py-0.5 text-[9px] font-medium border",
          completed === total
            ? "bg-gx-success/15 text-gx-success border-gx-success/30"
            : "bg-gx-accent/15 text-gx-accent border-gx-accent/30",
        )}>
          {statusLabel}
        </span>

        {/* Progress fraction */}
        <span className="text-[10px] text-gx-muted font-mono tabular-nums ml-auto mr-1">
          {completed}/{total}
        </span>

        {expanded
          ? <ChevronDown size={11} className="shrink-0 text-gx-muted/60" />
          : <ChevronUp size={11} className="shrink-0 text-gx-muted/60" />
        }
      </button>

      {/* Expanded content */}
      {expanded && (
        <div className="border-t border-gx-border/30 px-3 py-2 space-y-2 max-h-[240px] overflow-auto">
          {/* Progress bar */}
          <ProgressBar completed={completed} total={total} />

          {/* Task checklist */}
          <div className="space-y-0.5">
            {tasks.map((task) => (
              <div
                key={task.id}
                className={clsx(
                  "flex items-center gap-2 rounded px-1.5 py-1 transition",
                  task.status === "in_progress" && "bg-gx-accent/5 border-l-2 border-gx-accent/40",
                  task.status === "completed" && "border-l-2 border-gx-success/30",
                  task.status === "pending" && "border-l-2 border-transparent",
                )}
              >
                <StatusIcon status={task.status} />
                <span
                  className={clsx(
                    "flex-1 text-[10px] leading-tight",
                    task.status === "completed" && "text-gx-muted line-through",
                    task.status === "in_progress" && "text-gx-text font-medium",
                    task.status === "pending" && "text-gx-text/80",
                  )}
                >
                  {task.title}
                </span>
                <StatusBadge status={task.status} />
              </div>
            ))}
          </div>
        </div>
      )}
      </div>{/* close max-w-4xl */}
    </div>
  );
}
