/**
 * Unit and property tests for the turn-derivation helpers.
 *
 * Validates: Requirements CHAT1.3
 */

import { describe, it, expect } from "vitest";
import fc from "fast-check";

import {
  deriveTurns,
  deriveToolGroups,
  deriveCardId,
  type Turn,
  type ToolGroup,
} from "../src/lib/turn-derivation";
import type { AgentEvent } from "../electron/orchestrator-adapters";

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

function makeToolUse(itemId?: string, ts?: number): AgentEvent {
  return makeEvent("tool_use", {
    itemId,
    timestamp: ts ?? Date.now(),
    tool: "Bash",
    command: "echo hello",
  });
}

function makeToolResult(itemId?: string, ts?: number): AgentEvent {
  return makeEvent("tool_result", {
    itemId,
    timestamp: ts ?? Date.now(),
    tool: "Bash",
  });
}

function makeText(content: string, ts?: number): AgentEvent {
  return makeEvent("text", { content, timestamp: ts ?? Date.now() });
}

function makeDone(ts?: number): AgentEvent {
  return makeEvent("done", { timestamp: ts ?? Date.now() });
}

// ─── Unit tests: deriveTurns ─────────────────────────────────────────────────

describe("deriveTurns", () => {
  it("empty events → no turns", () => {
    const result = deriveTurns([], []);
    expect(result).toEqual([]);
  });

  it("one `done` → one complete turn", () => {
    const events: AgentEvent[] = [
      makeText("Hello", 1000),
      makeToolUse("item-1", 1100),
      makeToolResult("item-1", 1200),
      makeDone(1300),
    ];

    // Turn 0 has no user message (started by orchestrator's initial prompt).
    // feedback[0] maps to turn 1, feedback[1] to turn 2, etc.
    const turns = deriveTurns(events, ["user prompt"]);
    expect(turns).toHaveLength(1);
    expect(turns[0].complete).toBe(true);
    expect(turns[0].turnIndex).toBe(0);
    expect(turns[0].userMessage).toBeNull();
    expect(turns[0].durationMs).toBe(300);
    expect(turns[0].events).toHaveLength(4);
  });

  it("two `done` events → two turns split correctly", () => {
    const events: AgentEvent[] = [
      makeText("First turn text", 1000),
      makeToolUse("a", 1100),
      makeToolResult("a", 1200),
      makeDone(1300),
      makeText("Second turn text", 2000),
      makeToolUse("b", 2100),
      makeToolResult("b", 2200),
      makeDone(2300),
    ];

    // Turn 0 has no user message (started by orchestrator's initial prompt).
    // Turn 1 uses feedback[0], turn 2 uses feedback[1], etc.
    const turns = deriveTurns(events, ["prompt 1", "prompt 2"]);
    expect(turns).toHaveLength(2);

    expect(turns[0].turnIndex).toBe(0);
    expect(turns[0].complete).toBe(true);
    expect(turns[0].userMessage).toBeNull(); // Turn 0 never has a user message
    expect(turns[0].events).toHaveLength(4);

    expect(turns[1].turnIndex).toBe(1);
    expect(turns[1].complete).toBe(true);
    expect(turns[1].userMessage).toBe("prompt 1"); // Turn 1 uses feedback[0]
    expect(turns[1].events).toHaveLength(4);
  });

  it("malformed sequence (`done` with no preceding events) → best-effort empty turn, no throw", () => {
    const events: AgentEvent[] = [makeDone(1000)];

    expect(() => deriveTurns(events, [])).not.toThrow();
    const turns = deriveTurns(events, []);
    expect(turns).toHaveLength(1);
    expect(turns[0].complete).toBe(true);
    expect(turns[0].events).toHaveLength(1);
    expect(turns[0].durationMs).toBe(0);
  });

  it("trailing events after last done produce an incomplete turn", () => {
    const events: AgentEvent[] = [
      makeText("First", 1000),
      makeDone(1100),
      makeText("In progress", 2000),
      makeToolUse("x", 2100),
    ];

    const turns = deriveTurns(events, ["p1", "p2"]);
    expect(turns).toHaveLength(2);
    expect(turns[0].complete).toBe(true);
    expect(turns[1].complete).toBe(false);
    expect(turns[1].durationMs).toBeNull();
  });
});

// ─── Unit tests: deriveToolGroups ────────────────────────────────────────────

describe("deriveToolGroups", () => {
  it.each([false, true])("bounds item reads for a long stream with split groups: %s", split => {
    let reads = 0;
    const events = Array.from({ length: 256 }, (_, index) => {
      const id = `tool-${index}`;
      const pair = [makeToolUse(id), makeToolResult(id)];
      for (const event of pair) Object.defineProperty(event, "itemId", { get: () => { reads += 1; return id; } });
      return split ? [...pair, makeText("Next tool")] : pair;
    }).flat();
    deriveToolGroups(events);
    expect(reads).toBeLessThanOrEqual(events.length * 8);
  });

  it("keeps a group pending when another identified tool starts after a completed call", () => {
    const groups = deriveToolGroups([makeToolUse("first"), makeToolResult("first"), makeToolUse("second")]);
    expect(groups.map(group => group.complete)).toEqual([false]);
  });

  it("pairs late results with their identified group across text boundaries", () => {
    const groups = deriveToolGroups([
      makeToolUse("first", 1), makeText("Starting another tool", 2),
      makeToolUse("second", 3), makeToolResult("first", 4),
    ]);
    expect(groups.map(group => [group.complete, group.endedAt])).toEqual([[true, 4], [false, null]]);
  });

  it("does not use a future receipt to complete an earlier group when an item moves", () => {
    const groups = deriveToolGroups([
      makeToolUse("first", 1), makeToolUse("shared", 2), makeToolResult("first", 3),
      makeText("Another batch", 4), makeToolUse("shared", 5), makeToolResult("shared", 6),
    ]);
    expect(groups.map(group => [group.complete, group.endedAt])).toEqual([[false, 3], [true, 6]]);
  });

  it("alternating `tool_use` / `text` → multiple singleton groups", () => {
    const events: AgentEvent[] = [
      makeToolUse("a", 1000),
      makeText("some text", 1100),
      makeToolUse("b", 1200),
      makeText("more text", 1300),
      makeToolUse("c", 1400),
    ];

    const groups = deriveToolGroups(events);
    expect(groups).toHaveLength(3);
    expect(groups[0].eventIndices).toEqual([0]);
    expect(groups[1].eventIndices).toEqual([2]);
    expect(groups[2].eventIndices).toEqual([4]);
  });

  it("run of `tool_use` events → one group", () => {
    const events: AgentEvent[] = [
      makeToolUse("a", 1000),
      makeToolUse("b", 1100),
      makeToolUse("c", 1200),
    ];

    const groups = deriveToolGroups(events);
    expect(groups).toHaveLength(1);
    expect(groups[0].eventIndices).toEqual([0, 1, 2]);
    expect(groups[0].startedAt).toBe(1000);
  });

  it("`text` with empty `content` does NOT split a group", () => {
    const events: AgentEvent[] = [
      makeToolUse("a", 1000),
      makeEvent("text", { content: "", timestamp: 1050 }),
      makeToolUse("b", 1100),
    ];

    const groups = deriveToolGroups(events);
    expect(groups).toHaveLength(1);
    expect(groups[0].eventIndices).toEqual([0, 2]);
  });

  it("`text` with undefined content does NOT split a group", () => {
    const events: AgentEvent[] = [
      makeToolUse("a", 1000),
      makeEvent("text", { content: undefined, timestamp: 1050 }),
      makeToolUse("b", 1100),
    ];

    const groups = deriveToolGroups(events);
    expect(groups).toHaveLength(1);
    expect(groups[0].eventIndices).toEqual([0, 2]);
  });

  it("`tool_result` events update `complete` / `endedAt` but are not group members", () => {
    const events: AgentEvent[] = [
      makeToolUse("item-1", 1000),
      makeToolUse("item-2", 1100),
      makeToolResult("item-1", 1200),
      makeToolResult("item-2", 1300),
    ];

    const groups = deriveToolGroups(events);
    expect(groups).toHaveLength(1);
    // Only tool_use events are members
    expect(groups[0].eventIndices).toEqual([0, 1]);
    // tool_result updates endedAt
    expect(groups[0].endedAt).toBe(1300);
    // Both tool_use events have matching results → complete
    expect(groups[0].complete).toBe(true);
  });

  it("a `done` event ends the current group", () => {
    const events: AgentEvent[] = [
      makeToolUse("a", 1000),
      makeToolUse("b", 1100),
      makeDone(1200),
      makeToolUse("c", 1300),
    ];

    const groups = deriveToolGroups(events);
    expect(groups).toHaveLength(2);
    expect(groups[0].eventIndices).toEqual([0, 1]);
    expect(groups[1].eventIndices).toEqual([3]);
  });
});

// ─── Property tests ──────────────────────────────────────────────────────────

/**
 * **Validates: Requirements CHAT1.3**
 *
 * P5: Tool_Group membership is contiguous and reasoning-cycle bounded.
 *
 * For every generated sequence of events:
 * - deriveToolGroups() produces groups whose member event-indices form
 *   maximal contiguous runs of tool_use events
 * - Extending the run either way (inserting a tool_use adjacent to the
 *   group boundary) would either be absorbed into the group or blocked
 *   by a text event with non-empty content
 */
describe("Property P5: ToolGroup contiguity and maximality", () => {
  // Generator for random AgentEvent sequences
  const arbEventType = fc.constantFrom<AgentEvent["type"]>(
    "tool_use",
    "file_edit",
    "file_read",
    "command",
    "command_run",
    "search",
    "web",
    "task",
    "generic",
    "approval_request",
    "text",
    "tool_result",
    "done",
  );

  const arbEvent: fc.Arbitrary<AgentEvent> = fc.record({
    type: arbEventType,
    timestamp: fc.nat({ max: 100000 }),
    content: fc.oneof(
      fc.constant(undefined),
      fc.constant(""),
      fc.string({ minLength: 1, maxLength: 20 }),
    ),
    itemId: fc.option(fc.uuid(), { nil: undefined }),
    tool: fc.option(fc.constantFrom("Bash", "Edit", "Read"), { nil: undefined }),
  }) as fc.Arbitrary<AgentEvent>;

  const arbEventSequence = fc.array(arbEvent, { minLength: 0, maxLength: 50 });

  // Tool event types that get grouped together
  const TOOL_EVENT_TYPES = new Set([
    "tool_use", "file_edit", "file_read", "command", "command_run",
    "search", "web", "task", "generic", "approval_request",
  ]);

  it("every group's member indices are contiguous in the events array", () => {
    fc.assert(
      fc.property(arbEventSequence, (events) => {
        const groups = deriveToolGroups(events);

        for (const group of groups) {
          // All members must be contiguous indices
          for (let i = 1; i < group.eventIndices.length; i++) {
            const prev = group.eventIndices[i - 1];
            const curr = group.eventIndices[i];
            // Between prev and curr, there should be no tool-type events
            // that aren't in this group (contiguity of tool events
            // means no tool event is skipped)
            for (let j = prev + 1; j < curr; j++) {
              expect(TOOL_EVENT_TYPES.has(events[j].type)).toBe(false);
            }
          }
        }
      }),
      { numRuns: 200 },
    );
  });

  it("every member is a tool-type event", () => {
    fc.assert(
      fc.property(arbEventSequence, (events) => {
        const groups = deriveToolGroups(events);

        for (const group of groups) {
          for (const idx of group.eventIndices) {
            expect(TOOL_EVENT_TYPES.has(events[idx].type)).toBe(true);
          }
        }
      }),
      { numRuns: 200 },
    );
  });

  it("groups are maximal: extending either way hits a boundary", () => {
    fc.assert(
      fc.property(arbEventSequence, (events) => {
        const groups = deriveToolGroups(events);

        for (const group of groups) {
          const firstIdx = group.eventIndices[0];
          const lastIdx = group.eventIndices[group.eventIndices.length - 1];

          // Check left boundary: the event before the first member
          // (if it exists) must NOT be a tool-type event, OR it must be in
          // a different group (separated by a non-empty text or done)
          if (firstIdx > 0) {
            const prevEvent = events[firstIdx - 1];
            if (TOOL_EVENT_TYPES.has(prevEvent.type)) {
              // This tool event must be in a different group, meaning
              // there's a splitting event between them.
              const inAnotherGroup = groups.some(
                (g) => g !== group && g.eventIndices.includes(firstIdx - 1),
              );
              expect(inAnotherGroup).toBe(true);
            }
          }

          // Check right boundary: the event after the last member
          // (if it exists) must NOT be a tool-type event that could extend
          // this group. If it IS a tool-type event, it must be in a different
          // group (separated by a splitting event).
          if (lastIdx < events.length - 1) {
            const nextIdx = lastIdx + 1;
            // Find the next tool-type event after lastIdx
            let nextToolEventIdx = -1;
            for (let j = nextIdx; j < events.length; j++) {
              if (TOOL_EVENT_TYPES.has(events[j].type)) {
                nextToolEventIdx = j;
                break;
              }
            }

            if (nextToolEventIdx !== -1 && !group.eventIndices.includes(nextToolEventIdx)) {
              // There must be a splitting event (non-empty text or done)
              // between lastIdx and nextToolEventIdx
              let hasSplitter = false;
              for (let j = lastIdx + 1; j < nextToolEventIdx; j++) {
                const ev = events[j];
                if (ev.type === "done") {
                  hasSplitter = true;
                  break;
                }
                if (ev.type === "text" && ev.content) {
                  hasSplitter = true;
                  break;
                }
              }
              expect(hasSplitter).toBe(true);
            }
          }
        }
      }),
      { numRuns: 200 },
    );
  });

  it("every tool-type event belongs to exactly one group", () => {
    fc.assert(
      fc.property(arbEventSequence, (events) => {
        const groups = deriveToolGroups(events);
        const toolEventIndices = events
          .map((e, i) => (TOOL_EVENT_TYPES.has(e.type) ? i : -1))
          .filter((i) => i >= 0);

        // Every tool-type event index appears in exactly one group
        for (const idx of toolEventIndices) {
          const containingGroups = groups.filter((g) =>
            g.eventIndices.includes(idx),
          );
          expect(containingGroups).toHaveLength(1);
        }
      }),
      { numRuns: 200 },
    );
  });
});

/**
 * **Validates: Requirements CHAT1.3**
 *
 * P2: deriveCardId is stable across re-runs with the same input.
 *
 * The function is pure and deterministic — calling it multiple times
 * with the same arguments always produces the same CardId.
 */
describe("Property P2: deriveCardId stability", () => {
  const arbTaskId = fc.string({ minLength: 1, maxLength: 30 });
  const arbTurnIndex = fc.nat({ max: 100 });
  const arbEventIndex = fc.nat({ max: 500 });
  const arbItemId = fc.option(fc.uuid(), { nil: undefined });

  it("deriveCardId is deterministic: same inputs → same output across re-runs", () => {
    fc.assert(
      fc.property(
        arbTaskId,
        arbTurnIndex,
        arbEventIndex,
        arbItemId,
        (taskId, turnIndex, eventIndex, itemId) => {
          const event: AgentEvent = {
            type: "tool_use",
            timestamp: 12345,
            itemId,
          };

          const id1 = deriveCardId(taskId, turnIndex, eventIndex, event);
          const id2 = deriveCardId(taskId, turnIndex, eventIndex, event);
          const id3 = deriveCardId(taskId, turnIndex, eventIndex, event);

          expect(id1).toBe(id2);
          expect(id2).toBe(id3);
        },
      ),
      { numRuns: 200 },
    );
  });

  it("when itemId is present, deriveCardId returns itemId verbatim", () => {
    fc.assert(
      fc.property(
        arbTaskId,
        arbTurnIndex,
        arbEventIndex,
        fc.uuid(),
        (taskId, turnIndex, eventIndex, itemId) => {
          const event: AgentEvent = {
            type: "tool_use",
            timestamp: 12345,
            itemId,
          };

          const id = deriveCardId(taskId, turnIndex, eventIndex, event);
          expect(id).toBe(itemId);
        },
      ),
      { numRuns: 200 },
    );
  });

  it("when itemId is absent, deriveCardId returns the deterministic synthesis", () => {
    fc.assert(
      fc.property(
        arbTaskId,
        arbTurnIndex,
        arbEventIndex,
        (taskId, turnIndex, eventIndex) => {
          const event: AgentEvent = {
            type: "tool_use",
            timestamp: 12345,
            itemId: undefined,
          };

          const id = deriveCardId(taskId, turnIndex, eventIndex, event);
          expect(id).toBe(`${taskId}:${turnIndex}:${eventIndex}`);
        },
      ),
      { numRuns: 200 },
    );
  });
});
