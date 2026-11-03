/**
 * Graceful degradation test for SessionManager.
 *
 * Validates that when localStorage is unavailable (quota exceeded),
 * the app functions normally — repo switching works, no crashes,
 * and a warning is logged.
 *
 * **Validates: Requirements 7.5** (design property 3)
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { SessionManager } from "../src/lib/session-manager";

// ---------------------------------------------------------------------------
// localStorage mock that always throws on setItem (simulates quota exceeded)
// ---------------------------------------------------------------------------

function createBrokenLocalStorageMock() {
  const store = new Map<string, string>();
  return {
    getItem: vi.fn((key: string) => store.get(key) ?? null),
    setItem: vi.fn((_key: string, _value: string) => {
      throw new DOMException(
        "Failed to execute 'setItem' on 'Storage': Setting the value exceeded the quota.",
        "QuotaExceededError",
      );
    }),
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

let mockStorage: ReturnType<typeof createBrokenLocalStorageMock>;

beforeEach(() => {
  vi.useFakeTimers();
  mockStorage = createBrokenLocalStorageMock();
  Object.defineProperty(globalThis, "localStorage", { value: mockStorage, writable: true });
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("SessionManager graceful degradation (localStorage quota exceeded)", () => {
  it("deactivate() does not throw when localStorage.setItem fails", () => {
    const mgr = new SessionManager();

    expect(() => {
      mgr.deactivate("/repos/frontend", {
        activeView: "timeline",
        activeFile: "/repos/frontend/src/app.ts",
        navHistory: [{ view: "explorer" }],
        navIndex: 0,
        scrollPositions: { timeline: 200 },
      });
    }).not.toThrow();
  });

  it("activate() returns from in-memory cache even when localStorage writes fail", () => {
    const mgr = new SessionManager();

    // Deactivate stores in memory cache regardless of localStorage failures.
    mgr.deactivate("/repos/frontend", {
      activeView: "changes",
      scrollPositions: { changes: 150 },
    });

    // Activate should return the cached session.
    const session = mgr.activate("/repos/frontend");
    expect(session).not.toBeNull();
    expect(session!.activeView).toBe("changes");
    expect(session!.scrollPositions).toEqual({ changes: 150 });
  });

  it("update() does not throw when localStorage.setItem fails", () => {
    const mgr = new SessionManager();

    mgr.deactivate("/repos/frontend", { activeView: "explorer" });

    expect(() => {
      mgr.update("/repos/frontend", { activeView: "timeline" });
    }).not.toThrow();

    // The in-memory state should still reflect the update.
    const session = mgr.activate("/repos/frontend");
    expect(session!.activeView).toBe("timeline");
  });

  it("flush() does not throw when localStorage.setItem fails", () => {
    const mgr = new SessionManager();

    mgr.deactivate("/repos/frontend", { activeView: "branches" });

    expect(() => {
      mgr.flush();
    }).not.toThrow();
  });

  it("logs a warning when localStorage write fails after eviction retry", () => {
    const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
    const mgr = new SessionManager();

    mgr.deactivate("/repos/frontend", { activeView: "timeline" });
    mgr.flush();

    expect(warnSpy).toHaveBeenCalledWith(
      expect.stringContaining("[SessionManager]"),
      expect.anything(),
    );

    warnSpy.mockRestore();
  });

  it("repo switching works normally — deactivate old, activate new from cache", () => {
    const mgr = new SessionManager();

    // Simulate working with repo A.
    mgr.deactivate("/repos/repo-a", {
      activeView: "timeline",
      scrollPositions: { timeline: 500 },
      navHistory: [{ view: "explorer" }, { view: "timeline" }],
      navIndex: 1,
    });

    // Switch to repo B.
    mgr.deactivate("/repos/repo-b", {
      activeView: "changes",
      scrollPositions: { changes: 100 },
    });

    // Switch back to repo A — should restore from in-memory cache.
    const sessionA = mgr.activate("/repos/repo-a");
    expect(sessionA).not.toBeNull();
    expect(sessionA!.activeView).toBe("timeline");
    expect(sessionA!.scrollPositions).toEqual({ timeline: 500 });
    expect(sessionA!.navHistory).toEqual([{ view: "explorer" }, { view: "timeline" }]);
    expect(sessionA!.navIndex).toBe(1);

    // Switch back to repo B.
    const sessionB = mgr.activate("/repos/repo-b");
    expect(sessionB).not.toBeNull();
    expect(sessionB!.activeView).toBe("changes");
  });

  it("debounced flush does not crash when triggered automatically", () => {
    const mgr = new SessionManager();

    mgr.deactivate("/repos/frontend", { activeView: "explorer" });

    // Multiple rapid updates trigger the debounced flush.
    mgr.update("/repos/frontend", { activeView: "timeline" });
    mgr.update("/repos/frontend", { activeView: "changes" });
    mgr.update("/repos/frontend", { activeView: "branches" });

    // Advance past debounce period — flush fires automatically.
    expect(() => {
      vi.advanceTimersByTime(300);
    }).not.toThrow();

    // In-memory state should still be correct.
    const session = mgr.activate("/repos/frontend");
    expect(session!.activeView).toBe("branches");
  });

  it("clear() does not throw when localStorage is broken", () => {
    const mgr = new SessionManager();

    mgr.deactivate("/repos/frontend", { activeView: "explorer" });

    expect(() => {
      mgr.clear();
    }).not.toThrow();

    // After clear, activate should return null (cache was cleared).
    const session = mgr.activate("/repos/frontend");
    expect(session).toBeNull();
  });
});
