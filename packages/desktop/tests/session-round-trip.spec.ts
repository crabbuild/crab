/**
 * End-to-end session round-trip test.
 *
 * Exercises the SessionManager directly — simulating the deactivate/activate
 * cycle that Shell performs when the user switches repos. Validates that
 * active view, scroll positions, and active file survive a round-trip
 * through both the in-memory cache path and the cold-start (localStorage) path.
 *
 * Validates: Requirements 1.2, 3.2, 2.2
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { SessionManager } from "../src/lib/session-manager";

// ---------------------------------------------------------------------------
// localStorage mock
// ---------------------------------------------------------------------------

function createLocalStorageMock() {
  const store = new Map<string, string>();
  return {
    getItem: vi.fn((key: string) => store.get(key) ?? null),
    setItem: vi.fn((key: string, value: string) => { store.set(key, value); }),
    removeItem: vi.fn((key: string) => { store.delete(key); }),
    clear: vi.fn(() => { store.clear(); }),
    get length() { return store.size; },
    key: vi.fn((index: number) => [...store.keys()][index] ?? null),
    _store: store,
  };
}

// ---------------------------------------------------------------------------
// Setup / teardown
// ---------------------------------------------------------------------------

let mockStorage: ReturnType<typeof createLocalStorageMock>;

beforeEach(() => {
  vi.useFakeTimers();
  mockStorage = createLocalStorageMock();
  Object.defineProperty(globalThis, "localStorage", { value: mockStorage, writable: true });
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("Session round-trip: warm switch (in-memory cache)", () => {
  it("restores activeView, scrollPositions, and activeFile after repo switch", () => {
    const mgr = new SessionManager();
    const repoA = "/repos/repo-a";
    const repoB = "/repos/repo-b";

    // Step 1: Simulate repo A being active — user navigates to Timeline,
    // scrolls down, opens a file in diff.
    mgr.deactivate(repoA, {
      activeView: "timeline",
      scrollPositions: { timeline: 500 },
      activeFile: "/repo-a/src/main.ts",
    });

    // Step 2: Activate repo B (simulating the switch).
    mgr.activate(repoB);

    // Step 3: Switch back to repo A.
    const restored = mgr.activate(repoA);

    // Step 4: Assert session is fully restored.
    expect(restored).not.toBeNull();
    expect(restored!.activeView).toBe("timeline");
    expect(restored!.scrollPositions.timeline).toBe(500);
    expect(restored!.activeFile).toBe("/repo-a/src/main.ts");
  });

  it("preserves incremental updates through the switch cycle", () => {
    const mgr = new SessionManager();
    const repoA = "/repos/repo-a";
    const repoB = "/repos/repo-b";

    // Initial activation of repo A with some state.
    mgr.deactivate(repoA, {
      activeView: "explorer",
      scrollPositions: { explorer: 100 },
      activeFile: undefined,
    });

    // User navigates within repo A — incremental updates.
    mgr.update(repoA, { activeView: "timeline" });
    mgr.update(repoA, { scrollPositions: { timeline: 500, explorer: 100 } });
    mgr.update(repoA, { activeFile: "/repo-a/src/main.ts" });

    // Deactivate repo A (switch to B).
    const currentSession = mgr.activate(repoA);
    mgr.deactivate(repoA, {
      activeView: currentSession!.activeView,
      scrollPositions: currentSession!.scrollPositions,
      activeFile: currentSession!.activeFile,
    });

    // Activate repo B.
    mgr.deactivate(repoB, { activeView: "changes" });
    mgr.activate(repoB);

    // Switch back to repo A.
    const restored = mgr.activate(repoA);

    expect(restored).not.toBeNull();
    expect(restored!.activeView).toBe("timeline");
    expect(restored!.scrollPositions.timeline).toBe(500);
    expect(restored!.scrollPositions.explorer).toBe(100);
    expect(restored!.activeFile).toBe("/repo-a/src/main.ts");
  });

  it("multiple round-trips preserve state correctly", () => {
    const mgr = new SessionManager();
    const repoA = "/repos/repo-a";
    const repoB = "/repos/repo-b";

    // First activation of repo A.
    mgr.deactivate(repoA, {
      activeView: "timeline",
      scrollPositions: { timeline: 500 },
      activeFile: "/repo-a/src/main.ts",
    });

    // Switch to B.
    mgr.deactivate(repoB, { activeView: "explorer" });

    // Switch back to A.
    const firstRestore = mgr.activate(repoA);
    expect(firstRestore!.activeView).toBe("timeline");

    // User changes state in A.
    mgr.update(repoA, { activeView: "branches", scrollPositions: { branches: 200 } });

    // Switch to B again.
    const updatedA = mgr.activate(repoA);
    mgr.deactivate(repoA, {
      activeView: updatedA!.activeView,
      scrollPositions: updatedA!.scrollPositions,
      activeFile: updatedA!.activeFile,
    });
    mgr.activate(repoB);

    // Switch back to A — should have the updated state.
    const secondRestore = mgr.activate(repoA);
    expect(secondRestore!.activeView).toBe("branches");
    expect(secondRestore!.scrollPositions.branches).toBe(200);
    expect(secondRestore!.activeFile).toBe("/repo-a/src/main.ts");
  });
});

describe("Session round-trip: cold start (localStorage)", () => {
  it("restores session from localStorage after flush and new manager creation", () => {
    const repoA = "/repos/repo-a";

    // First manager: simulate user activity and flush to localStorage.
    const mgr1 = new SessionManager();
    mgr1.deactivate(repoA, {
      activeView: "timeline",
      scrollPositions: { timeline: 500 },
      activeFile: "/repo-a/src/main.ts",
    });
    mgr1.flush();

    // Second manager: simulates app restart (cold start, no in-memory cache).
    const mgr2 = new SessionManager();
    const restored = mgr2.activate(repoA);

    expect(restored).not.toBeNull();
    expect(restored!.activeView).toBe("timeline");
    expect(restored!.scrollPositions.timeline).toBe(500);
    expect(restored!.activeFile).toBe("/repo-a/src/main.ts");
  });

  it("cold start preserves full session state including navHistory and viewState", () => {
    const repoA = "/repos/repo-a";

    const mgr1 = new SessionManager();
    mgr1.deactivate(repoA, {
      activeView: "timeline",
      scrollPositions: { timeline: 500, explorer: 0 },
      activeFile: "/repo-a/src/main.ts",
      navHistory: [
        { view: "explorer" },
        { view: "timeline", commit: "abc123" },
      ],
      navIndex: 1,
      viewState: { selectedCommit: "abc123" },
    });
    mgr1.flush();

    // Simulate cold start.
    const mgr2 = new SessionManager();
    const restored = mgr2.activate(repoA);

    expect(restored).not.toBeNull();
    expect(restored!.activeView).toBe("timeline");
    expect(restored!.scrollPositions).toEqual({ timeline: 500, explorer: 0 });
    expect(restored!.activeFile).toBe("/repo-a/src/main.ts");
    expect(restored!.navHistory).toEqual([
      { view: "explorer" },
      { view: "timeline", commit: "abc123" },
    ]);
    expect(restored!.navIndex).toBe(1);
    expect(restored!.viewState).toEqual({ selectedCommit: "abc123" });
  });

  it("cold start with multiple repos restores each independently", () => {
    const repoA = "/repos/repo-a";
    const repoB = "/repos/repo-b";

    const mgr1 = new SessionManager();
    mgr1.deactivate(repoA, {
      activeView: "timeline",
      scrollPositions: { timeline: 500 },
      activeFile: "/repo-a/src/main.ts",
    });
    mgr1.deactivate(repoB, {
      activeView: "changes",
      scrollPositions: { changes: 200 },
      activeFile: "/repo-b/src/lib.ts",
    });
    mgr1.flush();

    // Cold start.
    const mgr2 = new SessionManager();

    const restoredA = mgr2.activate(repoA);
    expect(restoredA).not.toBeNull();
    expect(restoredA!.activeView).toBe("timeline");
    expect(restoredA!.scrollPositions.timeline).toBe(500);
    expect(restoredA!.activeFile).toBe("/repo-a/src/main.ts");

    const restoredB = mgr2.activate(repoB);
    expect(restoredB).not.toBeNull();
    expect(restoredB!.activeView).toBe("changes");
    expect(restoredB!.scrollPositions.changes).toBe(200);
    expect(restoredB!.activeFile).toBe("/repo-b/src/lib.ts");
  });

  it("debounced flush persists latest state for cold restore", () => {
    const repoA = "/repos/repo-a";

    const mgr1 = new SessionManager();
    mgr1.deactivate(repoA, {
      activeView: "explorer",
      scrollPositions: { explorer: 0 },
    });

    // Simulate rapid updates (scroll events).
    mgr1.update(repoA, { scrollPositions: { explorer: 100 } });
    mgr1.update(repoA, { scrollPositions: { explorer: 200 } });
    mgr1.update(repoA, { scrollPositions: { explorer: 300 } });
    mgr1.update(repoA, { activeView: "timeline" });
    mgr1.update(repoA, { scrollPositions: { timeline: 500, explorer: 300 } });

    // Advance past debounce period to trigger flush.
    vi.advanceTimersByTime(250);

    // Cold start — should have the final state.
    const mgr2 = new SessionManager();
    const restored = mgr2.activate(repoA);

    expect(restored).not.toBeNull();
    expect(restored!.activeView).toBe("timeline");
    expect(restored!.scrollPositions.timeline).toBe(500);
  });

  it("flush on beforeunload ensures no data loss", () => {
    const repoA = "/repos/repo-a";

    const mgr = new SessionManager();
    mgr.deactivate(repoA, {
      activeView: "timeline",
      scrollPositions: { timeline: 500 },
      activeFile: "/repo-a/src/main.ts",
    });

    // Simulate beforeunload: explicit flush before "quit".
    mgr.flush();

    // Verify localStorage has the data.
    const keys = [...mockStorage._store.keys()].filter((k) =>
      k.startsWith("crab.session.v1."),
    );
    expect(keys.length).toBe(1);

    const stored = JSON.parse(mockStorage._store.get(keys[0]!)!);
    expect(stored.activeView).toBe("timeline");
    expect(stored.scrollPositions.timeline).toBe(500);
    expect(stored.activeFile).toBe("/repo-a/src/main.ts");
  });
});
