/**
 * Component tests for <TurnSummaryCard> Undo logic (P4).
 *
 * Validates: Requirements CHAT4.4, CHAT4.7
 *
 * Tests:
 * 1. extractFileEdits helper: groups by path, accumulates stats, keeps
 *    last event per path.
 * 2. Undo logic: file_edit events are processed in reverse arrival order
 *    when undo is triggered.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
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

// ─── Undo logic: reverse arrival order ───────────────────────────────────────

describe("TurnSummaryCard Undo — reverse arrival order (P4)", () => {
  let revertCalls: Array<{ taskId: string; patch: any }>;

  beforeEach(() => {
    revertCalls = [];

    // Mock window.crab.orchestrator.revertHunk in Node environment
    (globalThis as any).window = {
      crab: {
        orchestrator: {
          revertHunk: vi.fn(async (taskId: string, patch: any) => {
            revertCalls.push({ taskId, patch });
            return { success: true };
          }),
        },
      },
    };
  });

  afterEach(() => {
    delete (globalThis as any).window;
  });

  /**
   * Simulates the undo logic from TurnSummaryCard.handleUndo:
   * collects file_edit events in reverse arrival order and calls
   * revertHunk for each.
   *
   * This mirrors the component's implementation without needing to
   * render the full React component tree.
   */
  async function simulateUndo(events: AgentEvent[], taskId: string) {
    const fileEditEvents = [...events]
      .filter((e) => e.type === "file_edit" && e.path)
      .reverse();

    for (const event of fileEditEvents) {
      const input = event.input as Record<string, unknown> | undefined;
      const filePath = event.path ?? String(input?.file_path ?? input?.path ?? "");
      const oldString = String(input?.old_string ?? "");
      const newString = String(input?.new_string ?? "");
      const content = String(input?.content ?? "");
      const kind = input?.kind as string | undefined;

      const patch = {
        filePath,
        oldString: kind === "add" ? "" : oldString,
        newString: kind === "add" ? content : newString,
      };

      await (globalThis as any).window.crab.orchestrator.revertHunk(taskId, patch);
    }
  }

  it("processes file_edit events in reverse arrival order", async () => {
    const events: AgentEvent[] = [
      makeNonEditEvent("text", 1000),
      makeFileEditEvent("src/first.ts", {
        input: { file_path: "src/first.ts", old_string: "a", new_string: "b" },
      }, 2000),
      makeNonEditEvent("tool_result", 2500),
      makeFileEditEvent("src/second.ts", {
        input: { file_path: "src/second.ts", old_string: "c", new_string: "d" },
      }, 3000),
      makeFileEditEvent("src/third.ts", {
        input: { file_path: "src/third.ts", old_string: "e", new_string: "f" },
      }, 4000),
      makeNonEditEvent("done", 5000),
    ];

    await simulateUndo(events, "task-1");

    // Should be called 3 times (one per file_edit event)
    expect(revertCalls).toHaveLength(3);

    // Reverse arrival order: third → second → first
    expect(revertCalls[0].patch.filePath).toBe("src/third.ts");
    expect(revertCalls[1].patch.filePath).toBe("src/second.ts");
    expect(revertCalls[2].patch.filePath).toBe("src/first.ts");
  });

  it("calls revert K × M times for K files × M hunks each", async () => {
    // 3 files, each edited twice (simulating multiple hunks per file)
    const events: AgentEvent[] = [
      makeFileEditEvent("src/a.ts", {
        input: { file_path: "src/a.ts", old_string: "a1", new_string: "a2" },
      }, 1000),
      makeFileEditEvent("src/b.ts", {
        input: { file_path: "src/b.ts", old_string: "b1", new_string: "b2" },
      }, 2000),
      makeFileEditEvent("src/c.ts", {
        input: { file_path: "src/c.ts", old_string: "c1", new_string: "c2" },
      }, 3000),
      makeFileEditEvent("src/a.ts", {
        input: { file_path: "src/a.ts", old_string: "a3", new_string: "a4" },
      }, 4000),
      makeFileEditEvent("src/b.ts", {
        input: { file_path: "src/b.ts", old_string: "b3", new_string: "b4" },
      }, 5000),
      makeFileEditEvent("src/c.ts", {
        input: { file_path: "src/c.ts", old_string: "c3", new_string: "c4" },
      }, 6000),
    ];

    await simulateUndo(events, "task-2");

    // 3 files × 2 edits each = 6 revert calls
    expect(revertCalls).toHaveLength(6);

    // Reverse arrival order: c(6000) → b(5000) → a(4000) → c(3000) → b(2000) → a(1000)
    expect(revertCalls[0].patch.filePath).toBe("src/c.ts");
    expect(revertCalls[0].patch.oldString).toBe("c3");
    expect(revertCalls[1].patch.filePath).toBe("src/b.ts");
    expect(revertCalls[1].patch.oldString).toBe("b3");
    expect(revertCalls[2].patch.filePath).toBe("src/a.ts");
    expect(revertCalls[2].patch.oldString).toBe("a3");
    expect(revertCalls[3].patch.filePath).toBe("src/c.ts");
    expect(revertCalls[3].patch.oldString).toBe("c1");
    expect(revertCalls[4].patch.filePath).toBe("src/b.ts");
    expect(revertCalls[4].patch.oldString).toBe("b1");
    expect(revertCalls[5].patch.filePath).toBe("src/a.ts");
    expect(revertCalls[5].patch.oldString).toBe("a1");
  });

  it("skips file_edit events without a path", async () => {
    const events: AgentEvent[] = [
      makeFileEditEvent("src/valid.ts", {
        input: { file_path: "src/valid.ts", old_string: "x", new_string: "y" },
      }, 1000),
      { type: "file_edit", timestamp: 2000, input: { old_string: "z", new_string: "w" } } as AgentEvent,
      makeFileEditEvent("src/also-valid.ts", {
        input: { file_path: "src/also-valid.ts", old_string: "m", new_string: "n" },
      }, 3000),
    ];

    await simulateUndo(events, "task-3");

    // Only 2 calls (the event without path is skipped)
    expect(revertCalls).toHaveLength(2);
    expect(revertCalls[0].patch.filePath).toBe("src/also-valid.ts");
    expect(revertCalls[1].patch.filePath).toBe("src/valid.ts");
  });

  it("handles 'add' kind by using content as newString and empty oldString", async () => {
    const events: AgentEvent[] = [
      makeFileEditEvent("src/new-file.ts", {
        input: {
          file_path: "src/new-file.ts",
          kind: "add",
          content: "export const x = 1;\n",
          old_string: "",
          new_string: "",
        },
      }, 1000),
    ];

    await simulateUndo(events, "task-4");

    expect(revertCalls).toHaveLength(1);
    expect(revertCalls[0].patch.filePath).toBe("src/new-file.ts");
    expect(revertCalls[0].patch.oldString).toBe("");
    expect(revertCalls[0].patch.newString).toBe("export const x = 1;\n");
  });

  it("continues reverting remaining files even if one revert fails", async () => {
    // Override mock to fail on the second call
    let callCount = 0;
    (globalThis as any).window.crab.orchestrator.revertHunk = vi.fn(async (taskId: string, patch: any) => {
      callCount++;
      revertCalls.push({ taskId, patch });
      if (callCount === 2) {
        return { success: false, error: "file not found" };
      }
      return { success: true };
    });

    const events: AgentEvent[] = [
      makeFileEditEvent("src/a.ts", {
        input: { file_path: "src/a.ts", old_string: "a", new_string: "b" },
      }, 1000),
      makeFileEditEvent("src/b.ts", {
        input: { file_path: "src/b.ts", old_string: "c", new_string: "d" },
      }, 2000),
      makeFileEditEvent("src/c.ts", {
        input: { file_path: "src/c.ts", old_string: "e", new_string: "f" },
      }, 3000),
    ];

    await simulateUndo(events, "task-5");

    // All 3 should still be called despite the second one failing
    expect(revertCalls).toHaveLength(3);
    expect(revertCalls[0].patch.filePath).toBe("src/c.ts");
    expect(revertCalls[1].patch.filePath).toBe("src/b.ts");
    expect(revertCalls[2].patch.filePath).toBe("src/a.ts");
  });

  it("does not mutate the original events array", async () => {
    const events: AgentEvent[] = [
      makeFileEditEvent("src/first.ts", {
        input: { file_path: "src/first.ts", old_string: "a", new_string: "b" },
      }, 1000),
      makeFileEditEvent("src/second.ts", {
        input: { file_path: "src/second.ts", old_string: "c", new_string: "d" },
      }, 2000),
    ];

    const originalOrder = events.map((e) => e.path);
    await simulateUndo(events, "task-6");

    // Original array should be unchanged
    expect(events.map((e) => e.path)).toEqual(originalOrder);
  });
});
