import { useCardExpanded } from "./useCardExpanded";
/**
 * Specialized renderer for task management tool calls (Claude Code protocol).
 *
 * Handles: TaskCreate, TaskGet, TaskList, TaskUpdate
 *
 * Claude Code uses these tools to manage its internal task/todo list:
 *   - TaskCreate: creates a new sub-task with a description
 *   - TaskUpdate: transitions a task to a new status (in_progress, completed)
 *   - TaskGet: reads a task's current state
 *   - TaskList: enumerates all tasks
 *
 * Rendering philosophy:
 *   - TaskUpdate → compact status transition row (icon + task# + arrow + status badge)
 *   - TaskCreate → shows the task title/description as a new item
 *   - TaskGet/TaskList → collapsible detail view
 */


import clsx from "clsx";
import {
  ChevronDown,
  ChevronRight,
  CirclePlus,
  ClipboardCheck,
  List,
  RefreshCw,
  Circle,
  CheckCircle2,
  Loader2,
  XCircle,
} from "lucide-react";
import type { ToolCardRendererProps } from "./types";
import { CollapsedCardLine } from "./CollapsedCardLine";

// ── Status styling ──────────────────────────────────────────────────

type TaskStatus = "completed" | "in_progress" | "pending" | "cancelled" | "failed" | string;

function getStatusConfig(status: TaskStatus) {
  switch (status) {
    case "completed":
    case "done":
      return {
        icon: <CheckCircle2 size={12} />,
        colorClass: "text-success",
        bgClass: "bg-success/10 border-success/30",
        label: "completed",
      };
    case "in_progress":
    case "running":
      return {
        icon: <Loader2 size={12} />,
        colorClass: "text-info",
        bgClass: "bg-info/10 border-info/30",
        label: "in_progress",
      };
    case "pending":
    case "queued":
      return {
        icon: <Circle size={12} />,
        colorClass: "text-muted-foreground",
        bgClass: "bg-accent border-border/50",
        label: "pending",
      };
    case "cancelled":
      return {
        icon: <XCircle size={12} />,
        colorClass: "text-muted-foreground",
        bgClass: "bg-accent border-border/50",
        label: "cancelled",
      };
    case "failed":
    case "error":
      return {
        icon: <XCircle size={12} />,
        colorClass: "text-destructive",
        bgClass: "bg-destructive/10 border-destructive/30",
        label: "failed",
      };
    default:
      return {
        icon: <Circle size={12} />,
        colorClass: "text-muted-foreground",
        bgClass: "bg-accent border-border/50",
        label: status || "unknown",
      };
  }
}

// ── Component ───────────────────────────────────────────────────────

export function TaskManagementCard({ event, defaultExpanded = false, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const [expanded, setExpanded] = useCardExpanded(collapsedProp, defaultExpanded);

  const tool = event.tool ?? "";
  const input = event.input as Record<string, unknown> | undefined;
  const output = event.detail ?? "";
  const hasOutput = output.length > 0;

  // Collapsed one-liner branch.
  const collapsed = collapsedProp ?? false;
  if (collapsed) {
    let collapsedLabel = "Task";
    let collapsedDetail = "";
    switch (tool) {
      case "TaskUpdate": {
        const taskId = String(input?.task_id ?? input?.id ?? "");
        const status = String(input?.status ?? "");
        collapsedLabel = "Update Task";
        collapsedDetail = taskId ? `#${taskId.slice(0, 8)}` : "";
        if (status) collapsedDetail += ` → ${status}`;
        break;
      }
      case "TaskCreate": {
        collapsedLabel = "New Task";
        collapsedDetail = String(input?.title ?? input?.description ?? "").slice(0, 50);
        break;
      }
      case "TaskGet": {
        collapsedLabel = "Get Task";
        const taskId = String(input?.task_id ?? input?.id ?? "");
        collapsedDetail = taskId ? `#${taskId.slice(0, 12)}` : "";
        break;
      }
      case "TaskList": {
        collapsedLabel = "List Tasks";
        break;
      }
    }
    const defaultSummary = (
      <>
        <RefreshCw size={11} className="shrink-0 text-muted-foreground" />
        <span className="font-medium text-foreground/80">{collapsedLabel}</span>
        {collapsedDetail && <span className="truncate text-foreground/70">{collapsedDetail}</span>}
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`${collapsedLabel}: ${collapsedDetail}`}
      />
    );
  }

  // ── TaskUpdate: compact status transition row ──────────────────────
  if (tool === "TaskUpdate") {
    const taskId = String(input?.task_id ?? input?.id ?? "");
    const status = String(input?.status ?? "");
    const statusConfig = status ? getStatusConfig(status) : null;

    return (
      <div className="flex items-center gap-2 px-1 py-0.5 text-[11px]">
        <RefreshCw size={11} className="text-muted-foreground shrink-0" />
        <span className="text-muted-foreground">Update Task</span>
        {taskId && (
          <span className="font-mono text-foreground/70">#{taskId.slice(0, 8)}</span>
        )}
        {statusConfig && (
          <>
            <span className="text-muted-foreground">→</span>
            <span className={clsx(
              "inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[10px] font-medium border",
              statusConfig.bgClass,
              statusConfig.colorClass,
            )}>
              {statusConfig.icon}
              {statusConfig.label}
            </span>
          </>
        )}
        {hasOutput && (
          <button
            type="button"
            onClick={() => setExpanded(!expanded)}
            className="ml-auto text-muted-foreground/60 hover:text-foreground transition cursor-pointer"
          >
            {expanded
              ? <ChevronDown size={10} />
              : <ChevronRight size={10} />
            }
          </button>
        )}
      </div>
    );
  }

  // ── TaskCreate: show as a new task item ────────────────────────────
  if (tool === "TaskCreate") {
    const title = String(input?.title ?? input?.description ?? input?.prompt ?? "");

    return (
      <div className="rounded-md border border-border/40 overflow-hidden">
        <button
          type="button"
          onClick={() => hasOutput && setExpanded(!expanded)}
          className={clsx(
            "flex w-full items-center gap-2 px-2.5 py-1.5 text-[11px] text-left transition",
            hasOutput && "hover:bg-accent cursor-pointer",
            !hasOutput && "cursor-default",
          )}
        >
          <CirclePlus size={12} className="text-success/70 shrink-0" />
          <span className="font-medium text-muted-foreground shrink-0">New Task</span>
          <span className="min-w-0 flex-1 truncate text-foreground/80">
            {title.slice(0, 80)}{title.length > 80 ? "…" : ""}
          </span>
          {hasOutput && (
            expanded
              ? <ChevronDown size={10} className="shrink-0 text-muted-foreground/60" />
              : <ChevronRight size={10} className="shrink-0 text-muted-foreground/60" />
          )}
        </button>

        {expanded && hasOutput && (
          <div className="border-t border-border/30 px-2.5 py-1.5 text-[10px] text-foreground/60">
            {output.length > 3000 ? output.slice(0, 3000) + "\n… (truncated)" : output}
          </div>
        )}
      </div>
    );
  }

  // ── TaskGet / TaskList: collapsible detail ─────────────────────────
  let Icon = ClipboardCheck;
  let label = "Task";
  let summary = "";

  switch (tool) {
    case "TaskGet": {
      Icon = ClipboardCheck;
      label = "Get Task";
      const taskId = String(input?.task_id ?? input?.id ?? "");
      summary = taskId ? `#${taskId.slice(0, 12)}` : "";
      break;
    }
    case "TaskList": {
      Icon = List;
      label = "List Tasks";
      const status = String(input?.status ?? input?.filter ?? "");
      summary = status ? `filter: ${status}` : "all tasks";
      break;
    }
  }

  return (
    <div className="rounded-md border border-border/40 overflow-hidden">
      <button
        type="button"
        onClick={() => hasOutput && setExpanded(!expanded)}
        className={clsx(
          "flex w-full items-center gap-2 px-2.5 py-1.5 text-[10px] text-left transition",
          hasOutput && "hover:bg-accent cursor-pointer",
          !hasOutput && "cursor-default",
        )}
      >
        <Icon size={11} className="text-muted-foreground shrink-0" />
        <span className="shrink-0 font-medium text-muted-foreground">{label}</span>
        <span className="min-w-0 flex-1 truncate text-foreground/80">{summary}</span>
        {hasOutput && (
          expanded
            ? <ChevronDown size={10} className="shrink-0 text-muted-foreground/60" />
            : <ChevronRight size={10} className="shrink-0 text-muted-foreground/60" />
        )}
      </button>

      {expanded && hasOutput && (
        <div className="border-t border-border/30 max-h-[200px] overflow-auto px-2.5 py-2">
          <pre className="font-mono text-[10px] leading-[1.5] text-foreground/60 whitespace-pre-wrap">
            {output.length > 3000 ? output.slice(0, 3000) + "\n… (truncated)" : output}
          </pre>
        </div>
      )}
    </div>
  );
}
