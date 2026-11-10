/** Tests extraction from actual turn-summary input events. */

import { describe, it, expect } from "vitest";
import type { AgentEvent } from "../electron/orchestrator-adapters";
import { extractFileEdits } from "../src/components/orchestrator/TurnSummaryCard";

// ─── Helpers ─────────────────────────────────────────────────────────────────

function makeFileEditEvent(
  path: string,
  overrides: Partial<AgentEvent> = {},
  timestamp?: number,
): AgentEvent {
  return {
    type: "file_edit",
    timestamp: timestamp ?? Date.now(),
    path,
    diff: overrides.diff ?? `--- a/${path}\n+++ b/${path}\n@@ -1,3 +1,4 @@\n line1\n+added\n line2\n line3\n`,
    tool: "Edit",
    input: overrides.input ?? {
      file_path: path,
      old_string: "line1\nline2",
      new_string: "line1\nadded\nline2",
    },
    ...overrides,
  };
}

function makeNonEditEvent(type: AgentEvent["type"], ts?: number): AgentEvent {
  return {
    type,
    timestamp: ts ?? Date.now(),
    content: "some content",
  };
}

// ─── extractFileEdits tests ──────────────────────────────────────────────────

describe("extractFileEdits", () => {
  it("returns empty array for events with no file_edit events", () => {
    const events: AgentEvent[] = [
      makeNonEditEvent("text", 1000),
      makeNonEditEvent("tool_use", 1100),
      makeNonEditEvent("done", 1200),
    ];

    const result = extractFileEdits(events);
    expect(result).toEqual([]);
  });

  it("extracts a single file edit event", () => {
    const events: AgentEvent[] = [
      makeFileEditEvent("src/utils.ts", {
        diff: "--- a/src/utils.ts\n+++ b/src/utils.ts\n@@ -1,2 +1,3 @@\n line1\n+added\n line2\n",
      }, 1000),
    ];

    const result = extractFileEdits(events);
    expect(result).toHaveLength(1);
    expect(result[0].path).toBe("src/utils.ts");
    expect(result[0].added).toBe(1);
    expect(result[0].removed).toBe(0);
  });

  it("groups multiple edits to the same path, accumulating stats", () => {
    const events: AgentEvent[] = [
      makeFileEditEvent("src/main.ts", {
        diff: "--- a/src/main.ts\n+++ b/src/main.ts\n@@ -1,2 +1,3 @@\n a\n+b\n c\n",
      }, 1000),
      makeFileEditEvent("src/main.ts", {
        diff: "--- a/src/main.ts\n+++ b/src/main.ts\n@@ -5,3 +5,4 @@\n x\n+y\n+z\n w\n",
      }, 2000),
    ];

    const result = extractFileEdits(events);
    expect(result).toHaveLength(1);
    expect(result[0].path).toBe("src/main.ts");
    // First edit: +1, second edit: +2 → total added = 3
    expect(result[0].added).toBe(3);
    expect(result[0].removed).toBe(0);
  });

  it("keeps the last event per path for the diff body", () => {
    const firstEvent = makeFileEditEvent("src/app.ts", {
      diff: "--- a/src/app.ts\n+++ b/src/app.ts\n@@ -1,1 +1,2 @@\n first\n+edit\n",
    }, 1000);
    const secondEvent = makeFileEditEvent("src/app.ts", {
      diff: "--- a/src/app.ts\n+++ b/src/app.ts\n@@ -3,1 +3,2 @@\n third\n+edit\n",
    }, 2000);

    const events: AgentEvent[] = [firstEvent, secondEvent];
    const result = extractFileEdits(events);

    expect(result).toHaveLength(1);
    // The diff should be from the second (latest) event
    expect(result[0].diff).toBe(secondEvent.diff);
    expect(result[0].event).toBe(secondEvent);
  });

  it("handles multiple distinct file paths", () => {
    const events: AgentEvent[] = [
      makeFileEditEvent("src/a.ts", {
        diff: "--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1,1 +1,2 @@\n a\n+b\n",
      }, 1000),
      makeFileEditEvent("src/b.ts", {
        diff: "--- a/src/b.ts\n+++ b/src/b.ts\n@@ -1,2 +1,1 @@\n x\n-y\n",
      }, 2000),
      makeFileEditEvent("src/c.ts", {
        diff: "--- a/src/c.ts\n+++ b/src/c.ts\n@@ -1,1 +1,3 @@\n c\n+d\n+e\n",
      }, 3000),
    ];

    const result = extractFileEdits(events);
    expect(result).toHaveLength(3);
    expect(result[0].path).toBe("src/a.ts");
    expect(result[0].added).toBe(1);
    expect(result[1].path).toBe("src/b.ts");
    expect(result[1].removed).toBe(1);
    expect(result[2].path).toBe("src/c.ts");
    expect(result[2].added).toBe(2);
  });

  it("skips file_edit events without a path", () => {
    const events: AgentEvent[] = [
      { type: "file_edit", timestamp: 1000, diff: "+added\n" } as AgentEvent,
      makeFileEditEvent("src/valid.ts", {}, 2000),
    ];

    const result = extractFileEdits(events);
    expect(result).toHaveLength(1);
    expect(result[0].path).toBe("src/valid.ts");
  });

  it("handles events with empty diff gracefully", () => {
    const events: AgentEvent[] = [
      makeFileEditEvent("src/empty.ts", { diff: "" }, 1000),
    ];

    const result = extractFileEdits(events);
    expect(result).toHaveLength(1);
    expect(result[0].added).toBe(0);
    expect(result[0].removed).toBe(0);
  });

  it("reads diff from input.diff when event.diff is absent", () => {
    const events: AgentEvent[] = [
      {
        type: "file_edit",
        timestamp: 1000,
        path: "src/fallback.ts",
        input: {
          diff: "--- a/src/fallback.ts\n+++ b/src/fallback.ts\n@@ -1,1 +1,2 @@\n x\n+y\n",
          file_path: "src/fallback.ts",
          old_string: "x",
          new_string: "x\ny",
        },
      } as AgentEvent,
    ];

    const result = extractFileEdits(events);
    expect(result).toHaveLength(1);
    expect(result[0].added).toBe(1);
    expect(result[0].removed).toBe(0);
  });
});
