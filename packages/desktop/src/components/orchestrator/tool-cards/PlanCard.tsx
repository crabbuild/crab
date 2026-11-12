import { Button } from "@/components/ui/button";
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

import clsx from "clsx";
import {
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  ClipboardList,
  Loader2,
} from "lucide-react";
import { useState } from "react";
import { Streamdown } from "streamdown";
import { CollapsedCardLine } from "./CollapsedCardLine";
import type { ToolCardRendererProps } from "./types";

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

  const Icon = isLive ? Loader2 : isEnter ? ClipboardList : CheckCircle2;
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
        <Icon size={11} className={clsx("shrink-0", isLive ? "text-info animate-spin" : "text-muted-foreground")} />
        <span className="font-medium text-foreground/80">{label}</span>
        <span className="truncate text-muted-foreground">{description}</span>
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
        isLive ? "border-info/30 bg-info/5" : "border-border/40",
      )}
    >
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={() => hasContent && setExpanded(!expanded)}
        className={clsx(
          "flex w-full items-center gap-2 px-2.5 py-1.5 text-[10px] text-left transition",
          hasContent && "hover:bg-accent cursor-pointer",
          !hasContent && "cursor-default",
        )}
      >
        <Icon
          size={11}
          className={clsx(
            "shrink-0",
            isLive ? "text-info animate-spin" : "text-muted-foreground",
          )}
        />
        <span className={clsx("shrink-0 font-medium", isLive ? "text-info" : "text-muted-foreground")}>
          {label}
        </span>
        <span className="flex-1 text-muted-foreground/80 truncate">
          {description}
        </span>

        {hasContent &&
          (expanded ? (
            <ChevronDown size={10} className="shrink-0 text-muted-foreground/60" />
          ) : (
            <ChevronRight size={10} className="shrink-0 text-muted-foreground/60" />
          ))}
      </Button>

      {expanded && hasContent && (
        <div className="border-t border-border/30 max-h-[360px] overflow-auto px-3 py-2">
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
