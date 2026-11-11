import { useCardExpanded } from "./useCardExpanded";
/**
 * Specialized renderer for Cron (scheduled task) tool calls.
 *
 * Handles: CronCreate, CronDelete, CronList
 *
 * Shows scheduled task management with timing info.
 */


import clsx from "clsx";
import { ChevronDown, ChevronRight, Clock, Trash2, List } from "lucide-react";
import type { ToolCardRendererProps } from "./types";
import { CollapsedCardLine } from "./CollapsedCardLine";

// ── Component ───────────────────────────────────────────────────────

export function CronCard({ event, defaultExpanded = false, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const [expanded, setExpanded] = useCardExpanded(collapsedProp, defaultExpanded);

  const tool = event.tool ?? "";
  const input = event.input as Record<string, unknown> | undefined;
  const output = event.detail ?? "";
  const hasOutput = output.length > 0;

  // Determine operation.
  let Icon = Clock;
  let label = "Schedule";
  let summary = "";

  switch (tool) {
    case "CronCreate": {
      Icon = Clock;
      label = "Schedule";
      const schedule = String(input?.schedule ?? input?.interval ?? input?.cron ?? "");
      const prompt = String(input?.prompt ?? input?.command ?? input?.task ?? "");
      summary = schedule
        ? `${schedule}: ${prompt.slice(0, 50)}`
        : prompt.slice(0, 60);
      break;
    }
    case "CronDelete": {
      Icon = Trash2;
      label = "Unschedule";
      const taskId = String(input?.id ?? input?.task_id ?? "");
      summary = taskId ? `Task ${taskId.slice(0, 12)}` : "task removed";
      break;
    }
    case "CronList": {
      Icon = List;
      label = "List Schedules";
      summary = "listing active tasks";
      break;
    }
  }

  // Collapsed one-liner branch.
  const collapsed = collapsedProp ?? false;
  if (collapsed) {
    const defaultSummary = (
      <>
        <Icon size={11} className="shrink-0 text-muted-foreground" />
        <span className="font-medium text-foreground/80">{label}</span>
        <span className="truncate text-foreground/70">{summary}</span>
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`Cron: ${label} ${summary}`}
      />
    );
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
            {output.length > 2000 ? output.slice(0, 2000) + "\n… (truncated)" : output}
          </pre>
        </div>
      )}
    </div>
  );
}
