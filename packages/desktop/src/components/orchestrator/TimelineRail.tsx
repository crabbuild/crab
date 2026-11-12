import { Button } from "@/components/ui/button";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";
/**
 * TimelineRail — vertical mini-rail rendered along the left side of the
 * chat column showing dots for each user message and tool call, coloured
 * by status. Click jumps the chat to that anchor. Hover shows a tooltip.
 *
 * Visibility controlled by `useChatUIState().timelineRail` per task.
 * When event count > 200, uses @tanstack/react-virtual to render only
 * visible dots plus a buffer of 50 above/below.
 */

import { useVirtualizer } from "@tanstack/react-virtual";
import { useCallback, useMemo, useRef, useState } from "react";
import type { AgentEvent } from "../../../electron/orchestrator-adapters";

// ─── Types ───────────────────────────────────────────────────────────────────

export interface TimelineRailProps {
  taskId: string;
  events: AgentEvent[];
  feedback: string[];
  onScrollTo: (eventIndex: number) => void;
}

interface DotEntry {
  /** Index into the original events array. */
  eventIndex: number;
  /** Whether this is a user message dot (larger) or tool dot (smaller). */
  kind: "user" | "tool";
  /** Colour token class name. */
  color: "info" | "success" | "error" | "warning";
  /** Tooltip label. */
  label: string;
  /** One-line summary for the tooltip. */
  summary: string;
  /** Formatted timestamp. */
  time: string;
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

const VIRTUALISATION_THRESHOLD = 200;
const DOT_HEIGHT = 14; // px per dot row

function formatTime(ts: number): string {
  const d = new Date(ts);
  return d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

/**
 * Determine the status colour for a tool event by looking ahead for a
 * matching tool_result in the events array.
 */
function resolveToolColor(
  event: AgentEvent,
  events: AgentEvent[],
  eventIndex: number,
): "info" | "success" | "error" | "warning" {
  if (event.type === "approval_request" || event.type === "input_request") return "warning";

  // Look for a matching tool_result by itemId
  if (event.itemId) {
    for (let i = eventIndex + 1; i < events.length; i++) {
      const e = events[i];
      if (e.itemId === event.itemId && e.type === "tool_result") {
        if (e.error || (e.exitCode !== undefined && e.exitCode !== 0)) {
          return "error";
        }
        return "success";
      }
    }
  }

  // No result found yet — still in progress
  return "info";
}

function buildSummary(event: AgentEvent): string {
  if (event.command) {
    const cmd = event.command.length > 200 ? event.command.slice(0, 200) + "…" : event.command;
    return cmd;
  }
  if (event.path) {
    const p = event.path.length > 200 ? event.path.slice(0, 200) + "…" : event.path;
    return p;
  }
  if (event.content) {
    const c = event.content.length > 200 ? event.content.slice(0, 200) + "…" : event.content;
    return c;
  }
  return "";
}

function buildLabel(event: AgentEvent): string {
  if (event.tool) return event.tool;
  switch (event.type) {
    case "file_edit": return "File edit";
    case "file_read": return "File read";
    case "command":
    case "command_run": return "Command";
    case "search": return "Search";
    case "web": return "Web fetch";
    case "approval_request": return "Approval";
    case "input_request": return "Question";
    case "tool_use": return event.tool ?? "Tool";
    default: return event.type;
  }
}

// ─── Dot derivation ──────────────────────────────────────────────────────────

function deriveDots(events: AgentEvent[], feedback: string[]): DotEntry[] {
  const dots: DotEntry[] = [];
  let feedbackIdx = 0;

  for (let i = 0; i < events.length; i++) {
    const event = events[i];

    // Skip non-visual event types
    if (
      event.type === "text" ||
      event.type === "thinking" ||
      event.type === "error" ||
      event.type === "usage" ||
      event.type === "thread_started" ||
      event.type === "tool_result"
    ) {
      continue;
    }

    // Done events represent user turn boundaries — render as user dots
    if (event.type === "done") {
      // The next user message (from feedback) corresponds to this boundary
      if (feedbackIdx < feedback.length) {
        const msg = feedback[feedbackIdx];
        dots.push({
          eventIndex: i,
          kind: "user",
          color: "success",
          label: "User message",
          summary: msg.length > 200 ? msg.slice(0, 200) + "…" : msg,
          time: formatTime(event.timestamp),
        });
        feedbackIdx++;
      }
      continue;
    }

    // Tool events (tool_use, file_edit, file_read, command, search, web, etc.)
    dots.push({
      eventIndex: i,
      kind: "tool",
      color: resolveToolColor(event, events, i),
      label: buildLabel(event),
      summary: buildSummary(event),
      time: formatTime(event.timestamp),
    });
  }

  return dots;
}

// ─── Colour mapping ──────────────────────────────────────────────────────────

const colorClasses: Record<DotEntry["color"], string> = {
  info: "bg-info",
  success: "bg-success",
  error: "bg-destructive",
  warning: "bg-warning",
};

// ─── Component ───────────────────────────────────────────────────────────────

export function TimelineRail({ events, feedback, onScrollTo }: TimelineRailProps) {
  const dots = useMemo(() => deriveDots(events, feedback), [events, feedback]);
  const parentRef = useRef<HTMLDivElement>(null);
  const [hoveredIdx, setHoveredIdx] = useState<number | null>(null);

  const useVirtual = dots.length > VIRTUALISATION_THRESHOLD;

  const virtualizer = useVirtualizer({
    count: dots.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => DOT_HEIGHT,
    overscan: 50,
    enabled: useVirtual,
  });

  const handleDotClick = useCallback(
    (eventIndex: number) => {
      onScrollTo(eventIndex);
    },
    [onScrollTo],
  );

  if (dots.length === 0) return null;

  // Non-virtualised render for small transcripts
  if (!useVirtual) {
    return (
      <div
        ref={parentRef}
        className="timeline-rail relative flex flex-col items-center gap-px py-2"
        style={{ width: 12 }}
        role="navigation"
        aria-label="Timeline rail"
      >
        {dots.map((dot, idx) => (
          <DotItem
            key={idx}
            dot={dot}
            isHovered={hoveredIdx === idx}
            onHover={() => setHoveredIdx(idx)}
            onLeave={() => setHoveredIdx(null)}
            onClick={() => handleDotClick(dot.eventIndex)}
          />
        ))}
      </div>
    );
  }

  // Virtualised render for large transcripts
  return (
    <div
      ref={parentRef}
      className="timeline-rail relative flex flex-col items-center overflow-auto py-2"
      style={{ width: 12, height: "100%" }}
      role="navigation"
      aria-label="Timeline rail"
    >
      <div
        style={{
          height: virtualizer.getTotalSize(),
          width: "100%",
          position: "relative",
        }}
      >
        {virtualizer.getVirtualItems().map((virtualRow) => {
          const dot = dots[virtualRow.index];
          return (
            <div
              key={virtualRow.index}
              style={{
                position: "absolute",
                top: 0,
                left: 0,
                width: "100%",
                height: DOT_HEIGHT,
                transform: `translateY(${virtualRow.start}px)`,
              }}
              className="flex items-center justify-center"
            >
              <DotItem
                dot={dot}
                isHovered={hoveredIdx === virtualRow.index}
                onHover={() => setHoveredIdx(virtualRow.index)}
                onLeave={() => setHoveredIdx(null)}
                onClick={() => handleDotClick(dot.eventIndex)}
              />
            </div>
          );
        })}
      </div>
    </div>
  );
}

// ─── DotItem ─────────────────────────────────────────────────────────────────

function DotItem({
  dot,
  isHovered,
  onHover,
  onLeave,
  onClick,
}: {
  dot: DotEntry;
  isHovered: boolean;
  onHover: () => void;
  onLeave: () => void;
  onClick: () => void;
}) {
  const size = dot.kind === "user" ? 8 : 5;

  return (
    <div
      className="flex items-center justify-center"
      style={{ height: DOT_HEIGHT }}
    >
      <TooltipProvider>
        <Tooltip
          open={isHovered}
          onOpenChange={(open) => {
            if (open) onHover();
            else onLeave();
          }}
        >
          <TooltipTrigger asChild>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className={`rounded-full p-0 transition-transform hover:scale-125 ${colorClasses[dot.color]}`}
              style={{ width: size, height: size }}
              onClick={onClick}
              aria-label={`${dot.label}: ${dot.summary || "no details"}`}
            />
          </TooltipTrigger>
          <TooltipContent
            side="right"
            className="whitespace-nowrap px-2 py-1 text-[10px]"
          >
            <div className="font-medium">{dot.label}</div>
            {dot.summary && (
              <div className="max-w-[200px] truncate opacity-75">
                {dot.summary}
              </div>
            )}
            <div className="opacity-75">{dot.time}</div>
          </TooltipContent>
        </Tooltip>
      </TooltipProvider>
    </div>
  );
}
