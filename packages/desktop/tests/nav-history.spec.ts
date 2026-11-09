import { describe, it, expect, vi } from "vitest";
import { VIEW_LABELS } from "../src/lib/navigation";
import type { NavEntry } from "../src/hooks/useNavHistory";

/**
 * Tests for the navigation history logic.
 *
 * Since useNavHistory is a React hook and we don't have @testing-library/react-hooks,
 * we test the core navigation logic directly by reimplementing the same algorithm
 * as a plain class. The hook is a thin wrapper around this logic with React state.
 */

const NAV_HISTORY_CAP = 50;

const KNOWN_NAV_VIEWS = new Set(Object.keys(VIEW_LABELS));

/** Minimal reimplementation of the nav history logic for testing. */
class NavHistoryTestHarness {
  stack: NavEntry[] = [{ view: "explorer" }];
  index = 0;
  onRepoSwitch?: (repoPath: string) => void;

  constructor(onRepoSwitch?: (repoPath: string) => void) {
    this.onRepoSwitch = onRepoSwitch;
  }

  get current(): NavEntry | undefined {
    return this.stack[this.index];
  }

  get canBack(): boolean {
    return this.index > 0;
  }

  get canForward(): boolean {
    return this.index < this.stack.length - 1;
  }

  push(entry: NavEntry): void {
    this.stack.splice(this.index + 1);
    this.stack.push(entry);

    // Cap at NAV_HISTORY_CAP — trim oldest entries from the beginning.
    if (this.stack.length > NAV_HISTORY_CAP) {
      const overflow = this.stack.length - NAV_HISTORY_CAP;
      this.stack.splice(0, overflow);
    }

    this.index = this.stack.length - 1;
  }

  back(): void {
    if (this.index > 0) {
      const prevEntry = this.stack[this.index];
      // Skip dead entries (views that no longer exist).
      let targetIdx = this.index - 1;
      while (targetIdx > 0 && !KNOWN_NAV_VIEWS.has(this.stack[targetIdx].view)) {
        targetIdx--;
      }
      // If the final candidate is also dead, don't navigate.
      if (!KNOWN_NAV_VIEWS.has(this.stack[targetIdx].view)) return;

      this.index = targetIdx;
      const newEntry = this.stack[this.index];
      if (
        this.onRepoSwitch &&
        newEntry?.repoPath != null &&
        newEntry.repoPath !== (prevEntry?.repoPath ?? null)
      ) {
        this.onRepoSwitch(newEntry.repoPath);
      }
    }
  }

  forward(): void {
    if (this.index < this.stack.length - 1) {
      const prevEntry = this.stack[this.index];
      // Skip dead entries (views that no longer exist).
      let targetIdx = this.index + 1;
      const maxIdx = this.stack.length - 1;
      while (targetIdx < maxIdx && !KNOWN_NAV_VIEWS.has(this.stack[targetIdx].view)) {
        targetIdx++;
      }
      // If the final candidate is also dead, don't navigate.
      if (!KNOWN_NAV_VIEWS.has(this.stack[targetIdx].view)) return;

      this.index = targetIdx;
      const newEntry = this.stack[this.index];
      if (
        this.onRepoSwitch &&
        newEntry?.repoPath != null &&
        newEntry.repoPath !== (prevEntry?.repoPath ?? null)
      ) {
        this.onRepoSwitch(newEntry.repoPath);
      }
    }
  }

  getState(): { entries: NavEntry[]; index: number } {
    return { entries: [...this.stack], index: this.index };
  }

  setState(entries: NavEntry[], index: number): void {
    let trimmed = entries;
    let adjustedIndex = index;
    if (trimmed.length > NAV_HISTORY_CAP) {
      const overflow = trimmed.length - NAV_HISTORY_CAP;
      trimmed = trimmed.slice(overflow);
      adjustedIndex = Math.max(0, adjustedIndex - overflow);
    }
    adjustedIndex = Math.max(0, Math.min(adjustedIndex, trimmed.length - 1));
    this.stack = trimmed.length > 0 ? trimmed : [{ view: "explorer" }];
    this.index = trimmed.length > 0 ? adjustedIndex : 0;
  }
}

describe("useNavHistory", () => {
  describe("basic navigation", () => {
    it("starts with a default explorer entry", () => {
      const nav = new NavHistoryTestHarness();
      expect(nav.current).toEqual({ view: "explorer" });
      expect(nav.canBack).toBe(false);
      expect(nav.canForward).toBe(false);
    });

    it("push adds entry and updates current", () => {
      const nav = new NavHistoryTestHarness();
      nav.push({ view: "timeline", commit: "abc123" });
      expect(nav.current).toEqual({ view: "timeline", commit: "abc123" });
      expect(nav.canBack).toBe(true);
      expect(nav.canForward).toBe(false);
    });

    it("back restores previous entry", () => {
      const nav = new NavHistoryTestHarness();
      nav.push({ view: "timeline" });
      nav.back();
      expect(nav.current).toEqual({ view: "explorer" });
      expect(nav.canBack).toBe(false);
      expect(nav.canForward).toBe(true);
    });

    it("forward restores next entry", () => {
      const nav = new NavHistoryTestHarness();
      nav.push({ view: "timeline" });
      nav.back();
      nav.forward();
      expect(nav.current).toEqual({ view: "timeline" });
      expect(nav.canForward).toBe(false);
    });

    it("push truncates forward history", () => {
      const nav = new NavHistoryTestHarness();
      nav.push({ view: "timeline" });
      nav.push({ view: "changes" });
      nav.back();
      nav.push({ view: "search" });
      expect(nav.current).toEqual({ view: "search" });
      expect(nav.canForward).toBe(false);
    });

    it("back at start is a no-op", () => {
      const nav = new NavHistoryTestHarness();
      nav.back();
      expect(nav.current).toEqual({ view: "explorer" });
    });

    it("forward at end is a no-op", () => {
      const nav = new NavHistoryTestHarness();
      nav.forward();
      expect(nav.current).toEqual({ view: "explorer" });
    });
  });

  describe("cross-repo navigation with repoPath", () => {
    it("push preserves repoPath in entry", () => {
      const nav = new NavHistoryTestHarness();
      nav.push({ view: "explorer", repoPath: "frontend" });
      expect(nav.current).toEqual({ view: "explorer", repoPath: "frontend" });
    });

    it("back triggers onRepoSwitch when repoPath differs", () => {
      const onRepoSwitch = vi.fn();
      const nav = new NavHistoryTestHarness(onRepoSwitch);
      nav.push({ view: "explorer", repoPath: "frontend" });
      nav.push({ view: "timeline", repoPath: "backend" });
      nav.back();
      expect(onRepoSwitch).toHaveBeenCalledWith("frontend");
      expect(nav.current).toEqual({ view: "explorer", repoPath: "frontend" });
    });

    it("forward triggers onRepoSwitch when repoPath differs", () => {
      const onRepoSwitch = vi.fn();
      const nav = new NavHistoryTestHarness(onRepoSwitch);
      nav.push({ view: "explorer", repoPath: "frontend" });
      nav.push({ view: "timeline", repoPath: "backend" });
      nav.back();
      onRepoSwitch.mockClear();
      nav.forward();
      expect(onRepoSwitch).toHaveBeenCalledWith("backend");
      expect(nav.current).toEqual({ view: "timeline", repoPath: "backend" });
    });

    it("back does NOT trigger onRepoSwitch when repoPath is the same", () => {
      const onRepoSwitch = vi.fn();
      const nav = new NavHistoryTestHarness(onRepoSwitch);
      nav.push({ view: "explorer", repoPath: "frontend" });
      nav.push({ view: "timeline", repoPath: "frontend" });
      nav.back();
      expect(onRepoSwitch).not.toHaveBeenCalled();
    });

    it("back does NOT trigger onRepoSwitch when target repoPath is null", () => {
      const onRepoSwitch = vi.fn();
      const nav = new NavHistoryTestHarness(onRepoSwitch);
      // Initial entry has no repoPath (undefined).
      nav.push({ view: "timeline", repoPath: "backend" });
      nav.back();
      // The initial entry has no repoPath, so no switch should fire.
      expect(onRepoSwitch).not.toHaveBeenCalled();
    });

    it("records repo switches as navigation entries with correct count", () => {
      const nav = new NavHistoryTestHarness();
      nav.push({ view: "explorer", repoPath: "frontend" });
      nav.push({ view: "explorer", repoPath: "backend" });
      nav.push({ view: "explorer", repoPath: "shared-lib" });
      // 4 entries total (initial + 3 pushes).
      expect(nav.stack.length).toBe(4);
      expect(nav.canBack).toBe(true);
      nav.back();
      expect(nav.current?.repoPath).toBe("backend");
      nav.back();
      expect(nav.current?.repoPath).toBe("frontend");
    });

    it("preserves view state (view, commit, repoPath) across back/forward", () => {
      const onRepoSwitch = vi.fn();
      const nav = new NavHistoryTestHarness(onRepoSwitch);
      nav.push({ view: "timeline", commit: "abc", repoPath: "frontend" });
      nav.push({ view: "changes", commit: null, repoPath: "backend" });
      nav.back();
      expect(nav.current).toEqual({
        view: "timeline",
        commit: "abc",
        repoPath: "frontend",
      });
      nav.forward();
      expect(nav.current).toEqual({
        view: "changes",
        commit: null,
        repoPath: "backend",
      });
    });

    it("back then forward is inverse (returns to same entry)", () => {
      const nav = new NavHistoryTestHarness();
      nav.push({ view: "timeline", commit: "sha1", repoPath: "frontend" });
      nav.push({ view: "changes", commit: "sha2", repoPath: "backend" });
      const beforeBack = nav.current;
      nav.back();
      nav.forward();
      expect(nav.current).toEqual(beforeBack);
    });

    it("multiple back/forward cycles with repo switches", () => {
      const onRepoSwitch = vi.fn();
      const nav = new NavHistoryTestHarness(onRepoSwitch);
      nav.push({ view: "explorer", repoPath: "repo-a" });
      nav.push({ view: "timeline", repoPath: "repo-b" });
      nav.push({ view: "changes", repoPath: "repo-c" });

      // Go back twice.
      nav.back(); // repo-c → repo-b
      expect(onRepoSwitch).toHaveBeenLastCalledWith("repo-b");
      nav.back(); // repo-b → repo-a
      expect(onRepoSwitch).toHaveBeenLastCalledWith("repo-a");

      // Go forward twice.
      onRepoSwitch.mockClear();
      nav.forward(); // repo-a → repo-b
      expect(onRepoSwitch).toHaveBeenLastCalledWith("repo-b");
      nav.forward(); // repo-b → repo-c
      expect(onRepoSwitch).toHaveBeenLastCalledWith("repo-c");
    });
  });

  describe("getState and setState", () => {
    it("getState returns current entries and index", () => {
      const nav = new NavHistoryTestHarness();
      nav.push({ view: "timeline" });
      nav.push({ view: "changes" });
      const state = nav.getState();
      expect(state.entries).toEqual([
        { view: "explorer" },
        { view: "timeline" },
        { view: "changes" },
      ]);
      expect(state.index).toBe(2);
    });

    it("getState returns a copy (mutations do not affect internal state)", () => {
      const nav = new NavHistoryTestHarness();
      nav.push({ view: "timeline" });
      const state = nav.getState();
      state.entries.push({ view: "injected" });
      state.index = 99;
      expect(nav.stack.length).toBe(2);
      expect(nav.index).toBe(1);
    });

    it("setState replaces the internal stack and index", () => {
      const nav = new NavHistoryTestHarness();
      nav.setState(
        [{ view: "changes" }, { view: "timeline" }, { view: "search" }],
        1,
      );
      expect(nav.current).toEqual({ view: "timeline" });
      expect(nav.canBack).toBe(true);
      expect(nav.canForward).toBe(true);
    });

    it("setState clamps index to valid range (too high)", () => {
      const nav = new NavHistoryTestHarness();
      nav.setState([{ view: "a" }, { view: "b" }], 99);
      expect(nav.index).toBe(1);
      expect(nav.current).toEqual({ view: "b" });
    });

    it("setState clamps index to valid range (negative)", () => {
      const nav = new NavHistoryTestHarness();
      nav.setState([{ view: "a" }, { view: "b" }], -5);
      expect(nav.index).toBe(0);
      expect(nav.current).toEqual({ view: "a" });
    });

    it("setState with empty array falls back to default explorer entry", () => {
      const nav = new NavHistoryTestHarness();
      nav.push({ view: "timeline" });
      nav.setState([], 0);
      expect(nav.current).toEqual({ view: "explorer" });
      expect(nav.index).toBe(0);
    });

    it("setState trims entries exceeding the cap and adjusts index", () => {
      const entries: NavEntry[] = Array.from({ length: 60 }, (_, i) => ({
        view: `view-${i}`,
      }));
      const nav = new NavHistoryTestHarness();
      nav.setState(entries, 55);
      // Should trim 10 oldest entries (60 - 50 = 10).
      expect(nav.stack.length).toBe(50);
      expect(nav.stack[0]).toEqual({ view: "view-10" });
      // Index adjusted: 55 - 10 = 45.
      expect(nav.index).toBe(45);
      expect(nav.current).toEqual({ view: "view-55" });
    });

    it("round-trip: getState then setState preserves state", () => {
      const nav = new NavHistoryTestHarness();
      nav.push({ view: "timeline", commit: "abc" });
      nav.push({ view: "changes", repoPath: "frontend" });
      nav.back();
      const state = nav.getState();

      const nav2 = new NavHistoryTestHarness();
      nav2.setState(state.entries, state.index);
      expect(nav2.current).toEqual(nav.current);
      expect(nav2.canBack).toBe(nav.canBack);
      expect(nav2.canForward).toBe(nav.canForward);
      expect(nav2.stack).toEqual(nav.stack);
    });
  });

  describe("entry cap (50 entries)", () => {
    it("push trims oldest entries when exceeding cap", () => {
      const nav = new NavHistoryTestHarness();
      // Push 55 entries (initial explorer + 55 pushes = 56, but cap trims to 50).
      for (let i = 0; i < 55; i++) {
        nav.push({ view: `view-${i}` });
      }
      expect(nav.stack.length).toBe(50);
      // The oldest entries should have been trimmed.
      expect(nav.stack[0]).toEqual({ view: "view-5" });
      // Current should be the last pushed entry.
      expect(nav.current).toEqual({ view: "view-54" });
      expect(nav.index).toBe(49);
    });

    it("push at exactly cap does not trim", () => {
      const nav = new NavHistoryTestHarness();
      // Push 49 entries (initial + 49 = 50, exactly at cap).
      for (let i = 0; i < 49; i++) {
        nav.push({ view: `view-${i}` });
      }
      expect(nav.stack.length).toBe(50);
      expect(nav.stack[0]).toEqual({ view: "explorer" });
    });

    it("push one over cap trims exactly one entry", () => {
      const nav = new NavHistoryTestHarness();
      // Fill to cap.
      for (let i = 0; i < 49; i++) {
        nav.push({ view: `view-${i}` });
      }
      expect(nav.stack.length).toBe(50);
      // One more push should trim the oldest.
      nav.push({ view: "overflow" });
      expect(nav.stack.length).toBe(50);
      expect(nav.stack[0]).toEqual({ view: "view-0" });
      expect(nav.current).toEqual({ view: "overflow" });
    });
  });

  describe("nav history round-trip (deactivate/activate simulation)", () => {
    it("push 5 entries, getState, setState on new instance — all 5 entries and correct index restored", () => {
      const nav = new NavHistoryTestHarness();
      nav.push({ view: "timeline", commit: "sha1" });
      nav.push({ view: "changes" });
      nav.push({ view: "worktrees", repoPath: "frontend" });
      nav.push({ view: "search" });
      nav.push({ view: "settings" });

      // Simulate deactivation: capture state.
      const state = nav.getState();
      expect(state.entries.length).toBe(6); // initial explorer + 5 pushes
      expect(state.index).toBe(5);

      // Simulate activation on a new instance: restore state.
      const nav2 = new NavHistoryTestHarness();
      nav2.setState(state.entries, state.index);

      // Verify all entries are restored.
      expect(nav2.stack.length).toBe(6);
      expect(nav2.stack[0]).toEqual({ view: "explorer" });
      expect(nav2.stack[1]).toEqual({ view: "timeline", commit: "sha1" });
      expect(nav2.stack[2]).toEqual({ view: "changes" });
      expect(nav2.stack[3]).toEqual({ view: "worktrees", repoPath: "frontend" });
      expect(nav2.stack[4]).toEqual({ view: "search" });
      expect(nav2.stack[5]).toEqual({ view: "settings" });
      // Verify correct index.
      expect(nav2.index).toBe(5);
      expect(nav2.current).toEqual({ view: "settings" });
      // Verify navigation still works after restore.
      expect(nav2.canBack).toBe(true);
      expect(nav2.canForward).toBe(false);
      nav2.back();
      expect(nav2.current).toEqual({ view: "search" });
    });

    it("dead entry skipping: push entry with nonexistent view, restore, back skips it", () => {
      const nav = new NavHistoryTestHarness();
      nav.push({ view: "timeline" });
      nav.push({ view: "nonexistent" }); // dead entry
      nav.push({ view: "changes" });

      // Simulate deactivate/activate round-trip.
      const state = nav.getState();
      const nav2 = new NavHistoryTestHarness();
      nav2.setState(state.entries, state.index);

      // Current is "changes" (index 3).
      expect(nav2.current).toEqual({ view: "changes" });

      // Navigate back — should skip "nonexistent" and land on "timeline".
      nav2.back();
      expect(nav2.current).toEqual({ view: "timeline" });
    });

    it("dead entry skipping: multiple consecutive dead entries are all skipped", () => {
      const nav = new NavHistoryTestHarness();
      nav.push({ view: "timeline" });
      nav.push({ view: "dead-view-1" });
      nav.push({ view: "dead-view-2" });
      nav.push({ view: "dead-view-3" });
      nav.push({ view: "changes" });

      const state = nav.getState();
      const nav2 = new NavHistoryTestHarness();
      nav2.setState(state.entries, state.index);

      expect(nav2.current).toEqual({ view: "changes" });
      nav2.back();
      // Should skip all 3 dead entries and land on "timeline".
      expect(nav2.current).toEqual({ view: "timeline" });
    });

    it("dead entry skipping: forward also skips dead entries", () => {
      const nav = new NavHistoryTestHarness();
      nav.push({ view: "timeline" });
      nav.push({ view: "nonexistent" });
      nav.push({ view: "changes" });

      // Go back to timeline.
      const state = nav.getState();
      const nav2 = new NavHistoryTestHarness();
      nav2.setState(state.entries, 1); // index 1 = "timeline"

      expect(nav2.current).toEqual({ view: "timeline" });
      // Forward should skip "nonexistent" and land on "changes".
      nav2.forward();
      expect(nav2.current).toEqual({ view: "changes" });
    });

    it("dead entry skipping: if all entries behind are dead, back is a no-op", () => {
      const nav = new NavHistoryTestHarness();
      // Replace the initial explorer with a dead entry via setState.
      nav.setState(
        [{ view: "dead-1" }, { view: "dead-2" }, { view: "changes" }],
        2,
      );
      expect(nav.current).toEqual({ view: "changes" });
      nav.back();
      // Should not navigate since all entries behind are dead.
      expect(nav.current).toEqual({ view: "changes" });
      expect(nav.index).toBe(2);
    });

    it("cap: push 60 entries, verify only 50 are retained (oldest trimmed)", () => {
      const nav = new NavHistoryTestHarness();
      // Push 60 entries (initial explorer + 60 pushes = 61 total before trimming).
      for (let i = 0; i < 60; i++) {
        nav.push({ view: "timeline", commit: `sha-${i}` });
      }
      // Cap is 50, so only 50 entries should remain.
      expect(nav.stack.length).toBe(50);
      // The oldest entries (initial explorer + first 10 pushes) should be trimmed.
      // 61 total - 50 cap = 11 trimmed from the front.
      expect(nav.stack[0]).toEqual({ view: "timeline", commit: "sha-10" });
      expect(nav.stack[49]).toEqual({ view: "timeline", commit: "sha-59" });
      expect(nav.current).toEqual({ view: "timeline", commit: "sha-59" });
      expect(nav.index).toBe(49);
    });
  });
});
