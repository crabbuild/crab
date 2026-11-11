/**
 * Turn header component.
 *
 * Renders a collapsible header at the top of each Turn showing:
 * - When complete: "▸ Worked for <duration> · <N> tool calls · <M> files edited"
 * - When running: expanded form with a live "Working…" indicator and elapsed counter
 *
 * After `done` arrives, auto-collapses with a 400ms animation (D11) unless
 * the user has clicked the header during the turn or prefers-reduced-motion is set.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import clsx from "clsx";
import { ChevronRight, ChevronDown, Undo2, Eye, Check } from "lucide-react";

import type { Turn } from "../../lib/turn-derivation";
import { useChatUIState } from "../../lib/chat-ui-state";

// ─── Props ───────────────────────────────────────────────────────────────────

export interface TurnHeaderProps {
  turn: Turn;
  taskId: string;
  collapsed: boolean;
  collapsible: boolean;
  onUndo?: () => void;
  onReview?: () => void;
  onApply?: () => void;
}

// ─── Duration formatting (CHAT5.2) ──────────────────────────────────────────

/**
 * Format a duration in milliseconds:
 * - ≥ 60s → "Xm Ys"
 * - < 60s → "X.Ys"
 */
export function formatDuration(ms: number): string {
  const totalSeconds = Math.max(0, ms / 1000);
  if (totalSeconds >= 60) {
    const minutes = Math.floor(totalSeconds / 60);
    const seconds = Math.round(totalSeconds % 60);
    return `${minutes}m ${seconds}s`;
  }
  return `${totalSeconds.toFixed(1)}s`;
}

// ─── Reduced motion detection ────────────────────────────────────────────────

function usePrefersReducedMotion(): boolean {
  const [reduced, setReduced] = useState(() => {
    if (typeof window === "undefined") return false;
    return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  });

  useEffect(() => {
    const mq = window.matchMedia("(prefers-reduced-motion: reduce)");
    const handler = (e: MediaQueryListEvent) => setReduced(e.matches);
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  }, []);

  return reduced;
}

// ─── Component ───────────────────────────────────────────────────────────────

export function TurnHeader({ turn, taskId, collapsed: isCollapsed, collapsible, onUndo, onReview, onApply }: TurnHeaderProps) {
  const store = useChatUIState();
  const prefersReducedMotion = usePrefersReducedMotion();

  // Track whether the user has manually clicked the header during this turn
  const userClickedRef = useRef(false);
  // Track whether the auto-collapse animation has been triggered
  const autoCollapseTriggeredRef = useRef(false);
  // Track previous complete state to detect the done transition
  const prevCompleteRef = useRef(turn.complete);

  // Elapsed time counter for running turns (1 Hz)
  const [elapsedMs, setElapsedMs] = useState(0);
  const turnStartTimestamp = turn.events[0]?.timestamp ?? Date.now();

  // 1 Hz elapsed time counter while the turn is running
  useEffect(() => {
    if (turn.complete) return;

    const update = () => {
      setElapsedMs(Date.now() - turnStartTimestamp);
    };
    update(); // immediate first tick
    const interval = setInterval(update, 1000);
    return () => clearInterval(interval);
  }, [turn.complete, turnStartTimestamp]);

  // Auto-collapse 400ms after done arrives (D11)
  useEffect(() => {
    if (collapsible && turn.complete && !prevCompleteRef.current && !autoCollapseTriggeredRef.current) {
      autoCollapseTriggeredRef.current = true;

      // Suppress animation if user clicked or reduced motion
      if (userClickedRef.current || prefersReducedMotion) {
        // Don't auto-collapse if user has expressed intent
        if (!userClickedRef.current) {
          store.setTurnCollapsed(taskId, turn.turnIndex, true);
        }
      } else {
        // Animate collapse after 400ms delay
        const timer = setTimeout(() => {
          store.setTurnCollapsed(taskId, turn.turnIndex, true);
        }, 400);
        return () => clearTimeout(timer);
      }
    }
    prevCompleteRef.current = turn.complete;
  }, [collapsible, turn.complete, taskId, turn.turnIndex, prefersReducedMotion, store]);

  // Toggle handler
  const handleToggle = useCallback(() => {
    if (!collapsible) return;
    userClickedRef.current = true;
    store.setTurnCollapsed(taskId, turn.turnIndex, !isCollapsed);
  }, [collapsible, store, taskId, turn.turnIndex, isCollapsed]);

  // Count tool calls (all tool-type events)
  const toolCallCount = turn.events.filter((e) =>
    e.type === "tool_use" || e.type === "file_edit" || e.type === "file_read" ||
    e.type === "command" || e.type === "command_run" || e.type === "search" ||
    e.type === "web" || e.type === "task" || e.type === "generic"
  ).length;

  // Determine if file edit actions should be shown (CHAT5.6):
  // Show Apply/Review/Undo only when the turn has file_edit events
  // that haven't been applied/reverted
  const hasUnappliedEdits = turn.fileEditCount > 0;

  // Build the summary text
  const durationText = turn.complete && turn.durationMs != null
    ? formatDuration(turn.durationMs)
    : formatDuration(elapsedMs);

  const summaryParts: string[] = [];
  summaryParts.push(`Worked for ${durationText}`);
  if (toolCallCount > 0) {
    summaryParts.push(`${toolCallCount} tool call${toolCallCount !== 1 ? "s" : ""}`);
  }
  if (turn.fileEditCount > 0) {
    summaryParts.push(
      `${turn.fileEditCount} file${turn.fileEditCount !== 1 ? "s" : ""} edited`,
    );
  }

  return (
    <div
      className={clsx(
        "turn-header-collapse",
        "group rounded-md border border-transparent",
        "hover:border-border/30 hover:bg-accent/40",
        "transition-colors",
      )}
    >
      <button
        type="button"
        onClick={handleToggle}
        disabled={!collapsible}
        aria-expanded={!isCollapsed}
        className={clsx(
          "flex w-full items-center gap-2 px-2.5 py-1.5",
          "text-[11px] text-left cursor-pointer select-none disabled:cursor-default",
        )}
      >
        {/* Chevron indicator */}
        {isCollapsed ? (
          <ChevronRight size={12} className="shrink-0 text-muted-foreground/70" />
        ) : (
          <ChevronDown size={12} className="shrink-0 text-muted-foreground/70" />
        )}

        {/* Summary or working indicator */}
        {turn.complete ? (
          <span className="flex-1 text-muted-foreground font-medium truncate">
            {summaryParts.join(" · ")}
          </span>
        ) : (
          <span className="flex-1 flex items-center gap-2 text-muted-foreground font-medium">
            <span className="inline-flex items-center gap-1.5">
              <span className="relative flex h-2 w-2">
                <span className="animate-ping absolute inline-flex h-full w-full rounded-full bg-info opacity-75" />
                <span className="relative inline-flex rounded-full h-2 w-2 bg-info" />
              </span>
              Working…
            </span>
            <span className="text-muted-foreground/70">{durationText}</span>
            {toolCallCount > 0 && (
              <span className="text-muted-foreground/60">
                · {toolCallCount} tool call{toolCallCount !== 1 ? "s" : ""}
              </span>
            )}
          </span>
        )}

        {/* Action buttons — only when turn has unapplied file edits (CHAT5.6) */}
        {hasUnappliedEdits && turn.complete && (
          <span
            className="flex items-center gap-1 ml-auto"
            onClick={(e) => e.stopPropagation()}
          >
            {onApply && (
              <button
                type="button"
                onClick={onApply}
                className={clsx(
                  "inline-flex items-center gap-1 px-1.5 py-0.5 rounded",
                  "text-[10px] font-medium text-success",
                  "hover:bg-success/10 transition-colors",
                )}
                title="Apply all changes in this turn"
              >
                <Check size={10} />
                Apply
              </button>
            )}
            {onReview && (
              <button
                type="button"
                onClick={onReview}
                className={clsx(
                  "inline-flex items-center gap-1 px-1.5 py-0.5 rounded",
                  "text-[10px] font-medium text-info",
                  "hover:bg-info/10 transition-colors",
                )}
                title="Review changes in Diff tab"
              >
                <Eye size={10} />
                Review
              </button>
            )}
            {onUndo && (
              <button
                type="button"
                onClick={onUndo}
                className={clsx(
                  "inline-flex items-center gap-1 px-1.5 py-0.5 rounded",
                  "text-[10px] font-medium text-warning",
                  "hover:bg-warning/10 transition-colors",
                )}
                title="Undo all changes in this turn"
              >
                <Undo2 size={10} />
                Undo
              </button>
            )}
          </span>
        )}
      </button>
    </div>
  );
}
