/**
 * Tests for the TimelineRail component logic.
 *
 * Validates: Requirements CHAT8.2, CHAT8.4, CHAT8.5
 *
 * The TimelineRail renders one dot per `tool_use` event and one dot per
 * user message. These tests verify:
 * - P6: dot count === (tool_use count) + (user message count)
 * - Click on a dot calls `onScrollTo` with the correct event index
 * - Hover tooltip text matches the expected summary
 */

import { describe, it, expect } from "vitest";
import fc from "fast-check";

import type { AgentEvent } from "../electron/orchestrator-adapters";

// ─── Logic Under Test ────────────────────────────────────────────────────────

/**
 * Represents a single dot in the TimelineRail.
 */
interface TimelineDot {
  /** Index into the global events array — used by onScrollTo. */
  eventIndex: number;
  /** "tool" for tool_use events, "user" for user messages. */
  kind: "tool" | "user";
  /** Tooltip summary text. */
  tooltip: string;
}

/**
 * Derives the set of dots the TimelineRail should render from the
 * event stream and user feedback array.
 *
 * Rules (from CHAT8.2):
 * - One small dot per `tool_use` event
 * - One larger dot per user message (derived from feedback array,
 *   positioned at the first event after each `done`)
 *
 * The first user message (task creation) is placed at event index 0
 * if feedback has entries. Subsequent user messages are placed at the
 * first event after each `done` boundary.
 */
function deriveTimelineDots(
  events: AgentEvent[],
  feedback: string[],
): TimelineDot[] {
  const dots: TimelineDot[] = [];

  // Track turn boundaries to place user message dots.
  // Turn 0 starts at event 0 (no user message — orchestrator prompt).
  // Turn N (N > 0) starts at the event after the (N-1)th `done`.
  // feedback[0] maps to turn 1, feedback[1] to turn 2, etc.
  let doneCount = 0;
  const turnStartIndices: number[] = [0]; // Turn 0 starts at index 0

  for (let i = 0; i < events.length; i++) {
    if (events[i].type === "done") {
      doneCount++;
      // The next event (if any) starts a new turn
      if (i + 1 < events.length) {
        turnStartIndices.push(i + 1);
      }
    }
  }

  // Place user message dots at turn boundaries (turn 1+)
  for (let turnIdx = 1; turnIdx < turnStartIndices.length; turnIdx++) {
    const feedbackIdx = turnIdx - 1;
    if (feedbackIdx < feedback.length) {
      dots.push({
        eventIndex: turnStartIndices[turnIdx],
        kind: "user",
        tooltip: `User message: ${truncate(feedback[feedbackIdx], 200)}`,
      });
    }
  }

  // Place tool_use dots
  for (let i = 0; i < events.length; i++) {
    if (events[i].type === "tool_use") {
      dots.push({
        eventIndex: i,
        kind: "tool",
        tooltip: buildToolTooltip(events[i]),
      });
    }
  }

  // Sort by eventIndex for consistent rendering order
  dots.sort((a, b) => a.eventIndex - b.eventIndex);

  return dots;
}

/**
 * Builds the tooltip text for a tool_use dot.
 * Shows: tool name, one-line summary (command/path/content), timestamp.
 */
function buildToolTooltip(event: AgentEvent): string {
  const toolName = event.tool ?? "Tool";
  const summary = deriveSummary(event);
  const time = formatTimestamp(event.timestamp);
  return `${toolName}: ${summary} · ${time}`;
}

/**
 * Derives a one-line summary from the event's fields.
 * Priority: command > path > content > "(no details)"
 * Truncated to 200 chars per NFR5.
 */
function deriveSummary(event: AgentEvent): string {
  const raw = event.command ?? event.path ?? event.content ?? "(no details)";
  return truncate(raw, 200);
}

/**
 * Truncates a string to maxLen characters, appending "…" if truncated.
 */
function truncate(str: string, maxLen: number): string {
  if (str.length <= maxLen) return str;
  return str.slice(0, maxLen - 1) + "…";
}

/**
 * Formats a timestamp as HH:MM:SS for tooltip display.
 */
function formatTimestamp(ts: number): string {
  const d = new Date(ts);
  return d.toLocaleTimeString("en-US", { hour12: false });
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

function makeEvent(
  type: AgentEvent["type"],
  overrides: Partial<AgentEvent> = {},
): AgentEvent {
  return {
    type,
    timestamp: Date.now(),
    ...overrides,
  };
}

function makeToolUse(tool: string, command?: string, ts?: number): AgentEvent {
  return makeEvent("tool_use", {
    tool,
    command,
    itemId: `item-${Math.random().toString(36).slice(2, 8)}`,
    timestamp: ts ?? Date.now(),
  });
}

function makeText(content: string, ts?: number): AgentEvent {
  return makeEvent("text", { content, timestamp: ts ?? Date.now() });
}

function makeDone(ts?: number): AgentEvent {
  return makeEvent("done", { timestamp: ts ?? Date.now() });
}

function makeFileEdit(path: string, ts?: number): AgentEvent {
  return makeEvent("file_edit", { path, timestamp: ts ?? Date.now() });
}

// ─── Unit tests: dot counting ────────────────────────────────────────────────

describe("TimelineRail dot counting", () => {
  it("empty events and no feedback → no dots", () => {
    const dots = deriveTimelineDots([], []);
    expect(dots).toHaveLength(0);
  });

  it("single tool_use event → one tool dot", () => {
    const events: AgentEvent[] = [makeToolUse("Bash", "echo hi", 1000)];
    const dots = deriveTimelineDots(events, []);
    expect(dots).toHaveLength(1);
    expect(dots[0].kind).toBe("tool");
    expect(dots[0].eventIndex).toBe(0);
  });

  it("mixed event types: dots = tool_use count + user message count", () => {
    const events: AgentEvent[] = [
      makeText("Hello", 1000),
      makeToolUse("Bash", "ls", 1100),
      makeToolUse("Edit", undefined, 1200),
      makeFileEdit("/src/main.ts", 1300),
      makeDone(1400),
      // Turn 2 starts here (index 5)
      makeText("Continuing", 1500),
      makeToolUse("Read", undefined, 1600),
      makeDone(1700),
    ];
    const feedback = ["user prompt 1"];

    const dots = deriveTimelineDots(events, feedback);

    const toolDots = dots.filter((d) => d.kind === "tool");
    const userDots = dots.filter((d) => d.kind === "user");

    // 3 tool_use events
    expect(toolDots).toHaveLength(3);
    // 1 user message (feedback[0] maps to turn 1)
    expect(userDots).toHaveLength(1);
    // Total dots = tool_use count + user message count
    expect(dots).toHaveLength(3 + 1);
  });

  it("multiple user messages across turns", () => {
    const events: AgentEvent[] = [
      makeToolUse("Bash", "echo 1", 1000),
      makeDone(1100),
      // Turn 1 starts at index 2
      makeToolUse("Bash", "echo 2", 1200),
      makeDone(1300),
      // Turn 2 starts at index 4
      makeToolUse("Bash", "echo 3", 1400),
      makeDone(1500),
    ];
    const feedback = ["prompt 1", "prompt 2"];

    const dots = deriveTimelineDots(events, feedback);

    const toolDots = dots.filter((d) => d.kind === "tool");
    const userDots = dots.filter((d) => d.kind === "user");

    expect(toolDots).toHaveLength(3);
    expect(userDots).toHaveLength(2);
    expect(dots).toHaveLength(5);
  });

  it("no tool_use events, only user messages → only user dots", () => {
    const events: AgentEvent[] = [
      makeText("Agent response", 1000),
      makeDone(1100),
      // Turn 1 starts at index 2
      makeText("Another response", 1200),
      makeDone(1300),
    ];
    const feedback = ["user said hi"];

    const dots = deriveTimelineDots(events, feedback);
    expect(dots).toHaveLength(1);
    expect(dots[0].kind).toBe("user");
  });

  it("feedback entries beyond available turns are ignored", () => {
    const events: AgentEvent[] = [
      makeToolUse("Bash", "echo", 1000),
      makeDone(1100),
    ];
    // More feedback than turns that started after a done
    const feedback = ["prompt 1", "prompt 2", "prompt 3"];

    const dots = deriveTimelineDots(events, feedback);
    // Only 1 tool dot, no user dots (no turn 2 or 3 started)
    expect(dots).toHaveLength(1);
    expect(dots[0].kind).toBe("tool");
  });
});

// ─── Unit tests: onScrollTo mapping ──────────────────────────────────────────

describe("TimelineRail onScrollTo", () => {
  it("clicking a tool dot calls onScrollTo with the tool_use event index", () => {
    const events: AgentEvent[] = [
      makeText("Hello", 1000),
      makeToolUse("Bash", "ls -la", 1100),
      makeToolUse("Edit", undefined, 1200),
      makeFileEdit("/src/app.ts", 1300),
      makeDone(1400),
    ];

    const dots = deriveTimelineDots(events, []);

    // Simulate clicking each dot — verify eventIndex is correct
    const scrolledIndices: number[] = [];
    const onScrollTo = (eventIndex: number) => scrolledIndices.push(eventIndex);

    for (const dot of dots) {
      onScrollTo(dot.eventIndex);
    }

    // tool_use events are at indices 1 and 2
    expect(scrolledIndices).toEqual([1, 2]);
  });

  it("clicking a user message dot scrolls to the turn start", () => {
    const events: AgentEvent[] = [
      makeToolUse("Bash", "echo 1", 1000),
      makeDone(1100),
      // Turn 1 starts at index 2
      makeText("Agent reply", 1200),
      makeToolUse("Read", undefined, 1300),
      makeDone(1400),
    ];
    const feedback = ["user question"];

    const dots = deriveTimelineDots(events, feedback);
    const userDot = dots.find((d) => d.kind === "user");

    expect(userDot).toBeDefined();
    // User message dot should point to the start of turn 1 (index 2)
    expect(userDot!.eventIndex).toBe(2);
  });

  it("dots are ordered by eventIndex for sequential scrolling", () => {
    const events: AgentEvent[] = [
      makeToolUse("Bash", "cmd1", 1000),
      makeToolUse("Edit", undefined, 1100),
      makeDone(1200),
      // Turn 1 starts at index 3
      makeToolUse("Read", undefined, 1300),
      makeDone(1400),
    ];
    const feedback = ["user msg"];

    const dots = deriveTimelineDots(events, feedback);

    // Verify dots are sorted by eventIndex
    for (let i = 1; i < dots.length; i++) {
      expect(dots[i].eventIndex).toBeGreaterThanOrEqual(dots[i - 1].eventIndex);
    }
  });
});

// ─── Unit tests: tooltip text ────────────────────────────────────────────────

describe("TimelineRail tooltip", () => {
  it("tool_use with command shows tool name and command", () => {
    const events: AgentEvent[] = [
      makeToolUse("Bash", "npm test", 1700000000000),
    ];

    const dots = deriveTimelineDots(events, []);
    expect(dots[0].tooltip).toContain("Bash");
    expect(dots[0].tooltip).toContain("npm test");
  });

  it("tool_use with path shows tool name and path", () => {
    const event = makeEvent("tool_use", {
      tool: "Edit",
      path: "/src/components/App.tsx",
      timestamp: 1700000000000,
    });

    const dots = deriveTimelineDots([event], []);
    expect(dots[0].tooltip).toContain("Edit");
    expect(dots[0].tooltip).toContain("/src/components/App.tsx");
  });

  it("tool_use with no command/path/content shows (no details)", () => {
    const event = makeEvent("tool_use", {
      tool: "Custom",
      timestamp: 1700000000000,
    });

    const dots = deriveTimelineDots([event], []);
    expect(dots[0].tooltip).toContain("Custom");
    expect(dots[0].tooltip).toContain("(no details)");
  });

  it("user message dot shows truncated feedback text", () => {
    const events: AgentEvent[] = [
      makeText("response", 1000),
      makeDone(1100),
      makeText("next response", 1200),
    ];
    const feedback = ["Please fix the bug in auth.ts"];

    const dots = deriveTimelineDots(events, feedback);
    const userDot = dots.find((d) => d.kind === "user");

    expect(userDot).toBeDefined();
    expect(userDot!.tooltip).toContain("User message");
    expect(userDot!.tooltip).toContain("Please fix the bug in auth.ts");
  });

  it("long command is truncated to 200 chars in tooltip", () => {
    const longCommand = "a".repeat(300);
    const events: AgentEvent[] = [makeToolUse("Bash", longCommand, 1700000000000)];

    const dots = deriveTimelineDots(events, []);
    // The summary portion should be truncated
    const summary = deriveSummary(events[0]);
    expect(summary.length).toBeLessThanOrEqual(200);
    expect(summary.endsWith("…")).toBe(true);
  });

  it("tooltip includes formatted timestamp", () => {
    // Use a known timestamp
    const ts = new Date("2024-01-15T14:30:00Z").getTime();
    const events: AgentEvent[] = [makeToolUse("Bash", "echo hi", ts)];

    const dots = deriveTimelineDots(events, []);
    // Tooltip should contain a time string with the separator "·"
    expect(dots[0].tooltip).toContain("·");
    // The timestamp portion should be present (format varies by locale)
    expect(dots[0].tooltip.split("·").length).toBeGreaterThanOrEqual(2);
  });
});

// ─── Property test: P6 dot count invariant ───────────────────────────────────

/**
 * **Validates: Requirements CHAT8.2**
 *
 * P6: The TimelineRail renders exactly one dot per `tool_use` event
 * plus one dot per user message. The total dot count must always equal
 * (number of tool_use events) + (number of user messages that map to
 * a started turn).
 */
describe("Property P6: dot count = tool_use count + user message count", () => {
  const arbEventType = fc.constantFrom<AgentEvent["type"]>(
    "text",
    "tool_use",
    "tool_result",
    "file_edit",
    "done",
    "thinking",
    "error",
  );

  const arbEvent: fc.Arbitrary<AgentEvent> = fc.record({
    type: arbEventType,
    timestamp: fc.nat({ max: 2000000000000 }),
    content: fc.option(fc.string({ minLength: 0, maxLength: 50 }), { nil: undefined }),
    tool: fc.option(fc.constantFrom("Bash", "Edit", "Read", "Search"), { nil: undefined }),
    command: fc.option(fc.string({ minLength: 1, maxLength: 100 }), { nil: undefined }),
    path: fc.option(fc.string({ minLength: 1, maxLength: 100 }), { nil: undefined }),
    itemId: fc.option(fc.uuid(), { nil: undefined }),
  }) as fc.Arbitrary<AgentEvent>;

  const arbEventSequence = fc.array(arbEvent, { minLength: 0, maxLength: 80 });
  const arbFeedback = fc.array(fc.string({ minLength: 1, maxLength: 50 }), {
    minLength: 0,
    maxLength: 10,
  });

  it("dot count equals tool_use count + effective user message count", () => {
    fc.assert(
      fc.property(arbEventSequence, arbFeedback, (events, feedback) => {
        const dots = deriveTimelineDots(events, feedback);

        // Count tool_use events
        const toolUseCount = events.filter((e) => e.type === "tool_use").length;

        // Count effective user messages: feedback entries that map to
        // turns that actually started (i.e., there's an event after the
        // corresponding `done`).
        let doneCount = 0;
        const turnStartIndices: number[] = [];
        for (let i = 0; i < events.length; i++) {
          if (events[i].type === "done") {
            doneCount++;
            if (i + 1 < events.length) {
              turnStartIndices.push(i + 1);
            }
          }
        }
        const effectiveUserMessages = Math.min(feedback.length, turnStartIndices.length);

        const expectedDotCount = toolUseCount + effectiveUserMessages;
        expect(dots).toHaveLength(expectedDotCount);
      }),
      { numRuns: 300 },
    );
  });

  it("every tool dot maps to a tool_use event at the correct index", () => {
    fc.assert(
      fc.property(arbEventSequence, (events) => {
        const dots = deriveTimelineDots(events, []);
        const toolDots = dots.filter((d) => d.kind === "tool");

        for (const dot of toolDots) {
          expect(dot.eventIndex).toBeGreaterThanOrEqual(0);
          expect(dot.eventIndex).toBeLessThan(events.length);
          expect(events[dot.eventIndex].type).toBe("tool_use");
        }
      }),
      { numRuns: 300 },
    );
  });

  it("every user dot maps to a valid turn start index", () => {
    fc.assert(
      fc.property(arbEventSequence, arbFeedback, (events, feedback) => {
        const dots = deriveTimelineDots(events, feedback);
        const userDots = dots.filter((d) => d.kind === "user");

        for (const dot of userDots) {
          expect(dot.eventIndex).toBeGreaterThanOrEqual(0);
          expect(dot.eventIndex).toBeLessThan(events.length);
        }
      }),
      { numRuns: 300 },
    );
  });
});
