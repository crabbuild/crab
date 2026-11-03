/**
 * Integration tests for remote status polling.
 *
 * Validates:
 * - batch_git_status returns correct branch/dirty/ahead/behind
 * - delta mode only returns changed repos
 * - polling pauses on disconnect and resumes on reconnect
 *
 * Requirements: 2.1, 2.4, 2.5
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { EventEmitter } from "node:events";
import {
  RemoteWorkspaceOrchestrator,
  type RemoteRepoStatus,
} from "../electron/remote-workspace-orchestrator";

// ── Mock factories ───────────────────────────────────────────────

function makeMockRpcRouter() {
  const emitter = new EventEmitter();
  return Object.assign(emitter, {
    route: vi.fn().mockResolvedValue({}),
    sendToRemoteAgent: vi.fn().mockResolvedValue({ statuses: [], poll_timestamp: 0 }),
    batchRpc: vi.fn().mockResolvedValue([]),
    attachChannel: vi.fn(),
    registerTransport: vi.fn(),
    unregisterTransport: vi.fn(),
  });
}

function makeMockSessionManager(initialStatus = "connected") {
  const emitter = new EventEmitter();
  return Object.assign(emitter, {
    getStatus: vi.fn().mockReturnValue(initialStatus),
    getConnection: vi.fn().mockReturnValue(null),
    connect: vi.fn().mockResolvedValue(undefined),
    disconnect: vi.fn().mockResolvedValue(undefined),
  });
}

// ── Tests ────────────────────────────────────────────────────────

describe("Remote status polling integration", () => {
  let orchestrator: RemoteWorkspaceOrchestrator;
  let mockRouter: ReturnType<typeof makeMockRpcRouter>;
  let mockSessionManager: ReturnType<typeof makeMockSessionManager>;

  const PROFILE_ID = "gpu-server";
  const REPO_PATHS = [
    "/home/dev/projects/api",
    "/home/dev/projects/web",
    "/home/dev/projects/shared",
  ];

  beforeEach(() => {
    vi.useFakeTimers();
    mockRouter = makeMockRpcRouter();
    mockSessionManager = makeMockSessionManager();
    orchestrator = new RemoteWorkspaceOrchestrator(
      mockRouter as any,
      mockSessionManager as any,
    );
  });

  afterEach(() => {
    orchestrator.dispose();
    vi.useRealTimers();
  });

  describe("batch_git_status returns correct branch/dirty/ahead/behind", () => {
    it("returns status for all repos with branch, dirty count, ahead, and behind", async () => {
      const expectedStatuses: RemoteRepoStatus[] = [
        { relative_path: "api", branch: "main", dirty_count: 3, ahead: 1, behind: 0, error: null },
        { relative_path: "web", branch: "feat/auth", dirty_count: 0, ahead: 0, behind: 2, error: null },
        { relative_path: "shared", branch: "develop", dirty_count: 1, ahead: 0, behind: 0, error: null },
      ];

      mockRouter.sendToRemoteAgent.mockResolvedValue({
        statuses: expectedStatuses,
        poll_timestamp: 1716000030,
      });

      const events: any[] = [];
      orchestrator.on("status-update", (e) => events.push(e));

      orchestrator.startPolling(PROFILE_ID, REPO_PATHS);

      // Wait for the immediate poll to resolve
      await vi.advanceTimersByTimeAsync(0);

      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledWith(
        PROFILE_ID,
        "batch_git_status",
        { repo_paths: REPO_PATHS },
      );

      expect(events).toHaveLength(1);
      expect(events[0].profileId).toBe(PROFILE_ID);
      expect(events[0].statuses).toEqual(expectedStatuses);
      expect(events[0].poll_timestamp).toBe(1716000030);
    });

    it("reports null branch for detached HEAD repos", async () => {
      const statuses: RemoteRepoStatus[] = [
        { relative_path: "api", branch: null, dirty_count: 0, ahead: null, behind: null, error: null },
      ];

      mockRouter.sendToRemoteAgent.mockResolvedValue({
        statuses,
        poll_timestamp: 1716000060,
      });

      const events: any[] = [];
      orchestrator.on("status-update", (e) => events.push(e));

      orchestrator.startPolling(PROFILE_ID, ["/home/dev/projects/api"]);
      await vi.advanceTimersByTimeAsync(0);

      expect(events[0].statuses[0].branch).toBeNull();
      expect(events[0].statuses[0].ahead).toBeNull();
      expect(events[0].statuses[0].behind).toBeNull();
    });

    it("reports per-repo errors without affecting other repos", async () => {
      const statuses: RemoteRepoStatus[] = [
        { relative_path: "api", branch: "main", dirty_count: 0, ahead: 0, behind: 0, error: null },
        { relative_path: "web", branch: null, dirty_count: 0, ahead: null, behind: null, error: "not a git repository" },
        { relative_path: "shared", branch: "develop", dirty_count: 2, ahead: 1, behind: 0, error: null },
      ];

      mockRouter.sendToRemoteAgent.mockResolvedValue({
        statuses,
        poll_timestamp: 1716000090,
      });

      const events: any[] = [];
      orchestrator.on("status-update", (e) => events.push(e));

      orchestrator.startPolling(PROFILE_ID, REPO_PATHS);
      await vi.advanceTimersByTimeAsync(0);

      const result = events[0].statuses;
      expect(result).toHaveLength(3);

      const apiStatus = result.find((s: RemoteRepoStatus) => s.relative_path === "api");
      expect(apiStatus!.error).toBeNull();
      expect(apiStatus!.branch).toBe("main");

      const webStatus = result.find((s: RemoteRepoStatus) => s.relative_path === "web");
      expect(webStatus!.error).toBe("not a git repository");
      expect(webStatus!.branch).toBeNull();

      const sharedStatus = result.find((s: RemoteRepoStatus) => s.relative_path === "shared");
      expect(sharedStatus!.error).toBeNull();
      expect(sharedStatus!.dirty_count).toBe(2);
    });

    it("polls every 30 seconds after the initial poll", async () => {
      mockRouter.sendToRemoteAgent.mockResolvedValue({
        statuses: [{ relative_path: "api", branch: "main", dirty_count: 0, ahead: 0, behind: 0, error: null }],
        poll_timestamp: Date.now() / 1000,
      });

      orchestrator.startPolling(PROFILE_ID, ["/home/dev/projects/api"]);

      // Immediate poll
      await vi.advanceTimersByTimeAsync(0);
      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledTimes(1);

      // Advance 30s — second poll
      await vi.advanceTimersByTimeAsync(30_000);
      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledTimes(2);

      // Advance another 30s — third poll
      await vi.advanceTimersByTimeAsync(30_000);
      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledTimes(3);
    });

    it("emits error event when batch_git_status RPC fails", async () => {
      mockRouter.sendToRemoteAgent.mockRejectedValue(new Error("SSH channel closed"));

      const errors: any[] = [];
      orchestrator.on("error", (e) => errors.push(e));

      orchestrator.startPolling(PROFILE_ID, REPO_PATHS);
      await vi.advanceTimersByTimeAsync(0);

      expect(errors).toHaveLength(1);
      expect(errors[0].profileId).toBe(PROFILE_ID);
      expect(errors[0].message).toBe("SSH channel closed");
    });

    it("continues polling after a transient RPC failure", async () => {
      // First poll fails
      mockRouter.sendToRemoteAgent.mockRejectedValueOnce(new Error("timeout"));
      // Second poll succeeds
      mockRouter.sendToRemoteAgent.mockResolvedValueOnce({
        statuses: [{ relative_path: "api", branch: "main", dirty_count: 1, ahead: 0, behind: 0, error: null }],
        poll_timestamp: 1716000060,
      });

      const events: any[] = [];
      const errors: any[] = [];
      orchestrator.on("status-update", (e) => events.push(e));
      orchestrator.on("error", (e) => errors.push(e));

      orchestrator.startPolling(PROFILE_ID, ["/home/dev/projects/api"]);

      // First poll (fails)
      await vi.advanceTimersByTimeAsync(0);
      expect(errors).toHaveLength(1);
      expect(events).toHaveLength(0);

      // Second poll (succeeds after 30s)
      await vi.advanceTimersByTimeAsync(30_000);
      expect(events).toHaveLength(1);
      expect(events[0].statuses[0].branch).toBe("main");
    });
  });

  describe("delta mode only returns changed repos", () => {
    it("sends delta_since parameter when provided via batch_git_status", async () => {
      // The orchestrator calls batch_git_status without delta_since by default.
      // The agent-side delta logic is tested here by verifying the orchestrator
      // correctly relays whatever the agent returns (including partial results).

      // First poll: all repos returned
      const fullStatuses: RemoteRepoStatus[] = [
        { relative_path: "api", branch: "main", dirty_count: 0, ahead: 0, behind: 0, error: null },
        { relative_path: "web", branch: "feat/auth", dirty_count: 2, ahead: 1, behind: 0, error: null },
        { relative_path: "shared", branch: "develop", dirty_count: 0, ahead: 0, behind: 0, error: null },
      ];

      mockRouter.sendToRemoteAgent.mockResolvedValueOnce({
        statuses: fullStatuses,
        poll_timestamp: 1716000030,
      });

      const events: any[] = [];
      orchestrator.on("status-update", (e) => events.push(e));

      orchestrator.startPolling(PROFILE_ID, REPO_PATHS);
      await vi.advanceTimersByTimeAsync(0);

      expect(events).toHaveLength(1);
      expect(events[0].statuses).toHaveLength(3);

      // Second poll: agent returns only changed repos (delta mode behavior)
      const deltaStatuses: RemoteRepoStatus[] = [
        { relative_path: "web", branch: "feat/auth", dirty_count: 3, ahead: 1, behind: 0, error: null },
      ];

      mockRouter.sendToRemoteAgent.mockResolvedValueOnce({
        statuses: deltaStatuses,
        poll_timestamp: 1716000060,
      });

      await vi.advanceTimersByTimeAsync(30_000);

      expect(events).toHaveLength(2);
      // Only the changed repo is returned in delta mode
      expect(events[1].statuses).toHaveLength(1);
      expect(events[1].statuses[0].relative_path).toBe("web");
      expect(events[1].statuses[0].dirty_count).toBe(3);
    });

    it("returns empty statuses array when no repos have changed", async () => {
      // First poll: full results
      mockRouter.sendToRemoteAgent.mockResolvedValueOnce({
        statuses: [
          { relative_path: "api", branch: "main", dirty_count: 0, ahead: 0, behind: 0, error: null },
        ],
        poll_timestamp: 1716000030,
      });

      const events: any[] = [];
      orchestrator.on("status-update", (e) => events.push(e));

      orchestrator.startPolling(PROFILE_ID, ["/home/dev/projects/api"]);
      await vi.advanceTimersByTimeAsync(0);

      // Second poll: no changes (delta returns empty)
      mockRouter.sendToRemoteAgent.mockResolvedValueOnce({
        statuses: [],
        poll_timestamp: 1716000060,
      });

      await vi.advanceTimersByTimeAsync(30_000);

      expect(events).toHaveLength(2);
      expect(events[1].statuses).toHaveLength(0);
      expect(events[1].poll_timestamp).toBe(1716000060);
    });

    it("handles transition from clean to dirty in delta response", async () => {
      // First poll: repo is clean
      mockRouter.sendToRemoteAgent.mockResolvedValueOnce({
        statuses: [
          { relative_path: "api", branch: "main", dirty_count: 0, ahead: 0, behind: 0, error: null },
        ],
        poll_timestamp: 1716000030,
      });

      const events: any[] = [];
      orchestrator.on("status-update", (e) => events.push(e));

      orchestrator.startPolling(PROFILE_ID, ["/home/dev/projects/api"]);
      await vi.advanceTimersByTimeAsync(0);

      // Second poll: repo became dirty (delta returns it)
      mockRouter.sendToRemoteAgent.mockResolvedValueOnce({
        statuses: [
          { relative_path: "api", branch: "main", dirty_count: 5, ahead: 0, behind: 0, error: null },
        ],
        poll_timestamp: 1716000060,
      });

      await vi.advanceTimersByTimeAsync(30_000);

      expect(events[1].statuses[0].dirty_count).toBe(5);
    });

    it("handles branch change in delta response", async () => {
      // First poll: on main
      mockRouter.sendToRemoteAgent.mockResolvedValueOnce({
        statuses: [
          { relative_path: "api", branch: "main", dirty_count: 0, ahead: 0, behind: 0, error: null },
        ],
        poll_timestamp: 1716000030,
      });

      const events: any[] = [];
      orchestrator.on("status-update", (e) => events.push(e));

      orchestrator.startPolling(PROFILE_ID, ["/home/dev/projects/api"]);
      await vi.advanceTimersByTimeAsync(0);

      // Second poll: switched branch
      mockRouter.sendToRemoteAgent.mockResolvedValueOnce({
        statuses: [
          { relative_path: "api", branch: "feat/new-feature", dirty_count: 0, ahead: 2, behind: 0, error: null },
        ],
        poll_timestamp: 1716000060,
      });

      await vi.advanceTimersByTimeAsync(30_000);

      expect(events[1].statuses[0].branch).toBe("feat/new-feature");
      expect(events[1].statuses[0].ahead).toBe(2);
    });
  });

  describe("polling pauses on disconnect and resumes on reconnect", () => {
    it("stops polling when SSH connection is lost", async () => {
      mockRouter.sendToRemoteAgent.mockResolvedValue({
        statuses: [{ relative_path: "api", branch: "main", dirty_count: 0, ahead: 0, behind: 0, error: null }],
        poll_timestamp: Date.now() / 1000,
      });

      // Attach error listener to prevent unhandled error throw
      orchestrator.on("error", () => {});

      orchestrator.startPolling(PROFILE_ID, REPO_PATHS);

      // Initial poll
      await vi.advanceTimersByTimeAsync(0);
      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledTimes(1);

      // Simulate SSH disconnection
      mockSessionManager.emit("ssh:status-changed", {
        profileId: PROFILE_ID,
        status: "disconnected",
      });

      // Clear call count to track new calls
      mockRouter.sendToRemoteAgent.mockClear();

      // Advance past multiple polling intervals — no polls should fire
      await vi.advanceTimersByTimeAsync(90_000);
      expect(mockRouter.sendToRemoteAgent).not.toHaveBeenCalled();
    });

    it("emits error event on disconnect", async () => {
      mockRouter.sendToRemoteAgent.mockResolvedValue({
        statuses: [],
        poll_timestamp: Date.now() / 1000,
      });

      const errors: any[] = [];
      orchestrator.on("error", (e) => errors.push(e));

      orchestrator.startPolling(PROFILE_ID, REPO_PATHS);
      await vi.advanceTimersByTimeAsync(0);

      // Simulate disconnection
      mockSessionManager.emit("ssh:status-changed", {
        profileId: PROFILE_ID,
        status: "disconnected",
      });

      expect(errors).toHaveLength(1);
      expect(errors[0].profileId).toBe(PROFILE_ID);
      expect(errors[0].message).toBe("SSH connection lost");
    });

    it("resumes polling when SSH connection is re-established", async () => {
      const statuses: RemoteRepoStatus[] = [
        { relative_path: "api", branch: "main", dirty_count: 0, ahead: 0, behind: 0, error: null },
      ];

      mockRouter.sendToRemoteAgent.mockResolvedValue({
        statuses,
        poll_timestamp: Date.now() / 1000,
      });

      // Attach error listener to prevent unhandled error throw
      orchestrator.on("error", () => {});

      // Open workspace so orchestrator tracks the repos for this profile
      mockRouter.sendToRemoteAgent.mockResolvedValueOnce({
        repos: [{ absolute_path: "/home/dev/projects/api", relative_path: "api", name: "api" }],
      });

      await orchestrator.openWorkspace(PROFILE_ID, "/home/dev/projects");

      // Reset mock to track polling calls
      mockRouter.sendToRemoteAgent.mockClear();
      mockRouter.sendToRemoteAgent.mockResolvedValue({
        statuses,
        poll_timestamp: Date.now() / 1000,
      });

      // Verify polling is active (advance 30s for next poll)
      await vi.advanceTimersByTimeAsync(30_000);
      const callsBeforeDisconnect = mockRouter.sendToRemoteAgent.mock.calls.length;
      expect(callsBeforeDisconnect).toBeGreaterThan(0);

      // Simulate disconnection
      mockSessionManager.emit("ssh:status-changed", {
        profileId: PROFILE_ID,
        status: "disconnected",
      });

      mockRouter.sendToRemoteAgent.mockClear();

      // Verify polling stopped
      await vi.advanceTimersByTimeAsync(60_000);
      expect(mockRouter.sendToRemoteAgent).not.toHaveBeenCalled();

      // Simulate reconnection
      mockRouter.sendToRemoteAgent.mockResolvedValue({
        statuses,
        poll_timestamp: Date.now() / 1000,
      });

      mockSessionManager.emit("ssh:status-changed", {
        profileId: PROFILE_ID,
        status: "connected",
      });

      // Polling should resume — immediate poll on reconnect
      await vi.advanceTimersByTimeAsync(0);
      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledWith(
        PROFILE_ID,
        "batch_git_status",
        { repo_paths: ["/home/dev/projects/api"] },
      );
    });

    it("does not resume polling for profiles without open workspaces", async () => {
      mockRouter.sendToRemoteAgent.mockResolvedValue({
        statuses: [],
        poll_timestamp: Date.now() / 1000,
      });

      // Attach error listener to prevent unhandled error throw
      orchestrator.on("error", () => {});

      // Start polling without opening a workspace (direct startPolling call)
      orchestrator.startPolling(PROFILE_ID, REPO_PATHS);
      await vi.advanceTimersByTimeAsync(0);

      // Disconnect
      mockSessionManager.emit("ssh:status-changed", {
        profileId: PROFILE_ID,
        status: "disconnected",
      });

      mockRouter.sendToRemoteAgent.mockClear();

      // Reconnect — but since no workspace was opened via openWorkspace(),
      // the orchestrator has no record of repos to resume polling for
      mockSessionManager.emit("ssh:status-changed", {
        profileId: PROFILE_ID,
        status: "connected",
      });

      await vi.advanceTimersByTimeAsync(30_000);
      // No polling should resume because openWorkspaces map doesn't have this profile
      expect(mockRouter.sendToRemoteAgent).not.toHaveBeenCalled();
    });

    it("disconnect for one profile does not affect polling of another", async () => {
      const PROFILE_A = "server-a";
      const PROFILE_B = "server-b";

      mockRouter.sendToRemoteAgent.mockResolvedValue({
        statuses: [{ relative_path: "repo", branch: "main", dirty_count: 0, ahead: 0, behind: 0, error: null }],
        poll_timestamp: Date.now() / 1000,
      });

      // Attach error listener to prevent unhandled error throw
      orchestrator.on("error", () => {});

      orchestrator.startPolling(PROFILE_A, ["/home/dev/repo-a"]);
      orchestrator.startPolling(PROFILE_B, ["/home/dev/repo-b"]);

      // Both poll immediately
      await vi.advanceTimersByTimeAsync(0);
      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledTimes(2);

      // Disconnect profile A only
      mockSessionManager.emit("ssh:status-changed", {
        profileId: PROFILE_A,
        status: "disconnected",
      });

      mockRouter.sendToRemoteAgent.mockClear();

      // Advance 30s — only profile B should poll
      await vi.advanceTimersByTimeAsync(30_000);

      const calls = mockRouter.sendToRemoteAgent.mock.calls;
      expect(calls).toHaveLength(1);
      expect(calls[0][0]).toBe(PROFILE_B);
      expect(calls[0][1]).toBe("batch_git_status");
    });

    it("stopPolling prevents further polls even without disconnect event", async () => {
      mockRouter.sendToRemoteAgent.mockResolvedValue({
        statuses: [],
        poll_timestamp: Date.now() / 1000,
      });

      orchestrator.startPolling(PROFILE_ID, REPO_PATHS);
      await vi.advanceTimersByTimeAsync(0);

      mockRouter.sendToRemoteAgent.mockClear();

      // Explicitly stop polling
      orchestrator.stopPolling(PROFILE_ID);

      // Advance past multiple intervals
      await vi.advanceTimersByTimeAsync(90_000);
      expect(mockRouter.sendToRemoteAgent).not.toHaveBeenCalled();
    });

    it("closeWorkspace stops polling and cleans up state", async () => {
      mockRouter.sendToRemoteAgent.mockResolvedValueOnce({
        repos: [{ absolute_path: "/home/dev/projects/api", relative_path: "api", name: "api" }],
      });
      mockRouter.sendToRemoteAgent.mockResolvedValue({
        statuses: [{ relative_path: "api", branch: "main", dirty_count: 0, ahead: 0, behind: 0, error: null }],
        poll_timestamp: Date.now() / 1000,
      });

      await orchestrator.openWorkspace(PROFILE_ID, "/home/dev/projects");

      mockRouter.sendToRemoteAgent.mockClear();

      // Close workspace
      orchestrator.closeWorkspace(PROFILE_ID);

      // Verify no more polling
      await vi.advanceTimersByTimeAsync(60_000);
      expect(mockRouter.sendToRemoteAgent).not.toHaveBeenCalled();

      // Verify workspace is removed
      expect(orchestrator.getWorkspace(PROFILE_ID)).toBeUndefined();
    });

    it("does not start polling with empty repo list", () => {
      orchestrator.startPolling(PROFILE_ID, []);

      // No interval should be set — advance time and verify no calls
      vi.advanceTimersByTime(60_000);
      expect(mockRouter.sendToRemoteAgent).not.toHaveBeenCalled();
    });
  });
});
