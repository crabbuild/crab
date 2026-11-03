/**
 * Specialized renderer for plan mode tool calls.
 *
 * Handles: EnterPlanMode, ExitPlanMode, and Codex `plan` items.
 *
 * Plan content streams via `item/plan/delta` and arrives at the
 * AgentTasksView reducer as a series of `task` events with
 * `input.status === "streaming"`. The card shows the partial plan
 * as it arrives, then replaces it with the authoritative version
 * when the matching `item.completed` lands (`status: "completed"`).
 *
 * The plan body is markdown-rendered through Streamdown so headings
 * and bullet lists land with proper formatting — matching the rest
 * of the chat surface.
 */

import { useState } from "react";
import clsx from "clsx";
import { ChevronDown, ChevronRight, ClipboardList, CheckCircle2, Loader2 } from "lucide-react";
import { Streamdown } from "streamdown";
import type { ToolCardRendererProps } from "./types";
import { CollapsedCardLine } from "./CollapsedCardLine";

// ── Component ───────────────────────────────────────────────────────

export function PlanModeCard({ event, defaultExpanded, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const tool = event.tool ?? "";
  const input = event.input as Record<string, unknown> | undefined;
  const isEnter = tool === "EnterPlanMode";
  const status = String(input?.status ?? "completed");
  const plan = String(input?.plan ?? input?.content ?? event.detail ?? "");
  const hasContent = plan.length > 0;

  // Default to expanded while the plan is streaming so the user
  // sees content arrive; collapse once it's done unless the caller
  // explicitly opted in.
  const isLive = status === "streaming" || status === "in_progress" || status === "updated";
  const [expanded, setExpanded] = useState(defaultExpanded ?? isLive);

  const Icon = isLive ? Loader2 : (isEnter ? ClipboardList : CheckCircle2);
  const label = isLive ? "Planning…" : isEnter ? "Plan" : "Plan Ready";
  const description = isLive
    ? "Drafting plan"
    : isEnter
      ? "Plan submitted"
      : "Plan submitted for approval";

  // Collapsed one-liner branch.
  const collapsed = collapsedProp ?? false;
  if (collapsed) {
    const defaultSummary = (
      <>
        <Icon size={11} className={clsx("shrink-0", isLive ? "text-gx-info animate-spin" : "text-gx-muted")} />
        <span className="font-medium text-gx-text/80">{label}</span>
        <span className="truncate text-gx-muted">{description}</span>
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`Plan: ${label}`}
      />
    );
  }

  return (
    <div
      className={clsx(
        "rounded-md border overflow-hidden transition-colors",
        isLive ? "border-gx-info/30 bg-gx-info/5" : "border-gx-border/40",
      )}
    >
      <button
        type="button"
        onClick={() => hasContent && setExpanded(!expanded)}
        className={clsx(
          "flex w-full items-center gap-2 px-2.5 py-1.5 text-[10px] text-left transition",
          hasContent && "hover:bg-gx-surface-hover cursor-pointer",
          !hasContent && "cursor-default",
        )}
      >
        <Icon
          size={11}
          className={clsx(
            "shrink-0",
            isLive ? "text-gx-info animate-spin" : "text-gx-muted",
          )}
        />
        <span className={clsx("shrink-0 font-medium", isLive ? "text-gx-info" : "text-gx-muted")}>
          {label}
        </span>
        <span className="flex-1 text-gx-muted/80 truncate">{description}</span>

        {hasContent && (
          expanded
            ? <ChevronDown size={10} className="shrink-0 text-gx-muted/60" />
            : <ChevronRight size={10} className="shrink-0 text-gx-muted/60" />
        )}
      </button>

      {expanded && hasContent && (
        <div className="border-t border-gx-border/30 max-h-[360px] overflow-auto px-3 py-2">
          <div className="prose-agent text-[11px]">
            <Streamdown shikiTheme={["github-light", "github-dark"]}>
              {plan}
            </Streamdown>
          </div>
        </div>
      )}
    </div>
  );
}
