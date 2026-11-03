/**
 * Multi-adapter regression: minimal adapter stream.
 *
 * Verifies that deriveTurns works correctly with a minimal/generic
 * adapter stream that only emits `text` + `done` events — no tool_use,
 * no itemId, no input.kind, no commandActions. This proves the chat
 * surface is truly adapter-agnostic and handles the lowest-effort
 * AgentEvent shape gracefully.
 */

import { describe, it, expect } from "vitest";
import { readFileSync } from "fs";
import { resolve } from "path";

import {
  deriveTurns,
  deriveCardId,
  type Turn,
} from "../src/lib/turn-derivation";
import type { AgentEvent } from "../electron/orchestrator-adapters";

// ─── Fixture loading ─────────────────────────────────────────────────────────

function loadFixture(filename: string): AgentEvent[] {
  const filepath = resolve(__dirname, "fixtures", filename);
  const raw = readFileSync(filepath, "utf-8");
  return raw
    .split("\n")
    .filter((line) => line.trim().length > 0)
    .map((line) => JSON.parse(line) as AgentEvent);
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("Multi-adapter regression: minimal adapter stream", () => {
  const events = loadFixture("minimal-adapter-stream.ndjson");

  describe("turn boundaries detected from `done` events alone", () => {
    it("produces the correct number of turns from done events", () => {
      const turns = deriveTurns(events, []);
      const doneCount = events.filter((e) => e.type === "done").length;
      expect(turns).toHaveLength(doneCount);
    });

    it("every turn is marked complete", () => {
      const turns = deriveTurns(events, []);
      for (const turn of turns) {
        expect(turn.complete).toBe(true);
      }
    });

    it("turn indices are sequential starting from 0", () => {
      const turns = deriveTurns(events, []);
      turns.forEach((turn, i) => {
        expect(turn.turnIndex).toBe(i);
      });
    });

    it("each turn contains the events between done boundaries", () => {
      const turns = deriveTurns(events, []);

      // Turn 0: 2 text events + 1 done = 3 events
      expect(turns[0].events).toHaveLength(3);
      expect(turns[0].events[0].type).toBe("text");
      expect(turns[0].events[1].type).toBe("text");
      expect(turns[0].events[2].type).toBe("done");

      // Turn 1: 2 text events + 1 done = 3 events
      expect(turns[1].events).toHaveLength(3);
      expect(turns[1].events[0].type).toBe("text");
      expect(turns[1].events[1].type).toBe("text");
      expect(turns[1].events[2].type).toBe("done");

      // Turn 2: 1 text event + 1 done = 2 events
      expect(turns[2].events).toHaveLength(2);
      expect(turns[2].events[0].type).toBe("text");
      expect(turns[2].events[1].type).toBe("done");
    });

    it("computes wall-clock duration correctly from timestamps", () => {
      const turns = deriveTurns(events, []);

      // Turn 0: 1700000003000 - 1700000001000 = 2000ms
      expect(turns[0].durationMs).toBe(2000);
      // Turn 1: 1700000012000 - 1700000010000 = 2000ms
      expect(turns[1].durationMs).toBe(2000);
      // Turn 2: 1700000021000 - 1700000020000 = 1000ms
      expect(turns[2].durationMs).toBe(1000);
    });
  });

  describe("zero tool groups when stream has no tool_use events", () => {
    it("every turn has an empty toolGroups array", () => {
      const turns = deriveTurns(events, []);
      for (const turn of turns) {
        expect(turn.toolGroups).toHaveLength(0);
      }
    });

    it("file edit counts are zero for text-only turns", () => {
      const turns = deriveTurns(events, []);
      for (const turn of turns) {
        expect(turn.fileEditCount).toBe(0);
        expect(turn.fileEditPaths).toEqual([]);
        expect(turn.totalAdded).toBe(0);
        expect(turn.totalRemoved).toBe(0);
      }
    });
  });

  describe("Card_Id synthesis fallback (no itemId)", () => {
    it("derives deterministic Card_Ids from (taskId, turnIndex, eventIndex)", () => {
      const taskId = "test-task-123";
      const turns = deriveTurns(events, []);

      for (const turn of turns) {
        for (let i = 0; i < turn.events.length; i++) {
          const event = turn.events[i];
          // None of the minimal events have itemId
          expect(event.itemId).toBeUndefined();

          const cardId = deriveCardId(taskId, turn.turnIndex, i, event);
          expect(cardId).toBe(`${taskId}:${turn.turnIndex}:${i}`);
        }
      }
    });

    it("Card_Ids are stable across repeated derivations", () => {
      const taskId = "stable-task";
      const turns1 = deriveTurns(events, []);
      const turns2 = deriveTurns(events, []);

      for (let t = 0; t < turns1.length; t++) {
        for (let i = 0; i < turns1[t].events.length; i++) {
          const id1 = deriveCardId(taskId, t, i, turns1[t].events[i]);
          const id2 = deriveCardId(taskId, t, i, turns2[t].events[i]);
          expect(id1).toBe(id2);
        }
      }
    });

    it("Card_Ids are unique across all events in the task", () => {
      const taskId = "unique-task";
      const turns = deriveTurns(events, []);
      const allIds = new Set<string>();

      for (const turn of turns) {
        for (let i = 0; i < turn.events.length; i++) {
          const cardId = deriveCardId(taskId, turn.turnIndex, i, turn.events[i]);
          expect(allIds.has(cardId)).toBe(false);
          allIds.add(cardId);
        }
      }
    });
  });

  describe("user message assignment from feedback array", () => {
    it("turn 0 has no user message (orchestrator-initiated)", () => {
      const turns = deriveTurns(events, ["prompt 1", "prompt 2"]);
      expect(turns[0].userMessage).toBeNull();
    });

    it("subsequent turns pick up feedback entries", () => {
      const turns = deriveTurns(events, ["prompt 1", "prompt 2"]);
      expect(turns[1].userMessage).toBe("prompt 1");
      expect(turns[2].userMessage).toBe("prompt 2");
    });

    it("missing feedback entries result in null user messages", () => {
      const turns = deriveTurns(events, []);
      for (const turn of turns) {
        expect(turn.userMessage).toBeNull();
      }
    });
  });
});
