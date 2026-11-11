/**
 * Single Turn renderer.
 *
 * Renders: TurnHeader → message column (interleaved text blocks and
 * tool GROUPS) → TurnSummaryCard (when applicable).
 *
 * The message column renders all events in arrival order within a
 * single column so that text → tool → text sequences appear as ONE
 * message column with interleaved text blocks and collapsible tool
 * group batches.
 *
 * The TurnHeader is the only top-level collapsible element. Inside
 * the turn body, tool groups provide batch-level collapse
 * ("▸ 15 tool calls · 0.5s").
 */

import { memo, useCallback, useMemo } from "react";
import { Streamdown } from "streamdown";
import type { Turn as TurnType, ToolGroup as ToolGroupType } from "../../lib/turn-derivation";
import { useChatUIState } from "../../lib/chat-ui-state";
import { TurnHeader } from "./TurnHeader";
import { TurnSummaryCard } from "./TurnSummaryCard";
import { StreamingCursor } from "./StreamingCursor";
import { ToolGroup } from "./ToolGroup";
import { resolveToolCard, CommandSessionCard, FileWatchNotificationCard } from "./tool-cards";
import { ToolCard } from "./ToolCard";
import type { ToolEvent } from "../../../electron/agent-chat-messages";
import type { AgentEvent } from "../../../electron/orchestrator-adapters";

// ─── Props ───────────────────────────────────────────────────────────────────

export interface TurnProps {
  turn: TurnType;
  taskId: string;
  /** Called when the user clicks Review on the TurnSummaryCard. */
  onReview?: () => void;
  /** Agent adapter name, used for left-border accent colour. */
  agentName?: string;
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/**
 * Convert an AgentEvent to a ToolEvent for the tool card renderers.
 * The existing tool cards expect ToolEvent shape.
 */
function agentEventToToolEvent(event: AgentEvent): ToolEvent {
  if (event.type === "input_request" && event.inputRequest) {
    const request = event.inputRequest;
    const summary = request.kind === "elicitation" ? `${request.serverName}: ${request.message}` : "Agent questions";
    return { type: "generic", tool: "AskUserQuestion", summary, input: request, itemId: event.itemId };
  }
  if (event.type === "approval_request" && event.approval) {
    return { type: "generic", tool: "CodexApproval", summary: event.approval.summary, input: event.approval, itemId: event.itemId };
  }
  return {
    type: event.type === "command_run" || event.type === "command" ? "command" :
          event.type === "file_edit" ? "file_edit" :
          event.type === "file_read" ? "file_read" :
          event.type === "search" ? "search" :
          event.type === "web" ? "web" :
          event.type === "task" ? "task" :
          "generic",
    tool: event.tool ?? "",
    summary: event.path ?? event.command ?? event.tool ?? "",
    input: event.input,
    detail: event.diff ?? event.stdout ?? undefined,
    itemId: event.itemId,
  };
}

function isPendingRequest(tool: ToolEvent): boolean {
  if (tool.tool !== "AskUserQuestion" && tool.tool !== "CodexApproval") return false;
  const input = tool.input as { requestId?: string; resolved?: boolean } | undefined;
  return Boolean(input?.requestId && !input.resolved);
}

function requestNeedsAttention(tool: ToolEvent): boolean {
  const request = tool.tool === "AskUserQuestion" || tool.tool === "CodexApproval";
  const input = tool.input as { resolutionReason?: string } | undefined;
  return isPendingRequest(tool) || Boolean(request && input?.resolutionReason);
}

/**
 * Render a single tool event through the card registry.
 */
function renderToolCard(
  event: ToolEvent,
  props: { collapsed: boolean; onToggleCollapse: (next: boolean) => void },
) {
  const tool = event.tool ?? "";
  const SpecializedCard = resolveToolCard(tool);
  if (SpecializedCard) {
    return (
      <SpecializedCard
        event={event}
        collapsed={props.collapsed}
        onToggleCollapse={props.onToggleCollapse}
      />
    );
  }
  return (
    <ToolCard
      tool={tool}
      summary={event.summary}
      input={event.input}
      detail={event.detail}
    />
  );
}

// ─── Segment types for interleaved rendering ─────────────────────────────────

interface TextSegment {
  kind: "text";
  content: string;
}

interface ThinkingSegment {
  kind: "thinking";
  content: string;
}

interface ToolGroupSegment {
  kind: "tool-group";
  group: ToolGroupType;
  members: ToolEvent[];
}

interface ErrorSegment {
  kind: "error";
  content: string;
}

interface UserSegment {
  kind: "user";
  content: string;
}

interface CommandSessionSegment {
  kind: "command-session";
  sessionId: string;
  /** All events for this session (command_output deltas + optional command_exited). */
  events: AgentEvent[];
}

interface FileWatchSegment {
  kind: "file-watch";
  path: string;
  watchId: string;
  changeKind: string;
}

type Segment = TextSegment | ThinkingSegment | ToolGroupSegment | ErrorSegment | CommandSessionSegment | FileWatchSegment | UserSegment;

// ─── Component ───────────────────────────────────────────────────────────────

export const Turn = memo(function Turn({ turn, taskId, onReview }: TurnProps) {
  const store = useChatUIState();
  const taskState = store.byTask.get(taskId);
  const turnCollapseEntry = taskState?.turnCollapse[turn.turnIndex];
  // While running, always expanded. Only allow collapse after turn completes.
  const needsAttention = turn.events.some(event => event.type === "error" || requestNeedsAttention(agentEventToToolEvent(event)));
  const canCollapse = turn.complete && turn.outcome === "completed" && !needsAttention;
  const isCollapsed = canCollapse
    ? (turnCollapseEntry != null ? turnCollapseEntry.collapsed : !turn.events.some(event => event.type === "user_message"))
    : false;

  // Build segments: interleave text blocks and tool groups in arrival order
  const segments = useMemo(() => {
    return buildSegments(turn);
  }, [turn]);

  // Determine if this turn has any tool work (tool groups).
  // Pure text turns (no tool calls) don't need a TurnHeader or collapse.
  const hasToolWork = turn.toolGroups.length > 0;

  // When the turn is complete AND has tool work, extract the final text
  // segment to render outside the collapsible body (Codex summary pattern).
  const { bodySegments, summarySegment } = useMemo(() => {
    if (!turn.complete || segments.length === 0 || !hasToolWork) {
      return { bodySegments: segments, summarySegment: null };
    }
    // Find the last text segment that comes AFTER the last tool-group segment.
    let lastToolGroupIdx = -1;
    for (let i = segments.length - 1; i >= 0; i--) {
      if (segments[i].kind === "tool-group") {
        lastToolGroupIdx = i;
        break;
      }
    }
    // If there's a text segment after the last tool group, extract it as summary
    if (lastToolGroupIdx >= 0 && lastToolGroupIdx < segments.length - 1) {
      const lastSeg = segments[segments.length - 1];
      if (lastSeg.kind === "text") {
        return {
          bodySegments: segments.slice(0, -1),
          summarySegment: lastSeg as TextSegment,
        };
      }
    }
    return { bodySegments: segments, summarySegment: null };
  }, [segments, turn.complete, hasToolWork]);

  const handleReview = useCallback(() => {
    onReview?.();
  }, [onReview]);

  // ── Edge case: pure text turn (no tool calls) ──────────────────────
  // Render text directly without TurnHeader or collapse wrapper.
  if (!hasToolWork) {
    return (
      <div className="space-y-2">
        {turn.userMessage && (
          <div className="flex justify-end py-1">
            <div className="max-w-[85%] rounded-lg bg-primary/10 border border-primary/20 px-3 py-2 text-sm text-foreground">
              {turn.userMessage}
            </div>
          </div>
        )}
        {segments.map((segment, idx) => renderSegment(segment, idx, turn, segments, store, taskId))}
      </div>
    );
  }

  // ── Normal turn with tool work ─────────────────────────────────────
  return (
    <div className="space-y-2">
      {/* User message (if this isn't the first turn) */}
      {turn.userMessage && (
        <div className="flex justify-end py-1">
          <div className="max-w-[85%] rounded-lg bg-primary/10 border border-primary/20 px-3 py-2 text-sm text-foreground">
            {turn.userMessage}
          </div>
        </div>
      )}

      {/* Light divider between user message and AI response */}
      {turn.userMessage && (turn.complete || bodySegments.length > 0) && (
        <div className="border-t border-border/20" />
      )}

      {/* Turn header — only shown for COMPLETED turns. While running,
          content streams directly without a collapsible wrapper so the
          user sees everything in real time without extra clicks. */}
      {turn.complete && (
        <TurnHeader
          turn={turn}
          taskId={taskId}
          collapsed={isCollapsed}
          collapsible={canCollapse}
          onReview={handleReview}
        />
      )}

      {/* Message column body:
          - Running: always visible (no collapse wrapper).
          - Complete: collapsible via TurnHeader; hidden when collapsed. */}
      {(!turn.complete || !isCollapsed) && (
        <div className="space-y-2">
          {bodySegments.map((segment, idx) => renderSegment(segment, idx, turn, segments, store, taskId))}
        </div>
      )}

      {/* Final summary text — always visible outside the collapse (Codex pattern) */}
      {summarySegment && (
        <div className="prose prose-sm prose-invert max-w-none text-sm text-foreground">
          <Streamdown shikiTheme={["github-light", "github-dark"]}>
            {summarySegment.content}
          </Streamdown>
        </div>
      )}

      {/* Turn summary card (always visible for completed turns with file edits) */}
      {turn.complete && turn.fileEditCount > 0 && (
        <TurnSummaryCard turn={turn} onReview={handleReview} taskId={taskId} />
      )}
    </div>
  );
});

// ─── Segment renderer ────────────────────────────────────────────────────────

function renderSegment(
  segment: Segment,
  idx: number,
  turn: TurnType,
  allSegments: Segment[],
  _store: ReturnType<typeof useChatUIState>,
  taskId: string,
) {
  switch (segment.kind) {
    case "user":
      return <div key={`user-${idx}`} className="flex justify-end py-1">
        <div className="max-w-[85%] rounded-lg border border-primary/20 bg-primary/10 px-3 py-2 text-sm text-foreground">{segment.content}</div>
      </div>;
    case "text": {
      const isLastTextSegment =
        !turn.complete &&
        allSegments.slice(idx + 1).every((s) => s.kind !== "text");
      return (
        <div key={`text-${idx}`} className="prose prose-sm prose-invert max-w-none text-sm text-foreground">
          <Streamdown shikiTheme={["github-light", "github-dark"]}>
            {segment.content}
          </Streamdown>
          {isLastTextSegment && (
            <StreamingCursor active={!turn.complete} />
          )}
        </div>
      );
    }

    case "thinking":
      return (
        <div key={`thinking-${idx}`} className="text-xs text-muted-foreground/70 italic border-l-2 border-thinking/40 pl-2">
          <Streamdown shikiTheme={["github-light", "github-dark"]}>
            {segment.content}
          </Streamdown>
        </div>
      );

    case "tool-group":
      return (
        <ToolGroup
          key={`group-${segment.group.groupId || idx}`}
          groupId={segment.group.groupId}
          members={segment.members}
          complete={segment.group.complete && !segment.members.some(isPendingRequest)}
          keepExpanded={segment.members.some(requestNeedsAttention)}
          startedAt={segment.group.startedAt}
          endedAt={segment.group.endedAt}
          defaultCollapsed={turn.outcome === "completed" && !segment.members.some(requestNeedsAttention)}
          cardRenderers={renderToolCard}
          taskId={taskId}
        />
      );

    case "error":
      return (
        <div key={`error-${idx}`} className="rounded-md border border-destructive/30 bg-destructive/5 px-3 py-2 text-xs text-destructive">
          {segment.content}
        </div>
      );

    case "command-session":
      return (
        <CommandSessionCard
          key={`cmd-${segment.sessionId || idx}`}
          event={segment.events[segment.events.length - 1]}
          collapsed={false}
        />
      );

    case "file-watch":
      return (
        <FileWatchNotificationCard
          key={`fw-${segment.watchId || idx}`}
          event={{
            type: "filesystem_change",
            timestamp: Date.now(),
            path: segment.path,
            fsWatchId: segment.watchId,
            fsChangeKind: segment.changeKind,
          }}
          collapsed={true}
        />
      );

    default:
      return null;
  }
}

// ─── Segment builder ─────────────────────────────────────────────────────────

// ─── Segment builder ─────────────────────────────────────────────────────────

/**
 * Build interleaved segments from a turn's events.
 *
 * Tool events are grouped into ToolGroupSegments using the turn's
 * pre-computed `toolGroups` from `deriveTurns()`. Non-empty text
 * events between tool batches create the natural group boundaries.
 * Each ToolGroupSegment renders as a collapsible batch
 * ("▸ 15 tool calls · 0.5s").
 */
function buildSegments(turn: TurnType): Segment[] {
  const segments: Segment[] = [];
  const events = turn.events;

  // Build a set of event indices that belong to tool groups,
  // and map each index to its group.
  const indexToGroup = new Map<number, ToolGroupType>();
  for (const group of turn.toolGroups) {
    for (const idx of group.eventIndices) {
      indexToGroup.set(idx, group);
    }
  }

  let pendingText = "";
  let pendingTextItem: string | undefined;
  let pendingThinking = "";
  // Track which groups we've already emitted
  const emittedGroups = new Set<ToolGroupType>();

  for (let i = 0; i < events.length; i++) {
    const event = events[i];

    if (event.type === "user_message") {
      if (pendingText) { segments.push({ kind: "text", content: pendingText }); pendingText = ""; }
      if (pendingThinking) { segments.push({ kind: "thinking", content: pendingThinking }); pendingThinking = ""; }
      segments.push({ kind: "user", content: event.content ?? "" });
      continue;
    }

    // Skip tool_result, done, and usage events in the segment output
    if (event.type === "tool_result" || event.type === "done" || event.type === "usage") {
      continue;
    }

    // Text events
    if (event.type === "text" && event.content) {
      if (pendingText && event.itemId !== pendingTextItem) {
        segments.push({ kind: "text", content: pendingText });
        pendingText = "";
      }
      if (pendingThinking) {
        segments.push({ kind: "thinking", content: pendingThinking });
        pendingThinking = "";
      }
      pendingText += event.content;
      pendingTextItem = event.itemId;
      continue;
    }

    // Thinking events
    if (event.type === "thinking" && event.content) {
      if (pendingText) {
        segments.push({ kind: "text", content: pendingText });
        pendingText = "";
      }
      pendingThinking += event.content;
      continue;
    }

    // Error events
    if (event.type === "error" && (event.error || event.content)) {
      if (pendingText) {
        segments.push({ kind: "text", content: pendingText });
        pendingText = "";
      }
      if (pendingThinking) {
        segments.push({ kind: "thinking", content: pendingThinking });
        pendingThinking = "";
      }
      segments.push({ kind: "error", content: event.error ?? event.content ?? "" });
      continue;
    }

    // Command-session events — group consecutive events by sessionId
    if (event.type === "command_output" || event.type === "command_exited") {
      if (pendingText) {
        segments.push({ kind: "text", content: pendingText });
        pendingText = "";
      }
      if (pendingThinking) {
        segments.push({ kind: "thinking", content: pendingThinking });
        pendingThinking = "";
      }
      // Collect all consecutive command_output/command_exited events for same session
      const sessionId = String(event.commandSessionId ?? "");
      const sessionEvents: AgentEvent[] = [event];
      let j = i + 1;
      while (j < events.length) {
        const next = events[j];
        if (
          (next.type === "command_output" || next.type === "command_exited") &&
          String(next.commandSessionId ?? "") === sessionId
        ) {
          sessionEvents.push(next);
          j++;
        } else {
          break;
        }
      }
      segments.push({
        kind: "command-session",
        sessionId,
        events: sessionEvents,
      });
      i = j - 1; // skip the events we consumed
      continue;
    }

    // Filesystem change events — render as inline notification
    if (event.type === "filesystem_change") {
      if (pendingText) {
        segments.push({ kind: "text", content: pendingText });
        pendingText = "";
      }
      if (pendingThinking) {
        segments.push({ kind: "thinking", content: pendingThinking });
        pendingThinking = "";
      }
      segments.push({
        kind: "file-watch",
        path: String(event.path ?? ""),
        watchId: String(event.fsWatchId ?? ""),
        changeKind: String(event.fsChangeKind ?? "changed"),
      });
      continue;
    }

    // Tool events — emit as part of their ToolGroup
    const group = indexToGroup.get(i);
    if (group && !emittedGroups.has(group)) {
      if (pendingText) {
        segments.push({ kind: "text", content: pendingText });
        pendingText = "";
      }
      if (pendingThinking) {
        segments.push({ kind: "thinking", content: pendingThinking });
        pendingThinking = "";
      }

      // Collect ToolEvent members for this group
      const members: ToolEvent[] = group.eventIndices.map((idx) =>
        agentEventToToolEvent(events[idx]),
      );

      segments.push({
        kind: "tool-group",
        group,
        members,
      });
      emittedGroups.add(group);
      continue;
    }

    // If this event is part of an already-emitted group, skip it
    if (group && emittedGroups.has(group)) {
      continue;
    }
  }

  // Flush any remaining text
  if (pendingText) {
    segments.push({ kind: "text", content: pendingText });
  }
  if (pendingThinking) {
    segments.push({ kind: "thinking", content: pendingThinking });
  }

  return segments;
}
