/**
 * Top-level chat transcript component.
 *
 * Replaces the flat message-column rendering in AgentTasksView with a
 * structured turn-by-turn transcript. Calls `deriveTurns(events, feedback)`
 * and maps each Turn to a `<Turn>` instance.
 *
 * Memoises per turn so a new event in the active turn doesn't re-render
 * completed turns.
 *
 * When the total event count exceeds 200, the message column switches to
 * virtualised rendering via `@tanstack/react-virtual` to keep large
 * transcripts performant. Below that threshold, flat rendering is used so
 * browser Cmd+F works on the full transcript.
 *
 * Props:
 *   - taskId: the active task's identifier
 *   - messages: the ChatMessage[] array from the renderer's message state
 *   - feedback: the user feedback strings (one per iteration)
 *   - isStreaming: whether the task is currently streaming
 *   - onReview: callback to open the Diff tab
 */

import { useMemo, useRef, useCallback, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { LayoutList } from "lucide-react";
import type { AgentEvent } from "../../../electron/orchestrator-adapters";
import type { ChatMessage, ToolEvent } from "./chat-types";
import { deriveTurns } from "../../lib/turn-derivation";
import { Turn } from "./Turn";
import { TimelineRail } from "./TimelineRail";
import { HunkActionsProvider } from "./tool-cards/HunkActionsContext";
import { useChatUIState } from "../../lib/chat-ui-state";

// ─── Props ───────────────────────────────────────────────────────────────────

export interface ChatTranscriptProps {
  taskId: string;
  /** The ChatMessage array from the renderer's message state. */
  messages: ChatMessage[];
  /** User feedback strings (one per iteration after the first turn). */
  feedback: string[];
  /** Whether the task is currently streaming events. */
  isStreaming?: boolean;
  /** Called when the user clicks Review on a TurnSummaryCard. */
  onReview?: () => void;
  /** Agent adapter name, used for left-border accent colour. */
  agentName?: string;
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/**
 * Convert the renderer's ChatMessage[] into AgentEvent[] for deriveTurns.
 *
 * The renderer accumulates events into ChatMessage objects (text messages,
 * tool events attached to messages, thinking messages, error messages, done
 * messages). We flatten these back into the AgentEvent stream that
 * deriveTurns expects.
 */
function messagesToAgentEvents(messages: ChatMessage[]): AgentEvent[] {
  const events: AgentEvent[] = [];

  for (const msg of messages) {
    if (msg.role === "user") {
      // User messages don't map to AgentEvents — they're captured in feedback
      continue;
    }

    // Agent text messages
    if (msg.content && (msg.kind === "text" || msg.kind === undefined)) {
      events.push({
        type: "text",
        timestamp: msg.timestamp,
        content: msg.content,
      });
    }

    // Done messages that carry content: emit the text first (so it
    // renders in the turn body) then the done marker (so deriveTurns
    // splits turns correctly). This handles the case where a streamed
    // text message was later marked as `kind: "done"` by the dedup
    // logic — without the text event, the content would vanish.
    if (msg.kind === "done" && msg.content) {
      events.push({
        type: "text",
        timestamp: msg.timestamp,
        content: msg.content,
      });
      events.push({
        type: "done",
        timestamp: msg.timestamp,
        content: msg.content,
      });
    }

    // Done messages without content: just the boundary marker.
    if (msg.kind === "done" && !msg.content) {
      events.push({
        type: "done",
        timestamp: msg.timestamp,
        content: "",
      });
    }

    // Thinking messages
    if (msg.kind === "thinking" && msg.content) {
      events.push({
        type: "thinking",
        timestamp: msg.timestamp,
        content: msg.content,
      });
    }

    // Error messages
    if (msg.kind === "error" && msg.content) {
      events.push({
        type: "error",
        timestamp: msg.timestamp,
        error: msg.content,
        content: msg.content,
      });
    }

    // Tool events attached to this message
    if (msg.events && msg.events.length > 0) {
      for (const toolEvt of msg.events) {
        events.push(toolEventToAgentEvent(toolEvt, msg.timestamp));
      }
    }
  }

  // If there's no explicit done event but we have events, add a synthetic
  // done at the end so deriveTurns produces at least one complete turn
  // (unless the task is still streaming, in which case the turn is open)
  return events;
}

/**
 * Convert a ToolEvent back to an AgentEvent for deriveTurns.
 */
function toolEventToAgentEvent(toolEvt: ToolEvent, fallbackTimestamp: number): AgentEvent {
  const base: AgentEvent = {
    type: "tool_use",
    timestamp: fallbackTimestamp,
    tool: toolEvt.tool,
    input: toolEvt.input,
    itemId: toolEvt.itemId,
  };

  // Map ToolEvent types back to AgentEvent types
  switch (toolEvt.type) {
    case "file_edit":
      return {
        ...base,
        type: "file_edit",
        path: extractPath(toolEvt),
        diff: toolEvt.detail,
      };
    case "file_read":
      return {
        ...base,
        type: "file_read",
        path: extractPath(toolEvt),
      };
    case "command":
      return {
        ...base,
        type: "command",
        command: extractCommand(toolEvt),
        stdout: toolEvt.detail,
      };
    case "search":
      return {
        ...base,
        type: "search",
        path: extractPath(toolEvt),
      };
    case "web":
      return {
        ...base,
        type: "web",
      };
    case "task":
      return {
        ...base,
        type: "task",
      };
    case "thinking":
      return {
        ...base,
        type: "thinking",
        content: toolEvt.detail ?? toolEvt.summary,
      };
    default:
      return {
        ...base,
        type: "tool_use",
      };
  }
}

function extractPath(toolEvt: ToolEvent): string | undefined {
  const input = toolEvt.input as Record<string, unknown> | undefined;
  return (input?.file_path ?? input?.path ?? toolEvt.summary?.split(": ")[1]) as string | undefined;
}

function extractCommand(toolEvt: ToolEvent): string | undefined {
  const input = toolEvt.input as Record<string, unknown> | undefined;
  return (input?.command ?? toolEvt.summary?.split(": ")[1]) as string | undefined;
}

// ─── Constants ───────────────────────────────────────────────────────────────

/**
 * When the total event count exceeds this threshold, the message column
 * switches to virtualised rendering. Below this, flat rendering is used
 * so browser Cmd+F works on the full transcript.
 */
const VIRTUALISATION_THRESHOLD = 200;

/**
 * Estimated height (px) for a Turn component before measurement.
 * After first render, each row self-measures for accurate layout.
 */
const ESTIMATED_TURN_HEIGHT = 120;

/**
 * Number of off-screen items to render above/below the viewport.
 * Keeps scroll smooth and preserves keyboard focus context.
 */
const OVERSCAN_COUNT = 5;

// ─── Component ───────────────────────────────────────────────────────────────

export function ChatTranscript({
  taskId,
  messages,
  feedback,
  isStreaming,
  onReview,
  agentName,
}: ChatTranscriptProps) {
  // Chat UI state for timeline rail visibility — only read the specific
  // task's timelineRail setting to avoid re-renders from other tasks.
  const chatUIState = useChatUIState();
  const timelineRail = chatUIState.byTask.get(taskId)?.timelineRail ?? "off";

  // Timeline rail density menu state
  const [railMenuOpen, setRailMenuOpen] = useState(false);

  // Convert messages to AgentEvents and derive turns
  const agentEvents = useMemo(
    () => messagesToAgentEvents(messages),
    [messages],
  );

  // Extract user feedback from messages (user messages after the first)
  const derivedFeedback = useMemo(() => {
    if (feedback.length > 0) return feedback;
    // Fallback: extract from user messages in the message array
    const userMessages = messages
      .filter((m) => m.role === "user")
      .map((m) => m.content);
    // Skip the first user message (it's the initial prompt, not feedback)
    return userMessages.slice(1);
  }, [feedback, messages]);

  const rawTurns = useMemo(
    () => deriveTurns(agentEvents, derivedFeedback),
    [agentEvents, derivedFeedback],
  );

  // Fix turn completion status:
  // 1. ALL turns except the last one are always marked complete (they
  //    finished before the current turn started, even if they lack a
  //    `done` event in the messages array).
  // 2. The last turn is marked complete only when `isStreaming` is false
  //    (task finished) and it doesn't already have a `done` event.
  const turns = useMemo(() => {
    if (rawTurns.length === 0) return rawTurns;

    let needsFix = false;

    // Check if any non-last turn is incomplete (shouldn't be, but fix it)
    for (let i = 0; i < rawTurns.length - 1; i++) {
      if (!rawTurns[i].complete) {
        needsFix = true;
        break;
      }
    }

    // Check if the last turn needs the isStreaming override
    const lastTurn = rawTurns[rawTurns.length - 1];
    if (!isStreaming && !lastTurn.complete) {
      needsFix = true;
    }

    if (!needsFix) return rawTurns;

    const fixed = rawTurns.map((turn, idx) => {
      const isLast = idx === rawTurns.length - 1;

      // Non-last turns: always complete
      if (!isLast && !turn.complete) {
        const events = turn.events;
        const firstTs = events[0]?.timestamp ?? Date.now();
        const lastTs = events[events.length - 1]?.timestamp ?? Date.now();
        return {
          ...turn,
          complete: true,
          durationMs: lastTs - firstTs,
          toolGroups: turn.toolGroups.map((g) => ({
            ...g,
            complete: true,
            endedAt: g.endedAt ?? lastTs,
          })),
        };
      }

      // Last turn: only mark complete when streaming has stopped
      if (isLast && !isStreaming && !turn.complete) {
        const events = turn.events;
        const firstTs = events[0]?.timestamp ?? Date.now();
        const lastTs = events[events.length - 1]?.timestamp ?? Date.now();
        return {
          ...turn,
          complete: true,
          durationMs: lastTs - firstTs,
          toolGroups: turn.toolGroups.map((g) => ({
            ...g,
            complete: true,
            endedAt: g.endedAt ?? lastTs,
          })),
        };
      }

      return turn;
    });

    return fixed;
  }, [rawTurns, isStreaming]);

  // Count total events across all turns to decide virtualisation
  const totalEventCount = useMemo(
    () => agentEvents.length,
    [agentEvents],
  );

  const shouldVirtualise = totalEventCount > VIRTUALISATION_THRESHOLD;

  // Scroll-to handler for the timeline rail
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const handleScrollTo = useCallback((eventIndex: number) => {
    // Find the turn that contains this event index and scroll to it.
    // For now, scroll the container to bring the approximate position into view.
    const container = scrollContainerRef.current;
    if (!container) return;
    // Approximate: each event maps to roughly (eventIndex / totalEvents) * scrollHeight
    const fraction = totalEventCount > 0 ? eventIndex / totalEventCount : 0;
    container.scrollTo({
      top: fraction * container.scrollHeight,
      behavior: "smooth",
    });
  }, [totalEventCount]);

  // Extract the first user message (initial prompt) from messages
  const firstUserMessage = useMemo(() => {
    const userMsg = messages.find((m) => m.role === "user");
    return userMsg?.content ?? null;
  }, [messages]);

  if (turns.length === 0 && messages.length === 0) {
    return null;
  }

  // If deriveTurns produced no turns but we have messages, fall back to
  // rendering a single open turn with all events
  if (turns.length === 0 && agentEvents.length > 0) {
    const fallbackTurn = {
      turnIndex: 0,
      userMessage: null,
      events: agentEvents,
      toolGroups: [],
      fileEditCount: 0,
      fileEditPaths: [],
      totalAdded: 0,
      totalRemoved: 0,
      complete: false,
      durationMs: null,
    };
    return (
      <HunkActionsProvider value={{ taskId }}>
        <div className="flex">
          {timelineRail !== "off" && (
            <TimelineRail
              taskId={taskId}
              events={agentEvents}
              feedback={derivedFeedback}
              onScrollTo={handleScrollTo}
            />
          )}
          <div className="flex-1 space-y-4">
            {firstUserMessage && (
              <div className="flex justify-end py-1">
                <div className="max-w-[85%] rounded-lg bg-gx-accent/10 border border-gx-accent/20 px-3 py-2 text-sm text-gx-text">
                  {firstUserMessage}
                </div>
              </div>
            )}
            <Turn turn={fallbackTurn} taskId={taskId} onReview={onReview} agentName={agentName} />
          </div>
        </div>
      </HunkActionsProvider>
    );
  }

  if (shouldVirtualise) {
    return (
      <HunkActionsProvider value={{ taskId }}>
        <div className="relative">
          {timelineRail !== "off" && (
            <TimelineRailDensityToggle
              taskId={taskId}
              timelineRail={timelineRail}
              open={railMenuOpen}
              onToggle={() => setRailMenuOpen((o) => !o)}
              onSelect={(density) => {
                chatUIState.setTimelineRail(taskId, density);
                setRailMenuOpen(false);
              }}
            />
          )}
          <div className="flex">
            {timelineRail !== "off" && (
              <TimelineRail
                taskId={taskId}
                events={agentEvents}
                feedback={derivedFeedback}
                onScrollTo={handleScrollTo}
              />
            )}
            <div className="flex-1 min-w-0">
              {firstUserMessage && (
                <div className="flex justify-end py-1 mb-4">
                  <div className="max-w-[85%] rounded-lg bg-gx-accent/10 border border-gx-accent/20 px-3 py-2 text-sm text-gx-text">
                    {firstUserMessage}
                  </div>
                </div>
              )}
              <VirtualisedTranscript
                turns={turns}
                taskId={taskId}
                onReview={onReview}
                agentName={agentName}
                scrollRef={scrollContainerRef}
              />
            </div>
          </div>
        </div>
      </HunkActionsProvider>
    );
  }

  return (
    <HunkActionsProvider value={{ taskId }}>
      <div className="relative">
        {timelineRail !== "off" && (
          <TimelineRailDensityToggle
            taskId={taskId}
            timelineRail={timelineRail}
            open={railMenuOpen}
            onToggle={() => setRailMenuOpen((o) => !o)}
            onSelect={(density) => {
              chatUIState.setTimelineRail(taskId, density);
              setRailMenuOpen(false);
            }}
          />
        )}
        <div className="flex" ref={scrollContainerRef}>
          {timelineRail !== "off" && (
            <TimelineRail
              taskId={taskId}
              events={agentEvents}
              feedback={derivedFeedback}
              onScrollTo={handleScrollTo}
            />
          )}
          <div className="flex-1 space-y-4 min-w-0">
            {firstUserMessage && (
              <div className="flex justify-end py-1">
                <div className="max-w-[85%] rounded-lg bg-gx-accent/10 border border-gx-accent/20 px-3 py-2 text-sm text-gx-text">
                  {firstUserMessage}
                </div>
              </div>
            )}
            {turns.map((turn, i) => (
              <div key={`turn-${turn.turnIndex}`}>
                {/* Light divider between turns (not before the first) */}
                {i > 0 && (
                  <div className="my-3 border-t border-gx-border/30" />
                )}
                <Turn
                  turn={turn}
                  taskId={taskId}
                  onReview={onReview}
                  agentName={agentName}
                />
              </div>
            ))}
            {/* Pending user message: renders immediately when the user
                sends a follow-up but no new turn exists yet (the AI
                hasn't started responding). Feedback index `turns.length - 1`
                is the message for the *next* turn that doesn't exist yet. */}
            {derivedFeedback.length > turns.length - 1 && turns[turns.length - 1]?.complete && (() => {
              const pending = derivedFeedback[turns.length - 1];
              // Don't render if the last turn already shows this message
              // (happens when deriveTurns created the turn but it's still open).
              if (!pending) return null;
              const lastTurn = turns[turns.length - 1];
              if (lastTurn?.userMessage === pending) return null;
              return (
                <>
                  <div className="my-3 border-t border-gx-border/30" />
                  <div className="flex justify-end py-1">
                    <div className="max-w-[85%] rounded-lg bg-gx-accent/10 border border-gx-accent/20 px-3 py-2 text-sm text-gx-text">
                      {pending}
                    </div>
                  </div>
                </>
              );
            })()}
          </div>
        </div>
      </div>
    </HunkActionsProvider>
  );
}

// ─── Virtualised transcript ──────────────────────────────────────────────────

interface VirtualisedTranscriptProps {
  turns: ReturnType<typeof deriveTurns>;
  taskId: string;
  onReview?: () => void;
  agentName?: string;
  scrollRef?: React.RefObject<HTMLDivElement | null>;
}

/**
 * Virtualised message column for large transcripts (> 200 events).
 *
 * Uses `@tanstack/react-virtual` to render only visible Turn components
 * plus an overscan buffer. Each Turn self-measures on first render so
 * `estimateSize` becomes accurate after one frame.
 *
 * Scroll position is preserved across collapse/expand toggles because
 * the virtualizer recalculates layout from measured sizes. Keyboard
 * focus is maintained via the overscan buffer which keeps nearby turns
 * in the DOM.
 */
function VirtualisedTranscript({
  turns,
  taskId,
  onReview,
  agentName,
  scrollRef: externalScrollRef,
}: VirtualisedTranscriptProps) {
  const internalScrollRef = useRef<HTMLDivElement>(null);
  const scrollRef = externalScrollRef ?? internalScrollRef;

  const virtualizer = useVirtualizer({
    count: turns.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ESTIMATED_TURN_HEIGHT,
    overscan: OVERSCAN_COUNT,
  });

  // Notify the virtualizer when turns change size (collapse/expand).
  // The virtualizer's measureElement callback handles this automatically
  // when we pass `ref` via `measureElement` to each row.
  const measureElement = useCallback(
    (el: HTMLDivElement | null) => {
      if (el) {
        virtualizer.measureElement(el);
      }
    },
    [virtualizer],
  );

  // Combine refs: assign the DOM element to both the internal ref and
  // the external ref (if provided) so the virtualizer and the parent
  // scroll-to handler both work.
  const setScrollRef = useCallback(
    (el: HTMLDivElement | null) => {
      (internalScrollRef as React.MutableRefObject<HTMLDivElement | null>).current = el;
      if (externalScrollRef) {
        (externalScrollRef as React.MutableRefObject<HTMLDivElement | null>).current = el;
      }
    },
    [externalScrollRef],
  );

  return (
    <div
      ref={setScrollRef}
      className="min-h-0 flex-1 overflow-auto"
      role="log"
      aria-label="Chat transcript"
    >
      <div
        className="relative"
        style={{ height: virtualizer.getTotalSize() }}
      >
        {virtualizer.getVirtualItems().map((virtualRow) => {
          const turn = turns[virtualRow.index];
          return (
            <div
              key={`turn-${turn.turnIndex}`}
              ref={measureElement}
              data-index={virtualRow.index}
              className="absolute left-0 top-0 w-full"
              style={{
                transform: `translateY(${virtualRow.start}px)`,
              }}
            >
              <div className="pb-4">
                <Turn
                  turn={turn}
                  taskId={taskId}
                  onReview={onReview}
                  agentName={agentName}
                />
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

// ─── Timeline Rail Density Toggle ────────────────────────────────────────────

interface TimelineRailDensityToggleProps {
  taskId: string;
  timelineRail: "compact" | "off";
  open: boolean;
  onToggle: () => void;
  onSelect: (density: "compact" | "off") => void;
}

/**
 * Small menu button rendered in the top-right corner of the transcript
 * area. Offers "Timeline rail" density options: compact / off.
 * Persists via `setTimelineRail(taskId, …)`.
 */
function TimelineRailDensityToggle({
  timelineRail,
  open,
  onToggle,
  onSelect,
}: TimelineRailDensityToggleProps) {
  const menuRef = useRef<HTMLDivElement>(null);

  return (
    <div className="absolute right-0 top-0 z-10" ref={menuRef}>
      <button
        type="button"
        onClick={onToggle}
        className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover transition"
        aria-label="Timeline rail settings"
        aria-expanded={open}
        aria-haspopup="menu"
        title="Timeline rail"
      >
        <LayoutList size={11} />
      </button>
      {open && (
        <div
          role="menu"
          className="absolute right-0 top-full mt-1 min-w-[140px] rounded-lg border border-gx-border bg-gx-bg shadow-xl overflow-hidden z-50"
        >
          <div className="px-3 py-1.5 border-b border-gx-border text-[10px] font-semibold uppercase tracking-wider text-gx-muted">
            Timeline rail
          </div>
          <button
            type="button"
            role="menuitem"
            onClick={() => onSelect("compact")}
            className={`flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs transition ${
              timelineRail === "compact"
                ? "bg-gx-accent/10 text-gx-text"
                : "text-gx-text hover:bg-gx-surface-hover"
            }`}
          >
            <span className="flex-1">Compact</span>
            {timelineRail === "compact" && (
              <span className="text-gx-accent text-[10px]">✓</span>
            )}
          </button>
          <button
            type="button"
            role="menuitem"
            onClick={() => onSelect("off")}
            className={`flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs transition ${
              timelineRail === "off"
                ? "bg-gx-accent/10 text-gx-text"
                : "text-gx-text hover:bg-gx-surface-hover"
            }`}
          >
            <span className="flex-1">Off</span>
            {timelineRail === "off" && (
              <span className="text-gx-accent text-[10px]">✓</span>
            )}
          </button>
        </div>
      )}
    </div>
  );
}
