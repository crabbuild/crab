/**
 * Integration tests for capability-based feature gating.
 *
 * Validates:
 * - LocalProvider returns full capabilities (all true except canTrash on Linux)
 * - RemoteProvider returns capabilities based on CapabilityNegotiator
 * - Capabilities downgrade on connection loss and upgrade on reconnection
 * - The `capabilities-changed` event is emitted on connection state transitions
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { EventEmitter } from "node:events";
import { RemoteProvider } from "../electron/remote-provider";
import { fullCapabilities, noCapabilities, type CapabilitySet } from "../electron/repo-capabilities";

// ── Mock factories (same patterns as remote-provider-polling.spec.ts) ──

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

function makeMockCapabilityNegotiator(options?: {
  fileWatching?: boolean;
  search?: boolean;
  buildRun?: boolean;
  hasCached?: boolean;
}) {
  const opts = {
    fileWatching: false,
    search: false,
    buildRun: false,
    hasCached: true,
    ...options,
  };

  return {
    isSupported: vi.fn().mockImplementation((_profileId: string, feature: string) => {
      if (feature === "watch_start") return opts.fileWatching;
      if (feature === "git_grep") return opts.search;
      if (feature === "exec_command") return opts.buildRun;
      return false;
    }),
    getCached: vi.fn().mockReturnValue(
      opts.hasCached
        ? {
            protocol: 1,
            version: "2.0.0-alpha.1",
            methods: [
              "capabilities",
              ...(opts.fileWatching ? ["watch_start", "watch_stop"] : []),
              ...(opts.search ? ["git_grep"] : []),
              ...(opts.buildRun ? ["exec_command"] : []),
            ],
          }
        : null,
    ),
  };
}

function makeMockSessionManager(status = "connected") {
  const emitter = new EventEmitter();
  return Object.assign(emitter, {
    getStatus: vi.fn().mockReturnValue(status),
    getConnection: vi.fn().mockReturnValue(null),
    agentSession: vi.fn().mockReturnValue("ssh:1"),
  });
}

// ── LocalProvider capabilities tests ─────────────────────────────

describe("LocalProvider capabilities", () => {
  it("returns all capabilities as true on non-Linux platforms", async () => {
    // LocalProvider.capabilities() returns fullCapabilities() with
    // canTrash adjusted for platform. We test the fullCapabilities()
    // helper directly since LocalProvider requires Electron's shell module.
    const caps = fullCapabilities();

    expect(caps.canEdit).toBe(true);
    expect(caps.canWatch).toBe(true);
    expect(caps.canStageHunks).toBe(true);
    expect(caps.canWorktree).toBe(true);
    expect(caps.canForge).toBe(true);
    expect(caps.canHydrate).toBe(true);
    expect(caps.canSearch).toBe(true);
    expect(caps.canExec).toBe(true);
    expect(caps.canTrash).toBe(true);
  });

  it("fullCapabilities has every flag set to true", () => {
    const caps = fullCapabilities();
    const values = Object.values(caps);
    expect(values.every((v) => v === true)).toBe(true);
  });

  it("noCapabilities has every flag set to false", () => {
    const caps = noCapabilities();
    const values = Object.values(caps);
    expect(values.every((v) => v === false)).toBe(true);
  });
});

// ── RemoteProvider capabilities tests ────────────────────────────

describe("RemoteProvider capabilities", () => {
  let provider: RemoteProvider;
  let mockRouter: ReturnType<typeof makeMockRouter>;
  let mockSessionManager: ReturnType<typeof makeMockSessionManager>;
  let mockCapNegotiator: ReturnType<typeof makeMockCapabilityNegotiator>;

  const REPO_ID = "ssh://profile-1/home/user/project";

  beforeEach(() => {
    vi.useFakeTimers();
    mockRouter = makeMockRouter();
    mockSessionManager = makeMockSessionManager("connected");
    mockCapNegotiator = makeMockCapabilityNegotiator({
      fileWatching: true,
      search: true,
      buildRun: false,
    });

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

  it("returns capabilities based on CapabilityNegotiator when connected", async () => {
    const caps = await provider.capabilities(REPO_ID);

    expect(caps.canEdit).toBe(true);
    expect(caps.canWatch).toBe(true); // fileWatching supported
    expect(caps.canStageHunks).toBe(true);
    expect(caps.canWorktree).toBe(true);
    expect(caps.canForge).toBe(true);
    expect(caps.canHydrate).toBe(false); // remote repos don't support hydrate
    expect(caps.canSearch).toBe(true); // search supported
    expect(caps.canExec).toBe(false); // buildRun not supported
    expect(caps.canTrash).toBe(false); // remote has no trash
  });

  it("returns noCapabilities when disconnected", async () => {
    mockSessionManager.getStatus.mockReturnValue("disconnected");

    const caps = await provider.capabilities(REPO_ID);

    expect(caps.canEdit).toBe(false);
    expect(caps.canWatch).toBe(false);
    expect(caps.canStageHunks).toBe(false);
    expect(caps.canWorktree).toBe(false);
    expect(caps.canForge).toBe(false);
    expect(caps.canHydrate).toBe(false);
    expect(caps.canSearch).toBe(false);
    expect(caps.canExec).toBe(false);
    expect(caps.canTrash).toBe(false);
  });

  it("returns noCapabilities when reconnecting", async () => {
    mockSessionManager.getStatus.mockReturnValue("reconnecting");

    const caps = await provider.capabilities(REPO_ID);

    expect(caps.canEdit).toBe(false);
    expect(caps.canWatch).toBe(false);
    expect(caps.canStageHunks).toBe(false);
    expect(caps.canWorktree).toBe(false);
    expect(caps.canForge).toBe(false);
    expect(caps.canHydrate).toBe(false);
    expect(caps.canSearch).toBe(false);
    expect(caps.canExec).toBe(false);
    expect(caps.canTrash).toBe(false);
  });

  it("returns noCapabilities when in error state", async () => {
    mockSessionManager.getStatus.mockReturnValue("error");

    const caps = await provider.capabilities(REPO_ID);

    expect(caps.canEdit).toBe(false);
    expect(caps.canWatch).toBe(false);
    expect(caps.canStageHunks).toBe(false);
    expect(caps.canWorktree).toBe(false);
    expect(caps.canForge).toBe(false);
    expect(caps.canHydrate).toBe(false);
    expect(caps.canSearch).toBe(false);
    expect(caps.canExec).toBe(false);
    expect(caps.canTrash).toBe(false);
  });

  it("returns no capabilities without a negotiated native agent", async () => {
    mockCapNegotiator.getCached.mockReturnValue(null);
    mockSessionManager.agentSession.mockReturnValue(null);

    const caps = await provider.capabilities(REPO_ID);

    expect(caps).toEqual(noCapabilities());
  });

  it("revokes watched repository capabilities when the native agent exits", async () => {
    await provider.watchStart(REPO_ID);
    const changes: unknown[] = [];
    provider.on("capabilities-changed", event => changes.push(event));
    mockCapNegotiator.getCached.mockReturnValue(null);
    mockSessionManager.agentSession.mockReturnValue(null);
    mockSessionManager.emit("ssh:agent-unavailable", { profileId: "profile-1" });
    await Promise.resolve();
    expect(changes).toEqual([{ type: "capabilities-changed", repoId: REPO_ID, capabilities: noCapabilities(), nativeSessionId: null, error: "Desktop services are unavailable." }]);
  });

  it("returns noCapabilities for non-remote repo_id", async () => {
    const caps = await provider.capabilities("/local/path/to/repo");

    expect(caps).toEqual(noCapabilities());
  });
});

// ── Capability transitions on connection state changes ───────────

describe("RemoteProvider capability transitions", () => {
  let provider: RemoteProvider;
  let mockRouter: ReturnType<typeof makeMockRouter>;
  let mockSessionManager: ReturnType<typeof makeMockSessionManager>;
  let mockCapNegotiator: ReturnType<typeof makeMockCapabilityNegotiator>;

  const REPO_ID = "ssh://profile-1/home/user/project";

  beforeEach(() => {
    vi.useFakeTimers();
    mockRouter = makeMockRouter();
    mockSessionManager = makeMockSessionManager("connected");
    mockCapNegotiator = makeMockCapabilityNegotiator({
      fileWatching: true,
      search: true,
      buildRun: true,
    });

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

  it("capabilities downgrade when connection transitions to disconnected", async () => {
    // Start watching so the repo is tracked
    await provider.watchStart(REPO_ID);

    // Verify capabilities are full when connected
    const connectedCaps = await provider.capabilities(REPO_ID);
    expect(connectedCaps.canEdit).toBe(true);
    expect(connectedCaps.canWatch).toBe(true);

    // Simulate connection loss
    mockSessionManager.getStatus.mockReturnValue("disconnected");

    const capEvents: { repoId: string; capabilities: CapabilitySet }[] = [];
    provider.on("capabilities-changed", (e) => capEvents.push(e));

    mockSessionManager.emit("ssh:status-changed", {
      profileId: "profile-1",
      status: "disconnected",
    });

    // Allow async emitCapabilityChanges to complete
    await vi.advanceTimersByTimeAsync(0);

    // Should have emitted a capabilities-changed event
    expect(capEvents).toHaveLength(1);
    expect(capEvents[0].repoId).toBe(REPO_ID);
    expect(capEvents[0].capabilities.canEdit).toBe(false);
    expect(capEvents[0].capabilities.canWatch).toBe(false);
    expect(capEvents[0].capabilities.canStageHunks).toBe(false);
    expect(capEvents[0].capabilities.canExec).toBe(false);

    // Direct query also returns downgraded capabilities
    const disconnectedCaps = await provider.capabilities(REPO_ID);
    expect(disconnectedCaps).toEqual(noCapabilities());
  });

  it("capabilities upgrade when connection transitions to connected", async () => {
    // Start watching so the repo is tracked
    await provider.watchStart(REPO_ID);

    // Simulate disconnection first
    mockSessionManager.getStatus.mockReturnValue("disconnected");
    mockSessionManager.emit("ssh:status-changed", {
      profileId: "profile-1",
      status: "disconnected",
    });
    await vi.advanceTimersByTimeAsync(0);

    // Verify downgraded
    const disconnectedCaps = await provider.capabilities(REPO_ID);
    expect(disconnectedCaps.canEdit).toBe(false);

    // Now simulate reconnection
    mockSessionManager.getStatus.mockReturnValue("connected");

    const capEvents: { repoId: string; capabilities: CapabilitySet }[] = [];
    provider.on("capabilities-changed", (e) => capEvents.push(e));

    mockSessionManager.emit("ssh:status-changed", {
      profileId: "profile-1",
      status: "connected",
    });
    await vi.advanceTimersByTimeAsync(0);

    // Should have emitted a capabilities-changed event with upgraded caps
    expect(capEvents).toHaveLength(1);
    expect(capEvents[0].repoId).toBe(REPO_ID);
    expect(capEvents[0].capabilities.canEdit).toBe(true);
    expect(capEvents[0].capabilities.canWatch).toBe(true);
    expect(capEvents[0].capabilities.canStageHunks).toBe(true);
    expect(capEvents[0].capabilities.canExec).toBe(true);

    // Direct query also returns upgraded capabilities
    const connectedCaps = await provider.capabilities(REPO_ID);
    expect(connectedCaps.canEdit).toBe(true);
    expect(connectedCaps.canWatch).toBe(true);
  });

  it("emits capabilities-changed event on reconnecting state", async () => {
    await provider.watchStart(REPO_ID);

    mockSessionManager.getStatus.mockReturnValue("reconnecting");

    const capEvents: { repoId: string; capabilities: CapabilitySet }[] = [];
    provider.on("capabilities-changed", (e) => capEvents.push(e));

    mockSessionManager.emit("ssh:status-changed", {
      profileId: "profile-1",
      status: "reconnecting",
    });
    await vi.advanceTimersByTimeAsync(0);

    expect(capEvents).toHaveLength(1);
    expect(capEvents[0].repoId).toBe(REPO_ID);
    // Reconnecting state returns all false (no operations available)
    expect(capEvents[0].capabilities.canEdit).toBe(false);
    expect(capEvents[0].capabilities.canWatch).toBe(false);
  });

  it("does not emit capabilities-changed for repos on a different profile", async () => {
    await provider.watchStart(REPO_ID);

    const capEvents: { repoId: string; capabilities: CapabilitySet }[] = [];
    provider.on("capabilities-changed", (e) => capEvents.push(e));

    // Emit status change for a different profile
    mockSessionManager.emit("ssh:status-changed", {
      profileId: "profile-2",
      status: "disconnected",
    });
    await vi.advanceTimersByTimeAsync(0);

    // No event should be emitted for profile-1's repo
    expect(capEvents).toHaveLength(0);
  });

  it("emits capabilities-changed for multiple repos on the same profile", async () => {
    const REPO_ID_2 = "ssh://profile-1/home/user/other-project";

    await provider.watchStart(REPO_ID);
    await provider.watchStart(REPO_ID_2);

    mockSessionManager.getStatus.mockReturnValue("disconnected");

    const capEvents: { repoId: string; capabilities: CapabilitySet }[] = [];
    provider.on("capabilities-changed", (e) => capEvents.push(e));

    mockSessionManager.emit("ssh:status-changed", {
      profileId: "profile-1",
      status: "disconnected",
    });
    await vi.advanceTimersByTimeAsync(0);

    // Should emit for both repos
    expect(capEvents).toHaveLength(2);
    const repoIds = capEvents.map((e) => e.repoId).sort();
    expect(repoIds).toEqual([REPO_ID, REPO_ID_2].sort());
  });

  it("full cycle: connected → disconnected → connected preserves correct capabilities", async () => {
    await provider.watchStart(REPO_ID);

    // Phase 1: Connected — full capabilities
    const phase1 = await provider.capabilities(REPO_ID);
    expect(phase1.canEdit).toBe(true);
    expect(phase1.canWatch).toBe(true);
    expect(phase1.canExec).toBe(true);

    // Phase 2: Disconnect
    mockSessionManager.getStatus.mockReturnValue("disconnected");
    mockSessionManager.emit("ssh:status-changed", {
      profileId: "profile-1",
      status: "disconnected",
    });
    await vi.advanceTimersByTimeAsync(0);

    const phase2 = await provider.capabilities(REPO_ID);
    expect(phase2.canEdit).toBe(false);
    expect(phase2.canWatch).toBe(false);
    expect(phase2.canExec).toBe(false);

    // Phase 3: Reconnect
    mockSessionManager.getStatus.mockReturnValue("connected");
    mockSessionManager.emit("ssh:status-changed", {
      profileId: "profile-1",
      status: "connected",
    });
    await vi.advanceTimersByTimeAsync(0);

    const phase3 = await provider.capabilities(REPO_ID);
    expect(phase3.canEdit).toBe(true);
    expect(phase3.canWatch).toBe(true);
    expect(phase3.canExec).toBe(true);
  });
});
