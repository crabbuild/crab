/**
 * Workspace session restore on restart.
 *
 * Exercises the WorkspaceSessionManager directly — simulating the
 * save/flush/reload cycle that occurs when the app restarts. Validates
 * that workspace-level state (active repo, tab bar order, terminal panel,
 * sidebar width) survives a full restart.
 *
 * Validates: Requirements 5.4
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { WorkspaceSessionManager } from "../src/lib/workspace-session-manager";

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

describe("Workspace session restore on restart", () => {
  it("restores workspace with active repo, terminal panel, and tab bar order after restart", () => {
    const workspaceRoot = "/Users/dev/my-workspace";

    // Step 1: Create a WorkspaceSessionManager and save workspace state.
    const mgr1 = new WorkspaceSessionManager();
    mgr1.save(workspaceRoot, {
      activeRepoRelativePath: "repo-b",
      tabBarOrder: ["repo-a", "repo-b", "repo-c"],
      terminalPanelOpen: true,
      sidebarWidth: 280,
    });

    // Step 2: Flush to localStorage (simulates before-quit).
    mgr1.flush();

    // Step 3: Create a NEW WorkspaceSessionManager (simulates app restart).
    const mgr2 = new WorkspaceSessionManager();

    // Step 4: Restore the workspace session.
    const restored = mgr2.restore(workspaceRoot);

    // Step 5: Assert all fields are preserved.
    expect(restored).not.toBeNull();
    expect(restored!.workspaceRoot).toBe(workspaceRoot);
    expect(restored!.activeRepoRelativePath).toBe("repo-b");
    expect(restored!.tabBarOrder).toEqual(["repo-a", "repo-b", "repo-c"]);
    expect(restored!.terminalPanelOpen).toBe(true);
    expect(restored!.sidebarWidth).toBe(280);
    expect(restored!.savedAt).toBeTypeOf("number");
  });

  it("preserves the last-saved state when multiple saves occur before flush", () => {
    const workspaceRoot = "/Users/dev/multi-save";

    const mgr1 = new WorkspaceSessionManager();

    // User opens workspace, activates repo-a first.
    mgr1.save(workspaceRoot, {
      activeRepoRelativePath: "repo-a",
      tabBarOrder: ["repo-a", "repo-b", "repo-c"],
      terminalPanelOpen: false,
      sidebarWidth: 260,
    });

    // User switches to repo-b and opens terminal panel.
    mgr1.save(workspaceRoot, {
      activeRepoRelativePath: "repo-b",
      terminalPanelOpen: true,
      sidebarWidth: 280,
    });

    // Flush on quit.
    mgr1.flush();

    // Restart.
    const mgr2 = new WorkspaceSessionManager();
    const restored = mgr2.restore(workspaceRoot);

    expect(restored).not.toBeNull();
    expect(restored!.activeRepoRelativePath).toBe("repo-b");
    expect(restored!.tabBarOrder).toEqual(["repo-a", "repo-b", "repo-c"]);
    expect(restored!.terminalPanelOpen).toBe(true);
    expect(restored!.sidebarWidth).toBe(280);
  });

  it("handles restart with terminal panel closed", () => {
    const workspaceRoot = "/Users/dev/no-terminal";

    const mgr1 = new WorkspaceSessionManager();
    mgr1.save(workspaceRoot, {
      activeRepoRelativePath: "repo-c",
      tabBarOrder: ["repo-a", "repo-b", "repo-c"],
      terminalPanelOpen: false,
      sidebarWidth: 320,
    });
    mgr1.flush();

    const mgr2 = new WorkspaceSessionManager();
    const restored = mgr2.restore(workspaceRoot);

    expect(restored).not.toBeNull();
    expect(restored!.activeRepoRelativePath).toBe("repo-c");
    expect(restored!.terminalPanelOpen).toBe(false);
    expect(restored!.sidebarWidth).toBe(320);
  });

  it("returns null for a workspace that was never saved", () => {
    const mgr = new WorkspaceSessionManager();
    const result = mgr.restore("/Users/dev/never-opened");
    expect(result).toBeNull();
  });

  it("survives debounced flush timing on restart", () => {
    const workspaceRoot = "/Users/dev/debounce-test";

    const mgr1 = new WorkspaceSessionManager();
    mgr1.save(workspaceRoot, {
      activeRepoRelativePath: "repo-b",
      tabBarOrder: ["repo-a", "repo-b", "repo-c"],
      terminalPanelOpen: true,
      sidebarWidth: 280,
    });

    // Let the debounced flush fire naturally.
    vi.advanceTimersByTime(250);

    // Restart without explicit flush — debounce should have persisted.
    const mgr2 = new WorkspaceSessionManager();
    const restored = mgr2.restore(workspaceRoot);

    expect(restored).not.toBeNull();
    expect(restored!.activeRepoRelativePath).toBe("repo-b");
    expect(restored!.tabBarOrder).toEqual(["repo-a", "repo-b", "repo-c"]);
    expect(restored!.terminalPanelOpen).toBe(true);
    expect(restored!.sidebarWidth).toBe(280);
  });
});
