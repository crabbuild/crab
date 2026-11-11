/**
 * Workflow Progress Inline — fixed bottom panel showing workflow run
 * progress when a workflow is active for the current chat task.
 */

import { useState } from "react";
import clsx from "clsx";
import {
  ChevronDown,
  ChevronUp,
  Circle,
  Loader2,
  Play,
  Pause,
  CheckCircle2,
  XCircle,
  SkipForward,
  UserCheck,
  Workflow,
} from "lucide-react";
import type { AgentWorkflowRun } from "../../ipc";

// ── Types ───────────────────────────────────────────────────────────────

interface Props {
  runs: AgentWorkflowRun[];
}

// ── Status Helpers ──────────────────────────────────────────────────────

const STATUS_ICONS: Record<string, React.ReactNode> = {
  completed: <CheckCircle2 className="w-3.5 h-3.5 text-success" />,
  failed: <XCircle className="w-3.5 h-3.5 text-destructive" />,
  cancelled: <XCircle className="w-3.5 h-3.5 text-warning" />,
  skipped: <SkipForward className="w-3.5 h-3.5 text-muted-foreground" />,
  running: <Loader2 className="w-3.5 h-3.5 text-info animate-spin" />,
  queued: <Circle className="w-3.5 h-3.5 text-muted-foreground" />,
  ready: <Circle className="w-3.5 h-3.5 text-muted-foreground" />,
  blocked: <Circle className="w-3.5 h-3.5 text-muted-foreground" />,
  needs_review: <UserCheck className="w-3.5 h-3.5 text-warning" />,
};

function statusLabel(status: string): string {
  switch (status) {
    case "needs_review": return "Review";
    case "running": return "Running";
    case "queued": return "Queued";
    case "ready": return "Ready";
    case "blocked": return "Blocked";
    case "completed": return "Done";
    case "failed": return "Failed";
    case "cancelled": return "Cancelled";
    case "skipped": return "Skipped";
    default: return status;
  }
}

function runBadgeColor(status: string): string {
  switch (status) {
    case "completed": return "bg-success/15 text-success border-success/30";
    case "failed": return "bg-destructive/15 text-destructive border-destructive/30";
    case "running": return "bg-info/15 text-info border-info/30";
    case "paused": return "bg-warning/15 text-warning border-warning/30";
    case "cancelled": return "bg-warning/15 text-warning border-warning/30";
    default: return "bg-accent/15 text-muted-foreground border-border/30";
  }
}

// ── Component ───────────────────────────────────────────────────────────

export function WorkflowProgressInline({ runs }: Props) {
  const [expandedRunId, setExpandedRunId] = useState<string | null>(null);

  if (runs.length === 0) return null;

  return (
    <div className="shrink-0 border-t border-border bg-background/50">
      {runs.map((run) => {
        const isExpanded = expandedRunId === run.id;
        const completedNodes = run.nodeRuns.filter((nr) => nr.status === "completed" || nr.status === "skipped").length;
        const totalNodes = run.nodeRuns.length;
        const name = run.definitionSnapshot?.name ?? "Workflow";

        return (
          <div key={run.id} className="border-b border-border last:border-b-0">
            {/* Header row */}
            <button
              type="button"
              className="flex items-center gap-2 w-full px-3 py-2 text-left hover:bg-card/30 transition-colors"
              onClick={() => setExpandedRunId(isExpanded ? null : run.id)}
            >
              <Workflow className="w-3.5 h-3.5 text-muted-foreground" />
              <span className="text-xs font-medium text-foreground truncate flex-1">
                {name}
              </span>
              <span className={clsx("text-[10px] px-1.5 py-0.5 rounded border", runBadgeColor(run.status))}>
                {run.status === "paused" ? (
                  <><Pause className="w-2.5 h-2.5 inline mr-0.5" />Paused</>
                ) : run.status === "running" ? (
                  <><Play className="w-2.5 h-2.5 inline mr-0.5" />Running</>
                ) : (
                  statusLabel(run.status)
                )}
              </span>
              <span className="text-[10px] text-muted-foreground tabular-nums">
                {completedNodes}/{totalNodes}
              </span>
              {isExpanded ? (
                <ChevronUp className="w-3 h-3 text-muted-foreground" />
              ) : (
                <ChevronDown className="w-3 h-3 text-muted-foreground" />
              )}
            </button>

            {/* Expanded node list */}
            {isExpanded && (
              <div className="px-3 pb-2 space-y-0.5">
                {run.nodeRuns.map((nodeRun) => {
                  const node = run.definitionSnapshot?.nodes?.find((n) => n.id === nodeRun.nodeId);
                  const title = node?.title ?? nodeRun.nodeId;
                  const isTerminal = ["completed", "failed", "cancelled", "skipped"].includes(nodeRun.status);
                  return (
                    <div
                      key={nodeRun.id}
                      className={clsx(
                        "flex items-center gap-2 px-2 py-1 rounded text-[11px]",
                        isTerminal ? "text-muted-foreground" : "text-muted-foreground",
                      )}
                    >
                      <span className="shrink-0">
                        {STATUS_ICONS[nodeRun.status] ?? <Circle className="w-3.5 h-3.5 text-muted-foreground" />}
                      </span>
                      <span className="flex-1 truncate">{title}</span>
                      <span className="text-[10px] text-muted-foreground">{statusLabel(nodeRun.status)}</span>
                      {nodeRun.attempt > 1 && (
                        <span className="text-[10px] text-warning">retry #{nodeRun.attempt}</span>
                      )}
                    </div>
                  );
                })}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
