/**
 * Collapsible group of contiguous tool cards.
 *
 * When collapsed AND complete, renders a single disclosure line:
 *   ▸ N tool calls · X.Ys
 * When collapsed AND running, renders a live "Working…" indicator
 * with elapsed time updating once per second.
 * When expanded, renders every member through its existing tool-card
 * renderer with per-card collapse state from the store.
 */

import { type ReactNode, useCallback, useEffect, useRef, useState } from "react";
import clsx from "clsx";
import { ChevronRight } from "lucide-react";
import type { ToolEvent } from "../../../electron/agent-chat-messages";
import { useChatUIState } from "../../lib/chat-ui-state";

// ─── Types ───────────────────────────────────────────────────────────────────

export interface ToolGroupProps {
  /** Stable group identity for collapse state lookup. */
  groupId: string;
  /** The ToolEvent members of this group. */
  members: ToolEvent[];
  /** True iff every tool_use in this group has a matching tool_result. */
  complete: boolean;
  keepExpanded?: boolean;
  /** Timestamp (ms) of the first tool_use event in the group. */
  startedAt: number;
  /** Timestamp (ms) of the last tool_result (or null if still running). */
  endedAt: number | null;
  /** Whether the group should default to collapsed. */
  defaultCollapsed: boolean;
  /** Renderer function that produces the appropriate card for each event. */
  cardRenderers: (event: ToolEvent, props: { collapsed: boolean; onToggleCollapse: (next: boolean) => void }) => ReactNode;
  /** Task ID for scoping collapse state. */
  taskId: string;
}

// ─── Duration formatting ─────────────────────────────────────────────────────

/**
 * Format a duration in milliseconds as a human-readable string.
 * ≥ 60s → "Xm Ys", otherwise → "X.Ys"
 */
function formatDuration(ms: number): string {
  const totalSeconds = ms / 1000;
  if (totalSeconds >= 60) {
    const minutes = Math.floor(totalSeconds / 60);
    const seconds = Math.floor(totalSeconds % 60);
    return `${minutes}m ${seconds}s`;
  }
  return `${totalSeconds.toFixed(1)}s`;
}

// ─── Elapsed time hook ───────────────────────────────────────────────────────

/**
 * Returns a live elapsed-time string that updates once per second
 * while the group is running.
 */
function useElapsedTime(startedAt: number, running: boolean): string {
  const [now, setNow] = useState(Date.now());
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  useEffect(() => {
    if (running) {
      setNow(Date.now());
      intervalRef.current = setInterval(() => {
        setNow(Date.now());
      }, 1000);
    }
    return () => {
      if (intervalRef.current !== null) {
        clearInterval(intervalRef.current);
        intervalRef.current = null;
      }
    };
  }, [running]);

  const elapsed = now - startedAt;
  return formatDuration(Math.max(0, elapsed));
}

// ─── Component ───────────────────────────────────────────────────────────────

export function ToolGroup({
  groupId,
  members,
  complete,
  keepExpanded = false,
  startedAt,
  endedAt,
  defaultCollapsed,
  cardRenderers,
  taskId,
}: ToolGroupProps) {
  const store = useChatUIState();
  const taskState = store.byTask.get(taskId);

  // Resolve collapsed state: store override → default
  const storeEntry = taskState?.groupCollapse[groupId];
  const collapsed = keepExpanded ? false : storeEntry?.collapsed ?? defaultCollapsed;

  const running = !complete;
  const elapsedStr = useElapsedTime(startedAt, running && collapsed);

  const handleToggle = useCallback(() => {
    store.setGroupCollapsed(taskId, groupId, !collapsed);
  }, [store, taskId, groupId, collapsed]);

  // Duration for the completed disclosure line
  const durationStr = endedAt !== null
    ? formatDuration(endedAt - startedAt)
    : null;

  const memberCount = members.length;

  // ── Collapsed: complete ──────────────────────────────────────────
  if (collapsed && complete) {
    return (
      <button
        type="button"
        onClick={handleToggle}
        aria-expanded={false}
        className={clsx(
          "flex w-full items-center gap-2 px-2.5 py-1.5",
          "text-[10px] text-left rounded-md",
          "bg-gx-surface/40 hover:bg-gx-surface-hover cursor-pointer",
          "transition-colors",
        )}
      >
        <ChevronRight size={10} className="shrink-0 text-gx-muted/60" />
        <span className="text-gx-muted">
          {memberCount} tool call{memberCount !== 1 ? "s" : ""}
          {durationStr && <span className="ml-1">· {durationStr}</span>}
        </span>
      </button>
    );
  }

  // ── Collapsed: running ───────────────────────────────────────────
  if (collapsed && running) {
    return (
      <button
        type="button"
        onClick={handleToggle}
        aria-expanded={false}
        className={clsx(
          "flex w-full items-center gap-2 px-2.5 py-1.5",
          "text-[10px] text-left rounded-md",
          "bg-gx-surface/40 hover:bg-gx-surface-hover cursor-pointer",
          "transition-colors",
        )}
      >
        <span className="inline-block h-2 w-2 rounded-full bg-gx-info animate-pulse shrink-0" />
        <span className="text-gx-muted">
          Working… {elapsedStr}
        </span>
      </button>
    );
  }

  // ── Expanded ─────────────────────────────────────────────────────
  return (
    <div className="space-y-1">
      {/* Disclosure header — only shown when the group is complete.
          While running, cards render directly without a wrapper so the
          user sees tool activity in real time without extra chrome. */}
      {complete && (
        <button
          type="button"
          onClick={handleToggle}
          aria-expanded={true}
          className={clsx(
            "flex w-full items-center gap-2 px-2.5 py-1",
            "text-[10px] text-left rounded-md",
            "hover:bg-gx-surface-hover cursor-pointer",
            "transition-colors",
          )}
        >
          <ChevronRight size={10} className="shrink-0 text-gx-muted/60 rotate-90 transition-transform" />
          <span className="text-gx-muted">
            {memberCount} tool call{memberCount !== 1 ? "s" : ""}
            {durationStr && <span className="ml-1">· {durationStr}</span>}
          </span>
        </button>
      )}

      {/* Render each member through its card renderer */}
      <div className={clsx("space-y-1", complete && "pl-2")}>
        {members.map((event, idx) => {
          const cardId = event.itemId ?? `${groupId}:${idx}`;
          const cardEntry = taskState?.cardCollapse[cardId];
          const cardCollapsed = cardEntry?.collapsed ?? (complete && defaultCollapsed);

          return (
            <div key={cardId}>
              {cardRenderers(event, {
                collapsed: cardCollapsed,
                onToggleCollapse: (next: boolean) => {
                  store.setCardCollapsed(taskId, cardId, next);
                },
              })}
            </div>
          );
        })}
      </div>
    </div>
  );
}
