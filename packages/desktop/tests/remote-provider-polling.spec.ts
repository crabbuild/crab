/**
 * Unit tests for RemoteProvider polling fallback.
 *
 * When the remote agent does not support native file watching
 * (fileWatching capability is false), the RemoteProvider falls back
 * to polling git_status_porcelain every 30 seconds and emitting
 * synthetic fs/changed events when the output changes.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { EventEmitter } from "node:events";
import { withRepositoryRuntimes } from "./repository-runtime-fixture";
import { RemoteProvider } from "../electron/remote-provider";

// ── Mock factories ───────────────────────────────────────────────

function makeMockRouter() {
  const emitter = new EventEmitter();
  return Object.assign(emitter, {
    route: vi.fn().mockResolvedValue(""),
    sendToRemoteAgent: vi.fn().mockResolvedValue({}),
  });
}

function makeMockFileEditor() {
  return {
    createFile: vi.fn().mockResolvedValue({}),
    renameFile: vi.fn().mockResolvedValue({}),
    deleteFile: vi.fn().mockResolvedValue({}),
    writeFile: vi.fn().mockResolvedValue({ success: true }),
    readFile: vi.fn().mockResolvedValue({ content: "" }),
  };
}

function makeMockGitManager() {
  return {};
}

function makeMockCapabilityNegotiator(supportsFileWatching = false) {
  return {
    isSupported: vi.fn().mockImplementation((_profileId: string, feature: string) => {
      if (feature === "watch_start") return supportsFileWatching;
      return false;
    }),
    getCached: vi.fn().mockReturnValue({
      protocol: 1,
      version: "2.0.0-alpha.1",
      methods: supportsFileWatching ? ["capabilities", "watch_start", "watch_stop"] : ["capabilities"],
    }),
  };
}

function makeMockSessionManager(status = "connected") {
  const emitter = new EventEmitter();
  return withRepositoryRuntimes(Object.assign(emitter, { repositoryWork: () => undefined,
    getStatus: vi.fn().mockReturnValue(status),
    getConnection: vi.fn().mockReturnValue(null),
    agentSession: vi.fn().mockReturnValue("ssh:1"),
  }));
}

// ── Tests ────────────────────────────────────────────────────────

describe("RemoteProvider polling fallback", () => {
  let provider: RemoteProvider;
  let mockRouter: ReturnType<typeof makeMockRouter>;
  let mockSessionManager: ReturnType<typeof makeMockSessionManager>;
  let mockCapNegotiator: ReturnType<typeof makeMockCapabilityNegotiator>;

  const REPO_ID = "ssh://profile-1/home/user/project";

  beforeEach(() => {
    vi.useFakeTimers();
    mockRouter = makeMockRouter();
    mockSessionManager = makeMockSessionManager();
    mockCapNegotiator = makeMockCapabilityNegotiator(false);

    provider = new RemoteProvider({
      router: mockRouter as any,
      fileEditor: makeMockFileEditor() as any,
      gitManager: makeMockGitManager() as any,
      capabilityNegotiator: mockCapNegotiator as any,
      sessionManager: mockSessionManager as any,
    });
  });

  afterEach(() => {
    provider.dispose();
    vi.useRealTimers();
  });

  it("reads complete text through its admitted repository with a relative path and limit", async () => {
    mockRouter.route.mockResolvedValue({ truncated: false, text: "🦀 café" });
    expect(await provider.fsRead(REPO_ID, "/home/user/project/settings.json", 64 * 1024)).toEqual({ exists: true, content: "🦀 café" });
    expect(mockRouter.route).toHaveBeenCalledWith(REPO_ID, "read_text", {
      repo_id: REPO_ID, path: "settings.json", max_bytes: 64 * 1024, native_session_id: "ssh:1",
    });
  });

  it("only typed path absence is a missing document, not repository or transport failure", async () => {
    const missing = Object.assign(new Error("Fixture file missing"), { code: 1004, data: { kind: "path_not_found" } });
    mockRouter.route.mockRejectedValueOnce(missing);
    expect(await provider.fsRead(REPO_ID, "/home/user/project/settings.json")).toEqual({ exists: false, content: null });
    const unavailable = Object.assign(new Error("Fixture repo unavailable"), { code: 1004 });
    mockRouter.route.mockRejectedValueOnce(unavailable);
    await expect(provider.fsRead(REPO_ID, "/home/user/project/settings.json")).rejects.toBe(unavailable);
  });

  it("truncated remote text cannot become mutable document content", async () => {
    mockRouter.route.mockResolvedValue({ truncated: true, text: null, reason: "Fixture limit exceeded" });
    await expect(provider.fsRead(REPO_ID, "/home/user/project/settings.json", 10)).rejects.toThrow("Fixture limit exceeded");
  });

  it("reconnection cannot return stale mutable content from a previous write", async () => {
    mockRouter.route.mockResolvedValue({ written: true, size: 4 });
    await provider.fsWrite(REPO_ID, "/home/user/project/settings.json", "text");
    mockSessionManager.getStatus.mockReturnValue("reconnecting");
    mockRouter.route.mockClear();
    await expect(provider.fsRead(REPO_ID, "/home/user/project/settings.json")).rejects.toMatchObject({ code: "SSH_RECONNECTING" });
    expect(mockRouter.route).not.toHaveBeenCalled();
  });

  it("writes through the admitted repository session with a relative path and UTF-8 acknowledgement", async () => {
    mockRouter.route.mockResolvedValue({ written: true, size: Buffer.byteLength("🦀 café", "utf8") });
    const result = await provider.fsWrite(REPO_ID, "/home/user/project/note.txt", "🦀 café");
    expect(mockRouter.route).toHaveBeenCalledWith(REPO_ID, "write_text", {
      repo_id: REPO_ID, path: "note.txt", content: "🦀 café", native_session_id: "ssh:1",
    });
    expect(result).toEqual({ path: "/home/user/project/note.txt", written: 10 });
  });

  it("rejects writes outside the repository before dispatch", async () => {
    await expect(provider.fsWrite(REPO_ID, "/home/user/project-other/note.txt", "text")).rejects.toThrow("inside the selected repository");
    expect(mockRouter.route).not.toHaveBeenCalled();
  });

  it("does not publish a successful file change for a partial remote acknowledgement", async () => {
    const changed = vi.fn(); provider.on("fs/changed", changed);
    mockRouter.route.mockResolvedValue({ written: true, size: 1 });
    await expect(provider.fsWrite(REPO_ID, "/home/user/project/note.txt", "text")).rejects.toThrow("complete buffer");
    expect(changed).not.toHaveBeenCalled();
  });

  it("starts polling when agent does not support fileWatching", async () => {
    await provider.watchStart(REPO_ID);

    // Should NOT have called watch_start RPC
    expect(mockRouter.route).not.toHaveBeenCalledWith(
      REPO_ID,
      "watch_start",
      expect.anything(),
    );

    // Advance 30s — should poll git_status_porcelain
    mockRouter.route.mockResolvedValue("M src/main.ts\n");
    await vi.advanceTimersByTimeAsync(30_000);

    expect(mockRouter.route).toHaveBeenCalledWith(
      REPO_ID,
      "git_status_porcelain",
      { repo_id: REPO_ID },
    );
  });

  it("uses native watch_start when agent supports fileWatching", async () => {
    mockCapNegotiator.isSupported.mockImplementation((_: string, feature: string) => {
      return feature === "watch_start";
    });

    await provider.watchStart(REPO_ID);

    expect(mockRouter.route).toHaveBeenCalledWith(
      REPO_ID,
      "watch_start",
      { repo_id: REPO_ID },
    );
  });

  it("emits fs/changed when polled status changes", async () => {
    await provider.watchStart(REPO_ID);

    const events: any[] = [];
    provider.on("fs/changed", (e) => events.push(e));

    // First poll — establishes baseline
    mockRouter.route.mockResolvedValue("M src/main.ts\n");
    await vi.advanceTimersByTimeAsync(30_000);

    expect(events).toHaveLength(0);

    // Second poll — same output, no event
    await vi.advanceTimersByTimeAsync(30_000);
    expect(events).toHaveLength(0);

    // Third poll — different output, should emit
    mockRouter.route.mockResolvedValue("M src/main.ts\nA src/new.ts\n");
    await vi.advanceTimersByTimeAsync(30_000);

    expect(events).toHaveLength(1);
    expect(events[0].type).toBe("fs/changed");
    expect(events[0].repoPath).toBe(REPO_ID);
  });

  it("does not emit fs/changed on first poll (no baseline to compare)", async () => {
    await provider.watchStart(REPO_ID);

    const events: any[] = [];
    provider.on("fs/changed", (e) => events.push(e));

    mockRouter.route.mockResolvedValue("M src/main.ts\n");
    await vi.advanceTimersByTimeAsync(30_000);

    expect(events).toHaveLength(0);
  });

  it("stops polling on watchStop", async () => {
    await provider.watchStart(REPO_ID);

    mockRouter.route.mockResolvedValue("M src/main.ts\n");
    await vi.advanceTimersByTimeAsync(30_000);

    // Stop watching
    mockRouter.route.mockClear();
    await provider.watchStop(REPO_ID);

    // Advance another 30s — should NOT poll
    await vi.advanceTimersByTimeAsync(30_000);
    expect(mockRouter.route).not.toHaveBeenCalledWith(
      REPO_ID,
      "git_status_porcelain",
      expect.anything(),
    );
  });

  it("stops polling on dispose", async () => {
    await provider.watchStart(REPO_ID);

    mockRouter.route.mockResolvedValue("M src/main.ts\n");
    await vi.advanceTimersByTimeAsync(30_000);

    mockRouter.route.mockClear();
    provider.dispose();

    // Advance another 30s — should NOT poll
    await vi.advanceTimersByTimeAsync(30_000);
    expect(mockRouter.route).not.toHaveBeenCalled();
  });

  it("stops polling when connection is lost", async () => {
    await provider.watchStart(REPO_ID);

    // First poll succeeds
    mockRouter.route.mockResolvedValue("M src/main.ts\n");
    await vi.advanceTimersByTimeAsync(30_000);

    // Simulate connection loss via status change event
    mockSessionManager.getStatus.mockReturnValue("disconnected");
    mockSessionManager.emit("ssh:status-changed", {
      profileId: "profile-1",
      status: "disconnected",
    });

    mockRouter.route.mockClear();

    // After stopping, no more polls should happen
    await vi.advanceTimersByTimeAsync(30_000);
    expect(mockRouter.route).not.toHaveBeenCalled();
  });

  it("handles poll RPC failure gracefully without crashing", async () => {
    await provider.watchStart(REPO_ID);

    const events: any[] = [];
    provider.on("fs/changed", (e) => events.push(e));

    // First poll fails
    mockRouter.route.mockRejectedValue(new Error("timeout"));
    await vi.advanceTimersByTimeAsync(30_000);

    // No event emitted, no crash
    expect(events).toHaveLength(0);

    // Next poll succeeds — establishes baseline
    mockRouter.route.mockResolvedValue("M src/main.ts\n");
    await vi.advanceTimersByTimeAsync(30_000);
    expect(events).toHaveLength(0);

    // Status changes — should emit
    mockRouter.route.mockResolvedValue("M src/main.ts\nA new.ts\n");
    await vi.advanceTimersByTimeAsync(30_000);
    expect(events).toHaveLength(1);
  });

  it("does not start polling when connection is not active", async () => {
    mockSessionManager.getStatus.mockReturnValue("disconnected");

    await provider.watchStart(REPO_ID);

    mockRouter.route.mockClear();
    await vi.advanceTimersByTimeAsync(30_000);

    expect(mockRouter.route).not.toHaveBeenCalled();
  });
});
