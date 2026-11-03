/**
 * Multi-adapter regression test: Claude Code stream.
 *
 * Verifies that `deriveTurns` works correctly with a Claude Code-style
 * event stream where events lack `itemId`, forcing the deterministic
 * synthesis fallback for CardId (`${taskId}:${turnIndex}:${eventIndex}`).
 *
 * Claude Code's adapter emits events without the Codex-style `itemId`
 * field. The turn-derivation logic must still:
 *   - detect turn boundaries from `done` events
 *   - group contiguous tool_use events into ToolGroups
 *   - synthesise stable CardIds from (taskId, turnIndex, eventIndex)
 *   - aggregate file_edit stats correctly
 */

import { describe, it, expect } from "vitest";

import {
  deriveTurns,
  deriveToolGroups,
  deriveCardId,
  deriveGroupId,
  type Turn,
} from "../src/lib/turn-derivation";
import type { AgentEvent } from "../electron/orchestrator-adapters";

// ─── Helpers ─────────────────────────────────────────────────────────────────

/** Build a Claude Code-style event (no itemId). */
function ccEvent(
  type: AgentEvent["type"],
  overrides: Partial<AgentEvent> = {},
): AgentEvent {
  return {
    type,
    timestamp: Date.now(),
    ...overrides,
  };
}

// ─── Synthetic Claude Code stream ────────────────────────────────────────────

/**
 * Simulates a realistic Claude Code session:
 *   Turn 1: text → tool_use(Read) → tool_result → tool_use(Write) →
 *           tool_result → file_edit → tool_use(Bash) → tool_result → done
 *   Turn 2: text → tool_use(Bash) → tool_result → done
 *
 * No event carries an `itemId` — this is the key difference from Codex.
 */
function buildClaudeCodeStream(): AgentEvent[] {
  const base = 1000;
  return [
    // Turn 1
    ccEvent("text", { content: "I'll refactor the push pipeline.", timestamp: base }),
    ccEvent("tool_use", { tool: "Read", command: undefined, path: "src/git/push.rs", timestamp: base + 100 }),
    ccEvent("tool_result", { tool: "Read", content: "pub fn push_xorbs(...) { ... }", timestamp: base + 200 }),
    ccEvent("tool_use", { tool: "Write", path: "src/git/push.rs", timestamp: base + 300 }),
    ccEvent("tool_result", { tool: "Write", timestamp: base + 400 }),
    ccEvent("file_edit", {
      path: "src/git/push.rs",
      diff: "--- a/src/git/push.rs\n+++ b/src/git/push.rs\n@@ -1,3 +1,5 @@\n-pub fn push_xorbs(pool: &UploadPool) -> Result<()> {\n+pub async fn push_xorbs(pool: &UploadPool) -> Result<()> {\n+    let stream = pool.stream();\n+    stream.flush().await?;\n     Ok(())\n }",
      timestamp: base + 450,
    }),
    ccEvent("tool_use", { tool: "Bash", command: "cargo build", timestamp: base + 500 }),
    ccEvent("tool_result", { tool: "Bash", content: "Compiling crab v0.1.0\nFinished release", timestamp: base + 600 }),
    ccEvent("done", { timestamp: base + 700 }),

    // Turn 2
    ccEvent("text", { content: "Running tests to verify.", timestamp: base + 1000 }),
    ccEvent("tool_use", { tool: "Bash", command: "cargo test", timestamp: base + 1100 }),
    ccEvent("tool_result", { tool: "Bash", content: "test result: ok. 42 passed; 0 failed", timestamp: base + 1200 }),
    ccEvent("done", { timestamp: base + 1300 }),
  ];
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("Multi-adapter regression: Claude Code stream", () => {
  const stream = buildClaudeCodeStream();
  const feedback = ["Run the tests please"];

  describe("deriveTurns produces correct turn boundaries", () => {
    it("detects one Turn per `done` event", () => {
      const turns = deriveTurns(stream, feedback);
      expect(turns).toHaveLength(2);
      expect(turns[0].complete).toBe(true);
      expect(turns[1].complete).toBe(true);
    });

    it("turn 0 has no user message (orchestrator-initiated)", () => {
      const turns = deriveTurns(stream, feedback);
      expect(turns[0].userMessage).toBeNull();
    });

    it("turn 1 picks up feedback[0] as user message", () => {
      const turns = deriveTurns(stream, feedback);
      expect(turns[1].userMessage).toBe("Run the tests please");
    });

    it("turn 0 contains the correct event count (9 events including done)", () => {
      const turns = deriveTurns(stream, feedback);
      expect(turns[0].events).toHaveLength(9);
    });

    it("turn 1 contains the correct event count (4 events including done)", () => {
      const turns = deriveTurns(stream, feedback);
      expect(turns[1].events).toHaveLength(4);
    });

    it("turn durations are computed correctly", () => {
      const turns = deriveTurns(stream, feedback);
      // Turn 0: done(1700) - text(1000) = 700ms
      expect(turns[0].durationMs).toBe(700);
      // Turn 1: done(2300) - text(2000) = 300ms
      expect(turns[1].durationMs).toBe(300);
    });
  });

  describe("ToolGroup boundaries match expected contiguity", () => {
    it("turn 0 groups tool calls and file edits until the turn ends", () => {
      const turns = deriveTurns(stream, feedback);
      const turn0 = turns[0];

      // File edits are rendered as tool cards alongside the calls that caused
      // them. Results update completion state without becoming extra cards.
      expect(turn0.toolGroups).toHaveLength(1);
      expect(turn0.toolGroups[0].eventIndices).toEqual([1, 3, 5, 6]);
    });

    it("turn 1 has one tool group with a single Bash tool_use", () => {
      const turns = deriveTurns(stream, feedback);
      const turn1 = turns[1];

      // Events in turn 1:
      //   0: text (non-empty, but no group open yet)
      //   1: tool_use(Bash) → starts group
      //   2: tool_result(Bash) → updates group
      //   3: done → ends group
      expect(turn1.toolGroups).toHaveLength(1);
      expect(turn1.toolGroups[0].eventIndices).toEqual([1]);
    });

    it("tool groups from turn 0 have correct startedAt timestamps", () => {
      const turns = deriveTurns(stream, feedback);
      // Group starts at the first tool_use (Read at base+100 = 1100)
      expect(turns[0].toolGroups[0].startedAt).toBe(1100);
    });
  });

  describe("file_edit aggregation", () => {
    it("turn 0 reports 1 edited file", () => {
      const turns = deriveTurns(stream, feedback);
      expect(turns[0].fileEditCount).toBe(1);
      expect(turns[0].fileEditPaths).toEqual(["src/git/push.rs"]);
    });

    it("turn 0 computes correct +/- line counts from the diff", () => {
      const turns = deriveTurns(stream, feedback);
      // Diff has 3 added lines (+pub async..., +let stream..., +stream.flush...)
      // and 1 removed line (-pub fn push_xorbs...)
      expect(turns[0].totalAdded).toBe(3);
      expect(turns[0].totalRemoved).toBe(1);
    });

    it("turn 1 reports 0 edited files", () => {
      const turns = deriveTurns(stream, feedback);
      expect(turns[1].fileEditCount).toBe(0);
      expect(turns[1].fileEditPaths).toEqual([]);
      expect(turns[1].totalAdded).toBe(0);
      expect(turns[1].totalRemoved).toBe(0);
    });
  });

  describe("CardId deterministic synthesis fallback (no itemId)", () => {
    const taskId = "task-claude-001";

    it("all events lack itemId, so deriveCardId uses the synthesis path", () => {
      const turns = deriveTurns(stream, feedback);

      for (let ti = 0; ti < turns.length; ti++) {
        const turn = turns[ti];
        for (let ei = 0; ei < turn.events.length; ei++) {
          const event = turn.events[ei];
          expect(event.itemId).toBeUndefined();

          const cardId = deriveCardId(taskId, ti, ei, event);
          expect(cardId).toBe(`${taskId}:${ti}:${ei}`);
        }
      }
    });

    it("synthesised CardIds are stable across multiple deriveTurns calls", () => {
      const turns1 = deriveTurns(stream, feedback);
      const turns2 = deriveTurns(stream, feedback);

      for (let ti = 0; ti < turns1.length; ti++) {
        for (let ei = 0; ei < turns1[ti].events.length; ei++) {
          const id1 = deriveCardId(taskId, ti, ei, turns1[ti].events[ei]);
          const id2 = deriveCardId(taskId, ti, ei, turns2[ti].events[ei]);
          expect(id1).toBe(id2);
        }
      }
    });

    it("deriveGroupId uses the first member's synthesised CardId", () => {
      const turns = deriveTurns(stream, feedback);
      const turn0 = turns[0];
      const group = turn0.toolGroups[0];

      const groupId = deriveGroupId(group, turn0.events, taskId, 0);
      const firstMemberCardId = deriveCardId(taskId, 0, group.eventIndices[0], turn0.events[group.eventIndices[0]]);
      expect(groupId).toBe(`g:${firstMemberCardId}`);
    });
  });

  describe("edge cases for Claude Code adapter", () => {
    it("stream with only a done event produces one empty complete turn", () => {
      const minimal: AgentEvent[] = [ccEvent("done", { timestamp: 5000 })];
      const turns = deriveTurns(minimal, []);
      expect(turns).toHaveLength(1);
      expect(turns[0].complete).toBe(true);
      expect(turns[0].events).toHaveLength(1);
      expect(turns[0].toolGroups).toHaveLength(0);
    });

    it("stream without any done event produces one incomplete turn", () => {
      const incomplete: AgentEvent[] = [
        ccEvent("text", { content: "Thinking...", timestamp: 1000 }),
        ccEvent("tool_use", { tool: "Read", path: "file.rs", timestamp: 1100 }),
      ];
      const turns = deriveTurns(incomplete, []);
      expect(turns).toHaveLength(1);
      expect(turns[0].complete).toBe(false);
      expect(turns[0].durationMs).toBeNull();
    });

    it("multiple file_edit events for the same path are deduplicated in fileEditPaths", () => {
      const events: AgentEvent[] = [
        ccEvent("file_edit", { path: "src/main.rs", diff: "+line1\n+line2", timestamp: 1000 }),
        ccEvent("file_edit", { path: "src/main.rs", diff: "+line3", timestamp: 1100 }),
        ccEvent("file_edit", { path: "src/lib.rs", diff: "-old\n+new", timestamp: 1200 }),
        ccEvent("done", { timestamp: 1300 }),
      ];
      const turns = deriveTurns(events, []);
      expect(turns[0].fileEditCount).toBe(2);
      expect(turns[0].fileEditPaths).toContain("src/main.rs");
      expect(turns[0].fileEditPaths).toContain("src/lib.rs");
      // Added: 2 + 1 + 1 = 4, Removed: 0 + 0 + 1 = 1
      expect(turns[0].totalAdded).toBe(4);
      expect(turns[0].totalRemoved).toBe(1);
    });

    it("text events with non-empty content split tool groups", () => {
      const events: AgentEvent[] = [
        ccEvent("tool_use", { tool: "Read", timestamp: 1000 }),
        ccEvent("text", { content: "Let me try a different approach.", timestamp: 1100 }),
        ccEvent("tool_use", { tool: "Write", timestamp: 1200 }),
        ccEvent("done", { timestamp: 1300 }),
      ];
      const turns = deriveTurns(events, []);
      expect(turns[0].toolGroups).toHaveLength(2);
      expect(turns[0].toolGroups[0].eventIndices).toEqual([0]);
      expect(turns[0].toolGroups[1].eventIndices).toEqual([2]);
    });
  });
});
