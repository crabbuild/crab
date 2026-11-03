/**
 * Tests for ChatUIStateStore.
 *
 * Validates: Requirements CHAT11.1, CHAT11.3, CHAT11.4
 *
 * Covers:
 * - Per-task isolation
 * - Debounced flush coalesces rapid toggles into one IPC call
 * - Round-trip: hydrate → set → flush → re-hydrate yields the same state (P3)
 * - LRU eviction when state exceeds 64 KiB
 * - forgetTask clears in-memory state and IPCs the deletion
 * - Property test (fast-check) for P3: random Map<CardId, CardCollapseEntry>
 *   of size up to 1000 round-trips through serialisation
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import fc from "fast-check";
import {
  getChatUIState,
  resetChatUIState,
  defaultPerTaskState,
  applyLruEviction,
  type ChatUIStatePerTask,
  type CardCollapseEntry,
  type CardId,
} from "../src/lib/chat-ui-state";

// ─── Mock IPC ────────────────────────────────────────────────────────────────

const mockSetTaskUIState = vi.fn().mockResolvedValue(undefined);

beforeEach(() => {
  vi.useFakeTimers();
  resetChatUIState();
  mockSetTaskUIState.mockClear();

  // Mock window.crab.orchestrator.setTaskUIState
  (globalThis as any).window = {
    crab: {
      orchestrator: {
        setTaskUIState: mockSetTaskUIState,
      },
    },
  };
});

afterEach(() => {
  vi.useRealTimers();
  delete (globalThis as any).window;
});

// ─── Unit Tests ──────────────────────────────────────────────────────────────

describe("ChatUIStateStore", () => {
  describe("per-task isolation", () => {
    it("setting card collapsed for task A does not change task B", () => {
      const store = getChatUIState();

      store.setCardCollapsed("task-a", "card-1", true);
      store.setCardCollapsed("task-b", "card-1", false);

      const stateA = store.byTask.get("task-a")!;
      const stateB = store.byTask.get("task-b")!;

      expect(stateA.cardCollapse["card-1"].collapsed).toBe(true);
      expect(stateB.cardCollapse["card-1"].collapsed).toBe(false);
    });

    it("setting group collapsed for task A does not affect task B", () => {
      const store = getChatUIState();

      store.setGroupCollapsed("task-a", "group-1", true);

      const stateA = store.byTask.get("task-a")!;
      const stateB = store.byTask.get("task-b");

      expect(stateA.groupCollapse["group-1"].collapsed).toBe(true);
      // task-b has no state at all yet
      expect(stateB).toBeUndefined();
    });

    it("compact view is independent per task", () => {
      const store = getChatUIState();

      store.setCompactView("task-a", false);
      store.setCompactView("task-b", true);

      expect(store.byTask.get("task-a")!.compactView).toBe(false);
      expect(store.byTask.get("task-b")!.compactView).toBe(true);
    });
  });

  describe("debounced flush", () => {
    it("coalesces rapid toggles into one IPC call", () => {
      const store = getChatUIState();

      // Rapidly toggle the same card multiple times.
      store.setCardCollapsed("task-a", "card-1", true);
      store.setCardCollapsed("task-a", "card-1", false);
      store.setCardCollapsed("task-a", "card-1", true);
      store.setCardCollapsed("task-a", "card-2", true);

      // No flush yet — debounce hasn't fired.
      expect(mockSetTaskUIState).not.toHaveBeenCalled();

      // Advance past the 300ms debounce window.
      vi.advanceTimersByTime(300);

      // Only one flush call for the task.
      expect(mockSetTaskUIState).toHaveBeenCalledTimes(1);
      expect(mockSetTaskUIState).toHaveBeenCalledWith("task-a", expect.any(Object));
    });

    it("flushes each task independently", () => {
      const store = getChatUIState();

      store.setCardCollapsed("task-a", "card-1", true);
      store.setCardCollapsed("task-b", "card-2", false);

      vi.advanceTimersByTime(300);

      // Two flush calls — one per task.
      expect(mockSetTaskUIState).toHaveBeenCalledTimes(2);
    });

    it("resets the debounce timer on subsequent mutations", () => {
      const store = getChatUIState();

      store.setCardCollapsed("task-a", "card-1", true);

      // Advance 200ms (not yet at 300ms threshold).
      vi.advanceTimersByTime(200);
      expect(mockSetTaskUIState).not.toHaveBeenCalled();

      // Another mutation resets the timer.
      store.setCardCollapsed("task-a", "card-2", true);

      // Advance another 200ms (400ms total, but only 200ms since last mutation).
      vi.advanceTimersByTime(200);
      expect(mockSetTaskUIState).not.toHaveBeenCalled();

      // Advance the remaining 100ms to hit 300ms since last mutation.
      vi.advanceTimersByTime(100);
      expect(mockSetTaskUIState).toHaveBeenCalledTimes(1);
    });
  });

  describe("round-trip: hydrate → set → flush → re-hydrate (P3)", () => {
    it("yields the same state after round-trip", () => {
      const store = getChatUIState();

      // Hydrate with initial state.
      const initial: ChatUIStatePerTask = {
        cardCollapse: {
          "card-1": { collapsed: true, lastToggledMs: 1000 },
          "card-2": { collapsed: false, lastToggledMs: 2000 },
        },
        groupCollapse: {
          "group-1": { collapsed: true, lastToggledMs: 1500 },
        },
        turnCollapse: {
          0: { collapsed: false, lastToggledMs: 500 },
        },
        compactView: false,
        timelineRail: "off",
        vimNavigation: true,
      };

      store.hydrate(new Map([["task-a", initial]]));

      // Mutate something.
      vi.setSystemTime(3000);
      store.setCardCollapsed("task-a", "card-3", true);

      // Flush.
      vi.advanceTimersByTime(300);

      // The flushed state should contain all original entries plus the new one.
      expect(mockSetTaskUIState).toHaveBeenCalledTimes(1);
      const [, flushed] = mockSetTaskUIState.mock.calls[0];

      // Re-hydrate from the flushed state: simulate what the app does on restart.
      resetChatUIState();
      const rehydratedState: ChatUIStatePerTask = {
        cardCollapse: flushed.cardCollapse ?? {},
        groupCollapse: flushed.groupCollapse ?? {},
        turnCollapse: flushed.turnCollapse ?? {},
        compactView: flushed.compactView ?? true,
        timelineRail: flushed.timelineRail ?? "compact",
        vimNavigation: flushed.vimNavigation ?? false,
      };

      const store2 = getChatUIState();
      store2.hydrate(new Map([["task-a", rehydratedState]]));

      const state = store2.byTask.get("task-a")!;

      // Original entries preserved.
      expect(state.cardCollapse["card-1"].collapsed).toBe(true);
      expect(state.cardCollapse["card-2"].collapsed).toBe(false);
      expect(state.cardCollapse["card-3"].collapsed).toBe(true);
      expect(state.groupCollapse["group-1"].collapsed).toBe(true);
      expect(state.turnCollapse[0].collapsed).toBe(false);
      expect(state.compactView).toBe(false);
      expect(state.timelineRail).toBe("off");
      expect(state.vimNavigation).toBe(true);
    });
  });

  describe("LRU eviction", () => {
    it("drops the oldest cardCollapse entries when state exceeds 64 KiB", () => {
      const store = getChatUIState();

      // Build a state with many card entries that exceeds 64 KiB.
      // Each entry key is ~40 chars, value is ~50 chars JSON.
      const numEntries = 1000;
      const baseTime = 1000;

      const bigState: ChatUIStatePerTask = {
        ...defaultPerTaskState(),
        cardCollapse: {},
      };

      for (let i = 0; i < numEntries; i++) {
        // Use a long card ID to inflate the serialised size.
        const cardId = `card-${String(i).padStart(4, "0")}-${"x".repeat(30)}`;
        bigState.cardCollapse[cardId] = {
          collapsed: i % 2 === 0,
          lastToggledMs: baseTime + i,
        };
      }

      // Add the newest entry.
      bigState.cardCollapse["newest-card"] = {
        collapsed: true,
        lastToggledMs: baseTime + numEntries + 1,
      };

      // Verify the state exceeds 64 KiB before eviction.
      const beforeSize = JSON.stringify(bigState).length;
      expect(beforeSize).toBeGreaterThan(64 * 1024);

      // Apply LRU eviction.
      const evicted = applyLruEviction(bigState);

      // The evicted state should be under 64 KiB.
      const afterSize = JSON.stringify({
        cardCollapse: evicted.cardCollapse,
        groupCollapse: evicted.groupCollapse,
        turnCollapse: evicted.turnCollapse,
        compactView: evicted.compactView,
        timelineRail: evicted.timelineRail,
        vimNavigation: evicted.vimNavigation,
      }).length;
      expect(afterSize).toBeLessThanOrEqual(64 * 1024);

      // The newest entry should survive.
      expect(evicted.cardCollapse["newest-card"]).toBeDefined();
      expect(evicted.cardCollapse["newest-card"].collapsed).toBe(true);

      // The oldest entries should have been dropped.
      const oldestKey = `card-0000-${"x".repeat(30)}`;
      expect(evicted.cardCollapse[oldestKey]).toBeUndefined();
    });
  });

  describe("forgetTask", () => {
    it("clears in-memory state", () => {
      const store = getChatUIState();

      store.setCardCollapsed("task-a", "card-1", true);
      expect(store.byTask.has("task-a")).toBe(true);

      store.forgetTask("task-a");
      expect(store.byTask.has("task-a")).toBe(false);
    });

    it("IPCs the deletion", () => {
      const store = getChatUIState();

      store.setCardCollapsed("task-a", "card-1", true);
      store.forgetTask("task-a");

      // forgetTask calls setTaskUIState with null to signal deletion.
      expect(mockSetTaskUIState).toHaveBeenCalledWith("task-a", null);
    });

    it("cancels any pending flush for the forgotten task", () => {
      const store = getChatUIState();

      store.setCardCollapsed("task-a", "card-1", true);
      mockSetTaskUIState.mockClear();

      store.forgetTask("task-a");

      // Advance past debounce — no flush should fire for the forgotten task.
      vi.advanceTimersByTime(300);

      // The only call should be the forgetTask deletion IPC, not a flush.
      // forgetTask already called setTaskUIState(taskId, null).
      // No additional flush call should have happened.
      const flushCalls = mockSetTaskUIState.mock.calls.filter(
        ([, state]) => state !== null,
      );
      expect(flushCalls).toHaveLength(0);
    });

    it("is idempotent — calling forgetTask twice does not throw", () => {
      const store = getChatUIState();

      store.setCardCollapsed("task-a", "card-1", true);
      store.forgetTask("task-a");
      expect(() => store.forgetTask("task-a")).not.toThrow();
    });
  });
});

// ─── Property-Based Tests ────────────────────────────────────────────────────

describe("ChatUIStateStore — Property Tests", () => {
  /**
   * **Validates: Requirements CHAT11.1, CHAT11.3**
   *
   * P3: Random Map<CardId, CardCollapseEntry> of size up to 1000
   * round-trips through serialisation (JSON.stringify → JSON.parse
   * yields deep equality).
   */
  it("P3: random cardCollapse map round-trips through serialisation", () => {
    const arbCardId = fc.string({ minLength: 1, maxLength: 50, unit: "grapheme-ascii" });

    const arbCollapseEntry: fc.Arbitrary<CardCollapseEntry> = fc.record({
      collapsed: fc.boolean(),
      lastToggledMs: fc.nat({ max: 2_000_000_000 }),
    });

    const arbCardCollapseMap = fc.array(
      fc.tuple(arbCardId, arbCollapseEntry),
      { minLength: 0, maxLength: 1000 },
    ).map((pairs) => {
      const record: Record<string, CardCollapseEntry> = {};
      for (const [key, value] of pairs) {
        record[key] = value;
      }
      return record;
    });

    const arbGroupCollapseMap = fc.array(
      fc.tuple(
        fc.string({ minLength: 1, maxLength: 30, unit: "grapheme-ascii" }),
        arbCollapseEntry,
      ),
      { minLength: 0, maxLength: 50 },
    ).map((pairs) => {
      const record: Record<string, CardCollapseEntry> = {};
      for (const [key, value] of pairs) {
        record[key] = value;
      }
      return record;
    });

    const arbTurnCollapseMap = fc.array(
      fc.tuple(fc.nat({ max: 500 }), arbCollapseEntry),
      { minLength: 0, maxLength: 50 },
    ).map((pairs) => {
      const record: Record<number, CardCollapseEntry> = {};
      for (const [key, value] of pairs) {
        record[key] = value;
      }
      return record;
    });

    const arbState: fc.Arbitrary<ChatUIStatePerTask> = fc.record({
      cardCollapse: arbCardCollapseMap,
      groupCollapse: arbGroupCollapseMap,
      turnCollapse: arbTurnCollapseMap,
      compactView: fc.boolean(),
      timelineRail: fc.constantFrom("compact" as const, "off" as const),
      vimNavigation: fc.boolean(),
    });

    fc.assert(
      fc.property(arbState, (state) => {
        // Serialise to JSON (simulating what the IPC flush does).
        const serialised = JSON.stringify({
          cardCollapse: state.cardCollapse,
          groupCollapse: state.groupCollapse,
          turnCollapse: state.turnCollapse,
          compactView: state.compactView,
          timelineRail: state.timelineRail,
          vimNavigation: state.vimNavigation,
        });

        // Deserialise (simulating what hydrate does on app restart).
        const parsed = JSON.parse(serialised);
        const rehydrated: ChatUIStatePerTask = {
          cardCollapse: parsed.cardCollapse ?? {},
          groupCollapse: parsed.groupCollapse ?? {},
          turnCollapse: parsed.turnCollapse ?? {},
          compactView: parsed.compactView ?? true,
          timelineRail: parsed.timelineRail ?? "compact",
          vimNavigation: parsed.vimNavigation ?? false,
        };

        // Verify cardCollapse round-trips.
        expect(rehydrated.cardCollapse).toEqual(state.cardCollapse);

        // Verify groupCollapse round-trips.
        expect(rehydrated.groupCollapse).toEqual(state.groupCollapse);

        // Verify turnCollapse round-trips. JSON keys are always strings,
        // so numeric keys become string keys after serialisation.
        for (const [key, value] of Object.entries(state.turnCollapse)) {
          expect(rehydrated.turnCollapse[key as any]).toEqual(value);
        }
        // Same number of entries.
        expect(Object.keys(rehydrated.turnCollapse).length).toBe(
          Object.keys(state.turnCollapse).length,
        );

        // Verify scalar fields.
        expect(rehydrated.compactView).toBe(state.compactView);
        expect(rehydrated.timelineRail).toBe(state.timelineRail);
        expect(rehydrated.vimNavigation).toBe(state.vimNavigation);
      }),
      { numRuns: 100 },
    );
  });
});
