/**
 * Specialized renderers for Codex-specific events:
 *   - CodexReviewCard: review mode entry/exit
 *   - ContextCompactionCard: context compaction indicator
 *
 * These are lightweight status indicators that show Codex's internal
 * state transitions without cluttering the chat with raw JSON.
 */

import clsx from "clsx";
import { Eye, Minimize2 } from "lucide-react";
import type { ToolCardRendererProps } from "./types";
import { CollapsedCardLine } from "./CollapsedCardLine";

// ── CodexReviewCard ─────────────────────────────────────────────────

export function CodexReviewCard({ event, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const input = event.input as Record<string, unknown> | undefined;
  const action = String(input?.action ?? "started");
  const review = String(input?.review ?? "");

  // Collapsed one-liner branch.
  const collapsed = collapsedProp ?? false;
  if (collapsed) {
    const defaultSummary = (
      <>
        <Eye size={11} className="shrink-0 text-info" />
        <span className="font-medium text-foreground/80">
          {action === "started" ? "Reviewing" : "Review complete"}
        </span>
        {review && <span className="truncate text-foreground/60">{review}</span>}
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`Review: ${action}`}
      />
    );
  }

  return (
    <div className="flex items-center gap-2 rounded-md border border-info/30 bg-info/5 px-2.5 py-1.5">
      <Eye size={11} className="shrink-0 text-info" />
      <span className="text-[10px] font-medium text-info">
        {action === "started" ? "Reviewing" : "Review complete"}
      </span>
      {review && (
        <span className="text-[10px] text-foreground/60 truncate">
          {review}
        </span>
      )}
    </div>
  );
}

// ── ContextCompactionCard ───────────────────────────────────────────

export function ContextCompactionCard({ event, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const input = event.input as Record<string, unknown> | undefined;
  const message = String(input?.message ?? "Context compacted");

  // Collapsed one-liner branch.
  const collapsed = collapsedProp ?? false;
  if (collapsed) {
    const defaultSummary = (
      <>
        <Minimize2 size={11} className="shrink-0 text-muted-foreground" />
        <span className="italic text-muted-foreground">{message}</span>
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel="Context compaction"
      />
    );
  }

  return (
    <div className={clsx(
      "flex items-center gap-2 rounded-md border border-border/30",
      "bg-accent/50 px-2.5 py-1.5",
    )}>
      <Minimize2 size={11} className="shrink-0 text-muted-foreground" />
      <span className="text-[10px] text-muted-foreground italic">
        {message}
      </span>
    </div>
  );
}
