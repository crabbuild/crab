/**
 * Performance smoke tests for the chat derivation helpers.
 *
 * Validates: NFR1 (performance) — deriveTurns() and deriveToolGroups()
 * must handle 1000 events without degrading responsiveness.
 */

import { describe, it, expect } from "vitest";
import { deriveTurns, deriveToolGroups } from "../src/lib/turn-derivation";
import type { AgentEvent } from "../electron/orchestrator-adapters";

// ─── Synthetic event generators ──────────────────────────────────────────────

function makeToolUse(itemId: string, ts: number): AgentEvent {
  return {
    type: "tool_use",
    timestamp: ts,
    itemId,
    tool: "Bash",
    command: `echo "task ${itemId}"`,
  };
}

function makeToolResult(itemId: string, ts: number): AgentEvent {
  return {
    type: "tool_result",
    timestamp: ts,
    itemId,
    tool: "Bash",
    output: `output for ${itemId}`,
  };
}

function makeText(content: string, ts: number): AgentEvent {
  return { type: "text", timestamp: ts, content };
}

function makeFileEdit(path: string, ts: number): AgentEvent {
  return {
    type: "file_edit",
    timestamp: ts,
    path,
    diff: `--- a/${path}\n+++ b/${path}\n@@ -1,3 +1,4 @@\n line1\n+added\n line2\n line3\n`,
  };
}

function makeDone(ts: number): AgentEvent {
  return { type: "done", timestamp: ts };
}

/**
 * Generate a synthetic stream of 1000 AgentEvents spanning ~40 turns.
 * Each turn has a mix of text, tool_use, tool_result, file_edit, and done events.
 */
function generate1000Events(): { events: AgentEvent[]; feedback: string[] } {
  const events: AgentEvent[] = [];
  const feedback: string[] = [];
  const turnsTarget = 40;
  const eventsPerTurn = Math.floor(1000 / turnsTarget); // ~25 events per turn
  let ts = 1000;
  let itemCounter = 0;

  for (let turn = 0; turn < turnsTarget; turn++) {
    if (turn > 0) {
      feedback.push(`User prompt for turn ${turn}`);
    }

    // Each turn: 1 text + ~10 tool_use/tool_result pairs + 2 file_edits + 1 text + 1 done
    // That's about 25 events per turn

    // Opening text from agent
    events.push(makeText(`Agent response for turn ${turn}`, ts));
    ts += 50;

    // Tool use/result pairs (10 per turn = 20 events)
    for (let i = 0; i < 10; i++) {
      const id = `item-${itemCounter++}`;
      events.push(makeToolUse(id, ts));
      ts += 100;
      events.push(makeToolResult(id, ts));
      ts += 50;
    }

    // File edits (2 per turn)
    events.push(makeFileEdit(`src/file-${turn}-a.ts`, ts));
    ts += 30;
    events.push(makeFileEdit(`src/file-${turn}-b.ts`, ts));
    ts += 30;

    // Closing text
    events.push(makeText(`Summary for turn ${turn}`, ts));
    ts += 20;

    // Done event
    events.push(makeDone(ts));
    ts += 200;
  }

  return { events, feedback };
}

/**
 * Generate 1000 events optimised for deriveToolGroups testing:
 * long runs of tool_use events interspersed with text breaks.
 */
function generate1000ToolHeavyEvents(): AgentEvent[] {
  const events: AgentEvent[] = [];
  let ts = 1000;
  let itemCounter = 0;

  // Create groups of varying sizes: 20 groups of ~50 events each
  for (let group = 0; group < 20; group++) {
    // 20 tool_use events in a contiguous run
    for (let i = 0; i < 20; i++) {
      const id = `item-${itemCounter++}`;
      events.push(makeToolUse(id, ts));
      ts += 50;
    }

    // 20 tool_result events for the above
    for (let i = 0; i < 20; i++) {
      const id = `item-${itemCounter - 20 + i}`;
      events.push(makeToolResult(id, ts));
      ts += 20;
    }

    // Text break to split groups (non-empty content)
    events.push(makeText(`Break after group ${group}`, ts));
    ts += 100;

    // A few more tool_use + results to fill up to ~50 events per group
    for (let i = 0; i < 4; i++) {
      const id = `item-${itemCounter++}`;
      events.push(makeToolUse(id, ts));
      ts += 30;
      events.push(makeToolResult(id, ts));
      ts += 20;
    }

    // Another text break
    events.push(makeText(`Second break in group ${group}`, ts));
    ts += 50;
  }

  return events;
}

// ─── Performance tests ───────────────────────────────────────────────────────

describe("Performance smoke: 1000 events", () => {
  it("deriveTurns() processes 1000 events in under 100ms", () => {
    const { events, feedback } = generate1000Events();
    expect(events.length).toBeGreaterThanOrEqual(1000);

    const start = performance.now();
    const turns = deriveTurns(events, feedback);
    const elapsed = performance.now() - start;

    expect(elapsed).toBeLessThan(100);
    expect(turns.length).toBe(40);

    // Verify structural correctness
    for (const turn of turns) {
      expect(turn.complete).toBe(true);
      expect(turn.events.length).toBeGreaterThan(0);
    }
  });

  it("deriveToolGroups() processes 1000 events in under 100ms", () => {
    const events = generate1000ToolHeavyEvents();
    expect(events.length).toBeGreaterThanOrEqual(1000);

    const start = performance.now();
    const groups = deriveToolGroups(events);
    const elapsed = performance.now() - start;

    expect(elapsed).toBeLessThan(100);
    expect(groups.length).toBeGreaterThan(0);

    // Verify all tool_use events are accounted for
    const toolUseCount = events.filter((e) => e.type === "tool_use").length;
    const groupedCount = groups.reduce(
      (sum, g) => sum + g.eventIndices.length,
      0,
    );
    expect(groupedCount).toBe(toolUseCount);
  });

  it("deriveTurns() scales linearly — 1000 events is not significantly slower than 100", () => {
    const { events: events1000, feedback: feedback1000 } = generate1000Events();

    // Take a 100-event subset (first ~4 turns)
    const doneIndices: number[] = [];
    for (let i = 0; i < events1000.length && doneIndices.length < 4; i++) {
      if (events1000[i].type === "done") doneIndices.push(i);
    }
    const events100 = events1000.slice(0, (doneIndices[3] ?? 99) + 1);
    const feedback100 = feedback1000.slice(0, 3);

    // Warm up
    deriveTurns(events100, feedback100);
    deriveTurns(events1000, feedback1000);

    // Measure 100 events
    const start100 = performance.now();
    for (let i = 0; i < 10; i++) deriveTurns(events100, feedback100);
    const elapsed100 = (performance.now() - start100) / 10;

    // Measure 1000 events
    const start1000 = performance.now();
    for (let i = 0; i < 10; i++) deriveTurns(events1000, feedback1000);
    const elapsed1000 = (performance.now() - start1000) / 10;

    // 1000 events should be no more than 20x slower than 100 events
    // (linear would be 10x; we allow 20x for overhead)
    expect(elapsed1000).toBeLessThan(elapsed100 * 20);
  });
});
